use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

use intrusive_collections::{Bound, KeyAdapter, RBTree, RBTreeAtomicLink, intrusive_adapter};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use spin::Lazy;

use crate::{Pid, ProcessGroup, Session};

/// Maximum number of process and thread membership records admitted by this
/// process model. The task layer may impose lower rlimits before reaching it.
pub const PROCESS_MEMBERSHIP_LIMIT: usize = 65_536;

/// Failure returned by fallible process-lifecycle admission.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProcessError {
    /// Allocator capacity could not be reserved.
    NoMemory,
    /// The requested PID/TID is already live or already reserved.
    AlreadyExists,
    /// The bounded membership ceiling has been reached.
    Capacity,
}

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
}

impl ThreadGroup {
    fn new() -> Self {
        Self {
            threads: RBTree::new(ThreadAdapter::new()),
            memberships: 0,
            live_threads: 0,
            exit_code: 0,
            group_exited: false,
        }
    }
}

/// Durable CPU usage totals for a process subtree.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ProcessUsage {
    /// User CPU time in nanoseconds.
    pub utime_ns: u64,
    /// System CPU time in nanoseconds.
    pub stime_ns: u64,
    /// Maximum resident set size in kilobytes.
    pub maxrss_kb: u64,
}

impl ProcessUsage {
    /// Creates a new usage record.
    pub const fn new(utime_ns: u64, stime_ns: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb: 0,
        }
    }

    /// Creates a new usage record with memory high-water accounting.
    pub const fn with_maxrss(utime_ns: u64, stime_ns: u64, maxrss_kb: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb,
        }
    }

    /// Returns the sum of two usage records, saturating on overflow.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            utime_ns: self.utime_ns.saturating_add(other.utime_ns),
            stime_ns: self.stime_ns.saturating_add(other.stime_ns),
            maxrss_kb: self.maxrss_kb.max(other.maxrss_kb),
        }
    }
}

/// Immutable state that must survive after the process's runtime data is gone.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ZombieSnapshot {
    /// Linux wait status.
    pub wait_status: i32,
    /// CPU usage charged directly to the exited process.
    pub self_usage: ProcessUsage,
    /// CPU usage already accumulated from waited-for descendants.
    pub child_usage: ProcessUsage,
    /// Real UID of the exiting process.
    pub uid: u32,
}

impl ZombieSnapshot {
    /// Returns the total usage of the exited child subtree.
    pub fn total_usage(self) -> ProcessUsage {
        self.self_usage.saturating_add(self.child_usage)
    }
}

/// A process.
pub struct Process {
    registry_link: RBTreeAtomicLink,
    published: AtomicBool,
    pid: Pid,
    is_zombie: AtomicBool,
    reaped: AtomicBool,
    tg: SpinNoIrq<ThreadGroup>,
    exit_signal: Option<u8>,
    zombie_snapshot: SpinNoIrq<Option<ZombieSnapshot>>,
    child_subreaper: AtomicBool,
    parent: SpinNoIrq<Weak<Process>>,
    group: SpinNoIrq<Arc<ProcessGroup>>,
}

intrusive_adapter!(ProcessAdapter = Arc<Process>: Process { registry_link: RBTreeAtomicLink });

impl<'a> KeyAdapter<'a> for ProcessAdapter {
    type Key = Pid;

    fn get_key(&self, process: &'a Process) -> Self::Key {
        process.pid
    }
}

struct ProcessRegistry {
    entries: RBTree<ProcessAdapter>,
    memberships: usize,
}

impl ProcessRegistry {
    fn new() -> Self {
        Self {
            entries: RBTree::new(ProcessAdapter::new()),
            memberships: 0,
        }
    }
}

static PROCESS_REGISTRY: Lazy<SpinNoIrq<ProcessRegistry>> =
    Lazy::new(|| SpinNoIrq::new(ProcessRegistry::new()));

