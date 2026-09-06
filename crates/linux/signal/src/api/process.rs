use alloc::{
    alloc::AllocError,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    array,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    task::Waker,
};

use kspin::SpinNoIrq;
use spin::Mutex;

use crate::{
    DefaultSignalAction, DequeuedSignal, DetachedSignal, PendingSignals, PreparedSignal,
    SignalAction, SignalActionFlags, SignalDisposition, SignalInfo, SignalSet, Signo,
    api::ThreadSignalManager,
};

#[derive(Debug, PartialEq, Eq)]
struct ResetClaim {
    generation: u64,
}

/// A claim on one `SA_RESETHAND` disposition generation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResetDeliveryClaim {
    signo: Signo,
    claim: ResetClaim,
}

/// Signal actions used to initialize one process signal manager.
///
/// After construction, callers query and replace actions through
/// [`ProcessSignalManager`]. Keeping the mutable table private makes action
/// generation and one-shot delivery claims impossible to bypass.
pub struct SignalActions {
    actions: [SignalAction; 64],
    generations: [u64; 64],
    reset_claims: [Option<ResetClaim>; 64],
}

impl Default for SignalActions {
    fn default() -> Self {
        Self {
            actions: array::from_fn(|_| SignalAction::default()),
            generations: [0; 64],
            reset_claims: array::from_fn(|_| None),
        }
    }
}

impl SignalActions {
    fn index(signo: Signo) -> usize {
        signo as usize - 1
    }

    fn is_uncatchable(signo: Signo) -> bool {
        matches!(signo, Signo::SIGKILL | Signo::SIGSTOP)
    }

    fn reset_claim_is_current(&self, index: usize) -> bool {
        self.reset_claims[index]
            .as_ref()
            .is_some_and(|claim| claim.generation == self.generations[index])
    }

    fn effective_action(&self, signo: Signo) -> SignalAction {
        if Self::is_uncatchable(signo) {
            return SignalAction::default();
        }
        let index = Self::index(signo);
        if self.reset_claim_is_current(index) {
            SignalAction::default()
        } else {
            self.actions[index]
        }
    }

    fn replace(
        &mut self,
        signo: Signo,
        action: SignalAction,
    ) -> Result<SignalAction, SignalActionUpdateError> {
        if Self::is_uncatchable(signo) {
            return Err(SignalActionUpdateError::UncatchableSignal);
        }
        let index = Self::index(signo);
        let old = self.effective_action(signo);
        let generation = self.generations[index]
            .checked_add(1)
            .ok_or(SignalActionUpdateError::GenerationExhausted)?;
        self.generations[index] = generation;
        self.actions[index] = action;
        Ok(old)
    }

    pub(crate) fn claim_delivery(
        &mut self,
        signo: Signo,
    ) -> (SignalAction, Option<ResetDeliveryClaim>) {
        if Self::is_uncatchable(signo) {
            return (SignalAction::default(), None);
        }
        let index = Self::index(signo);
        if self.reset_claim_is_current(index) {
            return (SignalAction::default(), None);
        }

        let action = self.actions[index];
        let reset = matches!(action.disposition, SignalDisposition::Handler(_))
            && action.flags.contains(SignalActionFlags::RESETHAND);
        if !reset {
            return (action, None);
        }

        let claim = ResetClaim {
            generation: self.generations[index],
        };
        self.reset_claims[index] = Some(ResetClaim {
            generation: claim.generation,
        });
        (action, Some(ResetDeliveryClaim { signo, claim }))
    }

    pub(crate) fn finish_delivery(&mut self, reservation: ResetDeliveryClaim, commit: bool) {
        let index = Self::index(reservation.signo);
        if self.reset_claims[index].as_ref() != Some(&reservation.claim) {
            return;
        }
        self.reset_claims[index] = None;
        if commit && self.generations[index] == reservation.claim.generation {
            self.actions[index] = SignalAction::default();
        }
    }

    fn snapshot(&self) -> Self {
        Self {
            actions: array::from_fn(|index| {
                if self.reset_claim_is_current(index) {
                    SignalAction::default()
                } else {
                    self.actions[index]
                }
            }),
            generations: [0; 64],
            reset_claims: array::from_fn(|_| None),
        }
    }

    /// Builds the action table which Linux installs after a successful exec.
    ///
    /// Caught dispositions do not survive exec, while dispositions explicitly
    /// set to `SIG_IGN` do.  Claims and generation state belong to the old
    /// owner and are intentionally not copied into the new owner.
    fn exec_snapshot(&self) -> Self {
        Self {
            actions: array::from_fn(|index| {
                let action = if self.reset_claim_is_current(index) {
                    SignalAction::default()
                } else {
                    self.actions[index]
                };
                match action.disposition {
                    SignalDisposition::Handler(_) | SignalDisposition::Default => {
                        SignalAction::default()
                    }
                    SignalDisposition::Ignore => SignalAction {
                        disposition: SignalDisposition::Ignore,
                        ..SignalAction::default()
                    },
                }
            }),
            generations: [0; 64],
            reset_claims: array::from_fn(|_| None),
        }
    }
}

/// The shared process signal-action owner used by Linux `sighand` sharing.
///
/// The action table and the update gate intentionally live in one opaque
/// owner.  A process manager owns only an `Arc` to this value, so managers
/// which share a sighand observe the same disposition generations and
/// `SA_RESETHAND` claims while retaining private pending queues and child
/// registries.
pub struct SharedSignalActions {
    update: Mutex<()>,
    actions: SpinNoIrq<SignalActions>,
}

