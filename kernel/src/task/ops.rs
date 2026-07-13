use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
#[cfg(test)]
extern crate std;
#[cfg(test)]
use core::cell::Cell;
use core::{
    alloc::Layout,
    ffi::c_long,
    sync::atomic::{AtomicU32, Ordering},
};

use axerrno::{AxError, AxResult};
use axhal::{paging::MappingFlags, power::system_off};
use axsync::Mutex;
use axtask::{AxTaskRef, TaskInner, WeakAxTaskRef, current};
use bytemuck::AnyBitPattern;
use hashbrown::{HashMap, HashSet};
use kernel_guard::NoPreemptIrqSave;
use linux_raw_sys::general::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, ROBUST_LIST_LIMIT};
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr};
use spin::Lazy;
use starry_process::{ExitOutcome, Pid, ProcessError};
use starry_signal::{SignalActionFlags, SignalDisposition, SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use super::{
    AsThread, CommittedProcessExit, ExecImageCommit, FutexKey, ITimerType, PreparedExecCredential,
    ProcStateHint, Process, ProcessAccessState, ProcessData, ProcessGroup, ProcessReparentBatch,
    Session, TaskParentNode, TaskUsage, Thread, ThreadExitTransition, TimerState, futex_table_for,
    lock_task_parent_publication, process_domain, send_signal_thread_inner, send_signal_to_process,
    send_signal_to_process_data, send_signal_to_thread, user::linux_pid_from_task_id,
};
use crate::{
    mm::{AddrSpace, UserPtr, access_user_memory},
    pseudofs::cgroup,
    syscall::acct_process_exit,
};

trait RegistryWeak: Clone {
    type Strong;

    fn downgrade(value: &Self::Strong) -> Self;
    fn upgrade(&self) -> Option<Self::Strong>;
    fn is_live(&self) -> bool;
}

impl<T: ?Sized> RegistryWeak for Weak<T> {
    type Strong = Arc<T>;

    fn downgrade(value: &Self::Strong) -> Self {
        Arc::downgrade(value)
    }

    fn upgrade(&self) -> Option<Self::Strong> {
        Weak::upgrade(self)
    }

    fn is_live(&self) -> bool {
        self.strong_count() != 0
    }
}

/// A weak lookup registry with explicit capacity credits.
///
/// `HashMap::try_reserve()` alone is not a transaction: another publisher can
/// consume the spare bucket before a prepared clone commits. `reserved` makes
/// that capacity unavailable to every other insertion until the admission
/// token either commits or rolls back.
struct WeakRegistry<W> {
    entries: HashMap<Pid, W>,
    reserved_keys: HashSet<Pid>,
    reserved: usize,
    operations: usize,
    cleanup_due: bool,
}

/// Ceiling for each Linux-visible weak lookup index. This bounds lookup and
/// admission metadata; it is not a replacement for RLIMIT_NPROC accounting.
const MAX_WEAK_REGISTRY_ENTRIES: usize = 65_536;

fn next_registry_reservation(live: usize, reserved: usize) -> AxResult<usize> {
    let admitted = live.checked_add(reserved).ok_or(AxError::NoMemory)?;
    if admitted >= MAX_WEAK_REGISTRY_ENTRIES {
        return Err(AxError::NoMemory);
    }
    reserved.checked_add(1).ok_or(AxError::NoMemory)
}

impl<W> WeakRegistry<W> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            reserved_keys: HashSet::new(),
            reserved: 0,
            operations: 0,
            cleanup_due: false,
        }
    }
}

impl<W: RegistryWeak> WeakRegistry<W> {
    fn take_one_stale(&mut self) -> Option<W> {
        let key = self
            .entries
            .iter()
            .find_map(|(&key, value)| (!value.is_live()).then_some(key))?;
        self.entries.remove(&key)
    }

    fn get(&self, key: &Pid) -> Option<W::Strong> {
        self.entries.get(key).and_then(RegistryWeak::upgrade)
    }

    fn values(&self) -> impl Iterator<Item = W::Strong> + '_ {
        self.entries.values().filter_map(RegistryWeak::upgrade)
    }

    fn reserve_slot(&mut self, key: Pid) -> AxResult<()> {
        if self.reserved_keys.contains(&key) {
            return Err(AxError::ResourceBusy);
        }
        let live = self
            .entries
            .values()
            .filter(|value| value.is_live())
            .count();
        let additional = next_registry_reservation(live, self.reserved)?;
        self.entries
            .try_reserve(additional)
            .map_err(|_| AxError::NoMemory)?;
        self.reserved_keys
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.reserved_keys.insert(key);
        self.reserved = additional;
        self.operations = self.operations.saturating_add(1);
        if self.operations >= 1_000 {
            self.cleanup_due = true;
        }
        Ok(())
    }

    fn take_one_stale_if_due(&mut self) -> Option<W> {
        if !self.cleanup_due {
            return None;
        }
        let stale = self.take_one_stale();
        if stale.is_none() {
            self.operations = 0;
            self.cleanup_due = false;
        }
        stale
    }

    fn release_slot(&mut self, key: Pid) {
        if self.reserved_keys.remove(&key) {
            self.reserved = self.reserved.saturating_sub(1);
        }
    }

    /// Inserts against a capacity credit created by `reserve_slot()`.
    /// No allocation is possible while the registry lock is held.
    fn insert_reserved(&mut self, key: Pid, value: &W::Strong) -> Option<W> {
        debug_assert!(self.reserved > 0);
        let had_key_reservation = self.reserved_keys.remove(&key);
        debug_assert!(had_key_reservation);
        self.reserved -= 1;
        self.entries.insert(key, W::downgrade(value))
    }

    fn remove(&mut self, key: &Pid) -> Option<W> {
        self.entries.remove(key)
    }
}

static TASK_TABLE: Lazy<Mutex<WeakRegistry<WeakAxTaskRef>>> =
    Lazy::new(|| Mutex::new(WeakRegistry::new()));
static TASK_ALIAS_TABLE: Lazy<Mutex<WeakRegistry<WeakAxTaskRef>>> =
    Lazy::new(|| Mutex::new(WeakRegistry::new()));

