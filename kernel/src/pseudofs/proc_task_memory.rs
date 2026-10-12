//! Aggregate per-mm procfs views. Linux 7.2.3 fs/proc/task_mmu.c supplies
//! statm's seven-field order and the two legacy fields that are always zero.

use alloc::{format, string::String};
use core::fmt::Write;

use axfs_ng_vfs::{VfsError, VfsResult};
use axhal::paging::{MappingFlags, PageSize};
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::{
    mm::{AddrSpace, Backend, BackendOps, MadviseThp, ProcVmaObservations},
    task::ProcessData,
};

#[derive(Default)]
struct Statm {
    size: usize,
    resident: usize,
    shared: usize,
    text: usize,
    data_stack: usize,
}

fn render(value: &Statm) -> String {
    format!(
        "{} {} {} {} 0 {} 0\n",
        value.size, value.resident, value.shared, value.text, value.data_stack
    )
}

/// Where a resident page's bytes are accounted, following Linux
/// `smaps_account()`'s anon / file / shmem split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageKind {
    Anon,
    File,
    Shmem,
}

/// One resident leaf (a 4 KiB PTE or a 2 MiB PMD) observed in a VMA.
#[derive(Clone, Copy, Debug)]
pub(super) struct ResidentLeaf {
    pub(super) kind: PageKind,
    pub(super) bytes: usize,
    /// CoW owners of the frame; 1 when the frame is not fork-shared.
    pub(super) owners: usize,
    /// The VMA itself is `MAP_SHARED`, so its pages are reported as shared
    /// even when this process is their only mapper.
    pub(super) shared_vma: bool,
    pub(super) huge: bool,
}

/// Linux `struct mem_size_stats`, in bytes. PSS is `bytes / owners` per leaf
/// rather than Linux's fixed-point `PSS_SHIFT` accumulator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MemSizeStats {
    pub(super) resident: usize,
    pub(super) pss: usize,
    pub(super) pss_dirty: usize,
    pub(super) pss_anon: usize,
    pub(super) pss_file: usize,
    pub(super) pss_shmem: usize,
    pub(super) shared_clean: usize,
    pub(super) shared_dirty: usize,
    pub(super) private_clean: usize,
    pub(super) private_dirty: usize,
    pub(super) referenced: usize,
    pub(super) anonymous: usize,
    pub(super) lazyfree: usize,
    pub(super) anonymous_thp: usize,
    pub(super) shmem_thp: usize,
    pub(super) file_thp: usize,
    pub(super) swap: usize,
    pub(super) swap_pss: usize,
    pub(super) locked: usize,
}

