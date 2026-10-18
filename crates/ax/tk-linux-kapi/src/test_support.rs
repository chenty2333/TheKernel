// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Scheduler context for host tests that exercise task-backed reclamation.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

static TEST_SERIAL: Mutex<()> = Mutex::new(());
static MAIN_TASK_PTR: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_INIT: Once = Once::new();

/// A serialized host-test scheduler context. Its teardown waits for queued RCU
/// callbacks while task context is still installed, then restores the thread's
/// prior current-task pointer.
pub(crate) struct SchedulerContext {
    previous_task: *const (),
    _serial: MutexGuard<'static, ()>,
}

impl Drop for SchedulerContext {
    fn drop(&mut self) {
        crate::rcu::rcu_barrier();
        // SAFETY: this restores the valid per-CPU task pointer saved on entry.
        unsafe { axhal::percpu::set_current_task_ptr(self.previous_task) };
    }
}

/// Attach the host thread to a scheduler task so LinuxKPI's real `call_rcu`
/// path can spawn its reclamation callback. The returned guard must outlive
/// every final fence put made by the test.
pub(crate) fn scheduler_context() -> SchedulerContext {
    let serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The per-CPU area is process-wide, but x86_64's GS base is thread-local
    // in hosted tests. Install it on every libtest thread before accessing any
    // per-CPU slot; the copy of the linker image itself is internally once-only.
    axhal::percpu::init_primary(0);
    let previous_task = axhal::percpu::current_task_ptr::<()>();
    SCHEDULER_INIT.call_once(|| {
        let _ = axtask::init_scheduler();
        let current = axhal::percpu::current_task_ptr::<()>() as usize;
        MAIN_TASK_PTR.store(current, Ordering::SeqCst);
    });
    let main_task = MAIN_TASK_PTR.load(Ordering::SeqCst);
    assert_ne!(
        main_task, 0,
        "host-test scheduler did not provide a main task"
    );
    // SAFETY: the Once-initialized scheduler main task stays live for the host
    // test process, and this guard restores the previous pointer on teardown.
    unsafe { axhal::percpu::set_current_task_ptr(main_task as *const ()) };
    SchedulerContext {
        previous_task,
        _serial: serial,
    }
}
