use alloc::{sync::Arc, vec::Vec};
use core::{any::Any, fmt};

use kspin::SpinNoIrq;

use crate::{Pid, ProcessError, ProcessGroup, process::try_collect_process_values};

/// A [`Session`] is a collection of [`ProcessGroup`]s.
pub struct Session {
    sid: Pid,
    terminal: SpinNoIrq<Option<Arc<dyn Any + Send + Sync>>>,
}

impl Session {
    /// Fallibly creates an unpublished [`Session`].
    pub(crate) fn try_new(sid: Pid) -> Result<Arc<Self>, ProcessError> {
        Arc::try_new(Self {
            sid,
            terminal: SpinNoIrq::new(None),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// The [`Session`] ID.
    pub fn sid(&self) -> Pid {
        self.sid
    }

    /// Fallibly snapshots the live process groups in this session.
    pub fn try_process_groups(self: &Arc<Self>) -> Result<Vec<Arc<ProcessGroup>>, ProcessError> {
        let mut groups = try_collect_process_values(|process| {
            let group = process.group();
            Arc::ptr_eq(&group.session(), self).then_some(group)
        })?;
        groups.sort_unstable_by_key(|group| group.pgid());
        groups.dedup_by_key(|group| group.pgid());
        Ok(groups)
    }

    /// Sets the terminal for this session.
    pub fn set_terminal_with(&self, terminal: impl FnOnce() -> Arc<dyn Any + Send + Sync>) -> bool {
        let terminal = terminal();
        let mut guard = self.terminal.lock();
        if guard.is_some() {
            return false;
        }
        *guard = Some(terminal);
        true
    }

    /// Unsets the terminal for this session if it is the given terminal.
    pub fn unset_terminal(&self, term: &Arc<dyn Any + Send + Sync>) -> bool {
        let mut guard = self.terminal.lock();
        if guard.as_ref().is_some_and(|it| Arc::ptr_eq(it, term)) {
            let removed = guard.take();
            drop(guard);
            drop(removed);
            true
        } else {
            false
        }
    }

    /// Gets the terminal for this session, if it exists.
    pub fn terminal(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.terminal.lock().clone()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session({})", self.sid)
    }
}
