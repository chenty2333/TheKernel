//! Background and threshold-driven writeback of disk-backed dirty page cache.
//!
//! Ports the policy of Linux `fs/fs-writeback.c` (`wb_check_old_data_flush()`,
//! `wb_check_background_flush()`, `writeback_sb_inodes()` dirty-expire aging)
//! and `mm/page-writeback.c` (`domain_dirty_limits()`, `balance_dirty_pages()`,
//! and the `vm.dirty_*` sysctls with their mutual-exclusion rules).
//!
//! Deliberate simplifications:
//! - One writeback domain. "Dirtyable memory" is every managed page, because
//!   the allocator has no LRU split between anonymous and file memory.
//! - Ages are per cached file, not per inode: `dirtied_when` is stamped when a
//!   file's first page becomes dirty and cleared when it has no dirty pages.
//! - A throttled writer flushes the oldest dirty files itself and yields
//!   between passes, instead of sleeping on a per-bdi flusher. Its pause budget
//!   is bounded, so a writer never waits forever on unflushable data.
//! - Writeback errors from the background path are published to the inode and
//!   superblock errseq (`publish_async_dirty_writeback_completion_error`), so
//!   the next `fsync`/`syncfs` reports them, matching `filemap_check_errors`.

use super::{reclaim::CachedFileWritebackSnapshotEntry, *};
use axfs_ng_vfs::WritebackAnchor;
use core::sync::atomic::{AtomicU32, AtomicU64};

const PAGE_BYTES: u64 = PAGE_SIZE as u64;
/// Linux `dirty_bytes_min`: `vm.dirty_bytes` must cover at least two pages.
const DIRTY_BYTES_MIN: u64 = 2 * PAGE_BYTES;
/// Linux keeps thresholds in 32-bit page counts.
const MAX_THRESHOLD_PAGES: u64 = u32::MAX as u64;

pub const DIRTY_BACKGROUND_RATIO_DEFAULT: u32 = 10;
pub const DIRTY_RATIO_DEFAULT: u32 = 20;
pub const DIRTY_WRITEBACK_CENTISECS_DEFAULT: u32 = 5 * 100;
pub const DIRTY_EXPIRE_CENTISECS_DEFAULT: u32 = 30 * 100;

/// A throttled writer returns after this long even if the cache is still over
/// its dirty limit; it pauses at most this many times per call.
pub(super) const BALANCE_MAX_PAUSES: usize = 200;
const BALANCE_MAX_ROUNDS: usize = 4096;

/// The six `vm.dirty_*` tunables, with Linux's mutual-exclusion rules:
/// setting a byte limit zeroes the matching ratio and vice versa.
pub struct DirtyWritebackTunables {
    background_ratio: AtomicU32,
    dirty_ratio: AtomicU32,
    background_bytes: AtomicU64,
    dirty_bytes: AtomicU64,
    writeback_centisecs: AtomicU32,
    expire_centisecs: AtomicU32,
}

/// Resolved global thresholds in pages. `background` is always below `dirty`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirtyLimits {
    pub background: u64,
    pub dirty: u64,
}

impl DirtyWritebackTunables {
    pub const fn new() -> Self {
        Self {
            background_ratio: AtomicU32::new(DIRTY_BACKGROUND_RATIO_DEFAULT),
            dirty_ratio: AtomicU32::new(DIRTY_RATIO_DEFAULT),
            background_bytes: AtomicU64::new(0),
            dirty_bytes: AtomicU64::new(0),
            writeback_centisecs: AtomicU32::new(DIRTY_WRITEBACK_CENTISECS_DEFAULT),
            expire_centisecs: AtomicU32::new(DIRTY_EXPIRE_CENTISECS_DEFAULT),
        }
    }

    pub fn background_ratio(&self) -> u32 {
        self.background_ratio.load(Ordering::Acquire)
    }

    pub fn dirty_ratio(&self) -> u32 {
        self.dirty_ratio.load(Ordering::Acquire)
    }

    pub fn background_bytes(&self) -> u64 {
        self.background_bytes.load(Ordering::Acquire)
    }

    pub fn dirty_bytes(&self) -> u64 {
        self.dirty_bytes.load(Ordering::Acquire)
    }

    pub fn writeback_centisecs(&self) -> u32 {
        self.writeback_centisecs.load(Ordering::Acquire)
    }

    pub fn expire_centisecs(&self) -> u32 {
        self.expire_centisecs.load(Ordering::Acquire)
    }

    /// `vm.dirty_background_ratio`: 0..=100. Zeroes `dirty_background_bytes`.
    pub fn set_background_ratio(&self, value: u32) -> VfsResult<()> {
        if value > 100 {
            return Err(VfsError::InvalidInput);
        }
        self.background_ratio.store(value, Ordering::Release);
        self.background_bytes.store(0, Ordering::Release);
        Ok(())
    }

