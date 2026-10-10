// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! LinuxKPI vmalloc/vmap primitives backed by the shared axmm kernel address
//! space. The kernel owner installs the acknowledged all-CPU TLB callback;
//! operations fail closed until that callback is ready.

#![allow(unsafe_code, non_camel_case_types, non_snake_case)]

use alloc::collections::BTreeMap;
use core::{
    ffi::{c_ulong, c_void},
    ptr,
};

use axhal::paging::MappingFlags;
use kernel_guard::NoPreemptIrqSave;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};
use spin::Mutex;

use crate::{
    i915_gem_object_types_upstream::Page,
    linux::{
        config::PAGE_SIZE,
        gem_memory::PgProt,
        memory::{kfree, kvmalloc_objs},
        shmem::{put_page, try_page_to_phys},
    },
};

/// Linux `VM_MAP_PUT_PAGES`: successful `vmap` owns both page references and
/// the page-pointer array until `vfree`.
pub const VM_MAP_PUT_PAGES: c_ulong = 0x0000_0200;

/// x86_64 `PAGE_KERNEL` pgprot from the target configuration used by i915.
/// Cache mode is write-back; executable permission is explicitly disabled.
/// The generic axmm flags do not emit `_PAGE_GLOBAL`; this conservatively uses
/// non-global supervisor RW/NX/WB entries, preserving access semantics while
/// foregoing only a TLB caching optimization.
pub const PAGE_KERNEL: PgProt = PgProt {
    pgprot: ((1usize << 0)
        | (1usize << 1)
        | (1usize << 5)
        | (1usize << 6)
        | (1usize << 8)
        | (1usize << 63)) as c_ulong,
};

/// x86 `pgprot_writecombine(PAGE_KERNEL)`. The target installs WC at PAT
/// index 1, encoded by PWT=1 for 4 KiB leaf entries; admitting this mode also
/// requires every startup CPU to have confirmed that palette.
pub const PAGE_KERNEL_WC: PgProt = PgProt {
    pgprot: PAGE_KERNEL.pgprot | (1u64 << 3),
};

const X86_64_PML4_SLOT_SIZE: usize = 1usize << 39;

#[derive(Clone, Copy)]
struct VmapRecord {
    page_array: usize,
    page_count: usize,
    put_pages: bool,
    retiring: bool,
}

static VMAP_RECORDS: Mutex<BTreeMap<usize, VmapRecord>> = Mutex::new(BTreeMap::new());

fn with_vmap_records<T>(f: impl FnOnce(&mut BTreeMap<usize, VmapRecord>) -> T) -> T {
    let _irq_guard = NoPreemptIrqSave::new();
    let mut records = VMAP_RECORDS.lock();
    f(&mut records)
}

fn vmap_search_window() -> Option<(VirtAddr, VirtAddrRange)> {
    let mut max_phys_end = 0usize;
    for region in axhal::mem::memory_regions() {
        let end = region.paddr.as_usize().checked_add(region.size)?;
        max_phys_end = max_phys_end.max(end);
    }

    let (base, size) = axhal::mem::kernel_aspace();
    let end = base.as_usize().checked_add(size)?;
    let slot_base = base.as_usize() & !(X86_64_PML4_SLOT_SIZE - 1);
    // User roots copy the kernel's top-level entry at address-space creation.
    // Stay inside the slot containing the always-present kernel image so a
    // later vmap PTE remains visible through those already-shared subtrees.
    if slot_base != base.as_usize() {
        return None;
    }
    let slot_end = slot_base.checked_add(X86_64_PML4_SLOT_SIZE)?;
    let limit_end = end.min(slot_end);
    let direct_end = axhal::mem::phys_to_virt(PhysAddr::from_usize(max_phys_end)).as_usize();
    let aligned_direct_end = direct_end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    let hint = aligned_direct_end.checked_add(PAGE_SIZE)?;
    if hint >= limit_end {
        return None;
    }
    Some((
        VirtAddr::from_usize(hint),
        VirtAddrRange::from_start_size(base, limit_end - base.as_usize()),
    ))
}

fn release_page_array(pages: *mut *mut Page, count: usize) {
    for index in 0..count {
        let page = unsafe { pages.add(index).read() };
        unsafe { put_page(page) };
    }
    unsafe { kfree(pages) };
}

