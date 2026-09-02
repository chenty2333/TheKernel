use core::sync::atomic::{AtomicU64, Ordering};

use axhal::time::{NANOS_PER_SEC, TimeValue};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use thekernel_linux_process_adapter::ProcessUsage;

use super::{AsThread, Thread, get_task};
use crate::time::TimeValueLike;

/// Linux exposes process CPU accounting in `clock_t` units, not raw platform
/// timer ticks. Keep this conversion centralized so `/proc` and `times()`
/// report the same scale.
pub const CLOCK_TICKS_PER_SEC: u64 = 100;
const KERNEL_HZ: u64 = 1_000;
/// Linux stores INITIAL_JIFFIES as a 32-bit unsigned `-300 * HZ`; on native
/// x86_64 `jiffies64_to_clock_t()` preserves that historical wrapped origin.
pub const INITIAL_JIFFIES: u64 = (-300_i32 * KERNEL_HZ as i32) as u32 as u64;

pub fn nanos_to_clock_ticks(nanos: u64) -> u64 {
    // Divide first: multiplying nanoseconds overflows after roughly 5.85
    // years even though the resulting clock_t remains representable.
    nanos.saturating_div(NANOS_PER_SEC / CLOCK_TICKS_PER_SEC)
}

/// Linux `jiffies64_to_clock_t(get_jiffies_64())` for HZ=1000 and USER_HZ=100.
pub fn times_clock_ticks(monotonic_nanos: u64) -> i64 {
    INITIAL_JIFFIES
        .saturating_add(monotonic_nanos / (NANOS_PER_SEC / KERNEL_HZ))
        .saturating_div(KERNEL_HZ / CLOCK_TICKS_PER_SEC) as i64
}

/// Durable CPU usage totals stored in nanoseconds.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct TaskUsage {
    /// User CPU time in nanoseconds.
    pub utime_ns: u64,
    /// System CPU time in nanoseconds.
    pub stime_ns: u64,
    /// Maximum resident set size in kilobytes.
    pub maxrss_kb: u64,
    /// Minor page faults.
    pub minflt: u64,
    /// Major page faults.
    pub majflt: u64,
    /// Input blocks in Linux's 512-byte units.
    pub inblock: u64,
    /// Output blocks in Linux's 512-byte units.
    pub oublock: u64,
    /// Voluntary context switches.
    pub nvcsw: u64,
    /// Involuntary context switches.
    pub nivcsw: u64,
}

impl TaskUsage {
    /// Creates a new usage record.
    pub const fn new(utime_ns: u64, stime_ns: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb: 0,
            minflt: 0,
            majflt: 0,
            inblock: 0,
            oublock: 0,
            nvcsw: 0,
            nivcsw: 0,
        }
    }

    pub const fn with_maxrss(utime_ns: u64, stime_ns: u64, maxrss_kb: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb,
            minflt: 0,
            majflt: 0,
            inblock: 0,
            oublock: 0,
            nvcsw: 0,
            nivcsw: 0,
        }
    }

    /// Collects usage from a live thread.
    pub fn from_thread(thread: &Thread) -> Self {
        // `TimeManager` is task-local and may be updated by that task's timer
        // IRQ on another CPU. Cross-task readers consume only the atomic
        // publication, never the interior RefCell itself.
        thread.usage_snapshot()
    }

    /// Creates usage from [`TimeValue`]s.
    pub fn from_time_values(utime: TimeValue, stime: TimeValue) -> Self {
        Self::new(utime.as_nanos() as u64, stime.as_nanos() as u64)
    }

    /// Returns the sum of two usage records, saturating on overflow.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            utime_ns: self.utime_ns.saturating_add(other.utime_ns),
            stime_ns: self.stime_ns.saturating_add(other.stime_ns),
            maxrss_kb: self.maxrss_kb.max(other.maxrss_kb),
            minflt: self.minflt.saturating_add(other.minflt),
            majflt: self.majflt.saturating_add(other.majflt),
            inblock: self.inblock.saturating_add(other.inblock),
            oublock: self.oublock.saturating_add(other.oublock),
            nvcsw: self.nvcsw.saturating_add(other.nvcsw),
            nivcsw: self.nivcsw.saturating_add(other.nivcsw),
        }
    }

    pub fn with_maxrss_floor(mut self, maxrss_kb: u64) -> Self {
        self.maxrss_kb = self.maxrss_kb.max(maxrss_kb);
        self
    }

    /// User CPU time as a [`TimeValue`].
    pub fn utime(self) -> TimeValue {
        TimeValue::from_nanos(self.utime_ns)
    }

    /// System CPU time as a [`TimeValue`].
    pub fn stime(self) -> TimeValue {
        TimeValue::from_nanos(self.stime_ns)
    }

    /// User CPU time in clock ticks.
    pub fn utime_ticks(self) -> u64 {
        nanos_to_clock_ticks(self.utime_ns)
    }

    /// System CPU time in clock ticks.
    pub fn stime_ticks(self) -> u64 {
        nanos_to_clock_ticks(self.stime_ns)
    }
}

