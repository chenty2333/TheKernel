use alloc::{
    alloc::AllocError,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    array,
    ops::{Index, IndexMut},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use kspin::SpinNoIrq;

use crate::{
    DefaultSignalAction, DetachedSignal, PendingSignals, PreparedSignal, SignalAction,
    SignalActionFlags, SignalDisposition, SignalInfo, SignalSet, Signo, api::ThreadSignalManager,
};

// Host-side kernel tests unify `axsync/multitask` through the wider kernel
// graph but cannot safely construct an ArceOS task context. Let those tests
// force this one serialization lock back to its spin-backed implementation;
// real kernel targets keep the sleepable mutex selected by `multitask`.
#[cfg(any(not(feature = "spin-action-update"), target_os = "none"))]
type ActionUpdateMutex<T> = axsync::Mutex<T>;
#[cfg(all(feature = "spin-action-update", not(target_os = "none")))]
type ActionUpdateMutex<T> = SpinNoIrq<T>;

/// Signal actions for a process.
#[derive(Clone)]
pub struct SignalActions(pub(crate) [SignalAction; 64]);

impl Default for SignalActions {
    fn default() -> Self {
        Self(array::from_fn(|_| SignalAction::default()))
    }
}

impl Index<Signo> for SignalActions {
    type Output = SignalAction;

    fn index(&self, signo: Signo) -> &SignalAction {
        &self.0[signo as usize - 1]
    }
}

impl IndexMut<Signo> for SignalActions {
    fn index_mut(&mut self, signo: Signo) -> &mut SignalAction {
        &mut self.0[signo as usize - 1]
    }
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

    fn upgrade(&self) -> Option<(u32, Arc<ThreadSignalManager>)> {
        if self.state.load(Ordering::Acquire) != REGISTRATION_ACTIVE {
            return None;
        }
        self.thread.upgrade().map(|thread| (self.tid, thread))
    }

    fn upgrade_for_action_update(&self) -> Option<Arc<ThreadSignalManager>> {
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
    pub published: bool,
    pub wake_tid: Option<u32>,
}

/// Process-level signal manager.
pub struct ProcessSignalManager {
    /// The process-level shared pending signals
    pending: SpinNoIrq<PendingSignals>,

    /// Publication state for the shared pending endpoint. Final process exit
    /// retains existing records but rejects late direct publication; reap then
    /// cancels and drains the queue.
    lifecycle: SpinNoIrq<u8>,

    /// The signal actions
    pub actions: Arc<SpinNoIrq<SignalActions>>,

    /// The default restorer function.
    pub(crate) default_restorer: usize,

    /// Thread-level signal managers.
    pub(crate) children: SpinNoIrq<Option<Arc<ThreadRegistry>>>,

    /// Serializes registry publication with action transitions. Real kernel
    /// targets use the sleepable mutex selected by `multitask`; the explicit
    /// host-test override is spin-backed because no ArceOS task exists there.
    /// Immutable snapshots are allocated without holding a SpinNoIrq guard.
    pub(crate) action_update: ActionUpdateMutex<()>,

    pub(crate) possibly_has_signal: AtomicBool,
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
    pub(crate) fn has_generation_effect(signo: Signo) -> bool {
        signo == Signo::SIGCONT || JOB_CONTROL_STOP_SIGNALS.contains(&signo)
    }

    /// Applies Linux's generation-time SIGCONT/stop queue cancellation while
    /// the caller owns `action_update`. ACTIVE and RETAINED endpoints are both
    /// included; cancelled endpoints cannot retain meaningful pending state.
    pub(crate) fn apply_generation_effect_locked(
        &self,
        signo: Signo,
        detached: &mut DetachedSignal,
    ) {
        let flush = if signo == Signo::SIGCONT {
            &JOB_CONTROL_STOP_SIGNALS[..]
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

    fn action_ignored(actions: &SignalActions, signo: Signo) -> bool {
        match &actions[signo].disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            _ => false,
        }
    }

    /// Creates a new process signal manager.
    pub fn new(actions: Arc<SpinNoIrq<SignalActions>>, default_restorer: usize) -> Self {
        Self {
            pending: SpinNoIrq::new(PendingSignals::default()),
            lifecycle: SpinNoIrq::new(PROCESS_ENDPOINT_ACTIVE),
            actions,
            default_restorer,
            children: SpinNoIrq::new(None),
            action_update: ActionUpdateMutex::new(()),
            possibly_has_signal: AtomicBool::new(false),
        }
    }

    /// Freezes the shared pending endpoint at final process exit. Existing
    /// records remain charged through zombie lifetime, while a sender that
    /// prepared concurrently must fail its commit-side state recheck.
    pub fn retain_pending_only(&self) {
        let update = self.action_update.lock();
        let mut lifecycle = self.lifecycle.lock();
        if *lifecycle == PROCESS_ENDPOINT_ACTIVE {
            *lifecycle = PROCESS_ENDPOINT_RETAINED;
        }
        drop(lifecycle);
        drop(update);
    }

    /// Cancels the shared endpoint and releases every retained queue record.
    /// Queue ownership is destroyed only after the lifecycle and pending guards
    /// have been released.
    pub fn retire_pending(&self) {
        let update = self.action_update.lock();
        let mut lifecycle = self.lifecycle.lock();
        if *lifecycle == PROCESS_ENDPOINT_CANCELLED {
            return;
        }
        *lifecycle = PROCESS_ENDPOINT_CANCELLED;
        let detached = self.pending.lock().take_all();
        self.possibly_has_signal.store(false, Ordering::Release);
        drop(lifecycle);
        drop(update);
        drop(detached);
    }

    pub(crate) fn children_registry_snapshot(&self) -> Option<Arc<ThreadRegistry>> {
        self.children.lock().clone()
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

    /// Atomically replaces one disposition with respect to signal
    /// publication. Switching to an ignored disposition also detaches that
    /// signal from the process and every registered thread before the action
    /// gate is released. Account Arcs and RT nodes are destroyed afterwards.
    pub fn try_replace_action(
        &self,
        signo: Signo,
        action: SignalAction,
    ) -> Result<SignalAction, AllocError> {
        let update = self.action_update.lock();
        let children = self.try_children_snapshot()?;
        let mut detached = DetachedSignal::empty();
        let old_action = {
            let mut actions = self.actions.lock();
            let old_action = actions[signo].clone();
            actions[signo] = action;
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

        // Neither queue nodes nor strong endpoint snapshots are destroyed
        // while an actions, registry, or pending SpinNoIrq guard is held.
        drop(update);
        drop(detached);
        drop(children);
        Ok(old_action)
    }

    pub(crate) fn dequeue_signal(&self, mask: &SignalSet) -> Option<SignalInfo> {
        let result = {
            let mut guard = self.pending.lock();
            let result = guard.dequeue_signal(mask);
            if guard.set.is_empty() {
                self.possibly_has_signal.store(false, Ordering::Release);
            }
            result
        };
        result.map(|signal| signal.into_info())
    }

    /// Checks if a signal is ignored by the process.
    pub fn signal_ignored(&self, signo: Signo) -> bool {
        Self::action_ignored(&self.actions.lock(), signo)
    }

    /// Checks if syscalls interrupted by the given signal can be restarted.
    pub fn can_restart(&self, signo: Signo) -> bool {
        self.actions.lock()[signo]
            .flags
            .contains(SignalActionFlags::RESTART)
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

        {
            // Generation-time cancellation may detach accounted RT nodes from
            // several queues. Declare this before every guard so early-return
            // drop order also destroys those nodes only after all guards.
            let mut generation_detached = DetachedSignal::empty();
            let generation = Self::has_generation_effect(signo).then(|| self.action_update.lock());
            let mut lifecycle = self.lifecycle.lock();
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                return Ok(inactive());
            }
            if generation.is_some() {
                // Process endpoint state transitions take action_update too,
                // so it is safe to release the spin gate while walking and
                // detaching every shared/private queue.
                drop(lifecycle);
                self.apply_generation_effect_locked(signo, &mut generation_detached);
                lifecycle = self.lifecycle.lock();
                if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                    return Ok(inactive());
                }
            }
            let actions = self.actions.lock();
            if Self::action_ignored(&actions, signo) && !self.blocked_by_any_thread(signo) {
                return Ok(inactive());
            }
            if !signo.is_realtime() && self.pending.lock().set.has(signo) {
                self.possibly_has_signal.store(true, Ordering::Release);
                return Ok(ProcessSignalSendOutcome {
                    published: false,
                    wake_tid: self.wake_thread_for(signo),
                });
            }
        }

        let mut prepared = Some(prepare(sig)?);
        let mut generation_detached = DetachedSignal::empty();
        let generation = Self::has_generation_effect(signo).then(|| self.action_update.lock());
        let mut lifecycle = self.lifecycle.lock();
        if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
            drop(lifecycle);
            drop(generation);
            drop(prepared);
            drop(generation_detached);
            return Ok(inactive());
        }
        if generation.is_some() {
            drop(lifecycle);
            self.apply_generation_effect_locked(signo, &mut generation_detached);
            lifecycle = self.lifecycle.lock();
            if *lifecycle != PROCESS_ENDPOINT_ACTIVE {
                drop(lifecycle);
                drop(generation);
                drop(prepared);
                drop(generation_detached);
                return Ok(inactive());
            }
        }
        let actions = self.actions.lock();
        let ignored = Self::action_ignored(&actions, signo) && !self.blocked_by_any_thread(signo);
        let outcome = if ignored {
            None
        } else {
            let mut pending = self.pending.lock();
            if !signo.is_realtime() && pending.set.has(signo) {
                None
            } else {
                Some(pending.publish(prepared.take().unwrap()))
            }
        };
        drop(actions);
        drop(lifecycle);
        drop(generation);
        // A disposition transition, final exit, or standard-signal race can
        // make the prepared node unnecessary. Release it outside every guard.
        drop(prepared);
        drop(generation_detached);
        let published = outcome.is_some_and(|outcome| outcome.finish());
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
        let detached = self.pending.lock().take_all();
        self.possibly_has_signal.store(false, Ordering::Release);
        drop(detached);
    }

    /// Detaches every process-directed instance of one signal and releases
    /// queue ownership after dropping the pending lock.
    pub fn flush_signal(&self, signo: Signo) {
        let (detached, empty) = {
            let mut pending = self.pending.lock();
            let detached = pending.take_signal(signo);
            (detached, pending.set.is_empty())
        };
        if empty {
            self.possibly_has_signal.store(false, Ordering::Release);
        }
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