fn admit_process(process: Arc<Process>) -> Result<ProcessAdmission, ProcessError> {
    let mut registry = PROCESS_REGISTRY.lock();
    if registry.memberships >= PROCESS_MEMBERSHIP_LIMIT {
        return Err(ProcessError::Capacity);
    }
    if !registry.entries.find(&process.pid).is_null() {
        return Err(ProcessError::AlreadyExists);
    }
    registry.entries.insert(process.clone());
    registry.memberships += 1;
    drop(registry);
    Ok(ProcessAdmission {
        process,
        committed: false,
    })
}

fn process_membership_count() -> usize {
    PROCESS_REGISTRY.lock().memberships
}

/// Iterates published processes in PID order without allocating or running
/// caller code under the global registry lock. Each lock acquisition clones at
/// most one node, and the initial membership count bounds the whole walk.
pub(crate) struct Processes {
    last: Option<Arc<Process>>,
    after: Option<Pid>,
    remaining: usize,
    finished: bool,
}

pub(crate) fn processes() -> Processes {
    Processes {
        last: None,
        after: None,
        remaining: process_membership_count(),
        finished: false,
    }
}

impl Iterator for Processes {
    type Item = Arc<Process>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished && self.remaining != 0 {
            let registry = PROCESS_REGISTRY.lock();
            let next = if let Some(last) = self
                .last
                .as_ref()
                .filter(|last| last.registry_link.is_linked())
            {
                let mut cursor = unsafe { registry.entries.cursor_from_ptr(Arc::as_ptr(last)) };
                cursor.move_next();
                cursor.clone_pointer()
            } else if let Some(after) = self.after.as_ref() {
                registry
                    .entries
                    .lower_bound(Bound::Excluded(after))
                    .clone_pointer()
            } else {
                registry.entries.front().clone_pointer()
            };
            drop(registry);

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

/// Fallibly collects values derived from a single pass over published
/// processes. Capacity growth is bounded geometrically by the explicit global
/// membership ceiling, so concurrent fork churn cannot cause an unbounded
/// retry loop.
pub(crate) fn try_collect_process_values<T>(
    mut map: impl FnMut(&Arc<Process>) -> Option<T>,
) -> Result<Vec<T>, ProcessError> {
    let mut snapshot = Vec::new();
    loop {
        snapshot.clear();
        let required = process_membership_count();
        if snapshot.capacity() < required {
            snapshot
                .try_reserve_exact(required)
                .map_err(|_| ProcessError::NoMemory)?;
        }

        let mut overflow = false;
        for process in processes() {
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
        if current >= PROCESS_MEMBERSHIP_LIMIT {
            return Err(ProcessError::Capacity);
        }
        let target = current
            .max(1)
            .saturating_mul(2)
            .min(PROCESS_MEMBERSHIP_LIMIT);
        snapshot.clear();
        snapshot
            .try_reserve_exact(target)
            .map_err(|_| ProcessError::NoMemory)?;
    }
}

impl Process {
    /// The [`Process`] ID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Returns `true` if the [`Process`] is the init process.
    pub fn is_init(self: &Arc<Self>) -> bool {
        INIT_PROC.get().is_some_and(|init| Arc::ptr_eq(self, init))
    }

    /// Returns the signal delivered to the parent when this process exits.
    pub fn exit_signal(&self) -> Option<u8> {
        self.exit_signal
    }

    /// Returns the published zombie snapshot, if any.
    pub fn zombie_snapshot(&self) -> Option<ZombieSnapshot> {
        *self.zombie_snapshot.lock()
    }

    /// Publishes the durable zombie snapshot for this process.
    pub fn publish_zombie_snapshot(&self, snapshot: ZombieSnapshot) {
        *self.zombie_snapshot.lock() = Some(snapshot);
    }

    /// Returns whether this process acts as a child subreaper.
    pub fn is_child_subreaper(&self) -> bool {
        self.child_subreaper.load(Ordering::Acquire)
    }

    /// Configures child subreaper state for this process.
    pub fn set_child_subreaper(&self, enabled: bool) {
        self.child_subreaper.store(enabled, Ordering::Release);
    }

    /// The parent [`Process`].
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }

    /// Fallibly snapshots this process's children.
    pub fn try_children(self: &Arc<Self>) -> Result<Vec<Arc<Process>>, ProcessError> {
        try_collect_process_values(|child| {
            child
                .parent()
                .is_some_and(|parent| Arc::ptr_eq(&parent, self))
                .then(|| child.clone())
        })
    }

    /// The [`ProcessGroup`] that this process belongs to.
    pub fn group(&self) -> Arc<ProcessGroup> {
        self.group.lock().clone()
    }

    fn set_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) {
        let old = core::mem::replace(&mut *self.group.lock(), group.clone());
        drop(old);
    }

    /// Fallibly creates a new session/group and moves this process into it.
    pub fn try_create_session(
        self: &Arc<Self>,
    ) -> Result<Option<(Arc<Session>, Arc<ProcessGroup>)>, ProcessError> {
        if self.group().session.sid() == self.pid {
            return Ok(None);
        }
        let session = Session::try_new(self.pid)?;
        let group = ProcessGroup::try_new(self.pid, &session)?;
        self.set_group(&group);
        Ok(Some((session, group)))
    }

    /// Fallibly creates a new process group and moves this process into it.
    pub fn try_create_group(self: &Arc<Self>) -> Result<Option<Arc<ProcessGroup>>, ProcessError> {
        let old_group = self.group();
        if old_group.pgid() == self.pid {
            return Ok(None);
        }
        let group = ProcessGroup::try_new(self.pid, &old_group.session)?;
        self.set_group(&group);
        Ok(Some(group))
    }

    /// Moves this process to a group in the same session.
    pub fn move_to_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) -> bool {
        let old_group = self.group();
        if Arc::ptr_eq(&old_group, group) {
            return true;
        }
        if !Arc::ptr_eq(&old_group.session, &group.session) {
            return false;
        }
        self.set_group(group);
        true
    }

