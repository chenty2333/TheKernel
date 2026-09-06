use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use intrusive_collections::{Bound, KeyAdapter, RBTree, RBTreeAtomicLink, intrusive_adapter};
use kspin::SpinNoIrq;

use crate::{Pid, ProcessGroup, Session};

/// Default maximum for process identities and domain-wide thread memberships.
///
/// A [`ProcessDomain`] may choose a lower limit. The same bound applies to its
/// process identities, group/session identities, total threads, and each
/// individual thread group.
pub const PROCESS_MEMBERSHIP_LIMIT: usize = 65_536;

/// Maximum child parent-pointer mutations performed while one topology guard
/// keeps interrupts disabled during process exit.
const REPARENT_BATCH_LIMIT: usize = 64;

#[cfg(test)]
static MAX_REPARENT_BATCH_OBSERVED: AtomicUsize = AtomicUsize::new(0);

fn try_atomic_update(
    counter: &AtomicUsize,
    set_order: Ordering,
    load_order: Ordering,
    mut update: impl FnMut(usize) -> Option<usize>,
) -> Result<usize, usize> {
    let mut current = counter.load(load_order);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match counter.compare_exchange_weak(current, next, set_order, load_order) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

fn reserve_bounded_counter(counter: &AtomicUsize, limit: usize) -> Result<(), ProcessError> {
    try_atomic_update(counter, Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(1).filter(|next| *next <= limit)
    })
    .map(|_| ())
    .map_err(|_| ProcessError::Capacity)
}

/// Releases one internal resource charge without ever wrapping through
/// `usize::MAX`. `None` means the counter was already zero; callers then retain
/// surrounding registry state instead of turning corruption into new capacity.
fn release_counter(counter: &AtomicUsize) -> Option<usize> {
    try_atomic_update(counter, Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_sub(1)
    })
    .ok()
}

fn decrement_count(counter: &mut usize) -> bool {
    let Some(next) = counter.checked_sub(1) else {
        return false;
    };
    *counter = next;
    true
}

/// Failure returned by fallible process-lifecycle operations.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProcessError {
    /// Allocator capacity could not be reserved.
    NoMemory,
    /// The requested PID or TID is already live or reserved.
    AlreadyExists,
    /// The configured membership ceiling has been reached.
    Capacity,
    /// An object belongs to a different process registry.
    WrongDomain,
    /// The process is unpublished, reaped, or otherwise not in the registry.
    NotPublished,
    /// The process has already entered zombie state.
    NotLive,
    /// A reversible lifecycle dependency must complete before retrying.
    Busy,
    /// The domain does not yet have an init process.
    NotInitialized,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => f.write_str("process allocation failed"),
            Self::AlreadyExists => f.write_str("process identifier already exists"),
            Self::Capacity => f.write_str("process membership limit reached"),
            Self::WrongDomain => f.write_str("object belongs to another process domain"),
            Self::NotPublished => f.write_str("process is not published"),
            Self::NotLive => f.write_str("process is no longer live"),
            Self::Busy => f.write_str("process lifecycle transition is busy"),
            Self::NotInitialized => f.write_str("process domain has no init process"),
        }
    }
}

/// Result of asking a process domain to transition a process to zombie state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExitOutcome {
    /// Init processes remain live and were not changed.
    InitProcess,
    /// Another caller had already completed the zombie transition.
    AlreadyZombie,
    /// This call performed the zombie transition and child reparenting.
    BecameZombie,
}

/// Result of removing one live thread from a process.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ThreadExitOutcome {
    /// The requested TID was not a live member and no state changed.
    NotFound,
    /// The thread exited while at least one live thread remained.
    LiveThreadsRemain,
    /// The thread exited and no live threads remained.
    FinalThread,
}

/// State observed when an already validated thread reservation is published.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[must_use = "a thread published after group exit must be terminated before it can run"]
pub enum ThreadPublicationOutcome {
    /// No group-wide exit had linearized at publication.
    Live,
    /// Group exit had already linearized; the adapter must terminate this TID.
    GroupExited,
}

/// Domain-coordinated result of removing one live thread.
///
/// The final-thread case carries the exclusive zombie-publication token that
/// was prepared in the same thread-group critical section as membership
/// removal. Dropping that token restores the final live membership, so a
/// caller cannot strand a zero-thread, non-zombie process.
pub enum ThreadExitTransition<Z> {
    /// The requested TID was not a live member and no state changed.
    NotFound,
    /// The thread exited while at least one live thread remained.
    LiveThreadsRemain,
    /// The final live thread was removed and zombie publication is reserved.
    FinalThread(ProcessExitAdmission<Z>),
}

/// A newly created session together with its initial process group.
pub type CreatedSession<Z> = (Arc<Session<Z>>, Arc<ProcessGroup<Z>>);

struct ThreadNode {
    link: RBTreeAtomicLink,
    tid: Pid,
    live: AtomicBool,
}

intrusive_adapter!(ThreadAdapter = Arc<ThreadNode>: ThreadNode { link: RBTreeAtomicLink });

impl<'a> KeyAdapter<'a> for ThreadAdapter {
    type Key = Pid;

    fn get_key(&self, thread: &'a ThreadNode) -> Self::Key {
        thread.tid
    }
}

struct ThreadGroup {
    threads: RBTree<ThreadAdapter>,
    memberships: usize,
    live_threads: usize,
    exit_code: i32,
    group_exited: bool,
    exit_prepared: bool,
}

impl ThreadGroup {
    fn new() -> Self {
        Self {
            threads: RBTree::new(ThreadAdapter::new()),
            memberships: 0,
            live_threads: 0,
            exit_code: 0,
            group_exited: false,
            exit_prepared: false,
        }
    }
}

/// A process whose durable zombie state is supplied by the caller as `Z`.
pub struct Process<Z> {
    registry_link: RBTreeAtomicLink,
    registry: Weak<ProcessRegistry<Z>>,
    published: AtomicBool,
    pid: Pid,
    /// Immutable kernel-owned identity retained for this process's entire
    /// lifetime, including after its live runtime state has gone away.
    identity: Arc<dyn Any + Send + Sync>,
    /// The namespace-local reparenting domain this process belongs to.
    reaper_scope: Arc<ReaperScope<Z>>,
    init: bool,
    is_zombie: AtomicBool,
    reaped: AtomicBool,
    tg: SpinNoIrq<ThreadGroup>,
    exit_signal: Option<u8>,
    zombie_payload: SpinNoIrq<Option<Arc<Z>>>,
    child_subreaper: AtomicBool,
    parent: SpinNoIrq<Weak<Process<Z>>>,
    group: SpinNoIrq<Arc<ProcessGroup<Z>>>,
}

intrusive_adapter!(ProcessAdapter<Z> = Arc<Process<Z>>: Process<Z> {
    registry_link: RBTreeAtomicLink
});

impl<'a, Z> KeyAdapter<'a> for ProcessAdapter<Z> {
    type Key = Pid;

    fn get_key(&self, process: &'a Process<Z>) -> Self::Key {
        process.pid
    }
}

intrusive_adapter!(ProcessGroupAdapter<Z> = Arc<ProcessGroup<Z>>: ProcessGroup<Z> {
    registry_link: RBTreeAtomicLink
});

impl<'a, Z> KeyAdapter<'a> for ProcessGroupAdapter<Z> {
    type Key = Pid;

    fn get_key(&self, group: &'a ProcessGroup<Z>) -> Self::Key {
        group.pgid
    }
}

intrusive_adapter!(SessionAdapter<Z> = Arc<Session<Z>>: Session<Z> {
    registry_link: RBTreeAtomicLink
});

impl<'a, Z> KeyAdapter<'a> for SessionAdapter<Z> {
    type Key = Pid;

    fn get_key(&self, session: &'a Session<Z>) -> Self::Key {
        session.sid
    }
}

struct RegistryState<Z> {
    entries: RBTree<ProcessAdapter<Z>>,
    memberships: usize,
    groups: RBTree<ProcessGroupAdapter<Z>>,
    group_count: usize,
    sessions: RBTree<SessionAdapter<Z>>,
    session_count: usize,
}

impl<Z> RegistryState<Z> {
    fn new() -> Self {
        Self {
            entries: RBTree::new(ProcessAdapter::new()),
            memberships: 0,
            groups: RBTree::new(ProcessGroupAdapter::new()),
            group_count: 0,
            sessions: RBTree::new(SessionAdapter::new()),
            session_count: 0,
        }
    }
}

/// A bounded PID registry owned by exactly one [`ProcessDomain`].
///
/// The registry has no singleton instance. Read-only enumeration is explicit,
/// and mutation is mediated by the owning domain and admission tokens.
pub struct ProcessRegistry<Z> {
    // When both guards are needed, topology is always acquired before state.
    // Admission paths that touch state first release it before taking topology.
    // Every removal of a published process must hold topology through the
    // state-tree unlink. Consequently topology alone pins the registry-owned
    // Arc behind every published ancestry Weak; unpublished admission rollback
    // may unlink under state alone because it can never appear in ancestry.
    state: SpinNoIrq<RegistryState<Z>>,
    topology: SpinNoIrq<()>,
    membership_limit: usize,
    thread_memberships: AtomicUsize,
}

impl<Z> ProcessRegistry<Z> {
    fn new(membership_limit: usize) -> Self {
        Self {
            state: SpinNoIrq::new(RegistryState::new()),
            topology: SpinNoIrq::new(()),
            membership_limit: membership_limit.min(PROCESS_MEMBERSHIP_LIMIT),
            thread_memberships: AtomicUsize::new(0),
        }
    }

    /// Returns the configured identity and domain-wide thread membership limit.
    pub fn membership_limit(&self) -> usize {
        self.membership_limit
    }

    /// Returns the number of live and reserved process membership records.
    pub fn membership_count(&self) -> usize {
        self.state.lock().memberships
    }

    /// Returns the domain-wide number of live and reserved thread memberships.
    pub fn thread_membership_count(&self) -> usize {
        self.thread_memberships.load(Ordering::Acquire)
    }

    /// Returns the number of registered process-group identities.
    pub fn process_group_count(&self) -> usize {
        self.state.lock().group_count
    }

    /// Returns the number of registered session identities.
    pub fn session_count(&self) -> usize {
        self.state.lock().session_count
    }

    /// Looks up one published process by PID.
    pub fn get(&self, pid: Pid) -> Option<Arc<Process<Z>>> {
        let state = self.state.lock();
        let process = state.entries.find(&pid).clone_pointer();
        drop(state);
        process.filter(|process| process.published.load(Ordering::Acquire))
    }

    /// Looks up one live process-group identity by PGID.
    pub fn get_process_group(&self, pgid: Pid) -> Option<Arc<ProcessGroup<Z>>> {
        let state = self.state.lock();
        let group = state.groups.find(&pgid).clone_pointer();
        drop(state);
        group.filter(|group| group.published.load(Ordering::Acquire))
    }

