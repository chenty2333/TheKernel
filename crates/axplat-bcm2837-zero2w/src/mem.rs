//! Physical memory layout of the Raspberry Pi Zero 2W.
//!
//! 512 MB RAM at physical `0x0`; the GPU firmware loads the kernel at
//! `0x0008_0000`. A page-aligned region at `0x0100_0000` is reserved for the
//! CPU-local runtime areas (see [`crate::boot::CPU_LOCAL_RESERVE_PADDR`]).

#[cfg(not(feature = "legacy"))]
use ax_memory_addr::{PhysAddr, VirtAddr, pa, va};
#[cfg(not(feature = "legacy"))]
use ax_plat::mem::{DCacheOp, IomapAttrs, IomapDecision, IomapError, MemIf, RawRange};
#[cfg(feature = "legacy")]
use axplat_old::mem::RawRange;

use crate::boot::{CPU_LOCAL_RESERVE_PADDR, CPU_LOCAL_RESERVE_SIZE};
use crate::config::{PERIPHERAL_BASE, PERIPHERAL_SIZE, PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE};
#[cfg(not(feature = "legacy"))]
use crate::config::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, PHYS_VIRT_OFFSET};

const RAM_RANGES: &[RawRange] = &[(PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE)];

const MMIO_RANGES: &[RawRange] = &[(PERIPHERAL_BASE, PERIPHERAL_SIZE)];

const RESERVED_RANGES: &[RawRange] = &[(CPU_LOCAL_RESERVE_PADDR, CPU_LOCAL_RESERVE_SIZE)];

#[cfg(feature = "legacy")]
/// Physical RAM ranges (shared with the legacy interface).
pub(crate) fn legacy_phys_ram_ranges() -> &'static [RawRange] {
    RAM_RANGES
}

#[cfg(feature = "legacy")]
/// Reserved RAM ranges (shared with the legacy interface).
pub(crate) fn legacy_reserved_phys_ram_ranges() -> &'static [RawRange] {
    RESERVED_RANGES
}

#[cfg(feature = "legacy")]
/// MMIO ranges (shared with the legacy interface).
pub(crate) fn legacy_mmio_ranges() -> &'static [RawRange] {
    MMIO_RANGES
}

#[cfg(not(feature = "legacy"))]
struct MemIfImpl;

#[cfg(not(feature = "legacy"))]
#[impl_plat_interface]
impl MemIf for MemIfImpl {
    fn phys_ram_ranges() -> &'static [RawRange] {
        RAM_RANGES
    }

    fn reserved_phys_ram_ranges() -> &'static [RawRange] {
        RESERVED_RANGES
    }

    fn mmio_ranges() -> &'static [RawRange] {
        MMIO_RANGES
    }

    fn prepare_iomap(
        _addr: PhysAddr,
        _size: usize,
        _attrs: IomapAttrs,
    ) -> Result<IomapDecision, IomapError> {
        // The generic page-table-backed mapper covers the kernel address
        // space, which includes the whole peripheral window.
        Ok(IomapDecision::UseGeneric(_addr))
    }

    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    fn kernel_aspace() -> (VirtAddr, usize) {
        (va!(KERNEL_ASPACE_BASE), KERNEL_ASPACE_SIZE)
    }

    fn user_aspace_needs_kernel_mappings() -> bool {
        false
    }

    fn dcache_range(op: DCacheOp, addr: VirtAddr, size: usize) {
        // BCM2837 is a non-coherent DMA platform; clean the CPU cache before
        // device ownership transfer. The GPU never writes kernel buffers that
        // the CPU reads on the 3D path in this design, so invalidation is a
        // no-op until a GPU->CPU path (e.g. framebuffer readback) lands.
        match op {
            DCacheOp::Clean | DCacheOp::CleanInvalidate => {
                ax_cpu::asm::clean_dcache_range_to_pou(addr, size);
            }
            DCacheOp::Invalidate => {}
        }
    }

    fn dma_coherent_before_make_uncached(addr: VirtAddr, size: usize) {
        ax_cpu::asm::clean_dcache_range_to_pou(addr, size);
    }

    fn dma_coherent_before_restore_cached(_addr: VirtAddr, _size: usize) {}

    fn dma_coherent_after_mapping_update() {}
}
