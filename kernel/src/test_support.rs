//! Shared bootstrap for host tests that need a live scheduler.
//!
//! The host test target emulates a single primary CPU, so every test that
//! reaches a sleepable lock, a wait queue, or `current()` shares one
//! current-task slot. Two properties follow, and both are the caller's
//! responsibility to obtain from here rather than to reimplement:
//!
//! * initialization must be idempotent. Test modules run in an arbitrary
//!   order within one binary, so any module may be the first to need a
//!   scheduler and every later one must tolerate finding it already up.
//! * tests that touch the shared slot must serialize against each other.
//!   Serializing only within a module leaves cross-module races that appear
//!   as unrelated failures whenever the full suite runs.
//!
//! Per-module bootstraps drift on both points. One module previously treated
//! "already initialized" as fatal, which turned an ordinary ordering
//! difference into a poisoned `Once` and failed every test in that module as
//! soon as any other module initialized first.

extern crate std;

use std::sync::{Mutex, MutexGuard, Once};

/// Serializes tests that share the emulated primary-CPU current-task slot.
static SERIAL: Mutex<()> = Mutex::new(());
static INIT: Once = Once::new();
static PAGE_ALLOC_INIT: Once = Once::new();

/// Brings up the host memory and scheduler fixtures once for this test process.
///
/// The allocator's page-table frames use the dummy platform's identity
/// phys-to-virt mapping, so its backing range must stay live for the complete
/// process. Address-space teardown also completes a TLB grace through the
/// current task, so a page-table test needs the scheduler even when it does not
/// otherwise use a scheduler-facing API.
pub(crate) fn ensure_host_memory() {
    PAGE_ALLOC_INIT.call_once(|| {
        let ranges = axhal::mem::phys_ram_ranges();
        assert_eq!(
            ranges.len(),
            1,
            "host tests require exactly one dummy page arena"
        );
        let (start, size) = ranges[0];
        assert!(size >= 0x9000, "host dummy page arena is too small");
        assert_eq!(
            axalloc::global_allocator().available_pages(),
            0,
            "host page allocator was initialized before the test bootstrap"
        );
        axalloc::global_init(start, size);
    });
    INIT.call_once(|| {
        if let Err(error) = axtask::init_scheduler() {
            assert!(
                axtask::current_may_uninit().is_some(),
                "host scheduler initialization failed: {error:?}"
            );
        }
    });
}

/// Brings the host scheduler up at most once per test binary.
///
/// Finding the scheduler already initialized is a normal outcome, not a
/// failure: it means another test module got there first. The current-task
/// slot being live is the property callers actually depend on, so that is what
/// is asserted.
pub(crate) fn ensure_scheduler() {
    ensure_host_memory();
}

/// Brings the scheduler up and serializes the caller against every other test
/// that shares the current-task slot.
///
/// The returned guard must be held for the duration of the test body.
///
/// A poisoned latch is recovered rather than propagated. This mutex orders
/// access to the slot; it guards no invariant of its own, so refusing to hand
/// it out after one test panicked would convert a single failure into a
/// cascade and hide the original one.
pub(crate) fn scheduler_test_context() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_scheduler();
    guard
}
