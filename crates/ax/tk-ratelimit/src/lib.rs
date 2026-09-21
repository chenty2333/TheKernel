//! Per-call-site rate limiting for log records, as Linux's
//! `printk_ratelimited()` does it.
//!
//! A record a remote peer, an unprivileged loop, a packet or a page can drive
//! is still reported at its real severity -- a failed block request is an
//! error, not a debug line -- but through [`error_ratelimited!`],
//! [`warn_ratelimited!`] or [`info_ratelimited!`]. Each of those expands to a
//! `static` [`RateLimit`] of its own, which is `DEFINE_RATELIMIT_STATE` in the
//! calling statement (`include/linux/printk.h`): one call site's storm spends
//! only that call site's budget, and no two call sites share one.
//!
//! [`RateLimit::admit`] is `___ratelimit()` (`lib/ratelimit.c`): `burst`
//! records per `interval`, a contended state answered from the remaining
//! burst rather than waited for, and, when a window reopens over refused
//! records, one `KERN_WARNING` line saying how many.
//!
//! The clock is the kernel's, installed once by the runtime through
//! [`set_clock`]. Until then every record is admitted: the only producers that
//! early are the boot's own, and a boot says each of those once.
#![no_std]

#[cfg(test)]
extern crate std;

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};

#[doc(hidden)]
pub use log;

/// `DEFAULT_RATELIMIT_INTERVAL`: `5 * HZ`.
pub const DEFAULT_RATELIMIT_INTERVAL_MS: u64 = 5 * 1000;
/// `DEFAULT_RATELIMIT_BURST`.
pub const DEFAULT_RATELIMIT_BURST: i32 = 10;

/// The monotonic millisecond clock, as a `fn() -> u64`; null until installed.
static CLOCK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the clock every [`RateLimit`] measures its window with.
pub fn set_clock(now_ms: fn() -> u64) {
    CLOCK.store(now_ms as *mut (), Ordering::Release);
}

fn now_ms() -> Option<u64> {
    let clock = CLOCK.load(Ordering::Acquire);
    if clock.is_null() {
        return None;
    }
    // SAFETY: the only non-null value ever stored is a `fn() -> u64` cast by
    // `set_clock`, and function pointers round-trip through `*mut ()`.
    let clock = unsafe { core::mem::transmute::<*mut (), fn() -> u64>(clock) };
    Some(clock())
}

/// `struct ratelimit_state`.
pub struct RateLimit {
    interval_ms: u64,
    burst: i32,
    /// `rs->lock`, only ever try-locked: a record is never delayed by another
    /// record's bookkeeping.
    lock: AtomicBool,
    /// `RATELIMIT_INITIALIZED`: the window opens at the first record, not at
    /// boot.
    initialized: AtomicBool,
    begin_ms: AtomicU64,
    /// `rs_n_left`: passes remaining in the current window.
    left: AtomicI32,
    /// `rs->missed`: refusals not yet reported.
    missed: AtomicU32,
}

impl RateLimit {
    /// `DEFINE_RATELIMIT_STATE(_rs, DEFAULT_RATELIMIT_INTERVAL,
    /// DEFAULT_RATELIMIT_BURST)`, which every `pr_*_ratelimited` uses.
    pub const fn with_defaults() -> Self {
        Self::new(DEFAULT_RATELIMIT_INTERVAL_MS, DEFAULT_RATELIMIT_BURST)
    }

    pub const fn new(interval_ms: u64, burst: i32) -> Self {
        Self {
            interval_ms,
            burst,
            lock: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            begin_ms: AtomicU64::new(0),
            left: AtomicI32::new(0),
            missed: AtomicU32::new(0),
        }
    }

    /// Take one pass from the remaining burst, if one is left.
    fn take(&self) -> bool {
        self.left.load(Ordering::Relaxed) > 0 && self.left.fetch_sub(1, Ordering::AcqRel) > 0
    }

    /// `___ratelimit()`: may the record at `target:line` be emitted?
    ///
    /// `target` and `line` name the call site in the suppression report, where
    /// Linux prints `__func__`.
    pub fn admit(&self, target: &str, line: u32) -> bool {
        let Some(now) = now_ms() else {
            return true;
        };
        // "Zero interval says never limit, otherwise, non-positive burst says
        // always limit."
        if self.interval_ms == 0 {
            return true;
        }
        if self.burst <= 0 {
            self.missed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Contended: answer from the burst without resetting the window,
            // accepting Linux's documented false positive at a window edge.
            let admitted = self.initialized.load(Ordering::Acquire) && self.take();
            if !admitted {
                self.missed.fetch_add(1, Ordering::Relaxed);
            }
            return admitted;
        }
        if !self.initialized.load(Ordering::Relaxed) {
            self.begin_ms.store(now, Ordering::Relaxed);
            self.left.store(self.burst, Ordering::Relaxed);
            self.initialized.store(true, Ordering::Release);
        }
        let mut suppressed = 0;
        // `time_is_before_jiffies(rs->begin + interval)`.
        if now.saturating_sub(self.begin_ms.load(Ordering::Relaxed)) > self.interval_ms {
            self.left.store(self.burst, Ordering::Relaxed);
            self.begin_ms.store(now, Ordering::Relaxed);
            suppressed = self.missed.swap(0, Ordering::Relaxed);
        }
        let admitted = self.take();
        self.lock.store(false, Ordering::Release);
        // Reported after the state is released, as `printk_deferred` keeps
        // Linux's report out of the caller's locks.
        if suppressed != 0 {
            log::log!(
                target: target,
                log::Level::Warn,
                "{target}:{line}: {suppressed} callbacks suppressed"
            );
        }
        if !admitted {
            self.missed.fetch_add(1, Ordering::Relaxed);
        }
        admitted
    }
}

