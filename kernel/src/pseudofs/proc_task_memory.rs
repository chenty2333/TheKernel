//! Aggregate per-mm procfs views. Linux 7.2.3 fs/proc/task_mmu.c supplies
//! statm's seven-field order and the two legacy fields that are always zero.

use alloc::{format, string::String};
use core::fmt::Write;

use axfs_ng_vfs::{VfsError, VfsResult};
use axhal::paging::MappingFlags;
use memory_addr::PAGE_SIZE_4K;

use crate::{mm::Backend, task::ProcessData};

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

/// Base per-VMA observations. Swap's numeric row is also a record delimiter
/// consumed by procps pmap -x; do not fabricate private/shared dirty or PSS.
pub(super) fn smaps_base(
    out: &mut String,
    size: usize,
    resident: usize,
    swapped: usize,
    locked: usize,
) {
    for (name, bytes) in [
        ("Size", size),
        ("Rss", resident),
        ("Swap", swapped),
        ("Locked", locked),
    ] {
        let label = format!("{name}:");
        let _ = writeln!(out, "{label:<16}{:>8} kB", bytes / 1024);
    }
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

    #[test]
    fn smaps_swap_row_precedes_locked_and_preserves_observed_kilobytes() {
        let mut text = String::new();
        smaps_base(&mut text, 32768, 8192, 16384, 4096);
        assert_eq!(
            text,
            "Size:                 32 kB\nRss:                   8 kB\nSwap:                 16 \
             kB\nLocked:                4 kB\n"
        );
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