    /// Looks up one live session identity by SID.
    pub fn get_session(&self, sid: Pid) -> Option<Arc<Session<Z>>> {
        let state = self.state.lock();
        let session = state.sessions.find(&sid).clone_pointer();
        drop(state);
        session.filter(|session| session.published.load(Ordering::Acquire))
    }

    /// Iterates published processes in PID order without allocating.
    pub fn processes(&self) -> Processes<'_, Z> {
        Processes {
            registry: self,
            last: None,
            after: None,
            remaining: self.membership_count(),
            finished: false,
        }
    }

    /// Iterates monotonically through the PID range that existed when this
    /// cursor was created. Concurrent insertion above the captured maximum is
    /// ignored, while insertion below/after the cursor cannot consume a fixed
    /// item budget and make an original higher-PID entry disappear.
    fn processes_through_current_max(&self) -> ProcessesThroughCurrentMax<'_, Z> {
        let upper_bound = self
            .state
            .lock()
            .entries
            .back()
            .get()
            .map(|process| process.pid);
        ProcessesThroughCurrentMax {
            registry: self,
            after: None,
            upper_bound,
            finished: upper_bound.is_none(),
        }
    }

    /// Fallibly snapshots every published process in PID order.
    pub fn try_processes(&self) -> Result<Vec<Arc<Process<Z>>>, ProcessError> {
        self.try_collect_process_values(|process| Some(process.clone()))
    }

    fn belongs(&self, process: &Process<Z>) -> bool {
        core::ptr::eq(process.registry.as_ptr(), self)
    }

    fn current_process_locked(state: &RegistryState<Z>, process: &Process<Z>) -> bool {
        state
            .entries
            .find(&process.pid)
            .get()
            .is_some_and(|current| core::ptr::eq(current, process))
    }

    fn contains_process(&self, process: &Process<Z>) -> bool {
        let state = self.state.lock();
        Self::current_process_locked(&state, process)
    }

    fn ensure_published(&self, process: &Process<Z>) -> Result<(), ProcessError> {
        if !self.belongs(process) {
            return Err(ProcessError::WrongDomain);
        }
        let state = self.state.lock();
        if !process.published.load(Ordering::Acquire)
            || !Self::current_process_locked(&state, process)
        {
            return Err(ProcessError::NotPublished);
        }
        Ok(())
    }

    fn current_group_locked(state: &RegistryState<Z>, group: &ProcessGroup<Z>) -> bool {
        state
            .groups
            .find(&group.pgid)
            .get()
            .is_some_and(|current| core::ptr::eq(current, group))
    }

    fn current_session_locked(state: &RegistryState<Z>, session: &Session<Z>) -> bool {
        state
            .sessions
            .find(&session.sid)
            .get()
            .is_some_and(|current| core::ptr::eq(current, session))
    }

    pub(crate) fn ensure_group_live(&self, group: &ProcessGroup<Z>) -> Result<(), ProcessError> {
        if !group.session.belongs_to(self) {
            return Err(ProcessError::WrongDomain);
        }
        let state = self.state.lock();
        let current =
            Self::current_group_locked(&state, group) && group.published.load(Ordering::Acquire);
        drop(state);
        current.then_some(()).ok_or(ProcessError::NotPublished)
    }

    fn admit_session_group(
        self: &Arc<Self>,
        session: Arc<Session<Z>>,
        group: Arc<ProcessGroup<Z>>,
        new_session: bool,
    ) -> Result<JobControlAdmission<Z>, ProcessError> {
        if !session.belongs_to(self)
            || !group.session.belongs_to(self)
            || !Arc::ptr_eq(&group.session, &session)
        {
            return Err(ProcessError::WrongDomain);
        }

        let mut state = self.state.lock();
        if state.group_count >= self.membership_limit
            || (new_session && state.session_count >= self.membership_limit)
        {
            return Err(ProcessError::Capacity);
        }
        if !state.groups.find(&group.pgid).is_null() {
            return Err(ProcessError::AlreadyExists);
        }
        let next_group_count = state
            .group_count
            .checked_add(1)
            .filter(|next| *next <= self.membership_limit)
            .ok_or(ProcessError::Capacity)?;
        let next_session_count = new_session
            .then(|| {
                state
                    .session_count
                    .checked_add(1)
                    .filter(|next| *next <= self.membership_limit)
                    .ok_or(ProcessError::Capacity)
            })
            .transpose()?;
        if new_session {
            if !state.sessions.find(&session.sid).is_null() {
                return Err(ProcessError::AlreadyExists);
            }
        } else if !Self::current_session_locked(&state, &session)
            || !session.published.load(Ordering::Acquire)
        {
            return Err(ProcessError::NotPublished);
        }

        reserve_bounded_counter(&session.groups, self.membership_limit)?;
        if let Some(next_session_count) = next_session_count {
            state.sessions.insert(session.clone());
            state.session_count = next_session_count;
        }

        state.groups.insert(group.clone());
        state.group_count = next_group_count;
        drop(state);
        Ok(JobControlAdmission {
            registry: self.clone(),
            session,
            group,
            new_session,
            committed: false,
        })
    }

    fn reserve_group_member(
        &self,
        group: &Arc<ProcessGroup<Z>>,
        allow_unpublished: bool,
    ) -> Result<(), ProcessError> {
        if !group.session.belongs_to(self) {
            return Err(ProcessError::WrongDomain);
        }
        let state = self.state.lock();
        if !Self::current_group_locked(&state, group)
            || (!allow_unpublished && !group.published.load(Ordering::Acquire))
        {
            return Err(ProcessError::NotPublished);
        }
        reserve_bounded_counter(&group.memberships, self.membership_limit)?;
        drop(state);
        Ok(())
    }

    fn remove_empty_group(&self, group: &Arc<ProcessGroup<Z>>) {
        let (removed_group, removed_session) = {
            let mut state = self.state.lock();
            if group.memberships.load(Ordering::Acquire) != 0
                || !Self::current_group_locked(&state, group)
            {
                return;
            }
            group.published.store(false, Ordering::Release);
            // SAFETY: pointer identity was checked under the registry lock.
            let removed_group = unsafe {
                state
                    .groups
                    .cursor_mut_from_ptr(Arc::as_ptr(group))
                    .remove()
            };
            if removed_group.is_some() {
                decrement_count(&mut state.group_count);
            }

            let session = &group.session;
            let removed_session = if removed_group.is_some()
                && release_counter(&session.groups) == Some(1)
                && Self::current_session_locked(&state, session)
            {
                session.published.store(false, Ordering::Release);
                // SAFETY: pointer identity was checked under the registry lock.
                let removed = unsafe {
                    state
                        .sessions
                        .cursor_mut_from_ptr(Arc::as_ptr(session))
                        .remove()
                };
                if removed.is_some() {
                    decrement_count(&mut state.session_count);
                }
                removed
            } else {
                None
            };
            (removed_group, removed_session)
        };
        drop(removed_group);
        drop(removed_session);
    }

    fn release_group_member(&self, group: &Arc<ProcessGroup<Z>>) {
        if release_counter(&group.memberships) == Some(1) {
            self.remove_empty_group(group);
        }
    }

    fn reserve_thread_member(&self) -> Result<(), ProcessError> {
        reserve_bounded_counter(&self.thread_memberships, self.membership_limit)
    }

    fn release_thread_member(&self) {
        release_counter(&self.thread_memberships);
    }

    pub(crate) fn try_session_groups(
        &self,
        session: &Arc<Session<Z>>,
    ) -> Result<Vec<Arc<ProcessGroup<Z>>>, ProcessError> {
        if !session.belongs_to(self) {
            return Err(ProcessError::WrongDomain);
        }
        {
            let state = self.state.lock();
            if !Self::current_session_locked(&state, session)
                || !session.published.load(Ordering::Acquire)
            {
                return Err(ProcessError::NotPublished);
            }
        }
        let mut groups = self.try_collect_process_values(|process| {
            let group = process.group();
            (group.is_live() && Arc::ptr_eq(&group.session(), session)).then_some(group)
        })?;
        groups.sort_unstable_by_key(|group| group.pgid());
        groups.dedup_by(|left, right| Arc::ptr_eq(left, right));
        Ok(groups)
    }

    fn admit(
        self: &Arc<Self>,
        process: Arc<Process<Z>>,
    ) -> Result<ProcessAdmission<Z>, ProcessError> {
        if !self.belongs(&process) {
            return Err(ProcessError::WrongDomain);
        }
        let group = process.group();
        let mut state = self.state.lock();
        if state.memberships >= self.membership_limit {
            return Err(ProcessError::Capacity);
        }
        let next_memberships = state
            .memberships
            .checked_add(1)
            .filter(|next| *next <= self.membership_limit)
            .ok_or(ProcessError::Capacity)?;
        if !state.entries.find(&process.pid).is_null() {
            return Err(ProcessError::AlreadyExists);
        }
        if !Self::current_group_locked(&state, &group) {
            return Err(ProcessError::NotPublished);
        }
        reserve_bounded_counter(&group.memberships, self.membership_limit)?;
        state.entries.insert(process.clone());
        state.memberships = next_memberships;
        drop(state);
        Ok(ProcessAdmission {
            registry: self.clone(),
            process,
            committed: false,
        })
    }

    pub(crate) fn try_collect_process_values<T>(
        &self,
        mut map: impl FnMut(&Arc<Process<Z>>) -> Option<T>,
    ) -> Result<Vec<T>, ProcessError> {
        let mut snapshot = Vec::new();
        loop {
            snapshot.clear();
            let required = self.membership_count();
            if snapshot.capacity() < required {
                snapshot
                    .try_reserve_exact(required)
                    .map_err(|_| ProcessError::NoMemory)?;
            }

            let mut overflow = false;
            for process in self.processes() {
                let Some(value) = map(&process) else {
                    continue;
                };
                if snapshot.len() == snapshot.capacity() {
                    drop(value);
                    overflow = true;
                    break;
                }
                snapshot.push(value);
            }
            if !overflow {
                return Ok(snapshot);
            }

            let current = snapshot.capacity();
            if current >= self.membership_limit {
                return Err(ProcessError::Capacity);
            }
            let target = current.max(1).saturating_mul(2).min(self.membership_limit);
            snapshot.clear();
            snapshot
                .try_reserve_exact(target)
                .map_err(|_| ProcessError::NoMemory)?;
        }
    }
}

/// Explicit owner of one process registry and its unique init identity.
pub struct ProcessDomain<Z> {
    registry: Arc<ProcessRegistry<Z>>,
    root_reaper_scope: Arc<ReaperScope<Z>>,
}

/// One independent reparenting domain, normally corresponding to one PID namespace.
///
/// A scope has exactly one init process.  Reparenting never selects a child
/// subreaper from another scope and falls back to this init when no eligible
/// ancestor exists in the scope.
pub struct ReaperScope<Z> {
    registry: Weak<ProcessRegistry<Z>>,
    init: SpinNoIrq<Option<Arc<Process<Z>>>>,
    outer_scope: SpinNoIrq<Option<Weak<ReaperScope<Z>>>>,
}

