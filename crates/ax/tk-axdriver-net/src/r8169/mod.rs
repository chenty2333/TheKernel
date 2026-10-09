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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HalError {
    NoMemory,
    Failed,
    /// DMA state or backing ownership is ambiguous; storage must be retained.
    Quarantined,
}

pub trait Hal: Send + Sync {
    /// Allocate CPU backing and return the address mapped for this HAL's PCI
    /// requester. Implementations must retain backing when returning
    /// `Quarantined` after a mapping attempt.
    fn allocate(&self, pages: usize) -> Result<(u64, NonNull<u8>), HalError>;
    /// # Safety
    /// Must be the exact allocation, with DMA stopped and no live packet loans.
    /// On any error the implementation must not release the CPU backing.
    unsafe fn deallocate(
        &self,
        address: u64,
        pointer: NonNull<u8>,
        pages: usize,
    ) -> Result<(), HalError>;
}
