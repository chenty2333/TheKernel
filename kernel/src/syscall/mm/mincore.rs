// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// Copyright (C) 2025 Azure-stars <Azure_stars@126.com>
// Copyright (C) 2025 Yuekai Jia <equation618@gmail.com>
// See LICENSES for license details.
//
// This file has been modified by KylinSoft on 2025.

use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use memory_addr::{PAGE_SIZE_4K, VirtAddr};
use thekernel_linux_mm::{MincorePlan, MmError};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, vm_write_slice};

use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    mm::{AddrSpace, map_usercopy_error},
};

fn map_mincore_plan_error(error: MmError) -> AxError {
    match error {
        MmError::Unaligned | MmError::InvalidPageSize => AxError::InvalidInput,
        MmError::AddressOutOfRange | MmError::Overflow => AxError::NoMemory,
        _ => AxError::InvalidInput,
    }
}

/// Linux x86-64's `access_ok` boundary check, including a zero-length range.
///
/// A zero-sized range at address zero is valid, but a noncanonical/output
/// pointer above the user limit is not. Mapping and permissions remain the
/// responsibility of the later VMA walk or usercopy operation.
fn mincore_access_ok(start: usize, len: usize) -> bool {
    const USER_POINTER_LIMIT: usize = USER_SPACE_BASE + USER_SPACE_SIZE - 1;

    start
        .checked_add(len)
        .is_some_and(|end| start <= USER_POINTER_LIMIT && end <= USER_POINTER_LIMIT)
}

fn mincore_snapshot(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    start_addr: VirtAddr,
    rounded_len: usize,
    page_count: usize,
) -> AxResult<Vec<u8>> {
    let aspace = aspace_handle.lock();

    if !aspace.contains_range(start_addr, rounded_len) {
        return Err(AxError::NoMemory);
    }

    let mut result = Vec::new();
    result
        .try_reserve_exact(page_count)
        .map_err(|_| AxError::NoMemory)?;
    result.resize(page_count, 0);
    let mut i = 0;

    while i < page_count {
        let addr = start_addr + i * PAGE_SIZE_4K;

        // ENOMEM: Check if this page is within a valid VMA
        let area = aspace.find_area(addr).ok_or(AxError::NoMemory)?;

        // Verify we have at least USER access permission
        if !area.flags().contains(MappingFlags::USER) {
            return Err(AxError::NoMemory);
        }

        // Query page table with batch awareness.
        let (is_resident, size) = match aspace.page_table().query(addr) {
            Ok((_, _, size)) => {
                // Physical page exists and is resident.
                (true, size as _)
            }
            Err(_) => {
                // Linux also reports a file-backed page as resident when
                // it is already in the shared file cache but this address
                // space has not installed a PTE for it yet.
                (area.backend().cached_page_resident(addr), PAGE_SIZE_4K)
            }
        };
        let n = size / PAGE_SIZE_4K;

        if is_resident {
            let end = (i + n).min(page_count);
            result[i..end].fill(1);
        }

        i += n;
    }

    Ok(result)
}

