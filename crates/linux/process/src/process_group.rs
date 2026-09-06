use alloc::{sync::Arc, vec::Vec};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{Pid, Process, ProcessError, ProcessRegistry, Session};
use intrusive_collections::RBTreeAtomicLink;

/// A collection of processes inside one session and process registry.
pub struct ProcessGroup<Z> {
    pub(crate) registry_link: RBTreeAtomicLink,
    pub(crate) published: AtomicBool,
    pub(crate) pgid: Pid,
    pub(crate) memberships: AtomicUsize,
    pub(crate) session: Arc<Session<Z>>,
}

impl<Z> ProcessGroup<Z> {
    pub(crate) fn try_new(pgid: Pid, session: &Arc<Session<Z>>) -> Result<Arc<Self>, ProcessError> {
        Arc::try_new(Self {
            registry_link: RBTreeAtomicLink::new(),
            published: AtomicBool::new(false),
            pgid,
            memberships: AtomicUsize::new(0),
            session: session.clone(),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// The process group ID.
    pub fn pgid(&self) -> Pid {
        self.pgid
    }

    /// Returns whether this exact group identity is registered and usable.
    pub fn is_live(&self) -> bool {
        self.published.load(Ordering::Acquire) && self.registry_link.is_linked()
    }

    /// Returns live plus reserved process memberships in this group.
    pub fn membership_count(&self) -> usize {
        self.memberships.load(Ordering::Acquire)
    }

    /// The session containing this process group.
    pub fn session(&self) -> Arc<Session<Z>> {
        self.session.clone()
    }

    /// Fallibly snapshots the published processes in this group.
    pub fn try_processes(
        self: &Arc<Self>,
        registry: &ProcessRegistry<Z>,
    ) -> Result<Vec<Arc<Process<Z>>>, ProcessError> {
        registry.ensure_group_live(self)?;
        registry.try_collect_process_values(|process| {
            Arc::ptr_eq(&process.group(), self).then(|| process.clone())
        })
    }

    /// Visits published group members without running caller code under the registry lock.
    pub fn for_each_process(
        self: &Arc<Self>,
        registry: &ProcessRegistry<Z>,
        mut visitor: impl FnMut(&Arc<Process<Z>>),
    ) -> Result<(), ProcessError> {
        registry.ensure_group_live(self)?;
        for process in registry.processes() {
            if Arc::ptr_eq(&process.group(), self) {
                visitor(&process);
            }
        }
        Ok(())
    }

    /// Tests published group members without allocating.
    pub fn any_process(
        self: &Arc<Self>,
        registry: &ProcessRegistry<Z>,
        mut predicate: impl FnMut(&Arc<Process<Z>>) -> bool,
    ) -> Result<bool, ProcessError> {
        registry.ensure_group_live(self)?;
        Ok(registry
            .processes()
            .any(|process| Arc::ptr_eq(&process.group(), self) && predicate(&process)))
    }
}

impl<Z> fmt::Debug for ProcessGroup<Z> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProcessGroup({}, session={})",
            self.pgid,
            self.session.sid()
        )
    }
}
