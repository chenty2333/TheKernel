//! Linux cachestat(2) ABI adapter.

use core::ffi::c_int;

use axerrno::{AxError, AxResult};
use bytemuck::{Pod, Zeroable};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{file::get_file_like, mm::map_usercopy_error};

const PAGE_SHIFT: u32 = 12;

/// Native x86_64 UAPI `struct cachestat_range`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CachestatRange {
    off: u64,
    len: u64,
}

/// Native x86_64 UAPI `struct cachestat`.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct Cachestat {
    nr_cache: u64,
    nr_dirty: u64,
    nr_writeback: u64,
    nr_evicted: u64,
    nr_recently_evicted: u64,
}

/// Implements Linux 6.12 cachestat(2).
///
/// Keep validation in the upstream order: descriptor lookup, range copyin,
/// hugetlb classification (not currently representable by TheKernel), flags,
/// then output copyout.  `off + len` deliberately wraps as the UAPI's unsigned
/// arithmetic does in Linux's filemap implementation.
pub(crate) fn sys_cachestat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: c_int,
    range: *const CachestatRange,
    output: *mut Cachestat,
    flags: u32,
) -> AxResult<isize> {
    // EBADF must win over an invalid range pointer.
    let file = get_file_like(fd)?;
    let range = VmPtr::vm_read(range, memory).map_err(map_usercopy_error)?;

    // TheKernel has no hugetlbfs FileLike. Keep this point before flags so a
    // future hugetlb implementation cannot accidentally change Linux errno
    // precedence.
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let first_page = range.off >> PAGE_SHIFT;
    let last_page = if range.len == 0 {
        u64::MAX
    } else {
        range.off.wrapping_add(range.len).wrapping_sub(1) >> PAGE_SHIFT
    };
    let snapshot = file.cachestat(first_page, last_page)?;
    let stats = Cachestat {
        nr_cache: snapshot.nr_cache,
        nr_dirty: snapshot.nr_dirty,
        nr_writeback: snapshot.nr_writeback,
        nr_evicted: snapshot.nr_evicted,
        nr_recently_evicted: snapshot.nr_recently_evicted,
    };
    VmMutPtr::vm_write(output, memory, stats).map_err(map_usercopy_error)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_page_bounds_match_linux_unsigned_arithmetic() {
        let range = CachestatRange { off: 4095, len: 2 };
        assert_eq!(range.off >> PAGE_SHIFT, 0);
        assert_eq!(
            range.off.wrapping_add(range.len).wrapping_sub(1) >> PAGE_SHIFT,
            1
        );

        let overflow = CachestatRange {
            off: u64::MAX,
            len: 2,
        };
        assert_eq!(overflow.off >> PAGE_SHIFT, u64::MAX >> PAGE_SHIFT);
        assert_eq!(
            overflow.off.wrapping_add(overflow.len).wrapping_sub(1) >> PAGE_SHIFT,
            0
        );
    }
}
