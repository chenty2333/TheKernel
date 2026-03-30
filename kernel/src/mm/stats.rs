use axalloc::{UsageKind, global_allocator};
use axhal::mem::total_ram_size;
use memory_addr::PAGE_SIZE_4K;

/// Snapshot of system-wide memory statistics backed by the page allocator.
#[derive(Debug, Clone, Copy)]
pub struct SystemMemoryStats {
    pub total_bytes: usize,
    pub free_bytes: usize,
    pub available_bytes: usize,
    pub used_bytes: usize,
    pub cached_bytes: usize,
    pub mapped_bytes: usize,
    pub page_table_bytes: usize,
}

/// Returns the best-effort system memory statistics used by procfs/sysinfo.
pub fn system_memory_stats() -> SystemMemoryStats {
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

    SystemMemoryStats {
        total_bytes,
        free_bytes,
        available_bytes: free_bytes,
        used_bytes: total_bytes.saturating_sub(free_bytes),
        cached_bytes: usages.get(UsageKind::PageCache),
        mapped_bytes: usages.get(UsageKind::VirtMem),
        page_table_bytes: usages.get(UsageKind::PageTable),
    }
}
