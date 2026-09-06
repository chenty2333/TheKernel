//! Optional kernel-wide scheduler observation transport.
//!
//! Unlike `TaskExt`, this hook is invoked for idle and kernel-only tasks too.
//! Implementations run with local IRQs and preemption disabled, so they must
//! use only bounded, non-blocking, allocation-free operations.

use crate::{SwitchReason, TaskInner};

/// Consumer-provided observer for scheduler edges and periodic local ticks.
#[doc(hidden)]
#[crate_interface::def_interface]
pub trait SchedulerObserver {
    /// Observe one actual task handoff on the local CPU.
    fn on_switch(
        prev: &TaskInner,
        next: &TaskInner,
        reason: SwitchReason,
        prev_priority: i32,
        next_priority: i32,
    );

    /// Observe a successful blocked-to-ready enqueue after releasing its
    /// run-queue lock. `timestamp` was captured inside that publication
    /// transaction, before the destination CPU could select the task.
    fn on_wakeup(task: &TaskInner, target_cpu: usize, timestamp: u64, priority: i32);

    /// Observe the periodic local scheduler tick for the current task.
    /// `interrupted_user` comes directly from the x86 trap frame CS for the
    /// timer IRQ.  It describes the interval which just elapsed, rather than
    /// a best-effort guess based on the currently scheduled task.
    fn on_timer_tick(current: &TaskInner, interrupted_user: bool);
}

/// Read the scheduler's atomically published class/priority tuple without
/// acquiring a run-queue lock from the observer.
pub(crate) fn trace_priority(task: &crate::AxTaskRef) -> i32 {
    #[cfg(feature = "sched-eevdf")]
    {
        let params = task.sched_params();
        match params.class {
            crate::SchedClass::Normal | crate::SchedClass::Batch | crate::SchedClass::Idle => {
                120 + i32::from(params.nice)
            }
            crate::SchedClass::Fifo | crate::SchedClass::RoundRobin => {
                99 - i32::from(params.rt_priority)
            }
            crate::SchedClass::Deadline => -1,
        }
    }
    #[cfg(not(feature = "sched-eevdf"))]
    {
        let _ = task;
        120
    }
}

// Host unit tests do not link a kernel consumer which owns this interface.
// Keep a lock-free implementation in the test binary so enabling the feature
// exercises the real call sites instead of silently compiling them out.
#[cfg(test)]
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

#[cfg(test)]
static TEST_SWITCHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_TIMER_TICKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_LAST_TARGET_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
#[cfg(test)]
static TEST_LAST_WAKE_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_LAST_WAKE_PRIORITY: AtomicI32 = AtomicI32::new(0);
#[cfg(test)]
static TEST_LAST_PREV_PRIORITY: AtomicI32 = AtomicI32::new(0);
#[cfg(test)]
static TEST_LAST_NEXT_PRIORITY: AtomicI32 = AtomicI32::new(0);
#[cfg(test)]
static TEST_LAST_SWITCH_REASON: AtomicUsize = AtomicUsize::new(usize::MAX);
#[cfg(test)]
static TEST_LAST_INTERRUPTED_USER: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct TestObserverSnapshot {
    pub switches: usize,
    pub wakeups: usize,
    pub timer_ticks: usize,
    pub last_target_cpu: Option<usize>,
    pub last_wake_timestamp: u64,
    pub last_wake_priority: i32,
    pub last_prev_priority: i32,
    pub last_next_priority: i32,
    pub last_switch_reason: Option<SwitchReason>,
    pub last_interrupted_user: bool,
}

#[cfg(test)]
fn switch_reason_code(reason: SwitchReason) -> usize {
    match reason {
        SwitchReason::Yield => 0,
        SwitchReason::Block => 1,
        SwitchReason::Preempt => 2,
        SwitchReason::Migrate => 3,
        SwitchReason::Exit => 4,
    }
}

