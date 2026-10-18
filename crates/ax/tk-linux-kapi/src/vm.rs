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
use memory_addr::{PhysAddr, VirtAddr, VirtAddrRange};
use spin::Mutex;

use crate::{
    types::Page,
    {
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

fn vmap_search_window_from_direct_end(
    direct_end: usize,
    kernel_base: usize,
    kernel_size: usize,
) -> Option<(VirtAddr, VirtAddrRange)> {
    let Some(end) = kernel_base.checked_add(kernel_size) else {
        return None;
    };
    let slot_base = kernel_base & !(X86_64_PML4_SLOT_SIZE - 1);
    // User roots copy the kernel's top-level entry at address-space creation.
    // Stay inside the slot containing the always-present kernel image so a
    // later vmap PTE remains visible through those already-shared subtrees.
    if slot_base != kernel_base {
        return None;
    }
    let slot_end = slot_base.checked_add(X86_64_PML4_SLOT_SIZE)?;
    let limit_end = end.min(slot_end);
    let aligned_direct_end = direct_end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    let hint = aligned_direct_end.checked_add(PAGE_SIZE)?;
    if hint >= limit_end {
        return None;
    }
    Some((
        VirtAddr::from_usize(hint),
        VirtAddrRange::from_start_size(
            VirtAddr::from_usize(kernel_base),
            limit_end.checked_sub(kernel_base)?,
        ),
    ))
}

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

#[cfg(not(target_os = "none"))]
fn vmap_search_window() -> Option<(VirtAddr, VirtAddrRange)> {
    // A host process has no kernel direct map or physical-memory inventory.
    None
}

#[cfg(target_os = "none")]
fn vmap_search_window() -> Option<(VirtAddr, VirtAddrRange)> {
    let mut max_ram_end = 0usize;
    for &(start, size) in axhal::mem::phys_ram_ranges() {
        let end = start.checked_add(size)?;
        max_ram_end = max_ram_end.max(end);
    }
    if max_ram_end == 0 {
        return None;
    }

    let (base, size) = axhal::mem::kernel_aspace();
    // Device MMIO can sit beyond the direct-mapped RAM and even outside the
    // kernel's shared PML4 slot. Only RAM ranges are direct-mapped here; using
    // `memory_regions()` would let a high MMIO aperture hide the whole vmap
    // search window.
    let direct_end = axhal::mem::phys_to_virt(PhysAddr::from_usize(max_ram_end)).as_usize();
    vmap_search_window_from_direct_end(direct_end, base.as_usize(), size)
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

/// Whether `[physical, physical + len)` lies inside one firmware-reported RAM
/// range. Page-backed GPU objects must never resolve to anything else.
pub fn phys_is_ram(physical: usize, len: usize) -> bool {
    let Some(end) = physical.checked_add(len) else {
        return false;
    };
    axhal::mem::phys_ram_ranges()
        .iter()
        .any(|&(start, size)| physical >= start && end <= start.saturating_add(size))
}

/// Fail-stop for a hardware access that would hit a non-RAM physical address
/// or leave its table. Spinning keeps the evidence on screen; letting the
/// write through has reset the whole N305 platform.
pub fn hw_guard_halt(args: core::fmt::Arguments<'_>) -> ! {
    loop {
        axlog::error!("HW GUARD: {args}; halting this CPU");
        let deadline = axhal::time::monotonic_time_nanos() + 10_000_000_000;
        while axhal::time::monotonic_time_nanos() < deadline {
            core::hint::spin_loop();
        }
    }
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
        if page.is_null() || crate::config::IS_ERR(page) {
            unsafe { kfree(physical_pages) };
            return ptr::null_mut();
        }
        let Some(physical) = (unsafe { try_page_to_phys(page) }) else {
            unsafe { kfree(physical_pages) };
            return ptr::null_mut();
        };
        if !phys_is_ram(physical, PAGE_SIZE) {
            hw_guard_halt(format_args!(
                "vmap page {index}/{count} page={page:p} phys={physical:#x} is not RAM"
            ));
        }
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

/// Linux `_PAGE_CACHE_MODE_*` numbering, as the i915 callers pass it.
fn cache_mode_from_linux(cache_mode: i32) -> Option<axmm::memtype::CacheMode> {
    usize::try_from(cache_mode)
        .ok()
        .and_then(|index| axmm::memtype::CacheMode::ALL.get(index).copied())
}

/// PTE cache bits (PWT, PCD, PAT) of a 4 KiB leaf.
const PTE_CACHE_BITS: c_ulong = (1 << 3) | (1 << 4) | (1 << 7);

/// Linux `pgprot_modify_cachemode`: replace the cache bits of a raw PTE
/// protection with the bits that select `mode` in `tables`.
fn pgprot_with_cache(prot: c_ulong, tables: &axmm::memtype::CacheTables, mode: axmm::memtype::CacheMode) -> c_ulong {
    (prot & !PTE_CACHE_BITS) | tables.pte_bits(mode) as c_ulong
}

/// Linux `cachemode2protval(mode)`: the 4 KiB PTE cache bits that select `mode`
/// in the palette this kernel is running with (`memtype::tables()`).
/// Without the PAT palette, WC falls back to UC- as Linux does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cachemode2protval(cache_mode: i32) -> c_ulong {
    let Some(mode) = cache_mode_from_linux(cache_mode) else {
        panic!("cachemode2protval: cache mode {cache_mode} does not exist");
    };
    axmm::memtype::tables().pte_bits(mode) as c_ulong
}

/// x86 `pgprot_writecombine(prot)`: the source protection with the WC cache
/// selection of the active palette (PAT1/PWT, or UC- without PAT).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pgprot_writecombine(prot: PgProt) -> PgProt {
    PgProt {
        pgprot: pgprot_with_cache(
            prot.pgprot,
            &axmm::memtype::tables(),
            axmm::memtype::CacheMode::Wc,
        ),
    }
}

/// Linux `pat_enabled()`: true once the WC palette is confirmed on every
/// startup CPU, which is the only state in which WC leaves are emitted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pat_enabled() -> bool {
    axmm::memtype::pat_enabled()
}

/// Linux `arch_phys_wc_add(base, size)`: with PAT the WC attribute is carried
/// by page-table bits, so no MTRR entry is needed and this returns 0. Without
/// PAT there is no MTRR driver, and the request fails with -ENODEV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_phys_wc_add(base: u64, size: u64) -> c_int {
    axmm::memtype::arch_phys_wc_add(base, size) as c_int
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
        let palette = axmm::memtype::CacheTables::from_pat(axmm::memtype::PAT_LINUX_FULL);
        let wc = pgprot_with_cache(PAGE_KERNEL.pgprot, &palette, axmm::memtype::CacheMode::Wc);
        assert_eq!(wc, PAGE_KERNEL_WC.pgprot);
        // Replacing the cache bits must not disturb the other protection bits.
        let uc = pgprot_with_cache(PAGE_KERNEL_WC.pgprot, &palette, axmm::memtype::CacheMode::Uc);
        assert_eq!(uc, PAGE_KERNEL.pgprot | (1 << 3) | (1 << 4));
    }

    #[test]
    fn cache_modes_map_to_linux_full_palette_indices() {
        use axmm::memtype::{CacheMode, CacheTables, PAT_LINUX_FULL};
        let palette = CacheTables::from_pat(PAT_LINUX_FULL);
        // Index 0 WB, 1 WC (PWT), 2 UC- (PCD), 3 UC (PCD|PWT), 7 WT (PAT|PCD|PWT).
        assert_eq!(palette.pte_bits(CacheMode::Wb), 0);
        assert_eq!(palette.pte_bits(CacheMode::Wc), 1 << 3);
        assert_eq!(palette.pte_bits(CacheMode::UcMinus), 1 << 4);
        assert_eq!(palette.pte_bits(CacheMode::Uc), (1 << 4) | (1 << 3));
        assert_eq!(palette.pte_bits(CacheMode::Wt), (1 << 7) | (1 << 4) | (1 << 3));
    }

    #[test]
    fn cachemode_numbering_is_linux_and_bounded() {
        assert_eq!(cache_mode_from_linux(1), Some(axmm::memtype::CacheMode::Wc));
        assert_eq!(cache_mode_from_linux(5), Some(axmm::memtype::CacheMode::Wp));
        assert_eq!(cache_mode_from_linux(6), None);
        assert_eq!(cache_mode_from_linux(-1), None);
    }

    #[test]
    fn unknown_vmap_pgprot_is_refused() {
        assert_eq!(admitted_kernel_prot(PgProt { pgprot: 0 }), None);
        assert_eq!(admitted_kernel_prot(PAGE_KERNEL), Some(false));
    }

    #[test]
    fn high_mmio_extent_does_not_hide_vmap_window() {
        const KERNEL_BASE: usize = 0xffff_8000_0000_0000;
        const KERNEL_SIZE: usize = 0x0000_7fff_ffff_f000;
        const RAM_END: usize = 0x2000_0000;
        // This models the QEMU PCI aperture at [0xc000000000, 0xc001000000),
        // whose high endpoint is outside the kernel's first 512-GiB PML4 slot.
        const HIGH_MMIO_END: usize = 0xc001_0000_00;

        let ram_direct_end = KERNEL_BASE + RAM_END;
        let mmio_direct_end = KERNEL_BASE + HIGH_MMIO_END;
        assert!(vmap_search_window_from_direct_end(ram_direct_end, KERNEL_BASE, KERNEL_SIZE).is_some());
        assert!(vmap_search_window_from_direct_end(mmio_direct_end, KERNEL_BASE, KERNEL_SIZE).is_none());

        // Both the address-space end and page-alignment arithmetic fail
        // closed instead of wrapping into an invalid vmap range.
        assert!(vmap_search_window_from_direct_end(ram_direct_end, usize::MAX, 1).is_none());
        assert!(vmap_search_window_from_direct_end(usize::MAX, KERNEL_BASE, KERNEL_SIZE).is_none());
    }
}
