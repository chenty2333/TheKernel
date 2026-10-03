//! NVMe 1.4b controller register offsets.
pub const CAP: usize = 0;
pub const CC: usize = 0x14;
pub const CSTS: usize = 0x1c;
pub const AQA: usize = 0x24;
pub const ASQ: usize = 0x28;
pub const ACQ: usize = 0x30;
pub const DBS: usize = 0x1000;
pub trait Bus: Send + Sync {
    fn read32(&mut self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    fn delay_us(&mut self, micros: u32);
    fn read64(&mut self, offset: usize) -> u64 {
        u64::from(self.read32(offset)) | (u64::from(self.read32(offset + 4)) << 32)
    }
    fn write64(&mut self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}
