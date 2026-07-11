use alloc::{
    alloc::{AllocError, handle_alloc_error},
    boxed::Box,
    string::String,
    sync::Arc,
};
#[cfg(feature = "preempt")]
use core::sync::atomic::AtomicUsize;
use core::{
    alloc::Layout,
    cell::{Cell, UnsafeCell},
    fmt,
    future::poll_fn,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
    task::{Context, Poll},
};

use axhal::context::TaskContext;
#[cfg(feature = "tls")]
use axhal::tls::TlsArea;
use futures_util::task::AtomicWaker;
use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

use crate::{AxCpuMask, AxTask, AxTaskRef, future::block_on};

/// A unique identifier for a thread.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TaskId(u64);

/// The possible states of a task.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskState {
    /// Task is running on some CPU.
    Running = 1,
    /// Task is ready to run on some scheduler's ready queue.
    Ready   = 2,
    /// Task is blocked (in the wait queue or timer list),
    /// and it has finished its scheduling process, it can be wake up by `notify()` on any run queue safely.
    Blocked = 3,
    /// Task is exited and waiting for being dropped.
    Exited  = 4,
}

struct TaskAffinity {
    mask: AxCpuMask,
    #[cfg(feature = "smp")]
    pending_migration: Option<AxTaskRef>,
}

impl TaskAffinity {
    fn new(mask: AxCpuMask) -> Self {
        Self {
            mask,
            #[cfg(feature = "smp")]
            pending_migration: None,
        }
    }
}

#[cfg(feature = "smp")]
pub(crate) enum MigrationClaim {
    Allowed,
    Prepared(AxTaskRef),
    Missing,
}

/// User-defined task extended data.
#[cfg(feature = "task-ext")]
#[extern_trait::extern_trait(
    /// The impl proxy type for [`TaskExt`].
    pub AxTaskExt
)]
pub trait TaskExt {
    /// Called when the task is switched in.
    fn on_enter(&self, _task: &TaskInner) {}
    /// Called when the task is switched out.
    fn on_leave(&self, _task: &TaskInner) {}
}

/// The inner task structure.
pub struct TaskInner {
    id: TaskId,
    name: SpinNoIrq<String>,
    is_idle: bool,
    is_init: bool,

    entry: Cell<Option<Box<dyn FnOnce()>>>,
    state: AtomicU8,

    /// CPU affinity and its at-most-one pre-admitted migration helper are one
    /// publication domain. Observing a newly disallowed CPU therefore always
    /// implies that the no-allocation scheduling safe point can claim a helper.
    affinity: SpinNoIrq<TaskAffinity>,

    /// Used to indicate the CPU ID where the task is running or will run.
    cpu_id: AtomicU32,
    /// Used to indicate whether the task is running on a CPU.
    #[cfg(feature = "smp")]
    on_cpu: AtomicBool,

    #[cfg(feature = "preempt")]
    need_resched: AtomicBool,
    #[cfg(feature = "preempt")]
    preempt_disable_count: AtomicUsize,

    /// Prevents recursive deferred-work dispatch on this task.
    deferred_work_dispatching: AtomicBool,

    interrupted: AtomicBool,
    interrupt_waker: AtomicWaker,

    exit_code: AtomicI32,
    wait_for_exit: AtomicWaker,

    kstack: Option<TaskStack>,
    ctx: UnsafeCell<TaskContext>,

    #[cfg(feature = "task-ext")]
    task_ext: Option<AxTaskExt>,

    #[cfg(feature = "tls")]
    tls: TlsArea,
}