impl SharedSignalActions {
    /// Fallibly allocates a shared action owner from an initial action table.
    pub fn try_new(actions: SignalActions) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            update: Mutex::new(()),
            actions: SpinNoIrq::new(actions),
        })
    }

    /// Copies the currently effective actions into an independent owner.
    ///
    /// A delivery which has claimed `SA_RESETHAND` is already observed as the
    /// default disposition by the source table, so the copied owner also
    /// materializes that in-flight one-shot action as default.  The shared
    /// update gate is held while the source table is copied, which gives the
    /// snapshot the same linearization domain as disposition replacement.
    pub fn try_snapshot(self: &Arc<Self>) -> Result<Arc<Self>, AllocError> {
        let update = self.update.lock();
        let actions = {
            let actions = self.actions.lock();
            actions.snapshot()
        };
        drop(update);

        Self::try_new(actions)
    }

    /// Returns whether two shared action owners are the same allocation.
    pub fn ptr_eq(left: impl AsRef<Self>, right: impl AsRef<Self>) -> bool {
        core::ptr::eq(left.as_ref(), right.as_ref())
    }

    /// Exposes the action-table lock to the sibling API implementation only.
    ///
    /// The owner remains opaque to users of this crate; public action access
    /// goes through [`ProcessSignalManager`].
    pub(crate) fn table_lock(&self) -> &SpinNoIrq<SignalActions> {
        &self.actions
    }

    /// Exposes the shared update lock to the sibling API implementation only.
    pub(crate) fn update_lock(&self) -> &Mutex<()> {
        &self.update
    }

    pub(crate) fn lock(&self) -> kspin::SpinNoIrqGuard<'_, SignalActions> {
        self.table_lock().lock()
    }
}

/// A delivery claim paired with the exact shared owner from which it was
/// selected.  Exec may detach that owner before frame publication; finishing
/// against the current manager owner would otherwise strand a peer's
/// `SA_RESETHAND` claim.
pub(crate) struct OwnedResetDeliveryClaim {
    owner: Arc<SharedSignalActions>,
    claim: ResetDeliveryClaim,
}

/// One preallocated entry in the process thread-signal registry.
///
/// Registry snapshots retain entries, never thread endpoints. Rollback only
/// marks it cancelled; stale or dead entries are compacted while the next
/// snapshot is built outside every spin lock.
pub(crate) struct RegisteredThread {
    tid: u32,
    thread: Weak<ThreadSignalManager>,
    state: AtomicU8,
}

const REGISTRATION_PENDING: u8 = 0;
const REGISTRATION_ACTIVE: u8 = 1;
const REGISTRATION_CANCELLED: u8 = 2;
const REGISTRATION_RETAINED: u8 = 3;

impl RegisteredThread {
    pub(crate) fn try_new(
        tid: u32,
        thread: &Arc<ThreadSignalManager>,
    ) -> Result<Arc<Self>, alloc::alloc::AllocError> {
        Arc::try_new(Self {
            tid,
            thread: Arc::downgrade(thread),
            state: AtomicU8::new(REGISTRATION_PENDING),
        })
    }

    pub(crate) fn activate(&self) {
        self.state.store(REGISTRATION_ACTIVE, Ordering::Release);
    }

    pub(crate) fn deactivate(&self) {
        self.state.store(REGISTRATION_CANCELLED, Ordering::Release);
    }

    pub(crate) fn retain_pending_only(&self) {
        self.state.store(REGISTRATION_RETAINED, Ordering::Release);
    }

    pub(crate) fn matches(&self, tid: u32, thread: *const ThreadSignalManager) -> bool {
        self.tid == tid && self.thread.as_ptr() == thread
    }

    pub(crate) fn is_live(&self) -> bool {
        self.state.load(Ordering::Acquire) != REGISTRATION_CANCELLED
            && self.thread.strong_count() != 0
    }

    pub(crate) fn claims_tid(&self, tid: u32) -> bool {
        self.tid == tid && self.is_live()
    }

    pub(crate) fn upgrade(&self) -> Option<(u32, Arc<ThreadSignalManager>)> {
        if self.state.load(Ordering::Acquire) != REGISTRATION_ACTIVE {
            return None;
        }
        self.thread.upgrade().map(|thread| (self.tid, thread))
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == REGISTRATION_ACTIVE
    }

    pub(crate) fn upgrade_for_action_update(&self) -> Option<Arc<ThreadSignalManager>> {
        if !matches!(
            self.state.load(Ordering::Acquire),
            REGISTRATION_ACTIVE | REGISTRATION_RETAINED
        ) {
            return None;
        }
        self.thread.upgrade()
    }
}

pub(crate) type ThreadRegistry = Vec<Arc<RegisteredThread>>;

/// Result of publishing one process-directed signal.
///
/// `published` distinguishes a record owned by this send from an ignored or
/// coalesced signal. `wake_tid` retains the historical wakeup selection used
/// by kernel integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessSignalSendOutcome {
    /// Whether this send published a new pending record.
    pub published: bool,
    /// One currently unblocked retained endpoint selected for wakeup.
    pub wake_tid: Option<u32>,
}

struct RetainedThreadEndpoint {
    registration: Arc<RegisteredThread>,
    tid: u32,
    thread: Arc<ThreadSignalManager>,
}

impl RetainedThreadEndpoint {
    fn is_active(&self) -> bool {
        self.registration.is_active()
    }
}

