use axerrno::AxResult;
use axhal::paging::MappingFlags;
use axtask::current;
use linux_raw_sys::general::{CAP_IPC_LOCK, RLIMIT_DATA};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, align_up_4k};
use tk_linux_mm::{BrkAdmission, classify_brk};

use super::mmap::check_mmap_memlock_limit;
use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    mm::{DeferredUffdWake, check_memory_overcommit, check_rlimit_as_growth},
    task::AsThread,
};

/// The highest address `brk` may reach, matching this kernel's address-space
/// upper bound.
///
/// Linux bounds growth with `check_brk_limits()` →
/// `get_unmapped_area(NULL, addr, len, 0, MAP_FIXED)`, whose only fixed-size
/// test is `len > mmap_end - mmap_min_addr` against the architecture
/// `TASK_SIZE`.  This kernel expresses the same bound as the end of its user
/// address space; it has no `vm.mmap_min_addr` floor of its own, and the floor
/// cannot matter here because the heap starts far above any such floor.
const USER_ADDRESS_SPACE_END: usize = USER_SPACE_BASE + USER_SPACE_SIZE;

/// Executes the real heap-VMA transaction. PR_SET_MM uses `publish_layout =
/// false` so its one final layout replacement remains atomic with respect to
/// procfs readers; ordinary brk publishes its new endpoint directly.
pub(crate) fn sys_brk_transaction(addr: usize, publish_layout: bool) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let current_top = proc_data.get_heap_top() as usize;
    let heap_base = proc_data.heap_base();
    let initial_heap_end = proc_data.heap_initial_end();

    if addr == 0 {
        return Ok(current_top as isize);
    }

    // Linux `SYSCALL_DEFINE1(brk)` checks, in order: `brk < min_brk`,
    // `check_data_rlimit()` against RLIMIT_DATA, then the
    // unchanged-page/shrink shortcuts, and only then `check_brk_limits()` for
    // growth.  There is no kernel-internal ceiling on the heap: an earlier
    // iteration capped growth at heap_base + 512MiB here, which rejected
    // requests Linux accepts whenever RLIMIT_DATA and the address space still
    // had room.  `end_data == start_data == heap_base` in this model, so
    // RLIMIT_DATA accounts the whole heap and no separate data segment.
    let data_limit = proc_data.rlim.read()[RLIMIT_DATA].current;
    // Every limit failure in Linux `SYSCALL_DEFINE1(brk)` reaches the same
    // `out:` label, which restores `mm->brk` to `origbrk` and returns it, so a
    // rejected request is not an error — it is an unchanged break.
    let Ok(admission) = classify_brk(
        addr as u64,
        current_top as u64,
        initial_heap_end as u64,
        data_limit,
        heap_base as u64,
        heap_base as u64,
        heap_base as u64,
        PAGE_SIZE_4K as u64,
        USER_ADDRESS_SPACE_END as u64,
        0,
    ) else {
        return Ok(current_top as isize);
    };
    match admission {
        BrkAdmission::BelowMinimum | BrkAdmission::DataLimit => {
            return Ok(current_top as isize);
        }
        BrkAdmission::Shrink | BrkAdmission::Grow { .. } => {}
    }

    let new_top_aligned = align_up_4k(addr);
    let current_top_aligned = align_up_4k(current_top);

    // Only map new pages when expanding beyond already mapped region
    // Expansion start should be the greater of initial_heap_end and current_top_aligned
    if new_top_aligned > current_top_aligned {
        let expand_start = VirtAddr::from(initial_heap_end.max(current_top_aligned));
        let expand_size = new_top_aligned.saturating_sub(expand_start.as_usize());
        let aspace_handle = proc_data.aspace();

        if expand_size > 0 {
            let mut aspace = aspace_handle.lock();
            let locked = aspace.locks_future_mappings();
            if locked
                && check_mmap_memlock_limit(
                    proc_data,
                    curr.as_thread().has_effective_capability(CAP_IPC_LOCK),
                    &aspace,
                    expand_start,
                    expand_size,
                )
                .is_err()
            {
                return Ok(current_top as isize);
            }

            let collision_end = new_top_aligned.saturating_add(PAGE_SIZE_4K);
            if aspace.brk_growth_collides(
                current_top_aligned.into(),
                collision_end.into(),
                heap_base.into(),
            ) {
                return Ok(current_top as isize);
            }
            if check_rlimit_as_growth(proc_data, &aspace, expand_size).is_err() {
                return Ok(current_top as isize);
            }
            if check_memory_overcommit(expand_size).is_err() {
                return Ok(current_top as isize);
            }

            let populate = locked && !aspace.locks_future_mappings_on_fault();
            let Some((heap_lineage, mut heap_backend)) = expand_start
                .checked_sub(1)
                .and_then(|tail| aspace.find_area(tail))
                .filter(|area| area.end() == expand_start && area.backend().is_private_anonymous())
                .map(|area| (area.lineage(), area.backend().clone()))
            else {
                return Ok(current_top as isize);
            };
            // brk creates a new VMA tail.  Unlike growdown/fork, Linux does
            // not carry VM_SEALED onto that newly allocated range.
            heap_backend.clear_sealed();
            let growth = aspace.map_with_existing_lineage(
                expand_start,
                expand_size,
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                populate,
                heap_backend,
                locked,
                heap_lineage,
            );
            if let Err(error) = growth
                && !error.published()
            {
                return Ok(current_top as isize);
            }
        }
    } else if new_top_aligned < current_top_aligned {
        // Only unmap pages beyond the initially mapped heap region.
        let shrink_start = VirtAddr::from(initial_heap_end.max(new_top_aligned));
        let shrink_size = current_top_aligned.saturating_sub(shrink_start.as_usize());
        let aspace_handle = proc_data.aspace();

        let wake = if shrink_size == 0 {
            DeferredUffdWake::empty()
        } else {
            let result = {
                let mut aspace = aspace_handle.lock();
                aspace.unmap(shrink_start, shrink_size)
            };
            let Ok(wake) = result else {
                return Ok(current_top as isize);
            };
            wake
        };
        proc_data.clear_mempolicy_range(shrink_start.as_usize(), shrink_size);
        if publish_layout {
            proc_data.set_heap_top(addr);
        }
        wake.finish();
        return Ok(addr as isize);
    }

    if publish_layout {
        proc_data.set_heap_top(addr);
    }
    Ok(addr as isize)
}

pub fn sys_brk(addr: usize) -> AxResult<isize> {
    sys_brk_transaction(addr, true)
}
