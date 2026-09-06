use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    cell::RefCell,
    ops::Deref,
    sync::atomic::{
        AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
    },
};

use axcpu::ioport::{self, IO_BITMAP_BYTES};
use axerrno::{AxError, AxResult};
use axfs::FsContext;
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::{
    SchedState, SwitchReason, TaskExt, TaskInner, UclampRequest, UtilizationBounds, current,
};
use extern_trait::extern_trait;
use scope_local::{ActiveScope, Scope};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_rseq::ThreadRseq;
use thekernel_linux_seccomp::SeccompState;
use thekernel_linux_signal::{
    SignalSet, SignalStack,
    api::{ThreadRegistrationError, ThreadSignalManager, ThreadSignalRegistration},
};

use super::{
    NamespaceProxy, ProcessData,
    accounting::{AtomicTaskUsage, TaskUsage},
    creds::{Cred, CredentialSlot, CredentialSnapshotGuard},
    restart::RestartTracker,
    seccomp::ThreadSeccompSlot,
    security::LandlockDomain,
    timer::{TimeManager, request_process_cpu_evaluation},
};
use crate::{deferred_work::DeferredWorkAccount, file::OpenCredentials};

const TASK_PARENT_RELATION_HARD_LIMIT: usize =
    thekernel_linux_process_adapter::PROCESS_MEMBERSHIP_LIMIT;
static LIVE_TASK_PARENT_RELATIONS: AtomicUsize = AtomicUsize::new(0);
static TASK_PARENT_TOPOLOGY: SpinNoIrq<()> = SpinNoIrq::new(());

/// Linux x86 I/O-port state. It is deliberately task-local: threads in one
/// process may grant different port ranges. The grant bitmap is shared after
/// fork, while the inline revoke overlay guarantees that removing a permission
/// can never need an allocation.
#[derive(Clone)]
pub(crate) struct IoPortState {
    bitmap: Option<Arc<[u8; IO_BITMAP_BYTES]>>,
    /// Bits explicitly revoked from a shared inherited grant bitmap. This is
    /// embedded in the thread state so ioperm(..., 0) remains infallible under
    /// memory pressure.
    revoked: [u8; IO_BITMAP_BYTES],
    iopl: u8,
}

/// Coherent view for operations which interpret credentials and pathname
/// state through the task's namespace proxy.  The publication gate is shared
/// with setns/unshare and fs_struct replacement, so a caller never combines a
/// pre-transition credential with a post-transition mount namespace.
#[derive(Clone)]
pub(crate) struct NamespaceCredentialFsSnapshot {
    pub(crate) namespaces: NamespaceProxy,
    /// The mount topology is pathname authority. Deferred work retains the
    /// submitter's idmap view instead of sampling a worker's namespace.
    pub(crate) mount_topology: Arc<crate::mounts::MountTopology>,
    pub(crate) credential: Arc<Cred>,
    pub(crate) fs_slot: Arc<FsContextSlot>,
    pub(crate) fs_context: Arc<Mutex<FsContext>>,
    /// Immutable root/cwd/umask sampled with the namespace publication gate.
    /// Deferred io_uring pathname work must not observe a later chdir/chroot.
    pub(crate) fs_snapshot: FsContext,
    pub(crate) landlock_domain: LandlockDomain,
    /// The caller's controlling terminal is process-session state, not
    /// namespace state.  Retain it with the other execution authority so a
    /// kernel worker cannot accidentally resolve `/dev/tty` against itself.
    pub(crate) controlling_terminal: Option<Arc<dyn Any + Send + Sync>>,
}

impl Default for IoPortState {
    fn default() -> Self {
        Self {
            bitmap: None,
            revoked: [0; IO_BITMAP_BYTES],
            iopl: 0,
        }
    }
}

impl IoPortState {
    fn update_range(bitmap: &mut [u8; IO_BITMAP_BYTES], from: usize, num: usize, turn_on: bool) {
        let end = from + num;
        let first = from / 8;
        let last = (end - 1) / 8;
        let first_mask = 0xffu8 << (from % 8);
        let last_mask = ((1u16 << ((end - 1) % 8 + 1)) - 1) as u8;
        let update = |byte: &mut u8, mask: u8| {
            if turn_on {
                *byte &= !mask;
            } else {
                *byte |= mask;
            }
        };
        if first == last {
            update(&mut bitmap[first], first_mask & last_mask);
            return;
        }
        update(&mut bitmap[first], first_mask);
        bitmap[first + 1..last].fill(if turn_on { 0 } else { 0xff });
        update(&mut bitmap[last], last_mask);
    }

    fn try_update_ioperm(&mut self, from: usize, num: usize, turn_on: bool) -> AxResult<()> {
        self.try_update_ioperm_with(from, num, turn_on, |bitmap| {
            Arc::try_new(bitmap).map_err(|_| AxError::NoMemory)
        })
    }

    fn try_update_ioperm_with<F>(
        &mut self,
        from: usize,
        num: usize,
        turn_on: bool,
        allocate: F,
    ) -> AxResult<()>
    where
        F: FnOnce([u8; IO_BITMAP_BYTES]) -> AxResult<Arc<[u8; IO_BITMAP_BYTES]>>,
    {
        if !turn_on {
            if self.bitmap.is_none() {
                return Ok(());
            }
            // Do not COW a shared inherited bitmap to revoke access. The
            // preallocated overlay is private to this thread and takes
            // precedence when the TSS image is installed.
            Self::update_range(&mut self.revoked, from, num, false);
            if self.revoked.iter().all(|&byte| byte == 0xff) {
                self.bitmap = None;
                self.revoked.fill(0);
            }
            return Ok(());
        }

        if let Some(shared) = self.bitmap.as_mut()
            && let Some(bitmap) = Arc::get_mut(shared)
        {
            Self::update_range(bitmap, from, num, true);
            Self::update_range(&mut self.revoked, from, num, true);
            return Ok(());
        }

        let mut bitmap = match self.bitmap.as_ref() {
            Some(bitmap) => **bitmap,
            None => [0xff; IO_BITMAP_BYTES],
        };
        // A grant must observe prior local revocations before it replaces the
        // shared base, then clear precisely the requested range.
        for (byte, revoked) in bitmap.iter_mut().zip(self.revoked.iter()) {
            *byte |= *revoked;
        }
        Self::update_range(&mut bitmap, from, num, true);
        let bitmap = allocate(bitmap)?;
        self.bitmap = Some(bitmap);
        self.revoked.fill(0);
        Ok(())
    }

    fn bitmap_and_revocations(
        &self,
    ) -> (
        Option<&[u8; IO_BITMAP_BYTES]>,
        Option<&[u8; IO_BITMAP_BYTES]>,
    ) {
        let bitmap = self.bitmap.as_deref();
        let revoked = (!self.revoked.iter().all(|&byte| byte == 0)).then_some(&self.revoked);
        (bitmap, revoked)
    }
}

#[cfg(not(test))]
type TaskParentPublicationMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type TaskParentPublicationMutex<T> = spin::Mutex<T>;
#[cfg(not(test))]
type TaskParentPublicationMutexGuard<'a, T> = axsync::MutexGuard<'a, T>;
#[cfg(test)]
type TaskParentPublicationMutexGuard<'a, T> = spin::MutexGuard<'a, T>;

static TASK_PARENT_PUBLICATION: TaskParentPublicationMutex<()> =
    TaskParentPublicationMutex::new(());

/// Sleepable outer gate for exact task-parent observation and publication.
///
/// Process lifecycle locks precede this guard. Ptrace action and credential
/// gates follow it. The short [`TASK_PARENT_TOPOLOGY`] spin guard may only be
/// acquired while this guard is already held.
pub(crate) struct TaskParentPublicationGuard<'a> {
    _guard: TaskParentPublicationMutexGuard<'a, ()>,
}

pub(crate) fn lock_task_parent_publication() -> TaskParentPublicationGuard<'static> {
    TaskParentPublicationGuard {
        _guard: TASK_PARENT_PUBLICATION.lock(),
    }
}

fn try_reserve_task_parent_relation(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1).filter(|next| *next <= limit)
        })
        .is_ok()
}

struct TaskParentState {
    live: bool,
    /// Stable next hop selected when this exact task exits. Keeping the hop
    /// on the dead node lets clone publication and parent snapshots traverse
    /// a bounded reparent chain while exit processes children in short
    /// topology sections.
    exit_reaper: Option<Arc<TaskParentNode>>,
    parent: Option<Arc<TaskParentNode>>,
    first_child: Option<Weak<TaskParentNode>>,
    previous_sibling: Option<Weak<TaskParentNode>>,
    next_sibling: Option<Weak<TaskParentNode>>,
}

impl TaskParentState {
    const fn new() -> Self {
        Self {
            live: true,
            exit_reaper: None,
            parent: None,
            first_child: None,
            previous_sibling: None,
            next_sibling: None,
        }
    }
}

/// Immutable, allocation-owned identity for one Linux task's exact
/// `real_parent` relation. Numeric TID reuse cannot alias this object.
pub(crate) struct TaskParentNode {
    kernel_tid: Pid,
    pdeath_signal: AtomicU32,
    process: Weak<ProcessData>,
    credential: Weak<CredentialSlot>,
    state: SpinNoIrq<TaskParentState>,
}

impl TaskParentNode {
    fn try_new(
        kernel_tid: Pid,
        process: Weak<ProcessData>,
        credential: Weak<CredentialSlot>,
    ) -> AxResult<Arc<Self>> {
        if !try_reserve_task_parent_relation(
            &LIVE_TASK_PARENT_RELATIONS,
            TASK_PARENT_RELATION_HARD_LIMIT,
        ) {
            return Err(AxError::NoMemory);
        }
        match Arc::try_new(Self {
            kernel_tid,
            pdeath_signal: AtomicU32::new(0),
            process,
            credential,
            state: SpinNoIrq::new(TaskParentState::new()),
        }) {
            Ok(node) => Ok(node),
            Err(_) => {
                LIVE_TASK_PARENT_RELATIONS.fetch_sub(1, Ordering::AcqRel);
                Err(AxError::NoMemory)
            }
        }
    }

    pub(crate) const fn kernel_tid(&self) -> Pid {
        self.kernel_tid
    }

    pub(crate) fn process_data(&self) -> Option<Arc<ProcessData>> {
        self.process.upgrade()
    }
}