/// `printk_ratelimited()` at a `log` level.
#[macro_export]
macro_rules! log_ratelimited {
    ($level:expr, $($arg:tt)+) => {{
        static RATELIMIT: $crate::RateLimit = $crate::RateLimit::with_defaults();
        if RATELIMIT.admit(::core::module_path!(), ::core::line!()) {
            $crate::log::log!($level, $($arg)+);
        }
    }};
}

/// `pr_err_ratelimited()`.
#[macro_export]
macro_rules! error_ratelimited {
    ($($arg:tt)+) => { $crate::log_ratelimited!($crate::log::Level::Error, $($arg)+) };
}

/// `pr_warn_ratelimited()`.
#[macro_export]
macro_rules! warn_ratelimited {
    ($($arg:tt)+) => { $crate::log_ratelimited!($crate::log::Level::Warn, $($arg)+) };
}

/// `pr_info_ratelimited()`.
#[macro_export]
macro_rules! info_ratelimited {
    ($($arg:tt)+) => { $crate::log_ratelimited!($crate::log::Level::Info, $($arg)+) };
}

#[cfg(test)]
mod tests {
    use std::{
        string::{String, ToString},
        sync::Mutex,
        vec::Vec,
    };

    use super::*;

    static NOW: AtomicU64 = AtomicU64::new(0);
    static SERIAL: Mutex<()> = Mutex::new(());
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct Capture;
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            SEEN.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    static CAPTURE: Capture = Capture;

    fn fake_clock() -> u64 {
        NOW.load(Ordering::Relaxed)
    }

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        let _ = log::set_logger(&CAPTURE);
        log::set_max_level(log::LevelFilter::Trace);
        set_clock(fake_clock);
        NOW.store(1_000, Ordering::Relaxed);
        SEEN.lock().unwrap().clear();
        guard
    }

    #[test]
    fn a_window_admits_its_burst_and_reports_the_rest_when_it_reopens() {
        let _guard = setup();
        let rs = RateLimit::new(5_000, 3);
        let admitted = (0..10).filter(|_| rs.admit("tk::site", 7)).count();
        assert_eq!(admitted, 3);
        assert!(SEEN.lock().unwrap().is_empty(), "nothing is reported mid-window");

        // Exactly one interval later is still the same window; past it is not.
        NOW.store(6_000, Ordering::Relaxed);
        assert!(!rs.admit("tk::site", 7));
        NOW.store(6_001, Ordering::Relaxed);
        assert!(rs.admit("tk::site", 7));
        assert_eq!(
            *SEEN.lock().unwrap(),
            ["tk::site:7: 8 callbacks suppressed"],
            "the reopening record reports every refusal of the closed window"
        );
    }

    #[test]
    fn a_contended_state_answers_from_the_remaining_burst() {
        let _guard = setup();
        let rs = RateLimit::new(5_000, 2);
        assert!(rs.admit("tk::site", 1));
        rs.lock.store(true, Ordering::Relaxed);
        assert!(rs.admit("tk::site", 1), "one pass was left");
        assert!(!rs.admit("tk::site", 1), "and a contended caller never waits");
        rs.lock.store(false, Ordering::Relaxed);
        assert_eq!(rs.missed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn zero_interval_never_limits_and_zero_burst_always_does() {
        let _guard = setup();
        let unlimited = RateLimit::new(0, 0);
        assert!((0..100).all(|_| unlimited.admit("tk::site", 1)));
        let closed = RateLimit::new(5_000, 0);
        assert!(!(0..3).any(|_| closed.admit("tk::site", 1)));
    }

    #[test]
    fn each_call_site_has_its_own_budget() {
        let _guard = setup();
        for _ in 0..(DEFAULT_RATELIMIT_BURST * 2) {
            warn_ratelimited!("first site");
        }
        warn_ratelimited!("second site");
        let seen = SEEN.lock().unwrap();
        assert_eq!(
            seen.iter().filter(|line| *line == "first site").count(),
            DEFAULT_RATELIMIT_BURST as usize
        );
        assert_eq!(seen.iter().filter(|line| *line == "second site").count(), 1);
    }
}