impl MemSizeStats {
    pub(super) fn add_leaf(&mut self, leaf: ResidentLeaf) {
        let dirty = match leaf.kind {
            // Anonymous and shmem pages are dirty from first write; this
            // kernel keeps no per-PTE dirty bit to refine file pages.
            PageKind::Anon | PageKind::Shmem => true,
            PageKind::File => false,
        };
        let shared = leaf.shared_vma || leaf.owners > 1;
        let pss = leaf.bytes / leaf.owners.max(1);
        self.resident += leaf.bytes;
        self.pss += pss;
        self.referenced += leaf.bytes;
        if dirty {
            self.pss_dirty += pss;
        }
        match leaf.kind {
            PageKind::Anon => {
                self.pss_anon += pss;
                self.anonymous += leaf.bytes;
                if leaf.huge {
                    self.anonymous_thp += leaf.bytes;
                }
            }
            PageKind::File => {
                self.pss_file += pss;
                if leaf.huge {
                    self.file_thp += leaf.bytes;
                }
            }
            PageKind::Shmem => {
                self.pss_shmem += pss;
                if leaf.huge {
                    self.shmem_thp += leaf.bytes;
                }
            }
        }
        match (shared, dirty) {
            (true, true) => self.shared_dirty += leaf.bytes,
            (true, false) => self.shared_clean += leaf.bytes,
            (false, true) => self.private_dirty += leaf.bytes,
            (false, false) => self.private_clean += leaf.bytes,
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        macro_rules! add {
            ($($field:ident),* $(,)?) => { $( self.$field += other.$field; )* };
        }
        add!(
            resident, pss, pss_dirty, pss_anon, pss_file, pss_shmem, shared_clean,
            shared_dirty, private_clean, private_dirty, referenced, anonymous, lazyfree,
            anonymous_thp, shmem_thp, file_thp, swap, swap_pss, locked,
        );
    }
}

fn kb_field(out: &mut String, label: &str, bytes: usize) {
    let label = format!("{label}:");
    let _ = writeln!(out, "{label:<16}{:>8} kB", bytes / 1024);
}

/// Linux `__show_smap()`: the field block shared by `smaps` and
/// `smaps_rollup`. The rollup adds the three Pss_{Anon,File,Shmem} rows.
pub(super) fn push_smap_fields(out: &mut String, stats: &MemSizeStats, rollup: bool) {
    kb_field(out, "Rss", stats.resident);
    kb_field(out, "Pss", stats.pss);
    kb_field(out, "Pss_Dirty", stats.pss_dirty);
    if rollup {
        kb_field(out, "Pss_Anon", stats.pss_anon);
        kb_field(out, "Pss_File", stats.pss_file);
        kb_field(out, "Pss_Shmem", stats.pss_shmem);
    }
    kb_field(out, "Shared_Clean", stats.shared_clean);
    kb_field(out, "Shared_Dirty", stats.shared_dirty);
    kb_field(out, "Private_Clean", stats.private_clean);
    kb_field(out, "Private_Dirty", stats.private_dirty);
    kb_field(out, "Referenced", stats.referenced);
    kb_field(out, "Anonymous", stats.anonymous);
    // KSM merging is not implemented, so the KSM row is structurally zero.
    kb_field(out, "KSM", 0);
    kb_field(out, "LazyFree", stats.lazyfree);
    kb_field(out, "AnonHugePages", stats.anonymous_thp);
    kb_field(out, "ShmemPmdMapped", stats.shmem_thp);
    kb_field(out, "FilePmdMapped", stats.file_thp);
    // Per-VMA hugetlb accounting (Shared_Hugetlb / Private_Hugetlb) belongs
    // to the hugetlb pseudofs owner; it is structurally zero until wired.
    kb_field(out, "Shared_Hugetlb", 0);
    // Linux prints this label with a trailing space and a width-7 value.
    let _ = writeln!(out, "{:<17}{:>7} kB", "Private_Hugetlb:", 0);
    kb_field(out, "Swap", stats.swap);
    kb_field(out, "SwapPss", stats.swap_pss);
    kb_field(out, "Locked", stats.locked);
}

/// `smaps` record for one VMA, in Linux `show_smap()` order.
pub(super) fn push_smap_record(
    out: &mut String,
    size: usize,
    stats: &MemSizeStats,
    thp_eligible: bool,
    vm_flags: &str,
) {
    kb_field(out, "Size", size);
    kb_field(out, "KernelPageSize", PAGE_SIZE_4K);
    kb_field(out, "MMUPageSize", PAGE_SIZE_4K);
    push_smap_fields(out, stats, false);
    let _ = writeln!(out, "THPeligible:    {:>8}", u8::from(thp_eligible));
    let _ = writeln!(out, "VmFlags: {vm_flags}");
}

/// `smaps_rollup` tail record: one `[rollup]` header, then the summed fields.
pub(super) fn push_smaps_rollup(
    out: &mut String,
    vma_start: usize,
    vma_end: usize,
    stats: &MemSizeStats,
) {
    // Linux seq_setwidth(72) pads the header prefix to column 72 before the
    // "[rollup]" tag.
    let prefix = format!("{vma_start:08x}-{vma_end:08x} ---p 00000000 00:00 0 ");
    out.push_str(&prefix);
    out.extend(core::iter::repeat_n(' ', 72usize.saturating_sub(prefix.len())));
    out.push_str("[rollup]\n");
    push_smap_fields(out, stats, true);
}

const HUGE_LEAF_BYTES: usize = 2 * 1024 * 1024;

/// Walks one VMA's populated leaves. `query` returns the physical address
/// of `vaddr` and its leaf size; `owners_of` returns the CoW owner count of a
/// leaf. Unpopulated spans advance by `fallback_step`, the VMA's page size.
pub(super) fn account_vma_leaves(
    stats: &mut MemSizeStats,
    (start, end): (usize, usize),
    fallback_step: usize,
    kind: PageKind,
    shared_vma: bool,
    mut query: impl FnMut(VirtAddr) -> Option<(PhysAddr, PageSize)>,
    owners_of: impl Fn(PhysAddr, PageSize) -> usize,
) {
    let mut cursor = start;
    while cursor < end {
        let remaining = end - cursor;
        match query(VirtAddr::from(cursor)) {
            Some((pa, size)) => {
                let leaf = size as usize;
                // query_mapped already adds the in-leaf offset to `pa`.
                let offset = cursor % leaf;
                let bytes = (leaf - offset).min(remaining);
                let base = PhysAddr::from(pa.as_usize() - offset);
                stats.add_leaf(ResidentLeaf {
                    kind,
                    bytes,
                    owners: owners_of(base, size),
                    shared_vma,
                    huge: leaf >= HUGE_LEAF_BYTES,
                });
                cursor += bytes;
            }
            None => cursor += fallback_step.max(1).min(remaining),
        }
    }
}

/// Linux `show_smap_vma_flags()` for the VM_* bits this kernel models, in
/// Linux bit order. The `mr`/`mw`/`me` bits are approximated from the current
/// protection because the kernel keeps no separate VM_MAY* state.
pub(super) fn vm_flags_text(
    flags: MappingFlags,
    shared: bool,
    obs: &ProcVmaObservations,
    thp: MadviseThp,
) -> String {
    let writable = flags.contains(MappingFlags::WRITE);
    let items = [
        (flags.contains(MappingFlags::READ), "rd"),
        (writable, "wr"),
        (flags.contains(MappingFlags::EXECUTE), "ex"),
        (shared, "sh"),
        (true, "mr"),
        (writable, "mw"),
        (flags.contains(MappingFlags::EXECUTE), "me"),
        (shared, "ms"),
        (obs.growsdown, "gd"),
        (obs.locked, "lo"),
        (obs.dontfork, "dc"),
        (!shared && writable, "ac"),
        (obs.wipe_on_fork, "wf"),
        (obs.dontdump, "dd"),
        (matches!(thp, MadviseThp::Huge), "hg"),
        (matches!(thp, MadviseThp::NoHuge), "nh"),
    ];
    let mut text = String::new();
    for (on, name) in items {
        if on {
            text.push_str(name);
            text.push(' ');
        }
    }
    text
}

/// Classifies a VMA's resident leaves as Linux would classify its folios.
pub(super) fn page_kind(backend: &Backend) -> PageKind {
    match backend {
        Backend::Shared(_) => PageKind::Shmem,
        _ if backend.has_file_cache_backing() => PageKind::File,
        _ => PageKind::Anon,
    }
}

/// Linux `task_mem()`: the VmPeak..HugetlbPages block of `/proc/<pid>/status`,
/// in Linux order with `%8` kB widths. The caller holds the mm lock.
pub(super) fn task_mem_status(
    aspace: &AddrSpace,
    layout: &crate::task::ProcessMmLayout,
) -> String {
    const PAGE: usize = PAGE_SIZE_4K;
    let (mut anon, mut file, mut shmem) = (0usize, 0usize, 0usize);
    let (mut vm_bytes, mut exec_bytes) = (0usize, 0usize);
    for area in aspace.areas() {
        if !area.flags().contains(MappingFlags::USER) {
            continue;
        }
        vm_bytes += area.size();
        if area.flags().contains(MappingFlags::EXECUTE) {
            exec_bytes += area.size();
        }
        let kind = page_kind(area.backend());
        let mut stats = MemSizeStats::default();
        account_vma_leaves(
            &mut stats,
            (area.start().as_usize(), area.end().as_usize()),
            area.backend().page_size() as usize,
            kind,
            false,
            |va| aspace.page_table().query_mapped(va).ok().map(|(pa, _, size)| (pa, size)),
            |_, _| 1,
        );
        match kind {
            PageKind::Anon => anon += stats.resident,
            PageKind::File => file += stats.resident,
            PageKind::Shmem => shmem += stats.resident,
        }
    }
    let rss_kb = (anon + file + shmem) / 1024;
    let vm_kb = vm_bytes / 1024;
    let hiwater_vm = aspace.merge_vm_highwater(vm_kb as u64);
    let hiwater_rss = aspace.merge_resident_highwater(rss_kb as u64);
    // Linux splits exec_vm into text (the ELF text span) and lib.
    let text = layout
        .end_code
        .div_ceil(PAGE)
        .saturating_sub(layout.start_code / PAGE)
        * PAGE;
    let text = text.min(exec_bytes);
    let lib = exec_bytes - text;
    let mut out = String::new();
    let rows: [(&str, usize); 16] = [
        ("VmPeak", hiwater_vm as usize),
        ("VmSize", vm_kb),
        ("VmLck", aspace.locked_bytes() / 1024),
        // mm->pinned_vm (FOLL_LONGTERM pins) is not tracked per mm.
        ("VmPin", 0),
        ("VmHWM", hiwater_rss as usize),
        ("VmRSS", rss_kb),
        ("RssAnon", anon / 1024),
        ("RssFile", file / 1024),
        ("RssShmem", shmem / 1024),
        ("VmData", aspace.current_data_mapping_bytes() / 1024),
        ("VmStk", aspace.current_stack_mapping_bytes() / 1024),
        ("VmExe", text / 1024),
        ("VmLib", lib / 1024),
        // Page-table bytes are a global allocator class, not per mm.
        ("VmPTE", 0),
        ("VmSwap", aspace.total_swapped_bytes() / 1024),
        // HUGETLB_PENDING: hugetlb's per-mm usage belongs to the hugetlb
        // pseudofs owner; structurally zero until that wiring lands.
        ("HugetlbPages", 0),
    ];
    for (name, kb) in rows {
        let _ = writeln!(out, "{name}:\t{kb:>8} kB");
    }
    out
}

pub(super) fn statm(process: &ProcessData) -> VfsResult<String> {
    let layout = process.mm_layout();
    let aspace = process.aspace();
    let aspace = aspace.lock();
    let mut value = Statm::default();
    for area in aspace.areas() {
        if !area.flags().contains(MappingFlags::USER) {
            continue;
        }
        value.size = value.size.saturating_add(area.size() / PAGE_SIZE_4K);
        let resident = aspace
            .page_table()
            .mapped_bytes(area.start(), area.size())
            .map_err(|_| VfsError::Io)?
            / PAGE_SIZE_4K;
        value.resident = value.resident.saturating_add(resident);
        // Private COW file/ELF leaves own copied frames here; they are not
        // cache aliases. Anonymous COW shared by fork is also not Linux's
        // file/shmem RSS, so it must not inflate statm's third field.
        if matches!(area.backend(), Backend::Shared(_) | Backend::File(_)) {
            value.shared = value.shared.saturating_add(resident);
        }
    }
    value.text = layout
        .end_code
        .div_ceil(PAGE_SIZE_4K)
        .saturating_sub(layout.start_code / PAGE_SIZE_4K);
    value.data_stack = aspace
        .current_data_mapping_bytes()
        .saturating_add(aspace.current_stack_mapping_bytes())
        / PAGE_SIZE_4K;
    Ok(render(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString};
    use axhal::paging::PageSize;

    fn leaf(kind: PageKind, bytes: usize, owners: usize, shared_vma: bool) -> ResidentLeaf {
        ResidentLeaf {
            kind,
            bytes,
            owners,
            shared_vma,
            huge: false,
        }
    }

    fn field(text: &str, name: &str) -> String {
        text.lines()
            .find(|line| line.split(':').next() == Some(name))
            .unwrap_or_else(|| panic!("missing smaps row {name}"))
            .to_string()
    }

    #[test]
    fn smaps_record_uses_linux_order_and_eight_wide_kb_values() {
        let mut stats = MemSizeStats::default();
        stats.add_leaf(leaf(PageKind::Anon, 8192, 1, false));
        stats.swap = 16384;
        stats.swap_pss = 16384;
        stats.locked = 4096;
        let mut text = String::new();
        push_smap_record(&mut text, 32768, &stats, true, "rd wr mr mw ac ");
        let names: alloc::vec::Vec<&str> = text
            .lines()
            .filter_map(|line| line.split(':').next())
            .collect();
        assert_eq!(
            names,
            [
                "Size", "KernelPageSize", "MMUPageSize", "Rss", "Pss", "Pss_Dirty",
                "Shared_Clean", "Shared_Dirty", "Private_Clean", "Private_Dirty",
                "Referenced", "Anonymous", "KSM", "LazyFree", "AnonHugePages",
                "ShmemPmdMapped", "FilePmdMapped", "Shared_Hugetlb", "Private_Hugetlb",
                "Swap", "SwapPss", "Locked", "THPeligible", "VmFlags",
            ]
        );
        // Rows are "label:" padded to 16, then a width-8 value and " kB".
        assert_eq!(field(&text, "Size"), format!("Size:{}32 kB", " ".repeat(17)));
        assert_eq!(field(&text, "Swap"), format!("Swap:{}16 kB", " ".repeat(17)));
        assert_eq!(field(&text, "Private_Hugetlb"), format!("Private_Hugetlb:{}0 kB", " ".repeat(7)));
        assert_eq!(field(&text, "THPeligible"), format!("THPeligible:{}1", " ".repeat(11)));
        assert_eq!(field(&text, "VmFlags"), "VmFlags: rd wr mr mw ac ");
    }

    #[test]
    fn anon_private_leaf_is_dirty_and_fork_shared_leaf_is_shared_with_pss_divided() {
        let mut stats = MemSizeStats::default();
        stats.add_leaf(leaf(PageKind::Anon, 4096, 1, false));
        stats.add_leaf(leaf(PageKind::Anon, 4096, 2, false));
        assert_eq!(stats.resident, 8192);
        assert_eq!(stats.anonymous, 8192);
        assert_eq!(stats.private_dirty, 4096);
        assert_eq!(stats.shared_dirty, 4096);
        // PSS: a 4 KiB leaf shared by two owners contributes 2 KiB.
        assert_eq!(stats.pss, 4096 + 2048);
        assert_eq!(stats.pss_anon, 4096 + 2048);
    }

    #[test]
    fn file_leaves_are_clean_and_shmem_leaves_are_dirty_in_the_right_bucket() {
        let mut stats = MemSizeStats::default();
        stats.add_leaf(leaf(PageKind::File, 4096, 1, false));
        stats.add_leaf(leaf(PageKind::Shmem, 4096, 1, true));
        assert_eq!(stats.private_clean, 4096);
        assert_eq!(stats.shared_dirty, 4096);
        assert_eq!(stats.pss_file, 4096);
        assert_eq!(stats.pss_shmem, 4096);
        assert_eq!(stats.pss_dirty, 4096);
    }

    #[test]
    fn rollup_header_pads_to_column_72_and_adds_per_kind_pss_rows() {
        let mut stats = MemSizeStats::default();
        stats.add_leaf(leaf(PageKind::Anon, 4096, 1, false));
        let mut text = String::new();
        push_smaps_rollup(&mut text, 0x40_0000, 0x40_9000, &stats);
        let header = text.lines().next().unwrap();
        assert_eq!(header.find("[rollup]"), Some(72));
        assert!(header.starts_with("00400000-00409000 ---p 00000000 00:00 0 "));
        assert!(text.contains("\nPss_Anon:"));
        assert!(text.contains("\nPss_Shmem:"));
        let names: alloc::vec::Vec<&str> = text
            .lines()
            .skip(1)
            .filter_map(|line| line.split(':').next())
            .collect();
        assert_eq!(names.first(), Some(&"Rss"));
        assert_eq!(names.last(), Some(&"Locked"));
    }

    #[test]
    fn vm_flags_follow_linux_bit_order_and_trailing_space() {
        let obs = ProcVmaObservations {
            locked: true,
            wipe_on_fork: true,
            dontdump: true,
            ..ProcVmaObservations::default()
        };
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        assert_eq!(
            vm_flags_text(flags, false, &obs, MadviseThp::Default),
            "rd wr mr mw lo ac wf dd "
        );
        assert_eq!(
            vm_flags_text(MappingFlags::READ, true, &ProcVmaObservations::default(), MadviseThp::Huge),
            "rd sh mr ms hg "
        );
    }

    #[test]
    fn leaf_walk_counts_huge_leaf_once_and_skips_holes_by_fallback_step() {
        let mut stats = MemSizeStats::default();
        let base = 0x20_0000usize;
        account_vma_leaves(
            &mut stats,
            (base, base + 0x20_0000 + 0x2000),
            0x1000,
            PageKind::Anon,
            false,
            |va| {
                let va = va.as_usize();
                if va < base + 0x20_0000 {
                    Some((PhysAddr::from(0x4000_0000 + (va - base)), PageSize::Size2M))
                } else {
                    None
                }
            },
            |_, _| 1,
        );
        assert_eq!(stats.resident, 0x20_0000);
        assert_eq!(stats.anonymous_thp, 0x20_0000);
    }

    #[test]
    fn task_mem_status_block_is_linux_task_mem_order_with_live_counters() {
        use crate::mm::{AddrSpace, Backend};
        let start = VirtAddr::from(0x40_0000usize);
        let mut aspace = AddrSpace::new_empty(start, 0x100_0000).unwrap();
        let user = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        aspace
            .map(
                start,
                PAGE_SIZE_4K * 4,
                user,
                false,
                Backend::new_alloc(start, PageSize::Size4K),
            )
            .unwrap();
        aspace.populate_area(start, PAGE_SIZE_4K * 2, user).unwrap();
        let layout = crate::task::ProcessMmLayout {
            start_code: 0x40_0000,
            end_code: 0x40_1000,
            start_data: 0,
            end_data: 0,
            start_brk: 0,
            brk: 0,
            start_stack: 0,
            arg_start: 0,
            arg_end: 0,
            env_start: 0,
            env_end: 0,
            auxv: alloc::vec::Vec::new(),
            heap_mapping_base: 0,
            heap_mapping_initial_end: 0,
        };
        let text = task_mem_status(&aspace, &layout);
        let names: alloc::vec::Vec<&str> = text
            .lines()
            .map(|line| line.split(':').next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "VmPeak", "VmSize", "VmLck", "VmPin", "VmHWM", "VmRSS", "RssAnon", "RssFile",
                "RssShmem", "VmData", "VmStk", "VmExe", "VmLib", "VmPTE", "VmSwap",
                "HugetlbPages",
            ]
        );
        let value = |name: &str| -> usize {
            text.lines()
                .find(|line| line.starts_with(&format!("{name}:")))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
                .unwrap()
        };
        // Four pages are mapped and two are resident: 16 kB of VM, 8 kB RSS.
        assert_eq!(value("VmSize"), 16);
        assert_eq!(value("VmRSS"), 8);
        assert_eq!(value("RssAnon"), 8);
        assert_eq!(value("RssFile"), 0);
        assert_eq!(value("RssShmem"), 0);
        assert_eq!(value("VmSwap"), 0);
        // Peak is at least the current value (publish before lowering).
        assert_eq!(value("VmPeak"), 16);
        assert_eq!(value("VmHWM"), 8);
        // Tab after the label, then a width-8 kB value, as Linux prints it.
        assert!(text.contains(&format!("VmSize:\t{:>8} kB\n", 16)));
        for line in text.lines() {
            assert!(line.ends_with(" kB"), "missing unit: {line:?}");
        }
    }

    #[test]
    fn statm_has_seven_page_count_fields_with_linux_legacy_zeroes() {
        let value = Statm {
            size: 100,
            resident: 50,
            shared: 10,
            text: 2,
            data_stack: 30,
        };
        assert_eq!(render(&value), "100 50 10 2 0 30 0\n");
        assert_eq!(render(&Statm::default()), "0 0 0 0 0 0 0\n");
    }
}
