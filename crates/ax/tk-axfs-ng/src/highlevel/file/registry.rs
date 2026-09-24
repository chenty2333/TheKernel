//! Cached-file identities, I/O counters, the shadow store and the cached-file registry.

use super::*;

/// Stable identity for one cached inode generation.
///
/// The device/inode pair is only a filesystem-visible slot and can be reused
/// after unlink.  `object` is a monotonically allocated generation token
/// carried by the identity lease in both the per-inode user data and the cache
/// shared state.  The key itself is copyable and non-owning so global
/// registries cannot pin an inode or leak an idle cache entry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CachedFileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) object: u64,
}

impl CachedFileIdentity {
    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }

    pub const fn object(self) -> u64 {
        self.object
    }
}

/// The identity generation lease carries no filesystem state.  Its strong
/// reference only keeps the generation attached to a live cache/futex lease;
/// the token itself is never recycled, even if a stale copy remains in a
/// bounded scan cursor.
pub(super) struct CachedFileIdentityLease {
    pub(super) object: u64,
    pub(super) discarded_unlinked: AtomicBool,
}

impl CachedFileIdentityLease {
    pub(super) const fn object(&self) -> u64 {
        self.object
    }
}

pub(super) type CachedFileRegistryKey = CachedFileIdentity;
pub(super) static NEXT_CACHED_FILE_IDENTITY: AtomicU64 = AtomicU64::new(1);
pub(super) static FILE_CACHE_REGISTRY: Once<Mutex<BTreeMap<CachedFileRegistryKey, FileUserData>>> =
    Once::new();
pub(super) static FILE_CACHE_ESTIMATE_CURSOR: Once<Mutex<Option<CachedFileRegistryKey>>> =
    Once::new();
pub(super) static FILE_CACHE_RECLAIM_CURSOR: Once<Mutex<Option<CachedFileRegistryKey>>> =
    Once::new();
