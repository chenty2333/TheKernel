use core::sync::atomic::{AtomicU64, Ordering};

use axhal::time::{NANOS_PER_SEC, TimeValue};
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use thekernel_linux_process_adapter::ProcessUsage;

use super::{AsThread, Thread, get_task};
use crate::time::TimeValueLike;

/// Linux exposes process CPU accounting in `clock_t` units, not raw platform
/// timer ticks. Keep this conversion centralized so `/proc` and `times()`
/// report the same scale.
pub const CLOCK_TICKS_PER_SEC: u64 = 100;

pub fn nanos_to_clock_ticks(nanos: u64) -> u64 {
    nanos
        .saturating_mul(CLOCK_TICKS_PER_SEC)
        .saturating_div(NANOS_PER_SEC)
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
}

impl TaskUsage {
    /// Creates a new usage record.
    pub const fn new(utime_ns: u64, stime_ns: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb: 0,
        }
    }

    pub const fn with_maxrss(utime_ns: u64, stime_ns: u64, maxrss_kb: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb,
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
        usage
    }
}

impl From<TaskUsage> for ProcessUsage {
    fn from(value: TaskUsage) -> Self {
        Self::with_maxrss(value.utime_ns, value.stime_ns, value.maxrss_kb)
    }
}

impl From<ProcessUsage> for TaskUsage {
    fn from(value: ProcessUsage) -> Self {
        Self::with_maxrss(value.utime_ns, value.stime_ns, value.maxrss_kb)
    }
}

/// Atomically accumulated CPU usage totals.
#[derive(Debug, Default)]
pub struct AtomicTaskUsage {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
    maxrss_kb: AtomicU64,
}

impl AtomicTaskUsage {
    /// Creates a new zeroed accumulator.
    pub const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
            maxrss_kb: AtomicU64::new(0),
        }
    }

    /// Adds the provided usage totals.
    pub fn add(&self, usage: TaskUsage) {
        self.utime_ns.fetch_add(usage.utime_ns, Ordering::AcqRel);
        self.stime_ns.fetch_add(usage.stime_ns, Ordering::AcqRel);
        self.update_maxrss(usage.maxrss_kb);
    }

    /// Replaces the accumulated totals with the provided snapshot.
    pub fn store(&self, usage: TaskUsage) {
        self.utime_ns.store(usage.utime_ns, Ordering::Release);
        self.stime_ns.store(usage.stime_ns, Ordering::Release);
        self.update_maxrss(usage.maxrss_kb);
    }

    /// Returns a snapshot of the accumulated usage.
    pub fn snapshot(&self) -> TaskUsage {
        TaskUsage {
            utime_ns: self.utime_ns.load(Ordering::Acquire),
            stime_ns: self.stime_ns.load(Ordering::Acquire),
            maxrss_kb: self.maxrss_kb.load(Ordering::Acquire),
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
    proc_data
        .proc
        .thread_ids()
        .fold(proc_data.exited_threads_usage.snapshot(), |acc, tid| {
            if let Ok(task) = get_task(tid) {
                acc.saturating_add(TaskUsage::from_thread(task.as_thread()))
            } else {
                acc
            }
        })
}
