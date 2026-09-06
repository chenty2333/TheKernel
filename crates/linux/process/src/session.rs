use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use intrusive_collections::RBTreeAtomicLink;
use kspin::SpinNoIrq;

use crate::{Pid, ProcessError, ProcessGroup, ProcessRegistry};

/// A collection of process groups inside one explicit process registry.
pub struct Session<Z> {
    pub(crate) registry_link: RBTreeAtomicLink,
    pub(crate) published: AtomicBool,
    pub(crate) sid: Pid,
    registry: alloc::sync::Weak<ProcessRegistry<Z>>,
    pub(crate) groups: AtomicUsize,
    terminal: SpinNoIrq<Option<Arc<dyn Any + Send + Sync>>>,
}

impl<Z> Session<Z> {
    pub(crate) fn try_new(
        sid: Pid,
        registry: &Arc<ProcessRegistry<Z>>,
    ) -> Result<Arc<Self>, ProcessError> {
        Arc::try_new(Self {
            registry_link: RBTreeAtomicLink::new(),
            published: AtomicBool::new(false),
            sid,
            registry: Arc::downgrade(registry),
            groups: AtomicUsize::new(0),
            terminal: SpinNoIrq::new(None),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    pub(crate) fn belongs_to(&self, registry: &ProcessRegistry<Z>) -> bool {
        core::ptr::eq(self.registry.as_ptr(), registry)
    }

    /// The session ID.
    pub fn sid(&self) -> Pid {
        self.sid
    }

    /// Returns whether this exact session identity is registered and usable.
    pub fn is_live(&self) -> bool {
        self.published.load(Ordering::Acquire) && self.registry_link.is_linked()
    }

    /// Returns the number of live process-group identities in this session.
    pub fn group_count(&self) -> usize {
        self.groups.load(Ordering::Acquire)
    }

    /// Fallibly snapshots the live process groups in this registry and session.
    pub fn try_process_groups(
        self: &Arc<Self>,
        registry: &ProcessRegistry<Z>,
    ) -> Result<Vec<Arc<ProcessGroup<Z>>>, ProcessError> {
        if !self.belongs_to(registry) {
            return Err(ProcessError::WrongDomain);
        }
        registry.try_session_groups(self)
    }

    /// Installs a terminal if this session does not already own one.
    pub fn set_terminal_with(&self, terminal: impl FnOnce() -> Arc<dyn Any + Send + Sync>) -> bool {
        let terminal = terminal();
        let mut guard = self.terminal.lock();
        if guard.is_some() {
            return false;
        }
        *guard = Some(terminal);
        true
    }

    /// Removes the terminal only when it is the same allocation as `terminal`.
    pub fn unset_terminal(&self, terminal: &Arc<dyn Any + Send + Sync>) -> bool {
        let mut guard = self.terminal.lock();
        if guard
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, terminal))
        {
            let removed = guard.take();
            drop(guard);
            drop(removed);
            true
        } else {
            false
        }
    }

    /// Returns the session terminal, if one is installed.
    pub fn terminal(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.terminal.lock().clone()
    }
}

impl<Z> fmt::Debug for Session<Z> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session({})", self.sid)
    }
}