pub(super) static FILE_CACHE_RECLAIM_SCAN_EPOCH: AtomicU64 = AtomicU64::new(1);
pub(super) static FILE_CACHE_NONRESIDENT_AGE: AtomicU64 = AtomicU64::new(0);
pub(super) static FILE_CACHE_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);
pub(super) static FILE_CACHE_ACTIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub(super) static FILE_CACHE_SHADOWS: Once<Mutex<FileCacheShadowStore>> = Once::new();
pub(super) static FILE_CACHE_MANAGED_PAGES_ONCE: Once<()> = Once::new();
pub(super) static FILE_CACHE_MANAGED_PAGES: AtomicUsize = AtomicUsize::new(0);
pub(super) static ENABLE_CACHED_FILE_IO_COUNTERS: AtomicBool = AtomicBool::new(false);
pub(super) static READ_BYPASS_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_SLICE_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_SLICE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_REJECT_IN_MEMORY: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_REJECT_UNALIGNED: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_REJECT_CACHED: AtomicU64 = AtomicU64::new(0);
pub(super) static READ_BYPASS_EOF_RACES: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_SLICE_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_SLICE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_REJECT_IN_MEMORY: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_BYPASS_REJECT_UNALIGNED: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_NO_READ_INSERT_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static WRITE_NO_READ_INSERT_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static FLUSH_DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static RANGE_FLUSH_DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static RANGE_FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_ERRORS: AtomicU64 = AtomicU64::new(0);
pub(super) static ENABLE_ASYNC_DIRTY_FLUSH_SG: AtomicBool = AtomicBool::new(false);
pub(super) static ASYNC_DIRTY_FLUSH_SG_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_SG_SEGMENTS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
pub(super) static ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS: AtomicU64 = AtomicU64::new(0);
pub(super) static ENABLE_CACHED_READAHEAD: AtomicBool = AtomicBool::new(false);
pub(super) static READAHEAD_MISSES: AtomicU64 = AtomicU64::new(0);
pub(super) static READAHEAD_WINDOWS: AtomicU64 = AtomicU64::new(0);
pub(super) static READAHEAD_PAGES_LOADED: AtomicU64 = AtomicU64::new(0);
pub(super) static READAHEAD_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static READAHEAD_PRESSURE_SKIPS: AtomicU64 = AtomicU64::new(0);
pub(super) static READAHEAD_RETIRED_UNUSED_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static SYNC_DATA_ONLY_REQUESTS: AtomicU64 = AtomicU64::new(0);
pub(super) static SYNC_METADATA_REQUESTS: AtomicU64 = AtomicU64::new(0);
pub(super) static SYNC_DATA_ONLY_METADATA_FALLBACKS: AtomicU64 = AtomicU64::new(0);
pub(super) static RANGE_INVALIDATE_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_REOPEN_HITS: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_RELEASES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_TRIM_RELEASES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_TRIM_PAGES: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAIN_EPOCH: AtomicU64 = AtomicU64::new(0);
pub(super) static CLOSED_FILE_CACHE_RETAINED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Disabled-by-default counters for cached-file direct/bypass experiments.
#[derive(Debug, Clone, Copy, Default)]
pub struct CachedFileIoCounters {
    pub read_bypass_eligible: u64,
    pub read_bypass_hits: u64,
    pub read_bypass_bytes: u64,
    pub read_bypass_slice_hits: u64,
    pub read_bypass_slice_bytes: u64,
    pub read_bypass_reject_in_memory: u64,
    pub read_bypass_reject_unaligned: u64,
    pub read_bypass_reject_cached: u64,
    pub read_bypass_eof_races: u64,
    pub write_bypass_eligible: u64,
    pub write_bypass_hits: u64,
    pub write_bypass_bytes: u64,
    pub write_bypass_slice_hits: u64,
    pub write_bypass_slice_bytes: u64,
    pub write_bypass_reject_in_memory: u64,
    pub write_bypass_reject_unaligned: u64,
    pub write_no_read_insert_pages: u64,
    pub write_no_read_insert_bytes: u64,
    pub flush_dirty_pages: u64,
    pub flush_bytes: u64,
    pub range_flush_dirty_pages: u64,
    pub range_flush_bytes: u64,
    pub async_dirty_flush_hits: u64,
    pub async_dirty_flush_pages: u64,
    pub async_dirty_flush_bytes: u64,
    pub async_dirty_flush_errors: u64,
    pub async_dirty_flush_sg_enabled: u64,
    pub async_dirty_flush_sg_hits: u64,
    pub async_dirty_flush_sg_segments: u64,
    pub async_dirty_flush_sg_async_submit_hits: u64,
    pub async_dirty_flush_sg_async_submit_segments: u64,
    pub async_dirty_flush_bounce_fallbacks: u64,
    pub async_dirty_flush_writeback_restarts: u64,
    pub readahead_enabled: u64,
    pub readahead_window_pages: u64,
    pub readahead_misses: u64,
    pub readahead_windows: u64,
    pub readahead_pages: u64,
    pub readahead_hits: u64,
    pub readahead_pressure_skips: u64,
    pub readahead_retired_unused_pages: u64,
    pub sync_data_only_requests: u64,
    pub sync_metadata_requests: u64,
    pub sync_data_only_metadata_fallbacks: u64,
    pub range_invalidate_pages: u64,
    pub closed_cache_retain_attempts: u64,
    pub closed_cache_retain_hits: u64,
    pub closed_cache_retain_pages: u64,
    pub closed_cache_retain_reject_pages: u64,
    pub closed_cache_reopen_hits: u64,
    pub closed_cache_retain_releases: u64,
    pub closed_cache_trim_releases: u64,
    pub closed_cache_trim_pages: u64,
    pub closed_cache_trim_flush_errors: u64,
    pub closed_cache_retained_pages_current: u64,
}

pub fn set_cached_file_io_counters_enabled(enabled: bool) {
    ENABLE_CACHED_FILE_IO_COUNTERS.store(enabled, Ordering::Relaxed);
}

pub fn set_async_dirty_flush_sg_enabled(enabled: bool) {
    ENABLE_ASYNC_DIRTY_FLUSH_SG.store(enabled, Ordering::Relaxed);
}

pub fn set_cached_readahead_enabled(enabled: bool) {
    ENABLE_CACHED_READAHEAD.store(enabled, Ordering::Relaxed);
}

