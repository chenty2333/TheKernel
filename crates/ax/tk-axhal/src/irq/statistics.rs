//! Read-only interrupt observations. No locks, allocations, or logging in IRQs.

use core::sync::atomic::{AtomicU64, Ordering};

/// x86 IDT vector count; TheKernel's IRQ identifiers are these vector numbers.
pub const VECTOR_COUNT: usize = 256;
const REASON_COUNT: usize = 8;

#[repr(align(64))]
struct CpuCounters {
    vectors: [AtomicU64; VECTOR_COUNT],
    reasons: [AtomicU64; REASON_COUNT],
}

impl CpuCounters {
    const fn new() -> Self {
        Self {
            vectors: [const { AtomicU64::new(0) }; VECTOR_COUNT],
            reasons: [const { AtomicU64::new(0) }; REASON_COUNT],
        }
    }

    #[inline]
    fn record(&self, vector: usize) {
        if let Some(counter) = self.vectors.get(vector) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> [u64; VECTOR_COUNT] {
        self.vectors.each_ref().map(|it| it.load(Ordering::Relaxed))
    }
}

static COUNTERS: [CpuCounters; axconfig::plat::MAX_CPU_NUM] =
    [const { CpuCounters::new() }; axconfig::plat::MAX_CPU_NUM];

#[inline]
pub(super) fn record_irq(cpu: usize, vector: usize) {
    if let Some(counters) = COUNTERS.get(cpu) {
        counters.record(vector);
    }
}

#[cfg(feature = "ipi")]
#[inline]
pub(super) fn record_ipi_reason(cpu: usize, reason: usize) {
    if let Some(counter) = COUNTERS.get(cpu).and_then(|it| it.reasons.get(reason)) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Best-effort cumulative observations for a CPU, including MSI/MSI-X vectors.
pub fn snapshot_cpu(cpu: usize) -> [u64; VECTOR_COUNT] {
    COUNTERS
        .get(cpu)
        .map(CpuCounters::snapshot)
        .unwrap_or([0; VECTOR_COUNT])
}

/// Delivered broker reasons in enum order. Reasons can coalesce, so these are
/// dispatch counts, not the count of requests or separate hardware interrupts.
pub fn snapshot_ipi_reasons(cpu: usize) -> [u64; REASON_COUNT] {
    COUNTERS
        .get(cpu)
        .map(|it| {
            it.reasons
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed))
        })
        .unwrap_or([0; REASON_COUNT])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_observations_are_independent_and_do_not_reset_on_read() {
        let counters = CpuCounters::new();
        for vector in [0x24, 0x80, 0x80, 0xf0] {
            counters.record(vector);
        }
        let first = counters.snapshot();
        assert_eq!(first[0x24], 1);
        assert_eq!(first[0x80], 2);
        assert_eq!(first[0xf0], 1);
        assert_eq!(first.iter().sum::<u64>(), 4);
        assert_eq!(first, counters.snapshot());
        counters.record(256);
        assert_eq!(first, counters.snapshot());
    }

    #[test]
    fn cpu_rows_are_cacheline_separated_and_invalid_reads_are_empty() {
        assert_eq!(core::mem::align_of::<CpuCounters>(), 64);
        assert_eq!(core::mem::size_of::<CpuCounters>() % 64, 0);
        assert_eq!(snapshot_cpu(usize::MAX), [0; VECTOR_COUNT]);
        assert_eq!(snapshot_ipi_reasons(usize::MAX), [0; REASON_COUNT]);
    }
}
