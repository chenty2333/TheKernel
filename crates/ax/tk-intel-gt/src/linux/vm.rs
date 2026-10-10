// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! LinuxKPI vmalloc/vmap primitives backed by the shared axmm kernel address
//! space. The kernel owner installs the acknowledged all-CPU TLB callback;
//! operations fail closed until that callback is ready.

#![allow(unsafe_code, non_camel_case_types, non_snake_case)]

use alloc::collections::BTreeMap;
use core::{
    ffi::{c_int, c_ulong, c_void},
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

/// Validate a `vmap()`-family pgprot. Returns `Some(wc)` for the two admitted
/// forms (`PAGE_KERNEL` and its PAT1/WC variant) when that mode is usable;
/// anything else (UC, unknown bits) is refused.
fn admitted_kernel_prot(prot: PgProt) -> Option<bool> {
    if prot.pgprot == PAGE_KERNEL_WC.pgprot {
        axhal::boot::intel_cpu_mmap_ready().then_some(true)
    } else if prot.pgprot == PAGE_KERNEL.pgprot {
        Some(false)
    } else {
        None
    }
}

/// Map `count` physical frames (`frames[i]` is page-aligned) into the kernel
/// VA window, optionally with PAT1/WC leaves, and return the base address.
/// Caller keeps ownership of `frames`. The mapping is complete only after the
/// shared kernel-map TLB grace period.
unsafe fn map_frames(frames: *const usize, count: usize, wc: bool) -> Option<usize> {
    let size = count.checked_mul(PAGE_SIZE)?;
    let (hint, limit) = vmap_search_window()?;

    let mut mapped = 0usize;
    let mut map_failed = false;
    let start = {
        let mut aspace = axmm::kernel_aspace().lock();
        let start = aspace.find_free_area(hint, size, limit)?;
        for index in 0..count {
            let physical = unsafe { frames.add(index).read() };
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

    if map_failed {
        if mapped != 0 {
            axmm::synchronize_kernel_map_tlb()
                .expect("kernel-map TLB synchronizer disappeared during vmap rollback");
            assert!(
                axmm::kernel_aspace().lock().unmap(start, size).is_ok(),
                "failed to release partial-vmap address reservation"
            );
        }
        return None;
    }

    axmm::synchronize_kernel_map_tlb().expect("kernel-map TLB synchronizer disappeared after vmap");
    Some(start.as_usize())
}

/// Record a region produced by [`map_frames`] so `vfree`/`vunmap` can retire
/// it. `page_array` is only meaningful when `put_pages` is set.
fn record_vmap_region(address: usize, count: usize, page_array: usize, put_pages: bool) {
    with_vmap_records(|records| {
        assert!(
            records
                .insert(
                    address,
                    VmapRecord {
                        page_array: if put_pages { page_array } else { 0 },
                        page_count: count,
                        put_pages,
                        retiring: false,
                    },
                )
                .is_none(),
            "vmap address reused while an earlier owner was registered"
        );
    });
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
    let Some(wc) = admitted_kernel_prot(prot) else {
        return ptr::null_mut();
    };
    if pages.is_null()
        || count == 0
        || (flags != 0 && flags != VM_MAP_PUT_PAGES)
        || !axmm::kernel_map_tlb_sync_installed()
    {
        return ptr::null_mut();
    }

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

    let mapped = unsafe { map_frames(physical_pages, count as usize, wc) };
    unsafe { kfree(physical_pages) };
    let Some(address) = mapped else {
        return ptr::null_mut();
    };
    record_vmap_region(
        address,
        count as usize,
        pages as usize,
        flags == VM_MAP_PUT_PAGES,
    );
    address as *mut c_void
}

/// Linux `vmap_pfn(pfns, count, prot)`: map raw page-frame numbers (no page
/// references are taken). Released with `vunmap`/`vfree`, which then leave
/// the frames untouched. Accepts the same pgprot forms as [`vmap`].
///
/// # Safety
/// `pfns` must hold `count` frame numbers the caller is allowed to map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vmap_pfn(pfns: *mut c_ulong, count: u32, prot: PgProt) -> *mut c_void {
    let Some(wc) = admitted_kernel_prot(prot) else {
        return ptr::null_mut();
    };
    if pfns.is_null() || count == 0 || !axmm::kernel_map_tlb_sync_installed() {
        return ptr::null_mut();
    }
    let frames = kvmalloc_objs::<usize, usize>(count as usize);
    if frames.is_null() {
        return ptr::null_mut();
    }
    for index in 0..count as usize {
        let pfn = unsafe { pfns.add(index).read() } as usize;
        let Some(physical) = pfn.checked_mul(PAGE_SIZE) else {
            unsafe { kfree(frames) };
            return ptr::null_mut();
        };
        unsafe { frames.add(index).write(physical) };
    }
    let mapped = unsafe { map_frames(frames, count as usize, wc) };
    unsafe { kfree(frames) };
    let Some(address) = mapped else {
        return ptr::null_mut();
    };
    record_vmap_region(address, count as usize, 0, false);
    address as *mut c_void
}

/// Linux `is_vmalloc_addr(ptr)` for the regions this owner maps: true when
/// `ptr` lies inside a live `vmap()`/`vmap_pfn()` region.
///
/// # Safety
/// Only compares addresses; never dereferences `ptr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn is_vmalloc_addr(ptr: *const c_void) -> bool {
    let address = ptr as usize;
    with_vmap_records(|records| {
        records
            .range(..=address)
            .next_back()
            .is_some_and(|(base, record)| {
                address < base.saturating_add(record.page_count * PAGE_SIZE)
            })
    })
}

/// x86 `cachemode2protval()` for the PAT palette this kernel programs
/// (index = PAT<<2 | PCD<<1 | PWT: 0 WB, 1 WC, 2 UC-, 3 UC, 5 WT). Cache mode
/// numbering follows Linux `_PAGE_CACHE_MODE_*`. Modes outside the palette
/// (WP) are refused with a panic rather than encoding an unintended cache type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cachemode2protval(cache_mode: i32) -> c_ulong {
    const PWT: c_ulong = 1 << 3;
    const PCD: c_ulong = 1 << 4;
    const PAT: c_ulong = 1 << 7;
    match cache_mode {
        0 => 0,         // _PAGE_CACHE_MODE_WB: PAT index 0
        1 => PWT,       // _PAGE_CACHE_MODE_WC: PAT index 1
        2 => PCD,       // _PAGE_CACHE_MODE_UC_MINUS: PAT index 2
        3 => PCD | PWT, // _PAGE_CACHE_MODE_UC: PAT index 3
        4 => PAT | PWT, // _PAGE_CACHE_MODE_WT: PAT index 5
        other => panic!("cachemode2protval: cache mode {other} is not in the PAT palette"),
    }
}

