//! Linux 7.2.3 `/proc/meminfo` (fs/proc/meminfo.c) and `/proc/vmstat`
//! (mm/vmstat.c) renderers. Every field is printed in Linux order. Counters
//! TheKernel tracks are read live. Counters it does not model are printed as
//! 0, as Linux does for an absent config option.

use alloc::{string::String, sync::Arc};
use core::fmt::Write;

use axalloc::UsageKind;
use memory_addr::PAGE_SIZE_4K;

use super::{DirMapping, SimpleFile, SimpleFs};

/// Linux x86_64 `VMALLOC_TOTAL` in kB, for the 4-level layout.
const VMALLOC_TOTAL_KB: u64 = 34_359_738_367;

/// Values behind `/proc/meminfo`, all in kB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MeminfoValues {
    total_kb: u64,
    free_kb: u64,
    available_kb: u64,
    cached_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
    dirty_kb: u64,
    writeback_kb: u64,
    anon_kb: u64,
    shmem_kb: u64,
    slab_kb: u64,
    page_tables_kb: u64,
    commit_limit_kb: u64,
    committed_as_kb: u64,
}

/// Linux `show_val_kb()`: a fixed label, then the value right-aligned to
/// width 8, then " kB".
fn push_kb(out: &mut String, label: &str, kb: u64) {
    let _ = writeln!(out, "{label}{kb:>8} kB");
}

fn render_meminfo(v: &MeminfoValues) -> String {
    let mut out = String::new();
    push_kb(&mut out, "MemTotal:       ", v.total_kb);
    push_kb(&mut out, "MemFree:        ", v.free_kb);
    push_kb(&mut out, "MemAvailable:   ", v.available_kb);
    // The page cache has no buffer-head layer; Buffers and SwapCached have no
    // separate population here.
    push_kb(&mut out, "Buffers:        ", 0);
    push_kb(&mut out, "Cached:         ", v.cached_kb);
    push_kb(&mut out, "SwapCached:     ", 0);
    // No LRU lists are modelled, so the Active/Inactive split is zero.
    push_kb(&mut out, "Active:         ", 0);
    push_kb(&mut out, "Inactive:       ", 0);
    push_kb(&mut out, "Active(anon):   ", 0);
    push_kb(&mut out, "Inactive(anon): ", 0);
    push_kb(&mut out, "Active(file):   ", 0);
    push_kb(&mut out, "Inactive(file): ", 0);
    push_kb(&mut out, "Unevictable:    ", 0);
    push_kb(&mut out, "Mlocked:        ", 0);
    push_kb(&mut out, "SwapTotal:      ", v.swap_total_kb);
    push_kb(&mut out, "SwapFree:       ", v.swap_free_kb);
    push_kb(&mut out, "Dirty:          ", v.dirty_kb);
    push_kb(&mut out, "Writeback:      ", v.writeback_kb);
    push_kb(&mut out, "AnonPages:      ", v.anon_kb);
    push_kb(&mut out, "Mapped:         ", 0);
    push_kb(&mut out, "Shmem:          ", v.shmem_kb);
    push_kb(&mut out, "KReclaimable:   ", 0);
    push_kb(&mut out, "Slab:           ", v.slab_kb);
    push_kb(&mut out, "SReclaimable:   ", 0);
    push_kb(&mut out, "SUnreclaim:     ", v.slab_kb);
    let _ = writeln!(out, "KernelStack:    {:>8} kB", 0);
    push_kb(&mut out, "PageTables:     ", v.page_tables_kb);
    push_kb(&mut out, "SecPageTables:  ", 0);
    push_kb(&mut out, "NFS_Unstable:   ", 0);
    push_kb(&mut out, "Bounce:         ", 0);
    push_kb(&mut out, "WritebackTmp:   ", 0);
    push_kb(&mut out, "CommitLimit:    ", v.commit_limit_kb);
    push_kb(&mut out, "Committed_AS:   ", v.committed_as_kb);
    let _ = writeln!(out, "VmallocTotal:   {VMALLOC_TOTAL_KB:>8} kB");
    push_kb(&mut out, "VmallocUsed:    ", 0);
    push_kb(&mut out, "VmallocChunk:   ", 0);
    push_kb(&mut out, "Percpu:         ", 0);
    let _ = writeln!(out, "HardwareCorrupted: {:>5} kB", 0);
    push_kb(&mut out, "AnonHugePages:  ", 0);
    push_kb(&mut out, "ShmemHugePages: ", 0);
    push_kb(&mut out, "ShmemPmdMapped: ", 0);
    push_kb(&mut out, "FileHugePages:  ", 0);
    push_kb(&mut out, "FilePmdMapped:  ", 0);
    push_kb(&mut out, "Balloon:        ", 0);
    push_kb(&mut out, "GPUActive:      ", 0);
    push_kb(&mut out, "GPUReclaim:     ", 0);
    // Linux hugetlb_report_meminfo(), between GPUReclaim and the DirectMap rows.
    out.push_str(&super::hugetlb::hugetlb_meminfo_lines());
    // Linux arch_report_meminfo(): the direct map is not modelled.
    let _ = writeln!(out, "DirectMap4k:    {:>8} kB", 0);
    let _ = writeln!(out, "DirectMap2M:    {:>8} kB", 0);
    out
}

