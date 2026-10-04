//! Cumulative observations at the existing successful page-fault accounting
//! edge. CPU-local cachelines avoid contending on one global fault counter.

use core::sync::atomic::{AtomicU64, Ordering};

#[repr(align(64))]
struct FaultCounters {
    total: AtomicU64,
    major: AtomicU64,
}

impl FaultCounters {
    const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            major: AtomicU64::new(0),
        }
    }

    fn account(&self, major: bool) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if major {
            self.major.fetch_add(1, Ordering::Release);
        }
    }

    fn snapshot(&self) -> (u64, u64) {
        // Keep the pair internally ordered even if another CPU publishes a
        // major fault between these loads. Other CPU rows remain best-effort.
        let major = self.major.load(Ordering::Acquire);
        (self.total.load(Ordering::Relaxed), major)
    }
}

static FAULTS: [FaultCounters; axconfig::plat::MAX_CPU_NUM] =
    [const { FaultCounters::new() }; axconfig::plat::MAX_CPU_NUM];

pub(crate) fn account_fault(major: bool) {
    if let Some(cpu) = FAULTS.get(axhal::percpu::this_cpu_id()) {
        cpu.account(major);
    }
}

pub(crate) fn snapshot() -> (u64, u64) {
    FAULTS
        .iter()
        .map(FaultCounters::snapshot)
        .fold((0u64, 0u64), |sum, cpu| {
            (sum.0.saturating_add(cpu.0), sum.1.saturating_add(cpu.1))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_are_cumulative_and_major_is_a_subset() {
        let cpu = FaultCounters::new();
        cpu.account(false);
        cpu.account(true);
        cpu.account(false);
        assert_eq!(cpu.snapshot(), (3, 1));
        assert_eq!(cpu.snapshot(), (3, 1));
        assert_eq!(core::mem::align_of::<FaultCounters>(), 64);
    }
}
