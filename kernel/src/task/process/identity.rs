//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

/// This is deliberately separate from [`ProcessData`], which is retired when
/// the process becomes a zombie. Everything here is state Linux itself keeps
/// on the `task_struct`/`struct pid` until `release_task()`, so it stays
/// observable exactly as long as Linux leaves the task findable:
/// * `pid_ns` is the namespace the retained identity renders in;
/// * `real_parent` is `p->real_parent`, which `wait4(2)`'s `__WNOTHREAD` rule
///   compares against the exact calling task (`kernel/exit.c:1657-1664`,
///   `:1725-1735`).
pub(crate) struct ProcessIdentity {
    pid_ns: Arc<PidNamespace>,
    real_parent: Option<Arc<TaskParentNode>>,
}

impl ProcessIdentity {
pub(crate) fn try_new(
        pid_ns: Arc<PidNamespace>,
        real_parent: Option<Arc<TaskParentNode>>,
    ) -> AxResult<Self> {
        Ok(Self {
            pid_ns,
            real_parent,
        })
    }

pub(crate) fn pid_ns(&self) -> &Arc<PidNamespace> {
        &self.pid_ns
    }

    /// The exact task Linux would name in `p->real_parent` for this process.
    ///
    /// The returned identity may belong to a task that has already exited:
    /// callers must resolve it with
    /// [`resolve_retained_task_parent`](crate::task::resolve_retained_task_parent)
    /// before comparing it with a live task.
pub(crate) fn real_parent(&self) -> Option<&Arc<TaskParentNode>> {
        self.real_parent.as_ref()
    }
}

/// Reads the retained identity of one registry entry, live or zombie.
pub(crate) fn process_identity(process: &Process) -> Option<&ProcessIdentity> {
    process.identity::<ProcessIdentity>()
}

/// The PID namespace this process's retained identity renders in.
pub(crate) fn process_identity_pid_ns(process: &Process) -> Option<&Arc<PidNamespace>> {
    process_identity(process).map(ProcessIdentity::pid_ns)
}

/// Linux's `__WNOTHREAD` ownership test: `current == p->real_parent`
/// (`kernel/exit.c:1657-1664`).
///
/// Both sides are exact task identities, so a sibling thread of the forking
/// thread never matches and numeric TID reuse cannot invent a relation. A
/// parent task that has already exited resolves through its retained reparent
/// hop to the live replacement Linux would have installed.
pub(crate) fn is_exact_child_of_thread(process: &Process, caller: &Arc<TaskParentNode>) -> bool {
    process_identity(process)
        .and_then(ProcessIdentity::real_parent)
        .and_then(resolve_retained_task_parent)
        .is_some_and(|real_parent| Arc::ptr_eq(&real_parent, caller))
}

/// Linux process identity bound to immutable exit credential and signal-owner
/// provenance retained in the durable zombie payload.
pub(crate) type Process = tk_linux_process_adapter::Process<Arc<Cred>, GroupLeaderSignalOwner>;
