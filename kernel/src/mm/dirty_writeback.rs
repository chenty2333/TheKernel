//! Kernel flusher task for periodic dirty page-cache writeback.
//!
//! Linux runs `wb_workfn()` from a per-bdi work item. This kernel has one
//! flusher thread that wakes often, reads `vm.dirty_writeback_centisecs` live,
//! and runs the periodic pass once per interval. Policy and thresholds live in
//! `tk-axfs-ng` (`highlevel::file::periodic_writeback`).

use alloc::string::String;
use core::time::Duration;

/// Wake cadence. Shorter than the smallest useful interval, so a changed
/// `dirty_writeback_centisecs` takes effect within one poll.
const POLL_INTERVAL_MS: u64 = 100;

pub(crate) fn init_dirty_writeback() {
    let mut name = String::new();
    name.try_reserve_exact("dirty-writeback".len())
        .expect("failed to allocate dirty-writeback worker name");
    name.push_str("dirty-writeback");
    if let Err(error) = axtask::try_spawn_with_name(dirty_writeback_worker, name) {
        warn!("dirty-writeback: flusher unavailable: {error:?}");
    }
}

fn dirty_writeback_worker() {
    let mut last_pass_ms = axfs::writeback_clock_ms();
    loop {
        if let Err(error) = axtask::sleep(Duration::from_millis(POLL_INTERVAL_MS)) {
            error!("dirty-writeback worker stopped: {error}");
            return;
        }
        // Interval 0 disables periodic writeback. Keep polling, so a later
        // non-zero value takes effect without a restart.
        let interval_ms = u64::from(axfs::dirty_writeback_tunables().writeback_centisecs()) * 10;
        if interval_ms == 0 {
            continue;
        }
        let now_ms = axfs::writeback_clock_ms();
        if now_ms.saturating_sub(last_pass_ms) < interval_ms {
            continue;
        }
        last_pass_ms = now_ms;
        let _attempted = axfs::run_periodic_dirty_writeback(now_ms);
    }
}