impl TaskId {
    fn new() -> Self {
        Self(ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Convert the task ID to a `u64`.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Returns the last task ID that will have been allocated by [`TaskId::new`].
pub fn last_task_id() -> u64 {
    ID_COUNTER.load(Ordering::Relaxed).saturating_sub(1)
}

/// Sets the last task ID used by [`TaskId::new`].
///
/// This backs Linux-compatible `/proc/sys/kernel/ns_last_pid` behavior. The
/// next allocated task receives `last + 1`.
pub fn set_last_task_id(last: u64) {
    ID_COUNTER.store(last.saturating_add(1), Ordering::Relaxed);
}

impl From<u8> for TaskState {
    #[inline]
    fn from(state: u8) -> Self {
        match state {
            1 => Self::Running,
            2 => Self::Ready,
            3 => Self::Blocked,
            4 => Self::Exited,
            _ => unreachable!(),
        }
    }
}

unsafe impl Send for TaskInner {}
unsafe impl Sync for TaskInner {}

impl TaskInner {
    /// Create a new task with the given entry function and stack size.
    pub fn new<F>(entry: F, name: String, stack_size: usize) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::try_new(entry, name, stack_size)
            .unwrap_or_else(|_| handle_alloc_error(Layout::new::<u8>()))
    }

    /// Fallibly creates an unpublished task, including its entry ownership and
    /// kernel stack.
    pub fn try_new<F>(entry: F, name: String, stack_size: usize) -> Result<Self, AllocError>
    where
        F: FnOnce() + Send + 'static,
    {
        let stack_size = stack_size
            .checked_add(4095)
            .map(|size| size & !4095)
            .filter(|size| *size != 0)
            .ok_or(AllocError)?;
        let mut t = Self::new_common(TaskId::new(), name);
        debug!("new task id: {}", t.id.as_u64());
        let kstack = TaskStack::try_alloc(stack_size)?;
        let entry = Box::try_new(entry)?;

        #[cfg(feature = "tls")]
        let tls = VirtAddr::from(t.tls.tls_ptr() as usize);
        #[cfg(not(feature = "tls"))]
        let tls = VirtAddr::from(0);

        t.entry = Cell::new(Some(entry));
        t.ctx_mut()
            .init(task_entry as *const () as usize, kstack.top(), tls);
        t.kstack = Some(kstack);
        if t.name.lock().as_str() == "idle" {
            t.is_idle = true;
        }
        Ok(t)
    }

    /// Gets the ID of the task.
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Gets the name of the task.
    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    /// Fallibly snapshots the task name without allocating under its spin lock.
    pub fn try_name(&self) -> Result<String, AllocError> {
        let mut name = String::new();
        loop {
            name.clear();
            let required = self.name.lock().len();
            if name.capacity() < required {
                name.try_reserve_exact(required).map_err(|_| AllocError)?;
            }
            let current = self.name.lock();
            if name.capacity() < current.len() {
                drop(current);
                continue;
            }
            name.push_str(&current);
            return Ok(name);
        }
    }

    /// Set the name of the task.
    pub fn set_name(&self, name: &str) {
        let name = String::from(name);
        drop(self.replace_name(name));
    }

    /// Replace the task name with an already-owned string.
    ///
    /// Constructing the replacement before taking the task-name lock keeps
    /// allocator work out of the spin-locked section. Returning the previous
    /// string likewise lets callers defer its destructor until after the lock
    /// has been released.
    pub fn replace_name(&self, name: String) -> String {
        core::mem::replace(&mut *self.name.lock(), name)
    }

    /// Get a combined string of the task ID and name.
    pub fn id_name(&self) -> alloc::string::String {
        alloc::format!("Task({}, {:?})", self.id.as_u64(), self.name())
    }

    /// Wait for the task to exit, and return the exit code.
    ///
    /// It will return immediately if the task has already exited (but not dropped).
    pub fn join(&self) -> i32 {
        block_on(poll_fn(|cx| self.poll_join(cx)))
    }

    fn exited_code(&self) -> Option<i32> {
        (self.state() == TaskState::Exited).then(|| self.exit_code.load(Ordering::Acquire))
    }

    fn poll_join(&self, cx: &mut Context<'_>) -> Poll<i32> {
        if let Some(exit_code) = self.exited_code() {
            return Poll::Ready(exit_code);
        }

        self.register_join_waiter_and_recheck(cx)
    }

    fn register_join_waiter_and_recheck(&self, cx: &mut Context<'_>) -> Poll<i32> {
        self.wait_for_exit.register(cx.waker());

        // Exit can race between the first state check and waker publication.
        // Rechecking after registration makes both interleavings safe: either
        // the registered waker observes a later exit, or this poll observes an
        // exit whose earlier wake had no waiter to consume it.
        self.exited_code().map_or(Poll::Pending, Poll::Ready)
    }

    #[cfg(test)]
    pub(crate) fn register_join_waiter_and_recheck_for_test(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<i32> {
        self.register_join_waiter_and_recheck(cx)
    }

    /// Returns a reference to the task extended data.
    #[cfg(feature = "task-ext")]
    pub fn task_ext(&self) -> Option<&AxTaskExt> {
        self.task_ext.as_ref()
    }

    /// Returns a mutable reference to the task extended data.
    #[cfg(feature = "task-ext")]
    pub fn task_ext_mut(&mut self) -> &mut Option<AxTaskExt> {
        &mut self.task_ext
    }

    /// Returns a mutable reference to the task context.
    #[inline]
    pub const fn ctx_mut(&mut self) -> &mut TaskContext {
        self.ctx.get_mut()
    }

    /// Returns the top address of the kernel stack.
    #[inline]
    pub const fn kernel_stack_top(&self) -> Option<VirtAddr> {
        match &self.kstack {
            Some(s) => Some(s.top()),
            None => None,
        }
    }

    /// Returns the CPU ID where the task is running or will run.
    ///
    /// Note: the task may not be running on the CPU, it just exists in the run queue.
    #[inline]
    pub fn cpu_id(&self) -> u32 {
        self.cpu_id.load(Ordering::Acquire)
    }

    /// Gets the cpu affinity mask of the task.
    ///
    /// Returns the cpu affinity mask of the task in type [`AxCpuMask`].
    #[inline]
    pub fn cpumask(&self) -> AxCpuMask {
        self.affinity.lock().mask
    }

    /// Sets the cpu affinity mask of the task.
    ///
    /// # Arguments
    /// `cpumask` - The cpu affinity mask to be set in type [`AxCpuMask`].
    #[inline]
    pub(crate) fn set_cpumask(&self, cpumask: AxCpuMask) {
        #[cfg(feature = "smp")]
        let displaced = self.publish_affinity(cpumask, None);
        #[cfg(not(feature = "smp"))]
        {
            self.affinity.lock().mask = cpumask;
        }
        #[cfg(feature = "smp")]
        drop(displaced);
    }

    /// Atomically publishes a new mask and its already allocated migration
    /// helper. The replaced helper is returned so its stack/entry are destroyed
    /// after the no-IRQ affinity lock has been released.
    #[cfg(feature = "smp")]
    pub(crate) fn publish_affinity(
        &self,
        cpumask: AxCpuMask,
        pending_migration: Option<AxTaskRef>,
    ) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        affinity.mask = cpumask;
        core::mem::replace(&mut affinity.pending_migration, pending_migration)
    }

    /// Claims the pre-admitted migration helper iff this CPU is no longer in
    /// the published mask. No allocation or destructor runs under the lock.
    #[cfg(feature = "smp")]
    pub(crate) fn claim_migration(&self, cpu_id: usize) -> MigrationClaim {
        let mut affinity = self.affinity.lock();
        if affinity.mask.get(cpu_id) {
            MigrationClaim::Allowed
        } else if let Some(task) = affinity.pending_migration.take() {
            MigrationClaim::Prepared(task)
        } else {
            MigrationClaim::Missing
        }
    }

    /// Clears only the caller's own still-pending helper. A concurrent later
    /// setaffinity publication cannot be mistaken for this token.
    #[cfg(feature = "smp")]
    pub(crate) fn clear_migration_if(&self, expected: &AxTaskRef) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        if affinity
            .pending_migration
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, expected))
        {
            affinity.pending_migration.take()
        } else {
            None
        }
    }

    /// Retires a helper which became unnecessary before it was claimed. The
    /// returned task must be dropped by a normal task-context caller after this
    /// lock has been released.
    #[cfg(feature = "smp")]
    pub(crate) fn take_allowed_migration(&self, cpu_id: usize) -> Option<AxTaskRef> {
        let mut affinity = self.affinity.lock();
        affinity
            .mask
            .get(cpu_id)
            .then(|| affinity.pending_migration.take())
            .flatten()
    }

    /// Polls whether the task has been interrupted.
    #[inline]
    pub fn poll_interrupt(&self, cx: &Context) -> Poll<()> {
        if self.interrupted.swap(false, Ordering::AcqRel) {
            Poll::Ready(())
        } else {
            self.interrupt_waker.register(cx.waker());
            if self.interrupted.swap(false, Ordering::AcqRel) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    /// Clears the interrupt state of the task.
    #[inline]
    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Returns whether the task has a pending interrupt wakeup.
    #[inline]
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Acquire)
    }

    /// Interrupts the task.
    #[inline]
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.interrupt_waker.wake();
    }
}

