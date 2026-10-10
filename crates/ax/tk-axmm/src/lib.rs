//! [ArceOS](https://github.com/arceos-org/arceos) memory management module.

#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;
#[cfg(test)]
extern crate std;

mod aspace;
mod backend;

use core::sync::atomic::{AtomicUsize, Ordering};

use axerrno::{AxError, AxResult, ax_err};
use axhal::{
    mem::{MemRegionFlags, phys_to_virt},
    paging::MappingFlags,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr};

pub use self::{aspace::AddrSpace, backend::Backend};

static KERNEL_ASPACE: LazyInit<SpinNoIrq<AddrSpace>> = LazyInit::new();

/// Installed owner callback for a synchronous all-CPU invalidation of shared
/// kernel mappings. Until the kernel's TLB subsystem installs its callback,
/// clients that need to reclaim or expose shared mappings must fail closed.
pub type KernelMapTlbSync = unsafe extern "C" fn();

static KERNEL_MAP_TLB_SYNC: AtomicUsize = AtomicUsize::new(0);

/// Install the single kernel owner for synchronous shared-map TLB invalidation.
/// Re-installing the same callback is idempotent; replacing the owner is refused.
pub fn install_kernel_map_tlb_sync(sync: KernelMapTlbSync) -> AxResult {
    let address = sync as *const () as usize;
    if address == 0 {
        return ax_err!(InvalidInput, "null kernel-map TLB synchronizer");
    }
    match KERNEL_MAP_TLB_SYNC.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(existing) if existing == address => Ok(()),
        Err(_) => ax_err!(
            AlreadyExists,
            "kernel-map TLB synchronizer already installed"
        ),
    }
}

/// Whether the kernel owner has installed the shared-map invalidation service.
#[inline]
pub fn kernel_map_tlb_sync_installed() -> bool {
    KERNEL_MAP_TLB_SYNC.load(Ordering::Acquire) != 0
}

/// Synchronously invalidate shared kernel mappings on every active CPU.
///
/// Returns an error rather than performing a local-only flush if the kernel
/// TLB owner has not yet installed its acknowledged shootdown callback.
pub fn synchronize_kernel_map_tlb() -> AxResult {
    let address = KERNEL_MAP_TLB_SYNC.load(Ordering::Acquire);
    if address == 0 {
        return ax_err!(BadState, "kernel-map TLB synchronizer is not installed");
    }
    // Function pointers are code addresses on the supported x86_64 target.
    let sync: KernelMapTlbSync = unsafe { core::mem::transmute(address) };
    unsafe { sync() };
    Ok(())
}

fn reg_flag_to_map_flag(f: MemRegionFlags) -> MappingFlags {
    let mut ret = MappingFlags::empty();
    if f.contains(MemRegionFlags::READ) {
        ret |= MappingFlags::READ;
    }
    if f.contains(MemRegionFlags::WRITE) {
        ret |= MappingFlags::WRITE;
    }
    if f.contains(MemRegionFlags::EXECUTE) {
        ret |= MappingFlags::EXECUTE;
    }
    if f.contains(MemRegionFlags::DEVICE) {
        ret |= MappingFlags::DEVICE;
    }
    if f.contains(MemRegionFlags::UNCACHED) {
        ret |= MappingFlags::UNCACHED;
    }
    ret
}

#[cfg(feature = "copy")]
/// Creates a new address space for user processes.
pub fn new_user_aspace(base: VirtAddr, size: usize) -> AxResult<AddrSpace> {
    let mut aspace = AddrSpace::new_empty(base, size)?;
    aspace.copy_mappings_from(&kernel_aspace().lock())?;
    Ok(aspace)
}

/// Creates a new address space for kernel itself.
pub fn new_kernel_aspace() -> AxResult<AddrSpace> {
    let (base, size) = axhal::mem::kernel_aspace();
    let mut aspace = AddrSpace::new_empty(base, size)?;
    for r in axhal::mem::memory_regions() {
        // mapped range should contain the whole region if it is not aligned.
        let start = r.paddr.align_down_4k();
        let end = (r.paddr + r.size).align_up_4k();
        aspace.map_linear(
            phys_to_virt(start),
            start,
            end - start,
            reg_flag_to_map_flag(r.flags),
        )?;
    }
    Ok(aspace)
}

/// Returns the globally unique kernel address space.
pub fn kernel_aspace() -> &'static SpinNoIrq<AddrSpace> {
    &KERNEL_ASPACE
}

/// Returns the root physical address of the kernel page table.
pub fn kernel_page_table_root() -> PhysAddr {
    KERNEL_ASPACE.lock().page_table_root()
}

/// Initializes virtual memory management.
///
/// It mainly sets up the kernel virtual memory address space and recreate a
/// fine-grained kernel page table.
pub fn init_memory_management() {
    info!("Initialize virtual memory management...");

    let kernel_aspace = new_kernel_aspace().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {kernel_aspace:#x?}");
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_aspace));
    unsafe {
        // KERNEL_ASPACE is static and owns this hierarchy until shutdown.
        // Bind task creation before the scheduler can publish any workers.
        axhal::asm::bind_kernel_task_page_table_root(kernel_page_table_root());
        axhal::asm::write_kernel_page_table(kernel_page_table_root());
        axhal::asm::flush_tlb(None);
    }
}

/// Initializes kernel paging for secondary CPUs.
pub fn init_memory_management_secondary() {
    unsafe {
        axhal::asm::write_kernel_page_table(kernel_page_table_root());
        axhal::asm::flush_tlb(None);
    }
}