#[cfg(test)]
std::thread_local! {
    static TASK_ALIAS_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
struct TaskAliasLockProbe;

#[cfg(test)]
impl TaskAliasLockProbe {
    fn new() -> Self {
        TASK_ALIAS_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

#[cfg(test)]
impl Drop for TaskAliasLockProbe {
    fn drop(&mut self) {
        TASK_ALIAS_LOCK_DEPTH.with(|depth| {
            let held = depth.get();
            debug_assert!(held != 0);
            depth.set(held - 1);
        });
    }
}

#[cfg(test)]
pub(in crate::task) fn task_alias_lock_held() -> bool {
    TASK_ALIAS_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

static PROCESS_TABLE: Lazy<Mutex<WeakRegistry<Weak<ProcessData>>>> =
    Lazy::new(|| Mutex::new(WeakRegistry::new()));

fn parent_sigchld_autoreap(parent: &ProcessData) -> (bool, bool) {
    let actions = parent.signal.actions.lock();
    let action = &actions[Signo::SIGCHLD];
    let ignored = matches!(action.disposition, SignalDisposition::Ignore);
    let no_cldwait = action.flags.contains(SignalActionFlags::NOCLDWAIT);
    (ignored || no_cldwait, ignored)
}

fn notify_reaper_of_inherited_zombie(child: &Arc<Process>) {
    let Some(parent) = child.parent() else {
        return;
    };
    let Ok(parent_data) = get_process_data(parent.pid()) else {
        return;
    };

    let (auto_reap, suppress_exit_signal) = if child.exit_signal() == Some(Signo::SIGCHLD as u8) {
        parent_sigchld_autoreap(&parent_data)
    } else {
        (false, false)
    };

    if auto_reap {
        match process_domain()
            .and_then(|domain| domain.reap(&child).map_err(process_lifecycle_error))
        {
            Ok(true) => cgroup::detach_process(child.pid()),
            Ok(false) => error!(
                "inherited zombie {} was already reaped during autoreap",
                child.pid()
            ),
            Err(error) => error!(
                "failed to autoreap inherited zombie {}: {}",
                child.pid(),
                error
            ),
        }
        parent_data.child_exit_event.wake();
        return;
    }

    if !suppress_exit_signal && let Some(signo) = child.exit_signal().and_then(Signo::from_repr) {
        let _ = send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(signo)));
    }
    parent_data.child_exit_event.wake();
}

#[allow(dead_code)] // reached through the feature-gated memtrack cleanup hook
fn cleanup_registry<W: RegistryWeak>(registry: &Mutex<WeakRegistry<W>>) {
    loop {
        let stale = {
            let mut registry = registry.lock();
            let stale = registry.take_one_stale();
            if stale.is_none() {
                registry.operations = 0;
                registry.cleanup_due = false;
            }
            stale
        };
        let Some(stale) = stale else {
            break;
        };
        // A final weak control block may deallocate. Keep that destructor out
        // of the registry lock and bound each locked scan to one detach.
        drop(stale);
    }
}

fn cleanup_registry_if_due<W: RegistryWeak>(registry: &Mutex<WeakRegistry<W>>) {
    let stale = {
        let mut registry = registry.lock();
        registry.take_one_stale_if_due()
    };
    drop(stale);
}

struct RegistryCleanupGuard<'a, W: RegistryWeak>(&'a Mutex<WeakRegistry<W>>);

impl<W: RegistryWeak> Drop for RegistryCleanupGuard<'_, W> {
    fn drop(&mut self) {
        cleanup_registry_if_due(self.0);
    }
}

fn install_current_user_page_table_root(curr_ptr: *mut TaskInner, root: PhysAddr) {
    unsafe {
        (*curr_ptr).ctx_mut().set_page_table_root(root);
        axhal::asm::write_user_page_table(root);
    }
    #[cfg(not(target_arch = "x86_64"))]
    axhal::asm::flush_tlb(None);
}

/// Cleanup expired entries in the task tables.
///
/// This function is intended to be used during memory leak analysis to remove
/// possible noise caused by expired entries in the weak registries.
pub fn cleanup_task_tables() {
    cleanup_registry(&TASK_TABLE);
    cleanup_registry(&TASK_ALIAS_TABLE);
    cleanup_registry(&PROCESS_TABLE);
}

/// Fallible capacity and identity admission for all task lookup registries.
///
/// Holding this token guarantees that
/// [`TaskTableAdmission::commit_with_publication`] can add the task, optional
/// visible-TID alias, live process-runtime index, and core process/thread state
/// without an externally visible split. Process/group/session topology is
/// published only by the kernel-owned [`super::ProcessDomain`].
pub(crate) struct TaskTableAdmission {
    task: AxTaskRef,
    tid: Pid,
    visible_tid: Pid,
    proc_data: Arc<ProcessData>,
    pid: Pid,
    task_slot: bool,
    alias_slot: bool,
    process_slot: bool,
    committed: bool,
}

fn validate_lookup_key(key: Pid) -> AxResult<()> {
    if key == 0 {
        Err(AxError::BadState)
    } else {
        Ok(())
    }
}

fn reserve_unique_task_key(table: &Mutex<WeakRegistry<WeakAxTaskRef>>, key: Pid) -> AxResult<bool> {
    validate_lookup_key(key)?;
    let _cleanup = RegistryCleanupGuard(table);
    let mut table = table.lock();
    if table.get(&key).is_some() {
        return Err(AxError::AlreadyExists);
    }
    table.reserve_slot(key)?;
    Ok(true)
}

fn reserve_process_key(key: Pid, expected: &Arc<ProcessData>) -> AxResult<bool> {
    validate_lookup_key(key)?;
    let _cleanup = RegistryCleanupGuard(&PROCESS_TABLE);
    let mut table = PROCESS_TABLE.lock();
    if let Some(existing) = table.get(&key) {
        return if Arc::ptr_eq(&existing, expected) {
            Ok(false)
        } else {
            Err(AxError::AlreadyExists)
        };
    }
    table.reserve_slot(key)?;
    Ok(true)
}