impl<Z> ReaperScope<Z> {
    /// Returns this scope's namespace-init process after it has been published.
    pub fn init_process(&self) -> Option<Arc<Process<Z>>> {
        self.init.lock().clone()
    }
}

fn reparent_if_owned<Z>(
    child: &Process<Z>,
    expected_parent: &Arc<Process<Z>>,
    replacement: &Weak<Process<Z>>,
) -> Option<Weak<Process<Z>>> {
    let old_parent = {
        let mut parent = child.parent.lock();
        if parent.as_ptr() != Arc::as_ptr(expected_parent) {
            return None;
        }
        core::mem::replace(&mut *parent, replacement.clone())
    };
    Some(old_parent)
}

struct ReaperSelection<Z> {
    reaper: Arc<Process<Z>>,
    retired_first_ancestor: Option<Arc<Process<Z>>>,
}

fn select_reaper_for_exit_locked<Z>(
    exiting: &Arc<Process<Z>>,
    child: &Arc<Process<Z>>,
    scope: &Arc<ReaperScope<Z>>,
) -> ReaperSelection<Z> {
    let init = scope
        .init_process()
        .expect("every published process has a bound reaper-scope init");
    // A scope's init is parented in its outer scope. It cannot reparent to
    // itself when that outer parent exits; use the outer scope fallback.
    let fallback = if Arc::ptr_eq(&init, child) {
        scope
            .outer_scope
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|outer| outer.init_process())
            .expect("a non-root scope init has an outer-scope init")
    } else {
        init
    };
    let Some(first_ancestor) = exiting.parent() else {
        return ReaperSelection {
            reaper: fallback.clone(),
            retired_first_ancestor: None,
        };
    };

    // The caller owns the topology guard. Published-process removal follows
    // topology -> state, so the registry's owning Arc keeps every ancestry
    // pointer alive without taking state here. This avoids inverting state ->
    // thread-group against thread admission's thread-group -> state check.
    // Final exit reparents children before reap, therefore a live ancestry
    // relation always names a process still present in this registry.
    let first_ptr = Arc::as_ptr(&first_ancestor);
    let mut candidate_ptr = first_ptr;
    loop {
        // SAFETY: `first_ancestor` retains `first_ptr`. Every later pointer was
        // read from the parent relation of a registered live process while the
        // topology guard prevents exit mutation and published registry removal,
        // keeping the registry's owning Arc alive.
        let candidate = unsafe { &*candidate_ptr };
        if Arc::ptr_eq(&candidate.reaper_scope, scope) && candidate.is_child_subreaper() {
            let tg = candidate.tg.lock();
            if tg.live_threads != 0
                && !tg.exit_prepared
                && !candidate.is_zombie.load(Ordering::Acquire)
            {
                drop(tg);
                let reaper = if candidate_ptr == first_ptr {
                    return ReaperSelection {
                        reaper: first_ancestor,
                        retired_first_ancestor: None,
                    };
                } else {
                    // SAFETY: the caller's topology guard prevents the
                    // candidate's published registry Arc from being removed
                    // until the newly incremented reference is materialized.
                    unsafe {
                        Arc::increment_strong_count(candidate_ptr);
                        Arc::from_raw(candidate_ptr)
                    }
                };
                return ReaperSelection {
                    reaper,
                    retired_first_ancestor: Some(first_ancestor),
                };
            }
        }

        if !Arc::ptr_eq(&candidate.reaper_scope, scope) {
            break;
        }
        let parent = candidate.parent.lock();
        let next_ptr = (parent.strong_count() != 0).then(|| parent.as_ptr());
        drop(parent);
        let Some(next_ptr) = next_ptr else {
            break;
        };
        candidate_ptr = next_ptr;
    }
    ReaperSelection {
        reaper: fallback,
        retired_first_ancestor: Some(first_ancestor),
    }
}

impl<Z> ProcessDomain<Z> {
    /// Fallibly creates a domain with the default bounded membership limit.
    pub fn try_new() -> Result<Self, ProcessError> {
        Self::try_with_membership_limit(PROCESS_MEMBERSHIP_LIMIT)
    }

    /// Fallibly creates a domain with a lower process/thread membership limit.
    pub fn try_with_membership_limit(limit: usize) -> Result<Self, ProcessError> {
        let registry =
            Arc::try_new(ProcessRegistry::new(limit)).map_err(|_| ProcessError::NoMemory)?;
        Ok(Self {
            registry: registry.clone(),
            root_reaper_scope: Arc::try_new(ReaperScope {
                registry: Arc::downgrade(&registry),
                init: SpinNoIrq::new(None),
                outer_scope: SpinNoIrq::new(None),
            })
            .map_err(|_| ProcessError::NoMemory)?,
        })
    }

    /// Returns this domain's explicit registry.
    pub fn registry(&self) -> &ProcessRegistry<Z> {
        &self.registry
    }

    /// Returns the domain's init process after initialization.
    pub fn init_process(&self) -> Option<Arc<Process<Z>>> {
        self.root_reaper_scope.init_process()
    }

    /// Returns the scope used by processes created through the legacy domain
    /// APIs. It is the domain's root PID namespace reparenting scope.
    pub fn root_reaper_scope(&self) -> Arc<ReaperScope<Z>> {
        self.root_reaper_scope.clone()
    }