// private methods
impl TaskInner {
    fn new_common(id: TaskId, name: String) -> Self {
        Self {
            id,
            name: SpinNoIrq::new(name),
            is_idle: false,
            is_init: false,
            entry: Cell::new(None),
            state: AtomicU8::new(TaskState::Ready as u8),
            // By default, the task is allowed to run on all CPUs.
            affinity: SpinNoIrq::new(TaskAffinity::new(crate::api::cpu_mask_full())),
            cpu_id: AtomicU32::new(0),
            #[cfg(feature = "smp")]
            on_cpu: AtomicBool::new(false),
            #[cfg(feature = "preempt")]
            need_resched: AtomicBool::new(false),
            #[cfg(feature = "preempt")]
            preempt_disable_count: AtomicUsize::new(0),
            deferred_work_dispatching: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            interrupt_waker: AtomicWaker::new(),
            exit_code: AtomicI32::new(0),
            wait_for_exit: AtomicWaker::new(),
            kstack: None,
            ctx: UnsafeCell::new(TaskContext::new()),
            #[cfg(feature = "task-ext")]
            task_ext: None,
            #[cfg(feature = "tls")]
            tls: TlsArea::alloc(),
        }
    }

    /// Creates an "init task" using the current CPU states, to use as the
    /// current task.
    ///
    /// As it is the current task, no other task can switch to it until it
    /// switches out.
    ///
    /// And there is no need to set the `entry`, `kstack` or `tls` fields, as
    /// they will be filled automatically when the task is switches out.
    pub(crate) fn new_init(name: String) -> Self {
        let mut t = Self::new_common(TaskId::new(), name);
        t.is_init = true;
        #[cfg(feature = "smp")]
        t.set_on_cpu(true);
        if t.name() == "idle" {
            t.is_idle = true;
        }
        t
    }

