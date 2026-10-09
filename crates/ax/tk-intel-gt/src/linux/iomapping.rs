// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Source-compatible, fail-closed x86 Linux `io_mapping` WC mappings.
//!
//! Unlike a device/UC `iomap`, i915 stolen-memory mappings require PAT WC.
//! This owner creates a dedicated kernel VA alias with PAT1/WC leaves and
//! keeps the VA reserved until the kernel's acknowledged global shootdown has
//! completed. The service admits only reserved or device physical ranges, so
//! it never creates a WC alias over allocator-owned normal RAM.

#![allow(unsafe_code)]

use alloc::collections::BTreeMap;
use core::ptr;

use axhal::{
    mem::{MemRegionFlags, memory_regions, phys_to_virt},
    paging::MappingFlags,
};
use kernel_guard::NoPreemptIrqSave;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};
use spin::Mutex;

use crate::{
    linux::{gem_memory::IoMapping, vm::PAGE_KERNEL_WC},
    linux_config::PAGE_SIZE,
};

const X86_64_PML4_SLOT_SIZE: usize = 1usize << 39;

#[derive(Clone, Copy)]
struct IoMappingRecord {
    physical_base: usize,
    mapped_base: usize,
    mapped_size: usize,
    return_offset: usize,
}

static IO_MAPPINGS: Mutex<BTreeMap<usize, IoMappingRecord>> = Mutex::new(BTreeMap::new());

fn with_records<T>(f: impl FnOnce(&mut BTreeMap<usize, IoMappingRecord>) -> T) -> T {
    let _irq_guard = NoPreemptIrqSave::new();
    let mut records = IO_MAPPINGS.lock();
    f(&mut records)
}

fn physical_extent(base: usize, size: usize) -> Option<(usize, usize, usize)> {
    if size == 0 {
        return None;
    }
    let end = base.checked_add(size)?;
    let mapped_base = base & !(PAGE_SIZE - 1);
    let mapped_end = end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    Some((
        mapped_base,
        mapped_end.checked_sub(mapped_base)?,
        base - mapped_base,
    ))
}

/// Return true only when the full byte range is firmware-reserved and not
/// part of the page allocator's free physical ranges.
fn wc_physical_range_allowed(base: usize, size: usize) -> bool {
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    if base >= end {
        return false;
    }
    let mut cursor = base;
    for region in memory_regions() {
        let start = region.paddr.as_usize();
        let Some(region_end) = start.checked_add(region.size) else {
            return false;
        };
        if region_end <= cursor || start > cursor {
            continue;
        }
        if region.flags.contains(MemRegionFlags::FREE)
            || !region
                .flags
                .intersects(MemRegionFlags::RESERVED | MemRegionFlags::DEVICE)
        {
            return false;
        }
        cursor = end.min(region_end);
        if cursor == end {
            return true;
        }
    }
    false
}

fn vmap_search_window() -> Option<(VirtAddr, VirtAddrRange)> {
    let mut max_phys_end = 0usize;
    for region in memory_regions() {
        let end = region.paddr.as_usize().checked_add(region.size)?;
        max_phys_end = max_phys_end.max(end);
    }
    let (base, size) = axhal::mem::kernel_aspace();
    let end = base.as_usize().checked_add(size)?;
    let slot_base = base.as_usize() & !(X86_64_PML4_SLOT_SIZE - 1);
    if slot_base != base.as_usize() {
        return None;
    }
    let slot_end = slot_base.checked_add(X86_64_PML4_SLOT_SIZE)?;
    let limit_end = end.min(slot_end);
    let direct_end = axhal::mem::phys_to_virt(PhysAddr::from_usize(max_phys_end)).as_usize();
    let hint = direct_end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    let hint = hint.checked_add(PAGE_SIZE)?;
    if hint >= limit_end {
        return None;
    }
    Some((
        VirtAddr::from_usize(hint),
        VirtAddrRange::from_start_size(base, limit_end - base.as_usize()),
    ))
}

unsafe fn unmap_partial(start: VirtAddr, mapped: usize) {
    if mapped == 0 {
        return;
    }
    {
        let mut aspace = axmm::kernel_aspace().lock();
        for index in 0..mapped / PAGE_SIZE {
            let va = VirtAddr::from_usize(start.as_usize() + index * PAGE_SIZE);
            assert!(aspace.unmap(va, PAGE_SIZE).is_ok());
        }
        assert!(aspace.reserve(start, mapped).is_ok());
    }
    axmm::synchronize_kernel_map_tlb()
        .expect("kernel-map TLB synchronizer missing during WC iomap rollback");
    assert!(axmm::kernel_aspace().lock().unmap(start, mapped).is_ok());
}

