use axerrno::AxResult;
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use memory_addr::{VirtAddr, align_up_4k};

use super::mmap::check_mmap_memlock_limit;
use crate::{
    config::{USER_HEAP_BASE, USER_HEAP_SIZE, USER_HEAP_SIZE_MAX},
    mm::Backend,
    task::AsThread,
};

pub fn sys_brk(addr: usize) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let current_top = proc_data.get_heap_top() as usize;
    let heap_limit = USER_HEAP_BASE + USER_HEAP_SIZE_MAX;
    let initial_heap_end = USER_HEAP_BASE + USER_HEAP_SIZE;

    if addr == 0 {
        return Ok(current_top as isize);
    }

    if addr < initial_heap_end || addr > heap_limit {
        return Ok(current_top as isize);
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
                && check_mmap_memlock_limit(proc_data, &aspace, expand_start, expand_size).is_err()
            {
                return Ok(current_top as isize);
            }

            let populate = locked && !aspace.locks_future_mappings_on_fault();
            if aspace
                .map_with_lock_state(
                    expand_start,
                    expand_size,
                    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
                    false,
                    Backend::new_alloc(expand_start, PageSize::Size4K),
                    locked,
                )
                .is_err()
            {
                return Ok(current_top as isize);
            }
            if populate
                && aspace
                    .populate_area(expand_start, expand_size, MappingFlags::empty())
                    .is_err()
            {
                let _ = aspace.unmap(expand_start, expand_size);
                return Ok(current_top as isize);
            }
        }
    } else if new_top_aligned < current_top_aligned {
        // Only unmap pages beyond the initially mapped heap region.
        let shrink_start = VirtAddr::from(initial_heap_end.max(new_top_aligned));
        let shrink_size = current_top_aligned.saturating_sub(shrink_start.as_usize());
        let aspace_handle = proc_data.aspace();

        if shrink_size > 0
            && aspace_handle
                .lock()
                .unmap(shrink_start, shrink_size)
                .is_err()
        {
            return Ok(current_top as isize);
        }
        proc_data.clear_mempolicy_range(shrink_start.as_usize(), shrink_size);
    }

    proc_data.set_heap_top(addr);
    Ok(addr as isize)
}