    /// Reserves capacity for a thread without publishing its TID.
    pub fn prepare_thread(self: &Arc<Self>, tid: Pid) -> Result<ThreadAdmission, ProcessError> {
        let node = Arc::try_new(ThreadNode {
            link: RBTreeAtomicLink::new(),
            tid,
            live: AtomicBool::new(false),
        })
        .map_err(|_| ProcessError::NoMemory)?;
        let mut tg = self.tg.lock();
        if tg.memberships >= PROCESS_MEMBERSHIP_LIMIT {
            return Err(ProcessError::Capacity);
        }
        if !tg.threads.find(&tid).is_null() {
            return Err(ProcessError::AlreadyExists);
        }
        tg.threads.insert(node.clone());
        tg.memberships += 1;
        drop(tg);
        Ok(ThreadAdmission {
            process: self.clone(),
            node,
            committed: false,
        })
    }

    fn detach_thread_locked(
        tg: &mut ThreadGroup,
        node: *const ThreadNode,
    ) -> Option<Arc<ThreadNode>> {
        let removed = unsafe { tg.threads.cursor_mut_from_ptr(node).remove() };
        if let Some(node) = removed.as_ref() {
            tg.memberships -= 1;
            if node.live.swap(false, Ordering::Relaxed) {
                tg.live_threads -= 1;
            }
        }
        removed
    }

    /// Removes a thread without updating the exit state.
    pub fn remove_thread(&self, tid: Pid) {
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
        drop(removed);
    }

    /// Removes a thread and returns whether it was the final live thread.
    pub fn exit_thread(self: &Arc<Self>, tid: Pid, exit_code: i32) -> bool {
        let mut tg = self.tg.lock();
        if !tg.group_exited {
            tg.exit_code = exit_code;
        }
        let node = tg.threads.find(&tid).get().and_then(|thread| {
            thread
                .live
                .load(Ordering::Relaxed)
                .then_some(thread as *const ThreadNode)
        });
        let removed = node.and_then(|node| Self::detach_thread_locked(&mut tg, node));
        let empty = tg.live_threads == 0;
        drop(tg);
        drop(removed);
        empty
    }

