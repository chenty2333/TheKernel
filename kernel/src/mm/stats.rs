use core::sync::atomic::{AtomicU32, Ordering};

use axalloc::{UsageKind, global_allocator};
use axerrno::{AxError, AxResult};
use axhal::mem::total_ram_size;
use memory_addr::PAGE_SIZE_4K;

const OVERCOMMIT_MEMORY_DEFAULT: u32 = 0;
const OVERCOMMIT_RATIO_DEFAULT: u32 = 50;

static OVERCOMMIT_MEMORY: AtomicU32 = AtomicU32::new(OVERCOMMIT_MEMORY_DEFAULT);
static OVERCOMMIT_RATIO: AtomicU32 = AtomicU32::new(OVERCOMMIT_RATIO_DEFAULT);

/// Snapshot of system-wide memory statistics backed by the page allocator.
#[derive(Debug, Clone, Copy)]
pub struct SystemMemoryStats {
    pub total_bytes: usize,
    pub free_bytes: usize,
    pub available_bytes: usize,
    pub reclaimable_file_cache_bytes: usize,
    pub low_watermark_bytes: usize,
    pub used_bytes: usize,
    pub cached_bytes: usize,
    pub page_table_bytes: usize,
}

fn system_memory_stats_with_reclaimable_pages(
    reclaimable_file_cache_pages: usize,
) -> SystemMemoryStats {
    let alloc = global_allocator();
    let used_pages = alloc.used_pages();
    let free_pages = alloc.available_pages();
    let managed_total_bytes = used_pages
        .saturating_add(free_pages)
        .saturating_mul(PAGE_SIZE_4K);

    let total_bytes = if managed_total_bytes != 0 {
        managed_total_bytes
    } else {
        total_ram_size()
    };
    let free_bytes = free_pages.saturating_mul(PAGE_SIZE_4K).min(total_bytes);
    let usages = alloc.usages();
    let total_pages = total_bytes / PAGE_SIZE_4K;
    let reclaimable_pages =
        reclaimable_file_cache_pages.min(total_pages.saturating_sub(free_pages));
    let available_pages = super::available_memory_pages(total_pages, free_pages, reclaimable_pages);
    let low_watermark_pages = super::memory_watermarks(total_pages).low_pages;

    SystemMemoryStats {
        total_bytes,
        free_bytes,
        available_bytes: available_pages.saturating_mul(PAGE_SIZE_4K),
        reclaimable_file_cache_bytes: reclaimable_pages.saturating_mul(PAGE_SIZE_4K),
        low_watermark_bytes: low_watermark_pages.saturating_mul(PAGE_SIZE_4K),
        used_bytes: total_bytes.saturating_sub(free_bytes),
        cached_bytes: usages.get(UsageKind::PageCache),
        page_table_bytes: usages.get(UsageKind::PageTable),
    }
}

/// Returns the cheap allocator-backed memory snapshot used by syscall and
/// allocation-policy paths. It deliberately performs no global file-cache
/// scan; the resulting availability estimate is conservative.
pub fn system_memory_stats() -> SystemMemoryStats {
    system_memory_stats_with_reclaimable_pages(0)
}

/// Returns a reclaim-aware snapshot for explicit procfs/diagnostic reads.
/// The caller owns the bounded cache scan so hot syscall paths cannot acquire
/// per-inode locks as a side effect of reading allocator state.
pub(crate) fn system_memory_stats_with_reclaimable_file_cache(
    reclaimable_file_cache_pages: usize,
) -> SystemMemoryStats {
    system_memory_stats_with_reclaimable_pages(reclaimable_file_cache_pages)
}

pub fn overcommit_memory_policy() -> u32 {
    OVERCOMMIT_MEMORY.load(Ordering::Relaxed)
}

pub fn set_overcommit_memory_policy(value: u32) -> AxResult<()> {
    if value > 2 {
        return Err(AxError::InvalidInput);
    }
    OVERCOMMIT_MEMORY.store(value, Ordering::Relaxed);
    Ok(())
}

pub fn overcommit_ratio() -> u32 {
    OVERCOMMIT_RATIO.load(Ordering::Relaxed)
}

pub fn set_overcommit_ratio(value: u32) {
    OVERCOMMIT_RATIO.store(value, Ordering::Relaxed);
}

pub fn commit_limit_bytes() -> usize {
    let stats = system_memory_stats();
    ((stats.total_bytes as u128 * overcommit_ratio() as u128) / 100).min(usize::MAX as u128)
        as usize
}

pub fn committed_as_bytes() -> usize {
    system_memory_stats().used_bytes
}

pub fn check_memory_overcommit(bytes: usize) -> AxResult<()> {
    if bytes == 0 {
        return Ok(());
    }

    let stats = system_memory_stats();
    match overcommit_memory_policy() {
        1 => Ok(()),
        2 => {
            let available_commit = commit_limit_bytes().saturating_sub(committed_as_bytes());
            if bytes > available_commit {
                Err(AxError::NoMemory)
            } else {
                Ok(())
            }
        }
        _ => {
            if bytes > stats.total_bytes {
                Err(AxError::NoMemory)
            } else {
                Ok(())
            }
        }
    }
}
