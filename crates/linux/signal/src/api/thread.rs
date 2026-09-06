use alloc::{alloc::AllocError, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use kspin::SpinNoIrq;
use spin::Mutex as DeliveryMutex;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use super::{
    ProcessSignalManager, RegisteredThread,
    frame::{
        PreparedSignalRestore, SignalFpState, SignalFrame, SignalFrameLayoutError,
        SignalFrameStack, prepare_signal_restore,
    },
};
use crate::{
    DefaultSignalAction, DequeuedSignal as PendingDequeuedSignal, DetachedSignal, PendingSignals,
    PreparedSignal, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo, SignalOSAction,
    SignalRecordGeneration, SignalSet, SignalStack, SignalStackRestoreError, Signo,
    arch::{SignalContextError, UserContext},
};

/// Result of publishing one thread-directed signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadSignalSendOutcome {
    /// Whether this send published a new pending record.
    pub published: bool,
    /// Whether the target endpoint should be interrupted for delivery.
    pub wake: bool,
    /// Generation of the exact queue record when this call published one.
    /// Ignored, coalesced, inactive, fallback, and exhausted sends return
    /// `None`.
    pub generation: Option<SignalRecordGeneration>,
}

/// Prepared exact thread endpoint for a non-blocking signal commit.
///
/// The strong endpoint owner is acquired before an embedding kernel enters an
/// unrelated credential or liveness spin critical section. Publication then
/// rechecks the endpoint lifecycle and returns the owner together with every
/// unused queue record, so no arbitrary destructor runs in the commit call.
#[must_use = "publishing or dropping the prepared send releases endpoint ownership"]
pub struct PreparedThreadSignalSend {
    thread: Arc<ThreadSignalManager>,
    registration: Arc<RegisteredThread>,
}

const ENDPOINT_PENDING: u8 = 0;
const ENDPOINT_ACTIVE: u8 = 1;
const ENDPOINT_RETAINED: u8 = 2;
const ENDPOINT_CANCELLED: u8 = 3;

#[derive(Clone, Copy)]
enum EndpointSendMode {
    Active,
    Retained,
}

impl EndpointSendMode {
    const fn accepts(self, state: u8) -> bool {
        matches!(
            (self, state),
            (Self::Active, ENDPOINT_ACTIVE) | (Self::Retained, ENDPOINT_RETAINED)
        )
    }
}

impl PreparedThreadSignalSend {
    /// Publishes one already allocated and accounted signal record.
    ///
    /// The commit is serialized against endpoint cancellation and performs no
    /// allocation. The returned deferred owner must leave every unrelated
    /// IRQ-disabled outer critical section before it is finished or dropped.
    pub fn publish(self, prepared: PreparedSignal) -> DeferredThreadSignalSend {
        let thread = &self.thread;
        let signo = prepared.signo();
        let inactive = ThreadSignalSendOutcome {
            published: false,
            wake: false,
            generation: None,
        };
        let mut prepared = Some(prepared);
        let mut unused = None;
        let mut published = false;
        let mut accepted = false;
        let mut wake = false;
        let mut generation = None;
        let mut inactive_commit = false;
        let detached = thread.proc.with_action_update(|owner| {
            let mut generation_detached = DetachedSignal::empty();
            let lifecycle = thread.lifecycle.lock();
            let exact_active = *lifecycle == ENDPOINT_ACTIVE
                && self.registration.is_active()
                && thread
                    .registration
                    .lock()
                    .as_ref()
                    .is_some_and(|entry| Arc::ptr_eq(entry, &self.registration));
            if !exact_active {
                inactive_commit = true;
                drop(lifecycle);
                return generation_detached;
            }
            if ProcessSignalManager::has_generation_effect(signo) {
                thread
                    .proc
                    .apply_generation_effect_locked(signo, &mut generation_detached);
                let still_active = *lifecycle == ENDPOINT_ACTIVE
                    && self.registration.is_active()
                    && thread
                        .registration
                        .lock()
                        .as_ref()
                        .is_some_and(|entry| Arc::ptr_eq(entry, &self.registration));
                if !still_active {
                    inactive_commit = true;
                    drop(lifecycle);
                    return generation_detached;
                }
            }

            let blocked = thread.signal_blocked(signo);
            let actions = owner.lock();
            let ignored = ProcessSignalManager::action_ignored(&actions, signo)
                && !blocked
                && !thread.signal_real_blocked(signo);
            if !ignored {
                let mut pending = thread.pending.lock();
                let coalesced = !signo.is_realtime() && pending.set.has(signo);
                if !coalesced {
                    match thread.assign_generation(
                        prepared
                            .as_mut()
                            .expect("prepared signal is retained until publication"),
                    ) {
                        Ok(next) => generation = next,
                        Err(()) => {
                            drop(pending);
                            drop(actions);
                            drop(lifecycle);
                            inactive_commit = true;
                            return generation_detached;
                        }
                    }
                }
                let outcome = pending.publish(
                    prepared
                        .take()
                        .expect("prepared signal is retained until publication"),
                );
                (published, unused) = outcome.into_parts();
                accepted = true;
                wake = !blocked;
            }
            drop(actions);
            drop(lifecycle);
            generation_detached
        });
        // Job-control cancellation detaches owned realtime nodes.  Release
        // those nodes and their queue-account charges only after every
        // shared-update and endpoint signal-state guard has left scope.
        drop(detached);
        if inactive_commit {
            return DeferredThreadSignalSend {
                _prepared: self,
                outcome: inactive,
                unused: prepared,
            };
        }
        unused = unused.or(prepared);
        if accepted {
            thread.possibly_has_signal.store(true, Ordering::Release);
        }
        DeferredThreadSignalSend {
            _prepared: self,
            outcome: ThreadSignalSendOutcome {
                published,
                wake,
                generation: published.then_some(generation).flatten(),
            },
            unused,
        }
    }
}

/// Ownership deferred out of a thread-signal publication critical section.
///
/// Dropping this value can release queue-account, queue-node, process-manager,
/// registration, and exact endpoint ownership. Retain it until every
/// unrelated IRQ-disabled outer guard has been released.
#[must_use = "finish or drop this value only after outer spin locks are released"]
pub struct DeferredThreadSignalSend {
    _prepared: PreparedThreadSignalSend,
    outcome: ThreadSignalSendOutcome,
    unused: Option<PreparedSignal>,
}

impl Drop for DeferredThreadSignalSend {
    fn drop(&mut self) {
        // Publication can happen under an embedding-owned IRQ-off guard.
        // This deferred owner must leave that guard before callbacks run.
        if self.outcome.published {
            self._prepared.thread.notify_pending();
        }
    }
}

impl DeferredThreadSignalSend {
    /// Returns the fixed publication and wakeup result without releasing
    /// deferred ownership.
    pub const fn outcome(&self) -> ThreadSignalSendOutcome {
        self.outcome
    }

    /// Releases retained endpoint ownership in the caller's current context
    /// and returns any queue record that publication did not consume.
    pub fn finish(mut self) -> (ThreadSignalSendOutcome, Option<PreparedSignal>) {
        let unused = self.unused.take();
        (self.outcome, unused)
    }
}

/// Why a thread endpoint could not be registered with its process manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRegistrationError {
    /// Allocating the registry entry or immutable replacement failed.
    NoMemory,
    /// This endpoint already owns a pending or active registration.
    AlreadyRegistered,
    /// Another live endpoint in the process already owns this thread ID.
    TidInUse,
    /// The process-local endpoint registry reached its explicit hard limit.
    Capacity,
    /// The admission was cancelled before it could be committed.
    Cancelled,
}

/// Why an exact endpoint could not be retained for an authorized send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSignalPrepareError {
    /// The manager has no currently active registration identity.
    NotRegistered,
}

/// Failure returned by a fallible signal pre-handler hook.
///
/// The hook runs after the effective userspace handler disposition has been
/// selected and before the signal frame is snapshotted or copied to userspace.
/// Keeping the hook error behind a signal-owned type lets callers distinguish a
/// rejected pre-handler from the existing signal actions, which continue to
/// report copyout and invalid-layout failures as `SignalOSAction::CoreDump`.
#[derive(Debug, PartialEq, Eq)]
pub enum SignalPreHandlerError<E> {
    /// The pre-handler rejected delivery.
    Hook(E),
}

/// Result of the kernel's pre-delivery rseq gate.
///
/// The callback runs after a signal has been selected and its effective
/// disposition has been resolved, but before a frame is prepared or copied
/// to userspace. Retry and Fault return the selected record to its source
/// queue so the caller can make progress without losing signal ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDeliveryPreflight {
    /// Proceed with default-action handling or userspace frame publication.
    Proceed,
    /// Retry delivery at a later user-return point.
    Retry,
    /// Enter the user-return fault path before retrying delivery.
    Fault,
    /// Consume the selected record because the caller published an
    /// origin-bound replacement signal. The caller should retry delivery so
    /// the replacement can be selected.
    Replaced,
    /// Consume the selected record because the caller must fail closed with a
    /// fatal signal action.
    Fatal,
}

