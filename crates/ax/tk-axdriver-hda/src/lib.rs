//! Original HDA controller/codec transport, Intel HDA 1.0a §§3–7.
#![no_std]
extern crate alloc;
pub mod bringup;
pub mod codec;
pub mod desc;
pub mod eld;
#[cfg(test)]
mod fake;
pub mod ids;
pub mod regs;
use core::ptr::NonNull;
/// Coherent page-aligned DMA contract.
/// # Safety
/// Allocations are contiguous in the device DMA domain and uniquely owned.
pub unsafe trait Hal: Send + Sync {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)>;
    /// # Safety
    /// The matching allocation must be owned and device DMA must have retired.
    unsafe fn release(address: u64, pointer: NonNull<u8>, pages: usize);
}
pub use bringup::Controller;