    /// Creates an empty reparenting scope owned by this domain.
    pub fn try_new_reaper_scope(&self) -> Result<Arc<ReaperScope<Z>>, ProcessError> {
        Arc::try_new(ReaperScope {
            registry: Arc::downgrade(&self.registry),
            init: SpinNoIrq::new(None),
            outer_scope: SpinNoIrq::new(None),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// Fallibly creates, publishes, and records the domain's unique init process.
    pub fn try_new_init(
        &self,
        pid: Pid,
        exit_signal: Option<u8>,
    ) -> Result<Arc<Process<Z>>, ProcessError> {
        self.try_new_init_with_identity(pid, exit_signal, ())
    }

    /// Creates the domain init with immutable caller-owned identity metadata.
    pub fn try_new_init_with_identity<I>(
        &self,
        pid: Pid,
        exit_signal: Option<u8>,
        identity: I,
    ) -> Result<Arc<Process<Z>>, ProcessError>
    where
        I: Any + Send + Sync,
    {
        self.try_new_init_in_reaper_scope_with_identity(
            &self.root_reaper_scope,
            pid,
            exit_signal,
            identity,
        )
    }

    /// Creates and publishes a root process for `scope`.
    ///
    /// Unlike [`Self::prepare_fork_as_reaper_scope_init_with_identity`], this has no
    /// parent and is therefore suitable only for the domain's root scope.
    pub fn try_new_init_in_reaper_scope_with_identity<I>(
        &self,
        scope: &Arc<ReaperScope<Z>>,
        pid: Pid,
        exit_signal: Option<u8>,
        identity: I,
    ) -> Result<Arc<Process<Z>>, ProcessError>
    where
        I: Any + Send + Sync,
    {
        self.ensure_scope_owned(scope)?;
        let session = Session::try_new(pid, &self.registry)?;
        let group = ProcessGroup::try_new(pid, &session)?;
        let job_control =
            self.registry
                .admit_session_group(session.clone(), group.clone(), true)?;
        let identity: Arc<dyn Any + Send + Sync> =
            Arc::try_new(identity).map_err(|_| ProcessError::NoMemory)?;
        let process = Process::try_allocate(ProcessAllocation {
            registry: &self.registry,
            pid,
            init: true,
            parent: None,
            group,
            exit_signal,
            identity,
            reaper_scope: scope.clone(),
        })?;
        let admission = self.registry.admit(process.clone())?;

        let mut init = scope.init.lock();
        if init.is_some() {
            drop(init);
            drop(admission);
            drop(job_control);
            return Err(ProcessError::AlreadyExists);
        }
        job_control.commit();
        admission.commit();
        *init = Some(process.clone());
        Ok(process)
    }

    /// Reserves an unpublished child process and registry capacity credit.
    ///
    /// The adapter must serialize this admission and its final commit against
    /// [`exit`](Self::exit) for the same parent, as it already must serialize
    /// the runtime resources installed between prepare and commit.
    pub fn prepare_fork(
        &self,
        parent: &Arc<Process<Z>>,
        pid: Pid,
        exit_signal: Option<u8>,
    ) -> Result<ProcessAdmission<Z>, ProcessError> {
        self.prepare_fork_with_identity(parent, pid, exit_signal, ())
    }

    /// Reserves a child with immutable caller-owned identity metadata.
    pub fn prepare_fork_with_identity<I>(
        &self,
        parent: &Arc<Process<Z>>,
        pid: Pid,
        exit_signal: Option<u8>,
        identity: I,
    ) -> Result<ProcessAdmission<Z>, ProcessError>
    where
        I: Any + Send + Sync,
    {
        self.prepare_fork_in_reaper_scope_with_identity(
            parent,
            &parent.reaper_scope,
            pid,
            exit_signal,
            identity,
        )
    }

    /// Reserves a child in an already initialized reparenting scope.
    pub fn prepare_fork_in_reaper_scope_with_identity<I>(
        &self,
        parent: &Arc<Process<Z>>,
        scope: &Arc<ReaperScope<Z>>,
        pid: Pid,
        exit_signal: Option<u8>,
        identity: I,
    ) -> Result<ProcessAdmission<Z>, ProcessError>
    where
        I: Any + Send + Sync,
    {
        self.registry.ensure_published(parent)?;
        self.ensure_scope_owned(scope)?;
        if scope.init_process().is_none() {
            return Err(ProcessError::NotInitialized);
        }
        if parent.is_zombie() {
            return Err(ProcessError::NotLive);
        }
        let identity: Arc<dyn Any + Send + Sync> =
            Arc::try_new(identity).map_err(|_| ProcessError::NoMemory)?;
        let process = Process::try_allocate(ProcessAllocation {
            registry: &self.registry,
            pid,
            init: false,
            parent: Some(parent),
            group: parent.group(),
            exit_signal,
            identity,
            reaper_scope: scope.clone(),
        })?;
        self.registry.admit(process)
    }

    /// Reserves the init process of a newly created child reparenting scope.
    /// Its ordinary parent remains `parent`, while its descendants reparent to
    /// itself or an eligible subreaper in `scope`.
    pub fn prepare_fork_as_reaper_scope_init_with_identity<I>(
        &self,
        parent: &Arc<Process<Z>>,
        scope: &Arc<ReaperScope<Z>>,
        pid: Pid,
        exit_signal: Option<u8>,
        identity: I,
    ) -> Result<ScopedInitProcessAdmission<Z>, ProcessError>
    where
        I: Any + Send + Sync,
    {
        self.registry.ensure_published(parent)?;
        self.ensure_scope_owned(scope)?;
        if parent.is_zombie() || scope.init_process().is_some() {
            return Err(ProcessError::NotLive);
        }
        let identity: Arc<dyn Any + Send + Sync> =
            Arc::try_new(identity).map_err(|_| ProcessError::NoMemory)?;
        let process = Process::try_allocate(ProcessAllocation {
            registry: &self.registry,
            pid,
            init: true,
            parent: Some(parent),
            group: parent.group(),
            exit_signal,
            identity,
            reaper_scope: scope.clone(),
        })?;
        let admission = self.registry.admit(process)?;
        Ok(ScopedInitProcessAdmission {
            admission,
            scope: scope.clone(),
            outer_scope: parent.reaper_scope.clone(),
        })
    }

    fn ensure_scope_owned(&self, scope: &Arc<ReaperScope<Z>>) -> Result<(), ProcessError> {
        scope
            .registry
            .upgrade()
            .filter(|registry| Arc::ptr_eq(registry, &self.registry))
            .map(|_| ())
            .ok_or(ProcessError::WrongDomain)
    }

    /// Reserves a thread membership in an already published live process.
    ///
    /// Admission and the zombie transition share the process thread-group
    /// lock. If exit linearizes first this returns [`ProcessError::NotLive`];
    /// if this reservation linearizes first, exit fails until the token is
    /// committed and the thread exits or the token is dropped.
    pub fn prepare_thread(
        &self,
        process: &Arc<Process<Z>>,
        tid: Pid,
    ) -> Result<ThreadAdmission<Z>, ProcessError> {
        self.registry.ensure_published(process)?;
        process.prepare_thread_in(&self.registry, tid, false)
    }

    /// Removes one live thread with an atomic final-exit admission.
    ///
    /// Non-final removal completes immediately. For the final membership, this
    /// method marks exit prepared before unlinking the TID while holding the
    /// domain topology and process thread-group locks. Dropping the returned
    /// final token restores the exact removed node without allocation.
    pub fn exit_thread(
        &self,
        process: &Arc<Process<Z>>,
        tid: Pid,
        exit_code: i32,
    ) -> Result<ThreadExitTransition<Z>, ProcessError> {
        self.registry.ensure_published(process)?;
        self.init_process().ok_or(ProcessError::NotInitialized)?;
        let topology = self.registry.topology.lock();
        let mut tg = process.tg.lock();
        let node = tg.threads.find(&tid).get().and_then(|thread| {
            thread
                .live
                .load(Ordering::Relaxed)
                .then_some(thread as *const ThreadNode)
        });
        let Some(node) = node else {
            return Ok(ThreadExitTransition::NotFound);
        };

        // A reserved-but-unpublished membership may still become live. Do not
        // consume the last currently-live thread and strand a zero-live,
        // non-zombie process if that reservation later rolls back. Adapters
        // normally serialize clone publication with their lifecycle gate; the
        // core still rejects violations of that outer contract safely.
        if tg.live_threads == 1 && tg.memberships != 1 {
            return Err(ProcessError::Busy);
        }

        if tg.live_threads != 1 {
            if !tg.group_exited {
                tg.exit_code = exit_code;
            }
            let removed = Process::<Z>::detach_thread_locked(&mut tg, node);
            drop(tg);
            drop(topology);
            let Some(removed) = removed else {
                return Ok(ThreadExitTransition::NotFound);
            };
            drop(removed);
            self.registry.release_thread_member();
            return Ok(ThreadExitTransition::LiveThreadsRemain);
        }

        if process.is_init() || process.is_zombie.load(Ordering::Acquire) {
            return Err(ProcessError::NotLive);
        }
        if tg.exit_prepared {
            return Err(ProcessError::Busy);
        }

        if !tg.group_exited {
            tg.exit_code = exit_code;
        }
        let removed = Process::<Z>::detach_thread_locked(&mut tg, node);
        let Some(departing_thread) = removed else {
            drop(tg);
            drop(topology);
            return Ok(ThreadExitTransition::NotFound);
        };
        tg.exit_prepared = true;
        drop(tg);
        drop(topology);

        Ok(ThreadExitTransition::FinalThread(ProcessExitAdmission {
            registry: self.registry.clone(),
            process: process.clone(),
            departing_thread: Some(departing_thread),
            committed: false,
        }))
    }

    /// Validates and exclusively reserves the final zombie transition.
    ///
    /// The returned token proves that `process` belongs to this domain, is
    /// published, has no live or reserved thread memberships, is not init or
    /// already a zombie, and has a valid reaper. While the token exists, new
    /// thread admission and competing exit publication are rejected.
    pub fn prepare_exit(
        &self,
        process: &Arc<Process<Z>>,
    ) -> Result<ProcessExitAdmission<Z>, ProcessError> {
        self.registry.ensure_published(process)?;
        if process.is_init() || process.is_zombie() {
            return Err(ProcessError::NotLive);
        }
        self.init_process().ok_or(ProcessError::NotInitialized)?;
        let topology = self.registry.topology.lock();
        let mut tg = process.tg.lock();
        if tg.memberships != 0 {
            return Err(ProcessError::NotLive);
        }
        if tg.exit_prepared {
            return Err(ProcessError::Busy);
        }
        if process.zombie_payload.lock().is_some() {
            return Err(ProcessError::NotLive);
        }
        tg.exit_prepared = true;
        drop(tg);
        drop(topology);
        Ok(ProcessExitAdmission {
            registry: self.registry.clone(),
            process: process.clone(),
            departing_thread: None,
            committed: false,
        })
    }

    /// Creates a new session/group identity and moves `process` into it.
    pub fn try_create_session(
        &self,
        process: &Arc<Process<Z>>,
    ) -> Result<Option<CreatedSession<Z>>, ProcessError> {
        self.registry.ensure_published(process)?;
        if !process.is_live() {
            return Err(ProcessError::NotLive);
        }
        let old_group = process.group();
        if old_group.session.sid() == process.pid {
            return Ok(None);
        }
        let session = Session::try_new(process.pid, &self.registry)?;
        let group = ProcessGroup::try_new(process.pid, &session)?;
        let admission = self
            .registry
            .admit_session_group(session.clone(), group.clone(), true)?;
        self.registry.reserve_group_member(&group, true)?;
        let topology = self.registry.topology.lock();
        if !process.is_live() {
            drop(topology);
            self.registry.release_group_member(&group);
            drop(admission);
            return Err(ProcessError::NotLive);
        }
        if process.group().session.sid() == process.pid {
            drop(topology);
            self.registry.release_group_member(&group);
            drop(admission);
            return Ok(None);
        }
        let previous = process.replace_group(group.clone());
        admission.commit();
        drop(topology);
        self.registry.release_group_member(&previous);
        Ok(Some((session, group)))
    }

    /// Creates a unique group in the current session and moves `process` into it.
    pub fn try_create_group(
        &self,
        process: &Arc<Process<Z>>,
    ) -> Result<Option<Arc<ProcessGroup<Z>>>, ProcessError> {
        self.registry.ensure_published(process)?;
        if !process.is_live() {
            return Err(ProcessError::NotLive);
        }
        let old_group = process.group();
        if old_group.pgid() == process.pid {
            return Ok(None);
        }
        let session = old_group.session();
        let group = ProcessGroup::try_new(process.pid, &session)?;
        let admission = self
            .registry
            .admit_session_group(session, group.clone(), false)?;
        self.registry.reserve_group_member(&group, true)?;
        let topology = self.registry.topology.lock();
        if !process.is_live() {
            drop(topology);
            self.registry.release_group_member(&group);
            drop(admission);
            return Err(ProcessError::NotLive);
        }
        let current_group = process.group();
        if current_group.pgid() == process.pid {
            drop(topology);
            self.registry.release_group_member(&group);
            drop(admission);
            return Ok(None);
        }
        if !Arc::ptr_eq(&current_group.session, &group.session) {
            drop(topology);
            self.registry.release_group_member(&group);
            drop(admission);
            return Err(ProcessError::Busy);
        }
        let previous = process.replace_group(group.clone());
        admission.commit();
        drop(topology);
        self.registry.release_group_member(&previous);
        Ok(Some(group))
    }

    /// Moves `process` to a live group in the same session.
    pub fn move_to_group(
        &self,
        process: &Arc<Process<Z>>,
        group: &Arc<ProcessGroup<Z>>,
    ) -> Result<bool, ProcessError> {
        self.registry.ensure_published(process)?;
        if !process.is_live() {
            return Err(ProcessError::NotLive);
        }
        self.registry.ensure_group_live(group)?;
        let current = process.group.lock();
        if Arc::ptr_eq(&current, group) {
            return Ok(true);
        }
        if !Arc::ptr_eq(&current.session, &group.session) {
            return Ok(false);
        }
        drop(current);
        self.registry.reserve_group_member(group, false)?;
        let topology = self.registry.topology.lock();
        if !process.is_live() || self.registry.ensure_group_live(group).is_err() {
            drop(topology);
            self.registry.release_group_member(group);
            return Err(ProcessError::NotLive);
        }
        let mut current = process.group.lock();
        if Arc::ptr_eq(&current, group) {
            drop(current);
            drop(topology);
            self.registry.release_group_member(group);
            return Ok(true);
        }
        if !Arc::ptr_eq(&current.session, &group.session) {
            drop(current);
            drop(topology);
            self.registry.release_group_member(group);
            return Ok(false);
        }
        let previous = core::mem::replace(&mut *current, group.clone());
        drop(current);
        drop(topology);
        self.registry.release_group_member(&previous);
        Ok(true)
    }

    /// Marks a process zombie and reparents its children without allocation.
    ///
    /// `inherited_zombie` runs outside registry and parent-pointer locks for
    /// each already-zombie child moved to the new reaper.
    /// The adapter must serialize this operation against child admission for
    /// `process`; the crate deliberately does not hide a kernel lifecycle lock.
    pub fn exit(
        &self,
        process: &Arc<Process<Z>>,
        payload: Arc<Z>,
        mut inherited_zombie: impl FnMut(Arc<Process<Z>>),
    ) -> Result<ExitOutcome, ProcessError> {
        self.registry.ensure_published(process)?;
        if process.is_init() {
            return Ok(ExitOutcome::InitProcess);
        }
        if process.is_zombie() {
            return Ok(ExitOutcome::AlreadyZombie);
        }
        let exit = self.prepare_exit(process)?;
        Ok(exit.commit(payload, &mut inherited_zombie).outcome())
    }

    /// Reaps a zombie from this domain, returning false for invalid or duplicate reap.
    pub fn reap(&self, process: &Process<Z>) -> Result<bool, ProcessError> {
        if !self.registry.belongs(process) {
            return Err(ProcessError::WrongDomain);
        }
        let topology = self.registry.topology.lock();
        if process.reaped.load(Ordering::Acquire) {
            drop(topology);
            return Ok(false);
        }
        self.registry.ensure_published(process)?;
        if process.tg.lock().memberships != 0 {
            drop(topology);
            return Err(ProcessError::NotLive);
        }
        if !process.is_zombie()
            || process
                .reaped
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            drop(topology);
            return Ok(false);
        }

        let group = process.group();
        let removed = {
            let mut state = self.registry.state.lock();
            // SAFETY: registry identity was checked above, the linked bit is
            // true, and this registry exclusively owns the intrusive tree.
            let removed = unsafe {
                state
                    .entries
                    .cursor_mut_from_ptr(process as *const Process<Z>)
                    .remove()
            };
            if removed.is_some() {
                decrement_count(&mut state.memberships);
            }
            removed
        };
        let existed = removed.is_some();
        drop(topology);
        drop(removed);
        if existed {
            self.registry.release_group_member(&group);
        }
        Ok(existed)
    }
}

/// Result of one committed final process-exit transaction.
///
/// The notification parent is captured while holding the exiting process's
/// parent pointer at the zombie publication linearization point.
pub struct CommittedProcessExit<Z> {
    outcome: ExitOutcome,
    notification_parent: Option<Arc<Process<Z>>>,
}

/// One process whose parent pointer was changed by a committed exit batch.
///
/// The process and its exact replacement reaper are retained by the enclosing
/// [`ProcessReparentBatch`]. Consumers therefore compare stable object
/// identities rather than PIDs which may later be reused.
pub struct ReparentedProcess<Z> {
    child: Arc<Process<Z>>,
    was_zombie: bool,
}

impl<Z> ReparentedProcess<Z> {
    /// Returns the process whose parent pointer was changed in this batch.
    pub fn child(&self) -> &Arc<Process<Z>> {
        &self.child
    }

    /// Returns whether the child had already published zombie state when it
    /// was moved. Adapters commonly use this to wake the new wait parent.
    pub fn was_zombie(&self) -> bool {
        self.was_zombie
    }
}

/// Allocation-free handoff for one process-exit reparent topology section.
///
/// Every child in the batch was moved to [`reaper`](Self::reaper) while the
/// process-domain topology lock was held. The batch is constructed from fixed
/// stack storage and is delivered only after every core topology, thread-group,
/// and parent-pointer guard has been released. Different batches from the same
/// exit may name different reapers if a selected subreaper exits or changes
/// state between bounded topology sections.
pub struct ProcessReparentBatch<Z> {
    reaper: Arc<Process<Z>>,
    reparented: [Option<ReparentedProcess<Z>>; REPARENT_BATCH_LIMIT],
    len: usize,
}

impl<Z> ProcessReparentBatch<Z> {
    /// Returns the exact process selected as reaper for this core transaction.
    pub fn reaper(&self) -> &Arc<Process<Z>> {
        &self.reaper
    }

    /// Iterates only processes whose parent pointer was actually replaced.
    pub fn reparented(&self) -> impl ExactSizeIterator<Item = &ReparentedProcess<Z>> {
        self.reparented[..self.len].iter().map(|entry| {
            entry
                .as_ref()
                .expect("reparent batch prefix is initialized")
        })
    }

    /// Returns the number of processes actually moved in this batch.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether this batch moved no process parent pointers.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<Z> CommittedProcessExit<Z> {
    /// Returns the zombie transition outcome.
    pub fn outcome(&self) -> ExitOutcome {
        self.outcome
    }

    /// Returns the parent observed atomically with zombie publication.
    pub fn notification_parent(&self) -> Option<&Arc<Process<Z>>> {
        self.notification_parent.as_ref()
    }
}

/// Exclusive, fully validated final process-exit transaction.
///
/// The token reserves the zero-membership zombie transition without taking a
/// payload. Dropping it rolls that reservation back; consuming it with
/// [`commit`](Self::commit) publishes the supplied immutable payload and
/// reparents children without a fallible branch.
pub struct ProcessExitAdmission<Z> {
    registry: Arc<ProcessRegistry<Z>>,
    process: Arc<Process<Z>>,
    departing_thread: Option<Arc<ThreadNode>>,
    committed: bool,
}

impl<Z> ProcessExitAdmission<Z> {
    /// Returns the exact process reserved for final exit.
    pub fn process(&self) -> &Arc<Process<Z>> {
        &self.process
    }

    /// Publishes the durable payload, transitions to zombie state, and
    /// reparents children without allocation or a recoverable error.
    pub fn commit(
        self,
        payload: Arc<Z>,
        inherited_zombie: impl FnMut(Arc<Process<Z>>),
    ) -> CommittedProcessExit<Z> {
        self.commit_with_reparent_handoff(payload, inherited_zombie, |_| {})
    }

    /// Publishes the durable payload and hands every authoritative child to
    /// reaper mapping to an adapter after releasing all core topology guards.
    ///
    /// `reparent_batch` may allocate, destroy arbitrary adapter objects, or
    /// acquire sleepable locks because it is never invoked under a core spin
    /// guard. Core-owned `Arc` and `Weak` values displaced while a topology
    /// section is active are retained in fixed stack slots and destroyed only
    /// after that section has ended. The callback must consume each batch
    /// synchronously; retaining identities requires explicitly cloning them.
    pub fn commit_with_reparent_handoff(
        mut self,
        payload: Arc<Z>,
        mut inherited_zombie: impl FnMut(Arc<Process<Z>>),
        mut reparent_batch: impl FnMut(&ProcessReparentBatch<Z>),
    ) -> CommittedProcessExit<Z> {
        // Job-control replacement and the zombie state transition share this
        // topology lock. The nested lifecycle order is topology -> thread
        // group -> own parent pointer -> payload slot. No adapter callback runs
        // while any of those IRQ-disabled guards is held.
        let topology = self.registry.topology.lock();
        let mut tg = self.process.tg.lock();
        debug_assert!(tg.exit_prepared);
        debug_assert_eq!(tg.memberships, 0);
        let parent = self.process.parent.lock();
        let notification_parent = parent.upgrade();
        let mut payload_slot = self.process.zombie_payload.lock();
        debug_assert!(payload_slot.is_none());
        *payload_slot = Some(payload);
        self.process.is_zombie.store(true, Ordering::Release);
        tg.exit_prepared = false;
        self.committed = true;
        drop(payload_slot);
        drop(parent);
        drop(tg);
        drop(topology);

        if let Some(departing_thread) = self.departing_thread.take() {
            drop(departing_thread);
            self.registry.release_thread_member();
        }

        // Reparent all non-zombie children in one or more topology sections.
        // A moved zombie is reported immediately after releasing the IRQ-off
        // guard, then selection is repeated. If the previously selected
        // subreaper exits between sections, its own commit either reparents the
        // children already moved to it or this loop selects the next live
        // ancestor for the remaining children.
        let mut children = self.registry.processes_through_current_max();
        let mut finished = false;
        let mut deferred_child = None;
        while !finished {
            let mut reparented = [const { None }; REPARENT_BATCH_LIMIT];
            let mut retired_scanned = [const { None }; REPARENT_BATCH_LIMIT];
            let mut retired_parents = [const { None }; REPARENT_BATCH_LIMIT];
            let mut retired_terminal = None;
            let topology = self.registry.topology.lock();
            let mut reparented_len = 0;
            let mut retired_scanned_len = 0;
            let mut batch_len = 0;
            let mut batch_reaper = None;
            while batch_len < REPARENT_BATCH_LIMIT {
                let child = match deferred_child
                    .take()
                    .map(ProcessCursorStep::Process)
                    .unwrap_or_else(|| children.next_raw())
                {
                    ProcessCursorStep::Process(child) => child,
                    ProcessCursorStep::Finished(retired) => {
                        retired_terminal = retired;
                        finished = true;
                        break;
                    }
                };
                batch_len += 1;
                if !child.published.load(Ordering::Acquire) {
                    retired_scanned[retired_scanned_len] = Some(child);
                    retired_scanned_len += 1;
                    continue;
                }
                if child.parent.lock().as_ptr() != Arc::as_ptr(&self.process) {
                    retired_scanned[retired_scanned_len] = Some(child);
                    retired_scanned_len += 1;
                    continue;
                }
                let selection =
                    select_reaper_for_exit_locked(&self.process, &child, &child.reaper_scope);
                let reaper = selection.reaper;
                let retired_first_ancestor = selection.retired_first_ancestor;
                if batch_reaper
                    .as_ref()
                    .is_some_and(|current: &Arc<Process<Z>>| !Arc::ptr_eq(current, &reaper))
                {
                    deferred_child = Some(child);
                    drop(retired_first_ancestor);
                    drop(reaper);
                    break;
                }
                let reaper_weak = Arc::downgrade(&reaper);
                if let Some(old_parent) = reparent_if_owned(&child, &self.process, &reaper_weak) {
                    let was_zombie = child.is_zombie();
                    retired_parents[reparented_len] = Some(old_parent);
                    reparented[reparented_len] = Some(ReparentedProcess { child, was_zombie });
                    reparented_len += 1;
                    if batch_reaper.is_none() {
                        batch_reaper = Some(reaper.clone());
                    }
                    drop(reaper_weak);
                    drop(retired_first_ancestor);
                    drop(reaper);
                    if was_zombie {
                        // Preserve the existing prompt inherited-zombie wakeup
                        // boundary instead of delaying it behind a full batch.
                        break;
                    }
                } else {
                    drop(reaper_weak);
                    drop(retired_first_ancestor);
                    drop(reaper);
                    retired_scanned[retired_scanned_len] = Some(child);
                    retired_scanned_len += 1;
                }
            }
            #[cfg(test)]
            MAX_REPARENT_BATCH_OBSERVED.fetch_max(batch_len, Ordering::Relaxed);
            drop(topology);

            // Weak/Arc reference-count decrements, arbitrary adapter work, and
            // inherited-zombie notification all happen after the IRQ-disabled
            // core topology section. The fixed arrays themselves never allocate.
            drop(retired_parents);
            drop(retired_scanned);
            drop(retired_terminal);

            if reparented_len != 0 {
                let batch = ProcessReparentBatch {
                    reaper: batch_reaper.expect("reparented batch has a selected reaper"),
                    reparented,
                    len: reparented_len,
                };
                reparent_batch(&batch);
                for moved in batch.reparented().filter(|moved| moved.was_zombie()) {
                    inherited_zombie(moved.child().clone());
                }
                drop(batch);
            }
        }

        CommittedProcessExit {
            outcome: ExitOutcome::BecameZombie,
            notification_parent,
        }
    }
}

impl<Z> Drop for ProcessExitAdmission<Z> {
    fn drop(&mut self) {
        if !self.committed {
            let mut tg = self.process.tg.lock();
            if let Some(departing_thread) = self.departing_thread.take() {
                debug_assert!(!departing_thread.link.is_linked());
                debug_assert!(!departing_thread.live.load(Ordering::Relaxed));
                departing_thread.live.store(true, Ordering::Relaxed);
                tg.threads.insert(departing_thread);
                tg.memberships += 1;
                tg.live_threads += 1;
            }
            tg.exit_prepared = false;
        }
    }
}

/// Allocation-free PID-ordered iterator over a [`ProcessRegistry`].
///
/// Each lock acquisition clones at most one intrusive node. The initial
/// membership count bounds the walk under concurrent fork and reap activity.
pub struct Processes<'a, Z> {
    registry: &'a ProcessRegistry<Z>,
    last: Option<Arc<Process<Z>>>,
    after: Option<Pid>,
    remaining: usize,
    finished: bool,
}

impl<Z> Iterator for Processes<'_, Z> {
    type Item = Arc<Process<Z>>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished && self.remaining != 0 {
            let state = self.registry.state.lock();
            let next = if let Some(last) = self
                .last
                .as_ref()
                .filter(|last| last.registry_link.is_linked())
            {
                // SAFETY: a linked node with matching registry identity is in
                // this tree; the state lock prevents concurrent removal.
                let mut cursor = unsafe { state.entries.cursor_from_ptr(Arc::as_ptr(last)) };
                cursor.move_next();
                cursor.clone_pointer()
            } else if let Some(after) = self.after.as_ref() {
                state
                    .entries
                    .lower_bound(Bound::Excluded(after))
                    .clone_pointer()
            } else {
                state.entries.front().clone_pointer()
            };
            drop(state);

            let Some(next) = next else {
                self.finished = true;
                break;
            };
            self.remaining -= 1;
            self.after = Some(next.pid);
            let last = self.last.replace(next.clone());
            drop(last);
            if next.published.load(Ordering::Acquire) {
                return Some(next);
            }
        }