impl Drop for TaskParentNode {
    fn drop(&mut self) {
        LIVE_TASK_PARENT_RELATIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Exact parent selection used by clone publication.
pub(crate) enum TaskParentChoice {
    Caller(Arc<TaskParentNode>),
    Inherit(Arc<TaskParentNode>),
}

/// Stable real-parent identity retained across credential sampling and later
/// publication revalidation (for example `PTRACE_TRACEME`).
pub(crate) struct TaskParentSnapshot {
    parent: Arc<TaskParentNode>,
    credential_slot: Arc<CredentialSlot>,
    credential: Arc<Cred>,
}

/// Outcome of a nonblocking exact-parent credential pin under the graph gate.
pub(crate) enum TaskParentCredentialPin<'a> {
    /// The same relation and immutable credential are pinned for publication.
    Pinned(CredentialSnapshotGuard<'a>),
    /// A credential writer owns the slot; callers must release outer gates,
    /// yield, and retry final revalidation.
    Busy,
    /// The exact relation, slot, or immutable credential no longer matches.
    Stale,
}

impl TaskParentSnapshot {
    pub(crate) fn kernel_tid(&self) -> Pid {
        self.parent.kernel_tid()
    }

    pub(crate) fn parent_node(&self) -> &Arc<TaskParentNode> {
        &self.parent
    }

    /// Immutable exact-parent credential sampled while this parent relation
    /// was current. Authorization may use this snapshot and then revalidate
    /// the node identity before publishing a relationship.
    pub(crate) fn credential(&self) -> &Arc<Cred> {
        &self.credential
    }
}

#[derive(Default)]
struct RetiredTaskParentLinks {
    _parent: Option<Arc<TaskParentNode>>,
    _previous: Option<Weak<TaskParentNode>>,
    _next: Option<Weak<TaskParentNode>>,
    _previous_node: Option<Arc<TaskParentNode>>,
    _next_node: Option<Arc<TaskParentNode>>,
    _displaced_forward: Option<Weak<TaskParentNode>>,
    _displaced_backward: Option<Weak<TaskParentNode>>,
}

fn unlink_task_parent_locked(child: &Arc<TaskParentNode>) -> RetiredTaskParentLinks {
    let (parent, previous, next) = {
        let mut child_state = child.state.lock();
        (
            child_state.parent.take(),
            child_state.previous_sibling.take(),
            child_state.next_sibling.take(),
        )
    };
    let previous_node = previous.as_ref().and_then(Weak::upgrade);
    let next_node = next.as_ref().and_then(Weak::upgrade);
    let displaced_forward = if let Some(previous_node) = previous_node.as_ref() {
        core::mem::replace(
            &mut previous_node.state.lock().next_sibling,
            next.as_ref().map(Weak::clone),
        )
    } else if let Some(parent) = parent.as_ref() {
        core::mem::replace(
            &mut parent.state.lock().first_child,
            next.as_ref().map(Weak::clone),
        )
    } else {
        None
    };
    let displaced_backward = next_node.as_ref().and_then(|next_node| {
        core::mem::replace(
            &mut next_node.state.lock().previous_sibling,
            previous.as_ref().map(Weak::clone),
        )
    });
    RetiredTaskParentLinks {
        _parent: parent,
        _previous: previous,
        _next: next,
        _previous_node: previous_node,
        _next_node: next_node,
        _displaced_forward: displaced_forward,
        _displaced_backward: displaced_backward,
    }
}

fn link_task_parent_locked(
    child: &Arc<TaskParentNode>,
    parent: &Arc<TaskParentNode>,
) -> RetiredTaskParentLinks {
    let old_head = parent.state.lock().first_child.take();
    let old_head_node = old_head.as_ref().and_then(Weak::upgrade);
    let displaced_backward = old_head_node.as_ref().and_then(|old_head| {
        old_head
            .state
            .lock()
            .previous_sibling
            .replace(Arc::downgrade(child))
    });
    let (old_parent, old_previous, old_next) = {
        let mut child_state = child.state.lock();
        (
            child_state.parent.replace(parent.clone()),
            child_state.previous_sibling.take(),
            core::mem::replace(&mut child_state.next_sibling, old_head),
        )
    };
    let displaced_forward = parent
        .state
        .lock()
        .first_child
        .replace(Arc::downgrade(child));
    RetiredTaskParentLinks {
        _parent: old_parent,
        _previous: old_previous,
        _next: old_next,
        _previous_node: None,
        _next_node: old_head_node,
        _displaced_forward: displaced_forward,
        _displaced_backward: displaced_backward,
    }
}

/// Resolves a live exact-task reaper without retaining or destroying an
/// `Arc` while the topology spinlock is held. Every hop is an immutable task
/// identity, so numeric TID reuse cannot redirect the traversal.
fn resolve_live_task_parent(
    _publication: &TaskParentPublicationGuard<'_>,
    mut candidate: Option<Arc<TaskParentNode>>,
    fallback: Option<&Arc<TaskParentNode>>,
) -> Option<Arc<TaskParentNode>> {
    let mut used_fallback = false;
    let mut remaining = TASK_PARENT_RELATION_HARD_LIMIT.saturating_add(1);
    loop {
        let Some(current) = candidate else {
            if used_fallback {
                return None;
            }
            used_fallback = true;
            candidate = fallback.cloned();
            continue;
        };
        if remaining == 0 {
            drop(current);
            panic!("cycle in exact task-parent reaper chain");
        }
        remaining -= 1;

        let (live, next) = {
            let topology = TASK_PARENT_TOPOLOGY.lock();
            let state = current.state.lock();
            let live = state.live;
            let next = if live {
                None
            } else {
                state.exit_reaper.clone()
            };
            drop(state);
            drop(topology);
            (live, next)
        };
        if live {
            return Some(current);
        }
        drop(current);
        candidate = next;
    }
}

/// Publishes one private task's exact Linux real-parent relation. Node
/// allocation and the global hard-limit charge happened in `Thread::try_new`;
/// this final publication is allocation-free and has no recoverable failure.
fn publish_task_parent_relation(
    publication: &TaskParentPublicationGuard<'_>,
    child: &Arc<TaskParentNode>,
    choice: TaskParentChoice,
) {
    {
        let topology = TASK_PARENT_TOPOLOGY.lock();
        let child_state = child.state.lock();
        let publishable = child_state.live && child_state.parent.is_none();
        drop(child_state);
        drop(topology);
        assert!(publishable, "exact task-parent relation published twice");
    }
    let candidate = match choice {
        TaskParentChoice::Caller(parent) => Some(parent),
        TaskParentChoice::Inherit(caller) => {
            let candidate = {
                let topology = TASK_PARENT_TOPOLOGY.lock();
                let state = caller.state.lock();
                let candidate = if state.live {
                    state.parent.clone()
                } else {
                    state.exit_reaper.clone()
                };
                drop(state);
                drop(topology);
                candidate
            };
            drop(caller);
            candidate
        }
    };
    let mut candidate = resolve_live_task_parent(publication, candidate, None);

    loop {
        let Some(parent) = candidate else {
            // The domain root intentionally has no exact parent. Inheriting
            // that relation for CLONE_THREAD/CLONE_PARENT is valid and must
            // not manufacture a self-Arc cycle.
            return;
        };
        let topology = TASK_PARENT_TOPOLOGY.lock();
        let parent_state = parent.state.lock();
        if !parent_state.live {
            let next = parent_state.exit_reaper.clone();
            drop(parent_state);
            drop(topology);
            drop(parent);
            candidate = resolve_live_task_parent(publication, next, None);
            continue;
        }
        drop(parent_state);

        let child_state = child.state.lock();
        let publishable = child_state.live && child_state.parent.is_none();
        drop(child_state);
        if !publishable {
            drop(topology);
            drop(parent);
            panic!("exact task-parent relation published twice");
        }
        let retired = link_task_parent_locked(child, &parent);
        drop(topology);
        drop(retired);
        drop(parent);
        return;
    }
}

fn task_parent_node_snapshot(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
) -> Option<Arc<TaskParentNode>> {
    let candidate = {
        let topology = TASK_PARENT_TOPOLOGY.lock();
        let state = node.state.lock();
        let candidate = state.live.then(|| state.parent.clone()).flatten();
        drop(state);
        drop(topology);
        candidate
    };
    resolve_live_task_parent(publication, candidate, None)
}

fn task_parent_node_matches(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
    parent: &Arc<TaskParentNode>,
) -> bool {
    task_parent_node_snapshot(publication, node)
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, parent))
}

fn task_parent_security_snapshot(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
) -> Option<TaskParentSnapshot> {
    let parent = task_parent_node_snapshot(publication, node)?;
    let slot = parent.credential.upgrade()?;
    let credential = slot.current();
    let snapshot = TaskParentSnapshot {
        parent,
        credential_slot: slot,
        credential,
    };
    task_parent_security_snapshot_matches(publication, node, &snapshot).then_some(snapshot)
}

fn task_parent_security_snapshot_matches(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
    snapshot: &TaskParentSnapshot,
) -> bool {
    if !task_parent_node_matches(publication, node, &snapshot.parent) {
        return false;
    }
    let Some(slot) = snapshot.parent.credential.upgrade() else {
        return false;
    };
    if !Arc::ptr_eq(&slot, &snapshot.credential_slot) {
        return false;
    }
    let credential = slot.current();
    let credential_matches = Arc::ptr_eq(&credential, &snapshot.credential);
    drop(credential);
    drop(slot);
    credential_matches && task_parent_node_matches(publication, node, &snapshot.parent)
}

fn try_lock_task_parent_security_snapshot<'a>(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
    snapshot: &'a TaskParentSnapshot,
) -> TaskParentCredentialPin<'a> {
    if !task_parent_node_matches(publication, node, &snapshot.parent) {
        return TaskParentCredentialPin::Stale;
    }
    let Some(slot) = snapshot.parent.credential.upgrade() else {
        return TaskParentCredentialPin::Stale;
    };
    if !Arc::ptr_eq(&slot, &snapshot.credential_slot) {
        return TaskParentCredentialPin::Stale;
    }
    drop(slot);
    let Some(guard) = snapshot.credential_slot.try_lock_snapshot() else {
        return TaskParentCredentialPin::Busy;
    };
    if !Arc::ptr_eq(guard.credential(), &snapshot.credential)
        || !core::ptr::eq(guard.slot(), &*snapshot.credential_slot)
        || !task_parent_node_matches(publication, node, &snapshot.parent)
    {
        return TaskParentCredentialPin::Stale;
    }
    TaskParentCredentialPin::Pinned(guard)
}

/// Rebinds one exact child only when it still belongs directly to the
/// departing task. The authoritative process core has already moved the
/// child's process to `replacement`; this function mirrors only that supplied
/// identity and never performs subreaper selection itself.
fn reparent_task_parent_if_owned(
    _publication: &TaskParentPublicationGuard<'_>,
    child: &Arc<TaskParentNode>,
    expected_parent: &Arc<TaskParentNode>,
    replacement: &Arc<TaskParentNode>,
) -> Option<u32> {
    let topology = TASK_PARENT_TOPOLOGY.lock();
    let child_state = child.state.lock();
    let owned = child_state.live
        && child_state
            .parent
            .as_ref()
            .is_some_and(|parent| Arc::ptr_eq(parent, expected_parent));
    drop(child_state);
    if !owned {
        drop(topology);
        return None;
    }
    let replacement_state = replacement.state.lock();
    let replacement_live = replacement_state.live;
    drop(replacement_state);
    if !replacement_live || Arc::ptr_eq(child, replacement) {
        drop(topology);
        return None;
    }
    let retired_unlink = unlink_task_parent_locked(child);
    let retired_link = link_task_parent_locked(child, replacement);
    let signo = child.pdeath_signal.load(Ordering::Acquire);
    drop(topology);
    drop(retired_unlink);
    drop(retired_link);
    Some(signo)
}

fn reparent_task_parent_children_matching(
    publication: &TaskParentPublicationGuard<'_>,
    departing: &Arc<TaskParentNode>,
    replacement: &Arc<TaskParentNode>,
    mut matches: impl FnMut(&Arc<TaskParentNode>) -> bool,
    mut deliver: impl FnMut(Arc<TaskParentNode>, u32),
) {
    let first = {
        let topology = TASK_PARENT_TOPOLOGY.lock();
        let first = departing.state.lock().first_child.clone();
        drop(topology);
        first
    };
    let mut child = first.and_then(|child| child.upgrade());
    let mut remaining = TASK_PARENT_RELATION_HARD_LIMIT.saturating_add(1);
    while let Some(current) = child {
        assert!(remaining != 0, "cycle in exact task-parent child list");
        remaining -= 1;

        // Publication serialization keeps the list stable except for the
        // current node we may unlink below. Retain its original successor
        // before performing that mutation, and destroy the Weak outside the
        // topology spin section.
        let next = {
            let topology = TASK_PARENT_TOPOLOGY.lock();
            let next = current.state.lock().next_sibling.clone();
            drop(topology);
            next
        };
        let next = next.and_then(|next| next.upgrade());
        if matches(&current) {
            let signo =
                reparent_task_parent_if_owned(publication, &current, departing, replacement)
                    .expect("matching exact child left departing parent under publication gate");
            deliver(current, signo);
        } else {
            drop(current);
        }
        child = next;
    }
}

fn finish_task_parent_exit(
    _publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
) -> bool {
    let topology = TASK_PARENT_TOPOLOGY.lock();
    let mut state = node.state.lock();
    if !state.live || state.first_child.is_some() {
        drop(state);
        drop(topology);
        return false;
    }
    state.live = false;
    let old_exit_reaper = state.exit_reaper.take();
    drop(state);
    let retired = unlink_task_parent_locked(node);
    drop(topology);
    drop(old_exit_reaper);
    drop(retired);
    true
}

/// Marks one exact parent task dead, detaches it from its own parent, and
/// processes every exact child. Each child relation is rebound under the
/// topology lock; signal delivery runs only after that lock is released.
fn exit_task_parent_relation(
    publication: &TaskParentPublicationGuard<'_>,
    node: &Arc<TaskParentNode>,
    primary_reaper: Option<Arc<TaskParentNode>>,
    fallback_reaper: Option<Arc<TaskParentNode>>,
    mut deliver: impl FnMut(Arc<TaskParentNode>, u32),
) {
    let primary_reaper =
        resolve_live_task_parent(publication, primary_reaper, fallback_reaper.as_ref());
    let fallback_reaper = resolve_live_task_parent(publication, fallback_reaper, None);
    let (retired, old_exit_reaper) = {
        let topology = TASK_PARENT_TOPOLOGY.lock();
        let selected_reaper = primary_reaper
            .as_ref()
            .filter(|candidate| !Arc::ptr_eq(candidate, node) && candidate.state.lock().live)
            .cloned()
            .or_else(|| {
                fallback_reaper
                    .as_ref()
                    .filter(|candidate| {
                        !Arc::ptr_eq(candidate, node) && candidate.state.lock().live
                    })
                    .cloned()
            });
        let old_exit_reaper = {
            let mut state = node.state.lock();
            if !state.live {
                drop(state);
                drop(topology);
                drop(selected_reaper);
                drop(primary_reaper);
                drop(fallback_reaper);
                return;
            }
            state.live = false;
            core::mem::replace(&mut state.exit_reaper, selected_reaper)
        };
        let retired = unlink_task_parent_locked(node);
        drop(topology);
        (retired, old_exit_reaper)
    };
    drop(retired);
    drop(old_exit_reaper);
    drop(primary_reaper);

    loop {
        let reaper = {
            let topology = TASK_PARENT_TOPOLOGY.lock();
            let candidate = node.state.lock().exit_reaper.clone();
            drop(topology);
            resolve_live_task_parent(publication, candidate, fallback_reaper.as_ref())
        };
        let delivery = {
            let topology = TASK_PARENT_TOPOLOGY.lock();
            let child = node
                .state
                .lock()
                .first_child
                .as_ref()
                .and_then(Weak::upgrade);
            let Some(child) = child else {
                let stale = node.state.lock().first_child.take();
                drop(topology);
                drop(stale);
                drop(reaper);
                break;
            };
            if reaper
                .as_ref()
                .is_some_and(|candidate| !candidate.state.lock().live)
            {
                drop(topology);
                drop(child);
                drop(reaper);
                continue;
            }
            let child_live = child.state.lock().live;
            let retired_unlink = unlink_task_parent_locked(&child);
            let retired_link = if child_live {
                reaper
                    .as_ref()
                    .map(|reaper| link_task_parent_locked(&child, reaper))
            } else {
                None
            };
            let signo = child.pdeath_signal.load(Ordering::Acquire);
            drop(topology);
            drop(retired_unlink);
            drop(retired_link);
            drop(reaper);
            child_live.then_some((child, signo))
        };
        if let Some((child, signo)) = delivery {
            deliver(child, signo);
        }
    }
    drop(fallback_reaper);
}

///  A wrapper type that assumes the inner type is `Sync`.
#[repr(transparent)]
pub struct AssumeSync<T>(pub T);

unsafe impl<T> Sync for AssumeSync<T> {}

impl<T> Deref for AssumeSync<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// One Linux `fs_struct` plus an explicit count of owning task slots.  This
/// deliberately does not use `Arc::strong_count`: boot references and
/// temporary operation snapshots are not Linux `fs_struct` users.
pub struct FsContextSlot {
    context: Arc<Mutex<FsContext>>,
    task_users: AtomicUsize,
}

/// One Linux `files_struct` plus the number of task slots owning it.  This is
/// deliberately separate from `Arc` refcounts: operation snapshots are not
/// Linux `CLONE_FILES` users.
pub struct FdTableSlot {
    table: Arc<crate::file::FdTable>,
    task_users: AtomicUsize,
}

impl FdTableSlot {
    pub(crate) fn new(table: Arc<crate::file::FdTable>) -> Arc<Self> {
        Arc::new(Self {
            table,
            task_users: AtomicUsize::new(0),
        })
    }
    fn share_for_task(slot: &Arc<Self>) -> Arc<Self> {
        slot.clone()
    }
    fn acquire_task(&self) {
        self.task_users.fetch_add(1, Ordering::Relaxed);
    }
    fn release_task(&self) {
        self.task_users.fetch_sub(1, Ordering::Relaxed);
    }
    pub(crate) fn table(&self) -> Arc<crate::file::FdTable> {
        self.table.clone()
    }
    pub(crate) fn has_task_users(&self) -> bool {
        self.task_users.load(Ordering::Acquire) != 0
    }
}

