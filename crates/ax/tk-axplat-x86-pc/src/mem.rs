//! Physical memory information.

use axplat::mem::{MemIf, PhysAddr, RawRange, VirtAddr, pa, va};
use heapless::Vec;
use lazyinit::LazyInit;
use multiboot::information::{MemoryManagement, MemoryType, Multiboot, PAddr};

use crate::{
    boot_info::{self, BootProtocol},
    config::{devices::MMIO_RANGES, plat::PHYS_VIRT_OFFSET},
};

const MAX_REGIONS: usize = boot_info::MAX_REGIONS;
const MAX_RESERVED_RANGES: usize = 1 + boot_info::MAX_MODULES;

static RUNTIME_MMIO: LazyInit<Vec<RawRange, 32>> = LazyInit::new();

static RAM_REGIONS: LazyInit<Vec<RawRange, MAX_REGIONS>> = LazyInit::new();
static RESERVED_RAM_REGIONS: LazyInit<Vec<RawRange, MAX_RESERVED_RANGES>> = LazyInit::new();

pub(crate) fn ram_regions() -> &'static [RawRange] {
    RAM_REGIONS.as_slice()
}

pub fn init() {
    let boot_info = boot_info::get();
    let mut regions = Vec::new();
    match boot_info.protocol() {
        BootProtocol::Multiboot2 => {
            for &region in boot_info.memory_regions() {
                regions
                    .push(region)
                    .expect("too many Multiboot2 memory regions");
            }
        }
        // Keep the established Multiboot1 parser and its bootloader-specific
        // memory-management adapter intact.  Only MB2 uses the owned copy.
        BootProtocol::Multiboot1 => {
            let mut mm = MemIfImpl;
            let info = unsafe {
                Multiboot::from_ptr(boot_info.info_paddr() as _, &mut mm)
                    .expect("invalid Multiboot1 information")
            };
            for r in info
                .memory_regions()
                .expect("missing Multiboot1 memory map")
            {
                if r.memory_type() == MemoryType::Available {
                    regions
                        .push((r.base_address() as usize, r.length() as usize))
                        .expect("too many Multiboot1 memory regions");
                }
            }
        }
    }
    // A firmware map that names no usable RAM is a machine fact worth one line
    // of its own.  Without it this boot dies later inside the heap allocator,
    // where the message no longer says that the memory map was empty.
    assert!(
        !regions.is_empty(),
        "{:?} boot: firmware reported no usable memory region",
        boot_info.protocol()
    );
    RAM_REGIONS.init_once(regions);

    let mut reserved = Vec::new();
    reserved
        .push((0, 0x100000))
        .expect("reserved-memory capacity must include low memory");
    if boot_info.protocol() == BootProtocol::Multiboot2 {
        for module in boot_info.modules() {
            let module = module.expect("module slots below module_count are initialized");
            let (start, end) = module.range();
            reserved
                .push((start, end - start))
                .expect("too many Multiboot2 module reservations");
        }
    }
    RESERVED_RAM_REGIONS.init_once(reserved);
}

/// Firmware-discovered windows must survive replacement of the boot page table.
/// This is a mapping list, not a claim that configured BAR allocation windows
/// describe the target. Already assigned PCI BARs are mapped by their drivers.
pub(crate) fn init_runtime_mmio() {
    let mut ranges: Vec<RawRange, 32> = Vec::new();
    for &range in MMIO_RANGES {
        ranges.push(range).expect("MMIO range capacity");
    }
    let mut add_page = |address: Option<usize>| {
        if let Some(address) = address.filter(|a| *a != 0 && a & 0xfff == 0) {
            ranges
                .push((address, 4096))
                .expect("runtime MMIO range capacity");
        }
    };
    if let Some(facts) = crate::cpu::apic_facts() {
        add_page(facts.lapic_address.and_then(|a| usize::try_from(a).ok()));
        add_page(facts.io_apic_address.map(|a| a as usize));
    }
    add_page(crate::acpi::hpet_base());
    // Uncore discovery can read ECAM before the bus driver maps it on demand.
    // Keep its runtime span in the early map as well, not the stale fallback.
    let (begin, end) = crate::pci::ecam_bus_range();
    let base = crate::pci::ecam_base();
    let span = (usize::from(end.saturating_sub(begin)) + 1) << 20;
    if base != 0 && base.checked_add(span).is_some() {
        ranges.push((base, span)).expect("runtime ECAM range capacity");
    }
    ranges.sort_unstable_by_key(|range| range.0);
    let mut merged: Vec<RawRange, 32> = Vec::new();
    for (start, size) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.0 + last.1
        {
            last.1 = (start + size).max(last.0 + last.1) - last.0;
        } else {
            merged.push((start, size)).expect("merged MMIO capacity");
        }
    }
    RUNTIME_MMIO.init_once(merged);
}

struct MemIfImpl;

impl MemoryManagement for MemIfImpl {
    unsafe fn paddr_to_slice(&self, addr: PAddr, size: usize) -> Option<&'static [u8]> {
        let ptr = Self::phys_to_virt(pa!(addr as usize)).as_ptr();
        Some(unsafe { core::slice::from_raw_parts(ptr, size) })
    }

    unsafe fn allocate(&mut self, _length: usize) -> Option<(PAddr, &mut [u8])> {
        None
    }

    unsafe fn deallocate(&mut self, _addr: PAddr) {}
}

#[cfg_attr(target_os = "none", impl_plat_interface)]
impl MemIf for MemIfImpl {
    /// Returns all physical memory (RAM) ranges on the platform.
    fn phys_ram_ranges() -> &'static [RawRange] {
        RAM_REGIONS.as_slice()
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Lower 1MiB memory and validated Multiboot modules are reserved and not
    /// allocatable. Module ranges are page-aligned and owned before this
    /// interface becomes visible to the allocator.
    fn reserved_phys_ram_ranges() -> &'static [RawRange] {
        RESERVED_RAM_REGIONS.as_slice()
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_ranges() -> &'static [RawRange] {
        RUNTIME_MMIO
            .get()
            .map_or(MMIO_RANGES, |ranges| ranges.as_slice())
    }

    /// Translates a physical address to a virtual address.
    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    /// Translates a virtual address to a physical address.
    fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    /// Returns the kernel address space base virtual address and size.
    fn kernel_aspace() -> (VirtAddr, usize) {
        (
            va!(crate::config::plat::KERNEL_ASPACE_BASE),
            crate::config::plat::KERNEL_ASPACE_SIZE,
        )
    }
}