fn bytes_to_kb(bytes: usize) -> u64 {
    (bytes / 1024) as u64
}

fn pages_to_kb(pages: usize) -> u64 {
    (pages * PAGE_SIZE_4K / 1024) as u64
}

/// Renders `/proc/meminfo` from the live allocator, page-cache, shmem,
/// pressure, and swap counters.
pub(super) fn meminfo_text() -> String {
    let reclaim = axfs::cached_file_reclaim_estimate();
    let stats = crate::mm::system_memory_stats_with_reclaimable_file_cache(
        reclaim.reclaimable_pages,
    );
    let usage = axalloc::global_allocator().usages();
    let swap = crate::mm::swap_usage();
    let shmem_pages = crate::mm::shmem_resident_pages() + axfs::in_memory_page_cache_pages();
    let values = MeminfoValues {
        total_kb: bytes_to_kb(stats.total_bytes),
        free_kb: bytes_to_kb(stats.free_bytes),
        available_kb: bytes_to_kb(stats.available_bytes),
        cached_kb: bytes_to_kb(stats.cached_bytes),
        swap_total_kb: bytes_to_kb(swap.total_bytes),
        swap_free_kb: bytes_to_kb(swap.free_bytes),
        dirty_kb: pages_to_kb(axfs::dirty_file_page_cache_pages()),
        writeback_kb: pages_to_kb(axfs::writeback_page_cache_pages()),
        anon_kb: bytes_to_kb(usage.get(UsageKind::VirtMem)),
        shmem_kb: pages_to_kb(shmem_pages),
        // Kernel heap bytes are the unreclaimable slab-class usage.
        slab_kb: bytes_to_kb(usage.get(UsageKind::RustHeap)),
        page_tables_kb: bytes_to_kb(stats.page_table_bytes),
        commit_limit_kb: bytes_to_kb(crate::mm::commit_limit_bytes()),
        committed_as_kb: bytes_to_kb(crate::mm::committed_as_bytes()),
    };
    render_meminfo(&values)
}

#[derive(Default)]
struct VmSnapshot {
    free_pages: usize,
    anon_pages: usize,
    file_pages: usize,
    dirty_pages: usize,
    writeback_pages: usize,
    shmem_pages: usize,
    page_table_pages: usize,
    slab_unreclaimable_bytes: usize,
    faults: u64,
    major_faults: u64,
    swap_reads: u64,
    swap_writes: u64,
    background_scanned: u64,
    background_reclaimed: u64,
}

fn snapshot() -> VmSnapshot {
    let allocator = axalloc::global_allocator();
    let usage = allocator.usages();
    let pressure = crate::mm::memory_pressure_snapshot();
    let (faults, major_faults) = crate::mm::vm_events::snapshot();
    let (swap_reads, swap_writes) = crate::mm::swap_io_events::snapshot();
    VmSnapshot {
        free_pages: allocator.available_pages(),
        anon_pages: usage.get(UsageKind::VirtMem) / PAGE_SIZE_4K,
        file_pages: usage.get(UsageKind::PageCache) / PAGE_SIZE_4K,
        dirty_pages: axfs::dirty_file_page_cache_pages(),
        writeback_pages: axfs::writeback_page_cache_pages(),
        shmem_pages: crate::mm::shmem_resident_pages() + axfs::in_memory_page_cache_pages(),
        page_table_pages: usage.get(UsageKind::PageTable) / PAGE_SIZE_4K,
        slab_unreclaimable_bytes: usage.get(UsageKind::RustHeap),
        faults,
        major_faults,
        swap_reads,
        swap_writes,
        background_scanned: pressure.scanned_pages,
        background_reclaimed: pressure.reclaimed_pages,
    }
}

