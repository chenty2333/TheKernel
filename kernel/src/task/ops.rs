use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
#[cfg(test)]
extern crate std;
#[cfg(test)]
use core::cell::Cell;
use core::{
    ffi::c_long,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axhal::{mem::phys_to_virt, paging::MappingFlags, power::system_off};
use axsync::Mutex;
use axtask::{AxTaskRef, TaskInner, WeakAxTaskRef, current};
use bytemuck::AnyBitPattern;
use hashbrown::{HashMap, HashSet};
use kernel_guard::NoPreemptIrqSave;
use linux_raw_sys::general::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, ROBUST_LIST_LIMIT};
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr};
use spin::Lazy;
use thekernel_linux_process_adapter::{ExitOutcome, Pid, ProcessError};
use thekernel_linux_signal::{SignalActionFlags, SignalDisposition, SignalInfo, Signo};

use super::{
    AsThread, CommittedProcessExit, CommittingExecCredential, ExecImageCommit, FutexKey,
    ProcStateHint, Process, ProcessAccessState, ProcessData, ProcessGroup, ProcessReparentBatch,
    PtraceRelationshipSnapshot, Session, TaskParentNode, TaskUsage, Thread, ThreadExitTransition,
    TimerState, fs_context_publication, futex_table_for, lock_task_parent_publication,
    process_domain, reap_process, request_process_cpu_evaluation, send_signal_to_process,
    send_signal_to_process_data, send_signal_to_thread, user::linux_pid_from_task_id,
};
use crate::{
    keyring::{self, KeyTaskOwner},
    mm::{
        AddrSpace, AddressSpaceToken, UserMemoryCapability, map_usercopy_error,
        try_read_user_u32_nofault_locked,
    },
    pseudofs::cgroup,
    syscall::acct_process_exit,
};

#[cfg(not(test))]
type RegistryMutex<T> = axsync::Mutex<T>;
// Host unit tests do not initialize an axtask current-task slot, which the
// sleepable production mutex requires even when uncontended.
#[cfg(test)]
type RegistryMutex<T> = spin::Mutex<T>;

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

static TASK_TABLE: Lazy<RegistryMutex<WeakRegistry<WeakAxTaskRef>>> =
    Lazy::new(|| RegistryMutex::new(WeakRegistry::new()));
static LIVE_THREADS: AtomicUsize = AtomicUsize::new(0);
static TASK_ALIAS_TABLE: Lazy<RegistryMutex<WeakRegistry<WeakAxTaskRef>>> =
    Lazy::new(|| RegistryMutex::new(WeakRegistry::new()));

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

/// Releases the ptrace/process action gate before the full-image callback. The
/// callback result is returned so retired owners can remain live through the
/// later exec/vfork gate release instead of being destroyed inside this call.
pub(crate) fn release_exec_action_then_complete<A, R>(
    action: A,
    complete: impl FnOnce() -> R,
) -> R {
    drop(action);
    complete()
}

static PROCESS_TABLE: Lazy<RegistryMutex<WeakRegistry<Weak<ProcessData>>>> =
    Lazy::new(|| RegistryMutex::new(WeakRegistry::new()));

fn sigchld_autoreap_policy(
    disposition: &SignalDisposition,
    flags: SignalActionFlags,
) -> (bool, bool) {
    let ignored = matches!(disposition, SignalDisposition::Ignore);
    let no_cldwait = flags.contains(SignalActionFlags::NOCLDWAIT);
    (ignored || no_cldwait, ignored)
}

fn parent_sigchld_autoreap(parent: &ProcessData) -> (bool, bool) {
    let action = parent.signal.action(Signo::SIGCHLD);
    sigchld_autoreap_policy(&action.disposition, action.flags)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildExitCompletionStep {
    Notify,
    Reap,
}

fn child_exit_completion_steps(
    auto_reap: bool,
    suppress_exit_signal: bool,
) -> [Option<ChildExitCompletionStep>; 2] {
    [
        (!suppress_exit_signal).then_some(ChildExitCompletionStep::Notify),
        auto_reap.then_some(ChildExitCompletionStep::Reap),
    ]
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

    for step in child_exit_completion_steps(auto_reap, suppress_exit_signal)
        .into_iter()
        .flatten()
    {
        match step {
            ChildExitCompletionStep::Notify => {
                if let Some(signo) = child.exit_signal().and_then(Signo::from_repr) {
                    let _ =
                        send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(signo)));
                }
            }
            ChildExitCompletionStep::Reap => match reap_process(child) {
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
            },
        }
    }
    parent_data.child_exit_event.wake();
}

#[allow(dead_code)] // reached through the feature-gated memtrack cleanup hook
fn cleanup_registry<W: RegistryWeak>(registry: &RegistryMutex<WeakRegistry<W>>) {
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

fn cleanup_registry_if_due<W: RegistryWeak>(registry: &RegistryMutex<WeakRegistry<W>>) {
    let stale = {
        let mut registry = registry.lock();
        registry.take_one_stale_if_due()
    };
    drop(stale);
}

struct RegistryCleanupGuard<'a, W: RegistryWeak>(&'a RegistryMutex<WeakRegistry<W>>);

impl<W: RegistryWeak> Drop for RegistryCleanupGuard<'_, W> {
    fn drop(&mut self) {
        cleanup_registry_if_due(self.0);
    }
}

