//! RTL8168H MAC configuration, selected only for PCI 10ec:8168 and XID 0x541
//! under mask 0x7cf. Facts: Linux 7.2.3 rtl_hw_start_8168h_1 and its helpers.
//! Original implementation; 未在硬件上验证. No display or NVMe writes.
use super::{
    indirect as i,
    regs::{
        self as r, Bus,
        Width::{Byte, Dword, Word},
    },
};
use crate::DevResult;
fn byte_clear(bus: &mut impl Bus, offset: usize, bits: u32) {
    let value = bus.read(offset, Byte);
    bus.write(offset, Byte, value & !bits);
}
fn configure(bus: &mut impl Bus) -> DevResult {
    // Keep chip ASPM disabled: we do not own PCIe link power policy.
    byte_clear(bus, 0x53, 0x80);
    byte_clear(bus, 0x56, 1);
    i::mac_modify(bus, 0xe092, 0x00ff, 0)?;
    i::ephy_modify(bus, 0x1e, 0x0800, 1)?;
    i::ephy_modify(bus, 0x1d, 0, 0x0800)?;
    for (reg, value) in [(5, 0x2089), (6, 0x5881), (4, 0x854a), (1, 0x068b)] {
        i::ephy_modify(bus, reg, 0xffff, value)?;
    }
    i::eri_write(bus, 0xc8, 15, 0x0008_0002)?;
    i::eri_write(bus, 0xe8, 15, 0x0010_0006)?;
    i::eri_write(bus, 0xcc, 1, 0x38)?;
    i::eri_write(bus, 0xd0, 1, 0x48)?;
    i::eri_modify(bus, 0xdc, 1, 0)?;
    i::eri_modify(bus, 0xdc, 0, 0x1d)?;
    i::eri_write(bus, 0x5f0, 3, 0x4f87)?;
    let misc = bus.read(r::MISC, Dword);
    bus.write(r::MISC, Dword, misc & !(1 << 19));
    i::eri_write(bus, 0xc0, 3, 0)?;
    i::eri_write(bus, 0xb8, 3, 0)?;
    byte_clear(bus, 0xd0, 0xc0);
    byte_clear(bus, 0xf2, 0x40);
    i::eri_modify(bus, 0x1b0, 1 << 12, 0)?;
    byte_clear(bus, 0x54, 2);
    let oscillator = i::phy_read(bus, 0xc426)? & 0x3fff;
    if oscillator != 0 {
        i::mac_modify(
            bus,
            0xd412,
            0x0fff,
            ((16_000_000 / u32::from(oscillator)) & 0x0fff) as u16,
        )?;
    }
    i::mac_modify(bus, 0xe056, 0x00f0, 0)?;
    i::mac_modify(bus, 0xe052, 0x6000, 0x8008)?;
    i::mac_modify(bus, 0xe0d6, 0x01ff, 0x017f)?;
    i::mac_modify(bus, 0xd420, 0x0fff, 0x047f)?;
    i::mac_write(bus, 0xe63e, 1)?;
    i::mac_write(bus, 0xe63e, 0)?;
    i::mac_write(bus, 0xc094, 0)?;
    i::mac_write(bus, 0xc09e, 0)?;
    Ok(())
}
pub fn program(bus: &mut impl Bus, tx: u64, rx: u64) -> DevResult {
    bus.write(r::CFG_LOCK, Byte, 0xc0);
    // Always restore the config lock, including indirect transaction timeouts.
    let result = configure(bus);
    if result.is_ok() {
        let cplus = bus.read(r::CPLUS, Word);
        bus.write(r::CPLUS, Word, cplus & !((1 << 5) | (1 << 6) | (1 << 9)));
        // Standard MTU; neither jumbo nor hardware VLAN/checksum stripping.
        byte_clear(bus, 0x54, 1 << 2);
        byte_clear(bus, 0x55, 1 << 1);
        bus.write(0xec, Byte, 0x27);
        bus.write(0xe2, Word, 0);
        bus.write(r::RX_MAX, Word, super::desc::BUFFER as u32);
        for (high, low, address) in [(r::TX_HIGH, r::TX_LOW, tx), (r::RX_HIGH, r::RX_LOW, rx)] {
            bus.write(high, Dword, (address >> 32) as u32);
            bus.write(low, Dword, address as u32);
        }
    }
    bus.write(r::CFG_LOCK, Byte, 0);
    result?;
    let _ = bus.read(r::COMMAND, Byte);
    bus.write(r::COMMAND, Byte, r::RX_TX_ENABLE);
    bus.write(
        r::RX_CONFIG,
        Dword,
        (1 << 15) | (1 << 14) | (7 << 8) | (1 << 11) | 0x0e,
    );
    bus.write(r::TX_CONFIG, Dword, (3 << 24) | (7 << 8) | (1 << 7));
    bus.write(0x08, Dword, u32::MAX);
    bus.write(0x0c, Dword, u32::MAX);
    bus.write(0x3e, Word, 0xffff);
    Ok(())
}

