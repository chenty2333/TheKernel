//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};

#[cfg(feature = "sched-cfs")]
pub use axsched::{
    CfsTaskClass as SchedClass, CfsTaskParams as SchedState, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX,
    RT_PRIORITY_MIN,
};
use kernel_guard::NoPreemptIrqSave;

pub(crate) use crate::run_queue::{current_run_queue, select_run_queue};
#[doc(cfg(all(feature = "multitask", feature = "task-ext")))]
#[cfg(feature = "task-ext")]
pub use crate::task::{AxTaskExt, TaskExt};
#[doc(cfg(all(feature = "multitask", feature = "irq")))]
#[cfg(feature = "irq")]
pub use crate::timers::register_timer_callback;
#[doc(cfg(feature = "multitask"))]
pub use crate::{
    task::{CurrentTask, TaskId, TaskInner, TaskState},
    wait_queue::WaitQueue,
};

/// The reference type of a task.
pub type AxTaskRef = Arc<AxTask>;

/// The weak reference type of a task.
pub type WeakAxTaskRef = Weak<AxTask>;

/// The wrapper type for [`cpumask::CpuMask`] with SMP configuration.
pub type AxCpuMask = cpumask::CpuMask<{ axconfig::plat::MAX_CPU_NUM }>;

cfg_if::cfg_if! {
    if #[cfg(feature = "sched-rr")] {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type AxTask = axsched::RRTask<TaskInner, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = axsched::RRScheduler<TaskInner, MAX_TIME_SLICE>;
    } else if #[cfg(feature = "sched-cfs")] {
        pub(crate) type AxTask = axsched::CFSTask<TaskInner>;
        pub(crate) type Scheduler = axsched::CFScheduler<TaskInner>;
    } else {
        // If no scheduler features are set, use FIFO as the default.
        pub(crate) type AxTask = axsched::FifoTask<TaskInner>;
        pub(crate) type Scheduler = axsched::FifoScheduler<TaskInner>;
    }
}

#[cfg(feature = "preempt")]
struct KernelGuardIfImpl;

#[cfg(feature = "preempt")]
#[crate_interface::impl_interface]
impl kernel_guard::KernelGuardIf for KernelGuardIfImpl {
    fn disable_preempt() {
        if let Some(curr) = current_may_uninit() {
            curr.disable_preempt();
        }
    }

    fn enable_preempt() {
        if let Some(curr) = current_may_uninit() {
            curr.enable_preempt(true);
        }
    }
}

/// Gets the current task, or returns [`None`] if the current task is not
/// initialized.
pub fn current_may_uninit() -> Option<CurrentTask> {
    CurrentTask::try_get()
}

/// Gets the current task.
///
/// # Panics
///
/// Panics if the current task is not initialized.
pub fn current() -> CurrentTask {
    CurrentTask::get()
}

/// Initializes the task scheduler (for the primary CPU).
pub fn init_scheduler() {
    info!("Initialize scheduling...");

    // Initialize the run queue.
    crate::run_queue::init();

    info!("  use {} scheduler.", Scheduler::scheduler_name());
}

pub(crate) fn cpu_mask_full() -> AxCpuMask {
    use spin::Lazy;

    static CPU_MASK_FULL: Lazy<AxCpuMask> = Lazy::new(|| {
        let cpu_num = axhal::cpu_num();
        let mut cpumask = AxCpuMask::new();
        for cpu_id in 0..cpu_num {
            cpumask.set(cpu_id, true);
        }
        cpumask
    });

    *CPU_MASK_FULL
}

/// Initializes the task scheduler for secondary CPUs.
pub fn init_scheduler_secondary() {
    crate::run_queue::init_secondary();
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
#[cfg(feature = "irq")]
#[doc(cfg(feature = "irq"))]
pub fn on_timer_event() {
    crate::timers::check_events();
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
#[cfg(feature = "irq")]
#[doc(cfg(feature = "irq"))]
pub fn on_timer_tick() {
    use kernel_guard::NoOp;
    on_timer_event();
    // Since irq and preemption are both disabled here,
    // we can get current run queue with the default `kernel_guard::NoOp`.
    current_run_queue::<NoOp>().scheduler_timer_tick();
}

/// Adds the given task to the run queue, returns the task reference.
pub fn spawn_task(task: TaskInner) -> AxTaskRef {
    let task_ref = task.into_arc();
    select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    task_ref
}

/// Adds the given task to the run queue with the specified scheduling state.
#[cfg(feature = "sched-cfs")]
pub fn spawn_task_with_sched(task: TaskInner, sched_state: SchedState) -> AxTaskRef {
    let task_ref = task.into_arc();
    assert!(
        task_ref.configure(sched_state),
        "invalid initial scheduling state"
    );
    select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    task_ref
}

/// Adds the given task to the run queue with the specified scheduling state,
/// inheriting the parent's fair vruntime when applicable.
#[cfg(feature = "sched-cfs")]
pub fn spawn_task_with_sched_from(
    task: TaskInner,
    sched_state: SchedState,
    parent: &AxTaskRef,
) -> AxTaskRef {
    let task_ref = task.into_arc();
    assert!(
        task_ref.configure(sched_state),
        "invalid initial scheduling state"
    );
    task_ref.inherit_fair_vruntime_from(parent);
    select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    task_ref
}

/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn_raw<F>(f: F, name: String, stack_size: usize) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_task(TaskInner::new(f, name, stack_size))
}

/// Spawns a new task with the given name and the default stack size ([`axconfig::TASK_STACK_SIZE`]).
///
/// Returns the task reference.
pub fn spawn_with_name<F>(f: F, name: String) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_raw(f, name, axconfig::TASK_STACK_SIZE)
}