/// Check whether pages are resident in memory.
///
/// The mincore() system call determines whether pages of the calling process's
/// virtual memory are resident in RAM.
///
/// # Arguments
/// * `addr` - Starting address (must be a multiple of the page size)
/// * `length` - Length of the region in bytes (effectively rounded up to next page boundary)
/// * `vec` - Output array containing at least (length+PAGE_SIZE-1)/PAGE_SIZE bytes.
///
/// # Return Value
/// * `Ok(0)` on success
/// * `Err(EAGAIN)` - Kernel is temporarily out of resources (not implemented in TheKernel)
/// * `Err(EFAULT)` - vec points to an invalid address (handled by vm_write_slice)
/// * `Err(EINVAL)` - addr is not a multiple of the page size
/// * `Err(ENOMEM)` - length is greater than (TASK_SIZE - addr), or negative length, or `addr` to `addr`+`length` contained unmapped memory
///
/// # Notes from Linux man page
/// - The least significant bit (bit 0) is set if page is resident in memory
/// - Bits 1-7 are reserved and currently cleared
/// - Information is only a snapshot; pages can be swapped at any moment
///
/// # Linux Errors
/// - EAGAIN:  kernel temporarily out of resources
/// - EFAULT: vec points to invalid address
/// - EINVAL: addr not page-aligned
/// - ENOMEM: length > (TASK_SIZE - addr), negative length, or unmapped memory
pub fn sys_mincore<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    aspace_handle: Arc<Mutex<AddrSpace>>,
    addr: usize,
    length: usize,
    vec: *mut u8,
) -> AxResult<isize> {
    const USER_POINTER_LIMIT: usize = USER_SPACE_BASE + USER_SPACE_SIZE - 1;
    let plan = MincorePlan::new(addr, length, PAGE_SIZE_4K, USER_POINTER_LIMIT)
        .map_err(map_mincore_plan_error)?;

    // Linux computes the output page count before access_ok(vec, pages). A
    // zero-sized vec accepts NULL but still rejects an out-of-range pointer.
    if !mincore_access_ok(vec as usize, plan.page_count()) {
        return Err(AxError::BadAddress);
    }
    if plan.is_empty() {
        return Ok(0);
    }

    // A nonempty output must name user memory. The ABI plan above computes
    // this output extent before this usercopy-specific check.
    if vec.is_null() {
        return Err(AxError::BadAddress);
    }

    debug!("sys_mincore <= addr: {addr:#x}, length: {length:#x}, vec: {vec:?}");
    let start_addr = VirtAddr::from(plan.start());

    // The supplied address-space handle is captured at dispatch entry and is
    // also used by the explicit user-memory provider. Keep its lock scoped to
    // the residency snapshot; copyout below must not hold it.
    let result = mincore_snapshot(
        &aspace_handle,
        start_addr,
        plan.rounded_len(),
        plan.page_count(),
    )?;

    // EFAULT: Write result to user space only after releasing the address
    // space lock. The explicit provider reacquires the same selected address
    // space for copyout without extending this inspection critical section.
    vm_write_slice(memory, vec, result.as_slice()).map_err(map_usercopy_error)?;

    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{mem::MaybeUninit, ptr};

    use thekernel_linux_usercopy::{UserCopyError, VmResult};

    use super::*;

    struct AccessProbe {
        reads: usize,
        writes: usize,
    }

    // SAFETY: This probe never dereferences userspace addresses and reports
    // every attempted access as a fault.
    unsafe impl UserMemory for AccessProbe {
        fn read(&mut self, _: usize, _: &mut [MaybeUninit<u8>]) -> VmResult {
            self.reads += 1;
            Err(UserCopyError::BadAddress)
        }

        fn write(&mut self, _: usize, _: &[u8]) -> VmResult {
            self.writes += 1;
            Err(UserCopyError::BadAddress)
        }
    }

    fn empty_aspace() -> Arc<Mutex<AddrSpace>> {
        Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(USER_SPACE_BASE), PAGE_SIZE_4K).unwrap(),
        ))
    }

    #[test]
    fn zero_length_accepts_null_vec_without_mapping_or_usercopy() {
        let mut provider = AccessProbe {
            reads: 0,
            writes: 0,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_mincore(
                &mut memory,
                empty_aspace(),
                USER_SPACE_BASE,
                0,
                ptr::null_mut(),
            ),
            Ok(0)
        );
        drop(memory);

        assert_eq!(
            sys_mincore(
                &mut UserMemoryContext::new(&mut provider),
                empty_aspace(),
                0,
                0,
                ptr::null_mut(),
            ),
            Ok(0)
        );
        assert_eq!((provider.reads, provider.writes), (0, 0));
    }

    #[test]
    fn zero_length_keeps_alignment_and_address_limit_checks() {
        let mut provider = AccessProbe {
            reads: 0,
            writes: 0,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_mincore(
                &mut memory,
                empty_aspace(),
                USER_SPACE_BASE + 1,
                0,
                ptr::null_mut(),
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_mincore(
                &mut memory,
                empty_aspace(),
                USER_SPACE_BASE + USER_SPACE_SIZE,
                0,
                ptr::null_mut(),
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(
            sys_mincore(
                &mut memory,
                empty_aspace(),
                USER_SPACE_BASE,
                0,
                (USER_SPACE_BASE + USER_SPACE_SIZE) as *mut u8,
            ),
            Err(AxError::BadAddress)
        );
        drop(memory);
        assert_eq!((provider.reads, provider.writes), (0, 0));
    }
}
