#[cfg(feature = "smp")]
use alloc::sync::Weak;
use alloc::{collections::VecDeque, sync::Arc};
use core::{
    future::poll_fn,
    mem::MaybeUninit,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};

use axhal::{mem::total_ram_size, percpu::this_cpu_id};
use axsched::{BaseScheduler, EnqueueReason};
use futures_util::task::AtomicWaker;
use kernel_guard::BaseGuard;
use kspin::{SpinNoIrqGuard, SpinRaw};
use lazyinit::LazyInit;

#[cfg(feature = "smp")]
use crate::task::MigrationClaim;
use crate::{
    AxCpuMask, AxTaskRef, Scheduler, TaskInner,
    future::block_on,
    task::{CurrentTask, TaskStack, TaskState},
};

macro_rules! percpu_static {
    ($(
        $(#[$comment:meta])*
        $name:ident: $ty:ty = $init:expr
    ),* $(,)?) => {
        $(
            $(#[$comment])*
            #[percpu::def_percpu]
            static $name: $ty = $init;
        )*
    };
}

percpu_static! {
    RUN_QUEUE: LazyInit<AxRunQueue> = LazyInit::new(),
    EXITED_TASKS: VecDeque<AxTaskRef> = VecDeque::new(),
    EXITED_TASKS_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0),
    WAIT_FOR_EXIT: AtomicWaker = AtomicWaker::new(),
    STACK_CACHE: kspin::SpinNoIrq<PerCpuStackCache> = kspin::SpinNoIrq::new(PerCpuStackCache::new()),
    IDLE_TASK: LazyInit<AxTaskRef> = LazyInit::new(),
    /// Stores the weak reference to the previous task that is running on this CPU.
    #[cfg(feature = "smp")]
    PREV_TASK: Weak<crate::AxTask> = Weak::new(),
}

const MIB: usize = 1024 * 1024;
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn idle_ticks() -> u64 {
    IDLE_TICKS.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct StackCacheKey {
    size: usize,
    align: usize,
}

struct StackCacheBucket {
    key: StackCacheKey,
    stack: TaskStack,
}

const STACK_CACHE_SLOTS: usize = 64;

struct PerCpuStackCache {
    cached_bytes: usize,
    budget_bytes: usize,
    slots: [Option<StackCacheBucket>; STACK_CACHE_SLOTS],
}

impl PerCpuStackCache {
    const fn new() -> Self {
        Self {
            cached_bytes: 0,
            budget_bytes: 0,
            slots: [const { None }; STACK_CACHE_SLOTS],
        }
    }

    fn take(&mut self, size: usize, align: usize) -> Option<TaskStack> {
        let key = StackCacheKey { size, align };
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|bucket| bucket.key == key))?;
        let bucket = slot.take()?;
        self.cached_bytes = self.cached_bytes.saturating_sub(size);
        Some(bucket.stack)
    }

    /// Returns the stack when it cannot be cached so its deallocation can occur
    /// after the per-CPU no-IRQ lock has been released.
    fn recycle(&mut self, mut stack: TaskStack) -> Option<TaskStack> {
        let size = stack.layout_size();
        let align = stack.layout_align();
        let budget = self.budget_bytes();
        if size == 0 || budget < size || self.cached_bytes > budget.saturating_sub(size) {
            return Some(stack);
        }

        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return Some(stack);
        };
        stack.scrub_for_cache();
        *slot = Some(StackCacheBucket {
            key: StackCacheKey { size, align },
            stack,
        });
        self.cached_bytes += size;
        None
    }

    fn budget_bytes(&mut self) -> usize {
        if self.budget_bytes == 0 {
            self.budget_bytes = per_cpu_stack_cache_budget_bytes();
        }
        self.budget_bytes
    }
}

fn system_stack_cache_budget_bytes() -> usize {
    let ram = total_ram_size();
    if ram <= 256 * MIB {
        0
    } else if ram <= 512 * MIB {
        4 * MIB
    } else if ram <= 2 * 1024 * MIB {
        32 * MIB
    } else {
        64 * MIB
    }
}