/// Reserves every registry bucket needed to publish `task`.
pub(crate) fn prepare_task_table_admission(task: &AxTaskRef) -> AxResult<TaskTableAdmission> {
    let tid = linux_pid_from_task_id(task.id().as_u64())?;
    let visible_tid = task.as_thread().tid();
    let proc_data = task.as_thread().proc_data.clone();
    let pid = proc_data.proc.pid();

    let mut admission = TaskTableAdmission {
        task: task.clone(),
        tid,
        visible_tid,
        proc_data,
        pid,
        task_slot: false,
        alias_slot: false,
        process_slot: false,
        committed: false,
    };

    admission.task_slot = reserve_unique_task_key(&TASK_TABLE, tid)?;
    if visible_tid != tid {
        admission.alias_slot = reserve_unique_task_key(&TASK_ALIAS_TABLE, visible_tid)?;
    }
    admission.process_slot = reserve_process_key(pid, &admission.proc_data)?;
    Ok(admission)
}

impl TaskTableAdmission {
    /// Publishes lookup tables and a core process/thread transaction as one
    /// externally atomic handoff.
    ///
    /// Weak table entries are inserted while all table locks are held, then the
    /// infallible core publication runs before those locks are released. A core
    /// registry reader that wins after that release point blocks on these table
    /// locks and then observes the runtime entries; a table reader cannot see
    /// the entries before core publication.
    pub(crate) fn commit_with_publication<R>(mut self, publish: impl FnOnce() -> R) -> R {
        let mut task_table = TASK_TABLE.lock();
        let mut alias_table = TASK_ALIAS_TABLE.lock();
        let mut process_table = PROCESS_TABLE.lock();

        let old_task = self
            .task_slot
            .then(|| task_table.insert_reserved(self.tid, &self.task))
            .flatten();
        let old_alias = self
            .alias_slot
            .then(|| alias_table.insert_reserved(self.visible_tid, &self.task))
            .flatten();
        let old_process = self
            .process_slot
            .then(|| process_table.insert_reserved(self.pid, &self.proc_data))
            .flatten();
        let result = publish();
        self.committed = true;

        drop(process_table);
        drop(alias_table);
        drop(task_table);
        drop((old_task, old_alias, old_process));
        result
    }
}

impl Drop for TaskTableAdmission {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.task_slot {
            TASK_TABLE.lock().release_slot(self.tid);
        }
        if self.alias_slot {
            TASK_ALIAS_TABLE.lock().release_slot(self.visible_tid);
        }
        if self.process_slot {
            PROCESS_TABLE.lock().release_slot(self.pid);
        }
    }
}

/// Capacity/identity admission for an exec de-thread task alias.
pub struct TaskAliasAdmission {
    alias: Pid,
    task: AxTaskRef,
    committed: bool,
}

/// Reserves an additional lookup key without publishing it.
pub fn prepare_task_alias_admission(alias: Pid, task: &AxTaskRef) -> AxResult<TaskAliasAdmission> {
    reserve_unique_task_key(&TASK_ALIAS_TABLE, alias)?;
    Ok(TaskAliasAdmission {
        alias,
        task: task.clone(),
        committed: false,
    })
}

impl TaskAliasAdmission {
    /// Publishes a non-leader exec's credential binding, visible TID, and
    /// alias as one short composite transition. All retired strong references
    /// and the credential writer guard are released after the alias and
    /// leader-binding locks have both been dropped.
    pub(crate) fn commit_exec_handoff<'a>(
        mut self,
        proc_data: &ProcessData,
        owner: Pid,
        thread: &Thread,
        prepared: PreparedExecCredential<'a>,
        new_aspace: Arc<Mutex<AddrSpace>>,
        new_access_state: Arc<ProcessAccessState>,
    ) -> ExecImageCommit<'a> {
        debug_assert!(core::ptr::eq(self.task.as_thread(), thread));
        let mut aliases = TASK_ALIAS_TABLE.lock();
        #[cfg(test)]
        let alias_lock_probe = TaskAliasLockProbe::new();
        let retirement =
            proc_data.publish_exec_image(owner, thread, prepared, new_aspace, new_access_state);
        thread.set_tid(self.alias);
        let old = aliases.insert_reserved(self.alias, &self.task);
        self.committed = true;
        drop(aliases);
        #[cfg(test)]
        drop(alias_lock_probe);
        drop(old);
        retirement
    }
}

/// Commits the only externally callable exec identity transition. A
/// non-leader exec includes the reserved alias in the same short critical
/// section; a leader exec only republishes its existing slot after applying
/// the prepared credential.
pub(crate) fn commit_exec_identity_handoff<'a>(
    admission: Option<TaskAliasAdmission>,
    proc_data: &ProcessData,
    owner: Pid,
    thread: &Thread,
    prepared: PreparedExecCredential<'a>,
    new_aspace: Arc<Mutex<AddrSpace>>,
    new_access_state: Arc<ProcessAccessState>,
) -> ExecImageCommit<'a> {
    if let Some(admission) = admission {
        admission.commit_exec_handoff(
            proc_data,
            owner,
            thread,
            prepared,
            new_aspace,
            new_access_state,
        )
    } else {
        proc_data.publish_exec_image(owner, thread, prepared, new_aspace, new_access_state)
    }
}

impl Drop for TaskAliasAdmission {
    fn drop(&mut self) {
        if !self.committed {
            TASK_ALIAS_TABLE.lock().release_slot(self.alias);
        }
    }
}

/// Fallibly snapshots live registry values. Capacity is admitted before the
/// lock is reacquired; the locked pass only upgrades weak references and
/// pushes into already-owned storage.
fn try_registry_values<W: RegistryWeak>(
    registry: &Mutex<WeakRegistry<W>>,
) -> AxResult<Vec<W::Strong>> {
    let mut snapshot = Vec::new();
    loop {
        snapshot.clear();
        let required = registry.lock().entries.len();
        if snapshot.capacity() < required {
            snapshot
                .try_reserve_exact(required)
                .map_err(|_| AxError::NoMemory)?;
        }
        let table = registry.lock();
        let mut retry = false;
        for task in table.values() {
            if snapshot.len() == snapshot.capacity() {
                retry = true;
                break;
            }
            snapshot.push(task);
        }
        drop(table);
        if !retry {
            return Ok(snapshot);
        }

        let current = snapshot.capacity();
        if current >= MAX_WEAK_REGISTRY_ENTRIES {
            return Err(AxError::NoMemory);
        }
        let target = current
            .max(1)
            .saturating_mul(2)
            .min(MAX_WEAK_REGISTRY_ENTRIES);
        snapshot.clear();
        snapshot
            .try_reserve_exact(target)
            .map_err(|_| AxError::NoMemory)?;
    }
}

