use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{InodeRef, SystemHal};

const HOT_INODE_CACHE_CAPACITY: usize = 16;
const EXTENT_STATUS_CACHE_CAPACITY: usize = 64;

pub(crate) static ENABLE_HOT_INODE_CACHE: AtomicBool = AtomicBool::new(true);
pub(crate) static ENABLE_EXTENT_STATUS_CACHE: AtomicBool = AtomicBool::new(true);
pub(crate) static ENABLE_IO_COUNTERS: AtomicBool = AtomicBool::new(false);
pub(crate) static ENABLE_ASYNC_MAPPED_READ: AtomicBool = AtomicBool::new(false);

macro_rules! counter_set {
    ($($name:ident),+ $(,)?) => {
        $(
            static $name: AtomicU64 = AtomicU64::new(0);
        )+

        pub fn reset_io_counters() {
            $(
                $name.store(0, Ordering::Relaxed);
            )+
        }
    };
}

counter_set! {
    HOT_INODE_HITS,
    HOT_INODE_MISSES,
    HOT_INODE_EVICTIONS,
    HOT_INODE_DRAINS,
    INODE_REF_GETS,
    EXTENT_GET_BLOCKS_CALLS,
    EXTENT_GET_BLOCKS_REQUESTED,
    EXTENT_GET_BLOCKS_RETURNED,
    EXTENT_GET_BLOCKS_CREATE_CALLS,
    LEGACY_DBLK_LOOKUPS,
    EXTENT_STATUS_HITS,
    EXTENT_STATUS_MISSES,
    EXTENT_STATUS_INSERTS,
    EXTENT_STATUS_INVALIDATIONS,
    EXTENT_STATUS_RECLAIMS,
    MAPPED_READ_RUNS,
    MAPPED_READ_BYTES,
    MAPPED_OVERWRITE_HITS,
    MAPPED_OVERWRITE_MISSES,
    MAPPED_OVERWRITE_BYTES,
    MAPPED_READ_VECTORED_RUNS,
    MAPPED_READ_VECTORED_BYTES,
    MAPPED_OVERWRITE_VECTORED_HITS,
    MAPPED_OVERWRITE_VECTORED_BYTES,
    ASYNC_MAPPED_READ_HITS,
    ASYNC_MAPPED_READ_RUNS,
    ASYNC_MAPPED_READ_BYTES,
    ASYNC_MAPPED_READ_SUBMIT_BATCHES,
    ASYNC_MAPPED_READ_FALLBACKS,
    ASYNC_MAPPED_READ_COOKIE_REJECTS,
    READAHEAD_ASYNC_PAGES,
    READAHEAD_ASYNC_HITS,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IoCounters {
    pub hot_inode_hits: u64,
    pub hot_inode_misses: u64,
    pub hot_inode_evictions: u64,
    pub hot_inode_drains: u64,
    pub inode_ref_gets: u64,
    pub extent_get_blocks_calls: u64,
    pub extent_get_blocks_requested: u64,
    pub extent_get_blocks_returned: u64,
    pub extent_get_blocks_create_calls: u64,
    pub legacy_dblk_lookups: u64,
    pub extent_status_hits: u64,
    pub extent_status_misses: u64,
    pub extent_status_inserts: u64,
    pub extent_status_invalidations: u64,
    pub extent_status_reclaims: u64,
    pub mapped_read_runs: u64,
    pub mapped_read_bytes: u64,
    pub mapped_overwrite_hits: u64,
    pub mapped_overwrite_misses: u64,
    pub mapped_overwrite_bytes: u64,
    pub mapped_read_vectored_runs: u64,
    pub mapped_read_vectored_bytes: u64,
    pub mapped_overwrite_vectored_hits: u64,
    pub mapped_overwrite_vectored_bytes: u64,
    pub async_mapped_read_enabled: u64,
    pub async_mapped_read_hits: u64,
    pub async_mapped_read_runs: u64,
    pub async_mapped_read_bytes: u64,
    pub async_mapped_read_submit_batches: u64,
    pub async_mapped_read_fallbacks: u64,
    pub async_mapped_read_cookie_rejects: u64,
    pub readahead_async_pages: u64,
    pub readahead_async_hits: u64,
}

pub fn set_io_counters_enabled(enabled: bool) {
    ENABLE_IO_COUNTERS.store(enabled, Ordering::Relaxed);
}

pub fn set_extent_status_cache_enabled(enabled: bool) {
    ENABLE_EXTENT_STATUS_CACHE.store(enabled, Ordering::Relaxed);
}

pub fn set_async_mapped_read_enabled(enabled: bool) {
    ENABLE_ASYNC_MAPPED_READ.store(enabled, Ordering::Relaxed);
}

#[inline(always)]
pub fn async_mapped_read_enabled() -> bool {
    ENABLE_ASYNC_MAPPED_READ.load(Ordering::Relaxed)
}

pub fn io_counters_snapshot() -> IoCounters {
    IoCounters {
        hot_inode_hits: HOT_INODE_HITS.load(Ordering::Relaxed),
        hot_inode_misses: HOT_INODE_MISSES.load(Ordering::Relaxed),
        hot_inode_evictions: HOT_INODE_EVICTIONS.load(Ordering::Relaxed),
        hot_inode_drains: HOT_INODE_DRAINS.load(Ordering::Relaxed),
        inode_ref_gets: INODE_REF_GETS.load(Ordering::Relaxed),
        extent_get_blocks_calls: EXTENT_GET_BLOCKS_CALLS.load(Ordering::Relaxed),
        extent_get_blocks_requested: EXTENT_GET_BLOCKS_REQUESTED.load(Ordering::Relaxed),
        extent_get_blocks_returned: EXTENT_GET_BLOCKS_RETURNED.load(Ordering::Relaxed),
        extent_get_blocks_create_calls: EXTENT_GET_BLOCKS_CREATE_CALLS.load(Ordering::Relaxed),
        legacy_dblk_lookups: LEGACY_DBLK_LOOKUPS.load(Ordering::Relaxed),
        extent_status_hits: EXTENT_STATUS_HITS.load(Ordering::Relaxed),
        extent_status_misses: EXTENT_STATUS_MISSES.load(Ordering::Relaxed),
        extent_status_inserts: EXTENT_STATUS_INSERTS.load(Ordering::Relaxed),
        extent_status_invalidations: EXTENT_STATUS_INVALIDATIONS.load(Ordering::Relaxed),
        extent_status_reclaims: EXTENT_STATUS_RECLAIMS.load(Ordering::Relaxed),
        mapped_read_runs: MAPPED_READ_RUNS.load(Ordering::Relaxed),
        mapped_read_bytes: MAPPED_READ_BYTES.load(Ordering::Relaxed),
        mapped_overwrite_hits: MAPPED_OVERWRITE_HITS.load(Ordering::Relaxed),
        mapped_overwrite_misses: MAPPED_OVERWRITE_MISSES.load(Ordering::Relaxed),
        mapped_overwrite_bytes: MAPPED_OVERWRITE_BYTES.load(Ordering::Relaxed),
        mapped_read_vectored_runs: MAPPED_READ_VECTORED_RUNS.load(Ordering::Relaxed),
        mapped_read_vectored_bytes: MAPPED_READ_VECTORED_BYTES.load(Ordering::Relaxed),
        mapped_overwrite_vectored_hits: MAPPED_OVERWRITE_VECTORED_HITS.load(Ordering::Relaxed),
        mapped_overwrite_vectored_bytes: MAPPED_OVERWRITE_VECTORED_BYTES.load(Ordering::Relaxed),
        async_mapped_read_enabled: ENABLE_ASYNC_MAPPED_READ.load(Ordering::Relaxed) as u64,
        async_mapped_read_hits: ASYNC_MAPPED_READ_HITS.load(Ordering::Relaxed),
        async_mapped_read_runs: ASYNC_MAPPED_READ_RUNS.load(Ordering::Relaxed),
        async_mapped_read_bytes: ASYNC_MAPPED_READ_BYTES.load(Ordering::Relaxed),
        async_mapped_read_submit_batches: ASYNC_MAPPED_READ_SUBMIT_BATCHES.load(Ordering::Relaxed),
        async_mapped_read_fallbacks: ASYNC_MAPPED_READ_FALLBACKS.load(Ordering::Relaxed),
        async_mapped_read_cookie_rejects: ASYNC_MAPPED_READ_COOKIE_REJECTS.load(Ordering::Relaxed),
        readahead_async_pages: READAHEAD_ASYNC_PAGES.load(Ordering::Relaxed),
        readahead_async_hits: READAHEAD_ASYNC_HITS.load(Ordering::Relaxed),
    }
}

#[inline(always)]
fn counters_enabled() -> bool {
    ENABLE_IO_COUNTERS.load(Ordering::Relaxed)
}

#[inline(always)]
fn inc(counter: &AtomicU64, value: u64) {
    if counters_enabled() {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

pub(crate) fn record_hot_inode_hit() {
    inc(&HOT_INODE_HITS, 1);
}

pub(crate) fn record_hot_inode_miss() {
    inc(&HOT_INODE_MISSES, 1);
}

pub(crate) fn record_hot_inode_eviction() {
    inc(&HOT_INODE_EVICTIONS, 1);
}

pub(crate) fn record_hot_inode_drain(count: usize) {
    inc(&HOT_INODE_DRAINS, count as u64);
}

pub(crate) fn record_inode_ref_get() {
    inc(&INODE_REF_GETS, 1);
}

pub(crate) fn record_extent_get_blocks(requested: u32, returned: u32, create: bool) {
    if counters_enabled() {
        EXTENT_GET_BLOCKS_CALLS.fetch_add(1, Ordering::Relaxed);
        EXTENT_GET_BLOCKS_REQUESTED.fetch_add(requested as u64, Ordering::Relaxed);
        EXTENT_GET_BLOCKS_RETURNED.fetch_add(returned as u64, Ordering::Relaxed);
        if create {
            EXTENT_GET_BLOCKS_CREATE_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) fn record_legacy_dblk_lookup() {
    inc(&LEGACY_DBLK_LOOKUPS, 1);
}

pub(crate) fn record_mapped_read(runs: usize, bytes: usize) {
    if counters_enabled() {
        MAPPED_READ_RUNS.fetch_add(runs as u64, Ordering::Relaxed);
        MAPPED_READ_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_mapped_read_vectored(runs: usize, bytes: usize) {
    if counters_enabled() {
        MAPPED_READ_VECTORED_RUNS.fetch_add(runs as u64, Ordering::Relaxed);
        MAPPED_READ_VECTORED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_async_mapped_read(runs: usize, bytes: usize, submit_batches: usize) {
    if counters_enabled() {
        ASYNC_MAPPED_READ_HITS.fetch_add(1, Ordering::Relaxed);
        ASYNC_MAPPED_READ_RUNS.fetch_add(runs as u64, Ordering::Relaxed);
        ASYNC_MAPPED_READ_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        ASYNC_MAPPED_READ_SUBMIT_BATCHES.fetch_add(submit_batches as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_async_mapped_read_fallback() {
    inc(&ASYNC_MAPPED_READ_FALLBACKS, 1);
}

pub(crate) fn record_async_mapped_read_cookie_reject() {
    inc(&ASYNC_MAPPED_READ_COOKIE_REJECTS, 1);
}

pub fn record_readahead_async_pages(pages: usize) {
    if counters_enabled() && async_mapped_read_enabled() && pages > 0 {
        READAHEAD_ASYNC_HITS.fetch_add(1, Ordering::Relaxed);
        READAHEAD_ASYNC_PAGES.fetch_add(pages as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_mapped_overwrite_hit(bytes: usize) {
    if counters_enabled() {
        MAPPED_OVERWRITE_HITS.fetch_add(1, Ordering::Relaxed);
        MAPPED_OVERWRITE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_mapped_overwrite_vectored_hit(bytes: usize) {
    if counters_enabled() {
        MAPPED_OVERWRITE_VECTORED_HITS.fetch_add(1, Ordering::Relaxed);
        MAPPED_OVERWRITE_VECTORED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_mapped_overwrite_miss() {
    inc(&MAPPED_OVERWRITE_MISSES, 1);
}

fn record_extent_status_hit() {
    inc(&EXTENT_STATUS_HITS, 1);
}

fn record_extent_status_miss() {
    inc(&EXTENT_STATUS_MISSES, 1);
}

fn record_extent_status_insert() {
    inc(&EXTENT_STATUS_INSERTS, 1);
}

fn record_extent_status_invalidation(count: usize) {
    inc(&EXTENT_STATUS_INVALIDATIONS, count as u64);
}

fn record_extent_status_reclaim() {
    inc(&EXTENT_STATUS_RECLAIMS, 1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentStatusKind {
    Written,
    Hole,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtentStatusRun {
    pub pblock: u64,
    pub blocks: u32,
    pub kind: ExtentStatusKind,
}

#[derive(Clone, Copy, Debug)]
struct ExtentStatusEntry {
    lblock: u32,
    pblock: u64,
    blocks: u32,
    kind: ExtentStatusKind,
    seq: u64,
}

pub(crate) struct ExtentStatusCache {
    entries: Vec<ExtentStatusEntry>,
    seq: u64,
    capacity: usize,
}

impl ExtentStatusCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            seq: 0,
            capacity: EXTENT_STATUS_CACHE_CAPACITY,
        }
    }

    pub(crate) fn lookup(&mut self, lblock: u32, max_blocks: u32) -> Option<ExtentStatusRun> {
        if max_blocks == 0 {
            record_extent_status_miss();
            return None;
        }

        let Some(idx) = self.entries.iter().position(|entry| {
            entry.seq == self.seq
                && lblock >= entry.lblock
                && lblock < entry.lblock.saturating_add(entry.blocks)
        }) else {
            record_extent_status_miss();
            return None;
        };

        let entry = self.entries.remove(idx);
        let delta = lblock - entry.lblock;
        let blocks = max_blocks.min(entry.blocks - delta);
        let pblock = match entry.kind {
            ExtentStatusKind::Written => entry.pblock + delta as u64,
            ExtentStatusKind::Hole => 0,
        };
        self.entries.insert(0, entry);
        record_extent_status_hit();
        Some(ExtentStatusRun {
            pblock,
            blocks,
            kind: entry.kind,
        })
    }

    pub(crate) fn insert(&mut self, lblock: u32, pblock: u64, blocks: u32, kind: ExtentStatusKind) {
        if blocks == 0 || self.capacity == 0 {
            return;
        }
        self.invalidate_range(lblock, blocks);
        self.entries.insert(
            0,
            ExtentStatusEntry {
                lblock,
                pblock,
                blocks,
                kind,
                seq: self.seq,
            },
        );
        record_extent_status_insert();
        if self.entries.len() > self.capacity {
            self.entries.pop();
            record_extent_status_reclaim();
        }
    }

    pub(crate) fn invalidate_range(&mut self, start: u32, blocks: u32) {
        if blocks == 0 {
            return;
        }
        let end = start.saturating_add(blocks);
        let old_len = self.entries.len();
        self.entries.retain(|entry| {
            let entry_end = entry.lblock.saturating_add(entry.blocks);
            entry_end <= start || entry.lblock >= end
        });
        let removed = old_len - self.entries.len();
        if removed > 0 {
            record_extent_status_invalidation(removed);
            self.seq = self.seq.wrapping_add(1);
        }
    }

    pub(crate) fn invalidate_from(&mut self, start: u32) {
        let old_len = self.entries.len();
        self.entries
            .retain(|entry| entry.lblock.saturating_add(entry.blocks) <= start);
        let removed = old_len - self.entries.len();
        if removed > 0 {
            record_extent_status_invalidation(removed);
            self.seq = self.seq.wrapping_add(1);
        }
    }

    pub(crate) fn clear(&mut self) {
        let count = self.entries.len();
        self.entries.clear();
        if count > 0 {
            record_extent_status_invalidation(count);
            self.seq = self.seq.wrapping_add(1);
        }
    }
}

struct HotInodeEntry<Hal: SystemHal> {
    ino: u32,
    inode: InodeRef<Hal>,
}

pub(crate) struct HotInodeCache<Hal: SystemHal> {
    entries: Vec<HotInodeEntry<Hal>>,
    capacity: usize,
}

impl<Hal: SystemHal> HotInodeCache<Hal> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            capacity: HOT_INODE_CACHE_CAPACITY,
        }
    }

    pub(crate) fn take(&mut self, ino: u32) -> Option<InodeRef<Hal>> {
        let idx = self.entries.iter().position(|entry| entry.ino == ino)?;
        Some(self.entries.remove(idx).inode)
    }

    pub(crate) fn put(&mut self, ino: u32, inode: InodeRef<Hal>) {
        if self.capacity == 0 {
            drop(inode);
            record_hot_inode_eviction();
            return;
        }
        if let Some(idx) = self.entries.iter().position(|entry| entry.ino == ino) {
            let old = self.entries.remove(idx);
            drop(old);
        }
        self.entries.insert(0, HotInodeEntry { ino, inode });
        if self.entries.len() > self.capacity {
            let old = self.entries.pop();
            drop(old);
            record_hot_inode_eviction();
        }
    }

    pub(crate) fn invalidate(&mut self, ino: u32) {
        if let Some(idx) = self.entries.iter().position(|entry| entry.ino == ino) {
            let old = self.entries.remove(idx);
            drop(old);
            record_hot_inode_drain(1);
        }
    }

    pub(crate) fn drain_all(&mut self) {
        let count = self.entries.len();
        self.entries.clear();
        record_hot_inode_drain(count);
    }
}