fn per_cpu_stack_cache_budget_bytes() -> usize {
    // Keep stack reuse lock-local, but avoid hoarding exited-task stacks on
    // low-memory guests where short-lived process bursts are common.
    let cpu_num = axhal::cpu_num().max(1);
    system_stack_cache_budget_bytes() / cpu_num
}

pub(crate) fn take_cached_task_stack(size: usize, align: usize) -> Option<TaskStack> {
    STACK_CACHE.with_current(|cache| cache.lock().take(size, align))
}

fn recycle_task_stack(stack: TaskStack) {
    let rejected = STACK_CACHE.with_current(|cache| cache.lock().recycle(stack));
    drop(rejected);
}

/// An array of references to run queues, one for each CPU, indexed by cpu_id.
///
/// This static variable holds references to the run queues for each CPU in the system.
///
/// # Safety
///
/// Access to this variable is marked as `unsafe` because it contains `MaybeUninit` references,
/// which require careful handling to avoid undefined behavior. The array should be fully
/// initialized before being accessed to ensure safe usage.
static mut RUN_QUEUES: [MaybeUninit<&'static mut AxRunQueue>; axconfig::plat::MAX_CPU_NUM] =
    [ARRAY_REPEAT_VALUE; axconfig::plat::MAX_CPU_NUM];
#[allow(clippy::declare_interior_mutable_const)] // It's ok because it's used only for initialization `RUN_QUEUES`.
const ARRAY_REPEAT_VALUE: MaybeUninit<&'static mut AxRunQueue> = MaybeUninit::uninit();

/// Returns a reference to the current run queue in [`CurrentRunQueueRef`].
///
/// ## Safety
///
/// This function returns a static reference to the current run queue, which
/// is inherently unsafe. It assumes that the `RUN_QUEUE` has been properly
/// initialized and is not accessed concurrently in a way that could cause
/// data races or undefined behavior.
///
/// ## Returns
///
/// * [`CurrentRunQueueRef`] - a static reference to the current [`AxRunQueue`].
#[inline(always)]
pub(crate) fn current_run_queue<G: BaseGuard>() -> CurrentRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    CurrentRunQueueRef {
        inner: unsafe { RUN_QUEUE.current_ref_mut_raw() },
        current_task: crate::current(),
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}

/// Selects the run queue index based on a CPU set bitmap and load balancing.
///
/// This function filters the available run queues based on the provided `cpumask` and
/// selects the run queue index for the next task. The selection is based on a round-robin algorithm.
///
/// ## Arguments
///
/// * `cpumask` - A bitmap representing the CPUs that are eligible for task execution.
///
/// ## Returns
///
/// The index (cpu_id) of the selected run queue.
///
/// ## Panics
///
/// This function will panic if `cpu_mask` is empty, indicating that there are no available CPUs for task execution.
#[cfg(feature = "smp")]
// The modulo operation is safe here because `axconfig::plat::MAX_CPU_NUM` is always greater than 1 with "smp" enabled.
#[allow(clippy::modulo_one)]
#[inline]
pub(crate) fn select_run_queue_index(cpumask: AxCpuMask) -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static RUN_QUEUE_INDEX: AtomicUsize = AtomicUsize::new(0);

    assert!(!cpumask.is_empty(), "No available CPU for task execution");

    // Round-robin selection of the run queue index.
    loop {
        let index = RUN_QUEUE_INDEX.fetch_add(1, Ordering::SeqCst) % axconfig::plat::MAX_CPU_NUM;
        if cpumask.get(index) {
            return index;
        }
    }
}

/// Retrieves a `'static` reference to the run queue corresponding to the given index.
///
/// This function asserts that the provided index is within the range of available CPUs
/// and returns a reference to the corresponding run queue.
///
/// ## Arguments
///
/// * `index` - The index of the run queue to retrieve.
///
/// ## Returns
///
/// A reference to the `AxRunQueue` corresponding to the provided index.
///
/// ## Panics
///
/// This function will panic if the index is out of bounds.
#[cfg(feature = "smp")]
#[inline]
fn get_run_queue(index: usize) -> &'static mut AxRunQueue {
    unsafe { RUN_QUEUES[index].assume_init_mut() }
}

