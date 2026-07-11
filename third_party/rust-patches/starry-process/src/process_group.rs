use alloc::{sync::Arc, vec::Vec};
use core::fmt;

use crate::{
    Pid, Process, ProcessError, Session,
    process::{processes, try_collect_process_values},
};

/// A [`ProcessGroup`] is a collection of [`Process`]es.
pub struct ProcessGroup {
    pgid: Pid,
    pub(crate) session: Arc<Session>,
}

impl ProcessGroup {
    /// Fallibly creates an unpublished [`ProcessGroup`] within a [`Session`].
    pub(crate) fn try_new(pgid: Pid, session: &Arc<Session>) -> Result<Arc<Self>, ProcessError> {
        Arc::try_new(Self {
            pgid,
            session: session.clone(),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// The [`ProcessGroup`] ID.
    pub fn pgid(&self) -> Pid {
        self.pgid
    }

    /// The [`Session`] that the [`ProcessGroup`] belongs to.
    pub fn session(&self) -> Arc<Session> {
        self.session.clone()
    }

    /// Fallibly snapshots the live [`Process`]es in this group.
    ///
    /// Snapshot storage is admitted outside the process registry lock. If the
    /// group grows concurrently, capacity expands geometrically up to the
    /// explicit global membership ceiling.
    pub fn try_processes(self: &Arc<Self>) -> Result<Vec<Arc<Process>>, ProcessError> {
        try_collect_process_values(|process| {
            Arc::ptr_eq(&process.group(), self).then(|| process.clone())
        })
    }

    /// Visits each process through a stable, allocation-free PID cursor.
    ///
    /// The global membership lock is held only long enough to clone one
    /// already-published process reference. `visitor` runs after the lock is
    /// released, so it may safely acquire signal and job-control locks.
    pub fn for_each_process(self: &Arc<Self>, mut visitor: impl FnMut(&Arc<Process>)) {
        for process in processes() {
            if Arc::ptr_eq(&process.group(), self) {
                visitor(&process);
            }
        }
    }

    /// Returns whether any process satisfies `predicate` without allocating.
    pub fn any_process(self: &Arc<Self>, mut predicate: impl FnMut(&Arc<Process>) -> bool) -> bool {
        processes().any(|process| Arc::ptr_eq(&process.group(), self) && predicate(&process))
    }
}

impl fmt::Debug for ProcessGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProcessGroup({}, session={})",
            self.pgid,
            self.session.sid()
        )
    }
}