        self.finished = true;
        let last = self.last.take();
        drop(last);
        None
    }
}

/// PID-monotonic process cursor bounded by the registry maximum captured at
/// construction. Used by multi-section exit reparenting.
struct ProcessesThroughCurrentMax<'a, Z> {
    registry: &'a ProcessRegistry<Z>,
    after: Option<Pid>,
    upper_bound: Option<Pid>,
    finished: bool,
}

enum ProcessCursorStep<Z> {
    Process(Arc<Process<Z>>),
    /// The optional process lies above the cursor's captured maximum and is
    /// returned to the caller so its `Arc` can be destroyed outside a topology
    /// section.
    Finished(Option<Arc<Process<Z>>>),
}

impl<Z> ProcessesThroughCurrentMax<'_, Z> {
    fn next_raw(&mut self) -> ProcessCursorStep<Z> {
        let Some(upper_bound) = self.upper_bound else {
            self.finished = true;
            return ProcessCursorStep::Finished(None);
        };
        if self.finished {
            return ProcessCursorStep::Finished(None);
        }

        let state = self.registry.state.lock();
        let next = if let Some(after) = self.after.as_ref() {
            state
                .entries
                .lower_bound(Bound::Excluded(after))
                .clone_pointer()
        } else {
            state.entries.front().clone_pointer()
        };
        drop(state);

        let Some(next) = next else {
            self.finished = true;
            return ProcessCursorStep::Finished(None);
        };
        if next.pid > upper_bound {
            self.finished = true;
            return ProcessCursorStep::Finished(Some(next));
        }
        self.after = Some(next.pid);
        ProcessCursorStep::Process(next)
    }
}