    /// `vm.dirty_ratio`: 0..=100. Zeroes `dirty_bytes`.
    pub fn set_dirty_ratio(&self, value: u32) -> VfsResult<()> {
        if value > 100 {
            return Err(VfsError::InvalidInput);
        }
        self.dirty_ratio.store(value, Ordering::Release);
        self.dirty_bytes.store(0, Ordering::Release);
        Ok(())
    }

    /// `vm.dirty_background_bytes`. Zero is accepted and selects the ratio.
    /// Zeroes `dirty_background_ratio`.
    pub fn set_background_bytes(&self, value: u64) -> VfsResult<()> {
        if value.div_ceil(PAGE_BYTES) > MAX_THRESHOLD_PAGES {
            return Err(VfsError::InvalidInput);
        }
        self.background_bytes.store(value, Ordering::Release);
        self.background_ratio.store(0, Ordering::Release);
        Ok(())
    }

    /// `vm.dirty_bytes`: zero or at least two pages. Zeroes `dirty_ratio`.
    pub fn set_dirty_bytes(&self, value: u64) -> VfsResult<()> {
        if value != 0 && value < DIRTY_BYTES_MIN {
            return Err(VfsError::InvalidInput);
        }
        if value.div_ceil(PAGE_BYTES) > MAX_THRESHOLD_PAGES {
            return Err(VfsError::InvalidInput);
        }
        self.dirty_bytes.store(value, Ordering::Release);
        self.dirty_ratio.store(0, Ordering::Release);
        Ok(())
    }

    /// `vm.dirty_writeback_centisecs`. Zero disables periodic writeback.
    pub fn set_writeback_centisecs(&self, value: u32) {
        self.writeback_centisecs.store(value, Ordering::Release);
    }

    /// `vm.dirty_expire_centisecs`: a file dirtied at least this long ago is
    /// written by the periodic pass.
    pub fn set_expire_centisecs(&self, value: u32) {
        self.expire_centisecs.store(value, Ordering::Release);
    }

    /// Computes the global limits from the current tunables, following
    /// Linux `domain_dirty_limits()`.
    pub fn limits(&self, dirtyable_pages: u64) -> DirtyLimits {
        let bytes = self.dirty_bytes();
        let dirty = if bytes != 0 {
            bytes.div_ceil(PAGE_BYTES)
        } else {
            u64::from(self.dirty_ratio()) * dirtyable_pages / 100
        };
        let bg_bytes = self.background_bytes();
        let background = if bg_bytes != 0 {
            bg_bytes.div_ceil(PAGE_BYTES)
        } else {
            u64::from(self.background_ratio()) * dirtyable_pages / 100
        };
        let dirty = dirty.min(MAX_THRESHOLD_PAGES);
        let background = background.min(MAX_THRESHOLD_PAGES);
        let background = if background >= dirty {
            dirty / 2
        } else {
            background
        };
        DirtyLimits {
            background,
            dirty,
        }
    }
}

impl Default for DirtyWritebackTunables {
    fn default() -> Self {
        Self::new()
    }
}

static DIRTY_WRITEBACK_TUNABLES: DirtyWritebackTunables = DirtyWritebackTunables::new();

/// The system-wide `vm.dirty_*` tunables.
pub fn dirty_writeback_tunables() -> &'static DirtyWritebackTunables {
    &DIRTY_WRITEBACK_TUNABLES
}

/// Pages that can hold dirty data: every managed page, see the module note.
pub fn dirtyable_pages() -> u64 {
    let allocator = global_allocator();
    (allocator.used_pages() as u64).saturating_add(allocator.available_pages() as u64)
}

/// Monotonic clock in milliseconds, the time base of `dirtied_when`.
pub fn writeback_clock_ms() -> u64 {
    axhal::time::monotonic_time_nanos() / 1_000_000
}