/// Spawns a new task with the default parameters.
///
/// The default task name is an empty string. The default task stack size is
/// [`axconfig::TASK_STACK_SIZE`].
///
/// Returns the task reference.
pub fn spawn<F>(f: F) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_with_name(f, String::new())
}

/// Set the priority for current task.
///
/// The range of the priority is dependent on the underlying scheduler. For
/// example, in the [CFS] scheduler, the priority is the nice value, ranging from
/// -20 to 19.
///
/// Returns `true` if the priority is set successfully.
///
/// [CFS]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub fn set_priority(prio: isize) -> bool {
    #[cfg(feature = "sched-cfs")]
    {
        if !(-20..=19).contains(&prio) {
            return false;
        }
        let task = current().clone();
        let mut state = task.sched_params();
        state.nice = prio as i8;
        return set_sched_state(&task, state);
    }

    #[allow(unreachable_code)]
    {
        current_run_queue::<NoPreemptIrqSave>().set_current_priority(prio)
    }
}

/// Returns the runtime scheduling state of a task.
#[cfg(feature = "sched-cfs")]
pub fn sched_state(task: &AxTaskRef) -> SchedState {
    task.sched_params()
}

/// Applies the runtime scheduling state of a task.
#[cfg(feature = "sched-cfs")]
pub fn set_sched_state(task: &AxTaskRef, sched_state: SchedState) -> bool {
    crate::run_queue::task_run_queue::<NoPreemptIrqSave>(task)
        .set_task_sched_state(task, sched_state)
}

/// Opportunistically reclaims exited tasks queued on the current CPU.
///
/// This complements the dedicated GC task for workloads that reap large child
/// bursts and immediately continue with more forks, where waiting for the GC
/// task to run can retain many dead task stacks and address spaces longer than
/// necessary.
pub fn reclaim_exited_tasks() {
    crate::run_queue::reclaim_exited_tasks_current_cpu();
}

/// Set the affinity for the current task.
/// [`AxCpuMask`] is used to specify the CPU affinity.
/// Returns `true` if the affinity is set successfully.
pub fn set_current_affinity(cpumask: AxCpuMask) -> bool {
    if cpumask.is_empty() {
        false
    } else {
        let curr = current().clone();

        curr.set_cpumask(cpumask);
        // After setting the affinity, we need to check if current cpu matches
        // the affinity. If not, we need to migrate the task to the correct CPU.
        #[cfg(feature = "smp")]
        if !cpumask.get(axhal::percpu::this_cpu_id()) {
            const MIGRATION_TASK_STACK_SIZE: usize = 4096;
            // Spawn a new migration task for migrating.
            let migration_task = TaskInner::new(
                move || crate::run_queue::migrate_entry(curr),
                "migration-task".into(),
                MIGRATION_TASK_STACK_SIZE,
            )
            .into_arc();

            // Migrate the current task to the correct CPU using the migration task.
            current_run_queue::<NoPreemptIrqSave>().migrate_current(migration_task);

            assert!(
                cpumask.get(axhal::percpu::this_cpu_id()),
                "Migration failed"
            );
        }
        true
    }
}

/// Sets the affinity for an arbitrary task.
///
/// For the current task this follows the existing migrate-current path. For a
/// remote ready task, the task is moved onto a run queue allowed by the new
/// mask immediately. For a remote running task, the new mask is recorded and
/// the task is nudged so it can self-migrate at its next scheduling point.
pub fn set_task_affinity(task: &AxTaskRef, cpumask: AxCpuMask) -> bool {
    if cpumask.is_empty() {
        return false;
    }

    if current().ptr_eq(task) {
        return set_current_affinity(cpumask);
    }

    task.set_cpumask(cpumask);

    #[cfg(feature = "smp")]
    match task.state() {
        TaskState::Ready => {
            let _ =
                crate::run_queue::task_run_queue::<NoPreemptIrqSave>(task).migrate_ready_task(task);
            !matches!(task.state(), TaskState::Exited)
        }
        TaskState::Running => {
            if !cpumask.get(task.cpu_id() as usize) {
                #[cfg(feature = "preempt")]
                task.set_preempt_pending(true);
                task.interrupt();
            }
            true
        }
        TaskState::Blocked => {
            if !cpumask.get(task.cpu_id() as usize) {
                task.set_cpu_id(crate::run_queue::select_run_queue_index(cpumask) as _);
            }
            true
        }
        TaskState::Exited => false,
    }

    #[cfg(not(feature = "smp"))]
    {
        !matches!(task.state(), TaskState::Exited)
    }
}

/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub fn yield_now() {
    current_run_queue::<NoPreemptIrqSave>().yield_current()
}

/// Current task is going to sleep for the given duration.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep(dur: core::time::Duration) {
    sleep_until(axhal::time::wall_time() + dur);
}

/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep_until(deadline: axhal::time::TimeValue) {
    #[cfg(feature = "irq")]
    crate::future::block_on(crate::future::sleep_until(deadline));
    #[cfg(not(feature = "irq"))]
    axhal::time::busy_wait_until(deadline);
}

/// Exits the current task.
pub fn exit(exit_code: i32) -> ! {
    current_run_queue::<NoPreemptIrqSave>().exit_current(exit_code)
}

/// The idle task routine.
///
/// It runs an infinite loop that keeps calling [`yield_now()`].
pub fn run_idle() -> ! {
    loop {
        yield_now();
        trace!("idle task: waiting for IRQs...");
        #[cfg(feature = "irq")]
        axhal::asm::wait_for_irqs();
    }
}