impl<Z> Iterator for ProcessesThroughCurrentMax<'_, Z> {
    type Item = Arc<Process<Z>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.next_raw() {
                ProcessCursorStep::Process(process) => {
                    if process.published.load(Ordering::Acquire) {
                        return Some(process);
                    }
                    drop(process);
                }
                ProcessCursorStep::Finished(retired) => {
                    drop(retired);
                    return None;
                }
            }
        }
    }
}

struct ProcessAllocation<'a, Z> {
    registry: &'a Arc<ProcessRegistry<Z>>,
    pid: Pid,
    init: bool,
    parent: Option<&'a Arc<Process<Z>>>,
    group: Arc<ProcessGroup<Z>>,
    exit_signal: Option<u8>,
    identity: Arc<dyn Any + Send + Sync>,
    reaper_scope: Arc<ReaperScope<Z>>,
}

impl<Z> Process<Z> {
    fn try_allocate(allocation: ProcessAllocation<'_, Z>) -> Result<Arc<Self>, ProcessError> {
        let ProcessAllocation {
            registry,
            pid,
            init,
            parent,
            group,
            exit_signal,
            identity,
            reaper_scope,
        } = allocation;
        Arc::try_new(Self {
            registry_link: RBTreeAtomicLink::new(),
            registry: Arc::downgrade(registry),
            published: AtomicBool::new(false),
            pid,
            identity,
            reaper_scope,
            init,
            is_zombie: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            tg: SpinNoIrq::new(ThreadGroup::new()),
            exit_signal,
            zombie_payload: SpinNoIrq::new(None),
            child_subreaper: AtomicBool::new(false),
            parent: SpinNoIrq::new(parent.map(Arc::downgrade).unwrap_or_default()),
            group: SpinNoIrq::new(group),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// The process ID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Returns immutable process identity metadata of the requested type.
    pub fn identity<T: Any>(&self) -> Option<&T> {
        self.identity.downcast_ref()
    }

    /// Returns the process's immutable reparenting scope.
    pub fn reaper_scope(&self) -> &Arc<ReaperScope<Z>> {
        &self.reaper_scope
    }

    /// Returns whether this is the unique init process of its domain.
    pub fn is_init(&self) -> bool {
        self.init
    }

    /// Returns the signal selected by the caller for parent notification.
    pub fn exit_signal(&self) -> Option<u8> {
        self.exit_signal
    }

    /// Retains the caller-defined zombie payload, if it has been published.
    ///
    /// The exit path receives an already allocated [`Arc`], so publication is
    /// fixed-cost and cannot fail while lifecycle locks are held. Retention
    /// performs only an atomic reference-count increment under the payload
    /// lock; payload destruction always happens after that guard is released.
    pub fn zombie_payload(&self) -> Option<Arc<Z>> {
        self.zombie_payload.lock().as_ref().map(Arc::clone)
    }

    /// Returns whether this process acts as a child subreaper.
    pub fn is_child_subreaper(&self) -> bool {
        self.child_subreaper.load(Ordering::Acquire)
    }

    /// Configures child-subreaper state.
    pub fn set_child_subreaper(&self, enabled: bool) {
        self.child_subreaper.store(enabled, Ordering::Release);
    }

    /// Returns the current parent, if it remains alive.
    pub fn parent(&self) -> Option<Arc<Process<Z>>> {
        self.parent.lock().upgrade()
    }

    /// Fallibly snapshots this process's published children in `registry`.
    pub fn try_children(
        self: &Arc<Self>,
        registry: &ProcessRegistry<Z>,
    ) -> Result<Vec<Arc<Process<Z>>>, ProcessError> {
        registry.ensure_published(self)?;
        registry.try_collect_process_values(|child| {
            child
                .parent()
                .is_some_and(|parent| Arc::ptr_eq(&parent, self))
                .then(|| child.clone())
        })
    }

    /// Returns this process's process group.
    pub fn group(&self) -> Arc<ProcessGroup<Z>> {
        self.group.lock().clone()
    }

    fn replace_group(&self, group: Arc<ProcessGroup<Z>>) -> Arc<ProcessGroup<Z>> {
        core::mem::replace(&mut *self.group.lock(), group)
    }

    fn prepare_thread_in(
        self: &Arc<Self>,
        registry: &Arc<ProcessRegistry<Z>>,
        tid: Pid,
        allow_unpublished: bool,
    ) -> Result<ThreadAdmission<Z>, ProcessError> {
        if !registry.belongs(self) {
            return Err(ProcessError::WrongDomain);
        }
        let node = Arc::try_new(ThreadNode {
            link: RBTreeAtomicLink::new(),
            tid,
            live: AtomicBool::new(false),
        })
        .map_err(|_| ProcessError::NoMemory)?;
        registry.reserve_thread_member()?;
        let limit = registry.membership_limit;
        let mut tg = self.tg.lock();
        if self.is_zombie.load(Ordering::Acquire) || self.reaped.load(Ordering::Acquire) {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::NotLive);
        }
        if tg.exit_prepared || tg.group_exited {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::NotLive);
        }
        if !allow_unpublished
            && (!self.published.load(Ordering::Acquire) || !registry.contains_process(self))
        {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::NotPublished);
        }
        if tg.memberships >= limit {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::Capacity);
        }
        if !tg.threads.find(&tid).is_null() {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::AlreadyExists);
        }
        let Some(next_memberships) = tg.memberships.checked_add(1).filter(|next| *next <= limit)
        else {
            drop(tg);
            registry.release_thread_member();
            return Err(ProcessError::Capacity);
        };
        tg.threads.insert(node.clone());
        tg.memberships = next_memberships;
        drop(tg);
        Ok(ThreadAdmission {
            registry: registry.clone(),
            process: self.clone(),
            node,
            committed: false,
        })
    }

    fn detach_thread_locked(
        tg: &mut ThreadGroup,
        node: *const ThreadNode,
    ) -> Option<Arc<ThreadNode>> {
        // SAFETY: callers obtain `node` from this tree while holding `tg`.
        let removed = unsafe { tg.threads.cursor_mut_from_ptr(node).remove() };
        if let Some(node) = removed.as_ref() {
            decrement_count(&mut tg.memberships);
            if node.live.swap(false, Ordering::Relaxed) {
                decrement_count(&mut tg.live_threads);
            }
        }
        removed
    }

    /// Removes a live thread without updating process exit state.
    pub fn remove_thread(&self, tid: Pid) -> bool {
        let registry = self.registry.upgrade();
        let topology = registry.as_ref().map(|registry| registry.topology.lock());
        let removed = {
            let mut tg = self.tg.lock();
            let node = tg.threads.find(&tid).get().and_then(|thread| {
                thread
                    .live
                    .load(Ordering::Relaxed)
                    .then_some(thread as *const ThreadNode)
            });
            node.and_then(|node| Self::detach_thread_locked(&mut tg, node))
        };
        drop(topology);
        let existed = removed.is_some();
        drop(removed);
        if existed && let Some(registry) = registry {
            registry.release_thread_member();
        }
        existed
    }

    /// Removes a live thread and reports the exact transition.
    pub fn exit_thread(&self, tid: Pid, exit_code: i32) -> ThreadExitOutcome {
        let registry = self.registry.upgrade();
        let topology = registry.as_ref().map(|registry| registry.topology.lock());
        let mut tg = self.tg.lock();
        let node = tg.threads.find(&tid).get().and_then(|thread| {
            thread
                .live
                .load(Ordering::Relaxed)
                .then_some(thread as *const ThreadNode)
        });
        let Some(node) = node else {
            return ThreadExitOutcome::NotFound;
        };
        if !tg.group_exited {
            tg.exit_code = exit_code;
        }
        let removed = Self::detach_thread_locked(&mut tg, node);
        let empty = tg.live_threads == 0;
        drop(tg);
        drop(topology);
        let Some(removed) = removed else {
            return ThreadExitOutcome::NotFound;
        };
        drop(removed);
        if let Some(registry) = registry {
            registry.release_thread_member();
        }
        if empty {
            ThreadExitOutcome::FinalThread
        } else {
            ThreadExitOutcome::LiveThreadsRemain
        }
    }

    /// Returns the live thread count without allocating.
    pub fn thread_count(&self) -> usize {
        self.tg.lock().live_threads
    }

    /// Returns whether `tid` is the only live thread.
    pub fn has_only_thread(&self, tid: Pid) -> bool {
        let tg = self.tg.lock();
        tg.live_threads == 1
            && tg
                .threads
                .find(&tid)
                .get()
                .is_some_and(|thread| thread.live.load(Ordering::Relaxed))
    }

    /// Fallibly snapshots all live thread IDs.
    pub fn try_threads(self: &Arc<Self>) -> Result<Vec<Pid>, ProcessError> {
        let limit = self
            .registry
            .upgrade()
            .ok_or(ProcessError::WrongDomain)?
            .membership_limit;
        let mut snapshot = Vec::new();
        loop {
            snapshot.clear();
            let required = self.tg.lock().memberships;
            if snapshot.capacity() < required {
                snapshot
                    .try_reserve_exact(required)
                    .map_err(|_| ProcessError::NoMemory)?;
            }

            let mut overflow = false;
            for tid in self.thread_ids() {
                if snapshot.len() == snapshot.capacity() {
                    overflow = true;
                    break;
                }
                snapshot.push(tid);
            }
            if !overflow {
                return Ok(snapshot);
            }

            let current = snapshot.capacity();
            if current >= limit {
                return Err(ProcessError::Capacity);
            }
            let target = current.max(1).saturating_mul(2).min(limit);
            snapshot.clear();
            snapshot
                .try_reserve_exact(target)
                .map_err(|_| ProcessError::NoMemory)?;
        }
    }

    /// Iterates live thread IDs without allocating.
    pub fn thread_ids(self: &Arc<Self>) -> ThreadIds<Z> {
        ThreadIds {
            process: self.clone(),
            last: None,
            after: None,
            remaining: self.tg.lock().memberships,
            finished: false,
        }
    }

    /// Visits every live thread ID without allocating.
    pub fn for_each_thread(self: &Arc<Self>, mut visitor: impl FnMut(Pid)) {
        for tid in self.thread_ids() {
            visitor(tid);
        }
    }

    /// Returns whether this is a non-zombie process with a live thread.
    pub fn is_live(&self) -> bool {
        !self.is_zombie() && self.thread_count() != 0
    }

    /// Returns whether a group-wide exit has been requested.
    pub fn is_group_exited(&self) -> bool {
        self.tg.lock().group_exited
    }

    /// Atomically records the first group-exit code and marks group exit.
    ///
    /// Returns `true` only for the caller that established the group exit.
    pub fn group_exit(&self, exit_code: i32) -> bool {
        let mut tg = self.tg.lock();
        if tg.group_exited {
            return false;
        }
        tg.exit_code = exit_code;
        tg.group_exited = true;
        true
    }

    /// Returns the stored process exit code.
    pub fn exit_code(&self) -> i32 {
        self.tg.lock().exit_code
    }

    /// Returns whether this process has transitioned to zombie state.
    pub fn is_zombie(&self) -> bool {
        self.is_zombie.load(Ordering::Acquire)
    }
}

