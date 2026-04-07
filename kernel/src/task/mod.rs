//! User task management.

mod accounting;
pub(crate) mod coredump;
mod futex;
mod ops;
mod resources;
mod restart;
mod signal;
mod stat;
mod timer;
mod user;

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    cell::RefCell,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::{TaskExt, TaskInner};
use extern_trait::extern_trait;
use scope_local::{ActiveScope, Scope};
use spin::RwLock;
use starry_process::{Pid, Process};
use starry_signal::{
    Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};

pub(crate) use self::restart::*;
pub use self::{
    accounting::*, futex::*, ops::*, resources::*, signal::*, stat::*, timer::*, user::*,
};
use crate::mm::AddrSpace;

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

/// The inner data of a thread.
pub struct Thread {
    /// The process data shared by all threads in the process.
    pub proc_data: Arc<ProcessData>,

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

    /// The head of the robust list
    robust_list_head: AtomicUsize,

    /// The thread-level signal manager
    pub signal: Arc<ThreadSignalManager>,

    /// Time manager
    ///
    /// This is assumed to be `Sync` because it's only borrowed mutably during
    /// context switches, which is exclusive to the current thread.
    pub time: AssumeSync<RefCell<TimeManager>>,

    /// The OOM score adjustment value.
    oom_score_adj: AtomicI32,

    /// Ready to exit
    pub exit: Arc<AtomicBool>,

    /// Indicates whether the thread is currently accessing user memory.
    accessing_user_memory: AtomicBool,

    /// Syscall restart bookkeeping shared across normal execution and signal handlers.
    restart: SpinNoIrq<RestartTracker>,

    /// Self exit event
    pub exit_event: Arc<PollSet>,
}

impl Thread {
    /// Create a new [`Thread`].
    pub fn new(tid: u32, proc_data: Arc<ProcessData>) -> Box<Self> {
        Box::new(Thread {
            signal: ThreadSignalManager::new(tid, proc_data.signal.clone()),
            proc_data,
            clear_child_tid: AtomicUsize::new(0),
            visible_tid: AtomicU32::new(tid),
            robust_list_head: AtomicUsize::new(0),
            time: AssumeSync(RefCell::new(TimeManager::new())),
            exit: Arc::new(AtomicBool::new(false)),
            oom_score_adj: AtomicI32::new(200),
            accessing_user_memory: AtomicBool::new(false),
            restart: SpinNoIrq::new(RestartTracker::default()),
            exit_event: Arc::default(),
        })
    }

    /// Get the clear child tid field.
    pub fn clear_child_tid(&self) -> usize {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    /// Get the user-visible thread ID.
    pub fn tid(&self) -> Pid {
        self.visible_tid.load(Ordering::Acquire)
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
        self.exit.store(true, Ordering::Release);
    }

    /// Check if the thread is accessing user memory.
    pub fn is_accessing_user_memory(&self) -> bool {
        self.accessing_user_memory.load(Ordering::Acquire)
    }

    /// Set the accessing user memory flag.
    pub fn set_accessing_user_memory(&self, accessing: bool) {
        self.accessing_user_memory
            .store(accessing, Ordering::Release);
    }
}

#[extern_trait]
impl TaskExt for Box<Thread> {
    fn on_enter(&self) {
        let scope = self.proc_data.scope.read();
        unsafe { ActiveScope::set(&scope) };
        core::mem::forget(scope);
    }