/// Implement the non-atomic x86 Linux `io_mapping_init_wc()` path. The mapping
/// remains unavailable until global TLB shootdown is installed and PAT1/WC is
/// ready on every startup CPU.
pub unsafe fn io_mapping_init_wc(mapping: *mut IoMapping, base: u64, size: usize) -> bool {
    if mapping.is_null()
        || unsafe { !(*mapping).iomem.is_null() }
        || size == 0
        || !axhal::boot::intel_cpu_mmap_ready()
        || !axmm::kernel_map_tlb_sync_installed()
    {
        return false;
    }
    let Some((physical_base, mapped_size, return_offset)) = physical_extent(base as usize, size)
    else {
        return false;
    };
    if !wc_physical_range_allowed(physical_base, mapped_size) {
        return false;
    }
    let Some((hint, limit)) = vmap_search_window() else {
        return false;
    };
    let Some((start, mapped)) = ({
        let aspace = axmm::kernel_aspace().lock();
        aspace
            .find_free_area(hint, mapped_size, limit)
            .map(|start| (start, mapped_size))
    }) else {
        return false;
    };

    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::WRITE_COMBINING;
    let mut installed = 0usize;
    let mut failed = false;
    {
        let mut aspace = axmm::kernel_aspace().lock();
        for offset in (0..mapped_size).step_by(PAGE_SIZE) {
            let va = VirtAddr::from_usize(start.as_usize() + offset);
            let pa = PhysAddr::from_usize(physical_base + offset);
            if aspace.map_linear(va, pa, PAGE_SIZE, flags).is_err() {
                failed = true;
                break;
            }
            installed += PAGE_SIZE;
        }
        if failed && installed != 0 {
            for offset in (0..installed).step_by(PAGE_SIZE) {
                let va = VirtAddr::from_usize(start.as_usize() + offset);
                assert!(aspace.unmap(va, PAGE_SIZE).is_ok());
            }
            assert!(aspace.reserve(start, mapped).is_ok());
        }
    }
    if failed {
        if installed != 0 {
            axmm::synchronize_kernel_map_tlb()
                .expect("kernel-map TLB synchronizer missing during WC iomap rollback");
            assert!(axmm::kernel_aspace().lock().unmap(start, mapped).is_ok());
        }
        return false;
    }
    if axmm::synchronize_kernel_map_tlb().is_err() {
        unsafe { unmap_partial(start, installed) };
        return false;
    }

    let returned = start.as_usize() + return_offset;
    let record = IoMappingRecord {
        physical_base,
        mapped_base: start.as_usize(),
        mapped_size,
        return_offset,
    };
    let inserted = with_records(|records| records.insert(returned, record).is_none());
    if !inserted {
        unsafe { unmap_partial(start, installed) };
        return false;
    }
    unsafe {
        (*mapping).base = base;
        (*mapping).size = size as _;
        (*mapping).prot = PAGE_KERNEL_WC;
        (*mapping).iomem = returned as *mut _;
    }
    true
}

/// Unmap a successful `io_mapping_init_wc()` after the shared-map TLB grace.
pub unsafe fn io_mapping_fini(mapping: *mut IoMapping) {
    if mapping.is_null() || unsafe { (*mapping).iomem.is_null() } {
        return;
    }
    let returned = unsafe { (*mapping).iomem as usize };
    let record = with_records(|records| records.get(&returned).copied())
        .expect("io_mapping_fini received an unowned WC mapping");
    assert_eq!(
        record.physical_base.checked_add(record.return_offset),
        Some(unsafe { (*mapping).base as usize })
    );
    assert!(
        unsafe { (*mapping).size as usize }
            .checked_add(record.return_offset)
            .is_some_and(|size| size <= record.mapped_size)
    );
    let start = VirtAddr::from_usize(record.mapped_base);
    {
        let mut aspace = axmm::kernel_aspace().lock();
        for offset in (0..record.mapped_size).step_by(PAGE_SIZE) {
            let va = VirtAddr::from_usize(record.mapped_base + offset);
            assert!(aspace.unmap(va, PAGE_SIZE).is_ok());
        }
        assert!(aspace.reserve(start, record.mapped_size).is_ok());
    }
    axmm::synchronize_kernel_map_tlb()
        .expect("kernel-map TLB synchronizer missing during WC iomap teardown");
    assert!(with_records(|records| records.remove(&returned)).is_some());
    assert!(
        axmm::kernel_aspace()
            .lock()
            .unmap(start, record.mapped_size)
            .is_ok()
    );
    unsafe {
        (*mapping).base = 0;
        (*mapping).size = 0;
        (*mapping).iomem = ptr::null_mut();
    }
}

/// Permanent-offset accessor matching the configured Linux io_mapping path.
pub unsafe fn io_mapping_map_wc(mapping: *mut IoMapping, offset: usize) -> *mut u8 {
    assert!(!mapping.is_null());
    let size = unsafe { (*mapping).size as usize };
    let base = unsafe { (*mapping).iomem.cast::<u8>() };
    assert!(!base.is_null() && offset < size);
    unsafe { base.add(offset) }
}

#[cfg(test)]
mod tests {
    use super::physical_extent;

    #[test]
    fn wc_iomap_extent_preserves_byte_offset_and_rounds_pages() {
        assert_eq!(
            physical_extent(0x1234, 0x2000),
            Some((0x1000, 0x3000, 0x234))
        );
        assert_eq!(physical_extent(0x2000, 0x1000), Some((0x2000, 0x1000, 0)));
        assert_eq!(physical_extent(0x2000, 0), None);
        assert_eq!(physical_extent(usize::MAX - 2, 8), None);
    }
}