impl FsContextSlot {
    pub(crate) fn new(context: Arc<Mutex<FsContext>>) -> Arc<Self> {
        Arc::new(Self {
            context,
            task_users: AtomicUsize::new(0),
        })
    }

    fn share_for_task(slot: &Arc<Self>) -> Arc<Self> {
        slot.clone()
    }

    fn acquire_task(&self) {
        self.task_users.fetch_add(1, Ordering::Relaxed);
    }

    fn release_task(&self) {
        self.task_users.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Temporary owner claims made before `Thread` is boxed.  Every error path
/// before commit releases both claims; after commit the Thread owns them.
struct TaskResourceAdmission {
    fs: Arc<FsContextSlot>,
    fd: Arc<FdTableSlot>,
    committed: bool,
}
impl TaskResourceAdmission {
    fn new(fs: Arc<FsContextSlot>, fd: Arc<FdTableSlot>) -> Self {
        fs.acquire_task();
        fd.acquire_task();
        Self {
            fs,
            fd,
            committed: false,
        }
    }
    fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for TaskResourceAdmission {
    fn drop(&mut self) {
        if !self.committed {
            self.fs.release_task();
            self.fd.release_task();
        }
    }
}

/// The inner data of a thread.
pub struct Thread {
    /// The process data shared by all threads in the process.
    pub proc_data: Arc<ProcessData>,

    /// Linux namespaces belong to a task, not its thread group.  The process
    /// keeps a creation snapshot for lifecycle bookkeeping, but all current
    /// lookup and setns/unshare publication uses this independently locked
    /// aggregate.  A clone snapshots its calling task before it is published.
    pub(in crate::task) namespaces: SpinNoIrq<NamespaceProxy>,

    /// SEM_UNDO follows the task's IPC namespace attachment.  It is not a
    /// ProcessData field: setns/unshare may retarget one task while siblings
    /// continue to operate on their original IPC manager.
    sem_undo: SpinNoIrq<Arc<super::process::SemUndoState>>,

    /// The task's single atomically published immutable security identity.
    ///
    /// A new thread or fork child starts from one caller snapshot, but owns an
    /// independent slot so later set-ID, capability, or prctl commits affect
    /// only this task. `Thread` is the only writer; `ProcessData` may retain an
    /// `Arc` to the same slot solely to preserve Linux group-leader identity
    /// after an early leader exit. There is never a second credential copy or
    /// publication point.
    pub(in crate::task) credential: Arc<CredentialSlot>,

    /// Linux `fs_struct`: shared only when clone semantics request it.  The
    /// slot itself is task-local so `unshare(CLONE_FS)` can replace just the
    /// calling thread's reference.
    fs_context: SpinNoIrq<Option<Arc<FsContextSlot>>>,

    /// Linux `files_struct`, independently selected for every task.
    fd_table: SpinNoIrq<Option<Arc<FdTableSlot>>>,

    /// One atomically consistent, task-local seccomp mode and filter ancestry
    /// published through the independent bounded seccomp RCU domain.
    ///
    /// Fork and clone initialize an independent publication slot from one
    /// caller snapshot. Immutable filter nodes remain shared and accounted
    /// until their final owner exits.
    pub(in crate::task) seccomp: ThreadSeccompSlot,

    /// Immutable Landlock domains are task-local and are snapshot by clone.
    landlock: SpinNoIrq<LandlockDomain>,

    /// Preallocated disabled state retained for cold snapshots and terminal
    /// teardown. An active slot is synchronously cleared at exit, so the
    /// terminal path needs neither a replacement allocation nor retire-queue
    /// capacity.
    pub(in crate::task) seccomp_terminal_disabled: Arc<SeccompState>,

    /// Linux personality is a task property.  A fork or clone snapshots the
    /// caller, while siblings may subsequently change theirs independently.
    personality: AtomicU32,

    /// Thread-local Linux restartable-sequence registration and event state.
    ///
    /// This deliberately lives on `Thread`, not `ProcessData`: rseq
    /// registration is private to one Linux thread even when siblings share
    /// an address space. Scheduler, signal, and final-return callers publish
    /// observations through this state without resolving an implicit task.
    pub(in crate::task) rseq: SpinNoIrq<ThreadRseq>,

    /// Per-task operation credential used while an OFD read/write is active.
    /// This is not process Scope state: sibling threads may block or perform
    /// I/O concurrently without replacing each other's Linux `file->f_cred`.
    file_operation_credential: SpinNoIrq<Option<Arc<Cred>>>,

    /// Legacy Linux write-side opener snapshot used by cgroup control files.
    /// Keep it task-local for the same reason as `file_operation_credential`:
    /// a process scope is shared by sibling threads and cannot represent two
    /// blocked OFD operations at once.
    file_write_credentials: SpinNoIrq<Option<OpenCredentials>>,

    /// Pending Linux `CLONE_CHILD_SETTID` publication.
    ///
    /// Clone installs this address while the task is still private. The child
    /// consumes it exactly once in its own address space before first entering
    /// user mode; the parent is not required to wait for that store.
    set_child_tid: AtomicUsize,

    /// The clear thread tid field
    ///
    /// See <https://manpages.debian.org/unstable/manpages-dev/set_tid_address.2.en.html#clear_child_tid>
    ///
    /// When the thread exits, the kernel clears the word at this address if it
    /// is not NULL.
    clear_child_tid: AtomicUsize,

    /// User-visible thread ID. Normally this matches the scheduler task ID,
    /// but after a non-leader execve() it is rebound to the process ID.
    visible_tid: AtomicU32,

    /// Immutable scheduler/core membership identity. Unlike `visible_tid`,
    /// this never changes during non-leader exec de-threading.
    kernel_tid: Pid,

    /// Exact Linux task-parent identity and its task-local parent-death signal.
    task_parent: Arc<TaskParentNode>,

    /// The head of the robust list
    robust_list_head: AtomicUsize,

    /// The thread-level signal manager
    pub signal: Arc<ThreadSignalManager>,

    /// Time manager
    ///
    /// This is assumed to be `Sync` because it's only borrowed mutably during
    /// context switches, which is exclusive to the current thread.
    pub time: AssumeSync<RefCell<TimeManager>>,
    /// Best-effort CPU usage snapshot that can be sampled without touching
    /// the live time manager.
    live_usage: AtomicTaskUsage,
    /// Linux task-local resource counters.  Bytes are retained for I/O so
    /// several sub-block operations are rounded only once at snapshot time,
    /// matching task_io_account_*'s 512-byte units.
    minor_faults: AtomicU64,
    major_faults: AtomicU64,
    io_read_bytes: AtomicU64,
    io_write_bytes: AtomicU64,
    voluntary_switches: AtomicU64,
    involuntary_switches: AtomicU64,
    /// Last CPU on which this task entered.  The sentinel avoids treating a
    /// first dispatch as a migration.
    perf_last_cpu: AtomicUsize,
    perf_events: SpinNoIrq<Vec<Arc<crate::file::PerfGroup>>>,
    /// Scheduler-owned utilization clamps, published as a coherent pair.
    ///
    /// `sched_clamp_sequence` is a tiny seqlock: writers are serialized with
    /// its low bit and readers retry instead of allocating or locking in a
    /// scheduler callback.  The scheduler's own commit serial is retained
    /// separately so a delayed completion cannot replace a newer clamp.
    #[cfg(feature = "hwp-uclamp")]
    sched_clamp: SchedulerClampCache,
    /// Even global clamp-policy generation last folded into this task's
    /// scheduler-owned effective bounds. Zero intentionally means dirty for
    /// a newly constructed task, closing the first-user-entry race with a
    /// concurrent cgroup/system clamp write.
    uclamp_policy_generation: AtomicU64,
    /// Set when the exit path pre-accounts the final TASK_DEAD handoff.
    /// The scheduler Exit callback consumes this marker so `nvcsw` is
    /// published exactly once before the frozen usage snapshot is queued.
    exit_switch_preaccounted: AtomicBool,
    /// Best-effort user-visible blocking state used by procfs.
    proc_state_hint: AtomicU8,
    /// Nested scheduler I/O-wait ownership.  The depth, outer-entry timestamp,
    /// and accumulated duration are independent of the procfs hint so wakeup,
    /// signal, and nested readiness guards cannot lose accounting.
    iowait_depth: AtomicU32,
    iowait_started_ns: AtomicU64,
    iowait_total_ns: AtomicU64,
    /// Exact per-thread ownership of one cgroup-freezer parked-count slot.
    /// It prevents repeated signal/user-return checks from double-counting a
    /// thread while it remains in the scheduler's stop wait.
    cgroup_freezer_parked: AtomicBool,

    /// Linux per-task I/O priority context. Linux does not allocate an
    /// `io_context` until a task first needs one; `None` is therefore a real
    /// state, not an eagerly allocated `IOPRIO_CLASS_NONE` value. The
    /// reference is shared by `CLONE_IO` children and copied for ordinary
    /// fork/clone children.
    io_context: SpinNoIrq<Option<Arc<AtomicU16>>>,

    /// `PR_SET_IO_FLUSHER` is task-local and intentionally independent from
    /// the task's Linux I/O priority context.
    io_flusher: AtomicBool,

    /// Per-thread x86 machine-check kill policy (`PR_MCE_KILL_*`).  This is
    /// sampled by the #MC-to-user delivery bridge, not a process-global knob.
    mce_kill_policy: AtomicU8,

    /// x86 `ioperm(2)` bitmap and `iopl(2)` emulation state.
    ioport: SpinNoIrq<IoPortState>,

    /// The OOM score adjustment value.
    oom_score_adj: AtomicI32,

    /// Ready to exit
    pub exit: Arc<AtomicBool>,

    /// Whether this thread currently owns the leaked active-scope read guard.
    active_scope_read_held: AtomicBool,

    /// Syscall restart bookkeeping shared across normal execution and signal handlers.
    pub(in crate::task) restart: SpinNoIrq<RestartTracker>,

    /// Self exit event
    pub exit_event: Arc<PollSet>,

    /// Final-OFD notifications published by this actor and not yet completed
    /// by the policy worker.
    deferred_work: Arc<DeferredWorkAccount>,
}

/// Exact scheduler tuple admitted for an unpublished Linux task.
#[derive(Clone, Copy)]
pub(crate) struct SchedulerSeed {
    pub(crate) state: SchedState,
    pub(crate) reset_on_fork: bool,
    pub(crate) uclamp: UclampRequest,
    pub(crate) utilization_bounds: UtilizationBounds,
    pub(crate) version: u64,
}

#[cfg(feature = "hwp-uclamp")]
const SCHED_CLAMP_MASK: u32 = 0x7ff;

#[cfg(feature = "hwp-uclamp")]
const fn pack_sched_clamp(min: u16, max: u16) -> u32 {
    debug_assert!(min <= max && max <= 1024);
    (min as u32) | ((max as u32) << 11)
}

#[cfg(feature = "hwp-uclamp")]
const fn unpack_sched_clamp(packed: u32) -> (u16, u16) {
    (
        (packed & SCHED_CLAMP_MASK) as u16,
        ((packed >> 11) & SCHED_CLAMP_MASK) as u16,
    )
}

#[cfg(feature = "hwp-uclamp")]
/// Serial-number ordering for scheduler commit streams, including wraparound.
const fn scheduler_commit_is_newer_or_equal(candidate: u64, published: u64) -> bool {
    candidate.wrapping_sub(published) < (1_u64 << 63)
}

#[cfg(feature = "hwp-uclamp")]
struct SchedulerClampCache {
    packed: AtomicU32,
    version: AtomicU64,
    sequence: AtomicU64,
}

#[cfg(feature = "hwp-uclamp")]
impl SchedulerClampCache {
    const fn new(min: u32, max: u32, version: u64) -> Self {
        debug_assert!(min <= max && max <= 1024);
        Self {
            packed: AtomicU32::new(pack_sched_clamp(min as u16, max as u16)),
            version: AtomicU64::new(version),
            sequence: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> (u16, u16, u64) {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let packed = self.packed.load(Ordering::Relaxed);
            let version = self.version.load(Ordering::Relaxed);
            if self.sequence.load(Ordering::Acquire) == before {
                let (min, max) = unpack_sched_clamp(packed);
                return (min, max, version);
            }
        }
    }

    fn publish(&self, min: u32, max: u32, version: u64) {
        debug_assert!(min <= max && max <= 1024);
        let packed = pack_sched_clamp(min as u16, max as u16);
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let published = self.version.load(Ordering::Acquire);
            if !scheduler_commit_is_newer_or_equal(version, published) {
                return;
            }
            if self
                .sequence
                .compare_exchange_weak(
                    sequence,
                    sequence.wrapping_add(1),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            self.packed.store(packed, Ordering::Relaxed);
            self.version.store(version, Ordering::Relaxed);
            self.sequence
                .store(sequence.wrapping_add(2), Ordering::Release);
            return;
        }
    }
}

/// The thread-local signal execution state which survives `fork`/`clone`.
///
/// This deliberately excludes every signal-delivery data-plane field: pending
/// records, an in-flight delivery selection, bypass tokens, and wake state all
/// remain private to the fresh child signal endpoint.
#[derive(Clone, Copy)]
pub(crate) struct ForkSignalExecutionState {
    blocked: SignalSet,
    visible_stack: SignalStack,
}

/// How clone initializes the child's alternate signal stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForkSignalAltStack {
    Inherit,
    Clear,
}

impl Thread {
    /// Takes the signal state snapshot needed before an unpublished fork child
    /// is constructed.
    ///
    /// Clone executes in the source thread, so its blocked mask and visible
    /// alternate-stack state cannot be concurrently changed by another
    /// execution context of that thread. `real_blocked` is intentionally not
    /// read: it is private to an active `rt_sigtimedwait` guard, and that guard
    /// keeps its owner inside the wait syscall. The guard restores it before a
    /// handler can run, hence a clone syscall is never reachable while it is
    /// set. `rt_sigsuspend`'s temporary mask is already the visible mask; its
    /// original mask, and an `SS_AUTODISARM` stack's original configuration,
    /// are retained in the inherited user signal frame and are restored by the
    /// child's `rt_sigreturn`.
    pub(crate) fn fork_signal_execution_state(&self) -> ForkSignalExecutionState {
        ForkSignalExecutionState {
            blocked: self.signal.blocked(),
            visible_stack: self.signal.stack(),
        }
    }

    /// Applies a source snapshot before this child is published. The fresh
    /// manager intentionally retains its empty pending queue and all fresh
    /// delivery reservation, selection, bypass, and wake state.
    pub(crate) fn apply_fork_signal_execution_state(
        &self,
        state: ForkSignalExecutionState,
        altstack: ForkSignalAltStack,
    ) {
        self.signal.set_blocked(state.blocked);
        self.signal.set_stack(match altstack {
            ForkSignalAltStack::Inherit => state.visible_stack,
            ForkSignalAltStack::Clear => SignalStack::default(),
        });
    }

    /// Publishes an event into this exact task's scheduler-owned lifecycle.
    /// Reservation occurs in perf_event_open, never in a switch callback.
    pub(crate) fn attach_perf_group(&self, group: Arc<crate::file::PerfGroup>) -> AxResult<()> {
        let mut events = self.perf_events.lock();
        events.retain(|attached| !attached.is_prunable());
        if events.iter().any(|attached| Arc::ptr_eq(attached, &group)) {
            return Ok(());
        }
        if events.len() == crate::file::MAX_GROUPS_PER_THREAD {
            return Err(AxError::OperationNotSupported);
        }
        events.push(group);
        Ok(())
    }

    /// Copy only attr-authorized perf contexts into a private child scheduler
    /// state.  The child inherits no descriptor numbers or ring mappings.
    pub(crate) fn inherit_perf_groups_to(
        &self,
        child: &Thread,
        child_task_id: u64,
        clone_thread: bool,
    ) -> AxResult<()> {
        let groups = self.perf_events.lock();
        for group in groups.iter() {
            if let Some(inherited) = group.inherit_for_child(child_task_id, clone_thread)? {
                child.attach_perf_group(inherited)?;
            }
        }
        Ok(())
    }

    pub(crate) fn detach_empty_perf_group(&self, group: &Arc<crate::file::PerfGroup>) {
        let mut events = self.perf_events.lock();
        events.retain(|attached| !Arc::ptr_eq(attached, group) || !attached.is_prunable());
    }

    fn perf_on_enter(&self) {
        self.arbitrate_perf_current(false, true);
        let mut slots = [None; 4];
        let mut used = 0;
        let events = self.perf_events.lock();
        for group in events.iter() {
            group.append_debug_breakpoints(&mut slots, &mut used);
        }
        crate::file::PerfGroup::cpu_context_append_debug_breakpoints(
            axhal::percpu::this_cpu_id(),
            &mut slots,
            &mut used,
        );
        axcpu::asm::program_perf_debug_registers(slots);
    }

    /// Take a bounded strong snapshot before entering the CPU PMU arbiter.
    /// This deliberately releases the task event lock before it takes the
    /// CPU-context registry, so open/close/control cannot invert scheduler
    /// locking. `activate` is used only at the real task-enter boundary.
    pub(crate) fn arbitrate_perf_current(&self, tick: bool, activate: bool) {
        let mut groups: [Option<Arc<crate::file::PerfGroup>>; crate::file::MAX_GROUPS_PER_THREAD] =
            core::array::from_fn(|_| None);
        {
            let mut events = self.perf_events.lock();
            if activate {
                events.retain(|group| {
                    group.on_enter();
                    !group.is_prunable()
                });
            } else {
                events.retain(|group| !group.is_prunable());
            }
            for (slot, group) in groups.iter_mut().zip(events.iter()) {
                *slot = Some(group.clone());
            }
        }
        crate::file::PerfGroup::arbitrate_cpu_with_task_slots(
            axhal::percpu::this_cpu_id(),
            &groups,
            tick,
        );
    }

    /// Scheduler observer entry supplied with the precise CPL of the timer
    /// interrupt's saved context. The task extension still handles generic
    /// scheduler housekeeping; perf accounting lives here so it can share
    /// that hardware fact with CPU/cgroup contexts.
    pub(crate) fn perf_on_timer_tick(&self, interrupted_user: bool) {
        let events = self.perf_events.lock();
        let now = axhal::time::monotonic_time_nanos();
        for group in events.iter() {
            group.account_clock_sources_domain(now, interrupted_user);
        }
        drop(events);
        self.arbitrate_perf_current(true, false);
    }

    /// Mark an explicit user/kernel execution boundary. This settles the
    /// preceding interval under its prior domain before beginning the new one.
    pub(crate) fn perf_clock_domain_transition(&self, user: bool) {
        let events = self.perf_events.lock();
        for group in events.iter() {
            group.account_clock_domain_transition(user);
        }
        drop(events);
        crate::file::PerfGroup::cpu_context_clock_domain_transition(
            axhal::percpu::this_cpu_id(),
            user,
        );
    }

    fn perf_on_leave(&self) {
        {
            let mut events = self.perf_events.lock();
            events.retain(|group| {
                group.on_leave();
                !group.is_prunable()
            });
        }
        // The global scheduler observer has already entered the successor's
        // CPU/cgroup contexts. Preserve their watchpoints when the successor
        // is idle or kernel-only; a user successor's on_enter will append its
        // task-local slots immediately afterwards.
        let mut slots = [None; 4];
        let mut used = 0;
        crate::file::PerfGroup::cpu_context_append_debug_breakpoints(
            axhal::percpu::this_cpu_id(),
            &mut slots,
            &mut used,
        );
        axcpu::asm::program_perf_debug_registers(slots);
    }

    fn perf_emit_switch(&self, switch_out: bool, peer: Option<(u32, u32)>) {
        let own = (self.proc_data.proc.pid() as u32, self.kernel_tid() as u32);
        for group in self.perf_events.lock().iter() {
            group.emit_switch_record(switch_out, own, peer);
        }
    }

    pub(crate) fn perf_emit_mmap(
        &self,
        addr: u64,
        len: u64,
        pgoff: u64,
        info: &crate::perf_records::MmapInfo<'_>,
    ) {
        let pid = self.proc_data.proc.pid() as u32;
        let tid = self.kernel_tid() as u32;
        for group in self.perf_events.lock().iter() {
            group.emit_mmap_record(addr, len, pgoff, info, pid, tid);
        }
    }

    pub(crate) fn perf_on_exec(&self) {
        let pid = self.proc_data.proc.pid() as u32;
        let tid = self.kernel_tid() as u32;
        let name = current().try_name().ok();
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.on_exec(pid, tid, name.as_deref().unwrap_or_default().as_bytes());
            !group.is_prunable()
        });
    }

    pub(crate) fn perf_emit_fork(&self, child_pid: u32, child_tid: u32) {
        let parent_pid = self.proc_data.proc.pid() as u32;
        let parent_tid = self.kernel_tid() as u32;
        for group in self.perf_events.lock().iter() {
            group.emit_fork_record(child_pid, parent_pid, child_tid, parent_tid);
        }
    }

    fn perf_emit_exit(&self) {
        let pid = self.proc_data.proc.pid() as u32;
        let ppid = self
            .proc_data
            .proc
            .parent()
            .map_or(0, |parent| parent.pid() as u32);
        let tid = self.kernel_tid() as u32;
        for group in self.perf_events.lock().iter() {
            group.emit_exit_record(pid, ppid, tid, ppid);
        }
    }

    fn perf_on_minor_fault(&self) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.on_minor_fault();
            !group.is_prunable()
        });
        crate::file::PerfGroup::cpu_context_minor_fault(axhal::percpu::this_cpu_id());
    }

    fn perf_on_major_fault(&self) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.on_major_fault();
            !group.is_prunable()
        });
        crate::file::PerfGroup::cpu_context_major_fault(axhal::percpu::this_cpu_id());
    }

    fn perf_on_migration(&self) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.on_migration();
            !group.is_prunable()
        });
    }

    /// Last scheduler CPU observed for an exact task.  Cgroup's membership
    /// commit uses this only to target its bounded PerfReconcile IPI; the IPI
    /// handler validates that the task is still current before touching a
    /// CPU-context group.
    pub(crate) fn perf_last_cpu_for_reconcile(&self) -> Option<usize> {
        let cpu = self.perf_last_cpu.load(Ordering::Acquire);
        (cpu != usize::MAX).then_some(cpu)
    }

    fn perf_emit_tracepoint(&self, id: u64) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.emit_tracepoint(id);
            !group.is_prunable()
        });
    }

    /// Trap-side dynamic perf source delivery.  The event descriptor is a
    /// compact Copy value, so debug/probe dispatch does not allocate or touch
    /// a user mapping while matching this task's active groups.
    pub(crate) fn perf_emit_dynamic(&self, event: crate::file::PerfEvent) {
        self.perf_emit_dynamic_raw(event, &[]);
    }

    pub(crate) fn perf_emit_dynamic_raw(&self, event: crate::file::PerfEvent, raw: &[u8]) {
        self.perf_emit_dynamic_raw_at(event, 0, raw);
    }

    pub(crate) fn perf_emit_dynamic_raw_at(
        &self,
        event: crate::file::PerfEvent,
        ip: u64,
        raw: &[u8],
    ) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.emit_dynamic_raw_at(event, ip, raw);
            !group.is_prunable()
        });
        drop(events);
        crate::file::PerfGroup::cpu_context_dynamic_raw_at(
            axhal::percpu::this_cpu_id(),
            event,
            ip,
            raw,
        );
    }

    pub(crate) fn perf_emit_tracepoint_raw(&self, id: u64, raw: &[u8], timestamp: u64) {
        let mut events = self.perf_events.lock();
        events.retain(|group| {
            group.emit_tracepoint_raw(id, raw, timestamp);
            !group.is_prunable()
        });
        drop(events);
        crate::file::PerfGroup::cpu_context_tracepoint(axhal::percpu::this_cpu_id(), id, raw, timestamp);
    }

    pub(crate) fn perf_emit_debug_exception(&self, slot_mask: u64, ip: u64, user: bool) {
        let mut events = self.perf_events.lock();
        let mut slot = 0;
        events.retain(|group| {
            group.emit_debug_exception(slot_mask, &mut slot, ip, user);
            !group.is_prunable()
        });
        drop(events);
        crate::file::PerfGroup::cpu_context_debug_exception(
            axhal::percpu::this_cpu_id(),
            slot_mask,
            &mut slot,
            ip,
            user,
        );
    }

    pub(crate) fn refresh_perf_debug_registers(&self) {
        let events = self.perf_events.lock();
        let mut slots = [None; 4];
        let mut used = 0;
        for group in events.iter() {
            group.append_debug_breakpoints(&mut slots, &mut used);
        }
        crate::file::PerfGroup::cpu_context_append_debug_breakpoints(
            axhal::percpu::this_cpu_id(),
            &mut slots,
            &mut used,
        );
        axcpu::asm::program_perf_debug_registers(slots);
    }

    /// Create a new [`Thread`].
    pub(crate) fn try_new(
        tid: u32,
        proc_data: Arc<ProcessData>,
        credential: Arc<CredentialSlot>,
        seccomp: Arc<SeccompState>,
        fs_context: Arc<FsContextSlot>,
        fd_table: Arc<FdTableSlot>,
        scheduler_seed: SchedulerSeed,
    ) -> AxResult<(Box<Self>, ThreadSignalRegistration)> {
        Self::try_new_with_io_context(
            tid,
            proc_data,
            credential,
            seccomp,
            None,
            false,
            fs_context,
            fd_table,
            0,
            LandlockDomain::default(),
            scheduler_seed,
        )
    }

    /// Create a task with an explicitly selected Linux I/O-priority context.
    /// Ordinary fork/clone callers pass an independent context, while
    /// `CLONE_IO` passes the parent's shared reference.
    pub(crate) fn try_new_with_io_context(
        tid: u32,
        proc_data: Arc<ProcessData>,
        credential: Arc<CredentialSlot>,
        seccomp: Arc<SeccompState>,
        io_context: Option<Arc<AtomicU16>>,
        io_flusher: bool,
        fs_context: Arc<FsContextSlot>,
        fd_table: Arc<FdTableSlot>,
        personality: u32,
        landlock: LandlockDomain,
        scheduler_seed: SchedulerSeed,
    ) -> AxResult<(Box<Self>, ThreadSignalRegistration)> {
        // ProcessData is created before the child scheduler object. Seed its
        // durable identity from the creator's already admitted tuple; never
        // resample a parent or substitute a default during allocation.
        if proc_data.proc.pid() == tid {
            // `proc.pid() == tid` above is the construction-time group-leader
            // identity; no Thread object exists yet to query it from.
            proc_data.seed_scheduler_state(
                scheduler_seed.state,
                scheduler_seed.reset_on_fork,
                scheduler_seed.uclamp,
                scheduler_seed.utilization_bounds,
                scheduler_seed.version,
            );
        }
        let signal = ThreadSignalManager::try_new(proc_data.signal.clone())
            .map_err(|_| AxError::NoMemory)?;
        let exit = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let exit_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let deferred_work =
            Arc::try_new(DeferredWorkAccount::new()).map_err(|_| AxError::NoMemory)?;
        let restart = RestartTracker::try_new().map_err(|_| AxError::NoMemory)?;
        let task_parent =
            TaskParentNode::try_new(tid, Arc::downgrade(&proc_data), Arc::downgrade(&credential))?;
        let time = TimeManager::new(&proc_data);
        let (seccomp, seccomp_terminal_disabled) = super::seccomp::new_thread_seccomp(seccomp)?;
        let resource_admission = TaskResourceAdmission::new(fs_context.clone(), fd_table.clone());
        let namespaces = proc_data.namespace_proxy();
        // SEM_UNDO follows the live task namespace, not ProcessData's leader
        // snapshot. Clone/unshare may replace this private state before task
        // publication.
        let sem_undo = super::process::SemUndoState::try_new(namespaces.ipc())?;
        let thread = Box::try_new(Thread {
            signal,
            proc_data,
            namespaces: SpinNoIrq::new(namespaces),
            sem_undo: SpinNoIrq::new(sem_undo),
            credential,
            fs_context: SpinNoIrq::new(Some(fs_context)),
            fd_table: SpinNoIrq::new(Some(fd_table)),
            seccomp,
            seccomp_terminal_disabled,
            landlock: SpinNoIrq::new(landlock),
            personality: AtomicU32::new(personality),
            rseq: SpinNoIrq::new(ThreadRseq::new()),
            file_operation_credential: SpinNoIrq::new(None),
            file_write_credentials: SpinNoIrq::new(None),
            set_child_tid: AtomicUsize::new(0),
            clear_child_tid: AtomicUsize::new(0),
            visible_tid: AtomicU32::new(tid),
            kernel_tid: tid,
            task_parent,
            robust_list_head: AtomicUsize::new(0),
            time: AssumeSync(RefCell::new(time)),
            live_usage: AtomicTaskUsage::new(),
            minor_faults: AtomicU64::new(0),
            major_faults: AtomicU64::new(0),
            io_read_bytes: AtomicU64::new(0),
            io_write_bytes: AtomicU64::new(0),
            voluntary_switches: AtomicU64::new(0),
            involuntary_switches: AtomicU64::new(0),
            perf_last_cpu: AtomicUsize::new(usize::MAX),
            perf_events: SpinNoIrq::new({
                let mut groups = Vec::new();
                groups
                    .try_reserve_exact(crate::file::MAX_GROUPS_PER_THREAD)
                    .map_err(|_| AxError::NoMemory)?;
                groups
            }),
            #[cfg(feature = "hwp-uclamp")]
            sched_clamp: SchedulerClampCache::new(
                scheduler_seed.utilization_bounds.minimum,
                scheduler_seed.utilization_bounds.maximum,
                scheduler_seed.version,
            ),
            uclamp_policy_generation: AtomicU64::new(0),
            exit_switch_preaccounted: AtomicBool::new(false),
            proc_state_hint: AtomicU8::new(ProcStateHint::None as u8),
            iowait_depth: AtomicU32::new(0),
            iowait_started_ns: AtomicU64::new(0),
            iowait_total_ns: AtomicU64::new(0),
            cgroup_freezer_parked: AtomicBool::new(false),
            io_context: SpinNoIrq::new(io_context),
            io_flusher: AtomicBool::new(io_flusher),
            mce_kill_policy: AtomicU8::new(2),
            ioport: SpinNoIrq::new(IoPortState::default()),
            exit,
            oom_score_adj: AtomicI32::new(200),
            active_scope_read_held: AtomicBool::new(false),
            restart: SpinNoIrq::new(restart),
            exit_event,
            deferred_work,
        })
        .map_err(|_| AxError::NoMemory)?;
        resource_admission.commit();
        let registration = thread
            .signal
            .try_register(tid)
            .map_err(|error| match error {
                ThreadRegistrationError::NoMemory => AxError::NoMemory,
                ThreadRegistrationError::Capacity => AxError::StorageFull,
                ThreadRegistrationError::AlreadyRegistered
                | ThreadRegistrationError::TidInUse
                | ThreadRegistrationError::Cancelled => AxError::BadState,
            })?;
        Ok((thread, registration))
    }

    pub(crate) fn namespace_proxy(&self) -> NamespaceProxy {
        self.namespaces.lock().clone()
    }

    pub(crate) fn namespace_credential_fs_snapshot(&self) -> NamespaceCredentialFsSnapshot {
        let _publication = super::fs_context_publication();
        let namespaces = self.namespaces.lock().clone();
        let mount_topology = namespaces.mount().topology();
        let credential = self.current_cred();
        let fs_slot = self
            .fs_context
            .lock()
            .as_ref()
            .expect("retired fs_struct")
            .clone();
        let fs_context = fs_slot.context.clone();
        let fs_snapshot = fs_context.lock().clone();
        let landlock_domain = self.landlock_domain();
        let controlling_terminal = self.proc_data.proc.group().session().terminal();
        NamespaceCredentialFsSnapshot {
            namespaces,
            mount_topology,
            credential,
            fs_slot,
            fs_context,
            fs_snapshot,
            landlock_domain,
            controlling_terminal,
        }
    }

    pub(crate) fn user_ns(&self) -> Arc<super::process::UserNamespace> {
        self.namespaces.lock().user()
    }
    pub(crate) fn pid_ns(&self) -> Arc<super::process::PidNamespace> {
        self.namespaces.lock().pid()
    }
    pub(crate) fn pid_ns_for_children(&self) -> Arc<super::process::PidNamespace> {
        self.namespaces.lock().pid_for_children()
    }
    pub(crate) fn mount_ns(&self) -> Arc<super::process::MountNamespace> {
        self.namespaces.lock().mount()
    }
    pub(crate) fn ipc_ns(&self) -> Arc<crate::syscall::IpcNamespace> {
        self.namespaces.lock().ipc()
    }
    pub(crate) fn net_ns(&self) -> Arc<super::process::NetworkNamespace> {
        self.namespaces.lock().net()
    }
    pub(crate) fn cgroup_ns(&self) -> Arc<super::process::CgroupNamespace> {
        self.namespaces.lock().cgroup()
    }
    pub(crate) fn uts_ns(&self) -> Arc<super::process::UtsNamespace> {
        self.namespaces.lock().uts()
    }
    pub(crate) fn time_ns(&self) -> Arc<super::process::TimeNamespace> {
        self.namespaces.lock().time()
    }
    pub(crate) fn time_ns_for_children(&self) -> Arc<super::process::TimeNamespace> {
        self.namespaces.lock().time_for_children()
    }

    pub(crate) fn prepare_namespace_replacement(
        &self,
        update: impl FnOnce(&mut NamespaceProxy),
    ) -> super::process::PreparedNamespaceProxyReplacement {
        let mut replacement = self.namespace_proxy();
        update(&mut replacement);
        super::process::PreparedNamespaceProxyReplacement { replacement }
    }

    /// Called only before child task publication.  This is deliberately a
    /// snapshot rather than a shared slot: a later setns in either sibling
    /// must not retarget the other task.
    pub(crate) fn inherit_namespace_proxy_from(&self, source: &Self) {
        let replacement = source.namespace_proxy();
        let old = core::mem::replace(&mut *self.namespaces.lock(), replacement);
        drop(old);
    }

    pub(crate) fn sem_undo(&self) -> Arc<super::process::SemUndoState> {
        self.sem_undo.lock().clone()
    }

    pub(crate) fn replace_sem_undo(&self, replacement: Arc<super::process::SemUndoState>) {
        let old = self.replace_sem_undo_deferred(replacement);
        Self::retire_sem_undo(old);
    }

    /// Exchanges the attachment while an external publication gate is held.
    /// The caller retires the displaced list only after that gate is released.
    pub(crate) fn replace_sem_undo_deferred(
        &self,
        replacement: Arc<super::process::SemUndoState>,
    ) -> Arc<super::process::SemUndoState> {
        core::mem::replace(&mut *self.sem_undo.lock(), replacement)
    }

    pub(crate) fn retire_sem_undo(old: Arc<super::process::SemUndoState>) {
        // Leaving an IPC namespace must not silently lose a private SEM_UNDO
        // adjustment. Shared CLONE_SYSVSEM state remains live until its last
        // thread owner exits or changes namespace.
        if Arc::strong_count(&old) == 1 {
            old.apply_on_final_exit();
        }
        drop(old);
    }

    /// Atomically replace a task's IPC proxy attachment and its matching
    /// private SEM_UNDO list. The displaced list retains its old manager and
    /// is retired only after the publication gate is released.
    pub(crate) fn commit_namespace_with_sem_undo(
        &self,
        prepared: super::process::PreparedNamespaceProxyReplacement,
        replacement: Arc<super::process::SemUndoState>,
    ) {
        let (old_proxy, old) = {
            let _publication = super::fs_context_publication();
            let old_proxy = prepared.commit_under_publication(self);
            let old_sem_undo = core::mem::replace(&mut *self.sem_undo.lock(), replacement);
            (old_proxy, old_sem_undo)
        };
        drop(old_proxy);
        Self::retire_sem_undo(old);
    }

    /// Publishes a prepared pidfd-setns namespace aggregate and every
    /// resource attachment coupled to it.  A pidfd request can select mount
    /// and IPC namespaces together, so the fs_struct and SEM_UNDO pointers
    /// cannot be committed through separate visibility windows.
    pub(crate) fn commit_namespace_with_resources(
        &self,
        prepared: super::process::PreparedNamespaceProxyReplacement,
        replacement_fs: Option<Arc<FsContextSlot>>,
        replacement_sem_undo: Option<Arc<super::process::SemUndoState>>,
    ) {
        let (old_proxy, old_fs, old_sem_undo) = {
            let _publication = super::fs_context_publication();
            let old_proxy = prepared.commit_under_publication(self);
            let old_fs = replacement_fs.map(|replacement| self.replace_fs_context(replacement));
            let old_sem_undo = replacement_sem_undo
                .map(|replacement| core::mem::replace(&mut *self.sem_undo.lock(), replacement));
            (old_proxy, old_fs, old_sem_undo)
        };
        drop(old_proxy);
        drop(old_fs);
        if let Some(old_sem_undo) = old_sem_undo {
            Self::retire_sem_undo(old_sem_undo);
        }
    }

    pub(crate) fn apply_sem_undo_on_exit(&self) {
        let state = self.sem_undo();
        if Arc::strong_count(&state) == 2 {
            state.apply_on_final_exit();
        }
    }

    pub(crate) fn personality(&self) -> u32 {
        self.personality.load(Ordering::Acquire)
    }

    /// Reads the scheduler clamp and its commit version as one coherent tuple.
    #[cfg(feature = "hwp-uclamp")]
    pub(crate) fn scheduler_clamp_snapshot(&self) -> (u16, u16, u64) {
        self.sched_clamp.snapshot()
    }

    /// Publishes a successfully committed scheduler clamp.  A delayed commit
    /// is harmlessly ignored once a newer serial has become visible.
    #[cfg(feature = "hwp-uclamp")]
    pub(crate) fn publish_scheduler_clamp(&self, min: u32, max: u32, version: u64) {
        self.sched_clamp.publish(min, max, version);
    }

    /// Marks this thread's task-local scheduler clamp as derived from one
    /// fully committed cgroup/system policy generation.  Writers only call
    /// this after the runqueue transaction succeeds; failed transactions
    /// leave the task dirty for a later safe-boundary retry.
    pub(crate) fn publish_uclamp_policy_generation(&self, generation: u64) {
        debug_assert_eq!(generation & 1, 0);
        self.uclamp_policy_generation
            .store(generation, Ordering::Release);
    }

    pub(crate) fn uclamp_policy_generation(&self) -> u64 {
        self.uclamp_policy_generation.load(Ordering::Acquire)
    }

    #[cfg(feature = "hwp-uclamp")]
    fn apply_current_hwp_clamp(&self) {
        let (min, max, _) = self.scheduler_clamp_snapshot();
        // Unsupported firmware/host stubs are an intentional no-op.  HWP
        // policy must never turn a scheduler transition into a failure.
        let _ = axhal::hwp::apply_current_clamp(min, max);
    }

    #[cfg(feature = "hwp-uclamp")]
    fn clear_current_hwp_clamp() {
        let _ = axhal::hwp::apply_current_clamp(0, 1024);
    }

    /// A child which inherits an in-flight signal handler also inherits the
    /// restart bookkeeping for that handler. CET signal authentication is not
    /// stored here: its LIFO state lives entirely in the copied shadow stack.
    pub(crate) fn copy_signal_handler_restart_state_from(&self, source: &Self) {
        self.restart
            .lock()
            .copy_handler_state_from(&source.restart.lock());
    }

    pub(crate) fn landlock_domain(&self) -> LandlockDomain {
        self.landlock.lock().clone()
    }
    pub(crate) fn replace_landlock_domain(&self, domain: LandlockDomain) {
        *self.landlock.lock() = domain;
    }

    pub(crate) fn set_personality(&self, personality: u32) {
        self.personality.store(personality, Ordering::Release);
    }

    pub(crate) fn clear_personality_flags(&self, flags: u32) {
        self.personality.fetch_and(!flags, Ordering::AcqRel);
    }

    /// Snapshot this task's current Linux filesystem context pointer.
    pub(crate) fn fs_context(&self) -> Arc<Mutex<FsContext>> {
        self.fs_context
            .lock()
            .as_ref()
            .expect("retired fs_struct")
            .context
            .clone()
    }

    /// Takes a live fs_struct reference without treating an exiting task's
    /// already-retired slot as a kernel invariant violation.
    pub(crate) fn try_fs_context(&self) -> Option<Arc<Mutex<FsContext>>> {
        self.fs_context
            .lock()
            .as_ref()
            .map(|slot| slot.context.clone())
    }

    /// Acquires one Linux task ownership of this `fs_struct`.
    pub(crate) fn fs_context_for_child(&self) -> Arc<FsContextSlot> {
        FsContextSlot::share_for_task(self.fs_context.lock().as_ref().expect("retired fs_struct"))
    }

    pub(crate) fn fd_table(&self) -> Arc<crate::file::FdTable> {
        self.fd_table
            .lock()
            .as_ref()
            .expect("retired files_struct")
            .table
            .clone()
    }

    /// Pins this task's files table if it has not crossed final files_struct
    /// retirement.  Cross-task inspection must use this fallible form: a live
    /// task-table reference can race exit after lookup, and that race is an
    /// ordinary missing-FD result rather than a kernel invariant failure.
    pub(crate) fn try_fd_table(&self) -> Option<Arc<crate::file::FdTable>> {
        self.fd_table.lock().as_ref().map(|slot| slot.table.clone())
    }
    pub(crate) fn fd_table_is_shared(&self) -> bool {
        self.fd_table
            .lock()
            .as_ref()
            .expect("retired files_struct")
            .task_users
            .load(Ordering::Relaxed)
            != 1
    }
    pub(crate) fn fd_table_for_child(&self) -> Arc<FdTableSlot> {
        FdTableSlot::share_for_task(self.fd_table.lock().as_ref().expect("retired files_struct"))
    }
    pub(crate) fn try_clone_fd_table_if_shared(&self) -> AxResult<Option<Arc<FdTableSlot>>> {
        let table = {
            let slot = self.fd_table.lock();
            (slot
                .as_ref()
                .expect("retired files_struct")
                .task_users
                .load(Ordering::Relaxed)
                != 1)
                .then(|| slot.as_ref().expect("retired files_struct").table.clone())
        };
        table
            .map(|table| {
                Arc::try_new(FdTableSlot {
                    table: Arc::try_new(table.fork_copy()?).map_err(|_| AxError::NoMemory)?,
                    task_users: AtomicUsize::new(0),
                })
                .map_err(|_| AxError::NoMemory)
            })
            .transpose()
    }
    pub(crate) fn replace_fd_table(&self, replacement: Arc<FdTableSlot>) -> Arc<FdTableSlot> {
        replacement.acquire_task();
        let old = (*self.fd_table.lock())
            .replace(replacement)
            .expect("retired files_struct");
        old.release_task();
        old
    }
    /// Retire the Linux task ownership at the authoritative task-unhash edge.
    /// Takes the exact task-owned table at authoritative unlink.  Subsequent
    /// accesses are invalid, and dropping the returned Arc performs final
    /// descriptor/resource close when it was the last owner.
    pub(crate) fn retire_fd_table(&self) -> Arc<FdTableSlot> {
        let slot = self
            .fd_table
            .lock()
            .take()
            .expect("files_struct retired twice");
        slot.release_task();
        slot
    }

    /// Creates a private `fs_struct` only when this task's slot actually
    /// shares one. The count is checked before making a local Arc clone, so
    /// an already-private `unshare(CLONE_FS)` needs no allocation.
    pub(crate) fn try_clone_fs_context_if_shared(&self) -> AxResult<Option<Arc<FsContextSlot>>> {
        let cloned = {
            let fs_context = self.fs_context.lock();
            let fs_context = fs_context.as_ref().expect("retired fs_struct");
            if fs_context.task_users.load(Ordering::Relaxed) == 1 {
                None
            } else {
                Some(fs_context.context.lock().clone())
            }
        };
        cloned
            .map(|context| {
                Arc::try_new(FsContextSlot {
                    context: Arc::try_new(Mutex::new(context)).map_err(|_| AxError::NoMemory)?,
                    task_users: AtomicUsize::new(0),
                })
                .map_err(|_| AxError::NoMemory)
            })
            .transpose()
    }

    pub(crate) fn fs_context_is_shared(&self) -> bool {
        self.fs_context
            .lock()
            .as_ref()
            .expect("retired fs_struct")
            .task_users
            .load(Ordering::Relaxed)
            != 1
    }

    /// Prepares a private fs_struct whose root/cwd already point into a mount
    /// namespace being entered.  No task slot is replaced here: setns can
    /// therefore complete every fallible path-resolution update before its
    /// namespace/credential commit becomes visible.
    pub(crate) fn prepare_fs_context_for_mount_namespace(
        &self,
        root: axfs_ng_vfs::Location,
    ) -> AxResult<Arc<FsContextSlot>> {
        let mut context = self.fs_context().lock().clone();
        context.set_root_dir(root.clone())?;
        context.set_current_dir(root)?;
        Arc::try_new(FsContextSlot {
            context: Arc::try_new(Mutex::new(context)).map_err(|_| AxError::NoMemory)?,
            task_users: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Rebinds the caller's root and cwd to the corresponding dentries in a
    /// freshly cloned mount tree.  CLONE_NEWNS duplicates the topology; it
    /// does not perform chroot(2) or chdir(2), so the two paths must be
    /// preserved independently.
    pub(crate) fn prepare_fs_context_for_cloned_mount_namespace(
        &self,
        namespace_root: axfs_ng_vfs::Location,
    ) -> AxResult<Arc<FsContextSlot>> {
        let fs_context = self.fs_context();
        let source = fs_context.lock();
        let root_path = source.root_dir().absolute_path()?;
        let cwd_path = source.current_dir().absolute_path()?;
        let namespace_view = FsContext::new(namespace_root);
        let root = namespace_view.resolve(&root_path)?;
        let cwd = namespace_view.resolve(&cwd_path)?;
        let mut context = source.clone();
        drop(source);
        context.set_root_dir(root)?;
        context.set_current_dir(cwd)?;
        Arc::try_new(FsContextSlot {
            context: Arc::try_new(Mutex::new(context)).map_err(|_| AxError::NoMemory)?,
            task_users: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Replaces this task's `fs_struct`, used by `unshare(CLONE_FS)`.
    pub(crate) fn replace_fs_context(&self, replacement: Arc<FsContextSlot>) -> Arc<FsContextSlot> {
        replacement.acquire_task();
        let old = (*self.fs_context.lock())
            .replace(replacement)
            .expect("retired fs_struct");
        old.release_task();
        old
    }

    /// Publish a prepared mount-namespace proxy and its already validated
    /// fs_struct under the one gate observers use for root/cwd replacement.
    /// There is no fallible operation after this gate is acquired.
    pub(crate) fn commit_namespace_with_fs_context(
        &self,
        prepared: super::process::PreparedNamespaceProxyReplacement,
        replacement: Arc<FsContextSlot>,
    ) -> Arc<FsContextSlot> {
        let (old_proxy, old_fs) = {
            let _publication = super::fs_context_publication();
            let old_proxy = prepared.commit_under_publication(self);
            let old_fs = self.replace_fs_context(replacement);
            (old_proxy, old_fs)
        };
        drop(old_proxy);
        old_fs
    }

    /// Removes the exact task owner's fs_struct at authoritative task unlink.
    pub(crate) fn retire_fs_context(&self) -> Arc<FsContextSlot> {
        let slot = self
            .fs_context
            .lock()
            .take()
            .expect("fs_struct retired twice");
        slot.release_task();
        slot
    }

    /// Returns the shared I/O-priority context used by `CLONE_IO`, if Linux
    /// has already allocated one for this task.
    pub(crate) fn io_context(&self) -> Option<Arc<AtomicU16>> {
        self.io_context.lock().clone()
    }

    pub(crate) fn io_flusher(&self) -> bool {
        self.io_flusher.load(Ordering::Acquire)
    }

    pub(crate) fn set_io_flusher(&self, enabled: bool) {
        self.io_flusher.store(enabled, Ordering::Release);
    }

    pub(crate) fn mce_kill_policy(&self) -> u8 {
        self.mce_kill_policy.load(Ordering::Acquire)
    }

    pub(crate) fn set_mce_kill_policy(&self, policy: u8) {
        self.mce_kill_policy.store(policy, Ordering::Release);
    }

    /// Returns the raw Linux `ioprio` value stored for this task.
    pub(crate) fn io_priority_raw(&self) -> u16 {
        self.io_context
            .lock()
            .as_ref()
            .map(|context| context.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Publishes a raw Linux `ioprio` value, allocating the Linux context only
    /// on the first setter that needs one.
    pub(crate) fn set_io_priority_raw(&self, priority: u16) -> AxResult<()> {
        let mut io_context = self.io_context.lock();
        if let Some(context) = io_context.as_ref() {
            context.store(priority, Ordering::Release);
        } else {
            let new_context =
                Arc::try_new(AtomicU16::new(priority)).map_err(|_| AxError::NoMemory)?;
            *io_context = Some(new_context);
        }
        Ok(())
    }

    /// Takes the fork-visible x86 I/O-port snapshot. The bitmap Arc is COW.
    pub(crate) fn ioport_snapshot(&self) -> IoPortState {
        self.ioport.lock().clone()
    }

    /// Installs an inherited x86 I/O-port snapshot before the child is
    /// runnable. This preserves Linux's fork semantics without sharing later
    /// mutations between parent and child.
    pub(crate) fn install_ioport_snapshot(&self, state: IoPortState) {
        *self.ioport.lock() = state;
    }

    /// Applies one already-validated `ioperm(2)` request to this task.
    pub(crate) fn update_ioperm(&self, from: usize, num: usize, turn_on: bool) -> AxResult<()> {
        self.ioport.lock().try_update_ioperm(from, num, turn_on)
    }

    pub(crate) fn iopl_level(&self) -> u8 {
        self.ioport.lock().iopl
    }

    pub(crate) fn set_iopl_level(&self, level: u8) {
        self.ioport.lock().iopl = level;
    }

    /// Refreshes the current CPU's TSS immediately before returning to ring 3.
    /// The caller holds the final user-return IRQ/preemption exclusion.
    pub(crate) fn install_user_io_permissions(&self) {
        let state = self.ioport.lock();
        let (bitmap, revoked) = state.bitmap_and_revocations();
        ioport::install_user_io_bitmap(bitmap, revoked, state.iopl == 3);
    }

    /// Updates the current thread's two PKRU access bits for one allocated
    /// x86 protection key.  PKRU is per-thread (unlike the mm allocation
    /// bitmap), so this deliberately does not change sibling threads.
    pub(crate) fn set_pkey_access_rights(&self, key: u8, rights: u32) -> AxResult<()> {
        if key == 0 || key >= 16 || rights & !0x3 != 0 {
            return Err(AxError::InvalidInput);
        }
        let shift = u32::from(key) * 2;
        let old = axtask::current_task_pkru();
        let new = (old & !(0x3 << shift)) | (rights << shift);
        axtask::set_current_task_pkru(new);
        Ok(())
    }

    pub(crate) fn deferred_work_account(&self) -> Arc<DeferredWorkAccount> {
        self.deferred_work.clone()
    }

    /// Stable weak access to this task's sole credential publication slot.
    /// Used by thread pidfds so de-threading or numeric TID reuse cannot change
    /// the credential object they name.
    pub(crate) fn credential_slot_weak(&self) -> Weak<CredentialSlot> {
        Arc::downgrade(&self.credential)
    }

    pub(in crate::task) fn credential_slot(&self) -> Arc<CredentialSlot> {
        self.credential.clone()
    }

    pub(crate) fn file_operation_credential(&self) -> Option<Arc<Cred>> {
        self.file_operation_credential.lock().clone()
    }

    pub(crate) fn replace_file_operation_credential(
        &self,
        replacement: Option<Arc<Cred>>,
    ) -> Option<Arc<Cred>> {
        core::mem::replace(&mut *self.file_operation_credential.lock(), replacement)
    }

    pub(crate) fn file_write_credentials(&self) -> Option<OpenCredentials> {
        *self.file_write_credentials.lock()
    }

    pub(crate) fn replace_file_write_credentials(
        &self,
        replacement: Option<OpenCredentials>,
    ) -> Option<OpenCredentials> {
        core::mem::replace(&mut *self.file_write_credentials.lock(), replacement)
    }

    /// Get the clear child tid field.
    pub fn clear_child_tid(&self) -> usize {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    /// Installs the one-shot `CLONE_CHILD_SETTID` address before publication.
    pub(crate) fn set_child_tid_address(&self, set_child_tid: usize) {
        self.set_child_tid.store(set_child_tid, Ordering::Release);
    }

    /// Takes the one-shot child-TID publication action on first task entry.
    pub(crate) fn take_child_tid_address(&self) -> usize {
        self.set_child_tid.swap(0, Ordering::AcqRel)
    }

    /// Get the user-visible thread ID.
    pub fn tid(&self) -> Pid {
        self.visible_tid.load(Ordering::Acquire)
    }

    /// Whether this task currently owns the Linux-visible thread-group ID.
    ///
    /// Scheduler task IDs are an internal allocation detail and can differ
    /// from the visible TID, including for PID 1 and after de-threading exec.
    pub(crate) fn is_thread_group_leader(&self) -> bool {
        self.tid() == self.proc_data.proc.pid()
    }

    /// Returns the immutable scheduler/core TID used for membership and
    /// identity revalidation. Numeric visible-TID rebinding never changes it.
    pub(crate) const fn kernel_tid(&self) -> Pid {
        self.kernel_tid
    }

    /// Set the clear child tid field.
    pub fn set_clear_child_tid(&self, clear_child_tid: usize) {
        self.clear_child_tid
            .store(clear_child_tid, Ordering::Relaxed);
    }

    /// Set the user-visible thread ID.
    pub fn set_tid(&self, tid: Pid) {
        self.visible_tid.store(tid, Ordering::Release);
    }

    pub(crate) fn pdeath_signal(&self) -> u32 {
        self.task_parent.pdeath_signal.load(Ordering::Acquire)
    }

    pub(crate) fn set_pdeath_signal(&self, signo: u32) {
        self.task_parent
            .pdeath_signal
            .store(signo, Ordering::Release);
    }

    pub(in crate::task) fn pdeath_signal_state(&self) -> &AtomicU32 {
        &self.task_parent.pdeath_signal
    }

    pub(crate) fn task_parent_node(&self) -> &Arc<TaskParentNode> {
        &self.task_parent
    }

    /// Publishes this task's exact Linux real-parent relation. Clone invokes
    /// this after every fallible admission and before making the task visible
    /// in any lookup table. The operation only mutates preallocated intrusive
    /// links and cannot fail or allocate.
    pub(crate) fn publish_task_parent(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
        choice: TaskParentChoice,
    ) {
        publish_task_parent_relation(publication, &self.task_parent, choice)
    }

    pub(crate) fn task_parent_snapshot(&self) -> Option<TaskParentSnapshot> {
        let publication = lock_task_parent_publication();
        task_parent_security_snapshot(&publication, &self.task_parent)
    }

    /// Revalidates both the exact parent relation and the immutable credential
    /// object sampled for a security hook. The second relation check closes a
    /// reparent window around the lock-free credential comparison.
    pub(crate) fn task_parent_security_snapshot_matches(
        &self,
        snapshot: &TaskParentSnapshot,
    ) -> bool {
        let publication = lock_task_parent_publication();
        task_parent_security_snapshot_matches(&publication, &self.task_parent, snapshot)
    }

    pub(crate) fn task_parent_security_snapshot_matches_locked(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
        snapshot: &TaskParentSnapshot,
    ) -> bool {
        task_parent_security_snapshot_matches(publication, &self.task_parent, snapshot)
    }

    /// Nonblockingly pins the exact parent's credential after revalidating the
    /// relation under an already-held graph publication gate.
    pub(crate) fn try_lock_task_parent_security_snapshot<'a>(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
        snapshot: &'a TaskParentSnapshot,
    ) -> TaskParentCredentialPin<'a> {
        try_lock_task_parent_security_snapshot(publication, &self.task_parent, snapshot)
    }

    pub(crate) fn reparent_task_parent_children_matching(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
        replacement: &Arc<TaskParentNode>,
        matches: impl FnMut(&Arc<TaskParentNode>) -> bool,
        deliver: impl FnMut(Arc<TaskParentNode>, u32),
    ) {
        reparent_task_parent_children_matching(
            publication,
            &self.task_parent,
            replacement,
            matches,
            deliver,
        )
    }

    /// Marks a final process task dead only after authoritative core batches
    /// have handed off every exact child. A remaining child is an invariant
    /// failure; this method never invents a fallback reaper.
    pub(crate) fn finish_task_parent_exit(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
    ) -> bool {
        finish_task_parent_exit(publication, &self.task_parent)
    }

    pub(crate) fn exit_task_parent(
        &self,
        publication: &TaskParentPublicationGuard<'_>,
        primary_reaper: Option<Arc<TaskParentNode>>,
        fallback_reaper: Option<Arc<TaskParentNode>>,
        deliver: impl FnMut(Arc<TaskParentNode>, u32),
    ) {
        exit_task_parent_relation(
            publication,
            &self.task_parent,
            primary_reaper,
            fallback_reaper,
            deliver,
        )
    }

    /// Temporarily releases the active-scope read lock so the current thread
    /// can mutate its process scope, then restores the active scope binding.
    pub fn with_mut_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> R {
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        ActiveScope::set_global();
        self.release_active_scope_read();

        let result = {
            let mut scope = self.proc_data.scope.write();
            f(&mut scope)
        };

        self.acquire_active_scope_read();

        result
    }

    /// Get the robust list head.
    pub fn robust_list_head(&self) -> usize {
        self.robust_list_head.load(Ordering::SeqCst)
    }

    /// Set the robust list head.
    pub fn set_robust_list_head(&self, robust_list_head: usize) {
        self.robust_list_head
            .store(robust_list_head, Ordering::SeqCst);
    }

    /// Get the oom score adjustment value.
    pub fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj.load(Ordering::SeqCst)
    }

    /// Set the oom score adjustment value.
    pub fn set_oom_score_adj(&self, value: i32) {
        self.oom_score_adj.store(value, Ordering::SeqCst);
    }

    /// Check if the thread is ready to exit.
    pub fn pending_exit(&self) -> bool {
        self.exit.load(Ordering::Acquire)
    }

    /// Set the thread to exit.
    pub fn set_exit(&self) {
        // Final scheduler state is sampled before zombie publication. Keep
        // this terminal flag idempotent and free of scheduler mutations: it
        // is also used by non-final thread exits and may be called again by
        // defensive teardown paths.
        self.exit.store(true, Ordering::Release);
        // CPU-clock sleeps retain an Arc to their target.  Its accounting can
        // no longer make progress after this transition, so wake waiters to
        // observe the terminal lifetime state instead of leaving them parked
        // on an unreachable CPU deadline.
        super::notify_cpu_clock_sleepers();
    }

    /// Returns the last published CPU usage snapshot for this thread.
    pub fn usage_snapshot(&self) -> TaskUsage {
        let mut usage = self.live_usage.snapshot();
        usage.minflt = self.minor_faults.load(Ordering::Acquire);
        usage.majflt = self.major_faults.load(Ordering::Acquire);
        usage.inblock = self.io_read_bytes.load(Ordering::Acquire) >> 9;
        usage.oublock = self.io_write_bytes.load(Ordering::Acquire) >> 9;
        usage.nvcsw = self.voluntary_switches.load(Ordering::Acquire);
        usage.nivcsw = self.involuntary_switches.load(Ordering::Acquire);
        usage
    }

    /// Publishes a CPU usage snapshot for lock-free readers such as procfs.
    pub fn store_usage_snapshot(&self, usage: TaskUsage) {
        self.live_usage.store(usage);
        // Publish after the atomic accounting snapshot so CPU-clock sleepers
        // never wake solely to observe the preceding value.  Keeping this in
        // the common publisher also covers explicit scheduler timer polls.
        super::notify_cpu_clock_sleepers();
    }

    /// Accounts one successfully handled minor page fault.
    pub(crate) fn account_minor_fault(&self) {
        self.minor_faults.fetch_add(1, Ordering::Relaxed);
        self.perf_on_minor_fault();
    }

    /// Accounts one successfully handled major page fault.
    pub(crate) fn account_major_fault(&self) {
        self.major_faults.fetch_add(1, Ordering::Relaxed);
        self.perf_on_major_fault();
    }

    /// Accounts bytes transferred by a real backing read, before conversion
    /// to Linux's 512-byte block units.
    pub(crate) fn account_backing_read(&self, bytes: usize) {
        self.io_read_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Returns the completed backing-read total used to classify the page
    /// fault that just ran.  A major fault is only possible when this value
    /// increases during the backend population transaction.
    pub(crate) fn backing_read_bytes(&self) -> u64 {
        self.io_read_bytes.load(Ordering::Acquire)
    }

    /// Accounts a successfully populated page after backend I/O has finished.
    /// Cache hits, tmpfs, and anonymous/COW population therefore remain minor.
    pub(crate) fn account_resolved_page_fault(&self, read_before: u64) {
        if self.backing_read_bytes() > read_before {
            self.account_major_fault();
        } else {
            self.account_minor_fault();
        }
    }

    /// Accounts bytes transferred by a real backing write.
    pub(crate) fn account_backing_write(&self, bytes: usize) {
        self.io_write_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Accounts a context switch at the scheduler's switch-out edge.
    pub(crate) fn account_context_switch(&self, voluntary: bool) {
        let counter = if voluntary {
            &self.voluntary_switches
        } else {
            &self.involuntary_switches
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Accounts the final voluntary switch before freezing an exiting task's
    /// usage.  `TaskExt::on_leave(SwitchReason::Exit)` consumes the marker
    /// instead of incrementing the same counter again.
    pub(crate) fn preaccount_exit_context_switch(&self) {
        let was_set = self.exit_switch_preaccounted.swap(true, Ordering::AcqRel);
        debug_assert!(!was_set, "exit context switch was pre-accounted twice");
        self.account_context_switch(true);
    }

    /// Returns the current procfs state hint.
    pub(crate) fn proc_state_hint(&self) -> ProcStateHint {
        ProcStateHint::from(self.proc_state_hint.load(Ordering::Acquire))
    }

    /// Replaces the current procfs state hint and returns the previous value.
    pub(crate) fn swap_proc_state_hint(&self, hint: ProcStateHint) -> ProcStateHint {
        let previous = ProcStateHint::from(self.proc_state_hint.swap(hint as u8, Ordering::AcqRel));
        super::loadavg::account_uninterruptible_transition(
            previous == ProcStateHint::Uninterruptible,
            hint == ProcStateHint::Uninterruptible,
        );
        previous
    }

    /// Restores the procfs state hint.
    pub(crate) fn set_proc_state_hint(&self, hint: ProcStateHint) {
        let previous = ProcStateHint::from(self.proc_state_hint.swap(hint as u8, Ordering::AcqRel));
        super::loadavg::account_uninterruptible_transition(
            previous == ProcStateHint::Uninterruptible,
            hint == ProcStateHint::Uninterruptible,
        );
    }

    /// Enters one nested I/O-wait interval.  Only the outermost holder owns
    /// elapsed-time accounting; inner readiness retries merely extend it.
    pub(crate) fn enter_iowait(&self) {
        if self.iowait_depth.fetch_add(1, Ordering::AcqRel) == 0 {
            self.iowait_started_ns
                .store(axhal::time::monotonic_time_nanos(), Ordering::Release);
        }
    }

    /// Leaves an I/O-wait interval without permitting a stale wake/exit path
    /// to underflow the nested counter.  The total is saturating because it is
    /// an observational scheduler/accounting value.
    pub(crate) fn leave_iowait(&self) {
        let mut depth = self.iowait_depth.load(Ordering::Acquire);
        loop {
            if depth == 0 {
                return;
            }
            match self.iowait_depth.compare_exchange_weak(
                depth,
                depth - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(1) => {
                    let start = self.iowait_started_ns.swap(0, Ordering::AcqRel);
                    let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
                    self.iowait_total_ns
                        .try_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                            Some(total.saturating_add(elapsed))
                        })
                        .ok();
                    return;
                }
                Ok(_) => return,
                Err(observed) => depth = observed,
            }
        }
    }

    /// Exposes scheduler I/O-wait state to proc/scheduler accounting readers.
    pub(crate) fn iowait_accounting(&self) -> (u32, u64) {
        (
            self.iowait_depth.load(Ordering::Acquire),
            self.iowait_total_ns.load(Ordering::Acquire),
        )
    }

    pub(crate) fn enter_cgroup_freezer(&self) {
        if self.proc_data.cgroup_freeze_requested()
            && self
                .cgroup_freezer_parked
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.proc_data.enter_cgroup_freezer();
        }
    }

    pub(crate) fn leave_cgroup_freezer(&self) {
        if self.cgroup_freezer_parked.swap(false, Ordering::AcqRel) {
            self.proc_data.leave_cgroup_freezer();
        }
    }

    fn pause_cpu_accounting_for_switch(&self) {
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let usage = {
            let mut time = self.time.borrow_mut();
            time.pause_for_switch(&self.proc_data);
            let (utime, stime) = time.output();
            TaskUsage::from_time_values(utime, stime)
        };
        self.store_usage_snapshot(usage);
        if let Some(cpu) = request_process_cpu_evaluation(&self.proc_data) {
            crate::deferred_work::wake_process_timer_worker(cpu);
        }
    }

    fn poll_cpu_accounting_for_tick(&self) -> bool {
        let usage = {
            // The timer IRQ and scheduler hooks already run IRQ-off. Every
            // task-context accounting borrow uses the same guard, so this
            // RefCell cannot be reentered or carried across a switch.
            let mut time = self.time.borrow_mut();
            time.poll_timer_tick(&self.proc_data);
            let (utime, stime) = time.output();
            TaskUsage::from_time_values(utime, stime)
        };
        self.store_usage_snapshot(usage);
        let work_pending = request_process_cpu_evaluation(&self.proc_data);
        if let Some(cpu) = work_pending {
            crate::deferred_work::wake_process_timer_worker(cpu);
        }
        work_pending.is_some()
    }

    fn resume_cpu_accounting_after_switch(&self) {
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let mut time = self.time.borrow_mut();
        time.resume_after_switch(&self.proc_data);
        let (utime, stime) = time.output();
        self.store_usage_snapshot(TaskUsage::from_time_values(utime, stime));
    }

    fn acquire_active_scope_read(&self) {
        let already_held = self.active_scope_read_held.swap(true, Ordering::AcqRel);
        let scope = self.proc_data.scope.read();
        // SAFETY: bind the task-local active scope to this process scope. When
        // this is a fresh acquire, keep the read guard alive until the matching
        // release forcefully decrements it. If a scheduler edge calls enter
        // twice, the existing leaked guard keeps the pointer valid and this
        // temporary guard is dropped normally.
        unsafe { ActiveScope::set(&scope) };
        if !already_held {
            core::mem::forget(scope);
        }
    }

    fn release_active_scope_read(&self) {
        if self.active_scope_read_held.swap(false, Ordering::AcqRel) {
            // SAFETY: guarded by active_scope_read_held, which is set only
            // after acquire_active_scope_read leaks exactly one read guard.
            unsafe { self.proc_data.scope.force_read_decrement() };
        }
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        crate::syscall::release_registered_ring_fds(self.kernel_tid() as u64);
        // Task teardown is a lifecycle edge, not a deferred scheduler hint.
        // A surviving descriptor (or inherited child-only file) may still
        // retain a group Arc, so force its exact placement through the same
        // generation/ack reconcile protocol before dropping task ownership.
        for group in self.perf_events.get_mut().iter() {
            group.reconcile_last_descriptor();
        }
        self.leave_cgroup_freezer();
        if let Some(slot) = self.fs_context.get_mut().take() {
            slot.release_task();
        }
        if let Some(slot) = self.fd_table.get_mut().take() {
            slot.release_task();
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProcStateHint {
    None            = 0,
    Interruptible   = 1,
    Uninterruptible = 2,
    /// Interruptible readiness sleep accounted as I/O wait.
    IoWait          = 3,
}

impl From<u8> for ProcStateHint {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Interruptible,
            2 => Self::Uninterruptible,
            3 => Self::IoWait,
            _ => Self::None,
        }
    }
}

#[extern_trait]
impl TaskExt for Box<Thread> {
    fn on_enter(&self, _task: &TaskInner) {
        self.perf_on_enter();
        let cpu = axhal::percpu::this_cpu_id();
        let previous_cpu = self.perf_last_cpu.swap(cpu, Ordering::AcqRel);
        if previous_cpu != usize::MAX && previous_cpu != cpu {
            self.perf_on_migration();
        }
        let state = self.proc_data.aspace_tlb_state();
        state.enter_current();
        // A scheduler enter is the migration observation consumed by the
        // final IRQ-disabled user-return gate.  The event publication is
        // allocation-free and intentionally best-effort while a lifecycle
        // transaction owns the rseq state.
        let _ = self.notify_rseq(thekernel_linux_rseq::RseqEventMask::MIGRATE);
        self.acquire_active_scope_read();
        self.resume_cpu_accounting_after_switch();
    }

    fn on_switch(
        &self,
        _task: &TaskInner,
        peer: &TaskInner,
        switch_out: bool,
        _reason: SwitchReason,
    ) {
        let peer_thread = peer.try_as_thread();
        let peer_pid = peer_thread.map_or(0, |thread| thread.proc_data.proc.pid() as u32);
        let peer_tid = peer_thread.map_or(0, |_| peer.id().as_u64() as u32);
        self.perf_emit_switch(switch_out, Some((peer_pid, peer_tid)));
    }

    fn on_leave(&self, task: &TaskInner, reason: SwitchReason) {
        let _ = task;
        // Every scheduler leave is a preemption observation.  The final
        // return gate decides whether the saved IP was in an active critical
        // section and performs any abort before user entry.
        let _ = self.notify_rseq(thekernel_linux_rseq::RseqEventMask::PREEMPT);
        let exit_was_preaccounted = reason == SwitchReason::Exit
            && self.exit_switch_preaccounted.swap(false, Ordering::AcqRel);
        if reason.counts_as_context_switch() && !exit_was_preaccounted {
            self.account_context_switch(!reason.is_involuntary());
        }
        self.perf_on_leave();
        if reason == SwitchReason::Exit {
            self.perf_emit_exit();
        }
        self.pause_cpu_accounting_for_switch();
        ActiveScope::set_global();
        self.release_active_scope_read();
    }

    fn on_ready_wake(&self, _task: &TaskInner) {
        // A readiness waker is about to publish Blocked -> Ready.  Clear the
        // D-state at that same wake edge, before the scheduler exposes R.
        if self.proc_state_hint() == ProcStateHint::Uninterruptible {
            self.set_proc_state_hint(ProcStateHint::None);
        }
    }

    fn on_timer_tick(&self, _task: &TaskInner) -> bool {
        super::load_average_sample_now();
        self.poll_cpu_accounting_for_tick()
    }
}

/// Helper trait to access the thread from a task.
pub trait AsThread {
    /// Try to get the thread from the task.
    fn try_as_thread(&self) -> Option<&Thread>;

    /// Get the thread from the task, panicking if it is a kernel task.
    fn as_thread(&self) -> &Thread {
        self.try_as_thread().expect("kernel task")
    }
}

impl AsThread for TaskInner {
    fn try_as_thread(&self) -> Option<&Thread> {
        self.task_ext()
            .map(|ext| ext.downcast_ref::<Box<Thread>>().as_ref())
    }
}

#[cfg(all(test, feature = "hwp-uclamp"))]
mod scheduler_clamp_tests {
    use super::{
        SchedulerClampCache, pack_sched_clamp, scheduler_commit_is_newer_or_equal,
        unpack_sched_clamp,
    };

    #[test]
    fn clamp_pack_round_trips_scheduler_boundaries() {
        for (min, max) in [(0, 1024), (0, 0), (1, 1023), (1024, 1024)] {
            assert_eq!(unpack_sched_clamp(pack_sched_clamp(min, max)), (min, max));
        }
    }

    #[test]
    fn scheduler_commit_versions_handle_wrap_and_reject_stale_updates() {
        assert!(scheduler_commit_is_newer_or_equal(8, 7));
        assert!(scheduler_commit_is_newer_or_equal(0, u64::MAX));
        assert!(!scheduler_commit_is_newer_or_equal(7, 8));
        assert!(!scheduler_commit_is_newer_or_equal(u64::MAX, 0));
    }

    #[test]
    fn stale_clamp_publication_cannot_regress_a_newer_commit() {
        let cache = SchedulerClampCache::new(0, 1024, 5);
        cache.publish(200, 700, 6);
        cache.publish(10, 20, 5);
        assert_eq!(cache.snapshot(), (200, 700, 6));
    }
}

#[cfg(test)]
mod ioport_tests {
    use axerrno::AxError;

    use super::IoPortState;

    #[test]
    fn shared_ioperm_revoke_needs_no_allocation() {
        let mut parent = IoPortState::default();
        parent.try_update_ioperm(7, 2, true).unwrap();
        let mut child = parent.clone();

        assert!(parent.bitmap.is_some());
        assert!(child.bitmap.is_some());
        assert!(parent.bitmap.as_ref().unwrap()[0] & 0x80 == 0);
        assert!(parent.bitmap.as_ref().unwrap()[1] & 0x01 == 0);

        // A fork-shared map previously forced an Arc allocation to revoke a
        // permission. This injected allocator always fails; revoke must not
        // invoke it and must leave the parent's permissions untouched.
        child
            .try_update_ioperm_with(7, 2, false, |_| Err(AxError::NoMemory))
            .unwrap();
        assert!(child.bitmap.is_some());
        assert!(child.revoked[0] & 0x80 != 0);
        assert!(child.revoked[1] & 0x01 != 0);
        assert!((child.bitmap.as_ref().unwrap()[0] | child.revoked[0]) & 0x80 != 0);
        assert!((child.bitmap.as_ref().unwrap()[1] | child.revoked[1]) & 0x01 != 0);
        assert!(parent.bitmap.as_ref().unwrap()[0] & 0x80 == 0);
        assert!(parent.bitmap.as_ref().unwrap()[1] & 0x01 == 0);
    }
}

#[cfg(test)]
mod task_parent_tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;
    use crate::task::UserNamespace;

    fn node(tid: Pid) -> Arc<TaskParentNode> {
        TaskParentNode::try_new(tid, Weak::new(), Weak::new()).unwrap()
    }

    fn credential_node(tid: Pid, slot: &Arc<CredentialSlot>) -> Arc<TaskParentNode> {
        TaskParentNode::try_new(tid, Weak::new(), Arc::downgrade(slot)).unwrap()
    }

    fn publish_task_parent_relation(child: &Arc<TaskParentNode>, choice: TaskParentChoice) {
        let publication = lock_task_parent_publication();
        super::publish_task_parent_relation(&publication, child, choice)
    }

    fn task_parent_node_snapshot(node: &Arc<TaskParentNode>) -> Option<Arc<TaskParentNode>> {
        let publication = lock_task_parent_publication();
        super::task_parent_node_snapshot(&publication, node)
    }

    fn task_parent_node_matches(node: &Arc<TaskParentNode>, parent: &Arc<TaskParentNode>) -> bool {
        let publication = lock_task_parent_publication();
        super::task_parent_node_matches(&publication, node, parent)
    }

    fn task_parent_security_snapshot(node: &Arc<TaskParentNode>) -> Option<TaskParentSnapshot> {
        let publication = lock_task_parent_publication();
        super::task_parent_security_snapshot(&publication, node)
    }

    fn task_parent_security_snapshot_matches(
        node: &Arc<TaskParentNode>,
        snapshot: &TaskParentSnapshot,
    ) -> bool {
        let publication = lock_task_parent_publication();
        super::task_parent_security_snapshot_matches(&publication, node, snapshot)
    }

    fn lock_task_parent_security_snapshot<'a>(
        node: &Arc<TaskParentNode>,
        snapshot: &'a TaskParentSnapshot,
    ) -> Option<CredentialSnapshotGuard<'a>> {
        let publication = lock_task_parent_publication();
        match super::try_lock_task_parent_security_snapshot(&publication, node, snapshot) {
            TaskParentCredentialPin::Pinned(guard) => Some(guard),
            TaskParentCredentialPin::Busy | TaskParentCredentialPin::Stale => None,
        }
    }

    fn exit_task_parent_relation(
        node: &Arc<TaskParentNode>,
        primary_reaper: Option<Arc<TaskParentNode>>,
        fallback_reaper: Option<Arc<TaskParentNode>>,
        deliver: impl FnMut(Arc<TaskParentNode>, u32),
    ) {
        let publication = lock_task_parent_publication();
        super::exit_task_parent_relation(
            &publication,
            node,
            primary_reaper,
            fallback_reaper,
            deliver,
        )
    }

    fn retire(node: &Arc<TaskParentNode>) {
        exit_task_parent_relation(node, None, None, |_, _| {});
    }

    #[test]
    fn exact_parent_publication_distinguishes_caller_and_inherited_parent() {
        let grandparent = node(101);
        let caller = node(102);
        let direct_child = node(103);
        let inherited_child = node(104);

        publish_task_parent_relation(&caller, TaskParentChoice::Caller(grandparent.clone()));
        publish_task_parent_relation(&direct_child, TaskParentChoice::Caller(caller.clone()));
        publish_task_parent_relation(&inherited_child, TaskParentChoice::Inherit(caller.clone()));

        let direct = task_parent_node_snapshot(&direct_child).unwrap();
        let inherited = task_parent_node_snapshot(&inherited_child).unwrap();
        assert!(Arc::ptr_eq(&direct, &caller));
        assert!(Arc::ptr_eq(&inherited, &grandparent));

        retire(&direct_child);
        retire(&inherited_child);
        retire(&caller);
        retire(&grandparent);
    }

    #[test]
    fn parent_thread_exit_notifies_and_reparents_to_live_sibling() {
        let parent = node(201);
        let sibling = node(202);
        let child = node(203);
        child.pdeath_signal.store(12, Ordering::Release);
        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));

        let mut delivered = Vec::new();
        exit_task_parent_relation(&parent, Some(sibling.clone()), None, |target, signo| {
            delivered.push((target, signo));
        });
        assert_eq!(delivered.len(), 1);
        assert!(Arc::ptr_eq(&delivered[0].0, &child));
        assert_eq!(delivered[0].1, 12);
        assert!(Arc::ptr_eq(
            &task_parent_node_snapshot(&child).unwrap(),
            &sibling
        ));

        delivered.clear();
        exit_task_parent_relation(&sibling, None, None, |target, signo| {
            delivered.push((target, signo));
        });
        assert_eq!(delivered.len(), 1);
        assert!(Arc::ptr_eq(&delivered[0].0, &child));
        assert_eq!(delivered[0].1, 12);
        assert!(task_parent_node_snapshot(&child).is_none());

        retire(&child);
    }

    #[test]
    fn authoritative_batch_reparents_only_the_supplied_exact_child() {
        let parent = node(211);
        let other_parent = node(212);
        let reaper = node(213);
        let child = node(214);
        let unrelated = node(215);
        child.pdeath_signal.store(15, Ordering::Release);

        let publication = lock_task_parent_publication();
        super::publish_task_parent_relation(
            &publication,
            &child,
            TaskParentChoice::Caller(parent.clone()),
        );
        super::publish_task_parent_relation(
            &publication,
            &unrelated,
            TaskParentChoice::Caller(other_parent.clone()),
        );
        assert!(!super::finish_task_parent_exit(&publication, &parent));
        let mut delivered = None;
        super::reparent_task_parent_children_matching(
            &publication,
            &parent,
            &reaper,
            |candidate| Arc::ptr_eq(candidate, &child),
            |candidate, signo| delivered = Some((candidate, signo)),
        );
        let (delivered_child, delivered_signo) = delivered.unwrap();
        assert!(Arc::ptr_eq(&delivered_child, &child));
        assert_eq!(delivered_signo, 15);
        assert!(Arc::ptr_eq(
            &super::task_parent_node_snapshot(&publication, &child).unwrap(),
            &reaper
        ));
        assert!(Arc::ptr_eq(
            &super::task_parent_node_snapshot(&publication, &unrelated).unwrap(),
            &other_parent
        ));
        assert!(super::finish_task_parent_exit(&publication, &parent));
        super::exit_task_parent_relation(&publication, &child, None, None, |_, _| {});
        super::exit_task_parent_relation(&publication, &unrelated, None, None, |_, _| {});
        super::exit_task_parent_relation(&publication, &other_parent, None, None, |_, _| {});
        super::exit_task_parent_relation(&publication, &reaper, None, None, |_, _| {});
        drop(publication);
    }

    #[test]
    fn root_parent_inheritance_is_valid_and_does_not_create_a_self_cycle() {
        let root = node(301);
        let sibling = node(302);

        publish_task_parent_relation(&sibling, TaskParentChoice::Inherit(root.clone()));
        assert!(task_parent_node_snapshot(&sibling).is_none());
        assert!(root.state.lock().first_child.is_none());

        retire(&sibling);
        retire(&root);
    }

    #[test]
    fn publication_after_parent_exit_follows_the_exact_reaper_chain() {
        let parent = node(301);
        let reaper = node(302);
        let child = node(303);
        exit_task_parent_relation(&parent, Some(reaper.clone()), None, |_, _| {});

        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));
        assert!(Arc::ptr_eq(
            &task_parent_node_snapshot(&child).unwrap(),
            &reaper
        ));
        assert!(parent.state.lock().first_child.is_none());

        retire(&child);
        retire(&reaper);
    }

    #[test]
    fn relation_charge_limit_rolls_back_exactly() {
        let counter = AtomicUsize::new(0);
        assert!(try_reserve_task_parent_relation(&counter, 2));
        assert!(try_reserve_task_parent_relation(&counter, 2));
        assert!(!try_reserve_task_parent_relation(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[test]
    fn numeric_tid_reuse_cannot_alias_group_delivery_trigger_identity() {
        let parent = node(401);
        let old_child = node(402);
        let reused_tid_child = node(402);
        old_child.pdeath_signal.store(9, Ordering::Release);
        publish_task_parent_relation(&old_child, TaskParentChoice::Caller(parent.clone()));

        let mut delivered = None;
        exit_task_parent_relation(&parent, None, None, |target, _| delivered = Some(target));
        let delivered = delivered.unwrap();
        assert!(Arc::ptr_eq(&delivered, &old_child));
        assert!(!Arc::ptr_eq(&delivered, &reused_tid_child));

        retire(&old_child);
        retire(&reused_tid_child);
    }

    #[test]
    fn nested_subreaper_exit_reparents_and_notifies_at_each_exact_death() {
        let init = node(501);
        let subreaper = node(502);
        let parent = node(503);
        let child = node(504);
        child.pdeath_signal.store(10, Ordering::Release);
        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));

        let mut deliveries = Vec::new();
        exit_task_parent_relation(
            &parent,
            Some(subreaper.clone()),
            Some(init.clone()),
            |target, signo| deliveries.push((target, signo)),
        );
        assert!(Arc::ptr_eq(
            &task_parent_node_snapshot(&child).unwrap(),
            &subreaper
        ));
        exit_task_parent_relation(
            &subreaper,
            Some(init.clone()),
            Some(init.clone()),
            |target, signo| deliveries.push((target, signo)),
        );
        assert!(Arc::ptr_eq(
            &task_parent_node_snapshot(&child).unwrap(),
            &init
        ));
        assert_eq!(deliveries.len(), 2);
        assert!(
            deliveries
                .iter()
                .all(|(target, signo)| Arc::ptr_eq(target, &child) && *signo == 10)
        );

        retire(&child);
        retire(&init);
    }

    #[test]
    fn stale_parent_snapshot_fails_after_exact_reparent() {
        let parent = node(601);
        let reaper = node(602);
        let child = node(603);
        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));
        let snapshot = task_parent_node_snapshot(&child).unwrap();

        exit_task_parent_relation(&parent, Some(reaper.clone()), None, |_, _| {});
        assert!(!task_parent_node_matches(&child, &snapshot));
        assert!(task_parent_node_matches(&child, &reaper));

        retire(&child);
        retire(&reaper);
    }

    #[test]
    fn security_snapshot_rejects_parent_credential_republication() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace).unwrap()).unwrap();
        let parent = credential_node(701, &slot);
        let child = node(702);
        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));
        let snapshot = task_parent_security_snapshot(&child).unwrap();
        assert!(task_parent_security_snapshot_matches(&child, &snapshot));

        // Even an otherwise identical credential is a new immutable security
        // publication and invalidates the hook-authorized Arc identity.
        slot.prepare().finish().unwrap().commit();
        assert!(!task_parent_security_snapshot_matches(&child, &snapshot));

        retire(&child);
        retire(&parent);
    }

    #[test]
    fn process_access_parent_credential_guard_matches_exact_snapshot() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace).unwrap()).unwrap();
        let parent = credential_node(711, &slot);
        let child = node(712);
        publish_task_parent_relation(&child, TaskParentChoice::Caller(parent.clone()));
        let snapshot = task_parent_security_snapshot(&child).unwrap();

        let guard = lock_task_parent_security_snapshot(&child, &snapshot).unwrap();
        assert!(Arc::ptr_eq(guard.credential(), snapshot.credential()));
        drop(guard);

        slot.prepare().finish().unwrap().commit();
        assert!(lock_task_parent_security_snapshot(&child, &snapshot).is_none());
        retire(&child);
        retire(&parent);
    }
}