impl<Z> fmt::Debug for Process<Z> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Process");
        builder.field("pid", &self.pid);
        let tg = self.tg.lock();
        if tg.group_exited {
            builder.field("group_exited", &tg.group_exited);
        }
        if self.is_zombie() {
            builder.field("exit_code", &tg.exit_code);
        }
        if let Some(parent) = self.parent() {
            builder.field("parent", &parent.pid());
        }
        builder.field("group", &self.group());
        builder.finish()
    }
}

struct JobControlAdmission<Z> {
    registry: Arc<ProcessRegistry<Z>>,
    session: Arc<Session<Z>>,
    group: Arc<ProcessGroup<Z>>,
    new_session: bool,
    committed: bool,
}

impl<Z> JobControlAdmission<Z> {
    fn commit(mut self) {
        if self.new_session {
            self.session.published.store(true, Ordering::Release);
        }
        self.group.published.store(true, Ordering::Release);
        self.committed = true;
    }
}

impl<Z> Drop for JobControlAdmission<Z> {
    fn drop(&mut self) {
        if !self.committed {
            self.registry.remove_empty_group(&self.group);
        }
    }
}

/// Reserved init process for a newly created reparenting scope.
///
/// The scope is bound immediately before release-publication, so a published
/// scoped process can never be observed without its fallback reaper.
pub struct ScopedInitProcessAdmission<Z> {
    admission: ProcessAdmission<Z>,
    scope: Arc<ReaperScope<Z>>,
    outer_scope: Arc<ReaperScope<Z>>,
}

impl<Z> ScopedInitProcessAdmission<Z> {
    /// Returns the unpublished namespace-init process object.
    pub fn process(&self) -> &Arc<Process<Z>> {
        self.admission.process()
    }

    /// Reserves and couples this init's initial thread to publication.
    pub fn prepare_initial_thread(
        self,
        tid: Pid,
    ) -> Result<ScopedInitialProcessAdmission<Z>, ProcessError> {
        Ok(ScopedInitialProcessAdmission {
            admission: self.admission.prepare_initial_thread(tid)?,
            scope: self.scope,
            outer_scope: self.outer_scope,
        })
    }

    /// Publishes this scope init without an initial thread.
    pub fn commit(mut self) {
        self.bind();
        self.admission.commit();
    }

    fn bind(&mut self) {
        let mut init = self.scope.init.lock();
        assert!(init.is_none(), "a reaper scope has exactly one init");
        *init = Some(self.admission.process().clone());
        drop(init);
        *self.scope.outer_scope.lock() = Some(Arc::downgrade(&self.outer_scope));
    }
}

/// Scoped counterpart to [`InitialProcessAdmission`].
pub struct ScopedInitialProcessAdmission<Z> {
    admission: InitialProcessAdmission<Z>,
    scope: Arc<ReaperScope<Z>>,
    outer_scope: Arc<ReaperScope<Z>>,
}

impl<Z> ScopedInitialProcessAdmission<Z> {
    /// Returns the unpublished namespace-init process object.
    pub fn process(&self) -> &Arc<Process<Z>> {
        self.admission.process()
    }

    /// Atomically publishes this scope init and its initial thread.
    pub fn commit(self) -> Result<Arc<Process<Z>>, ProcessError> {
        let mut init = self.scope.init.lock();
        if init.is_some() {
            return Err(ProcessError::AlreadyExists);
        }
        *init = Some(self.admission.process().clone());
        drop(init);
        *self.scope.outer_scope.lock() = Some(Arc::downgrade(&self.outer_scope));
        Ok(self.admission.commit())
    }
}

/// Reserved, fully allocated child process awaiting final publication.
pub struct ProcessAdmission<Z> {
    registry: Arc<ProcessRegistry<Z>>,
    process: Arc<Process<Z>>,
    committed: bool,
}

impl<Z> ProcessAdmission<Z> {
    /// Returns the unpublished process object.
    pub fn process(&self) -> &Arc<Process<Z>> {
        &self.process
    }

    /// Reserves an initial thread while this token still owns the unpublished
    /// process identity.
    ///
    /// The returned thread token cannot publish by itself until the process is
    /// published. Use [`commit_with_thread`](Self::commit_with_thread) to make
    /// both identities visible in one release publication.
    pub fn prepare_thread(&self, tid: Pid) -> Result<ThreadAdmission<Z>, ProcessError> {
        self.process.prepare_thread_in(&self.registry, tid, true)
    }

    /// Consumes this unpublished process admission and binds its initial
    /// thread into one infallibly publishable transaction.
    ///
    /// All allocation, identity, capacity, and domain checks finish before
    /// this returns. Keeping both tokens private inside
    /// [`InitialProcessAdmission`] prevents callers from mixing identities or
    /// publishing the thread separately from the process.
    pub fn prepare_initial_thread(
        self,
        tid: Pid,
    ) -> Result<InitialProcessAdmission<Z>, ProcessError> {
        let thread = self.prepare_thread(tid)?;
        Ok(InitialProcessAdmission {
            process: self,
            thread,
        })
    }

    /// Publishes the process against its reserved registry membership slot.
    pub fn commit(mut self) {
        let process = self.process.clone();
        let _tg = process.tg.lock();
        process.published.store(true, Ordering::Release);
        self.committed = true;
    }

    /// Publishes this process and one prepared initial thread atomically.
    ///
    /// Every allocation and capacity charge has already completed. A registry
    /// reader that observes the process's release publication also observes
    /// the initial live thread. Tokens from another process or domain are
    /// rejected and both reservations roll back when this call returns an
    /// error.
    pub fn commit_with_thread(
        mut self,
        mut thread: ThreadAdmission<Z>,
    ) -> Result<Arc<Process<Z>>, ProcessError> {
        if !Arc::ptr_eq(&self.registry, &thread.registry)
            || !Arc::ptr_eq(&self.process, &thread.process)
        {
            return Err(ProcessError::WrongDomain);
        }

        let process = self.process.clone();
        let mut tg = process.tg.lock();
        if process.published.load(Ordering::Acquire) || !self.registry.contains_process(&process) {
            return Err(ProcessError::NotPublished);
        }
        if process.is_zombie.load(Ordering::Acquire) || process.reaped.load(Ordering::Acquire) {
            return Err(ProcessError::NotLive);
        }
        thread.publish_locked(&mut tg, true)?;
        process.published.store(true, Ordering::Release);
        self.committed = true;
        drop(tg);
        Ok(process)
    }
}

impl<Z> Drop for ProcessAdmission<Z> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let group = self.process.group();
        let removed = {
            let mut state = self.registry.state.lock();
            if !self.process.registry_link.is_linked() {
                None
            } else {
                // SAFETY: the token owns a registry-matched process that was
                // inserted by `ProcessRegistry::admit` and is still linked.
                let removed = unsafe {
                    state
                        .entries
                        .cursor_mut_from_ptr(Arc::as_ptr(&self.process))
                        .remove()
                };
                if removed.is_some() {
                    decrement_count(&mut state.memberships);
                }
                removed
            }
        };
        let existed = removed.is_some();
        drop(removed);
        if existed {
            self.registry.release_group_member(&group);
        }
    }
}

/// Fully prepared process plus initial-thread publication transaction.
///
/// This type can only be constructed by
/// [`ProcessAdmission::prepare_initial_thread`], which proves that both
/// reservations belong to the same unpublished process and registry.
pub struct InitialProcessAdmission<Z> {
    process: ProcessAdmission<Z>,
    thread: ThreadAdmission<Z>,
}

impl<Z> InitialProcessAdmission<Z> {
    /// Returns the unpublished process while runtime resources are prepared.
    pub fn process(&self) -> &Arc<Process<Z>> {
        self.process.process()
    }

