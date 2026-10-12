// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Monotonic clock, delay and relax hooks used by the timed shims (completion
//! waits, iopoll loops).
//!
//! Production reads the HAL clock and sleeps through the task runtime. Host
//! unit tests run on the dummy HAL platform, whose tick counter never
//! advances, so the test build uses a per-thread simulated clock: each clock
//! probe advances it by one microsecond and each delay advances it by its own
//! length. Timeout paths therefore terminate deterministically in tests.

#[cfg(not(test))]
mod imp {
    use core::time::Duration;

    /// Monotonic time in nanoseconds (`ktime_get()`).
    pub fn now_ns() -> u64 {
        axhal::time::monotonic_time_nanos()
    }

    /// Calibrated busy-wait (`udelay()`).
    pub fn delay_us(micros: u32) {
        axhal::time::busy_wait(Duration::from_micros(u64::from(micros)));
    }

    /// Sleep for `micros` (`usleep_range()` minimum), falling back to a busy
    /// wait when the caller cannot block.
    pub fn sleep_us(micros: u64) {
        if axtask::sleep(Duration::from_micros(micros)).is_err() {
            delay_us(micros.min(u64::from(u32::MAX)) as u32);
        }
    }

    /// One cooperative relax step inside a polling loop.
    pub fn relax() {
        if axtask::can_block_current() {
            axtask::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod imp {
    use std::cell::Cell;

    /// Simulated nanoseconds per clock probe.
    const PROBE_STEP_NS: u64 = 1_000;

    std::thread_local! {
        static NOW_NS: Cell<u64> = const { Cell::new(0) };
    }

    pub fn now_ns() -> u64 {
        NOW_NS.with(|now| {
            let next = now.get() + PROBE_STEP_NS;
            now.set(next);
            next
        })
    }

    pub fn delay_us(micros: u32) {
        NOW_NS.with(|now| now.set(now.get() + u64::from(micros) * 1_000));
    }

    pub fn sleep_us(micros: u64) {
        NOW_NS.with(|now| now.set(now.get() + micros * 1_000));
    }

    pub fn relax() {
        let _ = now_ns();
    }
}

pub use imp::{delay_us, now_ns, relax, sleep_us};