/// Fallibly prepared process-directed routing state for a non-blocking commit.
///
/// Construction snapshots and retains every currently active thread endpoint
/// outside IRQ-disabled locks. [`Self::publish`] then performs only bounded
/// atomic loads, short signal-state spin-lock mutations, and ownership moves.
/// This lets a kernel commit signal publication while holding an unrelated
/// credential or liveness spin lock without acquiring this crate's sleepable
/// thread-registry mutex or allocating.
///
/// The snapshot is deliberately one-shot and remains owned by the returned
/// [`DeferredProcessSignalSend`] so endpoint `Arc` destruction cannot occur in
/// the commit call. A thread which cancels or re-registers after preparation
/// is rejected by its exact registration identity rather than confused with a
/// reused TID.
#[must_use = "publishing or dropping the prepared send releases retained endpoints"]
pub struct PreparedProcessSignalSend {
    process: Arc<ProcessSignalManager>,
    endpoints: Vec<RetainedThreadEndpoint>,
}

impl PreparedProcessSignalSend {
    /// Publishes one already allocated and accounted signal record.
    ///
    /// This method does not allocate and returns every unused queue record and
    /// retained endpoint in a deferred owner. The caller must carry that owner
    /// out of all IRQ-disabled outer critical sections before calling
    /// [`DeferredProcessSignalSend::finish`] or dropping it.
    pub fn publish(self, prepared: PreparedSignal) -> DeferredProcessSignalSend {
        let process = &self.process;
        let signo = prepared.signo();

        let mut prepared = Some(prepared);
        let mut unused = None;
        let mut published = false;
        let mut accepted = false;
        let detached = process.with_action_update(|actions_owner| {
            let mut generation_detached = DetachedSignal::empty();
            let lifecycle = process.lifecycle.lock();
            if *lifecycle == PROCESS_ENDPOINT_ACTIVE {
                if ProcessSignalManager::has_generation_effect(signo) {
                    process.apply_generation_effect_locked(signo, &mut generation_detached);
                }
                if *lifecycle == PROCESS_ENDPOINT_ACTIVE {
                    let blocked_by_any_thread = self.endpoints.iter().any(|endpoint| {
                        endpoint.is_active()
                            && (endpoint.thread.signal_blocked(signo)
                                || endpoint.thread.signal_real_blocked(signo))
                    });
                    let actions = actions_owner.lock();
                    if !ProcessSignalManager::action_ignored(&actions, signo)
                        || blocked_by_any_thread
                    {
                        let outcome = process
                            .pending
                            .lock()
                            .publish(prepared.take().expect("prepared signal is retained"));
                        (published, unused) = outcome.into_parts();
                        accepted = true;
                    }
                    drop(actions);
                }
            }
            drop(lifecycle);
            generation_detached
        });
        // Detached realtime nodes own queue-account charges.  Keep their
        // destruction outside the shared update, lifecycle, and pending
        // guards; publication itself only moves those owners.
        drop(detached);

        let wake_tid = if accepted {
            process.possibly_has_signal.store(true, Ordering::Release);
            self.endpoints.iter().find_map(|endpoint| {
                (endpoint.is_active() && !endpoint.thread.signal_blocked(signo))
                    .then_some(endpoint.tid)
            })
        } else {
            None
        };

        DeferredProcessSignalSend {
            _prepared: self,
            outcome: ProcessSignalSendOutcome {
                published,
                wake_tid,
            },
            unused: unused.or(prepared),
        }
    }
}

/// Ownership deferred out of a process-signal publication critical section.
///
/// Dropping this value can release queue-account, queue-node, process-manager,
/// registry-entry, and thread-endpoint ownership. It must therefore be
/// retained until every unrelated IRQ-disabled outer guard has been released.
#[must_use = "finish or drop this value only after outer spin locks are released"]
pub struct DeferredProcessSignalSend {
    _prepared: PreparedProcessSignalSend,
    outcome: ProcessSignalSendOutcome,
    unused: Option<PreparedSignal>,
}

impl Drop for DeferredProcessSignalSend {
    fn drop(&mut self) {
        // Publication can happen under an embedding-owned IRQ-off guard.
        // This deferred owner must leave that guard before callbacks run.
        if self.outcome.published {
            self._prepared.process.notify_pending();
        }
    }
}

impl DeferredProcessSignalSend {
    /// Returns the fixed publication and wakeup result without releasing
    /// deferred ownership.
    pub const fn outcome(&self) -> ProcessSignalSendOutcome {
        self.outcome
    }

    /// Releases retained routing ownership in the caller's current context and
    /// returns any queue record that publication did not consume.
    pub fn finish(mut self) -> (ProcessSignalSendOutcome, Option<PreparedSignal>) {
        let unused = self.unused.take();
        (self.outcome, unused)
    }
}

/// Why a process signal disposition could not be replaced atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalActionUpdateError {
    /// Allocating a fallible thread-registry snapshot failed.
    NoMemory,
    /// The per-signal generation space is exhausted.
    ///
    /// Reusing a generation would let an old `SA_RESETHAND` delivery claim
    /// overwrite a newer disposition, so exhaustion is reported explicitly.
    GenerationExhausted,
    /// Linux rejects all `rt_sigaction` dispositions for `SIGKILL` and
    /// `SIGSTOP` with `EINVAL`; these signals remain default and uncatchable.
    UncatchableSignal,
}

/// Default finite process-local thread endpoint ceiling.
pub const SIGNAL_THREAD_REGISTRY_LIMIT: usize = 65_536;

/// Invalid process-signal-manager resource configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalManagerConfigError {
    /// `usize::MAX` was rejected rather than treated as an unbounded registry.
    UnboundedThreadRegistry,
}

impl From<AllocError> for SignalActionUpdateError {
    fn from(_: AllocError) -> Self {
        Self::NoMemory
    }
}

/// Process-level signal manager.
pub struct ProcessSignalManager {
    /// Readiness notification independent of unblocked signal delivery.
    pending_waker: Option<Waker>,
    /// The process-level shared pending signals
    pending: SpinNoIrq<PendingSignals>,