/// Linux `vmap()` for the LinuxKPI-owned kernel address space. This target
/// intentionally supports WB `PAGE_KERNEL` and the explicitly enabled PAT1/WC
/// form of `pgprot_writecombine(PAGE_KERNEL)`, plus the two source flag forms
/// used by i915 (`0` and `VM_MAP_PUT_PAGES`). UC and unknown pgprots fail
/// closed. Unsupported pgprots or flags fail without consuming the caller's
/// page references or array. Axmm's generic PTE flags encode the same
/// supervisor RW, NX, and WB/WC cache mode; the source `_PAGE_GLOBAL` bit is
/// omitted because its only effect here is a TLB caching optimization, while
/// axmm's shared kernel root is already common to all task address spaces.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmap(
    pages: *mut *mut Page,
    count: u32,
    flags: c_ulong,
    prot: PgProt,
) -> *mut c_void {
    let wc = prot.pgprot == PAGE_KERNEL_WC.pgprot;
    if pages.is_null()
        || count == 0
        || (flags != 0 && flags != VM_MAP_PUT_PAGES)
        || (!wc && prot.pgprot != PAGE_KERNEL.pgprot)
        || (wc && !axhal::boot::intel_cpu_mmap_ready())
        || !axmm::kernel_map_tlb_sync_installed()
    {
        return ptr::null_mut();
    }
    let Some(size) = (count as usize).checked_mul(PAGE_SIZE) else {
        return ptr::null_mut();
    };
    let Some((hint, limit)) = vmap_search_window() else {
        return ptr::null_mut();
    };

    // Resolve and validate page identities before taking the shared kernel
    // address-space lock; this avoids nesting its lock with the shmem registry.
    let physical_pages = kvmalloc_objs::<usize, usize>(count as usize);
    if physical_pages.is_null() {
        return ptr::null_mut();
    }
    for index in 0..count as usize {
        let page = unsafe { pages.add(index).read() };
        if page.is_null() || crate::linux_config::IS_ERR(page) {
            unsafe { kfree(physical_pages) };
            return ptr::null_mut();
        }
        let Some(physical) = (unsafe { try_page_to_phys(page) }) else {
            unsafe { kfree(physical_pages) };
            return ptr::null_mut();
        };
        unsafe { physical_pages.add(index).write(physical) };
    }

    let mut mapped = 0usize;
    let mut map_failed = false;
    let start = {
        let mut aspace = axmm::kernel_aspace().lock();
        let Some(start) = aspace.find_free_area(hint, size, limit) else {
            unsafe { kfree(physical_pages) };
            return ptr::null_mut();
        };
        for index in 0..count as usize {
            let physical = unsafe { physical_pages.add(index).read() };
            let paddr = PhysAddr::from_usize(physical);
            let vaddr = VirtAddr::from_usize(start.as_usize() + index * PAGE_SIZE);
            if aspace
                .map_linear(
                    vaddr,
                    paddr,
                    PAGE_SIZE,
                    MappingFlags::READ
                        | MappingFlags::WRITE
                        | if wc {
                            MappingFlags::WRITE_COMBINING
                        } else {
                            MappingFlags::empty()
                        },
                )
                .is_err()
            {
                map_failed = true;
                break;
            }
            mapped += 1;
        }
        if map_failed && mapped != 0 {
            for index in 0..mapped {
                let vaddr = VirtAddr::from_usize(start.as_usize() + index * PAGE_SIZE);
                assert!(
                    aspace.unmap(vaddr, PAGE_SIZE).is_ok(),
                    "partial vmap rollback lost a mapped page"
                );
            }
            assert!(
                aspace.reserve(start, size).is_ok(),
                "failed to reserve a partially unmapped vmap range"
            );
        }
        start
    };
    unsafe { kfree(physical_pages) };

    if map_failed {
        if mapped != 0 {
            axmm::synchronize_kernel_map_tlb()
                .expect("kernel-map TLB synchronizer disappeared during vmap rollback");
            assert!(
                axmm::kernel_aspace().lock().unmap(start, size).is_ok(),
                "failed to release partial-vmap address reservation"
            );
        }
        return ptr::null_mut();
    }

    axmm::synchronize_kernel_map_tlb().expect("kernel-map TLB synchronizer disappeared after vmap");
    let address = start.as_usize();
    let put_pages = flags == VM_MAP_PUT_PAGES;
    with_vmap_records(|records| {
        assert!(
            records
                .insert(
                    address,
                    VmapRecord {
                        page_array: if put_pages { pages as usize } else { 0 },
                        page_count: count as usize,
                        put_pages,
                        retiring: false,
                    },
                )
                .is_none(),
            "vmap address reused while an earlier owner was registered"
        );
    });
    address as *mut c_void
}

/// Retire a region returned by [`vmap`]. The VA remains reserved between PTE
/// removal and the acknowledged global shootdown, and page refs are released
/// only after that grace period.
unsafe fn vunmap_inner(address: *mut c_void, put_pages_ok: bool) {
    if address.is_null() {
        return;
    }
    let base = address as usize;
    let record = with_vmap_records(|records| {
        let record = records
            .get_mut(&base)
            .expect("vfree received an address not owned by LinuxKPI vmap");
        assert!(!record.retiring, "vmap address is already being freed");
        assert!(
            put_pages_ok || !record.put_pages,
            "vunmap cannot release VM_MAP_PUT_PAGES ownership; use vfree"
        );
        record.retiring = true;
        *record
    });
    let size = record
        .page_count
        .checked_mul(PAGE_SIZE)
        .expect("recorded vmap size overflowed");
    let start = VirtAddr::from_usize(base);

    {
        let mut aspace = axmm::kernel_aspace().lock();
        for index in 0..record.page_count {
            let vaddr = VirtAddr::from_usize(base + index * PAGE_SIZE);
            assert!(
                aspace.unmap(vaddr, PAGE_SIZE).is_ok(),
                "vfree lost a mapped page"
            );
        }
        assert!(
            aspace.reserve(start, size).is_ok(),
            "failed to reserve vfree range until TLB grace"
        );
    }

    axmm::synchronize_kernel_map_tlb()
        .expect("kernel-map TLB synchronizer disappeared during vfree");

    let removed = with_vmap_records(|records| records.remove(&base));
    assert!(
        removed.is_some_and(|old| old.retiring),
        "vfree registry changed during shootdown"
    );
    assert!(
        axmm::kernel_aspace().lock().unmap(start, size).is_ok(),
        "failed to release vfree address reservation"
    );

    if record.put_pages {
        release_page_array(record.page_array as *mut *mut Page, record.page_count);
    }
}

/// Linux `vfree()` for a region returned by [`vmap`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfree(address: *mut c_void) {
    unsafe { vunmap_inner(address, true) };
}

/// Linux `vunmap()` for a `vmap()` region whose caller retains page ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vunmap(address: *mut c_void) {
    unsafe { vunmap_inner(address, false) };
}
