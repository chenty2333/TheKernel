// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Translation of Linux 7.2.3 `kernel/sched/completion.c` and the inline
//! helpers of `include/linux/completion.h` (GPL-2.0 upstream).
//!
//! The counting protocol is Linux's: `complete()` adds one to `done`,
//! `complete_all()` saturates it at `UINT_MAX`, and each successful wait that
//! is not a `complete_all()` consumes one count. The return conventions of the
//! timeout and interruptible variants match `do_wait_for_common()`.
//!
//! Deviation: waiters poll `done` through the yielding loop that `wait_until()`
//! uses elsewhere in this crate instead of queueing on a swait list. Wake-up
//! latency is therefore one scheduler yield, and no waiter can be lost.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::{
    config::{CONFIG_HZ, ERESTARTSYS, MAX_SCHEDULE_TIMEOUT},
    signal,
};

/// `UINT_MAX`: the saturated `done` count written by `complete_all()`.
pub const UINT_MAX: u32 = u32::MAX;

/// `struct completion` from include/linux/completion.h, with the x86_64
/// layout the DRM ports embed (`done` at 0, `struct swait_queue_head wait`
/// at 8; size 32, align 8). The swait head is opaque here: waiters poll
/// `done` (see the module note), so only its size and alignment are kept.
#[repr(C, align(8))]
pub struct Completion {
    done: AtomicU32,
    _pad: u32,
    /// `struct swait_queue_head` (24 bytes: raw spinlock plus list head).
    _swait: [u8; 24],
}

const _: [(); 32] = [(); core::mem::size_of::<Completion>()];
const _: [(); 8] = [(); core::mem::offset_of!(Completion, _swait)];

/// Linux `init_completion()`: a fresh completion has no pending completions.
pub const fn init_completion() -> Completion {
    Completion {
        done: AtomicU32::new(0),
        _pad: 0,
        _swait: [0; 24],
    }
}

impl Completion {
    /// Equivalent of `DECLARE_COMPLETION()`'s zeroed state.
    pub const fn new() -> Self {
        init_completion()
    }

    /// Linux `reinit_completion()`: forget any unconsumed completions.
    pub fn reinit(&self) {
        self.done.store(0, Ordering::Release);
    }

