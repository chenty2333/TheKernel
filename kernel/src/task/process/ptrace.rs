//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PtraceReverseLink {
pub(crate)     tracee: Pid,
pub(crate)     session: PtraceSession,
}

impl PtraceReverseLink {
pub(crate) fn new(tracee: Pid, session: PtraceSession) -> Self {
        Self { tracee, session }
    }

pub(crate) fn tracee(self) -> Pid {
        self.tracee
    }

pub(crate) fn session(self) -> PtraceSession {
        self.session
    }
}

pub(crate) struct PtraceReverseLinkNode {
pub(crate)     tracee: Pid,
pub(crate)     session: PtraceSession,
    /// Relationship owner retired while consuming an exit drain. Reusing the
    /// already allocated reverse-link node carries credential destruction to
    /// the caller's post-lifecycle boundary without allocating under locks.
pub(crate)     retired_relationship: Option<PtraceRelationshipSnapshot>,
pub(crate)     next: Option<Box<Self>>,
}

#[derive(Default)]
pub(crate) struct PtraceReverseLinks {
pub(crate)     head: Option<Box<PtraceReverseLinkNode>>,
pub(crate)     len: usize,
pub(crate)     reservations: usize,
pub(crate)     closed: bool,
}

impl PtraceReverseLinks {
pub(crate) fn try_reserve(&mut self) -> AxResult<()> {
        if self.closed {
            return Err(AxError::NoSuchProcess);
        }
        let Some(total) = self.len.checked_add(self.reservations) else {
            return Err(AxError::NoMemory);
        };
        if total >= PTRACE_REVERSE_LINK_HARD_LIMIT {
            return Err(AxError::NoMemory);
        }
        let Some(reservations) = self.reservations.checked_add(1) else {
            return Err(AxError::NoMemory);
        };
        self.reservations = reservations;
        Ok(())
    }

    fn cancel_reservation(&mut self) {
        let old = self.reservations;
        debug_assert!(old != 0);
        if old != 0 {
            self.reservations = old - 1;
        }
    }

    /// Allocation-free partition used when one tracer task exits while its
    /// thread group remains live. Nodes are only relinked under the spin lock;
    /// the returned chain is consumed and destroyed after the guard drops.
pub(crate) fn drain_task(
        &mut self,
        tracer_kernel_tid: Pid,
    ) -> Option<Box<PtraceReverseLinkNode>> {
        let mut source = self.head.take();
        let mut retained = None;
        let mut drained = None;
        let mut retained_len = 0usize;
        while let Some(mut node) = source {
            source = node.next.take();
            if node.session.tracer_kernel_tid == tracer_kernel_tid {
                node.next = drained.take();
                drained = Some(node);
            } else {
                retained_len += 1;
                node.next = retained.take();
                retained = Some(node);
            }
        }
        self.head = retained;
        self.len = retained_len;
        drained
    }
}

/// Preallocated and hard-limit-accounted reverse-link publication token.
///
/// Allocation happens before the reservation spin lock is acquired. Dropping
/// an unpublished token releases its reservation and node outside the lock.
pub(crate) struct PreparedPtraceReverseLink<'a> {
pub(crate)     owner: &'a SpinNoIrq<PtraceReverseLinks>,
pub(crate)     tracer: Pid,
pub(crate)     tracer_kernel_tid: Pid,
pub(crate)     node: Option<Box<PtraceReverseLinkNode>>,
pub(crate)     reserved: bool,
}

impl PreparedPtraceReverseLink<'_> {
pub(crate) fn publish(mut self, session: PtraceSession) -> Result<(), (AxError, Self)> {
        let Some(mut node) = self.node.take() else {
            return Err((AxError::BadState, self));
        };
        node.session = session;
        let mut links = self.owner.lock();
        if !self.reserved || links.reservations == 0 || links.closed {
            drop(links);
            self.node = Some(node);
            return Err((AxError::BadState, self));
        }
        node.next = links.head.take();
        links.head = Some(node);
        links.len += 1;
        links.cancel_reservation();
        self.reserved = false;
        drop(links);
        Ok(())
    }
}

