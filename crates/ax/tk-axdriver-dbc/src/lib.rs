//! Original, bounded xHCI Debug Capability transport. Hardware unverified.
#![no_std]
pub mod bringup;
pub mod desc;
pub mod ids;
pub mod probe;
pub mod regs;
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod fake;

/// Coherent DMA and bounded MMIO supplied by the platform. All offsets are
/// relative to the complete BAR / dedicated DMA allocation, respectively.
pub trait Bus {
    fn mmio_bytes(&self) -> usize;
    fn read32(&self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    fn dma_read32(&self, offset: usize) -> u32;
    fn dma_write32(&mut self, offset: usize, value: u32);
    fn dma_read8(&self, offset: usize) -> u8;
    fn dma_write8(&mut self, offset: usize, value: u8);
    fn physical(&self) -> u64;
    fn write64(&mut self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
    fn dma_write64(&mut self, offset: usize, value: u64) {
        self.dma_write32(offset, value as u32);
        self.dma_write32(offset + 4, (value >> 32) as u32);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Bounds,
    NotReady,
    Busy,
    Address,
    EnableTimeout,
    EnumerationTimeout,
    Disconnected,
    Transfer,
    MalformedEvent,
}