/// PHY calibration after the matching firmware. Missing firmware takes the
/// separate warm-PXE path, and does not reset or guess the PHY configuration.
pub fn configure_phy(bus: &mut impl Bus) -> DevResult {
    fn modify(bus: &mut impl Bus, addr: u16, clear: u16, set: u16) -> DevResult {
        let value = i::phy_read(bus, addr)?;
        i::phy_write(bus, addr, (value & !clear) | set)
    }
    fn parameter(bus: &mut impl Bus, key: u16, clear: u16, set: u16) -> DevResult {
        i::phy_write(bus, 0xa436, key)?;
        modify(bus, 0xa438, clear, set)
    }
    super::health::stage("parameter-808a", parameter(bus, 0x808a, 0x003f, 0x000a))?;
    super::health::stage("parameter-0811", parameter(bus, 0x0811, 0, 0x0800))?;
    super::health::stage("PHY-a42c", modify(bus, 0xa42c, 0, 2))?;
    super::health::stage("PHY-a442-set", modify(bus, 0xa442, 0, 1 << 11))?;
    super::health::stage("MAC-dd02", i::mac_write(bus, 0xdd02, 0x807d))?;
    let adc_high = super::health::stage("ADC-dd02", i::mac_read(bus, 0xdd02))?;
    let adc = super::health::stage("ADC-dd00", i::mac_read(bus, 0xdd00))?;
    let bias = ((adc >> 1) & 0x7ff8) | (adc & 7) | ((adc_high & 0x80) << 8);
    if bias != 0xffff {
        super::health::stage("ADC-bcfc", i::phy_write(bus, 0xbcfc, bias))?;
    }
    let length =
        (super::health::stage("length-bcdc", i::phy_read(bus, 0xbcdc))? & 15).saturating_sub(3);
    super::health::stage("length-bcde", i::phy_write(bus, 0xbcde, length * 0x1111))?;
    super::health::stage("PHY-a442-clear", modify(bus, 0xa442, 1 << 7, 0))?;
    super::health::stage("PHY-a430", modify(bus, 0xa430, (1 << 0) | (1 << 2), 0))?;
    super::health::stage("PHY-a432", modify(bus, 0xa432, 0, 1 << 4))?;
    // Advertise 10/100/1000 full/half duplex and restart auto-negotiation.
    super::health::stage("advertise-10-100", i::phy_write(bus, 0xa408, 0x0de1))?;
    super::health::stage("advertise-1000", i::phy_write(bus, 0xa412, 0x0300))?;
    let bmcr = super::health::stage("BMCR-read", i::phy_read(bus, 0xa400))?;
    super::health::stage(
        "autoneg-restart",
        i::phy_write(bus, 0xa400, (bmcr & !0x0c00) | 0x1200),
    )?;
    Ok(())
}