impl Drop for PreparedPtraceReverseLink<'_> {
    fn drop(&mut self) {
        if self.reserved {
            let mut links = self.owner.lock();
            links.cancel_reservation();
            self.reserved = false;
            drop(links);
        }
        // `node` is deliberately dropped only after the spin guard above.
    }
}

pub(crate) struct PtraceReverseLinkDrain {
pub(crate)     next: Option<Box<PtraceReverseLinkNode>>,
pub(crate)     retained: Option<Box<PtraceReverseLinkNode>>,
}

impl PtraceReverseLinkDrain {
    /// Consumes one reverse link and retains both its preallocated node and an
    /// optional detached relationship until this drain itself is dropped.
    /// Exit uses this to move credential free callbacks beyond process
    /// lifecycle and task-parent publication gates without a new allocation.
pub(crate) fn retain_next_retirement(
        &mut self,
        retire: impl FnOnce(PtraceReverseLink) -> Option<PtraceRelationshipSnapshot>,
    ) -> bool {
        let Some(mut node) = self.next.take() else {
            return false;
        };
        self.next = node.next.take();
        let link = PtraceReverseLink {
            tracee: node.tracee,
            session: node.session,
        };
        node.retired_relationship = retire(link);
        node.next = self.retained.take();
        self.retained = Some(node);
        true
    }
}

impl Iterator for PtraceReverseLinkDrain {
    type Item = PtraceReverseLink;

    fn next(&mut self) -> Option<Self::Item> {
        let mut node = self.next.take()?;
        self.next = node.next.take();
        debug_assert!(node.retired_relationship.is_none());
        Some(PtraceReverseLink {
            tracee: node.tracee,
            session: node.session,
        })
    }
}

impl Drop for PtraceReverseLinkDrain {
    fn drop(&mut self) {
        fn drop_chain(mut next: Option<Box<PtraceReverseLinkNode>>) {
            while let Some(mut node) = next {
                next = node.next.take();
                drop(node);
            }
        }

        drop_chain(self.next.take());
        drop_chain(self.retained.take());
    }
}

/// Serializes ptrace relationship actions. The wrapper is zero-cost in
/// production and carries a host-test lock-depth probe used by credential
/// post-commit callbacks.
pub(crate) struct PtraceActionGuard<'a> {
pub(crate)     _guard: axsync::MutexGuard<'a, ()>,
    #[cfg(test)]
    pub(crate) _probe: PostCommitLockProbe,
}

/// Composite outer gate for ptrace relationship publication.
///
/// Exit owns `process_lifecycle` before it removes a core thread membership
/// and later takes `ptrace_actions` during relationship cleanup. Publishing in
/// the same order prevents an attach from racing past the only exit cleanup or
/// deadlocking it with the inverse `ptrace_actions -> process_lifecycle` order.
pub(crate) struct PtracePublicationGuard<'a> {
pub(crate)     owner: &'a ProcessData,
pub(crate)     tracer_owner: Option<&'a ProcessData>,
    // Fields are declared in release order: action users leave before a new
    // lifecycle transition can enter.
pub(crate)     _actions: axsync::MutexGuard<'a, ()>,
pub(crate)     task_parent: TaskParentPublicationGuard<'static>,
pub(crate)     _second_lifecycle: Option<axsync::MutexGuard<'a, ()>>,
pub(crate)     _first_lifecycle: axsync::MutexGuard<'a, ()>,
}

impl PtracePublicationGuard<'_> {
pub(crate) fn task_parent_publication(&self) -> &TaskParentPublicationGuard<'static> {
        &self.task_parent
    }
}

pub(crate) fn ptrace_lifecycle_first(left: &ProcessData, right: &ProcessData) -> bool {
    ptrace_lifecycle_first_key(
        left as *const ProcessData as usize,
        right as *const ProcessData as usize,
    )
}

pub(crate) fn ptrace_lifecycle_first_key(left: usize, right: usize) -> bool {
    left < right
}