/// Stores one user address-space token in a task context.
pub fn set_task_user_address_space(
    ctx: &mut axhal::context::TaskContext,
    token: AddressSpaceToken,
) {
    #[cfg(all(feature = "asid-fast-switch", target_arch = "x86_64"))]
    unsafe {
        ctx.set_page_table_root_with_asid(
            token.root(),
            token.asid(),
            token.generation(),
            token.fallback_reason(),
        );
    }

    #[cfg(not(all(feature = "asid-fast-switch", target_arch = "x86_64")))]
    ctx.set_page_table_root(token.root());
}

#[cfg(all(feature = "asid-fast-switch", target_arch = "x86_64"))]
#[inline]
fn legal_current_user_address_space_identity(token: AddressSpaceToken) -> bool {
    let root = token.root().as_usize();
    token.asid() != 0
        && token.asid() < 4096
        && token.generation() != 0
        && matches!(
            token.fallback_reason(),
            axhal::context::AddressSpaceFallbackReason::None
        )
        && root & 0xfff == 0
        && root < (1usize << 52)
}

fn install_current_user_address_space(curr_ptr: *mut TaskInner, token: AddressSpaceToken) {
    #[cfg(all(feature = "asid-fast-switch", target_arch = "x86_64"))]
    let (identity_is_legal, switch_decision) = {
        // Snapshot the live context before replacing it.  Classifying only
        // the target would let stale current generation/fallback metadata
        // reach the NOFLUSH path.
        let (current_root, current_pcid, current_generation, current_fallback) = unsafe {
            let ctx = (*curr_ptr).ctx_mut();
            (
                ctx.cr3.as_usize(),
                ctx.cr3_pcid,
                ctx.cr3_generation,
                ctx.cr3_fallback_reason,
            )
        };
        let target_is_legal = legal_current_user_address_space_identity(token);
        let decision = axhal::asm::classify_user_tlb_switch(
            current_root,
            current_pcid,
            current_generation,
            current_fallback,
            token.root().as_usize(),
            token.asid(),
            token.generation(),
            token.fallback_reason(),
        );
        (target_is_legal, decision)
    };

    unsafe {
        set_task_user_address_space((*curr_ptr).ctx_mut(), token);
        #[cfg(all(feature = "asid-fast-switch", target_arch = "x86_64"))]
        match switch_decision {
            axhal::asm::UserTlbSwitchDecision::Retain if identity_is_legal => {
                axhal::asm::write_user_page_table_with_asid(token.root(), token.asid());
            }
            axhal::asm::UserTlbSwitchDecision::Flush(_)
                if token.asid() != 0 && token.asid() < 4096 =>
            {
                // A structurally valid nonzero target uses the target-PCID
                // flush form, even when its generation/fallback metadata is
                // invalid. The low-level helper rejects malformed roots and
                // falls back to the conservative PCID-0 full path.
                axhal::asm::write_user_page_table_with_asid_flush(token.root(), token.asid());
            }
            _ => {
                // ASID 0 or malformed target metadata takes the ordinary
                // CR3=PCID-0 full-flush path; it never receives NOFLUSH.
                axhal::asm::write_user_page_table(token.root());
            }
        }

        #[cfg(not(all(feature = "asid-fast-switch", target_arch = "x86_64")))]
        axhal::asm::write_user_page_table(token.root());
    }
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

fn reserve_unique_task_key(
    table: &RegistryMutex<WeakRegistry<WeakAxTaskRef>>,
    key: Pid,
) -> AxResult<bool> {
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
        account_published_thread();

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
        committing: CommittingExecCredential<'a>,
        new_aspace: Arc<Mutex<AddrSpace>>,
        new_access_state: Arc<ProcessAccessState>,
    ) -> ExecImageCommit<'a> {
        debug_assert!(core::ptr::eq(self.task.as_thread(), thread));
        let alias = self.alias;
        let task = &self.task;
        commit_exec_alias_publication(
            &mut self.committed,
            || {
                proc_data.publish_exec_image(
                    owner,
                    thread,
                    committing,
                    new_aspace,
                    new_access_state,
                )
            },
            |aliases| {
                thread.set_tid(alias);
                aliases.insert_reserved(alias, task)
            },
        )
    }
}

/// Runs the non-leader image and alias publication under the real alias-table
/// critical section. Keeping this sequencing in one helper lets host tests
/// exercise the lock/notification/retirement boundary without constructing a
/// scheduler-owned `AxTaskRef` or publishing global process runtime state.
fn commit_exec_alias_publication<R>(
    committed: &mut bool,
    publish_image: impl FnOnce() -> R,
    publish_alias: impl FnOnce(&mut WeakRegistry<WeakAxTaskRef>) -> Option<WeakAxTaskRef>,
) -> R {
    let mut aliases = TASK_ALIAS_TABLE.lock();
    #[cfg(test)]
    let alias_lock_probe = TaskAliasLockProbe::new();
    let retirement = publish_image();
    let old = publish_alias(&mut aliases);
    *committed = true;
    drop(aliases);
    #[cfg(test)]
    drop(alias_lock_probe);
    drop(old);
    retirement
}