    pub(crate) fn into_arc(self) -> AxTaskRef {
        Arc::new(AxTask::new(self))
    }

    /// Fallibly constructs the scheduler-owned task object without making it
    /// runnable. Lifecycle code can therefore complete every other admission
    /// step before publishing a user-visible handle to the task.
    pub(crate) fn try_into_arc(self) -> Result<AxTaskRef, alloc::alloc::AllocError> {
        Arc::try_new(AxTask::new(self))
    }

    /// Returns the current state of the task.
    #[inline]
    pub fn state(&self) -> TaskState {
        self.state.load(Ordering::Acquire).into()
    }

    #[inline]
    pub(crate) fn set_state(&self, state: TaskState) {
        self.state.store(state as u8, Ordering::Release)
    }

    /// Transition the task state from `current_state` to `new_state`,
    /// Returns `true` if the current state is `current_state` and the state is successfully set to `new_state`,
    /// otherwise returns `false`.
    #[inline]
    pub(crate) fn transition_state(&self, current_state: TaskState, new_state: TaskState) -> bool {
        self.state
            .compare_exchange(
                current_state as u8,
                new_state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[inline]
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state(), TaskState::Running)
    }

    #[inline]
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self.state(), TaskState::Ready)
    }

    pub(crate) fn try_enter_deferred_work(&self) -> bool {
        self.deferred_work_dispatching
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn leave_deferred_work(&self) {
        self.deferred_work_dispatching
            .store(false, Ordering::Release);
    }

    #[inline]
    pub(crate) const fn is_init(&self) -> bool {
        self.is_init
    }

    #[inline]
    pub(crate) const fn is_idle(&self) -> bool {
        self.is_idle
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn set_preempt_pending(&self, pending: bool) {
        self.need_resched.store(pending, Ordering::Release)
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn can_preempt(&self, current_disable_count: usize) -> bool {
        self.preempt_disable_count.load(Ordering::Acquire) == current_disable_count
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn disable_preempt(&self) {
        self.preempt_disable_count.fetch_add(1, Ordering::Release);
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn enable_preempt(&self, resched: bool) {
        if self.preempt_disable_count.fetch_sub(1, Ordering::Release) == 1 && resched {
            // If current task is pending to be preempted, do rescheduling.
            Self::current_check_preempt_pending();
        }
    }

    #[cfg(feature = "preempt")]
    pub(crate) fn current_check_preempt_pending() {
        use kernel_guard::NoPreemptIrqSave;
        let curr = crate::current();
        if curr.need_resched.load(Ordering::Acquire) && curr.can_preempt(0) {
            // Note: if we want to print log msg during `preempt_resched`, we have to
            // disable preemption here, because the axlog may cause preemption.
            {
                let mut rq = crate::current_run_queue::<NoPreemptIrqSave>();
                if curr.need_resched.load(Ordering::Acquire) {
                    rq.preempt_resched()
                }
            }
            // The runqueue guard has been released. The runner performs its
            // own IRQ/preemption checks, so calls originating in a still-unsafe
            // outer context remain deferred.
            crate::run_deferred_work();
        }
    }

    /// Notify all tasks that join on this task.
    pub(crate) fn notify_exit(&self, exit_code: i32) {
        self.exit_code.store(exit_code, Ordering::Release);
        // Publish the exit code before Exited. An Acquire state observation in
        // join() must never pair the terminal state with the old exit code.
        self.set_state(TaskState::Exited);
        self.wait_for_exit.wake();
    }

    #[inline]
    pub(crate) const unsafe fn ctx_mut_ptr(&self) -> *mut TaskContext {
        self.ctx.get()
    }

    /// Remove and return the kernel stack owned by this task, if any.
    pub(crate) fn take_kernel_stack(&mut self) -> Option<TaskStack> {
        self.kstack.take()
    }

    /// Set the CPU ID where the task is running or will run.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_cpu_id(&self, cpu_id: u32) {
        self.cpu_id.store(cpu_id, Ordering::Release);
    }

    /// Returns whether the task is running on a CPU.
    ///
    /// It is used to protect the task from being moved to a different run queue
    /// while it has not finished its scheduling process.
    /// The `on_cpu field is set to `true` when the task is preparing to run on a CPU,
    /// and it is set to `false` when the task has finished its scheduling process in `clear_prev_task_on_cpu()`.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// Sets whether the task is running on a CPU.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_on_cpu(&self, on_cpu: bool) {
        self.on_cpu.store(on_cpu, Ordering::Release)
    }
}

impl fmt::Debug for TaskInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TaskInner")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", &self.state())
            .finish()
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        debug!("task drop: id={}", self.id.as_u64());
    }
}

