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
    (
        thread.proc_data.proc.pid() as u32,
        task.id().as_u64() as u32,
    )
}

#[crate_interface::impl_interface]
impl SchedulerObserver for PerfSchedulerObserver {
    fn on_switch(prev: &TaskInner, next: &TaskInner, reason: SwitchReason) {
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
        crate::perf_sources::emit_sched_switch(
            prev.try_as_thread(),
            prev.try_as_thread()
                .map_or(0, |_| prev.id().as_u64() as u32),
            &previous_name[..previous_len],
            next.try_as_thread()
                .map_or(0, |_| next.id().as_u64() as u32),
            &next_name[..next_len],
            previous_state,
        );
        PerfGroup::cpu_context_leave(cpu);
        let previous = task_identity(prev);
        let incoming = task_identity(next);
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