/// Fallibly snapshots all task references.
pub fn try_tasks() -> AxResult<Vec<AxTaskRef>> {
    try_registry_values(&TASK_TABLE)
}

/// Fallibly snapshots all live process runtime objects.
pub fn try_processes() -> AxResult<Vec<Arc<ProcessData>>> {
    try_registry_values(&PROCESS_TABLE)
}

/// Finds the task with the given TID.
pub fn get_task(tid: Pid) -> AxResult<AxTaskRef> {
    if tid == 0 {
        return Ok(current().clone());
    }
    if let Some(task) = TASK_TABLE.lock().get(&tid) {
        return Ok(task);
    }
    TASK_ALIAS_TABLE
        .lock()
        .get(&tid)
        .ok_or(AxError::NoSuchProcess)
}

/// Finds the task with the given user-visible TID.
pub fn get_visible_task(tid: Pid) -> AxResult<AxTaskRef> {
    {
        let aliases = TASK_ALIAS_TABLE.lock();
        if let Some(task) = aliases.get(&tid)
            && task.as_thread().tid() == tid
            && !task.as_thread().pending_exit()
        {
            return Ok(task);
        }
    }

    if let Some(task) = TASK_TABLE.lock().get(&tid)
        && task.as_thread().tid() == tid
        && !task.as_thread().pending_exit()
    {
        return Ok(task);
    }

    Err(AxError::NoSuchProcess)
}

/// Finds the task with the given user-visible TID, including tasks that have
/// begun exiting but are still published in the task tables.
pub fn get_visible_task_including_exiting(tid: Pid) -> AxResult<AxTaskRef> {
    {
        let aliases = TASK_ALIAS_TABLE.lock();
        if let Some(task) = aliases.get(&tid)
            && task.as_thread().tid() == tid
        {
            return Ok(task);
        }
    }

    if let Some(task) = TASK_TABLE.lock().get(&tid)
        && task.as_thread().tid() == tid
    {
        return Ok(task);
    }

    Err(AxError::NoSuchProcess)
}

struct ProcStateHintGuard<'a> {
    thread: Option<&'a super::Thread>,
    prev: ProcStateHint,
}

impl Drop for ProcStateHintGuard<'_> {
    fn drop(&mut self) {
        if let Some(thread) = self.thread {
            thread.set_proc_state_hint(self.prev);
        }
    }
}

/// Temporarily publishes a procfs-visible blocking state for the current task.
pub fn with_proc_state_hint<R>(hint: ProcStateHint, f: impl FnOnce() -> R) -> R {
    let curr = current();
    let guard = if let Some(thread) = curr.try_as_thread() {
        ProcStateHintGuard {
            thread: Some(thread),
            prev: thread.swap_proc_state_hint(hint),
        }
    } else {
        ProcStateHintGuard {
            thread: None,
            prev: ProcStateHint::None,
        }
    };
    let result = f();
    drop(guard);
    result
}

/// Removes a task alias lookup key.
pub fn remove_task_alias(alias: Pid) {
    let removed = TASK_ALIAS_TABLE.lock().remove(&alias);
    drop(removed);
}

/// Finds the process with the given PID.
pub fn get_process_data(pid: Pid) -> AxResult<Arc<ProcessData>> {
    if pid == 0 {
        return Ok(current().as_thread().proc_data.clone());
    }
    PROCESS_TABLE.lock().get(&pid).ok_or(AxError::NoSuchProcess)
}

/// Removes exactly this runtime object from the live process index.
fn remove_process_runtime(proc_data: &Arc<ProcessData>) {
    let pid = proc_data.proc.pid();
    let removed = {
        let mut table = PROCESS_TABLE.lock();
        let matches = table
            .entries
            .get(&pid)
            .is_some_and(|current| current.as_ptr() == Arc::as_ptr(proc_data));
        matches.then(|| table.remove(&pid)).flatten()
    };
    drop(removed);
}

/// Finds a process by PID even after its runtime [`ProcessData`] has been
/// dropped. Zombie processes remain owned by their parent until wait reaps
/// them, so PID-existence checks such as kill(2) must still see them.
pub fn get_process_including_zombie(pid: Pid) -> AxResult<Arc<Process>> {
    if pid == 0 {
        return Ok(current().as_thread().proc_data.proc.clone());
    }
    process_domain()?
        .registry()
        .get(pid)
        .ok_or(AxError::NoSuchProcess)
}

/// Finds the process group with the given PGID.
pub fn get_process_group(pgid: Pid) -> AxResult<Arc<ProcessGroup>> {
    process_domain()?
        .registry()
        .get_process_group(pgid)
        .ok_or(AxError::NoSuchProcess)
}

/// Finds the session with the given SID.
pub fn get_session(sid: Pid) -> AxResult<Arc<Session>> {
    process_domain()?
        .registry()
        .get_session(sid)
        .ok_or(AxError::NoSuchProcess)
}

/// Updates the current task's saved and active user page table root.
pub fn set_current_user_page_table_root(root: PhysAddr) {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    // SAFETY: this only mutates the current task's saved TaskContext while the task
    // is running on the current CPU, so no other code can access it concurrently.
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    install_current_user_page_table_root(curr_ptr, root);
}

