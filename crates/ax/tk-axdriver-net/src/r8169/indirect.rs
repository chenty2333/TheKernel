//! Bounded Realtek indirect register transactions. Register protocol facts:
//! Linux 7.2.3 r8169_main.c ERI, EPHY and MAC/PHY OCP accessors.
use super::regs::{Bus, Width::Dword};
use crate::{DevError, DevResult};
const READY: u32 = 1 << 31;
pub fn wait(
    bus: &mut impl Bus,
    port: usize,
    high: bool,
    us: u32,
    attempts: usize,
) -> DevResult<u32> {
    for _ in 0..attempts {
        let value = bus.read(port, Dword);
        if (value & READY != 0) == high {
            return Ok(value);
        }
        bus.delay_us(us);
    }
    Err(DevError::Io)
}
pub fn mac_read(bus: &mut impl Bus, address: u16) -> DevResult<u16> {
    if address & 1 != 0 {
        return Err(DevError::InvalidParam);
    }
    bus.write(0xb0, Dword, u32::from(address) << 15);
    Ok(bus.read(0xb0, Dword) as u16)
}
pub fn mac_write(bus: &mut impl Bus, address: u16, data: u16) -> DevResult {
    if address & 1 != 0 {
        return Err(DevError::InvalidParam);
    }
    bus.write(
        0xb0,
        Dword,
        READY | (u32::from(address) << 15) | u32::from(data),
    );
    Ok(())
}
pub fn mac_modify(bus: &mut impl Bus, address: u16, clear: u16, set: u16) -> DevResult {
    let data = mac_read(bus, address)?;
    mac_write(bus, address, (data & !clear) | set)
}
pub fn phy_read(bus: &mut impl Bus, address: u16) -> DevResult<u16> {
    if address & 1 != 0 {
        return Err(DevError::InvalidParam);
    }
    bus.write(0xb8, Dword, u32::from(address) << 15);
    Ok(wait(bus, 0xb8, true, 25, 10)? as u16)
}
pub fn phy_write(bus: &mut impl Bus, address: u16, data: u16) -> DevResult {
    if address & 1 != 0 {
        return Err(DevError::InvalidParam);
    }
    bus.write(
        0xb8,
        Dword,
        READY | (u32::from(address) << 15) | u32::from(data),
    );
    wait(bus, 0xb8, false, 25, 10)?;
    Ok(())
}
pub fn eri_read(bus: &mut impl Bus, address: u16) -> DevResult<u32> {
    if address & 3 != 0 {
        return Err(DevError::InvalidParam);
    }
    bus.write(0x74, Dword, 0xf000 | u32::from(address));
    wait(bus, 0x74, true, 100, 100)?;
    Ok(bus.read(0x70, Dword))
}
pub fn eri_write(bus: &mut impl Bus, address: u16, lanes: u8, data: u32) -> DevResult {
    if address & 3 != 0 || lanes == 0 || lanes > 15 {
        return Err(DevError::InvalidParam);
    }
    bus.write(0x70, Dword, data);
    bus.write(
        0x74,
        Dword,
        READY | (u32::from(lanes) << 12) | u32::from(address),
    );
    wait(bus, 0x74, false, 100, 100)?;
    Ok(())
}
pub fn eri_modify(bus: &mut impl Bus, address: u16, clear: u32, set: u32) -> DevResult {
    let data = eri_read(bus, address)?;
    eri_write(bus, address, 15, (data & !clear) | set)
}
pub fn ephy_modify(bus: &mut impl Bus, reg: u8, clear: u16, set: u16) -> DevResult {
    if reg > 31 {
        return Err(DevError::InvalidParam);
    }
    bus.write(0x80, Dword, u32::from(reg) << 16);
    let data = wait(bus, 0x80, true, 10, 100)? as u16;
    bus.write(
        0x80,
        Dword,
        READY | (u32::from(reg) << 16) | u32::from((data & !clear) | set),
    );
    wait(bus, 0x80, false, 10, 100)?;
    bus.delay_us(10);
    Ok(())
}