pub fn reset_cached_file_io_counters() {
    for counter in [
        &READ_BYPASS_ELIGIBLE,
        &READ_BYPASS_HITS,
        &READ_BYPASS_BYTES,
        &READ_BYPASS_SLICE_HITS,
        &READ_BYPASS_SLICE_BYTES,
        &READ_BYPASS_REJECT_IN_MEMORY,
        &READ_BYPASS_REJECT_UNALIGNED,
        &READ_BYPASS_REJECT_CACHED,
        &READ_BYPASS_EOF_RACES,
        &WRITE_BYPASS_ELIGIBLE,
        &WRITE_BYPASS_HITS,
        &WRITE_BYPASS_BYTES,
        &WRITE_BYPASS_SLICE_HITS,
        &WRITE_BYPASS_SLICE_BYTES,
        &WRITE_BYPASS_REJECT_IN_MEMORY,
        &WRITE_BYPASS_REJECT_UNALIGNED,
        &WRITE_NO_READ_INSERT_PAGES,
        &WRITE_NO_READ_INSERT_BYTES,
        &FLUSH_DIRTY_PAGES,
        &FLUSH_BYTES,
        &RANGE_FLUSH_DIRTY_PAGES,
        &RANGE_FLUSH_BYTES,
        &ASYNC_DIRTY_FLUSH_HITS,
        &ASYNC_DIRTY_FLUSH_PAGES,
        &ASYNC_DIRTY_FLUSH_BYTES,
        &ASYNC_DIRTY_FLUSH_ERRORS,
        &ASYNC_DIRTY_FLUSH_SG_HITS,
        &ASYNC_DIRTY_FLUSH_SG_SEGMENTS,
        &ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS,
        &ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS,
        &ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS,
        &ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS,
        &READAHEAD_MISSES,
        &READAHEAD_WINDOWS,
        &READAHEAD_PAGES_LOADED,
        &READAHEAD_HITS,
        &READAHEAD_PRESSURE_SKIPS,
        &READAHEAD_RETIRED_UNUSED_PAGES,
        &SYNC_DATA_ONLY_REQUESTS,
        &SYNC_METADATA_REQUESTS,
        &SYNC_DATA_ONLY_METADATA_FALLBACKS,
        &RANGE_INVALIDATE_PAGES,
        &CLOSED_FILE_CACHE_RETAIN_ATTEMPTS,
        &CLOSED_FILE_CACHE_RETAIN_HITS,
        &CLOSED_FILE_CACHE_RETAIN_PAGES,
        &CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES,
        &CLOSED_FILE_CACHE_REOPEN_HITS,
        &CLOSED_FILE_CACHE_RETAIN_RELEASES,
        &CLOSED_FILE_CACHE_TRIM_RELEASES,
        &CLOSED_FILE_CACHE_TRIM_PAGES,
        &CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn cached_file_io_counters_snapshot() -> CachedFileIoCounters {
    CachedFileIoCounters {
        read_bypass_eligible: READ_BYPASS_ELIGIBLE.load(Ordering::Relaxed),
        read_bypass_hits: READ_BYPASS_HITS.load(Ordering::Relaxed),
        read_bypass_bytes: READ_BYPASS_BYTES.load(Ordering::Relaxed),
        read_bypass_slice_hits: READ_BYPASS_SLICE_HITS.load(Ordering::Relaxed),
        read_bypass_slice_bytes: READ_BYPASS_SLICE_BYTES.load(Ordering::Relaxed),
        read_bypass_reject_in_memory: READ_BYPASS_REJECT_IN_MEMORY.load(Ordering::Relaxed),
        read_bypass_reject_unaligned: READ_BYPASS_REJECT_UNALIGNED.load(Ordering::Relaxed),
        read_bypass_reject_cached: READ_BYPASS_REJECT_CACHED.load(Ordering::Relaxed),
        read_bypass_eof_races: READ_BYPASS_EOF_RACES.load(Ordering::Relaxed),
        write_bypass_eligible: WRITE_BYPASS_ELIGIBLE.load(Ordering::Relaxed),
        write_bypass_hits: WRITE_BYPASS_HITS.load(Ordering::Relaxed),
        write_bypass_bytes: WRITE_BYPASS_BYTES.load(Ordering::Relaxed),
        write_bypass_slice_hits: WRITE_BYPASS_SLICE_HITS.load(Ordering::Relaxed),
        write_bypass_slice_bytes: WRITE_BYPASS_SLICE_BYTES.load(Ordering::Relaxed),
        write_bypass_reject_in_memory: WRITE_BYPASS_REJECT_IN_MEMORY.load(Ordering::Relaxed),
        write_bypass_reject_unaligned: WRITE_BYPASS_REJECT_UNALIGNED.load(Ordering::Relaxed),
        write_no_read_insert_pages: WRITE_NO_READ_INSERT_PAGES.load(Ordering::Relaxed),
        write_no_read_insert_bytes: WRITE_NO_READ_INSERT_BYTES.load(Ordering::Relaxed),
        flush_dirty_pages: FLUSH_DIRTY_PAGES.load(Ordering::Relaxed),
        flush_bytes: FLUSH_BYTES.load(Ordering::Relaxed),
        range_flush_dirty_pages: RANGE_FLUSH_DIRTY_PAGES.load(Ordering::Relaxed),
        range_flush_bytes: RANGE_FLUSH_BYTES.load(Ordering::Relaxed),
        async_dirty_flush_hits: ASYNC_DIRTY_FLUSH_HITS.load(Ordering::Relaxed),
        async_dirty_flush_pages: ASYNC_DIRTY_FLUSH_PAGES.load(Ordering::Relaxed),
        async_dirty_flush_bytes: ASYNC_DIRTY_FLUSH_BYTES.load(Ordering::Relaxed),
        async_dirty_flush_errors: ASYNC_DIRTY_FLUSH_ERRORS.load(Ordering::Relaxed),
        async_dirty_flush_sg_enabled: ENABLE_ASYNC_DIRTY_FLUSH_SG.load(Ordering::Relaxed) as u64,
        async_dirty_flush_sg_hits: ASYNC_DIRTY_FLUSH_SG_HITS.load(Ordering::Relaxed),
        async_dirty_flush_sg_segments: ASYNC_DIRTY_FLUSH_SG_SEGMENTS.load(Ordering::Relaxed),
        async_dirty_flush_sg_async_submit_hits: ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS
            .load(Ordering::Relaxed),
        async_dirty_flush_sg_async_submit_segments: ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS
            .load(Ordering::Relaxed),
        async_dirty_flush_bounce_fallbacks: ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS
            .load(Ordering::Relaxed),
        async_dirty_flush_writeback_restarts: ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS
            .load(Ordering::Relaxed),
        readahead_enabled: ENABLE_CACHED_READAHEAD.load(Ordering::Relaxed) as u64,
        readahead_window_pages: READAHEAD_PAGES as u64,
        readahead_misses: READAHEAD_MISSES.load(Ordering::Relaxed),
        readahead_windows: READAHEAD_WINDOWS.load(Ordering::Relaxed),
        readahead_pages: READAHEAD_PAGES_LOADED.load(Ordering::Relaxed),
        readahead_hits: READAHEAD_HITS.load(Ordering::Relaxed),
        readahead_pressure_skips: READAHEAD_PRESSURE_SKIPS.load(Ordering::Relaxed),
        readahead_retired_unused_pages: READAHEAD_RETIRED_UNUSED_PAGES.load(Ordering::Relaxed),
        sync_data_only_requests: SYNC_DATA_ONLY_REQUESTS.load(Ordering::Relaxed),
        sync_metadata_requests: SYNC_METADATA_REQUESTS.load(Ordering::Relaxed),
        sync_data_only_metadata_fallbacks: SYNC_DATA_ONLY_METADATA_FALLBACKS
            .load(Ordering::Relaxed),
        range_invalidate_pages: RANGE_INVALIDATE_PAGES.load(Ordering::Relaxed),
        closed_cache_retain_attempts: CLOSED_FILE_CACHE_RETAIN_ATTEMPTS.load(Ordering::Relaxed),
        closed_cache_retain_hits: CLOSED_FILE_CACHE_RETAIN_HITS.load(Ordering::Relaxed),
        closed_cache_retain_pages: CLOSED_FILE_CACHE_RETAIN_PAGES.load(Ordering::Relaxed),
        closed_cache_retain_reject_pages: CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES
            .load(Ordering::Relaxed),
        closed_cache_reopen_hits: CLOSED_FILE_CACHE_REOPEN_HITS.load(Ordering::Relaxed),
        closed_cache_retain_releases: CLOSED_FILE_CACHE_RETAIN_RELEASES.load(Ordering::Relaxed),
        closed_cache_trim_releases: CLOSED_FILE_CACHE_TRIM_RELEASES.load(Ordering::Relaxed),
        closed_cache_trim_pages: CLOSED_FILE_CACHE_TRIM_PAGES.load(Ordering::Relaxed),
        closed_cache_trim_flush_errors: CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS.load(Ordering::Relaxed),
        closed_cache_retained_pages_current: CLOSED_FILE_CACHE_RETAINED_PAGES
            .load(Ordering::Relaxed) as u64,
    }
}

#[inline(always)]
pub(super) fn cached_file_io_counters_enabled() -> bool {
    ENABLE_CACHED_FILE_IO_COUNTERS.load(Ordering::Relaxed)
}

#[inline(always)]
pub(super) fn record_cached_file_counter(counter: &AtomicU64, value: u64) {
    if cached_file_io_counters_enabled() {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

pub(super) fn file_cache_registry() -> &'static Mutex<BTreeMap<CachedFileRegistryKey, FileUserData>>
{
    FILE_CACHE_REGISTRY.call_once(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct CachedFileShadowKey {
    pub(super) identity: CachedFileIdentity,
    pub(super) page: u32,
}

/// A deliberately small, allocation-aware LRU for non-resident workingset
/// entries.  `lru::LruCache` owns one heap allocation per inserted entry, so
/// merely resizing its map before detaching a cache page does not make its
/// subsequent `put` infallible.  Pageout has a stricter contract: once the
/// resident page and aliases are detached, publishing its shadow must not be
/// able to fail.  Keeping the entries in a reserved vector lets a reservation
/// prove precisely that property.
pub(super) struct FileCacheShadowStore {
    /// Least-recently used entry first.
    pub(super) entries: Vec<(CachedFileShadowKey, u64)>,
    pub(super) cap: NonZeroUsize,
    /// Publication tokens prepared before their corresponding page detach.
    pub(super) reservations: usize,
}

impl FileCacheShadowStore {
    pub(super) fn new(cap: NonZeroUsize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
            reservations: 0,
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&CachedFileShadowKey, &u64)> {
        self.entries.iter().map(|(key, age)| (key, age))
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(super) fn set_budget(&mut self, cap: NonZeroUsize) -> VfsResult<()> {
        if cap > self.cap {
            // A budget increase changes a previously full store into a
            // growing one.  Existing pageout credits must therefore gain
            // real Vec capacity before the larger logical cap is published.
            let required = self
                .entries
                .len()
                .checked_add(self.reservations)
                .ok_or(VfsError::NoMemory)?
                .min(cap.get());
            if required > self.entries.capacity() {
                self.entries
                    .try_reserve_exact(required.saturating_sub(self.entries.len()))
                    .map_err(|_| VfsError::NoMemory)?;
            }
        }
        self.cap = cap;
        while self.entries.len() > cap.get() {
            self.entries.remove(0);
        }
        Ok(())
    }

    pub(super) fn ensure_unreserved_slot(
        &mut self,
    ) -> Result<bool, alloc::collections::TryReserveError> {
        // Outstanding pageout credits own the corresponding logical slots as
        // well as their physical Vec capacity.  A best-effort eviction may
        // not displace a retained shadow merely to borrow one of those slots.
        let committed = self.entries.len().saturating_add(self.reservations);
        if committed >= self.cap.get() {
            return Ok(false);
        }
        let required = committed.saturating_add(1);
        if required > self.entries.capacity() {
            // `try_reserve_exact` is relative to len, not current capacity.
            self.entries
                .try_reserve_exact(required.saturating_sub(self.entries.len()))?;
        }
        Ok(true)
    }

    /// Reserve the storage for one new publication.  A full cache reuses its
    /// LRU entry, while a non-full cache reserves the exact forthcoming vec
    /// slot now.  Therefore `publish_reserved` cannot allocate.
    pub(super) fn reserve_publication(&mut self) -> VfsResult<()> {
        let required = self
            .entries
            .len()
            .checked_add(self.reservations)
            .and_then(|required| required.checked_add(1))
            .ok_or(VfsError::NoMemory)?
            .min(self.cap.get());
        if required > self.entries.capacity() {
            // `try_reserve_exact` guarantees `len + additional`, so request
            // the full distance from len even when spare capacity exists.
            self.entries
                .try_reserve_exact(required.saturating_sub(self.entries.len()))
                .map_err(|_| VfsError::NoMemory)?;
        }
        self.reservations = self.reservations.checked_add(1).ok_or(VfsError::NoMemory)?;
        Ok(())
    }

    pub(super) fn release_publication(&mut self) {
        self.reservations = self
            .reservations
            .checked_sub(1)
            .expect("file-cache shadow reservation underflow");
    }

    pub(super) fn pop(&mut self, key: &CachedFileShadowKey) -> Option<u64> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        Some(self.entries.remove(index).1)
    }

    pub(super) fn put_reserved(&mut self, key: CachedFileShadowKey, age: u64) {
        self.release_publication();
        self.put_prepared(key, age);
    }

    pub(super) fn put_prepared(&mut self, key: CachedFileShadowKey, age: u64) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)
        {
            self.entries.remove(index);
        } else if self.entries.len() == self.cap.get() {
            self.entries.remove(0);
        }
        debug_assert!(self.entries.len() < self.entries.capacity());
        self.entries.push((key, age));
    }
}

/// A one-shot pageout reservation.  Dropping an aborted transaction returns
/// its credit, while commit consumes it before publishing the shadow.
pub(super) struct FileCacheShadowPublicationReservation {
    pub(super) active: bool,
}

impl FileCacheShadowPublicationReservation {
    pub(super) fn commit(mut self, shared: &CachedFileShared, page: u32) {
        let mut shadows = file_cache_shadows().lock();
        advance_file_cache_nonresident_age_locked(&mut shadows);
        let age = current_file_cache_nonresident_age();
        shadows.put_reserved(
            CachedFileShadowKey {
                identity: shared.registry_key,
                page,
            },
            age,
        );
        self.active = false;
    }
}

impl Drop for FileCacheShadowPublicationReservation {
    fn drop(&mut self) {
        if self.active {
            file_cache_shadows().lock().release_publication();
        }
    }
}

pub(super) fn file_cache_shadows() -> &'static Mutex<FileCacheShadowStore> {
    FILE_CACHE_SHADOWS.call_once(|| {
        Mutex::new(FileCacheShadowStore::new(
            NonZeroUsize::new(MIN_FILE_CACHE_SHADOW_PAGES)
                .expect("nonzero global file-cache shadow budget"),
        ))
    })
}

pub(super) fn file_cache_shadow_budget() -> NonZeroUsize {
    // This is the one shared workingset domain in the absence of memcgs.
    // Contract it under allocator pressure rather than giving every inode a
    // permanent private shadow allowance.
    FILE_CACHE_MANAGED_PAGES_ONCE.call_once(|| {
        let allocator = axalloc::global_allocator();
        FILE_CACHE_MANAGED_PAGES.store(
            allocator
                .used_pages()
                .saturating_add(allocator.available_pages()),
            Ordering::Release,
        );
    });
    let managed = FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire);
    NonZeroUsize::new((managed / 8).max(MIN_FILE_CACHE_SHADOW_PAGES))
        .expect("nonzero dynamic file-cache shadow budget")
}

pub(super) fn current_file_cache_nonresident_age() -> u64 {
    FILE_CACHE_NONRESIDENT_AGE.load(Ordering::Acquire)
}

pub(super) fn advance_file_cache_nonresident_age() {
    let mut shadows = file_cache_shadows().lock();
    advance_file_cache_nonresident_age_locked(&mut shadows);
}

pub(super) fn advance_file_cache_nonresident_age_locked(shadows: &mut FileCacheShadowStore) {
    if FILE_CACHE_NONRESIDENT_AGE.load(Ordering::Acquire) == u64::MAX {
        shadows.clear();
        FILE_CACHE_NONRESIDENT_AGE.store(1, Ordering::Release);
    } else {
        FILE_CACHE_NONRESIDENT_AGE.fetch_add(1, Ordering::AcqRel);
    }
}

#[inline]
pub(super) fn file_cache_shadow_is_recent(
    age: u64,
    evicted_at: u64,
    recent_threshold: u64,
) -> bool {
    age.wrapping_sub(evicted_at) <= recent_threshold
}

pub(super) fn record_file_cache_shadow(shared: &CachedFileShared, page: u32) {
    let mut shadows = file_cache_shadows().lock();
    if shadows.set_budget(file_cache_shadow_budget()).is_err() {
        return;
    }
    // Ordinary eviction is best effort, but never relies on an infallible
    // allocator in the way a detached pageout transaction must not.
    if !matches!(shadows.ensure_unreserved_slot(), Ok(true)) {
        return;
    }
    // Allocate age and publish the LRU entry under the same domain lock.  On
    // wrap, old ages cannot be compared safely, so expire the domain and
    // restart its generation instead of manufacturing recent refaults.
    advance_file_cache_nonresident_age_locked(&mut shadows);
    let age = current_file_cache_nonresident_age();
    shadows.put_prepared(
        CachedFileShadowKey {
            identity: shared.registry_key,
            page,
        },
        age,
    );
}

/// Reserve the global shadow-store entry before a page is detached.  The
/// returned credit is consumed by commit or released by transaction rollback.
pub(super) fn prepare_file_cache_shadow_publication(
    _shared: &CachedFileShared,
) -> VfsResult<FileCacheShadowPublicationReservation> {
    let mut shadows = file_cache_shadows().lock();
    shadows.set_budget(file_cache_shadow_budget())?;
    shadows.reserve_publication()?;
    Ok(FileCacheShadowPublicationReservation { active: true })
}

pub(super) fn record_prepared_file_cache_shadow(
    reservation: FileCacheShadowPublicationReservation,
    shared: &CachedFileShared,
    page: u32,
) {
    reservation.commit(shared, page);
}

pub(super) fn consume_file_cache_shadow(shared: &CachedFileShared, page: u32) -> Option<u64> {
    file_cache_shadows().lock().pop(&CachedFileShadowKey {
        identity: shared.registry_key,
        page,
    })
}

pub(super) fn clear_file_cache_shadows<I>(shared: &CachedFileShared, pages: I)
where
    I: IntoIterator<Item = u32>,
{
    let mut shadows = file_cache_shadows().lock();
    for page in pages {
        shadows.pop(&CachedFileShadowKey {
            identity: shared.registry_key,
            page,
        });
    }
}

pub(super) fn clear_file_cache_shadow_domain(
    shared: &CachedFileShared,
    domain: &InvalidationShadowDomain,
) {
    let mut shadows = file_cache_shadows().lock();
    let keys = shadows
        .iter()
        .filter_map(|(key, _)| {
            (key.identity == shared.registry_key && domain.contains(key.page)).then_some(*key)
        })
        .collect::<Vec<_>>();
    for key in keys {
        shadows.pop(&key);
    }
}

/// Clear a pre-reserved invalidation snapshot without allocating after PTE
/// commit.  Keys are gathered before cache residency is changed.
pub(super) fn clear_file_cache_shadow_keys(keys: &[CachedFileShadowKey]) {
    let mut shadows = file_cache_shadows().lock();
    for key in keys {
        shadows.pop(key);
    }
}

pub(super) fn clear_all_file_cache_shadows(shared: &CachedFileShared) {
    clear_file_cache_shadow_domain(shared, &InvalidationShadowDomain::All);
}

pub(super) fn file_cache_resident_add(pages: usize) {
    FILE_CACHE_RESIDENT_PAGES.fetch_add(pages, Ordering::AcqRel);
}

pub(super) fn file_cache_resident_sub(pages: usize) {
    FILE_CACHE_RESIDENT_PAGES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_sub(pages)
        })
        .expect("file-cache resident accounting underflow");
}