    /// Publishes the process and its initial live thread as one infallible
    /// transition.
    ///
    /// The composite owns the only process-publication token and a thread
    /// reservation created from that exact token. An unpublished process
    /// cannot concurrently exit or be reaped, so no checked lifecycle outcome
    /// remains at this point.
    pub fn commit(mut self) -> Arc<Process<Z>> {
        let process = self.process.process.clone();
        let mut tg = process.tg.lock();
        let outcome = self.thread.publish_locked_infallible(&mut tg);
        debug_assert_eq!(outcome, ThreadPublicationOutcome::Live);
        process.published.store(true, Ordering::Release);
        self.process.committed = true;
        drop(tg);
        process
    }
}

/// Reserved thread-group membership awaiting final publication.
pub struct ThreadAdmission<Z> {
    registry: Arc<ProcessRegistry<Z>>,
    process: Arc<Process<Z>>,
    node: Arc<ThreadNode>,
    committed: bool,
}

impl<Z> ThreadAdmission<Z> {
    fn publish_locked_infallible(&mut self, tg: &mut ThreadGroup) -> ThreadPublicationOutcome {
        debug_assert!(!self.node.live.load(Ordering::Relaxed));
        // Construction charged and linked this exact membership before
        // exposing the consuming token, so one unpublished membership exists.
        self.node.live.store(true, Ordering::Relaxed);
        let next_live = tg.live_threads + 1;
        debug_assert!(next_live <= tg.memberships);
        tg.live_threads = next_live;
        self.committed = true;
        if tg.group_exited {
            ThreadPublicationOutcome::GroupExited
        } else {
            ThreadPublicationOutcome::Live
        }
    }

    fn publish_locked(
        &mut self,
        tg: &mut ThreadGroup,
        allow_unpublished: bool,
    ) -> Result<(), ProcessError> {
        if !self.node.link.is_linked() {
            return Err(ProcessError::NotPublished);
        }
        if self.process.is_zombie.load(Ordering::Acquire)
            || self.process.reaped.load(Ordering::Acquire)
            || tg.group_exited
            || tg.exit_prepared
        {
            return Err(ProcessError::NotLive);
        }
        if !allow_unpublished
            && (!self.process.published.load(Ordering::Acquire)
                || !self.registry.contains_process(&self.process))
        {
            return Err(ProcessError::NotPublished);
        }
        if !self.node.live.load(Ordering::Relaxed) {
            let next_live = tg
                .live_threads
                .checked_add(1)
                .filter(|next| *next <= tg.memberships)
                .ok_or(ProcessError::Capacity)?;
            self.node.live.store(true, Ordering::Relaxed);
            tg.live_threads = next_live;
        }
        self.committed = true;
        Ok(())
    }

    /// Publishes the TID against its reserved process membership capacity.
    pub fn commit(mut self) -> Result<(), ProcessError> {
        let process = self.process.clone();
        let mut tg = process.tg.lock();
        self.publish_locked(&mut tg, false)
    }

    /// Publishes this already validated live-process thread reservation
    /// without a fallible post-publication branch.
    ///
    /// The token's reserved membership prevents the process from completing
    /// exit while it exists. The process and registry identity were validated
    /// before the token was returned, and the linked node is private to this
    /// consuming value.
    pub fn commit_infallible(mut self) -> ThreadPublicationOutcome {
        let process = self.process.clone();
        let mut tg = process.tg.lock();
        self.publish_locked_infallible(&mut tg)
    }
}

impl<Z> Drop for ThreadAdmission<Z> {
    fn drop(&mut self) {
        if !self.committed {
            let removed = {
                let mut tg = self.process.tg.lock();
                if !self.node.link.is_linked() {
                    None
                } else {
                    Process::<Z>::detach_thread_locked(&mut tg, Arc::as_ptr(&self.node))
                }
            };
            let existed = removed.is_some();
            drop(removed);
            if existed {
                self.registry.release_thread_member();
            }
        }
    }
}

/// Allocation-free PID-ordered iterator over a process's live thread IDs.
pub struct ThreadIds<Z> {
    process: Arc<Process<Z>>,
    last: Option<Arc<ThreadNode>>,
    after: Option<Pid>,
    remaining: usize,
    finished: bool,
}

impl<Z> Iterator for ThreadIds<Z> {
    type Item = Pid;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished && self.remaining != 0 {
            let tg = self.process.tg.lock();
            let next = if let Some(last) = self.last.as_ref().filter(|last| last.link.is_linked()) {
                // SAFETY: `last` is linked in this process's thread tree and
                // the thread-group lock prevents concurrent removal.
                let mut cursor = unsafe { tg.threads.cursor_from_ptr(Arc::as_ptr(last)) };
                cursor.move_next();
                cursor.clone_pointer()
            } else if let Some(after) = self.after.as_ref() {
                tg.threads
                    .lower_bound(Bound::Excluded(after))
                    .clone_pointer()
            } else {
                tg.threads.front().clone_pointer()
            };
            drop(tg);

            let Some(next) = next else {
                self.finished = true;
                break;
            };
            self.remaining -= 1;
            self.after = Some(next.tid);
            let last = self.last.replace(next.clone());
            drop(last);
            if next.live.load(Ordering::Relaxed) {
                return Some(next.tid);
            }
        }

        self.finished = true;
        let last = self.last.take();
        drop(last);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_counters_never_wrap_at_either_edge() {
        let counter = AtomicUsize::new(0);
        assert_eq!(release_counter(&counter), None);
        assert_eq!(counter.load(Ordering::Acquire), 0);

        counter.store(usize::MAX, Ordering::Release);
        assert_eq!(
            reserve_bounded_counter(&counter, usize::MAX),
            Err(ProcessError::Capacity)
        );
        assert_eq!(counter.load(Ordering::Acquire), usize::MAX);

        let mut plain = 0usize;
        assert!(!decrement_count(&mut plain));
        assert_eq!(plain, 0);
    }

    #[test]
    fn duplicate_internal_release_keeps_zero_domain_charge() {
        let domain = ProcessDomain::<()>::try_with_membership_limit(1).unwrap();
        domain.registry.release_thread_member();
        domain.registry.release_thread_member();
        assert_eq!(domain.registry.thread_membership_count(), 0);
    }

    #[test]
    fn reparent_compare_does_not_overwrite_a_newer_parent() {
        let domain = ProcessDomain::<()>::try_new().unwrap();
        let init = domain.try_new_init(1, None).unwrap();
        let first = domain.prepare_fork(&init, 2, None).unwrap();
        let first_process = first.process().clone();
        first.commit();
        let second = domain.prepare_fork(&init, 3, None).unwrap();
        let second_process = second.process().clone();
        second.commit();
        let child = domain.prepare_fork(&first_process, 4, None).unwrap();
        let child_process = child.process().clone();
        child.commit();

        *child_process.parent.lock() = Arc::downgrade(&second_process);
        assert!(
            reparent_if_owned(&child_process, &first_process, &Arc::downgrade(&init),).is_none()
        );
        assert!(Arc::ptr_eq(
            &child_process.parent().unwrap(),
            &second_process
        ));
    }

    #[test]
    fn exit_reparenting_bounds_each_topology_section() {
        MAX_REPARENT_BATCH_OBSERVED.store(0, Ordering::Release);
        let domain = ProcessDomain::<()>::try_new().unwrap();
        let init = domain.try_new_init(1, None).unwrap();
        domain.prepare_thread(&init, 1).unwrap().commit().unwrap();
        let parent = domain
            .prepare_fork(&init, 2, None)
            .unwrap()
            .prepare_initial_thread(2)
            .unwrap()
            .commit();
        let mut children = Vec::new();
        children.try_reserve_exact(257).unwrap();
        for pid in 3..260 {
            let admission = domain.prepare_fork(&parent, pid, None).unwrap();
            children.push(admission.process().clone());
            admission.commit();
        }

        let exit = match domain.exit_thread(&parent, 2, 0).unwrap() {
            ThreadExitTransition::FinalThread(exit) => exit,
            _ => panic!("parent must publish a final-exit admission"),
        };
        assert_eq!(
            exit.commit(Arc::new(()), drop).outcome(),
            ExitOutcome::BecameZombie
        );

        assert_eq!(
            MAX_REPARENT_BATCH_OBSERVED.load(Ordering::Acquire),
            REPARENT_BATCH_LIMIT
        );
        assert!(children.iter().all(|child| {
            child
                .parent()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &init))
        }));
    }

    #[test]
    fn reparent_cursor_keeps_original_high_pid_under_concurrent_insertion() {
        let domain = ProcessDomain::<()>::try_new().unwrap();
        let init = domain.try_new_init(1, None).unwrap();
        for pid in [10, 30] {
            domain.prepare_fork(&init, pid, None).unwrap().commit();
        }
        let mut cursor = domain.registry.processes_through_current_max();
        assert_eq!(cursor.next().unwrap().pid(), 1);

        // A newly inserted PID after the cursor must not consume a fixed
        // remaining-item budget, and an insertion above the start maximum must
        // not extend exit work indefinitely.
        domain.prepare_fork(&init, 2, None).unwrap().commit();
        domain.prepare_fork(&init, 40, None).unwrap().commit();
        let seen: Vec<_> = cursor.map(|process| process.pid()).collect();
        assert_eq!(seen, [2, 10, 30]);
    }

    #[test]
    fn reparenting_is_scoped_and_never_mixes_handoff_reapers() {
        let domain = ProcessDomain::<()>::try_new().unwrap();
        let root = domain.try_new_init(1, None).unwrap();
        domain.prepare_thread(&root, 1).unwrap().commit().unwrap();
        root.set_child_subreaper(true);
        let parent = domain
            .prepare_fork(&root, 2, None)
            .unwrap()
            .prepare_initial_thread(2)
            .unwrap()
            .commit();
        let scope = domain.try_new_reaper_scope().unwrap();
        let scoped_init = domain
            .prepare_fork_as_reaper_scope_init_with_identity(&parent, &scope, 3, None, ())
            .unwrap()
            .prepare_initial_thread(3)
            .unwrap()
            .commit()
            .unwrap();
        let scoped_child = domain
            .prepare_fork_in_reaper_scope_with_identity(&parent, &scope, 4, None, ())
            .unwrap()
            .prepare_initial_thread(4)
            .unwrap()
            .commit();
        let root_child = domain
            .prepare_fork(&parent, 5, None)
            .unwrap()
            .prepare_initial_thread(5)
            .unwrap()
            .commit();

        let exit = match domain.exit_thread(&parent, 2, 0).unwrap() {
            ThreadExitTransition::FinalThread(exit) => exit,
            _ => panic!("parent must exit"),
        };
        exit.commit_with_reparent_handoff(
            Arc::new(()),
            |_| {},
            |batch| {
                for moved in batch.reparented() {
                    let expected = if Arc::ptr_eq(moved.child(), &scoped_child) {
                        &scoped_init
                    } else {
                        &root
                    };
                    assert!(Arc::ptr_eq(batch.reaper(), expected));
                }
            },
        );

        assert!(Arc::ptr_eq(&scoped_child.parent().unwrap(), &scoped_init));
        assert!(Arc::ptr_eq(&scoped_init.parent().unwrap(), &root));
        assert!(Arc::ptr_eq(&root_child.parent().unwrap(), &root));
    }
}