#[cfg(test)]
fn switch_reason_from_code(code: usize) -> Option<SwitchReason> {
    Some(match code {
        0 => SwitchReason::Yield,
        1 => SwitchReason::Block,
        2 => SwitchReason::Preempt,
        3 => SwitchReason::Migrate,
        4 => SwitchReason::Exit,
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) fn test_observer_snapshot() -> TestObserverSnapshot {
    TestObserverSnapshot {
        switches: TEST_SWITCHES.load(Ordering::Relaxed),
        wakeups: TEST_WAKEUPS.load(Ordering::Relaxed),
        timer_ticks: TEST_TIMER_TICKS.load(Ordering::Relaxed),
        last_target_cpu: match TEST_LAST_TARGET_CPU.load(Ordering::Relaxed) {
            usize::MAX => None,
            cpu => Some(cpu),
        },
        last_wake_timestamp: TEST_LAST_WAKE_TIMESTAMP.load(Ordering::Relaxed),
        last_wake_priority: TEST_LAST_WAKE_PRIORITY.load(Ordering::Relaxed),
        last_prev_priority: TEST_LAST_PREV_PRIORITY.load(Ordering::Relaxed),
        last_next_priority: TEST_LAST_NEXT_PRIORITY.load(Ordering::Relaxed),
        last_switch_reason: switch_reason_from_code(
            TEST_LAST_SWITCH_REASON.load(Ordering::Relaxed),
        ),
        last_interrupted_user: TEST_LAST_INTERRUPTED_USER.load(Ordering::Relaxed) != 0,
    }
}

#[cfg(test)]
pub(crate) fn reset_test_observer() {
    TEST_SWITCHES.store(0, Ordering::Relaxed);
    TEST_WAKEUPS.store(0, Ordering::Relaxed);
    TEST_TIMER_TICKS.store(0, Ordering::Relaxed);
    TEST_LAST_TARGET_CPU.store(usize::MAX, Ordering::Relaxed);
    TEST_LAST_WAKE_TIMESTAMP.store(0, Ordering::Relaxed);
    TEST_LAST_WAKE_PRIORITY.store(0, Ordering::Relaxed);
    TEST_LAST_PREV_PRIORITY.store(0, Ordering::Relaxed);
    TEST_LAST_NEXT_PRIORITY.store(0, Ordering::Relaxed);
    TEST_LAST_SWITCH_REASON.store(usize::MAX, Ordering::Relaxed);
    TEST_LAST_INTERRUPTED_USER.store(0, Ordering::Relaxed);
}

#[cfg(test)]
struct TestSchedulerObserver;

#[cfg(test)]
#[crate_interface::impl_interface]
impl SchedulerObserver for TestSchedulerObserver {
    fn on_switch(
        _prev: &TaskInner,
        _next: &TaskInner,
        reason: SwitchReason,
        prev_priority: i32,
        next_priority: i32,
    ) {
        TEST_LAST_PREV_PRIORITY.store(prev_priority, Ordering::Relaxed);
        TEST_LAST_NEXT_PRIORITY.store(next_priority, Ordering::Relaxed);
        TEST_LAST_SWITCH_REASON.store(switch_reason_code(reason), Ordering::Relaxed);
        TEST_SWITCHES.fetch_add(1, Ordering::Relaxed);
    }

    fn on_wakeup(_task: &TaskInner, target_cpu: usize, timestamp: u64, priority: i32) {
        TEST_LAST_TARGET_CPU.store(target_cpu, Ordering::Relaxed);
        TEST_LAST_WAKE_TIMESTAMP.store(timestamp, Ordering::Relaxed);
        TEST_LAST_WAKE_PRIORITY.store(priority, Ordering::Relaxed);
        TEST_WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }

    fn on_timer_tick(_current: &TaskInner, interrupted_user: bool) {
        TEST_LAST_INTERRUPTED_USER.store(usize::from(interrupted_user), Ordering::Relaxed);
        TEST_TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    }
}