/// Executes `f` with the current task temporarily bound to `root`.
///
/// Some kernel paths need to touch user memory on behalf of a freshly cloned or
/// exiting task. Those writes must target the intended user page table instead
/// of whichever root happens to be active at the moment.
pub fn with_current_user_page_table_root<R>(root: PhysAddr, f: impl FnOnce() -> R) -> R {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    let old_root = axhal::asm::read_user_page_table();
    install_current_user_page_table_root(curr_ptr, root);
    let result = f();
    install_current_user_page_table_root(curr_ptr, old_root);
    result
}

/// Writes `value` into `ptr` within `aspace_handle`.
///
/// The caller may target a different process or a not-yet-running child, so we
/// must both pre-populate the destination pages in that address space and then
/// execute the copy under the matching user page table root.
pub fn vm_write_in_aspace<T: Copy>(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    ptr: *mut T,
    value: T,
) -> AxResult<()> {
    let layout = Layout::new::<T>();
    let start = VirtAddr::from_usize(ptr.addr());
    let root = {
        let mut aspace = aspace_handle.lock();
        if !aspace.can_access_range(start, layout.size(), MappingFlags::WRITE) {
            return Err(AxError::BadAddress);
        }
        let page_start = start.align_down_4k();
        let page_end = (start + layout.size()).align_up_4k();
        aspace.populate_area(page_start, page_end - page_start, MappingFlags::WRITE)?;
        aspace.page_table_root()
    };
    with_current_user_page_table_root(root, || ptr.vm_write(value)).map_err(Into::into)
}

fn detach_ptrace_links_on_process_exit(proc_data: &ProcessData) {
    let pid = proc_data.proc.pid();
    let traced_session = {
        let _ptrace_action = proc_data.lock_ptrace_actions();
        proc_data.clear_ptrace()
    };
    if let Some(session) = traced_session
        && let Ok(tracer_data) = get_process_data(session.tracer)
    {
        tracer_data.remove_ptrace_tracee(super::PtraceReverseLink::new(pid, session));
    }

    detach_ptrace_reverse_links(proc_data.clear_ptrace_tracees());
}

fn detach_ptrace_reverse_links(links: super::process::PtraceReverseLinkDrain) {
    for link in links {
        let Ok(tracee_data) = get_process_data(link.tracee()) else {
            continue;
        };
        let _ptrace_action = tracee_data.lock_ptrace_actions();
        tracee_data.end_ptrace(link.session());
    }
}

fn detach_ptrace_links_on_thread_exit(proc_data: &ProcessData, tracer_kernel_tid: Pid) {
    detach_ptrace_reverse_links(proc_data.clear_ptrace_tracees_for_task(tracer_kernel_tid));
}

#[cfg(target_arch = "loongarch64")]
fn with_current_task_ctx_mut<R>(f: impl FnOnce(&mut axhal::context::TaskContext) -> R) -> R {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    unsafe { f((*curr_ptr).ctx_mut()) }
}

/// Copies the current task's saved user FPU state into another task context.
#[cfg(target_arch = "loongarch64")]
pub fn copy_current_user_fpu_state_to(dst: &mut axhal::context::TaskContext) {
    with_current_task_ctx_mut(|ctx| {
        dst.fpu = ctx.fpu;
    });
}

/// Restores the current task's saved user FPU state to the CPU.
#[cfg(target_arch = "loongarch64")]
pub fn restore_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu.restore();
    });
}

/// Saves the CPU's current FPU state into the current task's saved context.
#[cfg(target_arch = "loongarch64")]
pub fn save_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu.save();
    });
}

/// Resets the current task's saved FPU state and restores the reset state to the CPU.
#[cfg(target_arch = "loongarch64")]
pub fn reset_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu = Default::default();
        ctx.fpu.restore();
    });
}

/// Poll the timer
pub fn poll_timer(task: &TaskInner) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let mut signals = Vec::new();
    let usage = {
        let Ok(mut time) = thr.time.try_borrow_mut() else {
            // reentrant borrow, likely IRQ
            return;
        };
        time.poll(&mut signals);
        let (utime, stime) = time.output();
        TaskUsage::from_time_values(utime, stime)
    };
    thr.store_usage_snapshot(usage);
    for signo in signals {
        send_signal_thread_inner(task, thr, SignalInfo::new_kernel(signo));
    }
}

pub fn poll_itimer_alarm(task: &TaskInner, ty: ITimerType, sequence: u64) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let mut signals = Vec::new();
    let usage = {
        let Ok(mut time) = thr.time.try_borrow_mut() else {
            return;
        };
        if !time.itimer_sequence_matches(ty, sequence) {
            return;
        }
        time.poll(&mut signals);
        let (utime, stime) = time.output();
        TaskUsage::from_time_values(utime, stime)
    };
    thr.store_usage_snapshot(usage);
    for signo in signals {
        send_signal_thread_inner(task, thr, SignalInfo::new_kernel(signo));
    }
}

/// Sets the timer state.
pub fn set_timer_state(task: &TaskInner, state: TimerState) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let mut signals = Vec::new();
    let usage = {
        let Ok(mut time) = thr.time.try_borrow_mut() else {
            // reentrant borrow, likely IRQ
            return;
        };
        time.poll(&mut signals);
        time.set_state(state);
        let (utime, stime) = time.output();
        TaskUsage::from_time_values(utime, stime)
    };
    thr.store_usage_snapshot(usage);
    for signo in signals {
        send_signal_thread_inner(task, thr, SignalInfo::new_kernel(signo));
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustList {
    pub next: *mut RobustList,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustListHead {
    pub list: RobustList,
    pub futex_offset: c_long,
    pub list_op_pending: *mut RobustList,
}

fn handle_futex_death(entry: *mut RobustList, offset: i64) -> AxResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(AxError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| AxError::InvalidInput)?;
    let uaddr = address as *mut u32;
    if !mark_robust_owner_died(uaddr, current().as_thread().tid())? {
        return Ok(());
    }

    let key = FutexKey::new_current(address);
    let futex_table = futex_table_for(&key);
    if let Some(futex) = futex_table.get(&key) {
        futex.wq.wake(1, u32::MAX);
    }
    Ok(())
}

fn robust_owner_died_word(value: u32, tid: Pid) -> Option<u32> {
    if value & FUTEX_TID_MASK != tid {
        return None;
    }
    Some((value & FUTEX_WAITERS) | FUTEX_OWNER_DIED)
}

fn user_atomic_u32(uaddr: *mut u32) -> AxResult<&'static AtomicU32> {
    Ok(UserPtr::<AtomicU32>::from(uaddr.cast()).get_as_mut()?)
}