    /// Returns the number of threads without allocating.
    pub fn thread_count(&self) -> usize {
        self.tg.lock().live_threads
    }

    /// Returns whether `tid` is the only thread in this process.
    pub fn has_only_thread(&self, tid: Pid) -> bool {
        let tg = self.tg.lock();
        tg.live_threads == 1
            && tg
                .threads
                .find(&tid)
                .get()
                .is_some_and(|thread| thread.live.load(Ordering::Relaxed))
    }

    /// Fallibly snapshots all thread IDs.
    pub fn try_threads(self: &Arc<Self>) -> Result<Vec<Pid>, ProcessError> {
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
            if current >= PROCESS_MEMBERSHIP_LIMIT {
                return Err(ProcessError::Capacity);
            }
            let target = current
                .max(1)
                .saturating_mul(2)
                .min(PROCESS_MEMBERSHIP_LIMIT);
            snapshot.clear();
            snapshot
                .try_reserve_exact(target)
                .map_err(|_| ProcessError::NoMemory)?;
        }
    }

    /// Iterates thread IDs without allocating or holding the thread-group lock
    /// across caller code.
    pub fn thread_ids(self: &Arc<Self>) -> ThreadIds {
        ThreadIds {
            process: self.clone(),
            last: None,
            after: None,
            remaining: self.tg.lock().memberships,
            finished: false,
        }
    }

    /// Visits every thread ID without allocating.
    pub fn for_each_thread(self: &Arc<Self>, mut visitor: impl FnMut(Pid)) {
        for tid in self.thread_ids() {
            visitor(tid);
        }
    }

    /// Returns whether this process is non-zombie and has a live thread.
    pub fn is_live(&self) -> bool {
        !self.is_zombie() && self.thread_count() != 0
    }

    /// Returns whether this process has group-exited.
    pub fn is_group_exited(&self) -> bool {
        self.tg.lock().group_exited
    }

    /// Marks this process as group-exited.
    pub fn group_exit(&self) {
        self.tg.lock().group_exited = true;
    }

    /// Returns the process exit code.
    pub fn exit_code(&self) -> i32 {
        self.tg.lock().exit_code
    }

    fn reaper_for_exit(self: &Arc<Self>) -> Arc<Process> {
        let init = INIT_PROC.get().expect("init process not initialized");
        let mut ancestor = self.parent();
        while let Some(process) = ancestor {
            if process.is_child_subreaper() && !process.is_zombie() {
                return process;
            }
            ancestor = process.parent();
        }
        init.clone()
    }

    /// Returns whether this is a zombie process.
    pub fn is_zombie(&self) -> bool {
        self.is_zombie.load(Ordering::Acquire)
    }

    /// Marks the process as a zombie and allocation-freely reparents children.
    ///
    /// `inherited_zombie` runs after the membership lock is released for each
    /// already-zombie child moved to the new reaper.
    pub fn exit(self: &Arc<Self>, mut inherited_zombie: impl FnMut(Arc<Process>)) {
        if self.is_init() {
            return;
        }
        let reaper = self.reaper_for_exit();
        let reaper_weak = Arc::downgrade(&reaper);
        self.is_zombie.store(true, Ordering::Release);

        for child in processes().filter(|child| {
            child
                .parent()
                .is_some_and(|parent| Arc::ptr_eq(&parent, self))
        }) {
            let old_parent = core::mem::replace(&mut *child.parent.lock(), reaper_weak.clone());
            drop(old_parent);
            if child.is_zombie() {
                inherited_zombie(child);
            }
        }
    }

    /// Reaps a zombie process, returning false for invalid or duplicate reap.
    pub fn reap(&self) -> bool {
        if !self.is_zombie() || self.reaped.swap(true, Ordering::AcqRel) {
            return false;
        }
        let removed = {
            let mut registry = PROCESS_REGISTRY.lock();
            if !self.registry_link.is_linked() {
                return false;
            }
            unsafe {
                let removed = registry
                    .entries
                    .cursor_mut_from_ptr(self as *const Process)
                    .remove();
                if removed.is_some() {
                    registry.memberships -= 1;
                }
                removed
            }
        };
        let existed = removed.is_some();
        drop(removed);
        existed
    }

    fn try_allocate(
        pid: Pid,
        parent: Option<&Arc<Process>>,
        group: Arc<ProcessGroup>,
        exit_signal: Option<u8>,
    ) -> Result<Arc<Self>, ProcessError> {
        Arc::try_new(Self {
            registry_link: RBTreeAtomicLink::new(),
            published: AtomicBool::new(false),
            pid,
            is_zombie: AtomicBool::new(false),
            reaped: AtomicBool::new(false),
            tg: SpinNoIrq::new(ThreadGroup::new()),
            exit_signal,
            zombie_snapshot: SpinNoIrq::new(None),
            child_subreaper: AtomicBool::new(false),
            parent: SpinNoIrq::new(parent.map(Arc::downgrade).unwrap_or_default()),
            group: SpinNoIrq::new(group),
        })
        .map_err(|_| ProcessError::NoMemory)
    }

    /// Fallibly creates and publishes the unique init process.
    pub fn try_new_init(pid: Pid, exit_signal: Option<u8>) -> Result<Arc<Self>, ProcessError> {
        let session = Session::try_new(pid)?;
        let group = ProcessGroup::try_new(pid, &session)?;
        let process = Self::try_allocate(pid, None, group, exit_signal)?;
        let admission = admit_process(process.clone())?;
        admission.commit();
        INIT_PROC.init_once(process.clone());
        Ok(process)
    }

    /// Fallibly prepares an unpublished child process and its global capacity credit.
    pub fn prepare_fork(
        self: &Arc<Self>,
        pid: Pid,
        exit_signal: Option<u8>,
    ) -> Result<ProcessAdmission, ProcessError> {
        let process = Self::try_allocate(pid, Some(self), self.group(), exit_signal)?;
        admit_process(process)
    }
}