/// Linux `vmstat_text[]` order. Counters this kernel does not model print 0.
/// The zone-expanded `pgalloc_*`/`allocstall_*` rows, NUMA rows and the
/// compaction-free items are not emitted because this kernel has one zone.
fn render_vmstat(vm: &VmSnapshot) -> String {
    let free = vm.free_pages as u64;
    let anon = vm.anon_pages as u64;
    let file = vm.file_pages as u64;
    let dirty = vm.dirty_pages as u64;
    let writeback = vm.writeback_pages as u64;
    let shmem = vm.shmem_pages as u64;
    let page_tables = vm.page_table_pages as u64;
    let slab = vm.slab_unreclaimable_bytes as u64;
    let scanned = vm.background_scanned;
    let reclaimed = vm.background_reclaimed;
    let lru = crate::mm::reclaim_counters();
    let rows: &[(&str, u64)] = &[
        ("nr_free_pages", free),
        ("nr_zone_inactive_anon", 0),
        ("nr_zone_active_anon", 0),
        ("nr_zone_inactive_file", 0),
        ("nr_zone_active_file", 0),
        ("nr_zone_unevictable", 0),
        // Linux: dirty plus writeback file pages.
        ("nr_zone_write_pending", dirty + writeback),
        ("nr_mlock", 0),
        ("nr_free_cma", 0),
        ("nr_inactive_anon", lru.inactive_anon),
        ("nr_active_anon", lru.active_anon),
        ("nr_inactive_file", 0),
        ("nr_active_file", 0),
        ("nr_unevictable", 0),
        ("nr_slab_reclaimable", 0),
        ("nr_slab_unreclaimable", slab),
        ("nr_isolated_anon", 0),
        ("nr_isolated_file", 0),
        ("workingset_nodes", 0),
        ("workingset_refault_anon", 0),
        ("workingset_refault_file", 0),
        ("workingset_activate_anon", 0),
        ("workingset_activate_file", 0),
        ("workingset_restore_anon", 0),
        ("workingset_restore_file", 0),
        ("workingset_nodereclaim", 0),
        ("nr_anon_pages", anon),
        ("nr_mapped", 0),
        ("nr_file_pages", file),
        ("nr_dirty", dirty),
        ("nr_writeback", writeback),
        ("nr_shmem", shmem),
        ("nr_shmem_hugepages", 0),
        ("nr_shmem_pmdmapped", 0),
        ("nr_file_hugepages", 0),
        ("nr_file_pmdmapped", 0),
        ("nr_anon_transparent_hugepages", 0),
        ("nr_vmscan_write", 0),
        ("nr_vmscan_immediate_reclaim", 0),
        ("nr_dirtied", 0),
        ("nr_written", 0),
        ("nr_throttled_written", 0),
        ("nr_kernel_misc_reclaimable", 0),
        ("nr_foll_pin_acquired", 0),
        ("nr_foll_pin_released", 0),
        ("nr_vmalloc", 0),
        ("nr_kernel_stack", 0),
        ("nr_page_table_pages", page_tables),
        ("nr_sec_page_table_pages", 0),
        ("nr_iommu_pages", 0),
        ("nr_swapcached", 0),
        ("pgpromote_success", 0),
        ("pgpromote_candidate", 0),
        ("pgpromote_candidate_nrl", 0),
        ("pgdemote_kswapd", 0),
        ("pgdemote_direct", 0),
        ("pgdemote_khugepaged", 0),
        ("pgdemote_proactive", 0),
        ("pgsteal_kswapd", reclaimed),
        ("pgsteal_direct", lru.pgsteal_direct),
        ("pgsteal_khugepaged", 0),
        ("pgsteal_proactive", 0),
        ("pgsteal_anon", 0),
        ("pgsteal_file", reclaimed),
        ("pgscan_kswapd", scanned),
        ("pgscan_direct", lru.pgscan_direct),
        ("pgscan_khugepaged", 0),
        ("pgscan_proactive", 0),
        ("pgscan_anon", 0),
        ("pgscan_file", scanned),
        ("pgrefill", 0),
        ("nr_hugetlb", 0),
        ("nr_balloon_pages", 0),
        ("nr_gpu_active", 0),
        ("nr_gpu_reclaim", 0),
        ("nr_dirty_threshold", 0),
        ("nr_dirty_background_threshold", 0),
        ("nr_memmap_pages", 0),
        ("nr_memmap_boot_pages", 0),
        ("pgpgin", 0),
        ("pgpgout", 0),
        ("pswpin", vm.swap_reads),
        ("pswpout", vm.swap_writes),
        ("pgfree", 0),
        ("pgactivate", 0),
        ("pgdeactivate", 0),
        ("pglazyfree", 0),
        ("pgfault", vm.faults),
        ("pgmajfault", vm.major_faults),
        ("pglazyfreed", 0),
        ("pgreuse", 0),
        ("pgscan_direct_throttle", 0),
        ("zone_reclaim_success", 0),
        ("zone_reclaim_failed", 0),
        ("pginodesteal", 0),
        ("slabs_scanned", 0),
        ("kswapd_inodesteal", 0),
        ("kswapd_low_wmark_hit_quickly", 0),
        ("kswapd_high_wmark_hit_quickly", 0),
        ("pageoutrun", 0),
        ("pgrotated", 0),
        ("drop_pagecache", 0),
        ("drop_slab", 0),
        ("oom_kill", 0),
        ("pgmigrate_success", 0),
        ("pgmigrate_fail", 0),
        ("thp_migration_success", 0),
        ("thp_migration_fail", 0),
        ("thp_migration_split", 0),
        ("compact_migrate_scanned", 0),
        ("compact_free_scanned", 0),
        ("compact_isolated", 0),
        ("compact_stall", 0),
        ("compact_fail", 0),
        ("compact_success", 0),
        ("compact_daemon_wake", 0),
        ("compact_daemon_migrate_scanned", 0),
        ("compact_daemon_free_scanned", 0),
        ("unevictable_pgs_culled", 0),
        ("unevictable_pgs_scanned", 0),
        ("unevictable_pgs_rescued", 0),
        ("unevictable_pgs_mlocked", 0),
        ("unevictable_pgs_munlocked", 0),
        ("unevictable_pgs_cleared", 0),
        ("unevictable_pgs_stranded", 0),
        ("thp_fault_alloc", 0),
        ("thp_fault_fallback", 0),
        ("thp_fault_fallback_charge", 0),
        ("thp_collapse_alloc", 0),
        ("thp_collapse_alloc_failed", 0),
        ("thp_file_alloc", 0),
        ("thp_file_fallback", 0),
        ("thp_file_fallback_charge", 0),
        ("thp_file_mapped", 0),
        ("thp_split_page", 0),
        ("thp_split_page_failed", 0),
        ("thp_deferred_split_page", 0),
        ("thp_underused_split_page", 0),
        ("thp_split_pmd", 0),
        ("thp_scan_exceed_none_pte", 0),
        ("thp_scan_exceed_swap_pte", 0),
        ("thp_scan_exceed_share_pte", 0),
        ("thp_split_pud", 0),
        ("thp_zero_page_alloc", 0),
        ("thp_zero_page_alloc_failed", 0),
        ("thp_swpout", 0),
        ("thp_swpout_fallback", 0),
    ];
    let mut text = String::new();
    for (name, value) in rows {
        let _ = writeln!(text, "{name} {value}");
    }
    // Linux appends this deprecated counter after the last table entry.
    text.push_str("nr_unstable 0\n");
    text
}