fn mark_robust_owner_died(uaddr: *mut u32, tid: Pid) -> AxResult<bool> {
    let word = user_atomic_u32(uaddr)?;
    let mut value = access_user_memory(|| word.load(Ordering::Acquire));
    loop {
        let Some(next_value) = robust_owner_died_word(value, tid) else {
            return Ok(false);
        };
        match access_user_memory(|| {
            word.compare_exchange(value, next_value, Ordering::AcqRel, Ordering::Acquire)
        }) {
            Ok(_) => return Ok(true),
            Err(observed) => value = observed,
        }
    }
}

fn is_robust_list_error(err: AxError) -> bool {
    matches!(
        err,
        AxError::BadAddress | AxError::InvalidInput | AxError::FilesystemLoop
    )
}

pub fn exit_robust_list(head: *const RobustListHead) {
    // Reference: https://elixir.bootlin.com/linux/v6.13.6/source/kernel/futex/core.c#L777

    let mut limit = ROBUST_LIST_LIMIT;

    let end_ptr = unsafe { &raw const (*head).list };
    let head = match head.vm_read() {
        Ok(head) => head,
        Err(err) if is_robust_list_error(err.into()) => return,
        Err(_) => return,
    };
    let mut entry = head.list.next;
    let offset = head.futex_offset;
    let pending = head.list_op_pending;

    while !core::ptr::eq(entry, end_ptr) {
        let next_entry = match entry.vm_read() {
            Ok(next) => next.next,
            Err(err) if is_robust_list_error(err.into()) => break,
            Err(_) => break,
        };
        if entry != pending {
            match handle_futex_death(entry, offset) {
                Ok(()) => {}
                Err(err) if is_robust_list_error(err) => break,
                Err(_) => break,
            }
        }
        entry = next_entry;

        limit -= 1;
        if limit == 0 {
            break;
        }
        axtask::yield_now();
    }

    if !pending.is_null() {
        let _ = handle_futex_death(pending, offset);
    }
}

fn process_lifecycle_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive | ProcessError::NotInitialized => {
            AxError::NoSuchProcess
        }
        ProcessError::Busy => AxError::ResourceBusy,
        ProcessError::WrongDomain => AxError::BadState,
        _ => AxError::BadState,
    }
}

fn publish_final_process_exit(
    proc_data: &ProcessData,
    process: &Arc<Process>,
    departing: &Thread,
    task_parent_publication: &super::TaskParentPublicationGuard<'_>,
    exit: super::process::PreparedZombieExit,
    self_usage: TaskUsage,
    child_usage: TaskUsage,
) -> CommittedProcessExit {
    exit.commit_with_reparent_handoff(
        process.exit_code(),
        self_usage.into(),
        child_usage.into(),
        proc_data.group_leader_cred(),
        |child| notify_reaper_of_inherited_zombie(&child),
        |batch| reparent_exact_children_from_core_batch(departing, task_parent_publication, batch),
    )
}

fn begin_group_exit(proc_data: &ProcessData, exit_code: i32) -> bool {
    proc_data.begin_group_exit(exit_code)
}

fn remove_current_thread(
    process: &Arc<Process>,
    tid: Pid,
    exit_code: i32,
) -> AxResult<ThreadExitTransition> {
    process_domain()?
        .exit_thread(process, tid, exit_code)
        .map_err(process_lifecycle_error)
}

fn exact_parent_thread_in_process(
    process: &Arc<Process>,
    departing_kernel_tid: Option<Pid>,
) -> Option<Arc<TaskParentNode>> {
    if !process.is_live() {
        return None;
    }
    for tid in process.thread_ids() {
        if departing_kernel_tid == Some(tid) {
            continue;
        }
        let Ok(task) = get_task(tid) else {
            continue;
        };
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        if thread.kernel_tid() == tid && Arc::ptr_eq(&thread.proc_data.proc, process) {
            return Some(thread.task_parent_node().clone());
        }
    }
    None
}

fn exact_parent_init_reaper(departing_kernel_tid: Option<Pid>) -> Option<Arc<TaskParentNode>> {
    let init = process_domain().ok()?.init_process()?;
    exact_parent_thread_in_process(&init, departing_kernel_tid)
}

fn reparent_exact_children_from_core_batch(
    departing: &Thread,
    publication: &super::TaskParentPublicationGuard<'_>,
    batch: &ProcessReparentBatch,
) {
    let reaper = exact_parent_thread_in_process(batch.reaper(), Some(departing.kernel_tid()))
        .expect("core-selected process reaper has no exact live task endpoint");

    for moved in batch.reparented() {
        let authoritative_parent = moved
            .child()
            .parent()
            .expect("core-reparented child has no authoritative parent");
        assert!(
            Arc::ptr_eq(&authoritative_parent, batch.reaper()),
            "core child parent changed while exact-parent publication was gated"
        );
        drop(authoritative_parent);
    }

    departing.reparent_task_parent_children_matching(
        publication,
        &reaper,
        |child| {
            let Some(child_data) = child.process_data() else {
                return false;
            };
            batch
                .reparented()
                .any(|moved| Arc::ptr_eq(&child_data.proc, moved.child()))
        },
        deliver_exact_parent_death,
    );
}

fn deliver_exact_parent_death(child: Arc<TaskParentNode>, raw_signo: u32) {
    let Ok(raw_signo) = u8::try_from(raw_signo) else {
        return;
    };
    let Some(signo) = Signo::from_repr(raw_signo) else {
        return;
    };
    let Some(proc_data) = child.process_data() else {
        return;
    };
    // Linux v6.6 `forget_original_parent()` reads the task-local
    // `pdeath_signal` from this exact child, then uses
    // `group_send_sig_info(..., PIDTYPE_TGID)`. Retain the ABA-safe
    // ProcessData resolved through that child and perform the same
    // process-directed delivery without a second numeric-PID lookup.
    // https://elixir.bootlin.com/linux/v6.6/source/kernel/exit.c#L675
    let _ = send_signal_to_process_data(&proc_data, Some(SignalInfo::new_kernel(signo)));
}

