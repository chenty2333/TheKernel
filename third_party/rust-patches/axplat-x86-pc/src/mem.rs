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

static RAM_REGIONS: LazyInit<Vec<RawRange, MAX_REGIONS>> = LazyInit::new();

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
    RAM_REGIONS.init_once(regions);
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

#[impl_plat_interface]
impl MemIf for MemIfImpl {
    /// Returns all physical memory (RAM) ranges on the platform.
    fn phys_ram_ranges() -> &'static [RawRange] {
        RAM_REGIONS.as_slice()
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Lower 1MiB memory is reserved and not allocatable.
    fn reserved_phys_ram_ranges() -> &'static [RawRange] {
        &[(0, 0x100000)]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_ranges() -> &'static [RawRange] {
        &MMIO_RANGES
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