impl From<TaskUsage> for rusage {
    fn from(value: TaskUsage) -> Self {
        let mut usage: rusage = unsafe { core::mem::zeroed() };
        usage.ru_utime = __kernel_old_timeval::from_time_value(value.utime());
        usage.ru_stime = __kernel_old_timeval::from_time_value(value.stime());
        usage.ru_maxrss = value.maxrss_kb as _;
        usage.ru_minflt = value.minflt as _;
        usage.ru_majflt = value.majflt as _;
        usage.ru_inblock = value.inblock as _;
        usage.ru_oublock = value.oublock as _;
        usage.ru_nvcsw = value.nvcsw as _;
        usage.ru_nivcsw = value.nivcsw as _;
        usage
    }
}

impl From<TaskUsage> for ProcessUsage {
    fn from(value: TaskUsage) -> Self {
        Self {
            utime_ns: value.utime_ns,
            stime_ns: value.stime_ns,
            maxrss_kb: value.maxrss_kb,
            minflt: value.minflt,
            majflt: value.majflt,
            inblock: value.inblock,
            oublock: value.oublock,
            nvcsw: value.nvcsw,
            nivcsw: value.nivcsw,
        }
    }
}

impl From<ProcessUsage> for TaskUsage {
    fn from(value: ProcessUsage) -> Self {
        Self {
            utime_ns: value.utime_ns,
            stime_ns: value.stime_ns,
            maxrss_kb: value.maxrss_kb,
            minflt: value.minflt,
            majflt: value.majflt,
            inblock: value.inblock,
            oublock: value.oublock,
            nvcsw: value.nvcsw,
            nivcsw: value.nivcsw,
        }
    }
}

/// Atomically accumulated CPU usage totals.
#[derive(Debug, Default)]
pub struct AtomicTaskUsage {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
    maxrss_kb: AtomicU64,
    minflt: AtomicU64,
    majflt: AtomicU64,
    inblock: AtomicU64,
    oublock: AtomicU64,
    nvcsw: AtomicU64,
    nivcsw: AtomicU64,
    /// Serializes the multi-field publication.  This is IRQ-safe because
    /// [`SpinNoIrq`] masks local interrupts while a writer owns the
    /// transaction; the timer path and task-context poller therefore cannot
    /// interleave stores or publish an older snapshot over a newer one.
    writer: SpinNoIrq<()>,
    /// Readers retry instead of combining fields from the middle of a
    /// serialized add/store transaction.
    sequence: AtomicU64,
}