pub(crate) struct TaskStack {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl TaskStack {
    pub fn try_alloc(size: usize) -> Result<Self, AllocError> {
        if size == 0 {
            return Err(AllocError);
        }
        let layout = Layout::from_size_align(size, 16).map_err(|_| AllocError)?;
        if let Some(stack) = crate::run_queue::take_cached_task_stack(layout.size(), layout.align())
        {
            return Ok(stack);
        }
        Ok(Self {
            ptr: NonNull::new(unsafe { alloc::alloc::alloc(layout) }).ok_or(AllocError)?,
            layout,
        })
    }

    pub const fn top(&self) -> VirtAddr {
        unsafe { core::mem::transmute(self.ptr.as_ptr().add(self.layout.size())) }
    }

    pub(crate) const fn layout_size(&self) -> usize {
        self.layout.size()
    }

    pub(crate) const fn layout_align(&self) -> usize {
        self.layout.align()
    }

    pub(crate) fn scrub_for_cache(&mut self) {
        // Kernel stacks are not user-readable and are overwritten as the next
        // task runs. Avoid a full-stack zero fill on every thread exit; code
        // that exposes kernel memory must initialize the data it copies out.
        let _ = self;
    }
}

impl Drop for TaskStack {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// A wrapper of [`AxTaskRef`] as the current task.
///
/// It won't change the reference count of the task when created or dropped.
pub struct CurrentTask(ManuallyDrop<AxTaskRef>);

impl CurrentTask {
    pub(crate) fn try_get() -> Option<Self> {
        let ptr: *const super::AxTask = axhal::percpu::current_task_ptr();
        if !ptr.is_null() {
            Some(Self(unsafe { ManuallyDrop::new(AxTaskRef::from_raw(ptr)) }))
        } else {
            None
        }
    }

