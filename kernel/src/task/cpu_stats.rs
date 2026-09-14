//! Cumulative CPU accounting sampled by the existing local scheduler tick.
//!
//! Unlike summing live processes, these counters retain exited-task time and
//! include kernel-only workers. The four original procfs CPU fields are exposed;
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
        output.push('\n');
    }
    output
}

pub(crate) fn proc_stat() -> String {
    let mut samples = [[0; 4]; MAX_CPU_NUM];
    let online = axhal::cpu_num().min(MAX_CPU_NUM);
    for (sample, cpu) in samples.iter_mut().zip(&CPU_TICKS).take(online) {
        *sample = cpu.snapshot();
    }
    render(&samples[..online], axconfig::TICKS_PER_SEC as u64)
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
            "cpu 2 1 3 5\ncpu0 1 0 2 3\ncpu1 0 0 1 2\n"
        );
        assert_eq!(clock_ticks(250, 250), 100);
        assert_eq!(clock_ticks(u64::MAX, 100), u64::MAX);
    }
}