/// Selects the appropriate run queue for the provided task.
///
/// * In a single-core system, this function always returns a reference to the global run queue.
/// * In a multi-core system, this function selects the run queue based on the task's CPU affinity and load balance.
///
/// ## Arguments
///
/// * `task` - A reference to the task for which a run queue is being selected.
///
/// ## Returns
///
/// * [`AxRunQueueRef`] - a static reference to the selected [`AxRunQueue`] (current or remote).
///
/// ## TODO
///
/// 1. Implement better load balancing across CPUs for more efficient task distribution.
/// 2. Use a more generic load balancing algorithm that can be customized or replaced.
#[inline]
pub(crate) fn select_run_queue<G: BaseGuard>(task: &AxTaskRef) -> AxRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        // When SMP is disabled, all tasks are scheduled on the same global run queue.
        AxRunQueueRef {
            inner: unsafe { RUN_QUEUE.current_ref_mut_raw() },
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        // When SMP is enabled, select the run queue based on the task's CPU affinity and load balance.
        let index = select_run_queue_index(task.cpumask());
        AxRunQueueRef {
            inner: get_run_queue(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// Returns the run queue that currently owns the task, if any.
#[inline]
pub(crate) fn task_run_queue<G: BaseGuard>(task: &AxTaskRef) -> AxRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        AxRunQueueRef {
            inner: unsafe { RUN_QUEUE.current_ref_mut_raw() },
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        let index = task.cpu_id() as usize;
        AxRunQueueRef {
            inner: get_run_queue(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// [`AxRunQueue`] represents a run queue for global system or a specific CPU.
pub(crate) struct AxRunQueue {
    /// The ID of the CPU this run queue is associated with.
    cpu_id: usize,
    /// The core scheduler of this run queue.
    /// Since irq and preempt are preserved by the kernel guard hold by `AxRunQueueRef`,
    /// we just use a simple raw spin lock here.
    scheduler: SpinRaw<Scheduler>,
}

/// A reference to the run queue with specific guard.
///
/// Note:
/// [`AxRunQueueRef`] is used to get a reference to the run queue on current CPU
/// or a remote CPU, which is used to add tasks to the run queue or unblock tasks.
/// If you want to perform scheduling operations on the current run queue,
/// see [`CurrentRunQueueRef`].
pub(crate) struct AxRunQueueRef<'a, G: BaseGuard> {
    inner: &'a mut AxRunQueue,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for AxRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// A reference to the current run queue with specific guard.
///
/// Note:
/// [`CurrentRunQueueRef`] is used to get a reference to the run queue on current CPU,
/// in which scheduling operations can be performed.
pub(crate) struct CurrentRunQueueRef<'a, G: BaseGuard> {
    inner: &'a mut AxRunQueue,
    current_task: CurrentTask,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for CurrentRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// Management operations for run queue, including adding tasks, unblocking tasks, etc.
impl<G: BaseGuard> AxRunQueueRef<'_, G> {
    /// Adds a task to the scheduler.
    ///
    /// This function is used to add a new task to the scheduler.
    pub fn add_task(&mut self, task: AxTaskRef) {
        debug!(
            "task add: id={} on run_queue {}",
            task.id().as_u64(),
            self.inner.cpu_id
        );
        assert!(task.is_ready());
        #[cfg(feature = "smp")]
        task.set_cpu_id(self.inner.cpu_id as _);
        self.inner
            .scheduler
            .lock()
            .enqueue_task(task, EnqueueReason::New);
    }

    /// Unblock one task by inserting it into the run queue.
    ///
    /// This function does nothing if the task is not in [`TaskState::Blocked`],
    /// which means the task is already unblocked by other cores.
    pub fn unblock_task(&mut self, task: AxTaskRef, resched: bool) {
        let task_id = task.id().as_u64();
        // Try to change the state of the task from `Blocked` to `Ready`,
        // if successful, the task will be put into this run queue,
        // otherwise, the task is already unblocked by other cores.
        // Note:
        // target task can not be insert into the run queue until it finishes its scheduling process.
        if self
            .inner
            .put_task_with_state(task, TaskState::Blocked, resched)
        {
            // Since now, the task to be unblocked is in the `Ready` state.
            let cpu_id = self.inner.cpu_id;
            debug!("task unblock: id={task_id} on run_queue {cpu_id}");
            // Note: when the task is unblocked on another CPU's run queue,
            // we just ingiore the `resched` flag.
            if resched && cpu_id == this_cpu_id() {
                #[cfg(feature = "preempt")]
                crate::current().set_preempt_pending(true);
            }
        }
    }

    #[cfg(feature = "sched-cfs")]
    pub fn set_task_sched_state(
        &mut self,
        task: &AxTaskRef,
        sched_state: axsched::CfsTaskParams,
    ) -> bool {
        self.inner.set_task_sched_state(task, sched_state)
    }

    #[cfg(feature = "smp")]
    pub fn migrate_ready_task(&mut self, task: &AxTaskRef) -> bool {
        self.inner.migrate_ready_task(task)
    }
}

/// Core functions of run queue.
impl<G: BaseGuard> CurrentRunQueueRef<'_, G> {
    #[cfg(feature = "smp")]
    fn maybe_migrate_current(&mut self) -> bool {
        let curr = &self.current_task;
        match curr.claim_migration(self.inner.cpu_id) {
            MigrationClaim::Allowed => false,
            MigrationClaim::Prepared(migration_task) => {
                self.migrate_current(migration_task);
                true
            }
            MigrationClaim::Missing => {
                // All public affinity updates admit the helper before publishing
                // an excluding mask. Keep running rather than allocating or
                // panicking inside this runqueue/no-IRQ safe point if an internal
                // caller ever violates that contract.
                #[cfg(feature = "preempt")]
                curr.set_preempt_pending(true);
                false
            }
        }
    }

    #[cfg(feature = "smp")]
    pub(crate) fn migrate_current_if_needed(&mut self) -> bool {
        self.maybe_migrate_current()
    }

    #[cfg(feature = "irq")]
    pub fn scheduler_timer_tick(&mut self) {
        let curr = &self.current_task;
        #[cfg(feature = "smp")]
        if !curr.cpumask().get(self.inner.cpu_id) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
            return;
        }
        if curr.is_idle() {
            IDLE_TICKS.fetch_add(1, Ordering::Relaxed);
        } else if self.inner.scheduler.lock().task_tick(curr) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
        }
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub fn yield_current(&mut self) {
        let curr = self.current_task.clone();
        trace!("task yield: id={}", curr.id().as_u64());
        assert!(curr.is_running());

        #[cfg(feature = "smp")]
        if self.maybe_migrate_current() {
            return;
        }

        self.inner
            .put_task_with_state(curr, TaskState::Running, false);

        self.inner.resched();
    }

    /// Migrate the current task to a new run queue matching its CPU affinity and reschedule.
    /// This function will spawn a new `migration_task` to perform the migration, which will set
    /// current task to `Ready` state and select a proper run queue for it according to its CPU affinity,
    /// switch to the migration task immediately after migration task is prepared.
    ///
    /// Note: the ownership if migrating task (which is current task) is handed over to the migration task,
    /// before the migration task inserted it into the target run queue.
    #[cfg(feature = "smp")]
    pub fn migrate_current(&mut self, migration_task: AxTaskRef) {
        let curr = &self.current_task;
        trace!("task migrate: id={}", curr.id().as_u64());
        assert!(curr.is_running());

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Call `switch_to` to reschedule to the migration task that performs the migration directly.
        self.inner.switch_to(crate::current(), migration_task);
    }

    /// Preempts the current task and reschedules.
    /// This function is used to preempt the current task and reschedule
    /// to next task on current run queue.
    ///
    /// This function is called by `current_check_preempt_pending` with IRQs and preemption disabled.
    ///
    /// Note:
    /// preemption may happened in `enable_preempt`, which is called
    /// each time a [`kspin::NoPreemptGuard`] is dropped.
    #[cfg(feature = "preempt")]
    pub fn preempt_resched(&mut self) {
        // There is no need to disable IRQ and preemption here, because
        // they both have been disabled in `current_check_preempt_pending`.
        let curr = self.current_task.clone();
        assert!(curr.is_running());

        // When we call `preempt_resched()`, both IRQs and preemption must
        // have been disabled by `kernel_guard::NoPreemptIrqSave`. So we need
        // to set `current_disable_count` to 1 in `can_preempt()` to obtain
        // the preemption permission.
        let can_preempt = curr.can_preempt(1);

        trace!(
            "current task id={} is to be preempted, allow={}",
            curr.id().as_u64(),
            can_preempt
        );
        if can_preempt {
            #[cfg(feature = "smp")]
            if self.maybe_migrate_current() {
                return;
            }
            self.inner
                .put_task_with_state(curr, TaskState::Running, true);
            self.inner.resched();
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub fn exit_current(&mut self, exit_code: i32) -> ! {
        let curr = &self.current_task;
        debug!(
            "task exit: id={}, exit_code={}",
            curr.id().as_u64(),
            exit_code
        );
        assert!(curr.is_running(), "task is not running: {:?}", curr.state());
        assert!(!curr.is_idle());
        if curr.is_init() {
            clear_exited_tasks();
            axhal::power::system_off();
        } else {
            // Notify the joiner task.
            curr.notify_exit(exit_code);

            // Push current task to the `EXITED_TASKS` list, which will be
            // consumed by the GC task.
            push_exited_task(curr.clone());

            // Schedule to next task.
            self.inner.resched();
        }
        unreachable!("task exited!");
    }

    /// Block the current task, put current task into the wait queue and reschedule.
    /// Mark the state of current task as `Blocked`, set the `in_wait_queue` flag as true.
    /// Note:
    ///     1. The caller must hold the lock of the wait queue.
    ///     2. The caller must ensure that the current task is in the running state.
    ///     3. The caller must ensure that the current task is not the idle task.
    ///     4. The lock of the wait queue will be released explicitly after current task is pushed into it.
    pub fn blocked_resched(&mut self, mut woke: SpinNoIrqGuard<'_, bool>) {
        let curr = &self.current_task;
        assert!(curr.is_running());
        assert!(!curr.is_idle());
        // we must not block current task with preemption disabled.
        // Current expected preempt count is 2 for `NoPreemptIrqSave` and `woke`.
        #[cfg(feature = "preempt")]
        assert!(curr.can_preempt(2));

        #[cfg(feature = "smp")]
        if !curr.cpumask().get(self.inner.cpu_id) {
            curr.set_cpu_id(select_run_queue_index(curr.cpumask()) as _);
        }

        // Mark the task as blocked, this has to be done before adding it to the wait queue
        // while holding the lock of the wait queue.
        curr.set_state(TaskState::Blocked);
        *woke = false;
        drop(woke);

        // Current task's state has been changed to `Blocked` and added to the wait queue.
        // Note that the state may have been set as `Ready` in `unblock_task()`,
        // see `unblock_task()` for details.

        debug!("task block: id={}", curr.id().as_u64());
        self.inner.resched();
    }

    pub fn set_current_priority(&mut self, prio: isize) -> bool {
        self.inner
            .scheduler
            .lock()
            .set_priority(&self.current_task, prio)
    }
}

impl AxRunQueue {
    #[cfg(feature = "smp")]
    fn migrate_ready_task(&mut self, task: &AxTaskRef) -> bool {
        if !matches!(task.state(), TaskState::Ready) {
            return false;
        }

        let target_index = select_run_queue_index(task.cpumask());
        if target_index == self.cpu_id {
            return true;
        }

        let Some(task) = self.scheduler.lock().remove_task(task) else {
            return false;
        };

        let target = get_run_queue(target_index);
        task.set_cpu_id(target.cpu_id as _);
        target
            .scheduler
            .lock()
            .enqueue_task(task, EnqueueReason::Wakeup);
        true
    }

    #[cfg(feature = "sched-cfs")]
    fn set_task_sched_state(
        &mut self,
        task: &AxTaskRef,
        sched_state: axsched::CfsTaskParams,
    ) -> bool {
        match task.state() {
            TaskState::Ready => {
                let mut scheduler = self.scheduler.lock();
                if let Some(task) = scheduler.remove_task(task) {
                    if !scheduler.set_task_params(&task, sched_state) {
                        scheduler.enqueue_task(task, EnqueueReason::Wakeup);
                        return false;
                    }
                    scheduler.enqueue_task(task, EnqueueReason::Wakeup);
                    true
                } else {
                    match task.state() {
                        TaskState::Exited => false,
                        TaskState::Ready | TaskState::Running | TaskState::Blocked => {
                            scheduler.set_task_params(task, sched_state)
                        }
                    }
                }
            }
            TaskState::Running | TaskState::Blocked => {
                self.scheduler.lock().set_task_params(task, sched_state)
            }
            TaskState::Exited => false,
        }
    }

    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    fn new(cpu_id: usize) -> Self {
        let gc_task = TaskInner::new(
            || block_on(poll_fn(poll_gc)),
            "gc".into(),
            axconfig::TASK_STACK_SIZE,
        )
        .into_arc();
        // gc task should be pinned to the current CPU.
        gc_task.set_cpumask(AxCpuMask::one_shot(cpu_id));
        #[cfg(feature = "sched-cfs")]
        assert!(
            gc_task.configure(axsched::CfsTaskParams {
                // Exited-task stacks are only recycled after the GC task runs.
                // Keep it in the normal fair class so join-heavy thread bursts
                // cannot outrun cleanup and exhaust kernel stack memory.
                class: axsched::CfsTaskClass::Normal,
                nice: 0,
                rt_priority: 0,
                reset_on_fork: false,
                dl_runtime: 0,
                dl_deadline: 0,
                dl_period: 0,
            }),
            "invalid gc scheduling state"
        );

        let mut scheduler = Scheduler::new();
        scheduler.add_task(gc_task);
        Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
        }
    }

    /// Puts target task into current run queue with `Ready` state
    /// if its state matches `current_state` (except idle task).
    ///
    /// If `preempt`, keep current task's time slice, otherwise reset it.
    ///
    /// Returns `true` if the target task is put into this run queue successfully,
    /// otherwise `false`.
    fn put_task_with_state(
        &mut self,
        task: AxTaskRef,
        current_state: TaskState,
        preempt: bool,
    ) -> bool {
        // If the task's state matches `current_state`, set its state to `Ready` and
        // put it back to the run queue (except idle task).
        if task.transition_state(current_state, TaskState::Ready) && !task.is_idle() {
            // If the task is blocked, wait for the task to finish its scheduling process.
            // See `unblock_task()` for details.
            if current_state == TaskState::Blocked {
                // Wait for next task's scheduling process to complete.
                // If the owning (remote) CPU is still in the middle of schedule() with
                // this task (next task) as prev, wait until it's done referencing the task.
                //
                // Pairs with the `clear_prev_task_on_cpu()`.
                //
                // Note:
                // 1. This should be placed after the judgement of `TaskState::Blocked,`,
                //    because the task may have been woken up by other cores.
                // 2. This can be placed in the front of `switch_to()`
                #[cfg(feature = "smp")]
                while task.on_cpu() {
                    // Wait for the task to finish its scheduling process.
                    core::hint::spin_loop();
                }
            }
            let reason = match current_state {
                TaskState::Blocked => EnqueueReason::Wakeup,
                TaskState::Running if preempt => EnqueueReason::Preempt,
                TaskState::Running => EnqueueReason::Yield,
                TaskState::Ready | TaskState::Exited => EnqueueReason::New,
            };
            #[cfg(feature = "smp")]
            task.set_cpu_id(self.cpu_id as _);
            self.scheduler.lock().enqueue_task(task, reason);
            true
        } else {
            false
        }
    }

    /// Core reschedule subroutine.
    /// Pick the next task to run and switch to it.
    fn resched(&mut self) {
        let next = self
            .scheduler
            .lock()
            .pick_next_task()
            .unwrap_or_else(|| unsafe {
                // Safety: IRQs must be disabled at this time.
                IDLE_TASK.current_ref_raw().get_unchecked().clone()
            });
        assert!(
            next.is_ready(),
            "next task id={} is not ready: {:?}",
            next.id().as_u64(),
            next.state()
        );
        self.switch_to(crate::current(), next);
    }

    fn switch_to(&mut self, prev_task: CurrentTask, next_task: AxTaskRef) {
        // Make sure that IRQs are disabled by kernel guard or other means.
        #[cfg(all(target_os = "none", feature = "irq"))] // Note: irq is faked under unit tests.
        assert!(
            !axhal::asm::irqs_enabled(),
            "IRQs must be disabled during scheduling"
        );
        trace!(
            "context switch: id={} -> id={}",
            prev_task.id().as_u64(),
            next_task.id().as_u64()
        );
        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        if prev_task.ptr_eq(&next_task) {
            return;
        }

        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        next_task.set_on_cpu(true);

        #[cfg(feature = "task-ext")]
        {
            use crate::TaskExt;

            if let Some(ext) = prev_task.task_ext() {
                ext.on_leave(&prev_task)
            }
            if let Some(ext) = next_task.task_ext() {
                ext.on_enter(&next_task)
            }
        }

        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            // Store the weak pointer of **prev_task** in percpu variable `PREV_TASK`.
            #[cfg(feature = "smp")]
            {
                *PREV_TASK.current_ref_mut_raw() = Arc::downgrade(&prev_task);
            }

            // The strong reference count of `prev_task` will be decremented by 1,
            // but won't be dropped until `gc_entry()` is called.
            assert!(Arc::strong_count(&prev_task) > 1);
            assert!(Arc::strong_count(&next_task) >= 1);

            CurrentTask::set_current(prev_task, next_task);

            (*prev_ctx_ptr).switch_to(&*next_ctx_ptr);

            // Current it's **next_task** running on this CPU, clear the `prev_task`'s `on_cpu` field
            // to indicate that it has finished its scheduling process and no longer running on this CPU.
            #[cfg(feature = "smp")]
            clear_prev_task_on_cpu();
        }
    }
}

fn poll_gc(cx: &mut Context<'_>) -> Poll<()> {
    loop {
        let retained = reclaim_exited_tasks_current_cpu();
        // Note: we cannot block current task with preemption disabled,
        // use `current_ref_raw` to get the `WAIT_FOR_EXIT`'s reference here to avoid
        // the use of `NoPreemptGuard`. Since gc task is pinned to the current
        // CPU, there is no affection if the gc task is preempted during the process.
        unsafe { WAIT_FOR_EXIT.current_ref_raw() }.register(cx.waker());

        // New tasks might be added during the above section, recheck it to
        // prevent us from sleeping indefinitely.
        if EXITED_TASKS_COUNT.with_current(|c| c.load(core::sync::atomic::Ordering::Relaxed)) == 0 {
            break;
        }
        // A just-exited child can still be held by the clone/wakeup path that
        // spawned it. Re-polling immediately would spin the GC task against a
        // transient reference and steal CPU from fork/thread-heavy workloads.
        // Later exits wake the GC again, and explicit reclaim points in clone
        // and wait drain any retained task once that reference is gone.
        if retained {
            break;
        }

        crate::yield_now();
    }

    Poll::Pending
}

fn push_exited_task(task: AxTaskRef) {
    EXITED_TASKS_COUNT.with_current(|c| c.fetch_add(1, core::sync::atomic::Ordering::Relaxed));
    EXITED_TASKS.with_current(|exited_tasks| exited_tasks.push_back(task));
    // Safety: exit_current runs with IRQs + preemption disabled. Re-push
    // from the reclaim loop runs under the same percpu context.
    unsafe { WAIT_FOR_EXIT.current_ref_mut_raw().wake() };
}

fn clear_exited_tasks() {
    EXITED_TASKS.with_current(|exited_tasks| exited_tasks.clear());
    EXITED_TASKS_COUNT.with_current(|c| c.store(0, core::sync::atomic::Ordering::Relaxed));
}

pub(crate) fn has_exited_tasks() -> bool {
    let len = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.len());
    EXITED_TASKS_COUNT.with_current(|c| {
        c.store(len, core::sync::atomic::Ordering::Relaxed);
    });
    len > 0
}

fn reclaim_exited_tasks_current_cpu_batch(max_tasks: Option<usize>) -> (bool, bool) {
    // Snapshot the current queue depth so that tasks re-pushed because
    // Arc::try_unwrap failed are deferred to a later round rather than
    // keeping this loop spinning forever.
    let n = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.len());
    EXITED_TASKS_COUNT.with_current(|c| c.store(n, core::sync::atomic::Ordering::Relaxed));
    let budget = max_tasks.map_or(n, |max_tasks| n.min(max_tasks.max(1)));
    let mut retained = false;
    for _ in 0..budget {
        let Some(task) = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.pop_front()) else {
            break;
        };
        EXITED_TASKS_COUNT.with_current(|c| c.fetch_sub(1, core::sync::atomic::Ordering::Relaxed));
        match Arc::try_unwrap(task) {
            Ok(task) => {
                let mut task = task.into_inner();
                if let Some(stack) = task.take_kernel_stack() {
                    recycle_task_stack(stack);
                }
                drop(task);
            }
            Err(task) => {
                // Still held by a joiner or scheduler handoff; push back for a
                // later round.
                retained = true;
                push_exited_task(task);
            }
        }
    }
    let remaining = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.len());
    EXITED_TASKS_COUNT.with_current(|c| {
        c.store(remaining, core::sync::atomic::Ordering::Relaxed);
    });
    (retained, remaining > 0)
}