    pub(crate) fn get() -> Self {
        Self::try_get().expect("current task is uninitialized")
    }

    /// Clone the inner `AxTaskRef`.
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> AxTaskRef {
        self.0.deref().clone()
    }

    /// Returns `true` if the current task is the same as `other`.
    pub fn ptr_eq(&self, other: &AxTaskRef) -> bool {
        Arc::ptr_eq(&self.0, other)
    }

    pub(crate) unsafe fn init_current(init_task: AxTaskRef) {
        assert!(init_task.is_init());
        #[cfg(feature = "tls")]
        unsafe {
            axhal::asm::write_thread_pointer(init_task.tls.tls_ptr() as usize)
        };
        let ptr = Arc::into_raw(init_task);
        unsafe {
            axhal::percpu::set_current_task_ptr(ptr);
        }
    }

    pub(crate) unsafe fn set_current(prev: Self, next: AxTaskRef) {
        let Self(arc) = prev;
        ManuallyDrop::into_inner(arc); // `call Arc::drop()` to decrease prev task reference count.
        let ptr = Arc::into_raw(next);
        unsafe {
            axhal::percpu::set_current_task_ptr(ptr);
        }
    }
}

impl Deref for CurrentTask {
    type Target = AxTaskRef;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

extern "C" fn task_entry() -> ! {
    #[cfg(feature = "smp")]
    unsafe {
        // Clear the prev task on CPU before running the task entry function.
        crate::run_queue::clear_prev_task_on_cpu();
    }
    // Enable irq (if feature "irq" is enabled) before running the task entry function.
    #[cfg(feature = "irq")]
    axhal::asm::enable_irqs();
    #[cfg(feature = "smp")]
    {
        let task = crate::current();
        let retired = task.take_allowed_migration(axhal::percpu::this_cpu_id());
        drop(retired);
    }
    crate::run_deferred_work();
    let task = crate::current();
    if let Some(entry) = task.entry.take() {
        entry()
    }
    crate::exit(0);
}