pub(super) fn file_cache_active_add() {
    FILE_CACHE_ACTIVE_PAGES.fetch_add(1, Ordering::AcqRel);
}

pub(super) fn file_cache_active_sub(pages: usize) {
    FILE_CACHE_ACTIVE_PAGES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_sub(pages)
        })
        .expect("file-cache active accounting underflow");
}

pub(super) fn file_cache_remove_page(page: &PageCache) {
    file_cache_resident_sub(1);
    if page.is_active() {
        file_cache_active_sub(1);
    }
}

pub(super) fn file_cache_restore_page(page: &PageCache) {
    file_cache_resident_add(1);
    if page.is_active() {
        file_cache_active_add();
    }
}

pub(super) fn file_cache_record_page_reference(page: &mut PageCache) {
    if page.record_reference() {
        file_cache_active_add();
        advance_file_cache_nonresident_age();
    }
}

/// Consume a reclaim shadow while publishing a page.  A recent refault joins
/// the active list immediately; merely dropping the shadow loses the age
/// information and makes workingset refaults indistinguishable from cold
/// first faults.
pub(super) fn file_cache_apply_refault(
    shared: &CachedFileShared,
    page_no: u32,
    page: &mut PageCache,
) {
    let Some(evicted_at) = consume_file_cache_shadow(shared, page_no) else {
        return;
    };
    let age = current_file_cache_nonresident_age();
    let recent_threshold =
        u64::try_from(FILE_CACHE_ACTIVE_PAGES.load(Ordering::Acquire)).unwrap_or(u64::MAX);
    if file_cache_shadow_is_recent(age, evicted_at, recent_threshold) && !page.is_active() {
        page.activate_refault();
        file_cache_active_add();
    }
}