#[cfg(test)]
pub(in crate::task) fn commit_exec_alias_publication_for_test<R>(
    publish_image: impl FnOnce() -> R,
    publish_alias: impl FnOnce(),
) -> R {
    let mut committed = false;
    let retirement = commit_exec_alias_publication(&mut committed, publish_image, |_aliases| {
        publish_alias();
        None
    });
    assert!(committed);
    retirement
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
    committing: CommittingExecCredential<'a>,
    new_aspace: Arc<Mutex<AddrSpace>>,
    new_access_state: Arc<ProcessAccessState>,
) -> ExecImageCommit<'a> {
    if let Some(admission) = admission {
        admission.commit_exec_handoff(
            proc_data,
            owner,
            thread,
            committing,
            new_aspace,
            new_access_state,
        )
    } else {
        proc_data.publish_exec_image(owner, thread, committing, new_aspace, new_access_state)
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
    registry: &RegistryMutex<WeakRegistry<W>>,
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

/// Returns the current number of live thread objects without allocating a
/// snapshot.  Linux's `sysinfo.procs` is `nr_threads`, not process groups.
pub fn live_thread_count() -> usize {
    LIVE_THREADS.load(Ordering::Acquire)
}

pub(crate) fn account_published_thread() {
    LIVE_THREADS.fetch_add(1, Ordering::Release);
}
pub(crate) fn account_released_thread() {
    let _ = LIVE_THREADS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
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
    set_current_user_address_space(AddressSpaceToken::legacy(root));
}

/// Updates the current task's saved and active user address-space token.
pub fn set_current_user_address_space(token: AddressSpaceToken) {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    let tlb_state = curr.as_thread().proc_data.aspace_tlb_state();
    // SAFETY: this only mutates the current task's saved TaskContext while the task
    // is running on the current CPU, so no other code can access it concurrently.
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    install_current_user_address_space(curr_ptr, token);
    // Exec publishes the new image before changing the live hardware root.
    // Re-admit the current CPU immediately; the regular scheduler hook will
    // keep this membership idempotent on the next switch.
    tlb_state.enter_current();
}

/// Resets the current task's saved and live FP/SIMD state after `execve`.
///
/// The no-preemption/IRQ guard is required because `TaskContext` is normally
/// accessed by the scheduler during a context switch. The architecture hook
/// updates both the saved scheduler image and the registers currently owned by
/// this CPU, so the next executable cannot inherit the old image's FP state.
pub fn reset_current_task_extended_state() {
    super::signal::reset_current_legacy_fp_state();
}

/// Returns CET state saved for the running task.
#[cfg(target_arch = "x86_64")]
pub fn current_user_cet_state() -> axhal::asm::UserCetState {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    // SAFETY: preemption is disabled and this is the running task.
    unsafe {
        (*((&***curr) as *const TaskInner as *mut TaskInner))
            .ctx_mut()
            .user_cet
    }
}

/// Snapshots the current task's live CET MSRs and synchronizes the scheduler
/// image before a kernel path relies on PL3_SSP.  On hosted builds CET MSRs
/// are deliberately unavailable, so the saved test image remains authoritative.
#[cfg(target_arch = "x86_64")]
pub fn current_user_live_cet_state() -> axhal::asm::UserCetState {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    // SAFETY: preemption is disabled and this is the running task.
    let context = unsafe { (*((&***curr) as *const TaskInner as *mut TaskInner)).ctx_mut() };
    #[cfg(target_os = "none")]
    {
        if context.user_cet.u_cet == 0 {
            return context.user_cet;
        }
        let mut live = axhal::asm::read_user_cet_state();
        live.locked = context.user_cet.locked;
        context.user_cet = live;
        live
    }
    #[cfg(not(target_os = "none"))]
    {
        context.user_cet
    }
}

/// Replaces CET state for the running task and its live CPU image.
#[cfg(target_arch = "x86_64")]
pub fn set_current_user_cet_state(state: axhal::asm::UserCetState) {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    // SAFETY: preemption is disabled and this is the running task.
    unsafe {
        (*((&***curr) as *const TaskInner as *mut TaskInner))
            .ctx_mut()
            .set_current_user_cet_state(state);
    }
}

/// Clears CET state after an exec image replacement.
#[cfg(target_arch = "x86_64")]
pub fn reset_current_user_cet_state() {
    set_current_user_cet_state(axhal::asm::UserCetState::default());
}

struct ProcessPtraceExitRetirements {
    _traced_relationship: Option<PtraceRelationshipSnapshot>,
    _reverse_links: super::process::PtraceReverseLinkDrain,
}

#[derive(Default)]
struct ExitPtraceRetirements {
    thread_reverse_links: Option<super::process::PtraceReverseLinkDrain>,
    process: Option<ProcessPtraceExitRetirements>,
}

fn detach_ptrace_links_on_process_exit(proc_data: &ProcessData) -> ProcessPtraceExitRetirements {
    let pid = proc_data.proc.pid();
    let traced_relationship = {
        let ptrace_action = proc_data.lock_ptrace_actions();
        let relationship = proc_data.clear_ptrace();
        drop(ptrace_action);
        relationship
    };
    if let Some(relationship) = traced_relationship.as_ref() {
        let session = relationship.session();
        if let Ok(tracer_data) = get_process_data(session.tracer) {
            tracer_data.remove_ptrace_tracee(super::PtraceReverseLink::new(pid, session));
        }
    }

    let reverse_links = detach_ptrace_reverse_links(proc_data.clear_ptrace_tracees());
    ProcessPtraceExitRetirements {
        _traced_relationship: traced_relationship,
        _reverse_links: reverse_links,
    }
}

fn detach_ptrace_reverse_links(
    mut links: super::process::PtraceReverseLinkDrain,
) -> super::process::PtraceReverseLinkDrain {
    while links.retain_next_retirement(|link| {
        let Ok(tracee_data) = get_process_data(link.tracee()) else {
            return None;
        };
        let ptrace_action = tracee_data.lock_ptrace_actions();
        let retired_relationship = tracee_data.end_ptrace(link.session());
        drop(ptrace_action);
        retired_relationship
    }) {}
    links
}

fn detach_ptrace_links_on_thread_exit(
    proc_data: &ProcessData,
    tracer_kernel_tid: Pid,
) -> super::process::PtraceReverseLinkDrain {
    detach_ptrace_reverse_links(proc_data.clear_ptrace_tracees_for_task(tracer_kernel_tid))
}

/// Poll the timer
pub fn poll_timer(task: &TaskInner) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let usage = {
        let _guard = NoPreemptIrqSave::new();
        let mut time = thr.time.borrow_mut();
        time.poll(&thr.proc_data);
        let (utime, stime) = time.output();
        TaskUsage::from_time_values(utime, stime)
    };
    thr.store_usage_snapshot(usage);
    if let Some(cpu) = request_process_cpu_evaluation(&thr.proc_data) {
        crate::deferred_work::wake_process_timer_worker(cpu);
    }
}

