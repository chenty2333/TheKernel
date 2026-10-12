//! Transparent-huge-page global policy and bounded promotion worker.
//!
//! The global knobs mirror Linux `mm/huge_memory.c` and `mm/khugepaged.c`:
//! `/sys/kernel/mm/transparent_hugepage/*` is a view over the atomics below, and
//! writes take effect on the next scan.  `enabled` gates background promotion
//! in `AddrSpace::background_thp_policy_allows`; `scan_sleep_millisecs` is the
//! worker's live interval.  The remaining knobs are recorded for userspace but
//! have no effect because TheKernel has no compaction, huge zero folio or shmem
//! THP engine.
//!
//! This is deliberately independent from memory-pressure reclaim: explicit
//! MADV_HUGEPAGE and the mm default must make progress while memory is healthy.
//! Each wake retains at most one address-space lock and scans a fixed PMD
//! budget, so a large mm cannot monopolize the worker.

use alloc::string::String;
use core::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    time::Duration,
};

use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

const KHUGEPAGED_PMD_BUDGET: usize = 64;

/// Linux `HPAGE_PMD_SIZE` on x86_64.
pub(crate) const HPAGE_PMD_SIZE: usize = 2 * 1024 * 1024;
/// Linux `HPAGE_PMD_NR`.
pub(crate) const HPAGE_PMD_NR: u32 = 512;
/// Linux `KHUGEPAGED_MAX_PTES_LIMIT` (`HPAGE_PMD_NR - 1`).
pub(crate) const KHUGEPAGED_MAX_PTES_LIMIT: u32 = HPAGE_PMD_NR - 1;

/// Linux `global_enabled_mode`: the `enabled` file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalEnabled {
    Always,
    Madvise,
    Never,
}

impl GlobalEnabled {
    pub(crate) const NAMES: [&'static str; 3] = ["always", "madvise", "never"];

    pub(crate) const fn index(self) -> u8 {
        match self {
            Self::Always => 0,
            Self::Madvise => 1,
            Self::Never => 2,
        }
    }

    pub(crate) const fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::Always),
            1 => Some(Self::Madvise),
            2 => Some(Self::Never),
            _ => None,
        }
    }
}

/// Linux `ENABLED` default: `CONFIG_TRANSPARENT_HUGEPAGE_ALWAYS`.
static ENABLED: AtomicU8 = AtomicU8::new(GlobalEnabled::Always.index());
/// Linux `defrag` default: `TRANSPARENT_HUGEPAGE_DEFRAG_REQ_MADV_FLAG` only.
pub(crate) static DEFRAG: AtomicU8 = AtomicU8::new(DEFRAG_MADVISE);
pub(crate) const DEFRAG_NAMES: [&str; 5] = ["always", "defer", "defer+madvise", "madvise", "never"];
const DEFRAG_MADVISE: u8 = 3;
pub(crate) static USE_ZERO_PAGE: AtomicBool = AtomicBool::new(true);
/// Linux `shmem_huge` default: `TRANSPARENT_HUGEPAGE_SHMEM_HUGE_NEVER`.
pub(crate) static SHMEM_ENABLED: AtomicU8 = AtomicU8::new(3);
pub(crate) const SHMEM_ENABLED_NAMES: [&str; 6] =
    ["always", "within_size", "advise", "never", "deny", "force"];
/// `khugepaged/defrag` default: set.
pub(crate) static KHUGEPAGED_DEFRAG: AtomicBool = AtomicBool::new(true);
pub(crate) static PAGES_TO_SCAN: AtomicU32 = AtomicU32::new(HPAGE_PMD_NR * 8);
pub(crate) static MAX_PTES_NONE: AtomicU32 = AtomicU32::new(KHUGEPAGED_MAX_PTES_LIMIT);
pub(crate) static MAX_PTES_SWAP: AtomicU32 = AtomicU32::new(HPAGE_PMD_NR / 8);
pub(crate) static MAX_PTES_SHARED: AtomicU32 = AtomicU32::new(HPAGE_PMD_NR / 2);
pub(crate) static SCAN_SLEEP_MS: AtomicU32 = AtomicU32::new(10_000);
pub(crate) static ALLOC_SLEEP_MS: AtomicU32 = AtomicU32::new(60_000);

/// The current global `enabled` policy.
pub(crate) fn global_enabled() -> GlobalEnabled {
    GlobalEnabled::from_index(ENABLED.load(Ordering::Relaxed)).unwrap_or(GlobalEnabled::Always)
}

