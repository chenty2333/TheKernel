//! Pure `brk(2)` admission arithmetic.
//!
//! Mirrors Linux v7.2.3 `SYSCALL_DEFINE1(brk)` (`mm/mmap.c`) together with the
//! shared limit helper it calls.  The kernel owns the address space and the
//! heap VMA; this module owns only the numbers Linux compares, so every rule
//! here is host-testable.

use crate::MmError;

/// Outcome of the Linux `brk` limit checks for one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrkAdmission {
    /// The request is below the minimum break and is ignored.
    BelowMinimum,
    /// The request is rejected by `RLIMIT_DATA`; `brk` keeps its old value.
    DataLimit,
    /// Growth that passed every limit check.
    Grow {
        /// Newly covered length from the old aligned break to the new one.
        growth: u64,
    },
    /// Shrink (including an unaligned request that shares the old page).
    Shrink,
}

/// Size of the whole data segment Linux accounts for `RLIMIT_DATA`.
///
/// Linux v7.2.3 `include/linux/mm.h:check_data_rlimit()` compares
///
/// ```c
/// if (rlim < RLIM_INFINITY) {
/// 	if (((new - start) + (end_data - start_data)) > rlim)
/// 		return -ENOSPC;
/// }
/// ```
///
/// where `start` is `mm->start_brk`, `end_data`/`start_data` bound the initial
/// `PT_LOAD` data segment, and `new` is the requested break.  In this kernel's
/// address-space model the initial program data ends exactly at the heap base,
/// so `end_data - start_data` is zero and the accounted size is
/// `new - start_brk`; the general form is kept so the rule stays honest if the
/// model ever grows a data segment of its own.
pub const fn data_segment_size(
    new_brk: u64,
    start_brk: u64,
    end_data: u64,
    start_data: u64,
) -> Option<u64> {
    let Some(heap) = new_brk.checked_sub(start_brk) else {
        return None;
    };
    let Some(initial) = end_data.checked_sub(start_data) else {
        return None;
    };
    heap.checked_add(initial)
}

/// The `check_data_rlimit()` predicate, including the `RLIM_INFINITY` escape.
///
/// Linux never bypasses this check for `CAP_SYS_RESOURCE`: `mm/mmap.c` calls it
/// unconditionally before the grow/shrink branch, and `may_expand_vm()` repeats
/// the `RLIMIT_DATA` comparison for data mappings without a capability escape
/// either.  The only conditional compilation is `CONFIG_COMPAT_BRK`, which
/// changes `min_brk` (the lower bound), not this limit, plus the
/// `ignore_rlimit_data` boot parameter inside `may_expand_vm()`.
pub const fn check_data_rlimit(
    rlim: u64,
    new_brk: u64,
    start_brk: u64,
    end_data: u64,
    start_data: u64,
) -> Result<(), MmError> {
    if rlim == u64::MAX {
        return Ok(());
    }
    match data_segment_size(new_brk, start_brk, end_data, start_data) {
        Some(size) if size > rlim => Err(MmError::DataRlimitExceeded),
        Some(_) => Ok(()),
        // `new - start` underflowed. Linux would wrap and compare a huge value;
        // rejecting is the only safe equivalent here.
        None => Err(MmError::Overflow),
    }
}

/// `check_brk_limits()`'s address-space bound.
///
/// Linux v7.2.3 `mm/mmap.c:check_brk_limits()` calls
/// `get_unmapped_area(NULL, addr, len, 0, MAP_FIXED)`, which for a fixed
/// request reduces to `generic_get_unmapped_area()`'s
///
/// ```c
/// const unsigned long mmap_end = arch_get_mmap_end(addr, len, flags);
/// if (len > mmap_end - mmap_min_addr)
/// 	return -ENOMEM;
/// if (flags & MAP_FIXED)
/// 	return addr;
/// ```
///
/// so a growth whose length cannot fit between `mmap_min_addr` and the
/// architecture's `TASK_SIZE` fails with `-ENOMEM` regardless of free address
/// space.  Returns whether the growth length is representable.
pub const fn growth_fits_task_size(
    growth: u64,
    task_size: u64,
    mmap_min_addr: u64,
) -> Result<(), MmError> {
    match task_size.checked_sub(mmap_min_addr) {
        Some(span) if growth <= span => Ok(()),
        _ => Err(MmError::AddressSpaceExceeded),
    }
}

/// Whether the full address range `[start, start + growth)` is inside the
/// caller's address space.  This is the second half of the `TASK_SIZE` bound:
/// the length can fit yet still start above the last representable address,
/// which Linux rejects in `get_unmapped_area()`/`find_vma()`.
pub const fn growth_start_in_range(start: u64, growth: u64, space_end: u64) -> Result<(), MmError> {
    match start.checked_add(growth) {
        Some(end) if end <= space_end => Ok(()),
        _ => Err(MmError::AddressSpaceExceeded),
    }
}

/// Linux `SYSCALL_DEFINE1(brk)` limit classification for one request.
///
/// Order is the ABI:
/// 1. `brk < min_brk` is ignored (the caller keeps the old break);
/// 2. `check_data_rlimit()` runs *before* the grow/shrink branch, so a shrink
///    can still fail when `RLIMIT_DATA` was lowered below the current segment;
/// 3. `PAGE_ALIGN(brk) == PAGE_ALIGN(mm->brk)` publishes the unaligned value
///    without any further check;
/// 4. `brk <= mm->brk` is the "always allow shrinking" path — Linux skips
///    `check_brk_limits()` and `do_brk_flags()` there, but not step 2;
/// 5. anything else is growth and must satisfy the address-space bound.
pub fn classify_brk(
    requested: u64,
    current_brk: u64,
    min_brk: u64,
    rlim_data: u64,
    start_brk: u64,
    end_data: u64,
    start_data: u64,
    page_size: u64,
    task_size: u64,
    mmap_min_addr: u64,
) -> Result<BrkAdmission, MmError> {
    if requested < min_brk {
        return Ok(BrkAdmission::BelowMinimum);
    }
    check_data_rlimit(rlim_data, requested, start_brk, end_data, start_data)?;

    let new_aligned = align_up(requested, page_size).ok_or(MmError::Overflow)?;
    let old_aligned = align_up(current_brk, page_size).ok_or(MmError::Overflow)?;
    if new_aligned == old_aligned || requested <= current_brk {
        return Ok(BrkAdmission::Shrink);
    }

    let growth = new_aligned - old_aligned;
    growth_fits_task_size(growth, task_size, mmap_min_addr)?;
    growth_start_in_range(old_aligned, growth, task_size)?;
    Ok(BrkAdmission::Grow { growth })
}

/// Rounds `value` up to `page_size`, which must be a power of two.
pub const fn align_up(value: u64, page_size: u64) -> Option<u64> {
    if !page_size.is_power_of_two() {
        return None;
    }
    match value.checked_add(page_size - 1) {
        Some(rounded) => Some(rounded & !(page_size - 1)),
        None => None,
    }
}
