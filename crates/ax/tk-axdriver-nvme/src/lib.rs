//! Original NVMe PCI transport. Linux 7.2.3 pci.c/core.c and NVMe 1.4b
//! are behavioral references only. Coherent 4-KiB DMA; owned batch queues.
//! Request slot/commit/drain design also follows TGOSKits nvme-driver.
#![no_std]
extern crate alloc;
pub mod bringup;
pub mod desc;
#[cfg(test)]
mod fake;
pub mod ids;
pub mod regs;

use core::ptr::NonNull;
/// Platform-owned coherent, physically contiguous DMA and bounded MMIO.
///
/// # Safety
/// Allocations must be page aligned, uniquely owned and valid until release.
/// MMIO reads/writes must preserve device ordering. DMA addresses must address
/// the returned memory in the device's DMA domain (not merely a CPU address).
pub unsafe trait Hal: Send + Sync {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)>;
    fn allocate_for(
        requester: Option<tk_vtd::PciRequester>,
        pages: usize,
    ) -> Option<(u64, NonNull<u8>)> {
        let _ = requester;
        Self::allocate(pages)
    }
    /// # Safety
    /// Device access must have retired and the allocation must still be owned.
    unsafe fn release(address: u64, pointer: NonNull<u8>, pages: usize);
    unsafe fn release_for(
        requester: Option<tk_vtd::PciRequester>,
        address: u64,
        pointer: NonNull<u8>,
        pages: usize,
    ) {
        let _ = requester;
        unsafe { Self::release(address, pointer, pages) }
    }
}
pub use bringup::Controller;

pub mod msix;
