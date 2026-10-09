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
use crate::{DevError, DevResult};

const PHY_BMCR: u16 = 0xa400;
const PHY_BMCR_RESET: u16 = 1 << 15;
const PHY_RESET_POLL_US: u32 = 50_000;
const PHY_RESET_POLLS: usize = 12;

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

    // Linux r8169_apply_firmware() polls BMCR_RESET after the firmware image
    // runs. Indirect OCP completion is not the PHY's asynchronous reset ACK.
    super::health::stage(
        "firmware-PHY-reset",
        (|| {
            for attempt in 0..=PHY_RESET_POLLS {
                let bmcr = i::phy_read(bus, PHY_BMCR)?;
                if bmcr & PHY_BMCR_RESET == 0 {
                    return Ok(());
                }
                if attempt < PHY_RESET_POLLS {
                    bus.delay_us(PHY_RESET_POLL_US);
                }
            }
            Err(DevError::Io)
        })(),
    )?;

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
    // Linux clears LINK_SPEED_10M_PLL_OFF here. ALDPS and EEE are deliberately
    // applied after H-specific tuning, matching rtl8168h_2_hw_phy_config.
    super::health::stage("PHY-a430-pll", modify(bus, 0xa430, 1 << 0, 0))?;
    super::health::stage("ALDPS-disable", modify(bus, 0xa430, 1 << 2, 0))?;
    super::health::stage("EEE-config", modify(bus, 0xa432, 0, 1 << 4))?;
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

#[cfg(test)]
mod tests {
    use super::{
        super::{fake::FakeBus, regs::Width},
        *,
    };

    struct ResetStuckBus {
        writes: std::vec::Vec<(usize, Width, u32)>,
        delays: usize,
    }

    impl Bus for ResetStuckBus {
        fn read(&mut self, offset: usize, _width: Width) -> u32 {
            if offset == 0xb8 {
                (1 << 31) | u32::from(PHY_BMCR_RESET)
            } else {
                0
            }
        }

        fn write(&mut self, offset: usize, width: Width, value: u32) {
            self.writes.push((offset, width, value));
        }

        fn delay_us(&mut self, micros: u32) {
            assert_eq!(micros, PHY_RESET_POLL_US);
            self.delays += 1;
        }
    }

    struct FaultPhyWriteBus {
        inner: FakeBus,
        address: u16,
        occurrence: usize,
        seen: usize,
    }

    impl Bus for FaultPhyWriteBus {
        fn read(&mut self, offset: usize, width: Width) -> u32 {
            self.inner.read(offset, width)
        }

        fn write(&mut self, offset: usize, width: Width, value: u32) {
            if offset == 0xb8 && value & (1 << 31) != 0 {
                let address = (value >> 15) as u16;
                if address == self.address {
                    self.seen += 1;
                    if self.seen == self.occurrence {
                        self.inner.stuck_indirect = true;
                    }
                }
            }
            self.inner.write(offset, width, value);
        }

        fn delay_us(&mut self, micros: u32) {
            self.inner.delay_us(micros);
        }
    }

    fn phy_writes(bus: &FakeBus) -> std::vec::Vec<(u16, u16)> {
        bus.writes
            .iter()
            .filter_map(|&(port, _, value)| {
                (port == 0xb8 && value & (1 << 31) != 0)
                    .then_some(((value >> 15) as u16, value as u16))
            })
            .collect()
    }

    #[test]
    fn waits_for_firmware_phy_reset_then_orders_aldps_eee_before_autoneg() {
        let mut bus = FakeBus::h8168();
        i::phy_write(&mut bus, 0xa430, 0x0005).unwrap();
        bus.writes.clear();

        configure_phy(&mut bus).unwrap();

        assert_eq!(
            bus.writes.first(),
            Some(&(0xb8, Width::Dword, u32::from(PHY_BMCR) << 15))
        );
        let writes = phy_writes(&bus);
        assert_eq!(
            &writes[writes.len() - 6..],
            &[
                (0xa430, 0x0004), // clear only LINK_SPEED_10M_PLL_OFF
                (0xa430, 0x0000), // rtl8168g_disable_aldps(): clear ALDPS_PLL_OFF
                (0xa432, 0x0010), // rtl8168g_config_eee_phy()
                (0xa408, 0x0de1),
                (0xa412, 0x0300),
                (PHY_BMCR, 0x1200), // auto-negotiation is restarted last
            ]
        );
    }

    #[test]
    fn firmware_reset_timeout_propagates_before_calibration_writes() {
        let mut bus = ResetStuckBus {
            writes: std::vec::Vec::new(),
            delays: 0,
        };

        assert!(configure_phy(&mut bus).is_err());

        assert_eq!(bus.writes.len(), PHY_RESET_POLLS + 1);
        assert_eq!(bus.delays, PHY_RESET_POLLS);
        assert!(
            bus.writes
                .iter()
                .all(|&(port, _, value)| { port == 0xb8 && value & (1 << 31) == 0 })
        );
    }

    #[test]
    fn aldps_and_eee_ocp_failures_propagate_without_autoneg_restart() {
        for (address, occurrence) in [(0xa430, 2), (0xa432, 1)] {
            let mut bus = FaultPhyWriteBus {
                inner: FakeBus::h8168(),
                address,
                occurrence,
                seen: 0,
            };
            assert!(configure_phy(&mut bus).is_err());
            assert!(!bus.inner.writes.iter().any(|&(port, _, value)| {
                port == 0xb8
                    && value & (1 << 31) != 0
                    && (value >> 15) as u16 == PHY_BMCR
                    && value as u16 == 0x1200
            }));
        }
    }
}
