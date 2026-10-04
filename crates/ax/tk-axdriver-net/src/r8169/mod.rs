//! Shared RTL8125B / RTL8168H descriptor and DMA core. 未在硬件上验证.
pub mod bringup;
pub mod desc;
#[cfg(test)]
mod fake;
pub mod firmware;
pub mod h8168;
pub mod health;
pub mod ids;
pub mod indirect;
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
