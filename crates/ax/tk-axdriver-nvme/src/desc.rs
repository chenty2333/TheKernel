//! Little-endian submission descriptors and bounded PRP construction.
use axdriver_block::{DevError, DevResult};
pub const PAGE: usize = 4096;
pub const TRANSFER: usize = 128 * 1024;
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Command(pub [u32; 16]);
impl Command {
    pub fn new(opcode: u8, nsid: u32) -> Self {
        let mut c = Self::default();
        c.0[0] = u32::from(opcode);
        c.0[1] = nsid;
        c
    }
    pub fn pointer(&mut self, index: usize, address: u64) {
        self.0[index] = address as u32;
        self.0[index + 1] = (address >> 32) as u32;
    }
}
/// Contiguous bounce memory still needs a PRP list for three or more pages.
/// The bounded 128-KiB transfer fits one list page, so no chained list is used.
pub fn prps(base: u64, length: usize, list: u64, entries: &mut [u64]) -> DevResult<(u64, u64)> {
    if base == 0 || length == 0 || length > TRANSFER || base.checked_add(length as u64).is_none() {
        return Err(DevError::InvalidParam);
    }
    let first = PAGE - (base as usize & (PAGE - 1));
    if length <= first {
        return Ok((base, 0));
    }
    let next = base + first as u64;
    if length - first <= PAGE {
        return Ok((base, next));
    }
    let count = (length - first).div_ceil(PAGE);
    if list == 0 || list & 4095 != 0 || entries.len() < count {
        return Err(DevError::InvalidParam);
    }
    for (i, entry) in entries[..count].iter_mut().enumerate() {
        *entry = (next + (i * PAGE) as u64).to_le();
    }
    Ok((base, list))
}
