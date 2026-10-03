//! Read-only identity before reset or DMA publication.
use super::{
    ids,
    regs::{
        self as r, Bus,
        Width::{Byte, Dword},
    },
};
use crate::{DevError, DevResult};
pub fn identify(bus: &mut impl Bus) -> DevResult<(ids::Chip, [u8; 6])> {
    let chip = ids::identify(bus.read(r::TX_CONFIG, Dword)).ok_or(DevError::Unsupported)?;
    let mut mac = [0; 6];
    for (offset, byte) in mac.iter_mut().enumerate() {
        *byte = bus.read(r::MAC + offset, Byte) as u8;
    }
    if mac[0] & 1 != 0 || mac == [0; 6] || mac == [0xff; 6] {
        return Err(DevError::BadState);
    }
    Ok((chip, mac))
}
