//! Linux 7.2.3 mm/vmstat.c key/value format, with real allocator and event
//! sources. Unknown LRU/dirty gauges are not inferred from cumulative scans.

use alloc::{string::String, sync::Arc};
use core::fmt::Write;

use axalloc::UsageKind;
use memory_addr::PAGE_SIZE_4K;

use super::{DirMapping, SimpleFile, SimpleFs};

#[derive(Default)]
struct VmSnapshot {
    free_pages: usize,
    anon_pages: usize,
    file_pages: usize,
    page_table_pages: usize,
    faults: u64,
    major_faults: u64,
    background_scanned: u64,
    background_reclaimed: u64,
}

fn snapshot() -> VmSnapshot {
    let allocator = axalloc::global_allocator();
    let usage = allocator.usages();
    let pressure = crate::mm::memory_pressure_snapshot();
    let (faults, major_faults) = crate::mm::vm_events::snapshot();
    VmSnapshot {
        free_pages: allocator.available_pages(),
        anon_pages: usage.get(UsageKind::VirtMem) / PAGE_SIZE_4K,
        file_pages: usage.get(UsageKind::PageCache) / PAGE_SIZE_4K,
        page_table_pages: usage.get(UsageKind::PageTable) / PAGE_SIZE_4K,
        faults,
        major_faults,
        background_scanned: pressure.scanned_pages,
        background_reclaimed: pressure.reclaimed_pages,
    }
}

fn render_vmstat(vm: &VmSnapshot) -> String {
    let mut text = String::new();
    for (name, value) in [
        ("nr_free_pages", vm.free_pages as u64),
        ("nr_anon_pages", vm.anon_pages as u64),
        ("nr_file_pages", vm.file_pages as u64),
        ("nr_page_table_pages", vm.page_table_pages as u64),
        ("pgfault", vm.faults),
        ("pgmajfault", vm.major_faults),
        ("pgscan_kswapd", vm.background_scanned),
        ("pgsteal_kswapd", vm.background_reclaimed),
        // No swap device or CMA allocator exists in this image.
        ("pswpin", 0),
        ("pswpout", 0),
        ("nr_free_cma", 0),
    ] {
        let _ = writeln!(text, "{name} {value}");
    }
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
    use super::*;

    #[test]
    fn vmstat_uses_named_decimal_page_gauges_and_monotonic_events() {
        let vm = VmSnapshot {
            free_pages: 123,
            anon_pages: 4,
            file_pages: 5,
            page_table_pages: 6,
            faults: 1000,
            major_faults: 7,
            background_scanned: 80,
            background_reclaimed: 70,
        };
        let text = render_vmstat(&vm);
        assert!(text.starts_with(
            "nr_free_pages 123\nnr_anon_pages 4\nnr_file_pages 5\nnr_page_table_pages 6\n"
        ));
        assert!(text.contains("pgfault 1000\npgmajfault 7\n"));
        assert!(text.contains("pgscan_kswapd 80\npgsteal_kswapd 70\n"));
        assert_eq!(text.lines().count(), 11);
        for line in text.lines() {
            let fields: alloc::vec::Vec<_> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 2);
            assert!(fields[1].parse::<u64>().is_ok());
        }
        assert!(!text.contains("nr_dirty"));
    }
}