pub(super) fn file_cache_reclaim_cursor() -> &'static Mutex<Option<CachedFileRegistryKey>> {
    FILE_CACHE_RECLAIM_CURSOR.call_once(|| Mutex::new(None))
}

pub(super) fn file_cache_estimate_cursor() -> &'static Mutex<Option<CachedFileRegistryKey>> {
    FILE_CACHE_ESTIMATE_CURSOR.call_once(|| Mutex::new(None))
}

pub(super) fn remove_released_cached_file_registry_entry(
    key: CachedFileRegistryKey,
    shared: *const CachedFileShared,
) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_released_shared = registry.get(&key).is_some_and(|entry| {
            entry.retained.is_none()
                && entry.writeback_anchor.is_none()
                && core::ptr::eq(entry.shared.as_ptr(), shared)
        });
        if matches_released_shared {
            registry.remove(&key)
        } else {
            None
        }
    };
    // Weak state and writeback ownership can ultimately release filesystem or
    // inode objects. Keep every such destructor outside the registry lock.
    drop(retired);
}

pub(super) fn cached_file_registry_key(location: &Location) -> CachedFileRegistryKey {
    cached_file_user_data(location).identity()
}

pub(super) fn cached_file_user_data(location: &Location) -> Arc<FileUserData> {
    let mut data = location.user_data();
    data.get_or_insert_with(|| FileUserData::new_identity(location))
}