pub(super) fn register(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    root.add(
        "vmstat",
        SimpleFile::new_regular(fs.clone(), || Ok(render_vmstat(&snapshot()))),
    );
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    fn sample_meminfo() -> MeminfoValues {
        MeminfoValues {
            total_kb: 1_000_000,
            free_kb: 500_000,
            available_kb: 600_000,
            cached_kb: 100_000,
            swap_total_kb: 0,
            swap_free_kb: 0,
            dirty_kb: 12,
            writeback_kb: 4,
            anon_kb: 200_000,
            shmem_kb: 8,
            slab_kb: 3_000,
            page_tables_kb: 64,
            commit_limit_kb: 500_000,
            committed_as_kb: 210_000,
        }
    }

    fn labels(text: &str) -> alloc::vec::Vec<&str> {
        text.lines()
            .filter_map(|line| line.split(':').next())
            .collect()
    }

    #[test]
    fn meminfo_field_order_matches_linux_meminfo_c() {
        let text = render_meminfo(&sample_meminfo());
        let expected = [
            "MemTotal",
            "MemFree",
            "MemAvailable",
            "Buffers",
            "Cached",
            "SwapCached",
            "Active",
            "Inactive",
            "Active(anon)",
            "Inactive(anon)",
            "Active(file)",
            "Inactive(file)",
            "Unevictable",
            "Mlocked",
            "SwapTotal",
            "SwapFree",
            "Dirty",
            "Writeback",
            "AnonPages",
            "Mapped",
            "Shmem",
            "KReclaimable",
            "Slab",
            "SReclaimable",
            "SUnreclaim",
            "KernelStack",
            "PageTables",
            "SecPageTables",
            "NFS_Unstable",
            "Bounce",
            "WritebackTmp",
            "CommitLimit",
            "Committed_AS",
            "VmallocTotal",
            "VmallocUsed",
            "VmallocChunk",
            "Percpu",
            "HardwareCorrupted",
            "AnonHugePages",
            "ShmemHugePages",
            "ShmemPmdMapped",
            "FileHugePages",
            "FilePmdMapped",
            "Balloon",
            "GPUActive",
            "GPUReclaim",
            "HugePages_Total",
            "HugePages_Free",
            "HugePages_Rsvd",
            "HugePages_Surp",
            "Hugepagesize",
            "Hugetlb",
            "DirectMap4k",
            "DirectMap2M",
        ];
        assert_eq!(labels(&text), expected);
    }

    /// Parses the kB value of the row whose label is `name`.
    fn kb_of(text: &str, name: &str) -> u64 {
        text.lines()
            .find(|line| line.split(':').next() == Some(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("missing meminfo row {name}"))
    }

    #[test]
    fn meminfo_values_are_live_inputs_in_kb_with_width_eight() {
        let text = render_meminfo(&sample_meminfo());
        assert_eq!(kb_of(&text, "Dirty"), 12);
        assert_eq!(kb_of(&text, "Writeback"), 4);
        assert_eq!(kb_of(&text, "AnonPages"), 200_000);
        assert_eq!(kb_of(&text, "Shmem"), 8);
        assert_eq!(kb_of(&text, "SUnreclaim"), 3_000);
        assert_eq!(kb_of(&text, "Slab"), 3_000);
        assert_eq!(kb_of(&text, "Mapped"), 0);
        // Exact layout: the label is padded to 16 columns, then a width-8 value.
        let dirty = text.lines().find(|l| l.starts_with("Dirty:")).unwrap();
        assert_eq!(dirty, format!("Dirty:{}12 kB", " ".repeat(16)));
        assert!(text.contains(&format!("\nVmallocTotal:{}34359738367 kB\n", " ".repeat(3))));
        assert!(text.starts_with(&format!("MemTotal:{}1000000 kB\n", " ".repeat(8))));
        // HugePages_* count rows are unitless in Linux as well.
        for line in text.lines().filter(|l| !l.starts_with("HugePages_")) {
            assert!(line.ends_with(" kB"), "missing unit: {line:?}");
        }
    }

    #[test]
    fn vmstat_uses_linux_order_and_terminates_with_nr_unstable() {
        let vm = VmSnapshot {
            free_pages: 123,
            anon_pages: 4,
            file_pages: 5,
            dirty_pages: 7,
            writeback_pages: 2,
            shmem_pages: 9,
            page_table_pages: 6,
            slab_unreclaimable_bytes: 4096,
            faults: 1000,
            major_faults: 7,
            swap_reads: 11,
            swap_writes: 13,
            background_scanned: 80,
            background_reclaimed: 70,
        };
        let text = render_vmstat(&vm);
        assert!(text.starts_with("nr_free_pages 123\n"));
        assert!(text.contains("\nnr_dirty 7\nnr_writeback 2\n"));
        assert!(text.contains("\nnr_anon_pages 4\nnr_mapped 0\nnr_file_pages 5\n"));
        assert!(text.contains("\nnr_shmem 9\n"));
        assert!(text.contains("\nnr_page_table_pages 6\n"));
        assert!(text.contains("\nnr_zone_write_pending 9\n"));
        assert!(text.contains("\nnr_slab_unreclaimable 4096\n"));
        assert!(text.contains("pgsteal_kswapd 70\n"));
        assert!(text.contains("pgscan_kswapd 80\n"));
        assert!(text.contains("pswpin 11\npswpout 13\n"));
        assert!(text.contains("pgfault 1000\npgmajfault 7\n"));
        assert!(text.contains("\nthp_fault_alloc 0\n"));
        assert!(text.ends_with("thp_swpout_fallback 0\nnr_unstable 0\n"));
        // Linux order: anon pages precede file pages, which precede dirty.
        let pos = |name: &str| text.find(&format!("\n{name} ")).unwrap();
        assert!(pos("nr_anon_pages") < pos("nr_file_pages"));
        assert!(pos("nr_file_pages") < pos("nr_dirty"));
        assert!(pos("nr_dirty") < pos("nr_writeback"));
        assert!(pos("nr_page_table_pages") < pos("pgfault"));
        for line in text.lines() {
            let fields: alloc::vec::Vec<_> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 2, "bad row {line:?}");
            assert!(fields[1].parse::<u64>().is_ok());
        }
    }
}