pub(crate) fn reclaim_exited_tasks_current_cpu() -> bool {
    reclaim_exited_tasks_current_cpu_batch(None).0
}

pub(crate) fn reclaim_exited_tasks_current_cpu_bounded(max_tasks: usize) -> bool {
    reclaim_exited_tasks_current_cpu_batch(Some(max_tasks)).1
}

/// The task routine for migrating the current task to the correct CPU.
///
/// It calls `select_run_queue` to get the correct run queue for the task, and
/// then puts the task to the scheduler of target run queue.
#[cfg(feature = "smp")]
pub(crate) fn migrate_entry(migrated_task: AxTaskRef) {
    let target = select_run_queue::<kernel_guard::NoPreemptIrqSave>(&migrated_task);
    migrated_task.set_cpu_id(target.inner.cpu_id as _);
    target
        .inner
        .scheduler
        .lock()
        .put_prev_task(migrated_task, false)
}

/// Clear the `on_cpu` field of previous task running on this CPU.
#[cfg(feature = "smp")]
pub(crate) unsafe fn clear_prev_task_on_cpu() {
    unsafe {
        PREV_TASK
            .current_ref_raw()
            .upgrade()
            .expect("Invalid prev_task pointer or prev_task has been dropped")
            .set_on_cpu(false);
    }
}
pub(crate) fn init() {
    let cpu_id = this_cpu_id();

    // Create the `idle` task (not current task).
    // The idle task will run when there is no other runnable task.
    // Stack size of idle task should be large because traps/interrupts may happen in idle task,
    // which need more stack space.
    const IDLE_TASK_STACK_SIZE: usize = 16384;
    let idle_task = TaskInner::new(|| crate::run_idle(), "idle".into(), IDLE_TASK_STACK_SIZE);
    // idle task should be pinned to the current CPU.
    idle_task.set_cpumask(AxCpuMask::one_shot(cpu_id));
    IDLE_TASK.with_current(|i| {
        i.init_once(idle_task.into_arc());
    });

    // Put the subsequent execution into the `main` task.
    let main_task = TaskInner::new_init("main".into()).into_arc();
    main_task.set_state(TaskState::Running);
    unsafe { CurrentTask::init_current(main_task) }

    RUN_QUEUE.with_current(|rq| {
        rq.init_once(AxRunQueue::new(cpu_id));
    });
    unsafe {
        RUN_QUEUES[cpu_id].write(RUN_QUEUE.current_ref_mut_raw());
    }
}

pub(crate) fn init_secondary() {
    let cpu_id = this_cpu_id();

    // Put the subsequent execution into the `idle` task.
    let idle_task = TaskInner::new_init("idle".into()).into_arc();
    idle_task.set_state(TaskState::Running);
    IDLE_TASK.with_current(|i| {
        i.init_once(idle_task.clone());
    });
    unsafe { CurrentTask::init_current(idle_task) }

    RUN_QUEUE.with_current(|rq| {
        rq.init_once(AxRunQueue::new(cpu_id));
    });
    unsafe {
        RUN_QUEUES[cpu_id].write(RUN_QUEUE.current_ref_mut_raw());
    }
}
