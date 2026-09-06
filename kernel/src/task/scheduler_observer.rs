//! CPU-wide perf scheduling edges.
//!
//! Task-attached perf remains a `TaskExt` concern.  CPU, system-wide, and
//! cgroup perf contexts must also observe idle and kernel-only intervals, so
//! they use a scheduler-wide IRQ-off observer instead.

use axtask::{SchedulerObserver, SwitchReason, TaskInner};

use crate::{file::PerfGroup, task::AsThread};

struct PerfSchedulerObserver;

#[inline]
fn task_identity(task: &TaskInner) -> (u32, u32) {
    let Some(thread) = task.try_as_thread() else {
        return (0, 0);
    };
    initial_trace_identity(thread.pid_ns(), thread.proc_data.proc.pid(), thread.tid())
}

fn initial_trace_identity(
    mut namespace: alloc::sync::Arc<crate::task::PidNamespace>,
    pid: u32,
    tid: u32,
) -> (u32, u32) {
    // Linux scheduler raw tracepoints use task->pid, i.e. the initial PID
    // namespace identity. Our scheduler IDs are separate allocator keys even
    // in that namespace, so render both process and thread bindings explicitly.
    while let Some(parent) = namespace.parent() {
        namespace = parent;
    }
    (namespace.visible_pid(pid), namespace.visible_pid(tid))
}

#[crate_interface::impl_interface]
impl SchedulerObserver for PerfSchedulerObserver {
    fn on_wakeup(task: &TaskInner, target_cpu: usize, timestamp: u64, priority: i32) {
        // A perf ring notification can enqueue its reader while the source
        // still owns perf-group locks. Do not recursively trace that internal
        // notification and re-enter those same producer locks.
        #[cfg(feature = "perf-sampling")]
        if crate::file::perf_sampling::notifying_perf_waiters() {
            return;
        }
        let Some(waker) = axtask::current_may_uninit() else {
            return;
        };
        let mut name = [0; 16];
        let name_len = task.copy_name_into(&mut name);
        crate::perf_sources::emit_sched_wakeup(
            waker.try_as_thread(),
            task_identity(&waker).1,
            task_identity(task).1,
            &name[..name_len],
            target_cpu,
            timestamp,
            priority,
        );
    }

    fn on_switch(prev: &TaskInner, next: &TaskInner, reason: SwitchReason, prev_priority: i32, next_priority: i32) {
        let timestamp = axhal::time::monotonic_time_nanos();
        // The scheduler invokes this with IRQs and preemption disabled.  The
        // perf CPU-context methods therefore only publish/settle local state;
        // they neither allocate nor wait for a remote reconciliation.
        let cpu = axhal::percpu::this_cpu_id();
        let mut previous_name = [0u8; 16];
        let mut next_name = [0u8; 16];
        let previous_len = prev.copy_name_into(&mut previous_name);
        let next_len = next.copy_name_into(&mut next_name);
        let previous_state = match reason {
            SwitchReason::Block => 1,
            SwitchReason::Exit => 32,
            SwitchReason::Yield | SwitchReason::Preempt | SwitchReason::Migrate => 0,
        };
        let previous = task_identity(prev);
        let incoming = task_identity(next);
        crate::perf_sources::emit_sched_switch(
            prev.try_as_thread(),
            previous.1,
            &previous_name[..previous_len],
            incoming.1,
            &next_name[..next_len],
            previous_state,
            timestamp,
            prev_priority,
            next_priority,
        );
        PerfGroup::cpu_context_leave(cpu);
        PerfGroup::cpu_context_switch(cpu, true, previous, Some(incoming));
        PerfGroup::cpu_context_switch(cpu, false, incoming, Some(previous));
        PerfGroup::cpu_context_enter_for(cpu, next.id().as_u64());
        // User-thread groups join the same domain from Thread::perf_on_enter.
        // Idle/kernel tasks have no such callback, so finish their CPU-only
        // admission here instead of letting a CPU context bypass arbitration.
        if next.try_as_thread().is_none() {
            PerfGroup::arbitrate_cpu_with_task(cpu, &[], false);
        }
        let mut slots = [None; 4];
        let mut used = 0;
        PerfGroup::cpu_context_append_debug_breakpoints(cpu, &mut slots, &mut used);
        axcpu::asm::program_perf_debug_registers(slots);
    }

    fn on_timer_tick(current: &TaskInner, interrupted_user: bool) {
        // CPU/cgroup flexible groups need the same authoritative local
        // rotation edge as task-attached groups, even while the idle task is
        // current.
        let cpu = axhal::percpu::this_cpu_id();
        PerfGroup::cpu_context_account_clock_domain(cpu, interrupted_user);
        if let Some(thread) = current.try_as_thread() {
            thread.perf_on_timer_tick(interrupted_user);
        } else {
            PerfGroup::cpu_context_multiplex_tick(axhal::percpu::this_cpu_id());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scheduler_trace_ids_use_initial_namespace_not_core_or_nested_ids() {
        let user_ns = crate::task::UserNamespace::try_new_root().unwrap();
        let root = crate::task::PidNamespace::try_new_root(user_ns.clone()).unwrap();
        root.reserve_process(40).unwrap().commit();
        root.reserve_process(43).unwrap().commit();
        assert_eq!(super::initial_trace_identity(root.clone(), 40, 43), (1, 2));

        let nested = root.try_fork(50, user_ns).unwrap();
        nested.reserve_process(50).unwrap().commit();
        nested.reserve_process(51).unwrap().commit();
        assert_eq!(nested.visible_pid(50), 1);
        assert_eq!(nested.visible_pid(51), 2);
        assert_eq!(super::initial_trace_identity(nested, 50, 51), (3, 4));
    }
}