/// Replaces the global `enabled` policy; takes effect on the next scan.
pub(crate) fn set_global_enabled(mode: GlobalEnabled) {
    ENABLED.store(mode.index(), Ordering::Relaxed);
}

/// Linux `enabled_show()`: the active mode is bracketed.
pub(crate) fn enabled_text() -> String {
    let active = global_enabled().index() as usize;
    bracketed(&GlobalEnabled::NAMES, active)
}

/// Linux `sysfs_match_string()` as used by the `*_store()` helpers: an exact
/// name with an optional single trailing newline.
pub(crate) fn match_choice(buf: &[u8], names: &[&str]) -> Option<usize> {
    let buf = buf.strip_suffix(b"\n").unwrap_or(buf);
    let text = core::str::from_utf8(buf).ok()?;
    names.iter().position(|name| *name == text)
}

/// Linux `sysfs_emit()` of a `[active] other ...` list.
pub(crate) fn bracketed(names: &[&str], active: usize) -> String {
    let mut out = String::new();
    for (index, name) in names.iter().enumerate() {
        if index != 0 {
            out.push(' ');
        }
        if index == active {
            out.push('[');
            out.push_str(name);
            out.push(']');
        } else {
            out.push_str(name);
        }
    }
    out.push('\n');
    out
}

#[derive(Clone, Copy)]
struct KhugepagedCursor {
    mm_index: usize,
    address: usize,
}

static KHUGEPAGED_CURSOR: SpinNoIrq<KhugepagedCursor> = SpinNoIrq::new(KhugepagedCursor {
    mm_index: 0,
    address: 0,
});

fn scan_once() {
    let spaces = match crate::mm::try_live_address_spaces() {
        Ok(spaces) => spaces,
        Err(error) => {
            warn!("khugepaged live-mm snapshot failed: {error:?}");
            return;
        }
    };
    if spaces.is_empty() {
        *KHUGEPAGED_CURSOR.lock() = KhugepagedCursor {
            mm_index: 0,
            address: 0,
        };
        return;
    }

    let snapshot = *KHUGEPAGED_CURSOR.lock();
    let mm_index = snapshot.mm_index.min(spaces.len() - 1);
    let next = spaces[mm_index]
        .lock()
        .collapse_background_thp_budget(VirtAddr::from(snapshot.address), KHUGEPAGED_PMD_BUDGET);

    let mut cursor = KHUGEPAGED_CURSOR.lock();
    if let Some(next) = next {
        cursor.mm_index = mm_index;
        cursor.address = next.as_usize();
    } else {
        cursor.mm_index = (mm_index + 1) % spaces.len();
        cursor.address = 0;
    }
}

fn khugepaged_worker() {
    loop {
        let interval = u64::from(SCAN_SLEEP_MS.load(Ordering::Relaxed));
        if let Err(error) = axtask::sleep(Duration::from_millis(interval)) {
            error!("khugepaged worker stopped: {error}");
            return;
        }
        if global_enabled() != GlobalEnabled::Never {
            scan_once();
        }
        axtask::yield_now();
    }
}

pub(crate) fn init_khugepaged() {
    let mut name = String::new();
    name.try_reserve_exact("khugepaged".len())
        .expect("failed to allocate khugepaged worker name");
    name.push_str("khugepaged");
    axtask::try_spawn_with_name(khugepaged_worker, name)
        .expect("failed to start khugepaged worker");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_text_matches_linux_bracket_format() {
        assert_eq!(bracketed(&GlobalEnabled::NAMES, 0), "[always] madvise never\n");
        assert_eq!(bracketed(&GlobalEnabled::NAMES, 1), "always [madvise] never\n");
        assert_eq!(bracketed(&GlobalEnabled::NAMES, 2), "always madvise [never]\n");
    }

    #[test]
    fn choice_match_accepts_one_trailing_newline_only() {
        assert_eq!(match_choice(b"madvise\n", &GlobalEnabled::NAMES), Some(1));
        assert_eq!(match_choice(b"never", &GlobalEnabled::NAMES), Some(2));
        assert_eq!(match_choice(b"never\n\n", &GlobalEnabled::NAMES), None);
        assert_eq!(match_choice(b"Never", &GlobalEnabled::NAMES), None);
        assert_eq!(match_choice(b"", &GlobalEnabled::NAMES), None);
    }
}
