use core::sync::atomic::{AtomicU64, Ordering};

use axhal::time::{TimeValue, nanos_to_ticks};
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use starry_process::ProcessUsage;

use super::{AsThread, Thread, get_task};
use crate::time::TimeValueLike;

/// Durable CPU usage totals stored in nanoseconds.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct TaskUsage {
    /// User CPU time in nanoseconds.
    pub utime_ns: u64,
    /// System CPU time in nanoseconds.
    pub stime_ns: u64,
}

impl TaskUsage {
    /// Creates a new usage record.
    pub const fn new(utime_ns: u64, stime_ns: u64) -> Self {
        Self { utime_ns, stime_ns }
    }

    /// Collects usage from a live thread.
    pub fn from_thread(thread: &Thread) -> Self {
        let (utime, stime) = thread.time.borrow().output();
        Self::from_time_values(utime, stime)
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
        }
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
        nanos_to_ticks(self.utime_ns)
    }

    /// System CPU time in clock ticks.
    pub fn stime_ticks(self) -> u64 {
        nanos_to_ticks(self.stime_ns)
    }
}

impl From<TaskUsage> for rusage {
    fn from(value: TaskUsage) -> Self {
        let mut usage: rusage = unsafe { core::mem::zeroed() };
        usage.ru_utime = __kernel_old_timeval::from_time_value(value.utime());
        usage.ru_stime = __kernel_old_timeval::from_time_value(value.stime());
        usage
    }
}

impl From<TaskUsage> for ProcessUsage {
    fn from(value: TaskUsage) -> Self {
        Self::new(value.utime_ns, value.stime_ns)
    }
}

impl From<ProcessUsage> for TaskUsage {
    fn from(value: ProcessUsage) -> Self {
        Self::new(value.utime_ns, value.stime_ns)
    }
}

/// Atomically accumulated CPU usage totals.
#[derive(Debug, Default)]
pub struct AtomicTaskUsage {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
}

impl AtomicTaskUsage {
    /// Creates a new zeroed accumulator.
    pub const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
        }
    }

    /// Adds the provided usage totals.
    pub fn add(&self, usage: TaskUsage) {
        self.utime_ns.fetch_add(usage.utime_ns, Ordering::AcqRel);
        self.stime_ns.fetch_add(usage.stime_ns, Ordering::AcqRel);
    }

    /// Returns a snapshot of the accumulated usage.
    pub fn snapshot(&self) -> TaskUsage {
        TaskUsage {
            utime_ns: self.utime_ns.load(Ordering::Acquire),
            stime_ns: self.stime_ns.load(Ordering::Acquire),
        }
    }
}

pub(crate) fn live_process_usage(proc_data: &super::ProcessData) -> TaskUsage {
    proc_data.proc.threads().into_iter().fold(
        proc_data.exited_threads_usage.snapshot(),
        |acc, tid| {
            if let Ok(task) = get_task(tid) {
                acc.saturating_add(TaskUsage::from_thread(task.as_thread()))
            } else {
                acc
            }
        },
    )
}