pub fn do_exit(exit_code: i32, group_exit: bool) -> AxResult<()> {
    let curr = current();
    let thr = curr.as_thread();
    let tid = linux_pid_from_task_id(curr.id().as_u64())?;
    let visible_tid = thr.tid();

    match curr.id_name() {
        Ok(name) => info!("{} exit with code: {}", name, exit_code),
        Err(error) => info!(
            "Task({}) exit with code: {} (name unavailable: {})",
            curr.id().as_u64(),
            exit_code,
            error
        ),
    }
    let process = &thr.proc_data.proc;
    set_timer_state(&curr, TimerState::Kernel);
    thr.proc_data.end_exec(tid);
    let started_group_exit = group_exit && begin_group_exit(&thr.proc_data, exit_code);
    if started_group_exit {
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for tid in process.thread_ids() {
            let _ = send_signal_to_thread(None, tid, Some(sig.clone()));
        }
    }
    let lifecycle = thr.proc_data.lock_process_lifecycle();
    let mut task_parent_publication = Some(lock_task_parent_publication());
    let final_exit = match remove_current_thread(process, tid, exit_code) {
        Ok(ThreadExitTransition::NotFound) => {
            error!(
                "refusing partial exit because current TID {} is absent from process {}",
                tid,
                process.pid()
            );
            drop(task_parent_publication.take());
            drop(lifecycle);
            return Err(AxError::BadState);
        }
        Ok(ThreadExitTransition::LiveThreadsRemain) => None,
        Ok(ThreadExitTransition::FinalThread(exit)) => Some(exit),
        Err(error) => {
            error!(
                "refusing partial exit for TID {} in process {} after lifecycle error: {}",
                tid,
                process.pid(),
                error
            );
            drop(task_parent_publication.take());
            drop(lifecycle);
            return Err(error);
        }
    };

    // A final admission owns the removed membership and restores it on Drop.
    // Bind the process-owned payload before any per-thread/process teardown so
    // every operation after this point is an infallible commit sequence.
    let final_exit = final_exit
        .map(|exit| thr.proc_data.prepare_zombie_exit(exit))
        .transpose()?;
    if final_exit.is_some() {
        // The final core admission plus this ProcessData lifecycle guard now
        // exclude every new fork/thread publication. The exact node and its
        // Weak<ProcessData> endpoint remain discoverable from its parent's
        // intrusive child list while lengthy teardown runs without the global
        // graph gate. Reacquire the gate for core commit, batch handoff, and
        // exact-node retirement below.
        drop(task_parent_publication.take());
    }

    // ptrace ownership is task-exact even though reverse-link storage is
    // process-owned. Remove and detach every relationship created by this
    // immutable kernel TID before a non-final thread can disappear. The
    // partition is allocation-free and all tracee work happens after its spin
    // lock has been released.
    detach_ptrace_links_on_thread_exit(&thr.proc_data, thr.kernel_tid());

    // A non-final task death is independent of process liveness: notify its
    // exact children now and move them to a live sibling. Process lifecycle
    // serialization keeps the sibling live through this section; init is an
    // explicit defensive fallback. Final-process reparenting is deferred until
    // the process domain performs its authoritative subreaper/init transition.
    if final_exit.is_none() {
        let sibling_reaper = exact_parent_thread_in_process(process, Some(thr.kernel_tid()));
        let init_reaper = exact_parent_init_reaper(Some(thr.kernel_tid()));
        thr.exit_task_parent(
            task_parent_publication
                .as_ref()
                .expect("task-parent publication guard is retained through exact exit"),
            sibling_reaper,
            init_reaper,
            deliver_exact_parent_death,
        );
        drop(task_parent_publication.take());
    }

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    let aspace = thr.proc_data.aspace();
    let clear_result = if clear_child_tid.is_null() {
        Ok(())
    } else {
        vm_write_in_aspace(&aspace, clear_child_tid, 0u32)
    };
    if !clear_child_tid.is_null() && clear_result.is_ok() {
        let key = FutexKey::new_current(clear_child_tid as usize);
        let table = futex_table_for(&key);
        let guard = table.get(&key);
        if let Some(futex) = guard {
            futex.wq.wake(1, u32::MAX);
        }
    }
    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null() {
        exit_robust_list(head);
    }
    drop(aspace);

    thr.proc_data
        .account_exited_thread(TaskUsage::from_thread(thr));
    if visible_tid != tid {
        remove_task_alias(visible_tid);
    }
    if let Some(exit) = final_exit {
        // Drop resident anonymous memory before the zombie waits to be reaped.
        // CLONE_VM can share this mm with a different live process, so only do
        // this when the exiting process is the sole ProcessData owner. The VMA
        // metadata and file mappings are still released by AddrSpace drop.
        let aspace = thr.proc_data.aspace();
        if Arc::strong_count(&aspace) == 2 {
            aspace.lock().discard_private_anonymous_pages();
        }
        let self_usage = thr.proc_data.self_usage();
        let child_usage = thr.proc_data.children_usage();

        acct_process_exit(&thr.proc_data, process.exit_code(), self_usage);
        thr.proc_data.release_executable();
        crate::syscall::cleanup_process_aio(process.pid());
        crate::syscall::cleanup_process_mqueue_notifications(process.pid());
        let detached_fd_table = thr.proc_data.exit_fd_table();
        let closed_fds = thr.with_mut_scope(|scope| {
            crate::file::replace_process_fd_table(scope, detached_fd_table)
        });
        // wait(2) may return as soon as the parent observes the zombie state.
        // Release process-owned POSIX locks before the old files_struct can
        // drop its descriptions. Their Drop path publishes IN_CLOSE/FAN_CLOSE
        // work, which must only become observable after those locks are gone.
        crate::file::release_process_fd_table(process.pid(), closed_fds);
        crate::file::inotify::wait_current_close_notifications();
        detach_ptrace_links_on_process_exit(&thr.proc_data);
        crate::syscall::clear_proc_shm(process.pid());
        task_parent_publication = Some(lock_task_parent_publication());
        let task_parent_guard = task_parent_publication
            .as_ref()
            .expect("final exit retains the task-parent publication guard");
        let committed = publish_final_process_exit(
            &thr.proc_data,
            process,
            thr,
            task_parent_guard,
            exit,
            self_usage,
            child_usage,
        );
        debug_assert_eq!(committed.outcome(), ExitOutcome::BecameZombie);
        assert!(
            thr.finish_task_parent_exit(task_parent_guard),
            "authoritative process reparent handoff left exact children behind"
        );
        drop(task_parent_publication.take());
        remove_process_runtime(&thr.proc_data);

        let parent = committed.notification_parent().cloned();
        let parent_data = parent
            .as_ref()
            .and_then(|parent| get_process_data(parent.pid()).ok());
        let (auto_reap, suppress_exit_signal) = if thr.proc_data.exit_signal == Some(Signo::SIGCHLD)
        {
            parent_data
                .as_ref()
                .map(|parent| parent_sigchld_autoreap(parent))
                .unwrap_or((false, false))
        } else {
            (false, false)
        };

        if let Some(parent) = parent.as_ref()
            && !suppress_exit_signal
            && let Some(signo) = thr.proc_data.exit_signal
        {
            let _ = send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(signo)));
        }
        if auto_reap {
            match process_domain()
                .and_then(|domain| domain.reap(process).map_err(process_lifecycle_error))
            {
                Ok(true) => cgroup::detach_process(process.pid()),
                Ok(false) => error!(
                    "process {} was already reaped during final autoreap",
                    process.pid()
                ),
                Err(error) => error!("failed to autoreap process {}: {}", process.pid(), error),
            }
        }
        if let Some(data) = parent_data {
            data.child_exit_event.wake();
        }
        thr.proc_data.exit_event.wake();
        thr.proc_data.release_vfork();
    }
    drop(lifecycle);
    thr.proc_data.exec_event.wake();
    thr.exit_event.wake();

    thr.set_exit();
    Ok(())
}