    /// Publication state for the shared pending endpoint. Retention keeps
    /// already published records alive through the unreaped lifetime while
    /// cancellation rejects and drains every later publication.
    lifecycle: SpinNoIrq<u8>,

    /// Shared Linux sighand identity, action table, and update domain.
    ///
    /// The indirection is manager-local: an exec swaps this pointer while
    /// peers which still share the old sighand retain their original owner.
    pub(crate) actions: SpinNoIrq<Arc<SharedSignalActions>>,

    /// Serializes owner lookup/update with an exec owner swap.  The shared
    /// owner update lock is acquired after this gate, so a manager never uses
    /// an owner which an exec has already detached.
    action_update: Mutex<()>,

    /// The default restorer function.
    pub(crate) default_restorer: usize,

    /// Thread-level signal managers. Kernel targets use the sleepable mutex so
    /// snapshot `Arc` acquisition never runs with interrupts disabled.
    pub(crate) children: Mutex<Option<Arc<ThreadRegistry>>>,

    thread_limit: usize,

    pub(crate) possibly_has_signal: AtomicBool,
}

/// Fallibly prepared private sighand replacement for a successful exec.
///
/// The token borrows its manager so it cannot be committed to a different
/// process signal endpoint.  Its replacement owner is allocated while exec is
/// still recoverable; [`Self::commit`] only copies the fixed-size action table
/// and swaps the manager's owner pointer, so it cannot report an allocation
/// failure after the caller has started tearing down the old image.
#[must_use = "commit or drop the prepared exec sighand token"]
pub struct PreparedExecUnshare<'a> {
    manager: &'a ProcessSignalManager,
    replacement: Arc<SharedSignalActions>,
}

impl PreparedExecUnshare<'_> {
    /// Commits the prepared sighand replacement without allocation.
    ///
    /// The manager gate excludes another owner swap, while the source-owner
    /// update gate linearizes this exec against every peer disposition update.
    /// The source is copied again at this point rather than reusing the
    /// prepare-time snapshot, so an update which won the race during sibling
    /// teardown is visible in the execing manager's private owner.
    pub fn commit(self) {
        let Self {
            manager,
            replacement,
        } = self;
        let manager_update = manager.action_update.lock();
        let source = manager.action_owner();
        let source_update = source.update_lock().lock();
        let replacement_update = replacement.update_lock().lock();
        let actions = {
            let actions = source.lock();
            actions.exec_snapshot()
        };
        {
            let mut replacement_actions = replacement.lock();
            *replacement_actions = actions;
        }
        let replacement_for_owner = replacement.clone();
        let old = {
            let mut owner = manager.actions.lock();
            core::mem::replace(&mut *owner, replacement_for_owner)
        };
        drop(replacement_update);
        drop(source_update);
        drop(manager_update);
        drop(old);
    }
}

const PROCESS_ENDPOINT_ACTIVE: u8 = 1;
const PROCESS_ENDPOINT_RETAINED: u8 = 2;
const PROCESS_ENDPOINT_CANCELLED: u8 = 3;

const JOB_CONTROL_STOP_SIGNALS: [Signo; 4] = [
    Signo::SIGSTOP,
    Signo::SIGTSTP,
    Signo::SIGTTIN,
    Signo::SIGTTOU,
];

impl ProcessSignalManager {
    /// Installs an embedding-owned readiness notifier before this manager is
    /// shared. It runs outside signal-state locks, including for blocked signals.
    pub fn set_pending_waker(&mut self, waker: Waker) {
        self.pending_waker = Some(waker);
    }

    pub(crate) fn notify_pending(&self) {
        if let Some(waker) = &self.pending_waker {
            waker.wake_by_ref();
        }
    }

