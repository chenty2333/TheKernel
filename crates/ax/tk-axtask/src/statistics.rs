//! Read-only scheduler observations. Count committed publications, not task
//! ID reservations, and actual task changes, not no-op yields or queue-only migrations.

use core::sync::atomic::{AtomicU64, Ordering};

#[repr(align(64))]
struct CpuSwitches(AtomicU64);

impl CpuSwitches {
    fn account(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

static SWITCHES: [CpuSwitches; axconfig::plat::MAX_CPU_NUM] =
    [const { CpuSwitches(AtomicU64::new(0)) }; axconfig::plat::MAX_CPU_NUM];
static PUBLICATIONS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(crate) fn account_switch(cpu: usize) {
    if let Some(count) = SWITCHES.get(cpu) {
        count.account();
    }
}

#[inline]
pub(crate) fn account_publication() {
    PUBLICATIONS.fetch_add(1, Ordering::Relaxed);
}

/// Cumulative actual task-to-task switches, including kernel/idle transitions.
pub fn context_switches() -> u64 {
    SWITCHES
        .iter()
        .fold(0u64, |sum, count| sum.saturating_add(count.snapshot()))
}

/// Successfully published runnable tasks, including kernel tasks and threads.
/// Failed preparations and unpublished idle/initial tasks are not included.
pub fn task_publications() -> u64 {
    PUBLICATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_are_read_only_and_cpu_switch_rows_do_not_share_cachelines() {
        assert_eq!(core::mem::align_of::<CpuSwitches>(), 64);
        assert_eq!(core::mem::size_of::<CpuSwitches>(), 64);
        let local = CpuSwitches(AtomicU64::new(0));
        local.account();
        local.account();
        assert_eq!(local.snapshot(), 2);
        assert_eq!(local.snapshot(), 2);
        account_switch(usize::MAX);
    }
}