impl fmt::Debug for Process {
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

/// Reserved, fully allocated child process awaiting final publication.
pub struct ProcessAdmission {
    process: Arc<Process>,
    committed: bool,
}

impl ProcessAdmission {
    /// Returns the unpublished process object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Publishes the process against its reserved global membership slot.
    pub fn commit(mut self) {
        self.process.published.store(true, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for ProcessAdmission {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let removed = {
            let mut registry = PROCESS_REGISTRY.lock();
            if !self.process.registry_link.is_linked() {
                None
            } else {
                unsafe {
                    let removed = registry
                        .entries
                        .cursor_mut_from_ptr(Arc::as_ptr(&self.process))
                        .remove();
                    if removed.is_some() {
                        registry.memberships -= 1;
                    }
                    removed
                }
            }
        };
        drop(removed);
    }
}

/// Reserved thread-group membership awaiting final publication.
pub struct ThreadAdmission {
    process: Arc<Process>,
    node: Arc<ThreadNode>,
    committed: bool,
}

/// Allocation-free PID-ordered iterator over a process's thread IDs.
///
/// Each thread-group lock acquisition clones at most one intrusive node. The
/// initial membership count bounds the walk even if concurrent clone/exit
/// activity keeps replacing nodes with larger TIDs.
pub struct ThreadIds {
    process: Arc<Process>,
    last: Option<Arc<ThreadNode>>,
    after: Option<Pid>,
    remaining: usize,
    finished: bool,
}

impl Iterator for ThreadIds {
    type Item = Pid;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished && self.remaining != 0 {
            let tg = self.process.tg.lock();
            let next = if let Some(last) = self.last.as_ref().filter(|last| last.link.is_linked()) {
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

impl ThreadAdmission {
    /// Marks this already-linked membership live without consuming the token.
    /// Callers can therefore release higher-level lifecycle locks before the
    /// token's Arc references are dropped.
    pub fn publish(&mut self) {
        let mut tg = self.process.tg.lock();
        if !self.node.live.swap(true, Ordering::Relaxed) {
            tg.live_threads += 1;
        }
        drop(tg);
        self.committed = true;
    }

    /// Publishes the TID against its reserved process membership capacity.
    pub fn commit(mut self) {
        self.publish();
    }
}

impl Drop for ThreadAdmission {
    fn drop(&mut self) {
        if !self.committed {
            let removed = {
                let mut tg = self.process.tg.lock();
                if !self.node.link.is_linked() {
                    None
                } else {
                    Process::detach_thread_locked(&mut tg, Arc::as_ptr(&self.node))
                }
            };
            drop(removed);
        }
    }
}

static INIT_PROC: LazyInit<Arc<Process>> = LazyInit::new();

/// Gets the init process.
pub fn init_proc() -> Arc<Process> {
    INIT_PROC
        .get()
        .expect("init process not initialized")
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admissions_publish_only_at_commit_and_iteration_is_lock_free_for_visitor() {
        let init = Process::try_new_init(1, None).unwrap();
        init.prepare_thread(1).unwrap().commit();
        init.prepare_thread(11).unwrap().commit();

        assert_eq!(init.thread_count(), 2);
        assert!(!init.has_only_thread(1));
        let mut tids = init.try_threads().unwrap();
        tids.sort_unstable();
        assert_eq!(tids, [1, 11]);

        init.remove_thread(11);
        assert!(init.has_only_thread(1));

        let child_admission = init.prepare_fork(2, None).unwrap();
        let child = child_admission.process().clone();
        child.prepare_thread(2).unwrap().commit();
        assert!(init.try_children().unwrap().is_empty());
        child_admission.commit();

        let sibling_admission = init.prepare_fork(3, None).unwrap();
        let sibling = sibling_admission.process().clone();
        sibling.prepare_thread(3).unwrap().commit();
        sibling_admission.commit();

        let group = init.group();
        assert!(group.any_process(|process| process.pid() == child.pid()));
        let mut visited = Vec::new();
        group.for_each_process(|process| visited.push(process.pid()));
        visited.sort_unstable();
        assert_eq!(visited, [1, 2, 3]);

        let admission = init.prepare_thread(99).unwrap();
        assert!(!init.try_threads().unwrap().contains(&99));
        drop(admission);
        assert!(!init.try_threads().unwrap().contains(&99));
        init.prepare_thread(99).unwrap().commit();
        assert!(init.try_threads().unwrap().contains(&99));
        init.remove_thread(99);

        let walker_admission = init.prepare_fork(4, None).unwrap();
        let walker = walker_admission.process().clone();
        walker.prepare_thread(40).unwrap().commit();
        walker.prepare_thread(4).unwrap().commit();
        walker.prepare_thread(20).unwrap().commit();
        walker_admission.commit();
        let mut tids = walker.thread_ids();
        assert_eq!(tids.next(), Some(4));
        walker.remove_thread(4);
        assert_eq!(tids.collect::<Vec<_>>(), [20, 40]);

        let rolled_back = init.prepare_fork(5, None).unwrap();
        drop(rolled_back);
        init.prepare_fork(5, None).unwrap().commit();
        assert_eq!(
            processes().map(|process| process.pid()).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );

        let mut process_walk = processes();
        assert_eq!(process_walk.next().map(|process| process.pid()), Some(1));
        assert_eq!(process_walk.next().map(|process| process.pid()), Some(2));
        assert_eq!(process_walk.next().map(|process| process.pid()), Some(3));
        assert_eq!(process_walk.next().map(|process| process.pid()), Some(4));
        walker.exit(drop);
        assert!(walker.reap());
        assert_eq!(process_walk.next().map(|process| process.pid()), Some(5));
        assert!(process_walk.next().is_none());
    }
}