/// Result of one asynchronous signal-delivery scan.
#[must_use = "the caller must handle the selected signal or retry/fault result"]
pub enum SignalDeliveryResult {
    Delivered(DeliveredSignal),
    None,
    Retry,
    Fault,
    Replaced,
    Fatal,
}

impl From<AllocError> for ThreadRegistrationError {
    fn from(_: AllocError) -> Self {
        Self::NoMemory
    }
}

pub struct DeliveredSignal {
    pub info: SignalInfo,
    pub os_action: SignalOSAction,
    pub restartable_handler: bool,
    /// The restorer selected from the immutable sigaction/default-restorer
    /// snapshot while the frame was prepared.  This is present only for a
    /// userspace handler and must not be reconstructed by rereading the
    /// published user frame.
    pub handler_restorer: Option<usize>,
}

#[derive(Clone, Copy)]
struct SignalDeliveryAction {
    os_action: SignalOSAction,
    handler_restorer: Option<usize>,
}

impl SignalDeliveryAction {
    const fn ordinary(os_action: SignalOSAction) -> Self {
        Self {
            os_action,
            handler_restorer: None,
        }
    }

    const fn handler(restorer: usize) -> Self {
        Self {
            os_action: SignalOSAction::Handler,
            handler_restorer: Some(restorer),
        }
    }
}

fn no_signal_pre_handler<M: UserMemory + ?Sized>(
    _memory: &mut UserMemoryContext<'_, M>,
    _uctx: &mut UserContext,
) -> Result<(), core::convert::Infallible> {
    Ok(())
}

/// One linearized observation made by a synchronous signal wait.
///
/// `Accepted` owns a signal selected by the wait set. `Delivered` owns a
/// different, asynchronously deliverable signal whose disposition has already
/// been resolved and whose handler frame, if any, has already been published.
/// `Retry`/`Fault` retain the selected asynchronous record. `Replaced` means
/// the selected record was consumed because the caller published an
/// origin-bound replacement. `Fatal` means it was consumed before a caller's
/// fatal action. `None` means neither class was present at this observation.
#[must_use = "the caller must complete accepted or delivered signal ownership"]
pub enum SignalWaitObservation {
    Accepted(SignalInfo),
    Delivered(DeliveredSignal),
    Retry,
    Fault,
    Replaced,
    Fatal,
    None,
}

/// Keeps ownership provenance for one selected record. Retry/Fault must put
/// the exact record back at the front of the queue which supplied it; a
/// signal number alone is insufficient because process and thread queues have
/// independent RT FIFO/accounting state.
enum DequeuedSignal {
    Thread(PendingDequeuedSignal),
    Process(PendingDequeuedSignal),
}

impl DequeuedSignal {
    fn signo(&self) -> Signo {
        match self {
            Self::Thread(signal) | Self::Process(signal) => signal.signo(),
        }
    }

    fn generation(&self) -> Option<SignalRecordGeneration> {
        match self {
            Self::Thread(signal) | Self::Process(signal) => signal.generation(),
        }
    }

    fn info(&self) -> Option<&SignalInfo> {
        match self {
            Self::Thread(signal) | Self::Process(signal) => signal.info(),
        }
    }

    fn into_info(self) -> SignalInfo {
        match self {
            Self::Thread(signal) | Self::Process(signal) => signal.into_info(),
        }
    }

    fn requeue(self, manager: &ThreadSignalManager) {
        match self {
            Self::Thread(signal) => manager.requeue_thread_signal(signal),
            Self::Process(signal) => manager.proc.requeue_signal(signal),
        }
    }
}

/// Thread-level signal manager.
pub struct ThreadSignalManager {
    /// The process-level signal manager
    proc: Arc<ProcessSignalManager>,

    /// The pending signals
    pending: SpinNoIrq<PendingSignals>,
    /// The set of signals currently blocked from delivery.
    blocked: SpinNoIrq<SignalSet>,
    /// Temporarily preserved mask while a synchronous wait unblocks signals.
    real_blocked: SpinNoIrq<Option<SignalSet>>,
    /// The stack used by signal handlers
    stack: SpinNoIrq<SignalStack>,

    /// Quiesces a complete userspace delivery against endpoint teardown.
    /// The lock is kept outside IRQ-disabling state because frame copyout
    /// must never run with interrupts disabled.
    delivery: DeliveryMutex<()>,

    /// Serializes publication against explicit endpoint cancellation and
    /// stores the sole endpoint lifecycle state.
    lifecycle: SpinNoIrq<u8>,
    registration: SpinNoIrq<Option<Arc<RegisteredThread>>>,

    /// One-shot exact bypass for a signal forced by the user-return rseq
    /// fault path. Zero means no bypass is armed.
    delivery_bypass: AtomicU64,
    delivery_bypass_signo: AtomicU8,
    /// The exact record currently selected by the single delivery consumer.
    /// These values are visible to the pre-delivery callback only.
    selected_generation: AtomicU64,
    selected_signo: AtomicU8,
    /// Next nonzero record generation. Zero is a sticky exhausted state.
    next_generation: AtomicU64,

    possibly_has_signal: AtomicBool,
}

/// Deactivates a newly registered endpoint if the owning thread fails to
/// finish construction. Successful lifecycle publication disarms the token.
///
/// Dropping an uncommitted token deactivates the registry entry and clears the
/// manager-owned admission slot if it still has the same identity. The
/// registry's next immutable publication compacts the inactive entry.
#[must_use = "dropping the token rolls back thread-signal registration"]
pub struct ThreadSignalRegistration {
    entry: Arc<RegisteredThread>,
    thread: Arc<ThreadSignalManager>,
    rollback: bool,
}

impl ThreadSignalRegistration {
    /// Activates the admitted endpoint unless teardown cancelled it first.
    pub fn commit(mut self) -> Result<(), ThreadRegistrationError> {
        let committed = self.thread.proc.with_action_update(|_| {
            let mut lifecycle = self.thread.lifecycle.lock();
            let still_admitted = self
                .thread
                .registration
                .lock()
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry));
            if !still_admitted || *lifecycle != ENDPOINT_PENDING {
                return false;
            }
            *lifecycle = ENDPOINT_ACTIVE;
            self.entry.activate();
            true
        });
        if !committed {
            return Err(ThreadRegistrationError::Cancelled);
        }
        self.rollback = false;
        Ok(())
    }
}

impl Drop for ThreadSignalRegistration {
    fn drop(&mut self) {
        if self.rollback {
            let (removed, detached) = self.thread.proc.with_action_update(|_| {
                let mut lifecycle = self.thread.lifecycle.lock();
                self.entry.deactivate();
                let removed = {
                    let mut registration = self.thread.registration.lock();
                    if registration
                        .as_ref()
                        .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
                    {
                        *lifecycle = ENDPOINT_CANCELLED;
                        registration.take()
                    } else {
                        None
                    }
                };
                let detached = if removed.is_some() {
                    let mut pending = self.thread.pending.lock();
                    let detached = pending.take_all();
                    self.thread
                        .possibly_has_signal
                        .store(false, Ordering::Release);
                    Some(detached)
                } else {
                    None
                };
                drop(lifecycle);
                (removed, detached)
            });
            // The final strong reference may deallocate the registry entry.
            // Never release it while the IRQ-off registration guard is held.
            drop(removed);
            // A pending admission can be explicitly retained before its
            // token commits.  If that token is then dropped (or commit loses
            // the lifecycle race), reclaim any exact private records outside
            // all endpoint and action-update guards.
            drop(detached);
        }
    }
}

impl ThreadSignalManager {
    fn notify_pending(&self) {
        // Every thread shares its process readiness source: an epoll interest
        // may have been installed by a sibling of the eventual reader.
        self.proc.notify_pending();
    }

    /// Fallibly constructs an unregistered thread signal endpoint.
    /// Registration is separate so callers can finish building the owning
    /// thread object before making even a weak child entry observable.
    pub fn try_new(proc: Arc<ProcessSignalManager>) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            proc,

            pending: SpinNoIrq::new(PendingSignals::default()),
            blocked: SpinNoIrq::new(SignalSet::default()),
            real_blocked: SpinNoIrq::new(None),
            stack: SpinNoIrq::new(SignalStack::default()),

            delivery: DeliveryMutex::new(()),

            lifecycle: SpinNoIrq::new(ENDPOINT_PENDING),
            registration: SpinNoIrq::new(None),

            delivery_bypass: AtomicU64::new(0),
            delivery_bypass_signo: AtomicU8::new(0),
            selected_generation: AtomicU64::new(0),
            selected_signo: AtomicU8::new(0),
            next_generation: AtomicU64::new(1),