/// x86 `pgprot_writecombine(prot)`: the source protection with the WC cache
/// mode bits from [`cachemode2protval`] applied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pgprot_writecombine(prot: PgProt) -> PgProt {
    PgProt {
        pgprot: prot.pgprot | unsafe { cachemode2protval(1) },
    }
}

/// Linux `pat_enabled()`: true once the WC palette is confirmed on every
/// startup CPU, which is the only state in which WC leaves are emitted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pat_enabled() -> bool {
    axhal::boot::intel_cpu_mmap_ready()
}

/// Linux `arch_phys_wc_add(base, size)`: with PAT enabled the WC attribute is
/// carried by page-table bits and no MTRR entry is needed, so this returns 0
/// (the Linux PAT-path result). Without PAT there is no MTRR driver, so the
/// request fails with -ENODEV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_phys_wc_add(_base: u64, _size: u64) -> c_int {
    if unsafe { pat_enabled() } {
        0
    } else {
        -crate::linux_config::ENODEV
    }
}

/// Linux `arch_phys_wc_del(handle)`: releases an MTRR entry. TheKernel never
/// creates MTRR entries (see [`arch_phys_wc_add`]), so there is nothing to
/// release for any handle this owner returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_phys_wc_del(_mtrr: c_int) {}

/// Linux `boot_cpu_data.x86_clflush_size`: the CLFLUSH line size in bytes,
/// from CPUID leaf 1 EBX[15:8] (in 8-byte units).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boot_cpu_data_clflush_size() -> usize {
    let leaf1 = unsafe { core::arch::x86_64::__cpuid(1) };
    (((leaf1.ebx >> 8) & 0xff) as usize) * 8
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

#[cfg(test)]
mod cache_mode_tests {
    use super::*;

    #[test]
    fn writecombine_of_kernel_prot_is_the_pat1_variant() {
        let wc = unsafe { pgprot_writecombine(PAGE_KERNEL) };
        assert_eq!(wc.pgprot, PAGE_KERNEL_WC.pgprot);
    }

    #[test]
    fn cache_modes_map_to_palette_indices() {
        unsafe {
            assert_eq!(cachemode2protval(0), 0);
            assert_eq!(cachemode2protval(1), 1 << 3);
            assert_eq!(cachemode2protval(2), 1 << 4);
            assert_eq!(cachemode2protval(3), (1 << 4) | (1 << 3));
            assert_eq!(cachemode2protval(4), (1 << 7) | (1 << 3));
        }
    }

    #[test]
    fn unknown_vmap_pgprot_is_refused() {
        assert_eq!(admitted_kernel_prot(PgProt { pgprot: 0 }), None);
        assert_eq!(admitted_kernel_prot(PAGE_KERNEL), Some(false));
    }
}