    pub(crate) fn action_ignored(actions: &SignalActions, signo: Signo) -> bool {
        match actions.effective_action(signo).disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            _ => false,
        }
    }

    pub(crate) fn has_generation_effect(signo: Signo) -> bool {
        signo == Signo::SIGCONT || JOB_CONTROL_STOP_SIGNALS.contains(&signo)
    }

    /// Applies Linux's generation-time job-control cancellation while the
    /// caller owns the shared action-update gate. Both active and retained
    /// private endpoint queues are included; cancelled endpoints no longer
    /// retain meaningful pending state.
    pub(crate) fn apply_generation_effect_locked(
        &self,
        signo: Signo,
        detached: &mut DetachedSignal,
    ) {
        let flush: &[Signo] = if signo == Signo::SIGCONT {
            &JOB_CONTROL_STOP_SIGNALS
        } else if JOB_CONTROL_STOP_SIGNALS.contains(&signo) {
            core::slice::from_ref(&Signo::SIGCONT)
        } else {
            return;
        };

        for &pending_signo in flush {
            self.detach_signal_into(pending_signo, detached);
        }

        let registry = self.children_registry_snapshot();
        if let Some(registry) = registry.as_deref() {
            for entry in registry {
                if let Some(thread) = entry.upgrade_for_action_update() {
                    for &pending_signo in flush {
                        thread.detach_signal_into(pending_signo, detached);
                    }
                }
            }
        }
        drop(registry);
    }

    /// Creates a process signal manager with the default finite thread limit.
    pub fn new(actions: Arc<SharedSignalActions>, default_restorer: usize) -> Self {
        Self::with_validated_thread_limit(actions, default_restorer, SIGNAL_THREAD_REGISTRY_LIMIT)
    }

    /// Creates a process signal manager with a caller-selected finite thread
    /// endpoint limit.
    pub fn try_with_thread_limit(
        actions: Arc<SharedSignalActions>,
        default_restorer: usize,
        thread_limit: usize,
    ) -> Result<Self, SignalManagerConfigError> {
        if thread_limit == usize::MAX {
            return Err(SignalManagerConfigError::UnboundedThreadRegistry);
        }
        Ok(Self::with_validated_thread_limit(
            actions,
            default_restorer,
            thread_limit,
        ))
    }

    fn with_validated_thread_limit(
        actions: Arc<SharedSignalActions>,
        default_restorer: usize,
        thread_limit: usize,
    ) -> Self {
        Self {
            pending_waker: None,
            pending: SpinNoIrq::new(PendingSignals::default()),
            lifecycle: SpinNoIrq::new(PROCESS_ENDPOINT_ACTIVE),
            actions: SpinNoIrq::new(actions),
            action_update: Mutex::new(()),
            default_restorer,
            children: Mutex::new(None),
            thread_limit,
            possibly_has_signal: AtomicBool::new(false),
        }
    }

    /// Returns the immutable process-local endpoint ceiling.
    pub const fn thread_limit(&self) -> usize {
        self.thread_limit
    }

    fn action_owner(&self) -> Arc<SharedSignalActions> {
        self.actions.lock().clone()
    }

    /// Runs a bounded action operation while the manager's owner pointer and
    /// the shared owner update domain are both stable.  The manager gate is
    /// intentionally acquired before the shared gate; every owner mutation,
    /// endpoint transition, and exec swap follows this order.
    pub(crate) fn with_action_update<R>(
        &self,
        operation: impl FnOnce(&SharedSignalActions) -> R,
    ) -> R {
        let _manager_update = self.action_update.lock();
        let owner = self.action_owner();
        let _owner_update = owner.update_lock().lock();
        operation(&owner)
    }

    /// Runs an action-table operation against the manager's current owner.
    pub(crate) fn with_action_table<R>(
        &self,
        operation: impl FnOnce(&mut SignalActions) -> R,
    ) -> R {
        self.with_action_update(|owner| {
            let mut actions = owner.lock();
            operation(&mut actions)
        })
    }

    /// Prepares a private sighand owner for a successful exec.
    ///
    /// The source action table is snapshotted and the replacement owner is
    /// allocated while the manager and source-owner update gates are held.
    /// The gates are released before the caller interrupts or waits for
    /// sibling threads, so peer action updates are not blocked by teardown.
    /// [`PreparedExecUnshare::commit`] takes a fresh fixed-size snapshot under
    /// the source-owner gate; updates linearized before that commit therefore
    /// cannot be overwritten by this prepare-time snapshot.
    pub fn try_prepare_exec_unshare(&self) -> Result<PreparedExecUnshare<'_>, AllocError> {
        let manager_update = self.action_update.lock();
        let source = self.action_owner();
        let source_update = source.update_lock().lock();
        let actions = {
            let actions = source.lock();
            actions.exec_snapshot()
        };
        let replacement = SharedSignalActions::try_new(actions)?;
        drop(source_update);
        drop(manager_update);
        Ok(PreparedExecUnshare {
            manager: self,
            replacement,
        })
    }

    /// Claims one `SA_RESETHAND` action while retaining the exact owner to
    /// which the delivery must later be finished.  This matters if exec swaps
    /// the manager to a private owner between selection and frame handling.
    pub(crate) fn claim_delivery(
        &self,
        signo: Signo,
    ) -> (SignalAction, Option<OwnedResetDeliveryClaim>) {
        let manager_update = self.action_update.lock();
        let owner = self.action_owner();
        let owner_update = owner.update_lock().lock();
        let result = {
            let mut actions = owner.lock();
            actions.claim_delivery(signo)
        };
        drop(owner_update);
        drop(manager_update);
        let (action, claim) = result;
        (
            action,
            claim.map(|claim| OwnedResetDeliveryClaim { owner, claim }),
        )
    }

    pub(crate) fn finish_delivery(&self, reservation: OwnedResetDeliveryClaim, commit: bool) {
        let _owner_update = reservation.owner.update_lock().lock();
        let mut actions = reservation.owner.lock();
        actions.finish_delivery(reservation.claim, commit);
    }

    /// Freezes the process-directed queue while retaining already published
    /// records for the unreaped lifetime. Prepared and direct sends recheck
    /// this state while committing, so a sender which loses the race returns
    /// its prepared queue owner to the caller's deferred cleanup path.
    pub fn retain_pending_only(&self) {
        self.with_action_update(|_| {
            let mut lifecycle = self.lifecycle.lock();
            if *lifecycle == PROCESS_ENDPOINT_ACTIVE {
                *lifecycle = PROCESS_ENDPOINT_RETAINED;
            }
        });
    }

    /// Cancels the process-directed endpoint and drains every retained record.
    /// Queue nodes and their accounting are released only after all signal
    /// state guards have been dropped.
    pub fn retire_pending(&self) {
        let detached = self.with_action_update(|_| {
            let mut lifecycle = self.lifecycle.lock();
            if *lifecycle == PROCESS_ENDPOINT_CANCELLED {
                drop(lifecycle);
                return None;
            }
            *lifecycle = PROCESS_ENDPOINT_CANCELLED;
            let detached = {
                let mut pending = self.pending.lock();
                let detached = pending.take_all();
                self.possibly_has_signal.store(false, Ordering::Release);
                detached
            };
            drop(lifecycle);
            Some(detached)
        });
        drop(detached);
    }

    pub(crate) fn children_registry_snapshot(&self) -> Option<Arc<ThreadRegistry>> {
        self.children.lock().clone()
    }

    /// Returns the opaque shared sighand identity owned by this manager.
    pub fn shared_actions(&self) -> Arc<SharedSignalActions> {
        self.action_owner()
    }

    /// Returns the currently effective action for one signal.
    pub fn action(&self, signo: Signo) -> SignalAction {
        self.with_action_table(|actions| actions.effective_action(signo))
    }

    /// Copies the effective action table for a new, independent process.
    /// In-flight `SA_RESETHAND` claims are materialized as default actions.
    pub fn actions_snapshot(&self) -> SignalActions {
        self.with_action_table(|actions| actions.snapshot())
    }

    fn try_children_snapshot(&self) -> Result<Vec<Arc<ThreadSignalManager>>, AllocError> {
        let registry = self.children_registry_snapshot();
        let len = registry.as_deref().map_or(0, Vec::len);
        let mut snapshot = Vec::new();
        snapshot.try_reserve_exact(len).map_err(|_| AllocError)?;

        if let Some(registry) = registry.as_deref() {
            for entry in registry {
                if let Some(child) = entry.upgrade_for_action_update() {
                    snapshot.push(child);
                }
            }
        }
        drop(registry);
        Ok(snapshot)
    }

    /// Prepares a bounded endpoint snapshot for one non-blocking authorized
    /// process-signal commit.
    ///
    /// Allocation, registry locking, weak upgrades, and all temporary `Arc`
    /// destruction finish before this method returns. `usize::MAX` cannot be
    /// selected as the registry limit, and the snapshot reserves no more than
    /// the current finite registry length.
    pub fn try_prepare_signal_send(
        self: &Arc<Self>,
    ) -> Result<PreparedProcessSignalSend, AllocError> {
        let registry = self.children_registry_snapshot();
        let len = registry.as_deref().map_or(0, Vec::len);
        let mut endpoints = Vec::new();
        endpoints.try_reserve_exact(len).map_err(|_| AllocError)?;

        if let Some(registry) = registry.as_deref() {
            for entry in registry {
                if let Some((tid, thread)) = entry.upgrade() {
                    endpoints.push(RetainedThreadEndpoint {
                        registration: entry.clone(),
                        tid,
                        thread,
                    });
                }
            }
        }
        drop(registry);
        Ok(PreparedProcessSignalSend {
            process: self.clone(),
            endpoints,
        })
    }

    /// Atomically replaces one disposition with respect to signal
    /// publication. Switching to an ignored disposition also detaches that
    /// signal from the process and every registered thread before the action
    /// gate is released. Account Arcs and RT nodes are destroyed afterwards.
    pub fn try_replace_action(
        &self,
        signo: Signo,
        action: SignalAction,
    ) -> Result<SignalAction, SignalActionUpdateError> {
        // Linux rejects SIGKILL and SIGSTOP before any action-table or
        // endpoint work.  Keep this check at the public entry point as well
        // as in SignalActions::replace so the errno contract wins over a
        // possible allocation failure while preparing the child snapshot.
        if SignalActions::is_uncatchable(signo) {
            return Err(SignalActionUpdateError::UncatchableSignal);
        }
        // Keep any retained endpoint snapshot in the result on an action
        // replacement failure.  In particular, generation exhaustion is
        // reported after the snapshot has been prepared; letting the closure
        // unwind normally would drop those endpoint Arcs while the shared
        // action/update guards are still held.
        let result = self.with_action_update(|owner| {
            let children = match self.try_children_snapshot() {
                Ok(children) => children,
                Err(_) => {
                    return Err((SignalActionUpdateError::NoMemory, Vec::new()));
                }
            };
            let mut detached = DetachedSignal::empty();
            let old_action = {
                let mut actions = owner.lock();
                let old_action = match actions.replace(signo, action) {
                    Ok(old_action) => old_action,
                    Err(error) => return Err((error, children)),
                };
                if Self::action_ignored(&actions, signo) {
                    let empty = {
                        let mut pending = self.pending.lock();
                        pending.detach_signal_into(signo, &mut detached);
                        pending.set.is_empty()
                    };
                    if empty {
                        self.possibly_has_signal.store(false, Ordering::Release);
                    }
                    for child in &children {
                        child.detach_signal_into(signo, &mut detached);
                    }
                }
                old_action
            };
            Ok::<_, (SignalActionUpdateError, Vec<Arc<ThreadSignalManager>>)>((
                old_action, detached, children,
            ))
        });

        match result {
            Ok((old_action, detached, children)) => {
                // Neither queue nodes nor strong endpoint snapshots are
                // destroyed while an actions, registry, or pending
                // SpinNoIrq guard is held.
                drop(detached);
                drop(children);
                Ok(old_action)
            }
            Err((error, children)) => {
                // The closure has already released every short signal-state
                // guard.  This is the generation-exhaustion and allocation
                // failure path, so release the retained endpoint snapshot
                // only now as well.
                drop(children);
                Err(error)
            }
        }
    }

    pub(crate) fn dequeue_signal_owned(&self, mask: &SignalSet) -> Option<DequeuedSignal> {
        {
            let mut guard = self.pending.lock();
            let result = guard.dequeue_signal(mask);
            if guard.set.is_empty() {
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            result
        }
    }

    /// Returns one exact dequeued record to the front of the process queue.
    /// Delivery Retry/Fault uses this rather than reconstructing siginfo so
    /// RT FIFO order and queue accounting remain unchanged.
    pub(crate) fn requeue_signal(&self, signal: DequeuedSignal) {
        let detached = self.with_action_update(|_| {
            let lifecycle = self.lifecycle.lock();
            if *lifecycle == PROCESS_ENDPOINT_CANCELLED {
                drop(lifecycle);
                return Some(signal);
            }
            let mut pending = self.pending.lock();
            signal.requeue_front(&mut pending);
            self.possibly_has_signal.store(true, Ordering::Release);
            drop(pending);
            drop(lifecycle);
            None
        });
        // A process retirement can win after a thread selected a process
        // record but before Retry/Fault returns it.  Do not republish into a
        // cancelled endpoint; release that exact record only after all
        // lifecycle, pending, and shared-update guards have left scope.
        let requeued = detached.is_none();
        drop(detached);
        if requeued {
            self.notify_pending();
        }
    }

    /// Checks if a signal is ignored by the process.
    pub fn signal_ignored(&self, signo: Signo) -> bool {
        self.with_action_table(|actions| Self::action_ignored(actions, signo))
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    pub fn can_restart(&self, signo: Signo) -> bool {
        self.with_action_table(|actions| {
            actions
                .effective_action(signo)
                .flags
                .contains(SignalActionFlags::RESTART)
        })
    }

    fn blocked_by_any_thread(&self, signo: Signo) -> bool {
        let registry = self.children_registry_snapshot();
        let blocked = registry.as_deref().is_some_and(|registry| {
            registry.iter().any(|entry| {
                entry.upgrade().is_some_and(|(_, thread)| {
                    thread.signal_blocked(signo) || thread.signal_real_blocked(signo)
                })
            })
        });
        drop(registry);
        blocked
    }

    fn wake_thread_for(&self, signo: Signo) -> Option<u32> {
        let registry = self.children_registry_snapshot();
        let result = registry.as_deref().and_then(|registry| {
            registry.iter().find_map(|entry| {
                let (tid, thread) = entry.upgrade()?;
                (!thread.signal_blocked(signo)).then_some(tid)
            })
        });
        drop(registry);
        result
    }

    /// Sends a signal, preparing any owned queue record outside spin locks.
    ///
    /// The preparation closure is skipped for ignored signals and coalesced
    /// standard signals. It is never invoked while an actions, children, or
    /// pending SpinNoIrq guard is held.
    ///
    /// Returns publication and wakeup state separately. This distinction is
    /// required by preallocated kernel notifications: an ignored or
    /// coalesced signal must not be mistaken for an owned pending record.
    #[must_use = "the caller must handle queue-admission failure"]
    pub fn try_send_signal_with<E>(
        &self,
        sig: SignalInfo,
        prepare: impl FnOnce(SignalInfo) -> Result<PreparedSignal, E>,
    ) -> Result<ProcessSignalSendOutcome, E> {
        let signo = sig.signo();
        let inactive = || ProcessSignalSendOutcome {
            published: false,
            wake_tid: None,
        };

        // Generation-time job-control cancellation is deliberately part of
        // the same preflight and commit protocol as direct and prepared
        // publication. It runs before ignored/coalesced decisions and before
        // any queue record is prepared.
        let (preflight, detached) = self.with_action_update(|owner| {
            let mut generation_detached = DetachedSignal::empty();
            let lifecycle = self.lifecycle.lock();
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                drop(lifecycle);
                return (Some(inactive()), generation_detached);
            }
            if Self::has_generation_effect(signo) {
                self.apply_generation_effect_locked(signo, &mut generation_detached);
            }
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                drop(lifecycle);
                return (Some(inactive()), generation_detached);
            }
            let blocked_by_any_thread = self.blocked_by_any_thread(signo);
            let actions = owner.lock();
            if Self::action_ignored(&actions, signo) && !blocked_by_any_thread {
                drop(actions);
                drop(lifecycle);
                return (Some(inactive()), generation_detached);
            }
            let coalesced = !signo.is_realtime() && self.pending.lock().set.has(signo);
            drop(actions);
            drop(lifecycle);
            (
                coalesced.then(|| ProcessSignalSendOutcome {
                    published: false,
                    wake_tid: self.wake_thread_for(signo),
                }),
                generation_detached,
            )
        });
        drop(detached);
        if let Some(outcome) = preflight {
            if outcome.published || outcome.wake_tid.is_some() {
                self.possibly_has_signal.store(true, Ordering::Release);
            }
            return Ok(outcome);
        }

        // Preparation is outside every signal-state guard. A concurrent
        // retain/cancel or action transition may win before commit; the
        // commit-side state and action checks below then reject the prepared
        // owner and release it only after all guards have left scope.
        let mut prepared = Some(prepare(sig)?);
        let ((outcome, unused, ignored), detached) = self.with_action_update(|owner| {
            let mut generation_detached = DetachedSignal::empty();
            let lifecycle = self.lifecycle.lock();
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                drop(lifecycle);
                return ((None, prepared.take(), true), generation_detached);
            }
            if Self::has_generation_effect(signo) {
                self.apply_generation_effect_locked(signo, &mut generation_detached);
            }
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                drop(lifecycle);
                return ((None, prepared.take(), true), generation_detached);
            }
            let blocked_by_any_thread = self.blocked_by_any_thread(signo);
            let actions = owner.lock();
            let ignored = Self::action_ignored(&actions, signo) && !blocked_by_any_thread;
            let mut outcome = None;
            if !ignored {
                let mut pending = self.pending.lock();
                if signo.is_realtime() || !pending.set.has(signo) {
                    outcome = Some(
                        pending.publish(prepared.take().expect("prepared signal is retained")),
                    );
                }
            }
            drop(actions);
            drop(lifecycle);
            ((outcome, prepared.take(), ignored), generation_detached)
        });
        drop(detached);
        drop(unused);

        let published = outcome.is_some_and(|outcome| outcome.finish());
        if published {
            self.notify_pending();
        }
        if !ignored {
            self.possibly_has_signal.store(true, Ordering::Release);
        }
        Ok(ProcessSignalSendOutcome {
            published,
            wake_tid: (!ignored).then(|| self.wake_thread_for(signo)).flatten(),
        })
    }

    /// Sends a signal through the allocation-free fallback path.
    #[must_use]
    pub fn send_unqueued_signal(&self, sig: SignalInfo) -> Option<u32> {
        match self.try_send_signal_with(sig, |sig| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(sig))
        }) {
            Ok(outcome) => outcome.wake_tid,
            Err(error) => match error {},
        }
    }

    /// Gets currently pending signals.
    pub fn pending(&self) -> SignalSet {
        self.pending.lock().set
    }

    /// Detaches all pending records under the lock and destroys them after
    /// releasing it.
    pub fn flush_pending(&self) {
        let detached = {
            let mut pending = self.pending.lock();
            let detached = pending.take_all();
            self.possibly_has_signal.store(false, Ordering::Release);
            detached
        };
        drop(detached);
    }

    /// Detaches every process-directed instance of one signal and releases
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

    fn detach_signal_into(&self, signo: Signo, detached: &mut DetachedSignal) {
        let empty = {
            let mut pending = self.pending.lock();
            pending.detach_signal_into(signo, detached);
            pending.set.is_empty()
        };
        if empty {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessSignalManager, SharedSignalActions, SignalActionUpdateError, SignalActions,
    };
    use crate::{SignalAction, SignalActionFlags, SignalDisposition, Signo};

    fn reset_action(handler: usize) -> SignalAction {
        SignalAction {
            disposition: SignalDisposition::Handler(handler),
            flags: SignalActionFlags::RESETHAND,
            ..SignalAction::default()
        }
    }

    #[test]
    fn reset_claim_abort_can_retry_without_reusing_an_outstanding_identity() {
        let signo = Signo::SIGTERM;
        let mut actions = SignalActions::default();
        actions.replace(signo, reset_action(1)).unwrap();

        let (_, first) = actions.claim_delivery(signo);
        assert!(matches!(
            actions.effective_action(signo).disposition,
            SignalDisposition::Default
        ));
        actions.finish_delivery(first.unwrap(), false);
        assert!(matches!(
            actions.effective_action(signo).disposition,
            SignalDisposition::Handler(1)
        ));

        let (_, second) = actions.claim_delivery(signo);
        actions.finish_delivery(second.unwrap(), true);
        assert!(matches!(
            actions.effective_action(signo).disposition,
            SignalDisposition::Default
        ));
    }

    #[test]
    fn action_generation_exhaustion_is_explicit_and_non_mutating() {
        let signo = Signo::SIGTERM;
        let index = SignalActions::index(signo);
        let mut actions = SignalActions::default();
        actions.actions[index] = reset_action(1);
        actions.generations[index] = u64::MAX;

        assert!(matches!(
            actions.replace(signo, reset_action(2)),
            Err(SignalActionUpdateError::GenerationExhausted)
        ));
        assert_eq!(actions.generations[index], u64::MAX);
        assert!(matches!(
            actions.effective_action(signo).disposition,
            SignalDisposition::Handler(1)
        ));
    }

    #[test]
    fn uncatchable_signal_replacement_is_rejected_without_mutation() {
        for signo in [Signo::SIGKILL, Signo::SIGSTOP] {
            let mut actions = SignalActions::default();
            assert!(matches!(
                actions.replace(signo, reset_action(1)),
                Err(SignalActionUpdateError::UncatchableSignal)
            ));
            assert!(matches!(
                actions.effective_action(signo).disposition,
                SignalDisposition::Default
            ));
        }
    }

    #[test]
    fn snapshot_materializes_an_inflight_reset_claim_as_default() {
        let signo = Signo::SIGTERM;
        let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let claim = {
            let mut actions = shared.lock();
            actions.replace(signo, reset_action(1)).unwrap();
            let (_, claim) = actions.claim_delivery(signo);
            claim.unwrap()
        };

        let snapshot = shared.try_snapshot().unwrap();
        assert!(matches!(
            snapshot.lock().effective_action(signo).disposition,
            SignalDisposition::Default
        ));

        shared.lock().finish_delivery(claim, false);
        assert!(matches!(
            shared.lock().effective_action(signo).disposition,
            SignalDisposition::Handler(1)
        ));
        assert!(matches!(
            snapshot.lock().effective_action(signo).disposition,
            SignalDisposition::Default
        ));
    }

    #[test]
    fn manager_generation_failure_does_not_mutate_shared_owner() {
        let signo = Signo::SIGTERM;
        let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let manager = ProcessSignalManager::new(shared.clone(), 0);
        let index = SignalActions::index(signo);
        {
            let mut actions = shared.lock();
            actions.actions[index] = reset_action(1);
            actions.generations[index] = u64::MAX;
        }

        assert!(matches!(
            manager.try_replace_action(signo, reset_action(2)),
            Err(SignalActionUpdateError::GenerationExhausted)
        ));
        assert!(SharedSignalActions::ptr_eq(
            manager.shared_actions(),
            &shared
        ));
        assert!(matches!(
            manager.action(signo).disposition,
            SignalDisposition::Handler(1)
        ));
    }

    #[test]
    fn exec_unshare_resets_caught_actions_without_reusing_generation_space() {
        let signo = Signo::SIGTERM;
        let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let manager = ProcessSignalManager::new(shared.clone(), 0);
        let index = SignalActions::index(signo);
        {
            let mut actions = shared.lock();
            actions.actions[index] = reset_action(1);
            actions.generations[index] = u64::MAX;
        }

        manager.try_prepare_exec_unshare().unwrap().commit();

        assert!(!SharedSignalActions::ptr_eq(
            manager.shared_actions(),
            &shared
        ));
        assert!(matches!(
            manager.action(signo).disposition,
            SignalDisposition::Default
        ));
    }
}