            possibly_has_signal: AtomicBool::new(false),
        })
    }

    /// Fallibly publishes this endpoint in its process signal registry.
    pub fn try_register(
        self: &Arc<Self>,
        tid: u32,
    ) -> Result<ThreadSignalRegistration, ThreadRegistrationError> {
        let (entry, previous, registry) = self.proc.with_action_update(|_| {
            if self.registration.lock().is_some() {
                return Err(ThreadRegistrationError::AlreadyRegistered);
            }
            let registry = self.proc.children_registry_snapshot();
            let mut live = 0usize;
            if let Some(registry) = registry.as_deref() {
                for registered in registry {
                    if registered.is_live() {
                        if registered.claims_tid(tid) {
                            return Err(ThreadRegistrationError::TidInUse);
                        }
                        live += 1;
                    }
                }
            }
            if live >= self.proc.thread_limit() {
                return Err(ThreadRegistrationError::Capacity);
            }
            let capacity = live
                .checked_add(1)
                .ok_or(ThreadRegistrationError::NoMemory)?;
            let mut replacement = Vec::new();
            replacement
                .try_reserve_exact(capacity)
                .map_err(|_| ThreadRegistrationError::NoMemory)?;
            if let Some(registry) = registry.as_deref() {
                for registered in registry {
                    if registered.is_live() {
                        replacement.push(registered.clone());
                    }
                }
            }
            let entry = RegisteredThread::try_new(tid, self)?;
            replacement.push(entry.clone());
            let replacement =
                Arc::try_new(replacement).map_err(|_| ThreadRegistrationError::NoMemory)?;

            let manager_entry = entry.clone();
            {
                let mut lifecycle = self.lifecycle.lock();
                let mut registration = self.registration.lock();
                if registration.is_some() {
                    return Err(ThreadRegistrationError::AlreadyRegistered);
                }
                *lifecycle = ENDPOINT_PENDING;
                *registration = Some(manager_entry);
            }

            let previous = {
                let mut children = self.proc.children.lock();
                children.replace(replacement)
            };
            Ok((entry, previous, registry))
        })?;

        // The immutable registry and all of its owned Arcs are allocated and
        // destroyed outside the publication spin lock. The manager/shared
        // action gates are no longer held here.
        drop(previous);
        drop(registry);
        Ok(ThreadSignalRegistration {
            entry,
            thread: self.clone(),
            rollback: true,
        })
    }

    /// Cancels this endpoint and drains all thread-private pending records.
    ///
    /// Publication and cancellation share a short lifecycle lock. Once this
    /// method returns, a sender that had not already linearized cannot publish
    /// another record. It also waits for a delivery that already started, so
    /// no handler context or mask update can complete after teardown returns.
    /// A later `try_register` may publish the endpoint again.
    pub fn cancel_registration(&self) -> bool {
        let delivery = self.delivery.lock();
        let (cancelled, registration, detached) = self.proc.with_action_update(|_| {
            let mut lifecycle = self.lifecycle.lock();
            let registration = self.registration.lock().take();
            let cancelled = registration.is_some();
            *lifecycle = ENDPOINT_CANCELLED;
            if let Some(entry) = registration.as_ref() {
                entry.deactivate();
            }
            let detached = self.pending.lock().take_all();
            self.possibly_has_signal.store(false, Ordering::Release);
            drop(lifecycle);
            (cancelled, registration, detached)
        });
        drop(delivery);
        drop(registration);
        drop(detached);
        cancelled
    }

    /// Retires this exact registry identity. Retained group-leader endpoints
    /// remain available only to [`Self::try_send_retained_signal_with`] and
    /// remain visible to action/job-control flushes; ordinary routing and
    /// wakeup scans see only active entries. A non-retained retirement is a
    /// full cancellation and permits a later registration with a new entry.
    pub fn retire_registration(&self, tid: u32, retain_private_pending: bool) {
        // A delivery may already have selected a private record. Holding the
        // delivery mutex through the lifecycle transition guarantees that no
        // frame or mask update completes after retirement returns.
        let delivery = self.delivery.lock();
        let (_registry, removed, detached) = self.proc.with_action_update(|_| {
            let registry = self.proc.children_registry_snapshot();
            let entry = registry.as_deref().and_then(|registry| {
                registry
                    .iter()
                    .find(|entry| entry.matches(tid, self as *const Self))
            });
            let mut detached = None;
            let mut removed = None;
            if let Some(entry) = entry {
                let mut lifecycle = self.lifecycle.lock();
                let exact_registration = self
                    .registration
                    .lock()
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, entry));
                if exact_registration {
                    if retain_private_pending {
                        if matches!(*lifecycle, ENDPOINT_PENDING | ENDPOINT_ACTIVE) {
                            entry.retain_pending_only();
                            *lifecycle = ENDPOINT_RETAINED;
                        }
                    } else {
                        entry.deactivate();
                        *lifecycle = ENDPOINT_CANCELLED;
                        removed = self.registration.lock().take();
                        detached = Some(self.pending.lock().take_all());
                        self.possibly_has_signal.store(false, Ordering::Release);
                    }
                }
                drop(lifecycle);
            }
            (registry, removed, detached)
        });
        drop(delivery);
        // Registry-entry and queue ownership are released only outside every
        // lifecycle, action-update, registry, and pending guard.
        drop(removed);
        drop(detached);
    }

    /// Returns whether this endpoint currently accepts directed signals.
    pub fn is_registered(&self) -> bool {
        *self.lifecycle.lock() == ENDPOINT_ACTIVE
    }

    /// Retains this exact endpoint and registration identity before a
    /// non-blocking authorized signal commit.
    ///
    /// Registry snapshot acquisition is sleepable and every temporary `Arc`
    /// is released before this method returns. A token prepared before
    /// cancellation cannot target a later registration of the same manager.
    pub fn try_prepare_signal_send(
        self: &Arc<Self>,
    ) -> Result<PreparedThreadSignalSend, ThreadSignalPrepareError> {
        let registry = self.proc.children_registry_snapshot();
        let registration = registry.as_deref().and_then(|registry| {
            registry.iter().find_map(|entry| {
                let (_, thread) = entry.upgrade()?;
                Arc::ptr_eq(&thread, self).then(|| Arc::clone(entry))
            })
        });
        drop(registry);
        let registration = registration.ok_or(ThreadSignalPrepareError::NotRegistered)?;
        Ok(PreparedThreadSignalSend {
            thread: Arc::clone(self),
            registration,
        })
    }

    /// Dequeues a signal from the thread's pending signals.
    #[must_use]
    pub fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        self.dequeue_signal_with_source(mask)
            .map(DequeuedSignal::into_info)
    }

    /// Returns whether a pending signal matches both `mask` and the current
    /// blocked mask of this thread.
    ///
    /// The blocked-mask lock is held while both pending queues are observed.
    /// This gives readiness users the same blocked-mask linearization domain
    /// as [`Self::dequeue_signal_for_signalfd`]; the result is still only a
    /// readiness hint and may change before a later operation.
    pub fn has_pending_signal_for_signalfd(&self, mask: &SignalSet) -> bool {
        self.with_signalfd_mask(mask, |effective| {
            let thread_pending = self.pending.lock().set;
            !(thread_pending & *effective).is_empty()
                || !(self.proc.pending() & *effective).is_empty()
        })
    }

    /// Dequeues one pending signal selected by `mask` and the thread's
    /// currently blocked mask.
    ///
    /// The blocked-mask lock remains held until selection and dequeue have
    /// both completed. A concurrent mask update therefore cannot make an
    /// unblocked signal eligible after this operation has selected it.
    #[must_use]
    pub fn dequeue_signal_for_signalfd(&self, mask: &SignalSet) -> Option<SignalInfo> {
        // Keep queue-owned destruction outside the blocked spin lock. The
        // selection and removal are still linearized while that lock is held.
        let selected =
            self.with_signalfd_mask(mask, |effective| self.dequeue_signal_with_source(effective));
        selected.map(DequeuedSignal::into_info)
    }

    fn with_signalfd_mask<R>(&self, mask: &SignalSet, f: impl FnOnce(&SignalSet) -> R) -> R {
        let blocked = self.blocked.lock();
        let effective = *mask & *blocked;
        f(&effective)
    }

    fn dequeue_signal_with_source(&self, mask: &SignalSet) -> Option<DequeuedSignal> {
        self.dequeue_thread_signal_owned(mask)
            .map(DequeuedSignal::Thread)
            .or_else(|| {
                self.proc
                    .dequeue_signal_owned(mask)
                    .map(DequeuedSignal::Process)
            })
    }

    fn dequeue_thread_signal_owned(&self, mask: &SignalSet) -> Option<PendingDequeuedSignal> {
        {
            let mut pending = self.pending.lock();
            let signal = pending.dequeue_signal(mask);
            if pending.set.is_empty() {
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            signal
        }
    }

    fn requeue_thread_signal(&self, signal: PendingDequeuedSignal) {
        let mut pending = self.pending.lock();
        signal.requeue_front(&mut pending);
        self.possibly_has_signal.store(true, Ordering::Release);
        drop(pending);
        self.notify_pending();
    }

    /// Selects an armed exact record before ordinary signal-number ordering.
    /// This lets a forced SIGSEGV bypass a lower-numbered rejected signal,
    /// while stale or same-number records remain ordinary candidates.
    fn dequeue_signal_for_delivery(&self, mask: &SignalSet) -> Option<DequeuedSignal> {
        let generation = self.delivery_bypass.load(Ordering::Acquire);
        let priority = self.delivery_bypass_signo.load(Ordering::Acquire);
        if generation != 0
            && let Some(signo) = Signo::from_repr(priority)
        {
            let mut priority_mask = SignalSet::default();
            priority_mask.add(signo);
            if let Some(signal) = self.dequeue_signal_with_source(&(priority_mask & *mask)) {
                if signal
                    .generation()
                    .is_some_and(|selected| selected.get() == generation)
                {
                    return Some(signal);
                }
                // A stale token or a same-number coalesced/fallback record
                // cannot satisfy the bypass. Restore this exact record and
                // retire the stale arm before ordinary selection.
                signal.requeue(self);
                if self
                    .delivery_bypass
                    .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.delivery_bypass_signo.store(0, Ordering::Release);
                }
            }
        }
        self.dequeue_signal_with_source(mask)
    }

    pub fn process(&self) -> &Arc<ProcessSignalManager> {
        &self.proc
    }

    /// Handles a signal with a fallible pre-handler hook.
    ///
    /// The hook is called exactly once only for an effective userspace
    /// [`SignalDisposition::Handler`] action. It receives the same explicit
    /// userspace memory context and mutable machine context used for frame
    /// publication. Its changes to the machine context become the snapshot
    /// stored in the signal frame when it succeeds.
    ///
    /// A hook error leaves the machine context unchanged, writes no signal
    /// frame or restorer, and is returned as [`SignalPreHandlerError::Hook`].
    pub fn handle_signal_with_pre_handler<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
        pre_handler: impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
    ) -> Result<Option<SignalOSAction>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        self.handle_signal_with_pre_handler_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            sig,
            action,
            pre_handler,
            || panic!("embedding kernel must provide a full XSAVE snapshot"),
        )
        .map(|outcome| outcome.map(|outcome| outcome.os_action))
    }

    /// Handles a signal with a caller-provided complete XSAVE snapshot.
    ///
    /// The callback is called only after the selected action is a userspace
    /// handler and the checked frame layout succeeds. It returns an owned
    /// image or a pre-publication failure; CPU save instructions and any
    /// architecture validation stay in the embedding root.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_signal_with_fp_snapshot<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
        pre_handler: impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
        snapshot: impl FnOnce() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> Result<Option<SignalOSAction>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        self.handle_signal_with_pre_handler_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            sig,
            action,
            pre_handler,
            snapshot,
        )
        .map(|outcome| outcome.map(|outcome| outcome.os_action))
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_signal_with_pre_handler_and_fp_snapshot<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
        mut pre_handler: impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
        snapshot: impl FnOnce() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> Result<Option<SignalDeliveryAction>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        let signo = sig.signo();
        debug!("Handle signal: {signo:?}");
        match action.disposition {
            SignalDisposition::Default => match signo.default_action() {
                DefaultSignalAction::Terminate => Ok(Some(SignalDeliveryAction::ordinary(
                    SignalOSAction::Terminate,
                ))),
                DefaultSignalAction::CoreDump => Ok(Some(SignalDeliveryAction::ordinary(
                    SignalOSAction::CoreDump,
                ))),
                DefaultSignalAction::Stop => {
                    Ok(Some(SignalDeliveryAction::ordinary(SignalOSAction::Stop)))
                }
                DefaultSignalAction::Ignore => Ok(None),
                DefaultSignalAction::Continue => Ok(Some(SignalDeliveryAction::ordinary(
                    SignalOSAction::Continue,
                ))),
            },
            SignalDisposition::Ignore => Ok(None),
            SignalDisposition::Handler(handler) => {
                let pre_handler_context = *uctx;
                if let Err(error) = pre_handler(memory, uctx) {
                    *uctx = pre_handler_context;
                    return Err(SignalPreHandlerError::Hook(error));
                }

                let interrupted_sp = uctx.sp();
                let stack = *self.stack.lock();
                let already_on_altstack = stack.contains_sp(interrupted_sp);
                let use_altstack = action.flags.contains(SignalActionFlags::ONSTACK)
                    && !stack.disabled()
                    && !already_on_altstack;
                let frame_stack = if already_on_altstack {
                    SignalFrameStack::NestedAltStack
                } else if use_altstack {
                    SignalFrameStack::FreshAltStack
                } else {
                    SignalFrameStack::Normal
                };
                let restorer = action.restorer.unwrap_or(self.proc.default_restorer);
                let prepared = match super::frame::prepare_signal_frame_with_fpstate_snapshot(
                    uctx,
                    restore_blocked,
                    stack,
                    frame_stack,
                    sig.clone(),
                    handler,
                    restorer,
                    snapshot,
                ) {
                    Ok(prepared) => prepared,
                    Err(_) => {
                        *uctx = pre_handler_context;
                        return Ok(Some(SignalDeliveryAction::ordinary(
                            SignalOSAction::CoreDump,
                        )));
                    }
                };
                // The data-plane token consumes itself on publication.  A
                // failed frame/restorer copy therefore cannot install the
                // handler context or update the manager mask.
                let published = match prepared.publish(memory) {
                    Ok(published) => published,
                    Err(_) => {
                        *uctx = pre_handler_context;
                        return Ok(Some(SignalDeliveryAction::ordinary(
                            SignalOSAction::CoreDump,
                        )));
                    }
                };
                published.install(uctx);
                // Linux sas_ss_reset(): while an AUTODISARM handler runs,
                // nested delivery and sigaltstack observe SS_DISABLE. The
                // original record is retained in ucontext and restored by
                // rt_sigreturn (or swapcontext's explicit frame restore).
                if stack.flags & (1 << 31) != 0 {
                    *self.stack.lock() = SignalStack::default();
                }

                let mut add_blocked = action.mask;
                if !action.flags.contains(SignalActionFlags::NODEFER) {
                    add_blocked.add(signo);
                }

                {
                    let mut blocked = self.blocked.lock();
                    *blocked |= add_blocked;
                }
                self.notify_pending();
                Ok(Some(SignalDeliveryAction::handler(restorer)))
            }
        }
    }

    /// Handles a signal without a pre-handler.
    ///
    /// This is the infallible compatibility wrapper for consumers that do not
    /// need a delivery seam. The wrapper supplies a no-op hook, so its
    /// behavior and signature remain unchanged.
    pub fn handle_signal<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
    ) -> Option<SignalOSAction> {
        match self.handle_signal_with_pre_handler(
            memory,
            uctx,
            restore_blocked,
            sig,
            action,
            no_signal_pre_handler::<M>,
        ) {
            Ok(action) => action,
            Err(error) => match error {},
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_signal_with_pre_delivery_and_fp_snapshot<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: SignalSet,
        sig: &SignalInfo,
        action: &SignalAction,
        pre_delivery: &mut impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
        snapshot: &mut impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> Result<Option<SignalDeliveryAction>, SignalDeliveryPreflight> {
        let saved_uctx = *uctx;
        if matches!(action.disposition, SignalDisposition::Handler(_)) {
            match pre_delivery(uctx, sig, action) {
                SignalDeliveryPreflight::Proceed => {}
                SignalDeliveryPreflight::Retry => {
                    *uctx = saved_uctx;
                    return Err(SignalDeliveryPreflight::Retry);
                }
                SignalDeliveryPreflight::Fault => {
                    *uctx = saved_uctx;
                    return Err(SignalDeliveryPreflight::Fault);
                }
                SignalDeliveryPreflight::Replaced => {
                    *uctx = saved_uctx;
                    return Err(SignalDeliveryPreflight::Replaced);
                }
                SignalDeliveryPreflight::Fatal => {
                    *uctx = saved_uctx;
                    return Err(SignalDeliveryPreflight::Fatal);
                }
            }
        }

        match self.handle_signal_with_pre_handler_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            sig,
            action,
            no_signal_pre_handler::<M>,
            &mut *snapshot,
        ) {
            Ok(result) => Ok(result),
            Err(error) => match error {},
        }
    }

    #[cold]
    fn check_signals_slow<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        excluded: SignalSet,
        pre_handler: &mut impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
    ) -> Result<Option<DeliveredSignal>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        let mut snapshot = || -> Result<SignalFpState, SignalFrameLayoutError> {
            panic!("embedding kernel must provide a full XSAVE snapshot")
        };
        self.check_signals_slow_with_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            excluded,
            pre_handler,
            &mut snapshot,
        )
    }

    #[cold]
    fn check_signals_slow_with_fp_snapshot<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        excluded: SignalSet,
        pre_handler: &mut impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
        snapshot: &mut impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> Result<Option<DeliveredSignal>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        let blocked = self.blocked.lock();
        let mask = !*blocked & !excluded;
        let restore_blocked = restore_blocked.unwrap_or_else(|| *blocked);
        drop(blocked);

        loop {
            let (queued, action, reset_claim) = {
                let Some(queued) = self
                    .dequeue_thread_signal_owned(&mask)
                    .or_else(|| self.proc.dequeue_signal_owned(&mask))
                else {
                    return Ok(None);
                };
                let (action, reset_claim) = self.proc.claim_delivery(queued.signo());
                (queued, action, reset_claim)
            };
            let sig = queued.into_info();
            let restartable = matches!(action.disposition, SignalDisposition::Handler(_))
                && action.flags.contains(SignalActionFlags::RESTART);

            let os_action = match self.handle_signal_with_pre_handler_and_fp_snapshot(
                memory,
                uctx,
                restore_blocked,
                &sig,
                &action,
                &mut *pre_handler,
                &mut *snapshot,
            ) {
                Ok(os_action) => os_action,
                Err(error) => {
                    if let Some(reset_claim) = reset_claim {
                        self.proc.finish_delivery(reset_claim, false);
                    }
                    return Err(error);
                }
            };
            if let Some(reset_claim) = reset_claim {
                self.proc.finish_delivery(
                    reset_claim,
                    matches!(
                        os_action,
                        Some(SignalDeliveryAction {
                            os_action: SignalOSAction::Handler,
                            ..
                        })
                    ),
                );
            }

            if let Some(outcome) = os_action {
                break Ok(Some(DeliveredSignal {
                    info: sig,
                    os_action: outcome.os_action,
                    restartable_handler: restartable
                        && outcome.os_action == SignalOSAction::Handler,
                    handler_restorer: outcome.handler_restorer,
                }));
            }
        }
    }

    #[cold]
    fn check_signals_slow_with_pre_delivery<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        excluded: SignalSet,
        pre_delivery: &mut impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
    ) -> SignalDeliveryResult {
        let mut snapshot = || -> Result<SignalFpState, SignalFrameLayoutError> {
            panic!("embedding kernel must provide a full XSAVE snapshot")
        };
        self.check_signals_slow_with_pre_delivery_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            excluded,
            pre_delivery,
            &mut snapshot,
        )
    }

    #[cold]
    #[allow(clippy::too_many_arguments)]
    fn check_signals_slow_with_pre_delivery_and_fp_snapshot<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        excluded: SignalSet,
        pre_delivery: &mut impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
        snapshot: &mut impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> SignalDeliveryResult {
        let blocked = self.blocked.lock();
        let mask = !*blocked & !excluded;
        let restore_blocked = restore_blocked.unwrap_or_else(|| *blocked);
        drop(blocked);

        loop {
            let Some(selected) = self.dequeue_signal_for_delivery(&mask) else {
                return SignalDeliveryResult::None;
            };
            let sig = selected
                .info()
                .cloned()
                .unwrap_or_else(|| SignalInfo::new_user(selected.signo(), 0, 0, 0));

            // Claim one-shot actions while the action table is locked, then
            // release it before any frame preparation/copyout.
            let (action, reset_claim) = self.proc.claim_delivery(sig.signo());
            self.selected_signo
                .store(sig.signo() as u8, Ordering::Release);
            self.selected_generation.store(
                selected.generation().map_or(0, SignalRecordGeneration::get),
                Ordering::Release,
            );

            // A default/ignored transition cannot run the pre-delivery
            // callback. Retire any stale arm which happened to select this
            // exact record rather than leaving it for a later signal.
            if !matches!(action.disposition, SignalDisposition::Handler(_)) {
                self.take_signal_delivery_bypass(sig.signo());
            }

            let restartable = matches!(action.disposition, SignalDisposition::Handler(_))
                && action.flags.contains(SignalActionFlags::RESTART);
            let saved_uctx = *uctx;
            let result = self.handle_signal_with_pre_delivery_and_fp_snapshot(
                memory,
                uctx,
                restore_blocked,
                &sig,
                &action,
                pre_delivery,
                &mut *snapshot,
            );

            self.selected_generation.store(0, Ordering::Release);
            self.selected_signo.store(0, Ordering::Release);

            let reset_committed = matches!(
                &result,
                Ok(Some(SignalDeliveryAction {
                    os_action: SignalOSAction::Handler,
                    ..
                }))
            );
            if let Some(reset_claim) = reset_claim {
                self.proc.finish_delivery(reset_claim, reset_committed);
            }

            match result {
                Ok(Some(outcome)) => {
                    return SignalDeliveryResult::Delivered(DeliveredSignal {
                        info: selected.into_info(),
                        os_action: outcome.os_action,
                        restartable_handler: restartable
                            && outcome.os_action == SignalOSAction::Handler,
                        handler_restorer: outcome.handler_restorer,
                    });
                }
                Ok(None) => {
                    // Ignored signals are consumed and the scan continues.
                }
                Err(preflight) => {
                    // The callback may have changed the machine context
                    // before rejecting the operation. Restore it, roll back
                    // SA_RESETHAND. Retry/Fault retain the exact record in its
                    // original queue. Replaced/Fatal deliberately consume it:
                    // the caller has either published an origin-bound
                    // replacement or is about to fail closed.
                    *uctx = saved_uctx;
                    return match preflight {
                        SignalDeliveryPreflight::Retry => {
                            selected.requeue(self);
                            SignalDeliveryResult::Retry
                        }
                        SignalDeliveryPreflight::Fault => {
                            selected.requeue(self);
                            SignalDeliveryResult::Fault
                        }
                        SignalDeliveryPreflight::Replaced => SignalDeliveryResult::Replaced,
                        SignalDeliveryPreflight::Fatal => SignalDeliveryResult::Fatal,
                        SignalDeliveryPreflight::Proceed => unreachable!(),
                    };
                }
            }
        }
    }

    /// Checks pending signals and handle them.
    ///
    /// Returns the signal number and the action the OS should take, if any.
    pub fn check_signals<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
    ) -> Option<DeliveredSignal> {
        let mut pre_delivery = |_: &mut UserContext, _: &SignalInfo, _: &SignalAction| {
            SignalDeliveryPreflight::Proceed
        };
        match self.check_signals_with_pre_delivery(memory, uctx, restore_blocked, &mut pre_delivery)
        {
            SignalDeliveryResult::Delivered(delivered) => Some(delivered),
            SignalDeliveryResult::None
            | SignalDeliveryResult::Retry
            | SignalDeliveryResult::Fault
            | SignalDeliveryResult::Replaced
            | SignalDeliveryResult::Fatal => None,
        }
    }

    /// Checks pending signals while obtaining each delivered handler's
    /// complete XSAVE snapshot from the caller.
    ///
    /// This is the data-plane seam for an embedding root that owns CPU save
    /// instructions. The returned [`DeliveredSignal`] is otherwise identical
    /// to [`Self::check_signals`].
    pub fn check_signals_with_fp_snapshot<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        snapshot: impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> Option<DeliveredSignal> {
        let mut pre_delivery = |_: &mut UserContext, _: &SignalInfo, _: &SignalAction| {
            SignalDeliveryPreflight::Proceed
        };
        match self.check_signals_with_pre_delivery_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            &mut pre_delivery,
            snapshot,
        ) {
            SignalDeliveryResult::Delivered(delivered) => Some(delivered),
            SignalDeliveryResult::None
            | SignalDeliveryResult::Retry
            | SignalDeliveryResult::Fault
            | SignalDeliveryResult::Replaced
            | SignalDeliveryResult::Fatal => None,
        }
    }

    /// Checks pending signals with a fallible pre-handler hook.
    ///
    /// The hook is called once at most, only after a deliverable signal's
    /// effective disposition has been resolved to a userspace handler and
    /// before [`crate::api::frame::prepare_signal_frame`] snapshots the interrupted context. Signals
    /// that are blocked, ignored, or handled by a default action never invoke
    /// it.
    ///
    /// If the hook fails, no frame or restorer is written, the in-flight
    /// `SA_RESETHAND` claim is aborted, and the hook error is returned in a
    /// typed [`SignalPreHandlerError`].
    pub fn check_signals_with_pre_handler<M, E>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        mut pre_handler: impl FnMut(&mut UserMemoryContext<'_, M>, &mut UserContext) -> Result<(), E>,
    ) -> Result<Option<DeliveredSignal>, SignalPreHandlerError<E>>
    where
        M: UserMemory + ?Sized,
    {
        let delivery = self.delivery.lock();
        if !self.is_registered() {
            return Ok(None);
        }
        // Fast path
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.possibly_has_signal.load(Ordering::Acquire)
        {
            return Ok(None);
        }
        let delivered = self.check_signals_slow(
            memory,
            uctx,
            restore_blocked,
            SignalSet::default(),
            &mut pre_handler,
        );
        drop(delivery);
        delivered
    }

    /// Checks pending signals through the kernel's pre-delivery rseq gate.
    /// Retry/Fault retain the selected record in its original queue and roll
    /// back any in-flight `SA_RESETHAND` claim. Replaced/Fatal consume the
    /// selected record, allowing the embedding kernel to deliver an exact
    /// replacement or fail closed.
    pub fn check_signals_with_pre_delivery<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        mut pre_delivery: impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
    ) -> SignalDeliveryResult {
        let delivery = self.delivery.lock();
        if !self.is_registered() {
            drop(delivery);
            return SignalDeliveryResult::None;
        }
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.possibly_has_signal.load(Ordering::Acquire)
        {
            drop(delivery);
            return SignalDeliveryResult::None;
        }
        let result = self.check_signals_slow_with_pre_delivery(
            memory,
            uctx,
            restore_blocked,
            SignalSet::default(),
            &mut pre_delivery,
        );
        drop(delivery);
        result
    }

    /// Checks pending signals through the pre-delivery gate and supplies a
    /// caller-owned full XSAVE snapshot for a signal that reaches the
    /// handler publication path.
    ///
    /// The snapshot callback is called only for a userspace handler after the
    /// pre-delivery callback returns [`SignalDeliveryPreflight::Proceed`] and
    /// checked frame layout succeeds. Default/ignored signals, rejected
    /// preflight outcomes, and copy/layout failures never invoke it.
    #[allow(clippy::too_many_arguments)]
    pub fn check_signals_with_pre_delivery_and_fp_snapshot<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        restore_blocked: Option<SignalSet>,
        mut pre_delivery: impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
        mut snapshot: impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> SignalDeliveryResult {
        let delivery = self.delivery.lock();
        if !self.is_registered() {
            drop(delivery);
            return SignalDeliveryResult::None;
        }
        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.possibly_has_signal.load(Ordering::Acquire)
        {
            drop(delivery);
            return SignalDeliveryResult::None;
        }
        let result = self.check_signals_slow_with_pre_delivery_and_fp_snapshot(
            memory,
            uctx,
            restore_blocked,
            SignalSet::default(),
            &mut pre_delivery,
            &mut snapshot,
        );
        drop(delivery);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_signal_wait_inner<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        waited: &SignalSet,
        restore_blocked: SignalSet,
        after_waited_scan: impl FnOnce(),
        pre_delivery: &mut impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
        snapshot: &mut impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> SignalWaitObservation {
        // This is the same sole delivery owner used by `check_signals`. Signal
        // producers remain non-blocking: publication is serialized by the
        // pending/action locks and never acquires this mutex.
        let delivery = self.delivery.lock();
        if !self.is_registered() {
            drop(delivery);
            return SignalWaitObservation::None;
        }

        // The selected class always gets the first observation. Every lock in
        // `dequeue_signal` is released before the owned signal is returned.
        if let Some(accepted) = self.dequeue_signal(waited) {
            drop(delivery);
            return SignalWaitObservation::Accepted(accepted);
        }

        // The production caller supplies an empty closure. Keeping this seam
        // private lets the state-machine test deterministically publish in the
        // otherwise tiny dequeue-to-delivery race window.
        after_waited_scan();

        if !self.possibly_has_signal.load(Ordering::Acquire)
            && !self.proc.possibly_has_signal.load(Ordering::Acquire)
        {
            drop(delivery);
            return SignalWaitObservation::None;
        }

        // A selected signal published after the first scan is deliberately
        // excluded from asynchronous delivery. Publication requests an
        // embedding wake, so the caller observes it as `Accepted` on the next
        // pass; it can never be consumed into a handler frame in this gap.
        let delivered = self.check_signals_slow_with_pre_delivery_and_fp_snapshot(
            memory,
            uctx,
            Some(restore_blocked),
            *waited,
            pre_delivery,
            snapshot,
        );
        drop(delivery);
        match delivered {
            SignalDeliveryResult::Delivered(delivered) => {
                SignalWaitObservation::Delivered(delivered)
            }
            SignalDeliveryResult::None => SignalWaitObservation::None,
            SignalDeliveryResult::Retry => SignalWaitObservation::Retry,
            SignalDeliveryResult::Fault => SignalWaitObservation::Fault,
            SignalDeliveryResult::Replaced => SignalWaitObservation::Replaced,
            SignalDeliveryResult::Fatal => SignalWaitObservation::Fatal,
        }
    }

    /// Atomically observes the two classes relevant to a synchronous signal
    /// wait: a signal selected by `waited`, then any other asynchronously
    /// deliverable signal.
    ///
    /// Signals in `waited` are excluded from the asynchronous selection even
    /// if they arrive after the selected dequeue. Producers therefore remain
    /// non-blocking while the next observation retains selected-signal
    /// priority and owns the exact queued record.
    pub fn observe_signal_wait<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        waited: &SignalSet,
        restore_blocked: SignalSet,
    ) -> SignalWaitObservation {
        let mut pre_delivery = |_: &mut UserContext, _: &SignalInfo, _: &SignalAction| {
            SignalDeliveryPreflight::Proceed
        };
        let mut snapshot = || -> Result<SignalFpState, SignalFrameLayoutError> {
            panic!("embedding kernel must provide a full XSAVE snapshot")
        };
        self.observe_signal_wait_inner(
            memory,
            uctx,
            waited,
            restore_blocked,
            || {},
            &mut pre_delivery,
            &mut snapshot,
        )
    }

    /// Observes a synchronous wait while routing asynchronous signals through
    /// the kernel's pre-delivery callback. Signals in `waited` remain
    /// synchronous and are returned as `Accepted` without a frame or callback.
    pub fn observe_signal_wait_with_pre_delivery<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        waited: &SignalSet,
        restore_blocked: SignalSet,
        pre_delivery: impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
    ) -> SignalWaitObservation {
        let mut snapshot = || -> Result<SignalFpState, SignalFrameLayoutError> {
            panic!("embedding kernel must provide a full XSAVE snapshot")
        };
        self.observe_signal_wait_with_pre_delivery_and_fp_snapshot(
            memory,
            uctx,
            waited,
            restore_blocked,
            pre_delivery,
            &mut snapshot,
        )
    }

    /// Observes a synchronous wait while routing asynchronous delivery
    /// through both the pre-delivery gate and the caller's FP snapshot seam.
    /// Waited signals remain synchronous and never invoke `snapshot`.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_signal_wait_with_pre_delivery_and_fp_snapshot<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        waited: &SignalSet,
        restore_blocked: SignalSet,
        mut pre_delivery: impl FnMut(
            &mut UserContext,
            &SignalInfo,
            &SignalAction,
        ) -> SignalDeliveryPreflight,
        mut snapshot: impl FnMut() -> Result<SignalFpState, SignalFrameLayoutError>,
    ) -> SignalWaitObservation {
        self.observe_signal_wait_inner(
            memory,
            uctx,
            waited,
            restore_blocked,
            || {},
            &mut pre_delivery,
            &mut snapshot,
        )
    }

    #[cfg(test)]
    fn observe_signal_wait_with_hook<M: UserMemory + ?Sized>(
        &self,
        memory: &mut UserMemoryContext<'_, M>,
        uctx: &mut UserContext,
        waited: &SignalSet,
        restore_blocked: SignalSet,
        after_waited_scan: impl FnOnce(),
    ) -> SignalWaitObservation {
        let mut pre_delivery = |_: &mut UserContext, _: &SignalInfo, _: &SignalAction| {
            SignalDeliveryPreflight::Proceed
        };
        let mut snapshot = || -> Result<SignalFpState, SignalFrameLayoutError> {
            panic!("embedding kernel must provide a full XSAVE snapshot")
        };
        self.observe_signal_wait_inner(
            memory,
            uctx,
            waited,
            restore_blocked,
            after_waited_scan,
            &mut pre_delivery,
            &mut snapshot,
        )
    }

    /// Validates an owned signal frame without publishing any state.
    ///
    /// The caller must copy the complete frame from userspace before calling
    /// this method. Address predicates keep kernel address-space policy out of
    /// this reusable signal crate.
    pub fn prepare_restore(
        &self,
        current: &UserContext,
        frame: SignalFrame,
        valid_program_counter: impl FnOnce(usize) -> bool,
        valid_stack_pointer: impl FnOnce(usize) -> bool,
        validate_stack: impl FnOnce(
            &SignalStack,
            usize,
            &SignalStack,
        ) -> Result<(), SignalStackRestoreError>,
    ) -> Result<PreparedSignalRestore, SignalContextError> {
        let current_stack = *self.stack.lock();
        prepare_signal_restore(
            current,
            frame,
            valid_program_counter,
            valid_stack_pointer,
            current_stack,
            validate_stack,
        )
    }

    /// Commits a previously validated signal restore without failure.
    pub fn commit_restore(&self, uctx: &mut UserContext, prepared: PreparedSignalRestore) {
        let (context, blocked_value, stack_value) = prepared.into_parts();
        let mut blocked = self.blocked.lock();
        let mut stack = self.stack.lock();
        *uctx = context;
        *blocked = blocked_value;
        if let Some(restored) = stack_value {
            *stack = restored;
        }
        self.possibly_has_signal.store(true, Ordering::Release);
        drop(stack);
        drop(blocked);
        self.notify_pending();
    }

    /// Sends a signal, preparing any queue record outside spin locks.
    ///
    /// Returns publication and wakeup state separately.
    ///
    /// The preparation closure is skipped for ignored signals and coalesced
    /// standard signals, and is never called under a pending/actions lock.
    #[must_use = "the caller must handle queue-admission failure"]
    pub fn try_send_signal_with<E>(
        &self,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        self.try_send_signal_for_endpoint(EndpointSendMode::Active, sig, prepare)
    }

    /// Sends directly to an exited endpoint whose private queue is retained
    /// for its unreaped identity. The exact manager identity is required;
    /// ordinary active routing never considers this endpoint.
    #[must_use = "the caller must handle queue-admission failure"]
    pub fn try_send_retained_signal_with<E>(
        &self,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        self.try_send_signal_for_endpoint(EndpointSendMode::Retained, sig, prepare)
    }

    fn try_send_signal_for_endpoint<E>(
        &self,
        mode: EndpointSendMode,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ThreadSignalSendOutcome, E> {
        let signo = sig.signo();
        let inactive = || ThreadSignalSendOutcome {
            published: false,
            wake: false,
            generation: None,
        };

        // The first pass avoids queue preparation for an inactive, ignored,
        // or already-coalesced signal. Job-control cancellation happens first
        // and is shared with the commit pass below.
        let (preflight, detached) = self.proc.with_action_update(|owner| {
            let mut generation_detached = DetachedSignal::empty();
            let lifecycle = self.lifecycle.lock();
            if !mode.accepts(*lifecycle) {
                drop(lifecycle);
                return (Some(inactive()), generation_detached);
            }
            if ProcessSignalManager::has_generation_effect(signo) {
                self.proc
                    .apply_generation_effect_locked(signo, &mut generation_detached);
                if !mode.accepts(*lifecycle) {
                    drop(lifecycle);
                    return (Some(inactive()), generation_detached);
                }
            }
            let blocked = self.signal_blocked(signo);
            let actions = owner.lock();
            let ignored = ProcessSignalManager::action_ignored(&actions, signo)
                && !blocked
                && !self.signal_real_blocked(signo);
            if ignored {
                drop(actions);
                drop(lifecycle);
                return (Some(inactive()), generation_detached);
            }
            if !signo.is_realtime() && self.pending.lock().set.has(signo) {
                self.possibly_has_signal.store(true, Ordering::Release);
                let outcome = ThreadSignalSendOutcome {
                    published: false,
                    wake: mode.accepts(ENDPOINT_ACTIVE) && !blocked,
                    generation: None,
                };
                drop(actions);
                drop(lifecycle);
                return (Some(outcome), generation_detached);
            }
            drop(actions);
            drop(lifecycle);
            (None, generation_detached)
        });
        drop(detached);
        if let Some(outcome) = preflight {
            return Ok(outcome);
        }

        // Preparation is outside signal-state locks. The commit pass
        // rechecks endpoint identity/state after this potentially blocking
        // operation and safely returns the unused owner on rejection.
        let mut prepared = Some(prepare(sig)?);
        let ((outcome, unused, ignored, blocked, exhausted, generation), detached) =
            self.proc.with_action_update(|owner| {
                let mut generation_detached = DetachedSignal::empty();
                let lifecycle = self.lifecycle.lock();
                if !mode.accepts(*lifecycle) {
                    drop(lifecycle);
                    return (
                        (None, prepared.take(), true, false, true, None),
                        generation_detached,
                    );
                }
                if ProcessSignalManager::has_generation_effect(signo) {
                    self.proc
                        .apply_generation_effect_locked(signo, &mut generation_detached);
                    if !mode.accepts(*lifecycle) {
                        drop(lifecycle);
                        return (
                            (None, prepared.take(), true, false, true, None),
                            generation_detached,
                        );
                    }
                }

                let blocked = self.signal_blocked(signo);
                let actions = owner.lock();
                let ignored = ProcessSignalManager::action_ignored(&actions, signo)
                    && !blocked
                    && !self.signal_real_blocked(signo);
                let mut outcome = None;
                let mut exhausted = false;
                let mut generation = None;
                if !ignored {
                    let mut pending = self.pending.lock();
                    let coalesced = !signo.is_realtime() && pending.set.has(signo);
                    if !coalesced {
                        match self.assign_generation(
                            prepared
                                .as_mut()
                                .expect("prepared signal is retained until publication"),
                        ) {
                            Ok(next) => generation = next,
                            Err(()) => exhausted = true,
                        }
                    }
                    if !exhausted && !coalesced {
                        outcome = Some(
                            pending.publish(
                                prepared
                                    .take()
                                    .expect("prepared signal is retained until publication"),
                            ),
                        );
                    }
                }
                drop(actions);
                drop(lifecycle);
                (
                    (
                        outcome,
                        prepared.take(),
                        ignored,
                        blocked,
                        exhausted,
                        generation,
                    ),
                    generation_detached,
                )
            });
        drop(detached);
        drop(unused);
        if exhausted {
            return Ok(inactive());
        }
        let published = outcome.is_some_and(|outcome| outcome.finish());
        if published {
            self.notify_pending();
        }
        if !ignored {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        Ok(ThreadSignalSendOutcome {
            published,
            wake: !ignored && mode.accepts(ENDPOINT_ACTIVE) && !blocked,
            generation: published.then_some(generation).flatten(),
        })
    }

    fn allocate_generation(&self) -> Option<SignalRecordGeneration> {
        let mut current = self.next_generation.load(Ordering::Acquire);
        loop {
            // Zero is a sticky exhaustion marker. It is never reused as a
            // valid token and is never mapped back to an earlier generation.
            if current == 0 {
                return None;
            }
            let next = current.checked_add(1).unwrap_or(0);
            match self.next_generation.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(SignalRecordGeneration::new(current)),
                Err(observed) => current = observed,
            }
        }
    }

    /// Assigns one exact generation to an actually publishable prepared
    /// record. Allocation-free RT fallback deliberately returns `Ok(None)`;
    /// exhaustion returns `Err(())` so every publication entrance fails
    /// closed without creating an untagged record.
    fn assign_generation(
        &self,
        prepared: &mut PreparedSignal,
    ) -> Result<Option<SignalRecordGeneration>, ()> {
        if !prepared.supports_generation() {
            return Ok(None);
        }
        let generation = self.allocate_generation().ok_or(())?;
        prepared.set_generation(generation);
        Ok(Some(generation))
    }

    /// Arms a one-shot bypass for one exact forced signal record. The
    /// generation must come from an actual publication outcome; coalesced,
    /// ignored, fallback, and inactive sends cannot arm a meaningful bypass.
    pub fn arm_signal_delivery_bypass(&self, signo: Signo, generation: SignalRecordGeneration) {
        self.delivery_bypass_signo
            .store(signo as u8, Ordering::Release);
        self.delivery_bypass
            .store(generation.get(), Ordering::Release);
    }

    /// Consumes the one-shot bypass only when the currently selected record
    /// matches both the armed signal number and exact record generation.
    pub fn take_signal_delivery_bypass(&self, signo: Signo) -> bool {
        let selected_generation = self.selected_generation.load(Ordering::Acquire);
        if selected_generation == 0
            || self.selected_signo.load(Ordering::Acquire) != signo as u8
            || self.delivery_bypass_signo.load(Ordering::Acquire) != signo as u8
        {
            return false;
        }
        let consumed = self
            .delivery_bypass
            .compare_exchange(selected_generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if consumed {
            self.delivery_bypass_signo.store(0, Ordering::Release);
        }
        consumed
    }

    /// Sends a signal through the allocation-free fallback path.
    #[must_use]
    pub fn send_unqueued_signal(&self, sig: SignalInfo) -> bool {
        match self.try_send_signal_with(sig, |sig| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(sig))
        }) {
            Ok(outcome) => outcome.wake,
            Err(error) => match error {},
        }
    }

    /// Gets the blocked signals.
    pub fn blocked(&self) -> SignalSet {
        *self.blocked.lock()
    }

    /// Sets the blocked signals. Return the old value.
    pub fn set_blocked(&self, mut set: SignalSet) -> SignalSet {
        set.remove(Signo::SIGKILL);
        set.remove(Signo::SIGSTOP);
        self.possibly_has_signal.store(true, Ordering::Release);
        let mut guard = self.blocked.lock();
        let old = *guard;
        *guard = set;
        drop(guard);
        if old.bits() != set.bits() {
            self.notify_pending();
        }
        old
    }

    /// Checks if a signal is blocked.
    pub fn signal_blocked(&self, signo: Signo) -> bool {
        self.blocked.lock().has(signo)
    }

    pub fn signal_real_blocked(&self, signo: Signo) -> bool {
        self.real_blocked.lock().is_some_and(|set| set.has(signo))
    }

    pub fn set_real_blocked(&self, set: Option<SignalSet>) {
        *self.real_blocked.lock() = set;
    }

    /// Gets the signal stack.
    pub fn stack(&self) -> SignalStack {
        *self.stack.lock()
    }

    /// Sets the signal stack.
    pub fn set_stack(&self, stack: SignalStack) {
        *self.stack.lock() = stack;
    }

    /// Gets current pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set | self.proc.pending()
    }

    /// Detaches all thread-private pending records under the lock and destroys
    /// them after releasing it.
    pub fn flush_pending(&self) {
        let detached = {
            let mut pending = self.pending.lock();
            let detached = pending.take_all();
            self.possibly_has_signal.store(false, Ordering::Release);
            detached
        };
        drop(detached);
    }

    /// Detaches every thread-directed instance of one signal and releases
    /// queue ownership after dropping the pending lock.
    pub fn flush_signal(&self, signo: Signo) {
        let detached = {
            let mut pending = self.pending.lock();
            let detached = pending.take_signal(signo);
            if pending.set.is_empty() {
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            detached
        };
        drop(detached);
    }

    pub(crate) fn detach_signal_into(&self, signo: Signo, detached: &mut DetachedSignal) {
        let mut pending = self.pending.lock();
        pending.detach_signal_into(signo, detached);
        if pending.set.is_empty() {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
    }
}

impl Drop for ThreadSignalManager {
    fn drop(&mut self) {
        *self.lifecycle.lock() = ENDPOINT_CANCELLED;
        let registration = { self.registration.lock().take() };
        if let Some(entry) = registration {
            entry.deactivate();
        }
    }
}

#[cfg(test)]
mod signal_wait_tests {
    use alloc::sync::Arc;
    use core::mem::MaybeUninit;

    use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

    use super::{SignalWaitObservation, ThreadSignalManager};
    use crate::{
        SignalInfo, SignalSet, Signo,
        api::{ProcessSignalManager, SharedSignalActions, SignalActions},
        arch::UserContext,
    };

    struct NoUserAccess;

    // SAFETY: this provider never dereferences a user address and never claims
    // to have initialized or written any byte.
    unsafe impl UserMemory for NoUserAccess {
        fn read(&mut self, _start: usize, _dst: &mut [MaybeUninit<u8>]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, _start: usize, _src: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    fn registered_thread() -> Arc<ThreadSignalManager> {
        let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let process = Arc::new(ProcessSignalManager::new(actions, 0));
        let thread = ThreadSignalManager::try_new(process).unwrap();
        thread.try_register(1).unwrap().commit().unwrap();
        thread
    }

    #[test]
    fn requeued_delivery_notifies_a_reader_armed_during_preflight() {
        use alloc::task::Wake;
        use core::{
            sync::atomic::{AtomicUsize, Ordering},
            task::Waker,
        };

        use super::{SignalDeliveryPreflight, SignalDeliveryResult};
        use crate::{SignalAction, SignalDisposition};

        struct Counter(AtomicUsize);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let count = Arc::new(Counter(AtomicUsize::new(0)));
        let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let mut manager = ProcessSignalManager::new(actions, 0);
        manager.set_pending_waker(Waker::from(count.clone()));
        let process = Arc::new(manager);
        let thread = ThreadSignalManager::try_new(process.clone()).unwrap();
        thread.try_register(1).unwrap().commit().unwrap();
        process
            .try_replace_action(
                Signo::SIGUSR1,
                SignalAction {
                    disposition: SignalDisposition::Handler(0x1000),
                    ..SignalAction::default()
                },
            )
            .unwrap();
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGUSR1);
        for process_directed in [false, true] {
            for fault in [false, true] {
                thread.set_blocked(SignalSet::default());
                let info = SignalInfo::new_user(Signo::SIGUSR1, 0, 7, 0);
                if process_directed {
                    let _ = process.send_unqueued_signal(info);
                } else {
                    let _ = thread.send_unqueued_signal(info);
                }
                let mut provider = NoUserAccess;
                let mut memory = UserMemoryContext::new(&mut provider);
                let mut context = UserContext::new(0, 0, 0);
                let result = thread.check_signals_with_pre_delivery(
                    &mut memory,
                    &mut context,
                    None,
                    |_, _, _| {
                        // The record is temporarily absent. A signalfd reader
                        // can now block this signal and arm on an empty queue.
                        assert!(!thread.pending().has(Signo::SIGUSR1));
                        thread.set_blocked(mask);
                        count.0.store(0, Ordering::SeqCst);
                        if fault {
                            SignalDeliveryPreflight::Fault
                        } else {
                            SignalDeliveryPreflight::Retry
                        }
                    },
                );
                assert!(matches!(
                    result,
                    SignalDeliveryResult::Retry | SignalDeliveryResult::Fault
                ));
                assert_eq!(count.0.load(Ordering::SeqCst), 1);
                assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());
            }
        }
    }

    #[test]
    fn waited_arrival_in_dequeue_delivery_gap_stays_synchronous() {
        let thread = registered_thread();
        let mut waited = SignalSet::default();
        waited.add(Signo::SIGUSR1);
        let mut provider = NoUserAccess;
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut context = UserContext::new(0, 0, 0);
        let sender = Arc::clone(&thread);

        let first = thread.observe_signal_wait_with_hook(
            &mut memory,
            &mut context,
            &waited,
            SignalSet::default(),
            move || {
                assert!(
                    sender.send_unqueued_signal(SignalInfo::new_user(Signo::SIGUSR1, 1, 1, 0,))
                );
            },
        );
        assert!(matches!(first, SignalWaitObservation::None));
        assert!(thread.pending().has(Signo::SIGUSR1));

        let second =
            thread.observe_signal_wait(&mut memory, &mut context, &waited, SignalSet::default());
        assert!(matches!(
            second,
            SignalWaitObservation::Accepted(info) if info.signo() == Signo::SIGUSR1
        ));
        assert!(!thread.pending().has(Signo::SIGUSR1));
    }

    #[test]
    fn selected_signal_precedes_an_existing_async_delivery() {
        let thread = registered_thread();
        assert!(thread.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 1, 1, 0,)));
        assert!(thread.send_unqueued_signal(SignalInfo::new_user(Signo::SIGUSR1, 1, 1, 0,)));

        let mut waited = SignalSet::default();
        waited.add(Signo::SIGUSR1);
        let mut provider = NoUserAccess;
        let mut memory = UserMemoryContext::new(&mut provider);
        let mut context = UserContext::new(0, 0, 0);

        let selected =
            thread.observe_signal_wait(&mut memory, &mut context, &waited, SignalSet::default());
        assert!(matches!(
            selected,
            SignalWaitObservation::Accepted(info) if info.signo() == Signo::SIGUSR1
        ));

        let delivered =
            thread.observe_signal_wait(&mut memory, &mut context, &waited, SignalSet::default());
        assert!(matches!(
            delivered,
            SignalWaitObservation::Delivered(signal)
                if signal.info.signo() == Signo::SIGTERM
        ));
    }

    #[test]
    fn generation_exhaustion_is_sticky_and_fails_closed() {
        let thread = registered_thread();
        thread
            .next_generation
            .store(u64::MAX - 1, core::sync::atomic::Ordering::Release);

        assert_eq!(thread.allocate_generation().unwrap().get(), u64::MAX - 1);
        assert_eq!(thread.allocate_generation().unwrap().get(), u64::MAX);
        assert!(thread.allocate_generation().is_none());
        assert!(thread.allocate_generation().is_none());

        let outcome = thread.send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGTERM));
        assert!(!outcome);
        assert!(!thread.pending().has(Signo::SIGTERM));
    }
}
