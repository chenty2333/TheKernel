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

use axsync::Mutex;
use kspin::SpinNoIrq;

use crate::{
    DefaultSignalAction, DetachedSignal, PendingSignals, PreparedSignal, SignalAction,
    SignalActionFlags, SignalDisposition, SignalInfo, SignalSet, Signo, api::ThreadSignalManager,
};

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

    pub(crate) fn is_live(&self) -> bool {
        self.state.load(Ordering::Acquire) != REGISTRATION_CANCELLED
            && self.thread.strong_count() != 0
    }

    fn upgrade(&self) -> Option<(u32, Arc<ThreadSignalManager>)> {
        if self.state.load(Ordering::Acquire) != REGISTRATION_ACTIVE {
            return None;
        }
        self.thread.upgrade().map(|thread| (self.tid, thread))
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

    /// The signal actions
    pub actions: Arc<SpinNoIrq<SignalActions>>,

    /// The default restorer function.
    pub(crate) default_restorer: usize,

    /// Thread-level signal managers.
    pub(crate) children: SpinNoIrq<Option<Arc<ThreadRegistry>>>,

    /// Serializes registry publication with action transitions. This is a
    /// sleepable mutex when the crate's `multitask` feature is enabled, so
    /// immutable snapshots are allocated without holding a SpinNoIrq guard.
    pub(crate) action_update: Mutex<()>,

    pub(crate) possibly_has_signal: AtomicBool,
}

impl ProcessSignalManager {
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
            actions,
            default_restorer,
            children: SpinNoIrq::new(None),
            action_update: Mutex::new(()),
            possibly_has_signal: AtomicBool::new(false),
        }
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
                if let Some((_, child)) = entry.upgrade() {
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
        if self.signal_ignored(signo) && !self.blocked_by_any_thread(signo) {
            return Ok(ProcessSignalSendOutcome {
                published: false,
                wake_tid: None,
            });
        }

        let already_pending = !signo.is_realtime() && self.pending.lock().set.has(signo);
        let mut published = false;
        if !already_pending {
            let mut prepared = Some(prepare(sig)?);
            let outcome = {
                let actions = self.actions.lock();
                if Self::action_ignored(&actions, signo) && !self.blocked_by_any_thread(signo) {
                    None
                } else {
                    let mut pending = self.pending.lock();
                    Some(pending.publish(prepared.take().unwrap()))
                }
            };
            // A disposition transition can make a prepared node unnecessary.
            // Release it only after the action and pending guards are gone.
            drop(prepared);
            let Some(outcome) = outcome else {
                return Ok(ProcessSignalSendOutcome {
                    published: false,
                    wake_tid: None,
                });
            };
            // A racing standard sender may have filled the fixed slot after
            // preflight. Release its unused charge outside the pending lock.
            published = outcome.finish();
        }
        self.possibly_has_signal.store(true, Ordering::Release);
        Ok(ProcessSignalSendOutcome {
            published,
            wake_tid: self.wake_thread_for(signo),
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
}
