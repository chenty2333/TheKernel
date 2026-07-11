use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    cell::RefCell,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::spin::SpinNoIrq;
use axtask::{TaskExt, TaskInner};
use extern_trait::extern_trait;
use scope_local::{ActiveScope, Scope};
use starry_process::Pid;
use starry_signal::{
    SignalInfo,
    api::{ThreadSignalManager, ThreadSignalRegistration},
};

use super::{
    ProcessData,
    accounting::{AtomicTaskUsage, TaskUsage},
    creds::{Cred, CredentialSlot},
    restart::RestartTracker,
    timer::TimeManager,
};
use crate::deferred_work::DeferredWorkAccount;

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

    /// Atomically published immutable security identity owned by this task.
    ///
    /// A new thread or fork child starts from one caller snapshot, but owns an
    /// independent slot so later set-ID, capability, or prctl commits affect
    /// only this task.
    pub(in crate::task) credential: Arc<CredentialSlot>,

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
    /// Best-effort CPU usage snapshot that can be sampled without touching
    /// the live time manager.
    live_usage: AtomicTaskUsage,
    /// Best-effort user-visible blocking state used by procfs.
    proc_state_hint: AtomicU8,

    /// The OOM score adjustment value.
    oom_score_adj: AtomicI32,

    /// Ready to exit
    pub exit: Arc<AtomicBool>,

    /// Indicates whether the thread is currently accessing user memory.
    accessing_user_memory: AtomicBool,

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

impl Thread {
    /// Create a new [`Thread`].
    pub(crate) fn try_new(
        tid: u32,
        proc_data: Arc<ProcessData>,
        credential: Arc<Cred>,
    ) -> AxResult<(Box<Self>, ThreadSignalRegistration)> {
        let signal = ThreadSignalManager::try_new(proc_data.signal.clone())
            .map_err(|_| AxError::NoMemory)?;
        let exit = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let exit_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let deferred_work =
            Arc::try_new(DeferredWorkAccount::new()).map_err(|_| AxError::NoMemory)?;
        let restart = RestartTracker::try_new().map_err(|_| AxError::NoMemory)?;
        let credential =
            Arc::try_new(CredentialSlot::new(credential)).map_err(|_| AxError::NoMemory)?;
        let thread = Box::try_new(Thread {
            signal,
            proc_data,
            credential,
            clear_child_tid: AtomicUsize::new(0),
            visible_tid: AtomicU32::new(tid),
            robust_list_head: AtomicUsize::new(0),
            time: AssumeSync(RefCell::new(TimeManager::new())),
            live_usage: AtomicTaskUsage::new(),
            proc_state_hint: AtomicU8::new(ProcStateHint::None as u8),
            exit,
            oom_score_adj: AtomicI32::new(200),
            accessing_user_memory: AtomicBool::new(false),
            active_scope_read_held: AtomicBool::new(false),
            restart: SpinNoIrq::new(restart),
            exit_event,
            deferred_work,
        })
        .map_err(|_| AxError::NoMemory)?;
        let registration = thread
            .signal
            .try_register(tid)
            .map_err(|_| AxError::NoMemory)?;
        Ok((thread, registration))
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

    /// Returns the last published CPU usage snapshot for this thread.
    pub fn usage_snapshot(&self) -> TaskUsage {
        self.live_usage.snapshot()
    }

    /// Publishes a CPU usage snapshot for lock-free readers such as procfs.
    pub fn store_usage_snapshot(&self, usage: TaskUsage) {
        self.live_usage.store(usage);
    }

    /// Returns the current procfs state hint.
    pub(crate) fn proc_state_hint(&self) -> ProcStateHint {
        ProcStateHint::from(self.proc_state_hint.load(Ordering::Acquire))
    }

    /// Replaces the current procfs state hint and returns the previous value.
    pub(crate) fn swap_proc_state_hint(&self, hint: ProcStateHint) -> ProcStateHint {
        ProcStateHint::from(self.proc_state_hint.swap(hint as u8, Ordering::AcqRel))
    }

    /// Restores the procfs state hint.
    pub(crate) fn set_proc_state_hint(&self, hint: ProcStateHint) {
        self.proc_state_hint.store(hint as u8, Ordering::Release);
    }

    fn pause_cpu_accounting_for_switch(&self, task: &TaskInner) {
        let mut signals = Vec::new();
        let usage = {
            let Ok(mut time) = self.time.try_borrow_mut() else {
                return;
            };
            time.pause_for_switch(&mut signals);
            let (utime, stime) = time.output();
            TaskUsage::from_time_values(utime, stime)
        };
        self.store_usage_snapshot(usage);
        for signo in signals {
            // TimeManager emits only fixed standard signals (SIGALRM,
            // SIGVTALRM, SIGPROF, SIGXCPU, or SIGKILL), so no queued RT record
            // or RLIMIT_SIGPENDING charge is possible here.
            if self
                .signal
                .send_unqueued_signal(SignalInfo::new_kernel(signo))
            {
                task.interrupt();
            }
        }
    }

    fn resume_cpu_accounting_after_switch(&self) {
        let Ok(mut time) = self.time.try_borrow_mut() else {
            return;
        };
        time.resume_after_switch();
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

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProcStateHint {
    None            = 0,
    Interruptible   = 1,
    Uninterruptible = 2,
}

impl From<u8> for ProcStateHint {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Interruptible,
            2 => Self::Uninterruptible,
            _ => Self::None,
        }
    }
}

#[extern_trait]
impl TaskExt for Box<Thread> {
    fn on_enter(&self, _task: &TaskInner) {
        self.acquire_active_scope_read();
        self.resume_cpu_accounting_after_switch();
    }

    fn on_leave(&self, task: &TaskInner) {
        self.pause_cpu_accounting_for_switch(task);
        ActiveScope::set_global();
        self.release_active_scope_read();
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
