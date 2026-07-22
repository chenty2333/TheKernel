//! Bounded first-stage system memory-pressure handling.
//!
//! This deliberately reclaims only clean disk-backed file-cache pages. It
//! does not claim anonymous reclaim, swap, PSI, or OOM policy; those require a
//! unified page ownership model that this kernel does not have yet.

use alloc::string::String;
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use axalloc::global_allocator;

const LOW_WATERMARK_DIVISOR: usize = 50;
const MIN_LOW_WATERMARK_PAGES: usize = 64;
const MAX_LOW_WATERMARK_PAGES: usize = 4096;
const RECLAIM_BATCH_PAGES: usize = 256;
const MAX_RECLAIM_PASSES_PER_WAKE: usize = 8;
const ACTIVE_POLL_INTERVAL_MS: u64 = 250;
const IDLE_POLL_INTERVAL_MS: u64 = 1000;
const MAX_NO_PROGRESS_INTERVAL_MS: u64 = 4000;

static PRESSURE_CHECKS: AtomicU64 = AtomicU64::new(0);
static PRESSURE_EVENTS: AtomicU64 = AtomicU64::new(0);
static RECLAIM_PASSES: AtomicU64 = AtomicU64::new(0);
static RECLAIMED_PAGES: AtomicU64 = AtomicU64::new(0);
static NO_PROGRESS_PASSES: AtomicU64 = AtomicU64::new(0);
static VISITED_REGISTRY_ENTRIES: AtomicU64 = AtomicU64::new(0);
static SCANNED_FILES: AtomicU64 = AtomicU64::new(0);
static SCANNED_PAGES: AtomicU64 = AtomicU64::new(0);
static DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);
static PINNED_PAGES: AtomicU64 = AtomicU64::new(0);
static WRITEBACK_PAGES: AtomicU64 = AtomicU64::new(0);
static BUSY_FILES: AtomicU64 = AtomicU64::new(0);
static MAPPED_FILES: AtomicU64 = AtomicU64::new(0);
static SCAN_BUDGET_EXHAUSTED_FILES: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_TRUNCATIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryWatermarks {
    pub low_pages: usize,
    pub high_pages: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryPressureSnapshot {
    pub total_pages: usize,
    pub free_pages: usize,
    pub low_watermark_pages: usize,
    pub high_watermark_pages: usize,
    pub checks: u64,
    pub pressure_events: u64,
    pub reclaim_passes: u64,
    pub reclaimed_pages: u64,
    pub no_progress_passes: u64,
    pub visited_registry_entries: u64,
    pub scanned_files: u64,
    pub scanned_pages: u64,
    pub dirty_pages: u64,
    pub pinned_pages: u64,
    pub writeback_pages: u64,
    pub busy_files: u64,
    pub mapped_files: u64,
    pub scan_budget_exhausted_files: u64,
    pub snapshot_truncations: u64,
}

pub fn memory_watermarks(total_pages: usize) -> MemoryWatermarks {
    if total_pages == 0 {
        return MemoryWatermarks::default();
    }
    let low_pages = (total_pages / LOW_WATERMARK_DIVISOR)
        .clamp(
            MIN_LOW_WATERMARK_PAGES.min(total_pages),
            MAX_LOW_WATERMARK_PAGES,
        )
        .min(total_pages);
    MemoryWatermarks {
        low_pages,
        high_pages: low_pages.saturating_mul(2).min(total_pages),
    }
}

/// Linux-style best-effort availability: free pages above the low watermark
/// plus a conservative lower bound of immediately reclaimable clean cache.
pub fn available_memory_pages(
    total_pages: usize,
    free_pages: usize,
    reclaimable_clean_pages: usize,
) -> usize {
    let watermarks = memory_watermarks(total_pages);
    free_pages
        .saturating_add(reclaimable_clean_pages)
        .saturating_sub(watermarks.low_pages)
        .min(total_pages)
}

fn allocator_state() -> (usize, usize) {
    let allocator = global_allocator();
    let free_pages = allocator.available_pages();
    (
        allocator.used_pages().saturating_add(free_pages),
        free_pages,
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MemoryPressurePass {
    under_pressure: bool,
    reclaimed_pages: usize,
    visited_registry_entries: usize,
    registry_entries: usize,
    more_inode_scan_work: bool,
}

/// Executes one bounded background reclaim pass when free memory is below the
/// low watermark. This is not an allocation-retry hook: callers can still see
/// allocation failure before the periodic worker runs.
fn reclaim_memory_pressure_once() -> MemoryPressurePass {
    PRESSURE_CHECKS.fetch_add(1, Ordering::Relaxed);
    let (total_pages, free_pages) = allocator_state();
    let watermarks = memory_watermarks(total_pages);
    if free_pages >= watermarks.low_pages {
        return MemoryPressurePass::default();
    }

    PRESSURE_EVENTS.fetch_add(1, Ordering::Relaxed);
    let target = watermarks
        .high_pages
        .saturating_sub(free_pages)
        .min(RECLAIM_BATCH_PAGES);
    let report = axfs::reclaim_clean_cached_file_pages(target);
    RECLAIM_PASSES.fetch_add(1, Ordering::Relaxed);
    RECLAIMED_PAGES.fetch_add(report.reclaimed_pages as u64, Ordering::Relaxed);
    VISITED_REGISTRY_ENTRIES.fetch_add(report.visited_registry_entries as u64, Ordering::Relaxed);
    SCANNED_FILES.fetch_add(report.scanned_files as u64, Ordering::Relaxed);
    SCANNED_PAGES.fetch_add(report.scanned_pages as u64, Ordering::Relaxed);
    DIRTY_PAGES.fetch_add(report.dirty_pages as u64, Ordering::Relaxed);
    PINNED_PAGES.fetch_add(report.pinned_pages as u64, Ordering::Relaxed);
    WRITEBACK_PAGES.fetch_add(report.writeback_pages as u64, Ordering::Relaxed);
    BUSY_FILES.fetch_add(report.busy_files as u64, Ordering::Relaxed);
    MAPPED_FILES.fetch_add(report.mapped_files as u64, Ordering::Relaxed);
    SCAN_BUDGET_EXHAUSTED_FILES
        .fetch_add(report.scan_budget_exhausted_files as u64, Ordering::Relaxed);
    SNAPSHOT_TRUNCATIONS.fetch_add(u64::from(report.snapshot_truncated), Ordering::Relaxed);
    if report.reclaimed_pages == 0 {
        NO_PROGRESS_PASSES.fetch_add(1, Ordering::Relaxed);
    }
    MemoryPressurePass {
        under_pressure: true,
        reclaimed_pages: report.reclaimed_pages,
        visited_registry_entries: report.visited_registry_entries,
        registry_entries: report.registry_entries,
        more_inode_scan_work: report.scan_budget_exhausted_files != 0,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NoProgressSweep {
    visited_entries: usize,
    registry_entries: usize,
    saw_inode_scan_work: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SweepObservation {
    Continue,
    Complete,
}

impl NoProgressSweep {
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Advances a bounded registry sweep. A completed registry traversal is
    /// not a true no-candidate result when an inode still has an unvisited LRU
    /// window; in that case the next traversal continues its persistent LRU
    /// cursor before backoff is allowed.
    fn observe(&mut self, pass: MemoryPressurePass) -> SweepObservation {
        if pass.registry_entries == 0 {
            self.reset();
            return SweepObservation::Complete;
        }
        self.visited_entries = self
            .visited_entries
            .saturating_add(pass.visited_registry_entries);
        self.registry_entries = pass.registry_entries;
        self.saw_inode_scan_work |= pass.more_inode_scan_work;
        if self.visited_entries < self.registry_entries {
            return SweepObservation::Continue;
        }

        if self.saw_inode_scan_work {
            self.reset();
            SweepObservation::Continue
        } else {
            self.reset();
            SweepObservation::Complete
        }
    }
}

pub fn memory_pressure_snapshot() -> MemoryPressureSnapshot {
    let (total_pages, free_pages) = allocator_state();
    let watermarks = memory_watermarks(total_pages);
    MemoryPressureSnapshot {
        total_pages,
        free_pages,
        low_watermark_pages: watermarks.low_pages,
        high_watermark_pages: watermarks.high_pages,
        checks: PRESSURE_CHECKS.load(Ordering::Relaxed),
        pressure_events: PRESSURE_EVENTS.load(Ordering::Relaxed),
        reclaim_passes: RECLAIM_PASSES.load(Ordering::Relaxed),
        reclaimed_pages: RECLAIMED_PAGES.load(Ordering::Relaxed),
        no_progress_passes: NO_PROGRESS_PASSES.load(Ordering::Relaxed),
        visited_registry_entries: VISITED_REGISTRY_ENTRIES.load(Ordering::Relaxed),
        scanned_files: SCANNED_FILES.load(Ordering::Relaxed),
        scanned_pages: SCANNED_PAGES.load(Ordering::Relaxed),
        dirty_pages: DIRTY_PAGES.load(Ordering::Relaxed),
        pinned_pages: PINNED_PAGES.load(Ordering::Relaxed),
        writeback_pages: WRITEBACK_PAGES.load(Ordering::Relaxed),
        busy_files: BUSY_FILES.load(Ordering::Relaxed),
        mapped_files: MAPPED_FILES.load(Ordering::Relaxed),
        scan_budget_exhausted_files: SCAN_BUDGET_EXHAUSTED_FILES.load(Ordering::Relaxed),
        snapshot_truncations: SNAPSHOT_TRUNCATIONS.load(Ordering::Relaxed),
    }
}

fn memory_pressure_worker() {
    let mut poll_interval_ms = ACTIVE_POLL_INTERVAL_MS;
    let mut no_progress_interval_ms = ACTIVE_POLL_INTERVAL_MS;
    let mut no_progress_sweep = NoProgressSweep::default();
    let mut pressure_episode_active = false;
    loop {
        if let Err(error) = axtask::sleep(Duration::from_millis(poll_interval_ms)) {
            error!("memory-pressure worker stopped: {error}");
            return;
        }
        let mut reclaimed_this_wake = 0usize;
        let mut under_pressure = false;
        let mut completed_no_progress_sweep = false;
        for _ in 0..MAX_RECLAIM_PASSES_PER_WAKE {
            let pass = reclaim_memory_pressure_once();
            if !pass.under_pressure {
                break;
            }
            under_pressure = true;
            reclaimed_this_wake = reclaimed_this_wake.saturating_add(pass.reclaimed_pages);
            if pass.reclaimed_pages != 0 {
                no_progress_sweep.reset();
            } else if no_progress_sweep.observe(pass) == SweepObservation::Complete {
                axfs::advance_clean_cached_file_reclaim_scan_epoch();
                completed_no_progress_sweep = true;
                break;
            }
            let (total_pages, free_pages) = allocator_state();
            if free_pages >= memory_watermarks(total_pages).high_pages {
                break;
            }
            axtask::yield_now();
        }

        let (total_pages, free_pages) = allocator_state();
        pressure_episode_active |= under_pressure;
        if !under_pressure || free_pages >= memory_watermarks(total_pages).low_pages {
            if pressure_episode_active {
                // A later pressure episode must not inherit per-inode
                // completed markers from this one.
                axfs::advance_clean_cached_file_reclaim_scan_epoch();
                pressure_episode_active = false;
            }
            no_progress_sweep.reset();
            no_progress_interval_ms = ACTIVE_POLL_INTERVAL_MS;
            poll_interval_ms = IDLE_POLL_INTERVAL_MS;
        } else if reclaimed_this_wake != 0 {
            no_progress_interval_ms = ACTIVE_POLL_INTERVAL_MS;
            poll_interval_ms = ACTIVE_POLL_INTERVAL_MS;
        } else if completed_no_progress_sweep {
            no_progress_interval_ms = no_progress_interval_ms
                .saturating_mul(2)
                .min(MAX_NO_PROGRESS_INTERVAL_MS);
            poll_interval_ms = no_progress_interval_ms;
        } else {
            // A large registry can require more than one bounded wake to
            // cover. Keep the cursor moving at the active interval until one
            // complete no-progress sweep has actually been observed.
            poll_interval_ms = ACTIVE_POLL_INTERVAL_MS;
        }
    }
}

pub(crate) fn init_memory_pressure() {
    let mut name = String::new();
    name.try_reserve_exact("memory-pressure".len())
        .expect("failed to allocate memory-pressure worker name");
    name.push_str("memory-pressure");
    axtask::try_spawn_with_name(memory_pressure_worker, name)
        .expect("failed to start memory-pressure worker");
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryPressurePass, NoProgressSweep, SweepObservation, available_memory_pages,
        memory_watermarks,
    };

    #[test]
    fn watermarks_are_bounded_for_small_and_large_machines() {
        assert_eq!(memory_watermarks(0).low_pages, 0);
        assert_eq!(memory_watermarks(32).low_pages, 32);
        assert_eq!(memory_watermarks(32).high_pages, 32);
        assert_eq!(memory_watermarks(32_768).low_pages, 655);
        assert_eq!(memory_watermarks(1_000_000).low_pages, 4096);
        assert_eq!(memory_watermarks(1_000_000).high_pages, 8192);
    }

    #[test]
    fn availability_reserves_low_watermark_and_caps_total() {
        assert_eq!(available_memory_pages(1000, 500, 100), 536);
        assert_eq!(available_memory_pages(1000, 10, 0), 0);
        assert_eq!(available_memory_pages(1000, 10, 100), 46);
        assert_eq!(available_memory_pages(1000, 1000, 1000), 1000);
    }

    fn no_progress_pass(
        visited_registry_entries: usize,
        registry_entries: usize,
        more_inode_scan_work: bool,
    ) -> MemoryPressurePass {
        MemoryPressurePass {
            under_pressure: true,
            reclaimed_pages: 0,
            visited_registry_entries,
            registry_entries,
            more_inode_scan_work,
        }
    }

    #[test]
    fn no_progress_sweep_waits_for_a_complete_large_registry() {
        let mut sweep = NoProgressSweep::default();
        assert_eq!(
            sweep.observe(no_progress_pass(64, 100, false)),
            SweepObservation::Continue
        );
        assert_eq!(
            sweep.observe(no_progress_pass(36, 100, false)),
            SweepObservation::Complete
        );
    }

    #[test]
    fn no_progress_sweep_handles_registry_change_and_inode_scan_debt() {
        let mut sweep = NoProgressSweep::default();
        assert_eq!(
            sweep.observe(no_progress_pass(64, 100, false)),
            SweepObservation::Continue
        );
        assert_eq!(
            sweep.observe(no_progress_pass(0, 0, false)),
            SweepObservation::Complete
        );

        assert_eq!(
            sweep.observe(no_progress_pass(1, 1, true)),
            SweepObservation::Continue
        );
        assert_eq!(
            sweep.observe(no_progress_pass(1, 1, false)),
            SweepObservation::Complete
        );
    }

    #[test]
    fn reclaim_progress_resets_no_progress_sweep() {
        let mut sweep = NoProgressSweep::default();
        assert_eq!(
            sweep.observe(no_progress_pass(32, 100, false)),
            SweepObservation::Continue
        );
        sweep.reset();
        assert_eq!(
            sweep.observe(no_progress_pass(64, 64, false)),
            SweepObservation::Complete
        );
    }
}
