use crate::{Bus, Error, ids, regs};
/// No writes, bounded forward-only DWORD capability chain. Reject truncated
/// registers and CNR before touching DbC. Existing firmware DbC is not stolen.
pub fn find(bus: &impl Bus, size: usize) -> Result<Option<usize>, Error> {
    let size = size.min(bus.mmio_bytes());
    if size < 0x20 {
        return Err(Error::Bounds);
    }
    let operational = (bus.read32(0) & 0xff) as usize;
    if !operational.is_multiple_of(4)
        || operational < 0x20
        || operational.checked_add(8).is_none_or(|end| end > size)
    {
        return Err(Error::Bounds);
    }
    if bus.read32(operational + 4) & (1 << 11) != 0 {
        return Err(Error::NotReady);
    }
    let mut offset = ((bus.read32(0x10) >> 16) as usize) * 4;
    for _ in 0..256 {
        if offset == 0 {
            return Ok(None);
        }
        if offset < 0x20 || offset.checked_add(4).is_none_or(|end| end > size) {
            return Err(Error::Bounds);
        }
        let cap = bus.read32(offset);
        if cap as u8 == ids::CAPABILITY {
            if offset.checked_add(0x40).is_none_or(|end| end > size) {
                return Err(Error::Bounds);
            }
            if bus.read32(offset + regs::CONTROL) & regs::ENABLE != 0 {
                return Err(Error::Busy);
            }
            return Ok(Some(offset));
        }
        let next = ((cap >> 8) & 0xff) as usize;
        if next == 0 {
            return Ok(None);
        }
        offset = offset.checked_add(next * 4).ok_or(Error::Bounds)?;
    }
    Err(Error::Bounds)
}
