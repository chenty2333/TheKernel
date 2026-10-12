//! `/sys/kernel/mm`: transparent hugepage knobs and hugetlb pool counters.
//!
//! Layout and text formats follow Linux `mm/huge_memory.c`,
//! `mm/khugepaged.c` and `mm/hugetlb_sysfs.c`.  The state lives in
//! `crate::mm::thp` and `super::hugetlb`; writes change that state directly.

use alloc::{string::String, sync::Arc, vec::Vec};

use axfs_ng_vfs::VfsError;

use super::{DirMapping, RwFile, SimpleDir, SimpleFile, SimpleFileOperation, SimpleFs};
use crate::mm::thp::{
    self, ALLOC_SLEEP_MS, DEFRAG, DEFRAG_NAMES, GlobalEnabled, HPAGE_PMD_SIZE,
    KHUGEPAGED_DEFRAG, KHUGEPAGED_MAX_PTES_LIMIT, MAX_PTES_NONE, MAX_PTES_SHARED, MAX_PTES_SWAP,
    PAGES_TO_SCAN, SCAN_SLEEP_MS, SHMEM_ENABLED, SHMEM_ENABLED_NAMES, USE_ZERO_PAGE,
};

/// Linux `kstrtoul(buf, 10, ...)` as used by the sysfs store helpers: decimal
/// digits with at most one trailing newline.
fn parse_decimal(data: &[u8]) -> Option<u64> {
    let data = data.strip_suffix(b"\n").unwrap_or(data);
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(data).ok()?.parse().ok()
}

/// Linux `single_hugepage_flag_store()`: `0` or `1` only.
fn parse_flag(data: &[u8]) -> Result<bool, VfsError> {
    match parse_decimal(data) {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(VfsError::InvalidInput),
    }
}

fn parse_bounded_u32(data: &[u8], max: u32) -> Result<u32, VfsError> {
    match parse_decimal(data) {
        Some(value) if value <= u64::from(max) => Ok(value as u32),
        _ => Err(VfsError::InvalidInput),
    }
}

/// A read/write attribute with Linux's `__ATTR_RW` permissions (0644).
fn rw_file(
    fs: Arc<SimpleFs>,
    read: impl Fn() -> String + Send + Sync + 'static,
    write: impl Fn(&[u8]) -> Result<(), VfsError> + Send + Sync + 'static,
) -> Arc<SimpleFile> {
    SimpleFile::new_regular(
        fs,
        RwFile::new_root_writable(move |operation| match operation {
            SimpleFileOperation::Read => Ok::<_, VfsError>(Some(read().into_bytes())),
            SimpleFileOperation::Write(data) => {
                write(data)?;
                Ok(Some(Vec::<u8>::new()))
            }
        }),
    )
}

/// A read-only attribute with Linux's `__ATTR_RO` permissions (0444).
fn ro_file(fs: Arc<SimpleFs>, read: impl Fn() -> String + Send + Sync + 'static) -> Arc<SimpleFile> {
    SimpleFile::new_regular(fs, move || -> Result<String, VfsError> { Ok(read()) })
}

fn flag_text(value: bool) -> String {
    alloc::format!("{}\n", u8::from(value))
}

