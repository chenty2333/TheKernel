//! Minimal RTL8125B/BG polling driver. 未在硬件上验证.
//! The PHY is inherited from PXE firmware, not reinitialized; see nic-rtl8125.md.
pub mod bringup;
pub mod desc;
#[cfg(test)]
mod fake;
pub mod ids;
pub mod nic;
pub mod probe;
pub mod regs;
use core::ptr::NonNull;
pub trait Hal {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)>;
    /// # Safety
    /// Must be the exact allocation, with DMA stopped and no live packet loans.
    unsafe fn deallocate(address: u64, pointer: NonNull<u8>, pages: usize);
}