pub(super) fn cached_file_is_in_memory(location: &Location) -> bool {
    location.flags().contains(NodeFlags::ALWAYS_CACHE) || location.filesystem().name() == "tmpfs"
}

pub(super) fn cached_file_shared_for_location(
    location: &Location,
) -> Option<Arc<CachedFileShared>> {
    let user_data = location.user_data().get::<FileUserData>()?;
    let key = user_data.identity();
    let registry_shared = {
        let registry = file_cache_registry().lock();
        registry.get(&key).and_then(FileUserData::shared)
    };
    registry_shared.or_else(|| user_data.shared())
}

pub(super) fn cached_file_shared_for_location_or_create(
    location: &Location,
) -> Arc<CachedFileShared> {
    // `TypeMap` is the inode-generation synchronization point.  It is shared
    // by hard-link aliases on backends that expose persistent inode data, and
    // falls back to the exact dentry for backends without that capability.
    // Installing the identity before taking the global registry lock makes
    // concurrent first opens observe the same lease rather than allocating
    // two identities for one inode generation.
    let user_data = cached_file_user_data(location);
    let key = user_data.identity();
    let identity_lease = user_data.identity_lease.clone();
    let (shared, retired_entry, released_retained, install_user_data) = 'registry: {
        let mut registry = file_cache_registry().lock();
        let mut released_retained = None;

        if let Some(entry) = registry.get_mut(&key) {
            let shared = entry.shared();
            entry.update_location(location);
            released_retained = entry.release_retained();
            if let Some(shared) = shared {
                break 'registry (shared, None, released_retained, false);
            }
        }

        // A cache shared state can outlive its registry slot while a caller
        // still owns the per-inode user-data attachment.  Restore that exact
        // state instead of manufacturing a second cache for the generation.
        if let Some(shared) = user_data.shared() {
            let retired_entry = registry.insert(key, FileUserData::new(location, &shared));
            break 'registry (shared, retired_entry, released_retained, false);
        }

        let shared = Arc::new(CachedFileShared::with_identity(
            key,
            identity_lease,
            cached_file_is_in_memory(location),
        ));
        let retired_entry = registry.insert(key, FileUserData::new(location, &shared));
        (shared, retired_entry, released_retained, true)
    };

    // Cached ownership may release filesystem or inode state. Never run those
    // destructors while the registry is locked: teardown can re-enter here.
    if released_retained.is_some() {
        record_cached_file_counter(&CLOSED_FILE_CACHE_REOPEN_HITS, 1);
    }
    drop(retired_entry);
    drop(released_retained);
    if install_user_data {
        location
            .user_data()
            .insert(FileUserData::new(location, &shared));
    }
    shared
}

pub(super) fn retain_cached_file_writeback_anchor_if_dirty(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) {
    let page_cache = shared.page_cache.lock();
    if !page_cache.iter().any(|(_pn, page)| page.is_dirty()) {
        return;
    }

    let key = cached_file_registry_key(location);
    let mut candidate_anchor = Some(location.writeback_anchor());
    let retired_entry = {
        let mut registry = file_cache_registry().lock();
        let entry = registry
            .entry(key)
            .or_insert_with(|| FileUserData::new(location, shared));
        let retired = if !entry
            .shared()
            .is_some_and(|registered| Arc::ptr_eq(&registered, shared))
        {
            Some(core::mem::replace(
                entry,
                FileUserData::new(location, shared),
            ))
        } else {
            None
        };
        entry.update_location(location);
        if entry.writeback_anchor.is_none() {
            entry.writeback_anchor = candidate_anchor.take();
        }
        retired
    };
    // Filesystem and inode teardown may call back into cache bookkeeping.
    // Drop replaced ownership only after releasing both bookkeeping locks.
    drop(page_cache);
    drop(retired_entry);
    drop(candidate_anchor);
}