fn transparent_hugepage_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut thp_dir = DirMapping::new();
    thp_dir.add(
        "enabled",
        rw_file(fs.clone(), thp::enabled_text, |data| {
            let index = thp::match_choice(data, &GlobalEnabled::NAMES)
                .and_then(|index| GlobalEnabled::from_index(index as u8))
                .ok_or(VfsError::InvalidInput)?;
            thp::set_global_enabled(index);
            Ok(())
        }),
    );
    thp_dir.add(
        "defrag",
        rw_file(
            fs.clone(),
            || {
                let active = usize::from(DEFRAG.load(core::sync::atomic::Ordering::Relaxed));
                thp::bracketed(&DEFRAG_NAMES, active)
            },
            |data| {
                let index = thp::match_choice(data, &DEFRAG_NAMES)
                    .ok_or(VfsError::InvalidInput)?;
                DEFRAG.store(index as u8, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        ),
    );
    thp_dir.add(
        "use_zero_page",
        rw_file(
            fs.clone(),
            || flag_text(USE_ZERO_PAGE.load(core::sync::atomic::Ordering::Relaxed)),
            |data| {
                USE_ZERO_PAGE.store(parse_flag(data)?, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        ),
    );
    thp_dir.add(
        "hpage_pmd_size",
        ro_file(fs.clone(), || alloc::format!("{HPAGE_PMD_SIZE}\n")),
    );
    thp_dir.add(
        "shmem_enabled",
        rw_file(
            fs.clone(),
            || {
                let active = usize::from(SHMEM_ENABLED.load(core::sync::atomic::Ordering::Relaxed));
                thp::bracketed(&SHMEM_ENABLED_NAMES, active)
            },
            |data| {
                let index = thp::match_choice(data, &SHMEM_ENABLED_NAMES)
                    .ok_or(VfsError::InvalidInput)?;
                SHMEM_ENABLED.store(index as u8, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        ),
    );
    thp_dir.add("khugepaged", khugepaged_dir(fs.clone()));
    SimpleDir::new_maker(fs, Arc::new(thp_dir))
}

fn khugepaged_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    use core::sync::atomic::Ordering::Relaxed;

    let mut dir = DirMapping::new();
    dir.add(
        "defrag",
        rw_file(
            fs.clone(),
            || flag_text(KHUGEPAGED_DEFRAG.load(Relaxed)),
            |data| {
                KHUGEPAGED_DEFRAG.store(parse_flag(data)?, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "max_ptes_none",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", MAX_PTES_NONE.load(Relaxed)),
            |data| {
                MAX_PTES_NONE.store(parse_bounded_u32(data, KHUGEPAGED_MAX_PTES_LIMIT)?, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "max_ptes_swap",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", MAX_PTES_SWAP.load(Relaxed)),
            |data| {
                MAX_PTES_SWAP.store(parse_bounded_u32(data, KHUGEPAGED_MAX_PTES_LIMIT)?, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "max_ptes_shared",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", MAX_PTES_SHARED.load(Relaxed)),
            |data| {
                MAX_PTES_SHARED
                    .store(parse_bounded_u32(data, KHUGEPAGED_MAX_PTES_LIMIT)?, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "pages_to_scan",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", PAGES_TO_SCAN.load(Relaxed)),
            |data| {
                // Linux `pages_to_scan_store()` rejects zero.
                let pages = parse_bounded_u32(data, u32::MAX)?;
                if pages == 0 {
                    return Err(VfsError::InvalidInput);
                }
                PAGES_TO_SCAN.store(pages, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "scan_sleep_millisecs",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", SCAN_SLEEP_MS.load(Relaxed)),
            |data| {
                SCAN_SLEEP_MS.store(parse_bounded_u32(data, u32::MAX)?, Relaxed);
                Ok(())
            },
        ),
    );
    dir.add(
        "alloc_sleep_millisecs",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", ALLOC_SLEEP_MS.load(Relaxed)),
            |data| {
                ALLOC_SLEEP_MS.store(parse_bounded_u32(data, u32::MAX)?, Relaxed);
                Ok(())
            },
        ),
    );
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn hstate_dir(fs: Arc<SimpleFs>, hstate: &'static super::hugetlb::Hstate) -> crate::pseudofs::DirMaker {
    use core::sync::atomic::Ordering::Relaxed;

    let mut dir = DirMapping::new();
    dir.add(
        "nr_hugepages",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", hstate.nr_hugepages.load(Relaxed)),
            |data| {
                // Linux `__nr_hugepages_store_common()` parses the request and
                // grows or shrinks the pool.  This kernel has no hugetlb pool
                // allocator, so a request cannot be satisfied and the pool
                // stays at its current size; the write itself succeeds.
                parse_decimal(data).ok_or(VfsError::InvalidInput)?;
                Ok(())
            },
        ),
    );
    dir.add(
        "nr_overcommit_hugepages",
        rw_file(
            fs.clone(),
            || alloc::format!("{}\n", hstate.nr_overcommit_hugepages.load(Relaxed)),
            |data| {
                let value = parse_decimal(data).ok_or(VfsError::InvalidInput)?;
                hstate.nr_overcommit_hugepages.store(value, Relaxed);
                Ok(())
            },
        ),
    );
    for (name, counter) in [
        ("free_hugepages", &hstate.free_hugepages),
        ("resv_hugepages", &hstate.resv_hugepages),
        ("surplus_hugepages", &hstate.surplus_hugepages),
    ] {
        dir.add(
            name,
            ro_file(fs.clone(), move || alloc::format!("{}\n", counter.load(Relaxed))),
        );
    }
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn hugepages_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    for hstate in &super::hugetlb::HSTATES {
        dir.add(hstate.name, hstate_dir(fs.clone(), hstate));
    }
    SimpleDir::new_maker(fs, Arc::new(dir))
}

/// `/sys/kernel/mm`, the directory `kernel/src/pseudofs/sys.rs` mounts under
/// `kernel/`.
pub(super) fn mm_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    dir.add("transparent_hugepage", transparent_hugepage_dir(fs.clone()));
    dir.add("hugepages", hugepages_dir(fs.clone()));
    SimpleDir::new_maker(fs, Arc::new(dir))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn decimal_store_accepts_one_trailing_newline_like_kstrtoul() {
        assert_eq!(parse_decimal(b"10000\n"), Some(10_000));
        assert_eq!(parse_decimal(b"0"), Some(0));
        assert_eq!(parse_decimal(b"\n"), None);
        assert_eq!(parse_decimal(b"12x"), None);
        assert_eq!(parse_decimal(b"-1"), None);
        assert_eq!(parse_decimal(b"99999999999999999999999"), None);
    }

    #[test]
    fn flag_and_bounded_stores_reject_out_of_range_values() {
        assert_eq!(parse_flag(b"1\n"), Ok(true));
        assert_eq!(parse_flag(b"0"), Ok(false));
        assert_eq!(parse_flag(b"2"), Err(VfsError::InvalidInput));
        assert_eq!(parse_bounded_u32(b"511", KHUGEPAGED_MAX_PTES_LIMIT), Ok(511));
        assert_eq!(
            parse_bounded_u32(b"512", KHUGEPAGED_MAX_PTES_LIMIT),
            Err(VfsError::InvalidInput)
        );
    }

    #[test]
    fn linux_default_knob_values_match_khugepaged_init() {
        // mm/khugepaged.c:khugepaged_init() and the sysfs defaults.
        assert_eq!(PAGES_TO_SCAN.load(Ordering::Relaxed), 4096);
        assert_eq!(MAX_PTES_NONE.load(Ordering::Relaxed), 511);
        assert_eq!(MAX_PTES_SWAP.load(Ordering::Relaxed), 64);
        assert_eq!(MAX_PTES_SHARED.load(Ordering::Relaxed), 256);
        assert_eq!(SCAN_SLEEP_MS.load(Ordering::Relaxed), 10_000);
        assert_eq!(ALLOC_SLEEP_MS.load(Ordering::Relaxed), 60_000);
        assert_eq!(HPAGE_PMD_SIZE, 2 * 1024 * 1024);
    }

    #[test]
    fn hugetlb_meminfo_reports_empty_pool_with_linux_widths() {
        assert_eq!(
            super::super::hugetlb::hugetlb_meminfo_lines(),
            "HugePages_Total:       0\nHugePages_Free:        0\nHugePages_Rsvd:        0\nHugePages_Surp:        0\nHugepagesize:       2048 kB\nHugetlb:               0 kB\n"
        );
    }

    #[test]
    fn hugetlb_meminfo_sums_pool_bytes_across_hstates() {
        let hstate = &super::super::hugetlb::HSTATES[0];
        hstate.nr_hugepages.store(3, Ordering::Relaxed);
        hstate.free_hugepages.store(2, Ordering::Relaxed);
        let text = super::super::hugetlb::hugetlb_meminfo_lines();
        hstate.nr_hugepages.store(0, Ordering::Relaxed);
        hstate.free_hugepages.store(0, Ordering::Relaxed);
        assert!(text.contains("HugePages_Total:       3\n"), "{text}");
        assert!(text.contains("HugePages_Free:        2\n"), "{text}");
        assert!(text.contains("Hugetlb:            6144 kB\n"), "{text}");
    }
}