/// Maps a physical memory region to virtual address space for device access.
pub fn iomap(addr: PhysAddr, size: usize) -> AxResult<VirtAddr> {
    let (addr_aligned, size_aligned) = iomap_extent(addr, size)?;
    let virt = phys_to_virt(addr);
    let virt_aligned = virt.align_down_4k();
    map_iomap_range(
        &mut kernel_aspace().lock(),
        virt_aligned,
        addr_aligned,
        size_aligned,
    )?;
    Ok(virt)
}

fn iomap_extent(addr: PhysAddr, size: usize) -> AxResult<(PhysAddr, usize)> {
    if size == 0 {
        return Err(AxError::InvalidInput);
    }
    let end = addr
        .as_usize()
        .checked_add(size)
        .and_then(|end| end.checked_add(PAGE_SIZE_4K - 1))
        .ok_or(AxError::InvalidInput)?
        & !(PAGE_SIZE_4K - 1);
    let start = addr.align_down_4k();
    Ok((start, end - start.as_usize()))
}

fn map_iomap_range(
    tb: &mut AddrSpace,
    virt_aligned: VirtAddr,
    addr_aligned: PhysAddr,
    size_aligned: usize,
) -> AxResult {
    let flags = MappingFlags::DEVICE | MappingFlags::READ | MappingFlags::WRITE;
    match tb.map_linear(virt_aligned, addr_aligned, size_aligned, flags) {
        Err(AxError::AlreadyExists) => {
            // MemorySet rejects the whole range before mapping any pages if
            // even one area overlaps. Fill holes left by earlier short maps
            // (e.g. the OpRegion header followed by its complete mailboxes).
            for offset in (0..size_aligned).step_by(PAGE_SIZE_4K) {
                let virt = virt_aligned + offset;
                let physical = addr_aligned + offset;
                match tb.map_linear(virt, physical, PAGE_SIZE_4K, flags) {
                    Ok(()) => {}
                    Err(AxError::AlreadyExists) => {
                        // Reusing a different physical mapping is not iomap:
                        // do not silently change its permissions or return it.
                        if tb.query_leaf(virt)?.0 != physical {
                            return Err(AxError::AlreadyExists);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    // Existing direct mappings need the same device attributes and TLB
    // invalidation as pages newly added above.
    tb.protect(virt_aligned, size_aligned, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address_space() -> AddrSpace {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let (start, size) = axhal::mem::phys_ram_ranges()[0];
            axalloc::global_init(start, size);
        });
        AddrSpace::new_empty(VirtAddr::from_usize(0x100000), 0x100000).unwrap()
    }

    fn check_pages(space: &AddrSpace, start: usize, pages: usize) {
        for offset in (0..pages * PAGE_SIZE_4K).step_by(PAGE_SIZE_4K) {
            let (physical, flags, _) = space
                .query_leaf(VirtAddr::from_usize(start + offset))
                .unwrap();
            assert_eq!(physical.as_usize(), start + offset);
            assert!(
                flags.contains(MappingFlags::UNCACHED | MappingFlags::READ | MappingFlags::WRITE)
            );
        }
    }

    #[test]
    fn iomap_extends_a_short_opregion_mapping_without_leaving_holes() {
        let mut space = address_space();
        let start = 0x100000;
        map_iomap_range(&mut space, start.into(), start.into(), 2 * PAGE_SIZE_4K).unwrap();
        assert!(space.query_leaf((start + 2 * PAGE_SIZE_4K).into()).is_err());
        map_iomap_range(&mut space, start.into(), start.into(), 3 * PAGE_SIZE_4K).unwrap();
        check_pages(&space, start, 3);
        map_iomap_range(&mut space, start.into(), start.into(), PAGE_SIZE_4K).unwrap();
        check_pages(&space, start, 3);
    }

    #[test]
    fn iomap_fills_prefix_and_middle_holes_between_existing_areas() {
        let mut space = address_space();
        let start = 0x110000;
        for page in [1, 3] {
            let addr = start + page * PAGE_SIZE_4K;
            map_iomap_range(&mut space, addr.into(), addr.into(), PAGE_SIZE_4K).unwrap();
        }
        map_iomap_range(&mut space, start.into(), start.into(), 5 * PAGE_SIZE_4K).unwrap();
        check_pages(&space, start, 5);
    }

    #[test]
    fn iomap_does_not_reuse_a_foreign_mapping() {
        let mut space = address_space();
        let start = VirtAddr::from_usize(0x120000);
        let physical = PhysAddr::from_usize(0x110000);
        space
            .map_linear(start, physical, PAGE_SIZE_4K, MappingFlags::READ)
            .unwrap();
        assert_eq!(
            map_iomap_range(&mut space, start, start.as_usize().into(), PAGE_SIZE_4K),
            Err(AxError::AlreadyExists)
        );
        assert_eq!(space.query_leaf(start).unwrap().0, physical);
        assert_eq!(space.query_leaf(start).unwrap().1, MappingFlags::READ);
    }

    #[test]
    fn iomap_extent_checks_zero_overflow_and_unaligned_ends() {
        assert_eq!(
            iomap_extent(0x1234.into(), 0x2000),
            Ok((0x1000.into(), 0x3000))
        );
        assert_eq!(
            iomap_extent(0x1000.into(), PAGE_SIZE_4K),
            Ok((0x1000.into(), PAGE_SIZE_4K))
        );
        assert_eq!(iomap_extent(0x1000.into(), 0), Err(AxError::InvalidInput));
        assert_eq!(
            iomap_extent((usize::MAX - 2).into(), 4),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            iomap_extent((usize::MAX - 2).into(), 1),
            Err(AxError::InvalidInput)
        );
    }
}