    /// Linux `complete()`: wake one waiter by adding one pending completion.
    pub fn complete(&self) {
        let mut current = self.done.load(Ordering::Acquire);
        loop {
            if current == UINT_MAX {
                return;
            }
            match self.done.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Linux `complete_all()`: every current and future waiter proceeds.
    pub fn complete_all(&self) {
        self.done.store(UINT_MAX, Ordering::Release);
    }

    /// Linux `try_wait_for_completion()`: consume a completion without
    /// blocking.
    pub fn try_wait(&self) -> bool {
        self.try_consume()
    }

    /// Linux `completion_done()`: true when a waiter would not block.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire) != 0
    }

    /// Linux `wait_for_completion()`: block without a timeout.
    pub fn wait(&self) {
        let _ = self.do_wait_for_common(MAX_SCHEDULE_TIMEOUT as i64, false);
    }

    /// Linux `wait_for_completion_timeout()`: returns the remaining jiffies
    /// (at least one) or zero when the timeout elapsed first.
    pub fn wait_timeout(&self, timeout_jiffies: i64) -> i64 {
        self.do_wait_for_common(timeout_jiffies, false)
    }

    /// Linux `wait_for_completion_interruptible()`: returns zero on
    /// completion or `-ERESTARTSYS` when a signal interrupts the wait.
    pub fn wait_interruptible(&self) -> i32 {
        let ret = self.do_wait_for_common(MAX_SCHEDULE_TIMEOUT as i64, true);
        if ret == -(ERESTARTSYS as i64) { -ERESTARTSYS } else { 0 }
    }

    /// Linux `wait_for_completion_interruptible_timeout()`.
    pub fn wait_interruptible_timeout(&self, timeout_jiffies: i64) -> i64 {
        self.do_wait_for_common(timeout_jiffies, true)
    }

    /// Consume one pending completion; `UINT_MAX` stays saturated.
    fn try_consume(&self) -> bool {
        let mut current = self.done.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return false;
            }
            if current == UINT_MAX {
                return true;
            }
            match self.done.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Linux `do_wait_for_common()` with the `schedule_timeout()` replaced by
    /// the polling loop described in the module documentation.
    fn do_wait_for_common(&self, timeout_jiffies: i64, interruptible: bool) -> i64 {
        let unbounded = timeout_jiffies >= MAX_SCHEDULE_TIMEOUT as i64;
        let tick_ns = 1_000_000_000 / CONFIG_HZ as u64;
        let timeout_ns = if unbounded {
            u64::MAX
        } else {
            (timeout_jiffies.max(0) as u64).saturating_mul(tick_ns)
        };
        let start = crate::clock::now_ns();
        loop {
            if self.try_consume() {
                if unbounded {
                    return MAX_SCHEDULE_TIMEOUT as i64;
                }
                let elapsed = crate::clock::now_ns().saturating_sub(start);
                let left_ns = timeout_ns.saturating_sub(elapsed);
                return (left_ns.div_ceil(tick_ns) as i64).max(1);
            }
            if interruptible && interrupted() {
                return -(ERESTARTSYS as i64);
            }
            if !unbounded {
                let elapsed = crate::clock::now_ns().saturating_sub(start);
                if elapsed >= timeout_ns {
                    return 0;
                }
            }
            crate::clock::relax();
        }
    }
}

/// Whether the current task has a pending signal. Host tests have no task
/// runtime, so the test build reports no signal.
#[cfg(not(test))]
fn interrupted() -> bool {
    signal::signal_pending_current()
}

#[cfg(test)]
fn interrupted() -> bool {
    false
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn complete_then_wait_returns_immediately_and_consumes_one_count() {
        let completion = Completion::new();
        assert!(!completion.is_done());
        completion.complete();
        assert!(completion.is_done());
        assert!(completion.wait_timeout(0) >= 1);
        assert!(!completion.is_done());
        assert_eq!(completion.wait_timeout(1), 0);
    }

    #[test]
    fn counts_accumulate_and_try_wait_consumes_one_at_a_time() {
        let completion = Completion::new();
        completion.complete();
        completion.complete();
        assert!(completion.try_wait());
        assert!(completion.try_wait());
        assert!(!completion.try_wait());
    }

    #[test]
    fn complete_all_saturates_and_never_consumes() {
        let completion = Completion::new();
        completion.complete_all();
        completion.complete();
        for _ in 0..3 {
            assert!(completion.try_wait());
            assert!(completion.is_done());
        }
        assert_eq!(completion.done.load(Ordering::Relaxed), UINT_MAX);
    }

    #[test]
    fn reinit_forgets_pending_completions() {
        let completion = Completion::new();
        completion.complete_all();
        completion.reinit();
        assert!(!completion.is_done());
        assert!(!completion.try_wait());
    }

    #[test]
    fn timed_wait_reports_zero_when_nothing_completes() {
        let completion = Completion::new();
        assert_eq!(completion.wait_timeout(1), 0);
        assert_eq!(completion.wait_interruptible_timeout(1), 0);
    }

    #[test]
    fn completion_from_another_thread_wakes_the_waiter() {
        let completion = Arc::new(Completion::new());
        let signaller = Arc::clone(&completion);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            signaller.complete();
        });
        assert!(completion.wait_timeout(500) >= 1);
        handle.join().unwrap();
    }

    #[test]
    fn unbounded_wait_returns_after_complete_all_from_another_thread() {
        let completion = Arc::new(Completion::new());
        let signaller = Arc::clone(&completion);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            signaller.complete_all();
        });
        completion.wait();
        assert_eq!(completion.wait_interruptible(), 0);
        handle.join().unwrap();
    }
}