/// Stops the machine after an unrecoverable internal exit-transaction fault.
///
/// User-issued exit syscalls return the typed error to their caller. Fatal
/// signal/default-action paths cannot safely resume userspace after consuming
/// the fatal event, so they use this explicit fail-closed policy rather than
/// silently leaving a partially exited task runnable.
pub(crate) fn fail_closed_exit(error: AxError) -> ! {
    error!("fatal process-exit invariant failure: {error}");
    system_off()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robust_owner_died_marks_matching_tid() {
        let tid = 42;
        assert_eq!(
            robust_owner_died_word(FUTEX_WAITERS | tid, tid),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED)
        );
    }

    #[test]
    fn robust_owner_died_ignores_other_owner() {
        assert_eq!(robust_owner_died_word(7, 42), None);
    }

    #[test]
    fn process_lifecycle_busy_preserves_resource_busy() {
        assert_eq!(
            process_lifecycle_error(ProcessError::Busy),
            AxError::ResourceBusy
        );
    }

    #[test]
    fn lookup_publication_rejects_pid_zero_before_registry_access() {
        assert_eq!(validate_lookup_key(0), Err(AxError::BadState));
        assert_eq!(validate_lookup_key(1), Ok(()));
    }

    #[test]
    fn weak_registry_holds_capacity_and_key_credits_until_commit() {
        let first = Arc::new(11usize);
        let second = Arc::new(22usize);
        let mut registry: WeakRegistry<Weak<usize>> = WeakRegistry::new();

        registry.reserve_slot(1).unwrap();
        assert_eq!(registry.reserve_slot(1), Err(AxError::ResourceBusy));
        registry.reserve_slot(2).unwrap();
        assert!(registry.entries.capacity() >= registry.entries.len() + registry.reserved);

        assert!(registry.insert_reserved(1, &first).is_none());
        assert_eq!(registry.get(&1).as_deref(), Some(&11));
        assert_eq!(registry.reserved, 1);
        assert!(registry.reserved_keys.contains(&2));

        assert!(registry.insert_reserved(2, &second).is_none());
        assert_eq!(registry.get(&2).as_deref(), Some(&22));
        assert_eq!(registry.reserved, 0);
        assert!(registry.reserved_keys.is_empty());
    }

    #[test]
    fn weak_registry_rollback_releases_only_its_key_credit() {
        let mut registry: WeakRegistry<Weak<usize>> = WeakRegistry::new();
        registry.reserve_slot(7).unwrap();
        registry.reserve_slot(8).unwrap();

        registry.release_slot(7);

        assert_eq!(registry.reserved, 1);
        assert!(!registry.reserved_keys.contains(&7));
        assert!(registry.reserved_keys.contains(&8));
        assert!(registry.entries.capacity() >= registry.entries.len() + registry.reserved);
    }

    #[test]
    fn weak_registry_ceiling_counts_live_and_reserved_admissions() {
        assert_eq!(next_registry_reservation(0, 0), Ok(1));
        assert_eq!(
            next_registry_reservation(MAX_WEAK_REGISTRY_ENTRIES - 1, 0),
            Ok(1)
        );
        assert_eq!(
            next_registry_reservation(MAX_WEAK_REGISTRY_ENTRIES, 0),
            Err(AxError::NoMemory)
        );
        assert_eq!(
            next_registry_reservation(0, MAX_WEAK_REGISTRY_ENTRIES),
            Err(AxError::NoMemory)
        );
    }

    #[test]
    fn stale_registry_entries_do_not_consume_live_admission() {
        let mut registry: WeakRegistry<Weak<usize>> = WeakRegistry::new();
        let stale = Arc::new(11usize);
        registry.reserve_slot(1).unwrap();
        assert!(registry.insert_reserved(1, &stale).is_none());
        drop(stale);

        registry.reserve_slot(2).unwrap();
        assert_eq!(registry.reserved, 1);
        registry.release_slot(2);
        assert_eq!(registry.reserved, 0);
    }
}