/// Sets the timer state.
pub fn set_timer_state(task: &TaskInner, state: TimerState) -> bool {
    let Some(thr) = task.try_as_thread() else {
        return false;
    };
    let usage = {
        let _guard = NoPreemptIrqSave::new();
        let mut time = thr.time.borrow_mut();
        time.poll(&thr.proc_data);
        time.set_state(state);
        let (utime, stime) = time.output();
        TaskUsage::from_time_values(utime, stime)
    };
    thr.store_usage_snapshot(usage);
    let timer_work_published = request_process_cpu_evaluation(&thr.proc_data);
    if let Some(cpu) = timer_work_published {
        crate::deferred_work::wake_process_timer_worker(cpu);
    }
    timer_work_published.is_some()
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

fn handle_futex_death(
    memory: &UserMemoryCapability,
    entry: *mut RobustList,
    offset: i64,
) -> AxResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(AxError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| AxError::InvalidInput)?;
    let uaddr = address as *mut u32;
    let Some(key) = mark_robust_owner_died(memory, uaddr, current().as_thread().tid())? else {
        return Ok(());
    };

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

/// A robust-list exit must not spin forever behind a userspace futex updater.
const ROBUST_CAS_RETRIES: usize = 8;
const ROBUST_NOFAULT_RETRIES: usize = 4;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RobustOwnerDiedError {
    Retry,
    BadAddress,
    Contended,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RobustOwnerDiedResult {
    Updated,
    NotOwner,
    Contended,
}

/// Atomically changes one futex word to the owner-died state.
///
/// The caller has already resolved the user virtual address to a resident
/// physical page while holding the selected address-space lock.  Operating on
/// that page through an `AtomicU32` is the same compare/exchange protocol used
/// by the userspace futex owner handoff; a read followed by an ordinary write
/// could overwrite a new owner and lose its handoff.
fn atomic_robust_owner_died(word: &AtomicU32, tid: Pid) -> RobustOwnerDiedResult {
    let mut value = word.load(Ordering::Acquire);
    for _ in 0..ROBUST_CAS_RETRIES {
        let Some(next_value) = robust_owner_died_word(value, tid) else {
            return RobustOwnerDiedResult::NotOwner;
        };
        match word.compare_exchange(value, next_value, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return RobustOwnerDiedResult::Updated,
            Err(observed) => value = observed,
        }
    }
    RobustOwnerDiedResult::Contended
}

/// Performs the resident part of the robust-list owner-died update through
/// the exiting thread's explicitly selected image.  The image lock is held
/// from mapping validation through the physical translation and CAS, so the
/// futex key returned on success names the mapping that was actually updated.
fn try_mark_robust_owner_died(
    memory: &UserMemoryCapability,
    uaddr: *mut u32,
    tid: Pid,
) -> Result<Option<FutexKey>, RobustOwnerDiedError> {
    if !uaddr.is_aligned() {
        return Err(RobustOwnerDiedError::BadAddress);
    }
    let address = uaddr as usize;
    let Some(aspace) = memory.address_space().try_lock() else {
        return Err(RobustOwnerDiedError::Retry);
    };

    let value = try_read_user_u32_nofault_locked(&aspace, address, None, None).map_err(
        |error| match error {
            crate::mm::UserU32NofaultError::Retry => RobustOwnerDiedError::Retry,
            crate::mm::UserU32NofaultError::BadAddress => RobustOwnerDiedError::BadAddress,
        },
    )?;
    if robust_owner_died_word(value, tid).is_none() {
        return Ok(None);
    }

    if address.checked_add(core::mem::size_of::<u32>()).is_none() {
        return Err(RobustOwnerDiedError::BadAddress);
    }
    if !aspace.can_access_range(
        VirtAddr::from(address),
        core::mem::size_of::<u32>(),
        MappingFlags::USER | MappingFlags::WRITE,
    ) {
        return Err(RobustOwnerDiedError::BadAddress);
    }

    let page_start = VirtAddr::from(address).align_down_4k();
    let (paddr, pte_flags, _) = aspace
        .page_table()
        .query(page_start)
        .map_err(|_| RobustOwnerDiedError::BadAddress)?;
    // A writable VMA with a read-only leaf is the post-fork COW state.  Let
    // the bounded task-context recovery below populate it before retrying.
    if !pte_flags.contains(MappingFlags::WRITE) {
        return Err(RobustOwnerDiedError::Retry);
    }

    let page_offset = address - page_start.as_usize();
    debug_assert!(page_offset <= memory_addr::PAGE_SIZE_4K - core::mem::size_of::<u32>());
    let physical = paddr + page_offset;
    let word = unsafe { &*phys_to_virt(physical).as_mut_ptr().cast::<AtomicU32>() };
    match atomic_robust_owner_died(word, tid) {
        RobustOwnerDiedResult::Updated => Ok(Some(FutexKey::new(&aspace, address))),
        RobustOwnerDiedResult::NotOwner => Ok(None),
        RobustOwnerDiedResult::Contended => Err(RobustOwnerDiedError::Contended),
    }
}

/// Faults in one robust futex word in the explicitly selected image.  This is
/// only used after the nofault snapshot reports a recoverable retry; it is
/// bounded to the word's containing page and never consults `current()`.
fn fault_robust_owner_died_word(memory: &UserMemoryCapability, address: usize) -> AxResult<()> {
    let end = address
        .checked_add(core::mem::size_of::<u32>())
        .ok_or(AxError::BadAddress)?;
    let start = VirtAddr::from(address);
    let page_start = start.align_down_4k();
    let page_end = VirtAddr::from(crate::mm::checked_align_up_4k(end).ok_or(AxError::BadAddress)?);
    let mut aspace = memory.address_space().lock();
    if !aspace.contains_range(start, core::mem::size_of::<u32>())
        || !aspace.can_access_range(
            start,
            core::mem::size_of::<u32>(),
            MappingFlags::USER | MappingFlags::WRITE,
        )
    {
        return Err(AxError::BadAddress);
    }
    aspace.populate_area(
        page_start,
        page_end.sub_addr(page_start),
        MappingFlags::WRITE,
    )
}

/// Performs the robust-list owner-died update with bounded recovery.
///
/// `Retry` is not an invalid user address: it means the selected image needs a
/// task-context fault (or the address-space lock/CAS was contended).  Give it a
/// bounded number of attempts and report resource pressure if the word never
/// reaches a stable resident state; this keeps exit processing finite without
/// silently discarding a recoverable robust entry as `BadAddress`.
fn mark_robust_owner_died(
    memory: &UserMemoryCapability,
    uaddr: *mut u32,
    tid: Pid,
) -> AxResult<Option<FutexKey>> {
    let address = uaddr as usize;
    for attempt in 0..ROBUST_NOFAULT_RETRIES {
        match try_mark_robust_owner_died(memory, uaddr, tid) {
            Ok(result) => return Ok(result),
            Err(RobustOwnerDiedError::BadAddress) => return Err(AxError::BadAddress),
            Err(RobustOwnerDiedError::Retry) => {
                if attempt + 1 == ROBUST_NOFAULT_RETRIES {
                    return Err(AxError::ResourceBusy);
                }
                fault_robust_owner_died_word(memory, address)?;
                axtask::yield_now();
            }
            Err(RobustOwnerDiedError::Contended) => {
                if attempt + 1 == ROBUST_NOFAULT_RETRIES {
                    return Err(AxError::ResourceBusy);
                }
                axtask::yield_now();
            }
        }
    }
    Err(AxError::ResourceBusy)
}

fn is_robust_list_error(err: AxError) -> bool {
    matches!(
        err,
        AxError::BadAddress | AxError::InvalidInput | AxError::FilesystemLoop
    )
}

pub fn exit_robust_list(memory: &UserMemoryCapability, head: *const RobustListHead) {
    // Reference: https://elixir.bootlin.com/linux/v6.13.6/source/kernel/futex/core.c#L777

    let mut limit = ROBUST_LIST_LIMIT;

    let end_ptr = match (head as usize).checked_add(core::mem::offset_of!(RobustListHead, list)) {
        Some(end_ptr) => end_ptr,
        None => return,
    };
    let head = match memory.read_value(head).map_err(map_usercopy_error) {
        Ok(head) => head,
        Err(err) if is_robust_list_error(err) => return,
        Err(_) => return,
    };
    let mut entry = head.list.next;
    let offset = head.futex_offset;
    let pending = head.list_op_pending;

    while entry as usize != end_ptr {
        let next_entry = match memory
            .read_value(entry as *const RobustList)
            .map_err(map_usercopy_error)
        {
            Ok(next) => next.next,
            Err(err) if is_robust_list_error(err) => break,
            Err(_) => break,
        };
        if entry != pending {
            match handle_futex_death(memory, entry, offset) {
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
        let _ = handle_futex_death(memory, pending, offset);
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
    // Scheduler syscalls publish the durable process snapshot only after the
    // scheduler transaction succeeds.  Preserve that exact last publication
    // through final exit.  Re-sampling ambient `current()` here would create a
    // second authority after live process membership has already been removed
    // and after teardown has crossed blocking/context-switch boundaries.
    exit.commit_with_reparent_handoff(
        process.exit_code(),
        self_usage.into(),
        child_usage.into(),
        proc_data.group_leader_cred(),
        proc_data.group_leader_signal_owner(),
        |child| notify_reaper_of_inherited_zombie(&child),
        |batch| reparent_exact_children_from_core_batch(departing, task_parent_publication, batch),
    )
}

fn begin_group_exit(proc_data: &ProcessData, exit_code: i32) -> bool {
    proc_data.begin_group_exit(exit_code)
}

/// Linux `zap_other_threads()` targets only peers in the exiting thread
/// group. Queueing SIGKILL back to the caller would leave a second fatal
/// event behind after its exit transaction has already removed the TID.
const fn is_group_exit_peer(current_tid: Pid, candidate_tid: Pid) -> bool {
    candidate_tid != current_tid
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
    thr.set_proc_state_hint(ProcStateHint::None);
    let tid = linux_pid_from_task_id(curr.id().as_u64())?;
    let visible_tid = thr.tid();

    match curr.id_name() {
        Ok(name) => info!("{name} exit with code: {exit_code}"),
        Err(error) => info!(
            "Task({}) exit with code: {} (name unavailable: {})",
            curr.id().as_u64(),
            exit_code,
            error
        ),
    }
    let process = &thr.proc_data.proc;
    // Reserve the bounded terminal-retirement entry before group exit can
    // become irreversible.  A full queue must still be reported while the
    // caller can return without leaving the group-exit gate or peer SIGKILLs
    // behind.
    let seccomp_retirement_plan = thr.prepare_seccomp_exit_retirement()?;
    set_timer_state(&curr, TimerState::Kernel);
    // rseq teardown is purely kernel state. Linux does not attempt to repair
    // the registered user area while a task is exiting (the mapping may have
    // already disappeared), so this path must not perform a user write.
    thr.reset_rseq_on_exit();
    thr.proc_data.end_exec(tid);
    let started_group_exit = group_exit && begin_group_exit(&thr.proc_data, exit_code);
    if started_group_exit {
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for peer_tid in process.thread_ids() {
            if is_group_exit_peer(tid, peer_tid) {
                let _ = send_signal_to_thread(Some(process.pid()), peer_tid, Some(sig.clone()));
            }
        }
    }
    // Declared before the outer guards so every early unwind also releases
    // lifecycle/task-parent serialization before any retained credential.
    // Normal exit performs the same order explicitly at the end below.
    let mut ptrace_retirements = ExitPtraceRetirements::default();
    // Declared before the lifecycle guard so unwind also releases every outer
    // lock before a detached immutable filter chain can run its iterative Drop.
    let mut seccomp_retirement = None;
    // Completion below only clears and queues the exact old owner; it never
    // waits for a grace period while lifecycle serialization is held.
    let lifecycle = thr.proc_data.lock_process_lifecycle();
    let mut task_parent_publication = Some(lock_task_parent_publication());
    thr.proc_data.begin_usage_transition();
    // Preserve the leader's effective affinity while its zombie remains
    // PID-addressable. The axtask snapshot holds the affinity lock and
    // intersects active CPUs before the task is removed from the scheduler.
    if let Some(token) = thr.proc_data.scheduler_publication_token(thr.kernel_tid())
        && let Ok(mask) = axtask::task_allowed_active_cpus(&current())
    {
        thr.proc_data
            .publish_affinity_snapshot(thr.kernel_tid(), token, mask);
    }
    let final_exit = match remove_current_thread(process, tid, exit_code) {
        Ok(ThreadExitTransition::NotFound) => {
            fail_closed_exit(AxError::BadState);
        }
        Ok(ThreadExitTransition::LiveThreadsRemain) => {
            // Non-leader threads have no waitable zombie.  The core task-list
            // unlink is their release edge, regardless of scheduler Arc GC.
            account_released_thread();
            None
        }
        Ok(ThreadExitTransition::FinalThread(exit)) => Some(exit),
        Err(error) => {
            error!(
                "fatal exit transition failure for TID {} in process {} after irreversible setup: \
                 {}",
                tid,
                process.pid(),
                error
            );
            fail_closed_exit(error);
        }
    };
    // CET default stacks are task-owned VMAs in the mm, not Thread-drop
    // state. Retire the authoritative owner at the task-unhash edge while
    // this task still names its old image; a CLONE_VM peer has a distinct
    // record and is therefore untouched.
    #[cfg(target_arch = "x86_64")]
    {
        thr.clear_cet_signal_frames();
        let aspace = thr.proc_data.aspace();
        let wake = {
            let mut aspace = aspace.lock();
            aspace
                .take_cet_default_shadow_stack(thr.kernel_tid())
                .and_then(|owner| aspace.unmap(owner.start, owner.size).ok())
        };
        if let Some(wake) = wake {
            wake.finish();
        }
    }
    // The core unlink above is the sole Linux task ownership retirement edge;
    // scheduler Arc destruction is not allowed to define files_struct lifetime.
    // Keep the slot live while pivot_root walks its task snapshot. The task
    // table unlink above intentionally precedes this gate, avoiding a
    // task-table -> fs-publication lock inversion.
    let retired_fs_context = {
        let _fs_context_publication = fs_context_publication();
        thr.retire_fs_context()
    };
    let mut retired_fd_table = Some(thr.retire_fd_table());
    // Do not defer cwd/root release to scheduler Arc destruction.
    drop(retired_fs_context);
    if final_exit.is_none() {
        let retired = retired_fd_table
            .take()
            .expect("nonfinal exit retains files_struct");
        // A private files_struct must take the normal close path while this
        // exiting task still supplies the POSIX-lock owner context. Shared
        // CLONE_FILES tables retain another task owner and are left intact.
        if !retired.has_task_users() {
            let table = retired.table();
            let closed = crate::file::close_fd_table(&table)
                .unwrap_or_else(|_| fail_closed_exit(AxError::BadState));
            drop(closed);
            crate::file::inotify::wait_current_close_notifications();
        }
        drop(retired);
    }

    // A final admission owns the removed membership and restores it on Drop.
    // Bind the process-owned payload before any per-thread/process teardown.
    // A missing preallocated payload is an internal invariant failure after
    // thread removal; fail closed rather than returning a syscall error after
    // the irreversible group-exit setup above.
    let final_exit = final_exit
        .map(|exit| thr.proc_data.prepare_zombie_exit(exit))
        .transpose()
        .unwrap_or_else(|error| {
            fail_closed_exit(error);
        });
    let final_thread = final_exit.is_some();
    // A process PID must survive until its zombie is reaped, but every other
    // thread's namespace TID becomes reusable once its core membership is
    // gone. `exit_thread` above is that authoritative removal edge.
    if tid != process.pid() {
        thr.proc_data.pid_ns().release_exited_thread(tid);
    }
    if final_exit.is_some() {
        // Freeze shared generation before zombie publication. A concurrent
        // sender that prepared outside the endpoint lock must fail its commit
        // recheck, while existing shared records remain charged until reap.
        thr.proc_data.signal.retain_pending_only();
    }
    // The exact task is no longer a routing candidate. Preserve only an
    // exited group leader's private pending queue; ordinary dead threads are
    // fully deactivated. Action changes can still flush retained records.
    thr.signal
        .retire_registration(tid, visible_tid == process.pid());
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
    ptrace_retirements.thread_reverse_links = Some(detach_ptrace_links_on_thread_exit(
        &thr.proc_data,
        thr.kernel_tid(),
    ));

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

    // Exit-time usercopy belongs to the image of the thread being torn down,
    // not to whichever caller image happened to invoke this path.
    let exit_memory = UserMemoryCapability::new(thr.proc_data.aspace());
    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if !clear_child_tid.is_null() {
        // Linux attempts FUTEX_WAKE even when clearing the user word faults.
        // Both operations are best-effort during terminal task teardown.
        let _ = exit_memory
            .write_value(clear_child_tid, 0u32)
            .map_err(map_usercopy_error);
        let key = FutexKey::new_current(clear_child_tid as usize);
        let table = futex_table_for(&key);
        let guard = table.get(&key);
        if let Some(futex) = guard {
            futex.wq.wake(1, u32::MAX);
        }
    }
    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null() {
        exit_robust_list(&exit_memory, head);
    }

    // The axtask Exit edge is the real TASK_DEAD schedule switch.  It runs
    // after this teardown returns, so pre-account its voluntary switch before
    // freezing the usage ledger; the scheduler callback consumes the marker
    // and does not double-count it.
    thr.preaccount_exit_context_switch();
    thr.proc_data
        .account_exited_thread(TaskUsage::from_thread(thr));
    thr.proc_data.end_usage_transition();
    if visible_tid != tid {
        remove_task_alias(visible_tid);
    }
    if let Some(exit) = final_exit {
        // Drop resident anonymous memory before the zombie waits to be reaped.
        // CLONE_VM can share this mm with a different live process, so only do
        // this when the exiting process is the sole ProcessData owner. The VMA
        // metadata and file mappings are still released by AddrSpace drop.
        // Sample first: discarding private pages below must not erase the
        // process high-water mark that Linux retains in its signal struct.
        let _ = thr.proc_data.sample_maxrss_kb();
        let aspace = thr.proc_data.aspace();
        if Arc::strong_count(&aspace) == 2 {
            let mut aspace = aspace.lock();
            if let Ok(true) = aspace.begin_oom_reap() {
                let _ = aspace.oom_reap_private_pages();
                aspace.finish_oom_reap();
            }
        }
        let self_usage = thr.proc_data.self_usage();
        let child_usage = thr.proc_data.children_usage();

        acct_process_exit(&thr.proc_data, process.exit_code(), self_usage);
        thr.proc_data.release_executable();
        crate::syscall::cleanup_process_aio(process.pid());
        crate::syscall::cleanup_process_mqueue_notifications(process.pid());
        let closed_fds = retired_fd_table
            .take()
            .expect("final exit retains files_struct");
        // wait(2) may return as soon as the parent observes the zombie state.
        // Release process-owned POSIX locks before the old files_struct can
        // drop its descriptions. Their Drop path publishes IN_CLOSE/FAN_CLOSE
        // work, which must only become observable after those locks are gone.
        crate::file::release_process_fd_table(process.pid(), closed_fds.table());
        // Retire the slot itself before close notifications or zombie
        // publication: it is the remaining table Arc after the lock release.
        drop(closed_fds);
        crate::file::inotify::wait_current_close_notifications();
        ptrace_retirements.process = Some(detach_ptrace_links_on_process_exit(&thr.proc_data));
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

        for step in child_exit_completion_steps(auto_reap, suppress_exit_signal)
            .into_iter()
            .flatten()
        {
            match step {
                ChildExitCompletionStep::Notify => {
                    if let Some(parent) = parent.as_ref()
                        && let Some(signo) = thr.proc_data.exit_signal
                    {
                        let _ = send_signal_to_process(
                            parent.pid(),
                            Some(SignalInfo::new_kernel(signo)),
                        );
                    }
                }
                ChildExitCompletionStep::Reap => match reap_process(process) {
                    Ok(true) => cgroup::detach_process(process.pid()),
                    Ok(false) => error!(
                        "process {} was already reaped during final autoreap",
                        process.pid()
                    ),
                    Err(error) => {
                        error!("failed to autoreap process {}: {}", process.pid(), error)
                    }
                },
            }
        }
        if let Some(data) = parent_data {
            data.child_exit_event.wake();
        }
        thr.proc_data.exit_event.wake();
        thr.proc_data.release_vfork();
    }
    // Publish exact task exit before releasing lifecycle serialization. A
    // thread-pidfd resolver must never observe removed membership paired with
    // a still-live task flag and retry the retired task identity.
    thr.set_exit();
    // The task is now terminal while lifecycle serialization still excludes
    // clone/TSYNC-style publishers. Only detach under that ordering boundary;
    // the returned chain is destroyed after every outer guard below.
    assert!(
        seccomp_retirement.is_none(),
        "seccomp exit ownership retired more than once"
    );
    seccomp_retirement = Some(
        thr.complete_seccomp_exit_retirement(seccomp_retirement_plan)
            .unwrap_or_else(|error| fail_closed_exit(error)),
    );
    // Both non-final and final paths have released their graph gate by here.
    // Keep this defensive take adjacent to lifecycle release so future exit
    // edits cannot accidentally move credential free callbacks back under it.
    drop(task_parent_publication.take());
    drop(lifecycle);
    if final_thread {
        // VT_PROCESS owners are process-scoped.  Do this only after the
        // final process-exit transaction has released its lifecycle locks:
        // notifying a pending VT switch can route a signal to another task.
        crate::pseudofs::dev::tty::notify_vt_owner_exit(process.pid());
    }
    keyring::exit_committed(
        KeyTaskOwner::new(thr.kernel_tid(), process.pid()),
        final_thread,
    )
    .unwrap_or_else(|error| fail_closed_exit(error));
    drop(ptrace_retirements);
    drop(seccomp_retirement.take());
    thr.proc_data.exec_event.wake();
    thr.exit_event.wake();
    Ok(())
}

/// Stops the machine after an unrecoverable internal exit-transaction fault.
///
/// User-issued exit syscalls return typed errors only while exit is still
/// reversible. Once timer/rseq/group-exit or membership removal has begun,
/// the caller cannot safely resume userspace after an internal fault, so this
/// explicit fail-closed policy prevents a partially exited task from running.
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
    fn robust_owner_died_update_is_atomic_and_does_not_clobber_handoff() {
        let tid = 42;
        let word = AtomicU32::new(FUTEX_WAITERS | tid);
        assert_eq!(
            atomic_robust_owner_died(&word, tid),
            RobustOwnerDiedResult::Updated
        );
        assert_eq!(
            word.load(Ordering::Acquire),
            FUTEX_WAITERS | FUTEX_OWNER_DIED
        );

        word.store(7, Ordering::Release);
        assert_eq!(
            atomic_robust_owner_died(&word, tid),
            RobustOwnerDiedResult::NotOwner
        );
        assert_eq!(word.load(Ordering::Acquire), 7);
    }

    #[test]
    fn process_lifecycle_busy_preserves_resource_busy() {
        assert_eq!(
            process_lifecycle_error(ProcessError::Busy),
            AxError::ResourceBusy
        );
    }

    #[test]
    fn group_exit_signals_only_peer_threads() {
        assert!(!is_group_exit_peer(17, 17));
        assert!(is_group_exit_peer(17, 18));
    }

    #[test]
    fn sigchld_autoreap_policy_distinguishes_default_ignore_and_no_cldwait() {
        let none = SignalActionFlags::empty();
        let no_cldwait = SignalActionFlags::NOCLDWAIT;

        assert_eq!(
            sigchld_autoreap_policy(&SignalDisposition::Default, none),
            (false, false)
        );
        assert_eq!(
            sigchld_autoreap_policy(&SignalDisposition::Ignore, none),
            (true, true)
        );
        assert_eq!(
            sigchld_autoreap_policy(&SignalDisposition::Default, no_cldwait),
            (true, false)
        );
        assert_eq!(
            sigchld_autoreap_policy(&SignalDisposition::Handler(0x1000), no_cldwait),
            (true, false)
        );
    }

    #[test]
    fn sigchld_inherited_no_cldwait_notifies_before_reap() {
        assert_eq!(
            child_exit_completion_steps(true, false),
            [
                Some(ChildExitCompletionStep::Notify),
                Some(ChildExitCompletionStep::Reap),
            ]
        );
        assert_eq!(
            child_exit_completion_steps(true, true),
            [None, Some(ChildExitCompletionStep::Reap)]
        );
        assert_eq!(
            child_exit_completion_steps(false, false),
            [Some(ChildExitCompletionStep::Notify), None]
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
