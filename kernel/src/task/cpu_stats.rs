//! Cumulative CPU accounting sampled by the existing local scheduler tick.
//!
//! Unlike summing live processes, these counters retain exited-task time and
//! include kernel-only workers. The ten Linux procfs CPU fields are exposed;
//! IRQ/softirq time is not split from system time, nor I/O wait from idle time.

use alloc::string::String;
use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

use axconfig::plat::MAX_CPU_NUM;

#[repr(align(64))]
struct CpuTicks([AtomicU64; 4]);

impl CpuTicks {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; 4])
    }

    fn account(&self, idle: bool, user: bool, nice: i8) {
        let field = if idle {
            3
        } else if user {
            usize::from(nice > 0)
        } else {
            2
        };
        self.0[field].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> [u64; 4] {
        self.0
            .each_ref()
            .map(|counter| counter.load(Ordering::Relaxed))
    }
}

static CPU_TICKS: [CpuTicks; MAX_CPU_NUM] = [const { CpuTicks::new() }; MAX_CPU_NUM];

/// Called with IRQs/preemption disabled; `user` comes from the interrupted CS,
/// not the task's time-accounting state (which has already entered the kernel).
pub(super) fn account_tick(cpu: usize, current: &axtask::TaskInner, user: bool) {
    let nice = if user {
        axtask::sched_state(&axtask::current()).nice
    } else {
        0
    };
    CPU_TICKS[cpu].account(current.is_idle(), user, nice);
}

fn clock_ticks(ticks: u64, hz: u64) -> u64 {
    ((ticks as u128 * super::CLOCK_TICKS_PER_SEC as u128) / hz as u128).min(u64::MAX as u128) as u64
}

fn render(samples: &[[u64; 4]], hz: u64) -> String {
    let mut total = [0u64; 4];
    for sample in samples {
        for (sum, ticks) in total.iter_mut().zip(sample) {
            *sum = sum.saturating_add(*ticks);
        }
    }
    let mut output = String::new();
    for (index, sample) in core::iter::once(&total).chain(samples).enumerate() {
        if index == 0 {
            output.push_str("cpu");
        } else {
            let _ = write!(output, "cpu{}", index - 1);
        }
        for ticks in sample {
            let _ = write!(output, " {}", clock_ticks(*ticks, hz));
        }
        // The remaining Linux fields are not separately accounted here:
        // I/O wait is included in idle and IRQ execution in system time.
        output.push_str(" 0 0 0 0 0 0\n");
    }
    output
}

pub(crate) fn proc_stat() -> String {
    let mut samples = [[0; 4]; MAX_CPU_NUM];
    let online = axhal::cpu_num().min(MAX_CPU_NUM);
    for (sample, cpu) in samples.iter_mut().zip(&CPU_TICKS).take(online) {
        *sample = cpu.snapshot();
    }
    let mut output = render(&samples[..online], axconfig::TICKS_PER_SEC as u64);
    let mut interrupts = [0u64; axhal::irq::statistics::VECTOR_COUNT];
    for cpu in 0..online {
        for (sum, count) in interrupts
            .iter_mut()
            .zip(axhal::irq::statistics::snapshot_cpu(cpu))
        {
            *sum = sum.saturating_add(count);
        }
    }
    append_interrupt_totals(&mut output, &interrupts);
    output
}

fn append_interrupt_totals(output: &mut String, interrupts: &[u64]) {
    let total = interrupts
        .iter()
        .fold(0u64, |sum, count| sum.saturating_add(*count));
    let _ = write!(output, "intr {total}");
    for count in interrupts {
        let _ = write!(output, " {count}");
    }
    output.push_str("\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_accounting_classifies_interrupted_context_and_retains_ticks() {
        let cpu = CpuTicks::new();
        cpu.account(true, false, 0);
        cpu.account(false, false, 5);
        cpu.account(false, true, -5);
        cpu.account(false, true, 0);
        cpu.account(false, true, 5);
        assert_eq!(cpu.snapshot(), [2, 1, 1, 1]);
        cpu.account(false, false, 0);
        assert_eq!(cpu.snapshot(), [2, 1, 2, 1]);
    }

    #[test]
    fn proc_cpu_rows_use_user_hz_and_sum_before_rounding() {
        assert_eq!(
            render(&[[15, 5, 20, 30], [5, 5, 10, 20]], 1000),
            "cpu 2 1 3 5 0 0 0 0 0 0\ncpu0 1 0 2 3 0 0 0 0 0 0\ncpu1 0 0 1 2 0 0 0 0 0 0\n"
        );
        assert_eq!(clock_ticks(250, 250), 100);
        assert_eq!(clock_ticks(u64::MAX, 100), u64::MAX);
    }

    #[test]
    fn proc_stat_interrupt_total_sums_raw_vectors_not_ipi_reasons() {
        let mut output = String::new();
        append_interrupt_totals(&mut output, &[2, 0, 7]);
        assert_eq!(output, "intr 9 2 0 7\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n");
    }
}