impl CachedFileShared {
    /// Stamps the start of a dirty episode. Only the clean-to-dirty edge
    /// changes the stamp, so a redirtied file keeps its original age.
    pub(super) fn note_dirtied(&self, now_ms: u64) {
        let _ = self.dirtied_when.compare_exchange(
            0,
            now_ms.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Dirty-episode start time, or `None` when the file has no dirty pages.
    pub(super) fn dirtied_at_ms(&self) -> Option<u64> {
        match self.dirtied_when.load(Ordering::Acquire) {
            0 => None,
            stamp => Some(stamp - 1),
        }
    }
}

/// Disk-backed cached files with dirty pages, oldest dirty episode first.
fn dirty_candidates_oldest_first() -> Vec<(u64, CachedFileWritebackSnapshotEntry)> {
    let mut candidates = cached_file_writeback_snapshot()
        .into_iter()
        .filter_map(|entry| {
            let (_key, shared, _location) = &entry;
            if shared.in_memory || shared.unlinked.load(Ordering::Acquire) {
                return None;
            }
            shared.dirtied_at_ms().map(|dirtied| (dirtied, entry))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(dirtied, _)| *dirtied);
    candidates
}

/// Writes every dirty page of one cached file. Returns the page count the
/// flush attempted. A real writeback error is published to the errseq state
/// and reported as zero progress, because no caller is waiting for it.
fn write_back_cached_file(
    shared: &Arc<CachedFileShared>,
    location: &WritebackAnchor,
) -> usize {
    let dirty = cached_dirty_page_numbers(shared);
    if dirty.is_empty() {
        // Truncation can leave a stale episode stamp behind. Clearing it here
        // lets the next dirtying write start a fresh age.
        release_cached_file_writeback_anchor_if_clean(shared);
        return 0;
    }
    let Ok(file) = location.entry().as_file() else {
        return 0;
    };
    let attempted = dirty.len();
    match flush_dirty_page_list(shared, file, dirty, false) {
        Ok(()) => attempted,
        // Lease contention is a skipped attempt, not a device failure.
        Err(VfsError::ResourceBusy) => 0,
        Err(error) => {
            publish_async_dirty_writeback_completion_error(file, error);
            0
        }
    }
}

/// Writes dirty files oldest-first until `page_budget` pages are attempted.
pub(super) fn write_back_oldest_first(page_budget: usize) -> usize {
    let mut written = 0usize;
    for (_dirtied, (_key, shared, location)) in dirty_candidates_oldest_first() {
        if written >= page_budget {
            break;
        }
        written = written.saturating_add(write_back_cached_file(&shared, &location));
    }
    written
}

/// One periodic pass, the equivalent of Linux `wb_workfn()`.
///
/// Writes files whose dirty episode is at least `dirty_expire_centisecs` old
/// (`wb_check_old_data_flush`). Then, if the global dirty count is over the
/// background limit, writes oldest files until it is back to that limit
/// (`wb_check_background_flush`). Returns the number of pages attempted.
pub fn run_periodic_dirty_writeback_at(
    now_ms: u64,
    tunables: &DirtyWritebackTunables,
    dirtyable: u64,
    dirty_pages: u64,
) -> usize {
    let mut attempted = 0usize;
    if tunables.writeback_centisecs() != 0 {
        let expire_ms = u64::from(tunables.expire_centisecs()) * 10;
        for (dirtied, (_key, shared, location)) in dirty_candidates_oldest_first() {
            if now_ms.saturating_sub(dirtied) >= expire_ms {
                attempted = attempted.saturating_add(write_back_cached_file(&shared, &location));
            }
        }
    }
    let limits = tunables.limits(dirtyable);
    if dirty_pages > limits.background {
        let budget = (dirty_pages - limits.background) as usize;
        attempted = attempted.saturating_add(write_back_oldest_first(budget));
    }
    attempted
}

/// Kernel flusher entry point: one periodic pass against the live tunables
/// and global dirty count.
pub fn run_periodic_dirty_writeback(now_ms: u64) -> usize {
    run_periodic_dirty_writeback_at(
        now_ms,
        dirty_writeback_tunables(),
        dirtyable_pages(),
        dirty_file_page_cache_pages() as u64,
    )
}

/// Throttle loop of Linux `balance_dirty_pages()`. While the global dirty count
/// exceeds `limits.dirty`, the caller writes oldest files down to the
/// background limit. It pauses when no progress is made, and returns after a
/// bounded number of pauses or rounds.
pub(super) fn balance_dirty_pages_with(
    tunables: &DirtyWritebackTunables,
    dirtyable: u64,
    dirty_now: &dyn Fn() -> u64,
    pause: &dyn Fn(),
) {
    let limits = tunables.limits(dirtyable);
    let mut pauses = 0usize;
    for _ in 0..BALANCE_MAX_ROUNDS {
        let dirty = dirty_now();
        if dirty <= limits.dirty {
            return;
        }
        let budget = (dirty - limits.background).max(1) as usize;
        if write_back_oldest_first(budget) == 0 {
            if pauses >= BALANCE_MAX_PAUSES {
                return;
            }
            pauses += 1;
            pause();
        }
    }
}

impl CachedFile {
    /// Called after a successful write with no cache or inode lock held. Cheap
    /// unless the global dirty count is over `vm.dirty_ratio`.
    pub(super) fn throttle_dirty_writer(&self) {
        if self.shared.in_memory {
            return;
        }
        let tunables = dirty_writeback_tunables();
        if dirty_file_page_cache_pages() as u64 <= tunables.limits(dirtyable_pages()).dirty {
            return;
        }
        balance_dirty_pages_with(
            tunables,
            dirtyable_pages(),
            &|| dirty_file_page_cache_pages() as u64,
            &|| axtask::yield_now(),
        );
    }
}