    fn on_leave(&self) {
        ActiveScope::set_global();
        unsafe { self.proc_data.scope.force_read_decrement() };
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

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum StopState {
    Running  = 0,
    Stopping = 1,
    Stopped  = 2,
}

impl From<u8> for StopState {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Stopping,
            2 => Self::Stopped,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ContinueResult {
    None,
    CanceledStopping,
    ResumedStopped,
}

#[derive(Debug, Clone, Copy)]
struct JobControlState {
    state: StopState,
    stop_signal: u8,
    continued: bool,
    stop_reported: bool,
}

impl Default for JobControlState {
    fn default() -> Self {
        Self {
            state: StopState::Running,
            stop_signal: 0,
            continued: false,
            stop_reported: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ExecControlState {
    owner: Option<Pid>,
}

#[derive(Debug, Clone, Copy, Default)]
struct VforkControlState {
    parent_tid: Option<Pid>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Credentials {
    ruid: u32,
    euid: u32,
    suid: u32,
    rgid: u32,
    egid: u32,
    sgid: u32,
}

/// [`Process`]-shared data.
pub struct ProcessData {
    /// The process.
    pub proc: Arc<Process>,
    /// The executable path
    pub exe_path: RwLock<String>,
    /// The command line arguments
    pub cmdline: RwLock<Arc<Vec<String>>>,
    /// The virtual memory address space.
    // TODO: scopify
    aspace_handle: RwLock<Arc<Mutex<AddrSpace>>>,
    /// The resource scope
    pub scope: RwLock<Scope>,
    /// The user heap top
    heap_top: AtomicUsize,

    /// The resource limits
    pub rlim: RwLock<Rlimits>,

    /// The child exit wait event
    pub child_exit_event: Arc<PollSet>,
    /// Self exit event
    pub exit_event: Arc<PollSet>,
    /// Woken when exec de-thread state changes or a sibling exits.
    pub exec_event: Arc<PollSet>,
    /// The exit signal of the thread
    pub exit_signal: Option<Signo>,

    /// The process signal manager
    pub signal: Arc<ProcessSignalManager>,

    /// The futex table.
    futex_table: Arc<FutexTable>,

    /// The default mask for file permissions.
    umask: AtomicU32,
    /// Process credentials shared by all threads.
    creds: SpinNoIrq<Credentials>,

    /// CPU time accumulated from sibling threads that have already exited.
    exited_threads_usage: AtomicTaskUsage,
    /// CPU time accumulated from waited-for child subtrees.
    waited_children_usage: AtomicTaskUsage,

    /// Serializes wait* selection and consumption for this process.
    pub wait_lock: Mutex<()>,

    /// Job-control stop state shared by all threads in the process.
    job_ctl: SpinNoIrq<JobControlState>,
    /// Multi-thread exec coordination state.
    exec_ctl: SpinNoIrq<ExecControlState>,
    /// CLONE_VFORK coordination state.
    vfork_ctl: SpinNoIrq<VforkControlState>,
    /// Woken when threads should resume from stopped state.
    pub stop_event: Arc<PollSet>,
    /// Woken when a vfork child releases the parent.
    pub vfork_event: Arc<PollSet>,

    /// The network namespace (network stack) for this process.
    pub net_ns: Arc<axnet::NetStack>,
}

impl ProcessData {
    /// Create a new [`ProcessData`].
    pub fn new(
        proc: Arc<Process>,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        aspace: Arc<Mutex<AddrSpace>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        exit_signal: Option<Signo>,
        net_ns: Arc<axnet::NetStack>,
    ) -> Arc<Self> {
        Arc::new(Self {
            proc,
            exe_path: RwLock::new(exe_path),
            cmdline: RwLock::new(cmdline),
            aspace_handle: RwLock::new(aspace),
            scope: RwLock::new(Scope::new()),
            heap_top: AtomicUsize::new(crate::config::USER_HEAP_BASE),

            rlim: RwLock::default(),

            child_exit_event: Arc::default(),
            exit_event: Arc::default(),
            exec_event: Arc::default(),
            exit_signal,

            signal: Arc::new(ProcessSignalManager::new(
                signal_actions,
                crate::config::SIGNAL_TRAMPOLINE,
            )),

            futex_table: Arc::new(FutexTable::new()),

            umask: AtomicU32::new(0o022),
            creds: SpinNoIrq::new(Credentials::default()),
            exited_threads_usage: AtomicTaskUsage::new(),
            waited_children_usage: AtomicTaskUsage::new(),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            vfork_ctl: SpinNoIrq::new(VforkControlState::default()),
            stop_event: Arc::default(),
            vfork_event: Arc::default(),

            net_ns,
        })
    }

    /// Get the top address of the user heap.
    pub fn get_heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Returns the current address-space handle for this process.
    pub fn aspace(&self) -> Arc<Mutex<AddrSpace>> {
        self.aspace_handle.read().clone()
    }

    /// Rebinds the process to a new address-space handle and returns the old one.
    pub fn replace_aspace(&self, aspace: Arc<Mutex<AddrSpace>>) -> Arc<Mutex<AddrSpace>> {
        core::mem::replace(&mut *self.aspace_handle.write(), aspace)
    }

    /// Set the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }

    /// Linux manual: A "clone" child is one which delivers no signal, or a
    /// signal other than SIGCHLD to its parent upon termination.
    pub fn is_clone_child(&self) -> bool {
        self.exit_signal != Some(Signo::SIGCHLD)
    }

    /// Returns process CPU usage, including live threads and exited siblings.
    pub fn self_usage(&self) -> TaskUsage {
        live_process_usage(self)
    }

    /// Returns waited-for child CPU usage accumulated for this process.
    pub fn children_usage(&self) -> TaskUsage {
        self.waited_children_usage.snapshot()
    }

    /// Returns the total usage that should be published when this process exits.
    pub fn total_usage(&self) -> TaskUsage {
        self.self_usage().saturating_add(self.children_usage())
    }

    /// Records the final CPU usage of a thread that is exiting.
    pub fn account_exited_thread(&self, usage: TaskUsage) {
        self.exited_threads_usage.add(usage);
    }

    /// Records a waited-for child subtree into the process's child ledger.
    pub fn account_waited_child(&self, usage: TaskUsage) {
        self.waited_children_usage.add(usage);
    }

    /// Get the umask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Set the umask.
    pub fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::SeqCst);
    }

    /// Set the umask and return the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
    }

    pub(crate) fn credentials(&self) -> Credentials {
        *self.creds.lock()
    }

    pub(crate) fn set_credentials(&self, creds: Credentials) {
        *self.creds.lock() = creds;
    }

    pub fn uid(&self) -> u32 {
        self.creds.lock().ruid
    }

    pub fn euid(&self) -> u32 {
        self.creds.lock().euid
    }

    pub fn gid(&self) -> u32 {
        self.creds.lock().rgid
    }

    pub fn egid(&self) -> u32 {
        self.creds.lock().egid
    }

    pub fn setuid(&self, uid: u32) -> AxResult<()> {
        let mut creds = self.creds.lock();
        if creds.euid == 0 {
            creds.ruid = uid;
            creds.euid = uid;
            creds.suid = uid;
            return Ok(());
        }
        if uid == creds.ruid || uid == creds.suid {
            creds.euid = uid;
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub fn setgid(&self, gid: u32) -> AxResult<()> {
        let mut creds = self.creds.lock();
        if creds.egid == 0 {
            creds.rgid = gid;
            creds.egid = gid;
            creds.sgid = gid;
            return Ok(());
        }
        if gid == creds.rgid || gid == creds.sgid {
            creds.egid = gid;
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub fn setreuid(&self, ruid: Option<u32>, euid: Option<u32>) -> AxResult<()> {
        let mut creds = self.creds.lock();
        let old = *creds;
        if old.euid != 0 {
            for id in [ruid, euid].into_iter().flatten() {
                if id != old.ruid && id != old.euid && id != old.suid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        let new_ruid = ruid.unwrap_or(old.ruid);
        let new_euid = euid.unwrap_or(old.euid);
        creds.ruid = new_ruid;
        creds.euid = new_euid;
        if ruid.is_some() || euid.is_some_and(|id| id != old.ruid) {
            creds.suid = new_euid;
        }
        Ok(())
    }

    pub fn setresuid(
        &self,
        ruid: Option<u32>,
        euid: Option<u32>,
        suid: Option<u32>,
    ) -> AxResult<()> {
        let mut creds = self.creds.lock();
        let old = *creds;
        if old.euid != 0 {
            for id in [ruid, euid, suid].into_iter().flatten() {
                if id != old.ruid && id != old.euid && id != old.suid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = ruid {
            creds.ruid = id;
        }
        if let Some(id) = euid {
            creds.euid = id;
        }
        if let Some(id) = suid {
            creds.suid = id;
        }
        Ok(())
    }

    pub fn setresgid(
        &self,
        rgid: Option<u32>,
        egid: Option<u32>,
        sgid: Option<u32>,
    ) -> AxResult<()> {
        let mut creds = self.creds.lock();
        let old = *creds;
        if old.egid != 0 {
            for id in [rgid, egid, sgid].into_iter().flatten() {
                if id != old.rgid && id != old.egid && id != old.sgid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = rgid {
            creds.rgid = id;
        }
        if let Some(id) = egid {
            creds.egid = id;
        }
        if let Some(id) = sgid {
            creds.sgid = id;
        }
        Ok(())
    }

    fn stop_state(&self) -> StopState {
        self.job_ctl.lock().state
    }

    /// Returns whether the process is currently stopped.
    pub fn is_stopped(&self) -> bool {
        self.stop_state() == StopState::Stopped
    }

    /// Returns whether threads should park for a job-control stop.
    pub fn should_wait_for_stop(&self) -> bool {
        self.stop_state() != StopState::Running
    }

    /// Begins a job-control stop transition.
    pub fn begin_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopping;
        job_ctl.stop_signal = signo;
        true
    }

    /// Finalizes a stop transition if it has not been canceled by SIGCONT.
    pub fn finish_stop(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Stopping {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Resumes or cancels a job-control stop transition.
    pub(crate) fn continue_job(&self) -> ContinueResult {
        let result = {
            let mut job_ctl = self.job_ctl.lock();
            match job_ctl.state {
                StopState::Running => ContinueResult::None,
                StopState::Stopping => {
                    job_ctl.state = StopState::Running;
                    ContinueResult::CanceledStopping
                }
                StopState::Stopped => {
                    job_ctl.state = StopState::Running;
                    job_ctl.continued = true;
                    ContinueResult::ResumedStopped
                }
            }
        };
        if result != ContinueResult::None {
            self.stop_event.wake();
        }
        result
    }

    /// Atomically takes the continued flag (returns true at most once per continuation).
    pub fn take_continued(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        let continued = job_ctl.continued;
        job_ctl.continued = false;
        continued
    }

    /// Takes the current stopped status for waitpid reporting, if it has not been reported yet.
    pub fn take_stop_status(&self) -> Option<u8> {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            job_ctl.stop_reported = true;
            Some(job_ctl.stop_signal)
        } else {
            None
        }
    }

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    pub fn peek_stop_status(&self) -> Option<u8> {
        let job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            Some(job_ctl.stop_signal)
        } else {
            None
        }
    }

    /// Claims the pending stop report so a waiter can complete userspace copies first.
    pub fn claim_stop_status(&self) -> Option<u8> {
        self.take_stop_status()
    }

    /// Restores a previously claimed stop report after a failed userspace copy.
    pub fn restore_stop_status(&self, stop_signal: u8) {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && job_ctl.stop_signal == stop_signal {
            job_ctl.stop_reported = false;
        }
    }

    /// Peeks at the continued flag without consuming it (for WNOWAIT).
    pub fn peek_continued(&self) -> bool {
        self.job_ctl.lock().continued
    }

    /// Claims the pending continued report so a waiter can complete userspace copies first.
    pub fn claim_continued(&self) -> bool {
        self.take_continued()
    }

    /// Restores a previously claimed continued report after a failed userspace copy.
    pub fn restore_continued(&self) {
        let mut job_ctl = self.job_ctl.lock();
        job_ctl.continued = true;
    }

    /// Begins a multi-thread exec de-threading phase.
    pub fn begin_exec(&self, owner: Pid) -> bool {
        let mut exec_ctl = self.exec_ctl.lock();
        match exec_ctl.owner {
            Some(curr) => curr == owner,
            None => {
                exec_ctl.owner = Some(owner);
                true
            }
        }
    }

    /// Returns whether this thread should exit because another thread is committing execve().
    pub fn should_exit_for_exec(&self, tid: Pid) -> bool {
        matches!(self.exec_ctl.lock().owner, Some(owner) if owner != tid)
    }

    /// Returns whether the given thread still owns the in-flight exec.
    pub fn is_exec_owner(&self, tid: Pid) -> bool {
        self.exec_ctl.lock().owner == Some(tid)
    }

    /// Returns whether an exec de-thread phase is currently in progress.
    pub fn exec_in_progress(&self) -> bool {
        self.exec_ctl.lock().owner.is_some()
    }

    /// Adds a thread to the process unless an exec de-thread phase is already
    /// in progress.
    pub fn try_add_thread(&self, tid: Pid) -> bool {
        let exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner.is_some() {
            return false;
        }
        self.proc.add_thread(tid);
        true
    }

    /// Returns whether the thread group has drained to the exec owner only.
    pub fn exec_ready(&self, owner: Pid) -> bool {
        self.is_exec_owner(owner) && self.proc.threads().as_slice() == [owner]
    }

    /// Finishes or cancels the in-flight exec owned by `owner`.
    pub fn end_exec(&self, owner: Pid) {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner == Some(owner) {
            exec_ctl.owner = None;
            drop(exec_ctl);
            self.exec_event.wake();
        }
    }

    /// Marks the process as a vfork child whose parent thread must remain blocked.
    pub fn begin_vfork(&self, parent_tid: Pid) {
        self.vfork_ctl.lock().parent_tid = Some(parent_tid);
    }

    /// Returns whether an active CLONE_VFORK relationship is still blocking the parent.
    pub fn vfork_in_progress(&self) -> bool {
        self.vfork_ctl.lock().parent_tid.is_some()
    }

    /// Releases a blocked vfork parent after execve commits or the last thread exits.
    pub fn release_vfork(&self) {
        let mut vfork_ctl = self.vfork_ctl.lock();
        if vfork_ctl.parent_tid.take().is_some() {
            drop(vfork_ctl);
            self.vfork_event.wake();
        }
    }
}