impl AtomicTaskUsage {
    /// Creates a new zeroed accumulator.
    pub const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
            maxrss_kb: AtomicU64::new(0),
            minflt: AtomicU64::new(0),
            majflt: AtomicU64::new(0),
            inblock: AtomicU64::new(0),
            oublock: AtomicU64::new(0),
            nvcsw: AtomicU64::new(0),
            nivcsw: AtomicU64::new(0),
            writer: SpinNoIrq::new(()),
            sequence: AtomicU64::new(0),
        }
    }

    /// Adds the provided usage totals.
    pub fn add(&self, usage: TaskUsage) {
        let _writer = self.writer.lock();
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.utime_ns.fetch_add(usage.utime_ns, Ordering::AcqRel);
        self.stime_ns.fetch_add(usage.stime_ns, Ordering::AcqRel);
        self.update_maxrss(usage.maxrss_kb);
        self.minflt.fetch_add(usage.minflt, Ordering::AcqRel);
        self.majflt.fetch_add(usage.majflt, Ordering::AcqRel);
        self.inblock.fetch_add(usage.inblock, Ordering::AcqRel);
        self.oublock.fetch_add(usage.oublock, Ordering::AcqRel);
        self.nvcsw.fetch_add(usage.nvcsw, Ordering::AcqRel);
        self.nivcsw.fetch_add(usage.nivcsw, Ordering::AcqRel);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    /// Replaces the accumulated totals with the provided snapshot.
    pub fn store(&self, usage: TaskUsage) {
        let _writer = self.writer.lock();
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.utime_ns.store(usage.utime_ns, Ordering::Release);
        self.stime_ns.store(usage.stime_ns, Ordering::Release);
        self.update_maxrss(usage.maxrss_kb);
        self.minflt.store(usage.minflt, Ordering::Release);
        self.majflt.store(usage.majflt, Ordering::Release);
        self.inblock.store(usage.inblock, Ordering::Release);
        self.oublock.store(usage.oublock, Ordering::Release);
        self.nvcsw.store(usage.nvcsw, Ordering::Release);
        self.nivcsw.store(usage.nivcsw, Ordering::Release);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    /// Returns a snapshot of the accumulated usage.
    pub fn snapshot(&self) -> TaskUsage {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let usage = TaskUsage {
                utime_ns: self.utime_ns.load(Ordering::Acquire),
                stime_ns: self.stime_ns.load(Ordering::Acquire),
                maxrss_kb: self.maxrss_kb.load(Ordering::Acquire),
                minflt: self.minflt.load(Ordering::Acquire),
                majflt: self.majflt.load(Ordering::Acquire),
                inblock: self.inblock.load(Ordering::Acquire),
                oublock: self.oublock.load(Ordering::Acquire),
                nvcsw: self.nvcsw.load(Ordering::Acquire),
                nivcsw: self.nivcsw.load(Ordering::Acquire),
            };
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return usage;
            }
        }
    }

    fn update_maxrss(&self, maxrss_kb: u64) {
        let mut current = self.maxrss_kb.load(Ordering::Acquire);
        while maxrss_kb > current {
            match self.maxrss_kb.compare_exchange_weak(
                current,
                maxrss_kb,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

pub(crate) fn live_process_usage(proc_data: &super::ProcessData) -> TaskUsage {
    loop {
        let before = proc_data.usage_transition_epoch.load(Ordering::Acquire);
        if before & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let usage = proc_data.proc.thread_ids().fold(
            proc_data.exited_threads_usage.snapshot(),
            |acc, tid| {
                if let Ok(task) = get_task(tid) {
                    acc.saturating_add(TaskUsage::from_thread(task.as_thread()))
                } else {
                    acc
                }
            },
        );
        let after = proc_data.usage_transition_epoch.load(Ordering::Acquire);
        if before == after {
            return usage;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_tick_conversion_divides_before_multiplying() {
        assert_eq!(nanos_to_clock_ticks(NANOS_PER_SEC), CLOCK_TICKS_PER_SEC);
        assert_eq!(
            nanos_to_clock_ticks(u64::MAX),
            u64::MAX / (NANOS_PER_SEC / CLOCK_TICKS_PER_SEC)
        );
    }

    #[test]
    fn times_preserves_linux_wrapped_initial_jiffies_offset() {
        assert_eq!(times_clock_ticks(0), 429_466_729);
        assert_eq!(times_clock_ticks(NANOS_PER_SEC), 429_466_829);
    }
}
