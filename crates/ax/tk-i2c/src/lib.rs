//! Intel DesignWare I2C host controller and the small bus contract used by
//! platform drivers.  This crate deliberately has no OS-specific dependencies.
//!
//! Translated from FreeBSD `sys/dev/ichiic/ig4_iic.c`, `ig4_pci.c`,
//! `ig4_acpi.c`, `ig4_reg.h`, and `ig4_var.h` (FreeBSD source snapshot
//! 2026-10-08; BSD-3-Clause for the DragonFly-derived sources and
//! BSD-2-Clause for `ig4_acpi.c`). Original copyright notices: Copyright
//! (c) 2014 The DragonFly Project; Copyright (c) 2016 Oleksandr Tymoshenko
//! <gonzo@FreeBSD.org>. License texts are retained under `LICENSES/`.

#![no_std]

#[cfg(test)]
extern crate std;

use core::fmt;

pub mod regs {
    //! DesignWare register layout translated from FreeBSD `ig4_reg.h`.
    pub const CONTROL: u32 = 0x0000;
    pub const TARGET_ADDRESS: u32 = 0x0004;
    pub const DATA_COMMAND: u32 = 0x0010;
    pub const SS_SCL_HCNT: u32 = 0x0014;
    pub const SS_SCL_LCNT: u32 = 0x0018;
    pub const FS_SCL_HCNT: u32 = 0x001c;
    pub const FS_SCL_LCNT: u32 = 0x0020;
    pub const INTR_STAT: u32 = 0x002c;
    pub const INTR_MASK: u32 = 0x0030;
    pub const RAW_INTR_STAT: u32 = 0x0034;
    pub const RX_TL: u32 = 0x0038;
    pub const TX_TL: u32 = 0x003c;
    pub const CLEAR_INTR: u32 = 0x0040;
    pub const CLEAR_TX_ABORT: u32 = 0x0054;
    pub const ENABLE: u32 = 0x006c;
    pub const STATUS: u32 = 0x0070;
    pub const TXFLR: u32 = 0x0074;
    pub const RXFLR: u32 = 0x0078;
    pub const SDA_HOLD: u32 = 0x007c;
    pub const TX_ABORT_SOURCE: u32 = 0x0080;
    pub const ENABLE_STATUS: u32 = 0x009c;
    pub const COMPONENT_PARAM: u32 = 0x00f4;
    pub const COMPONENT_VERSION: u32 = 0x00f8;
    pub const COMPONENT_TYPE: u32 = 0x00fc;

    pub const CTL_MASTER: u32 = 1;
    pub const CTL_SPEED_STANDARD: u32 = 2;
    pub const CTL_SPEED_FAST: u32 = 4;
    pub const CTL_10BIT: u32 = 1 << 4;
    pub const CTL_RESTART_ENABLE: u32 = 1 << 5;
    pub const CTL_SLAVE_DISABLE: u32 = 1 << 6;
    pub const CMD_READ: u32 = 1 << 8;
    pub const CMD_STOP: u32 = 1 << 9;
    pub const CMD_RESTART: u32 = 1 << 10;
    pub const STATUS_ACTIVITY: u32 = 1;
    pub const STATUS_TX_NOT_FULL: u32 = 1 << 1;
    pub const STATUS_RX_NOT_EMPTY: u32 = 1 << 3;
    pub const INTR_TX_ABORT: u32 = 1 << 6;
    pub const COMP_TYPE_DESIGNWARE: u32 = 0x4457_0140;
}

/// MMIO operations required by the controller. Implementations must perform
/// volatile 32-bit accesses and provide ordering between accesses.
pub trait RegisterIo {
    fn read32(&mut self, offset: u32) -> u32;
    fn write32(&mut self, offset: u32, value: u32);
    /// Bound polling loops so a wedged bus cannot hang the kernel forever.
    fn relax(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Speed {
    Standard100Khz,
    Fast400Khz,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Address {
    pub value: u16,
    pub ten_bit: bool,
}

impl Address {
    pub const fn seven_bit(value: u8) -> Option<Self> {
        if value <= 0x7f {
            Some(Self {
                value: value as u16,
                ten_bit: false,
            })
        } else {
            None
        }
    }

    pub const fn ten_bit(value: u16) -> Option<Self> {
        if value <= 0x3ff {
            Some(Self {
                value,
                ten_bit: true,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Write,
    Read,
}

/// One I2C message. Adjacent messages are issued as a combined transaction;
/// the final byte of the final message receives STOP.
pub struct Message<'a> {
    pub address: Address,
    pub direction: Direction,
    pub data: &'a mut [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAddress,
    EmptyTransfer,
    Busy,
    Timeout,
    Nack,
    InvalidConfiguration,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "I2C error: {self:?}")
    }
}

/// Timing values used by the FreeBSD ig4 controller configuration. Counts
/// are hardware-clock periods, not bus-clock frequency values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timing {
    pub standard_high: u16,
    pub standard_low: u16,
    pub fast_high: u16,
    pub fast_low: u16,
    pub sda_hold: u16,
}

impl Timing {
    pub const N305: Self = Self {
        standard_high: 0x264,
        standard_low: 0x2c2,
        fast_high: 0x6e,
        fast_low: 0xcf,
        sda_hold: 1,
    };
}

/// Current task endpoint: register-programmed DesignWare engine corresponding
/// to the upstream master-transfer path. Platform discovery and VFS publication
/// are kept outside the controller core.
pub struct Controller<I> {
    io: I,
    timing: Timing,
    speed: Speed,
    poll_limit: usize,
}

impl<I: RegisterIo> Controller<I> {
    /// Initialize a disabled DesignWare master and program the upstream timing
    /// fields. Caller must have reserved and mapped the MMIO BAR/resource.
    // upstream: ig4_iic.c ig4iic_attach()
    pub fn new(mut io: I, timing: Timing, speed: Speed, poll_limit: usize) -> Result<Self, Error> {
        if poll_limit == 0 {
            return Err(Error::InvalidConfiguration);
        }
        let component_type = io.read32(regs::COMPONENT_TYPE);
        if component_type != 0 && component_type != regs::COMP_TYPE_DESIGNWARE {
            return Err(Error::InvalidConfiguration);
        }
        io.write32(regs::ENABLE, 0);
        let mut ctl = regs::CTL_MASTER | regs::CTL_RESTART_ENABLE | regs::CTL_SLAVE_DISABLE;
        ctl |= match speed {
            Speed::Standard100Khz => regs::CTL_SPEED_STANDARD,
            Speed::Fast400Khz => regs::CTL_SPEED_FAST,
        };
        io.write32(regs::CONTROL, ctl);
        io.write32(regs::SS_SCL_HCNT, u32::from(timing.standard_high));
        io.write32(regs::SS_SCL_LCNT, u32::from(timing.standard_low));
        io.write32(regs::FS_SCL_HCNT, u32::from(timing.fast_high));
        io.write32(regs::FS_SCL_LCNT, u32::from(timing.fast_low));
        io.write32(regs::SDA_HOLD, u32::from(timing.sda_hold));
        io.write32(regs::TX_TL, 0);
        io.write32(regs::RX_TL, 0);
        io.write32(regs::INTR_MASK, 0);
        io.write32(regs::CLEAR_INTR, 0);
        io.write32(regs::ENABLE, 1);
        for _ in 0..poll_limit {
            if io.read32(regs::ENABLE_STATUS) & 1 != 0 {
                return Ok(Self {
                    io,
                    timing,
                    speed,
                    poll_limit,
                });
            }
            io.relax();
        }
        Err(Error::Timeout)
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }
    pub fn speed(&self) -> Speed {
        self.speed
    }
    pub fn into_inner(self) -> I {
        self.io
    }

    /// Execute one or more combined messages. Each queued command is gated by
    /// TX FIFO availability; reads are drained from RX FIFO as soon as present.
    // upstream: ig4_iic.c ig4iic_transfer()
    pub fn transfer(&mut self, messages: &mut [Message<'_>]) -> Result<(), Error> {
        if messages.is_empty() || messages.iter().any(|m| m.data.is_empty()) {
            return Err(Error::EmptyTransfer);
        }
        if messages.iter().any(|m| {
            if m.address.ten_bit {
                m.address.value > 0x3ff
            } else {
                m.address.value > 0x7f
            }
        }) {
            return Err(Error::InvalidAddress);
        }
        let address = messages[0].address;
        if messages.iter().any(|message| message.address != address) {
            // The upstream ig4 transfer programs TAR once for an I2C transfer;
            // changing TAR mid-flight is not a supported repeated-start form.
            return Err(Error::InvalidConfiguration);
        }
        if self.io.read32(regs::STATUS) & regs::STATUS_ACTIVITY != 0 {
            return Err(Error::Busy);
        }
        self.io
            .write32(regs::TARGET_ADDRESS, u32::from(address.value));
        self.set_address_mode(address.ten_bit);
        for message_index in 0..messages.len() {
            let len = messages[message_index].data.len();
            for byte_index in 0..len {
                let last = message_index + 1 == messages.len() && byte_index + 1 == len;
                match messages[message_index].direction {
                    Direction::Write => {
                        self.wait_status(regs::STATUS_TX_NOT_FULL)?;
                        let mut command = u32::from(messages[message_index].data[byte_index]);
                        if byte_index == 0 && message_index != 0 {
                            command |= regs::CMD_RESTART;
                        }
                        if last {
                            command |= regs::CMD_STOP;
                        }
                        self.io.write32(regs::DATA_COMMAND, command);
                    }
                    Direction::Read => {
                        self.wait_status(regs::STATUS_TX_NOT_FULL)?;
                        let mut command = regs::CMD_READ;
                        if byte_index == 0 && message_index != 0 {
                            command |= regs::CMD_RESTART;
                        }
                        if last {
                            command |= regs::CMD_STOP;
                        }
                        self.io.write32(regs::DATA_COMMAND, command);
                        self.wait_status(regs::STATUS_RX_NOT_EMPTY)?;
                        messages[message_index].data[byte_index] =
                            self.io.read32(regs::DATA_COMMAND) as u8;
                    }
                }
                if self.io.read32(regs::RAW_INTR_STAT) & regs::INTR_TX_ABORT != 0 {
                    let _abort_source = self.io.read32(regs::TX_ABORT_SOURCE);
                    let _ = self.io.read32(regs::CLEAR_TX_ABORT);
                    return Err(Error::Nack);
                }
            }
        }
        self.set_address_mode(false);
        self.wait_idle()?;
        Ok(())
    }

    fn set_address_mode(&mut self, ten_bit: bool) {
        let mut control = self.io.read32(regs::CONTROL) & !regs::CTL_10BIT;
        if ten_bit {
            control |= regs::CTL_10BIT;
        }
        self.io.write32(regs::CONTROL, control);
    }

    fn wait_status(&mut self, mask: u32) -> Result<(), Error> {
        for _ in 0..self.poll_limit {
            if self.io.read32(regs::STATUS) & mask != 0 {
                return Ok(());
            }
            if self.io.read32(regs::RAW_INTR_STAT) & regs::INTR_TX_ABORT != 0 {
                let _ = self.io.read32(regs::CLEAR_TX_ABORT);
                return Err(Error::Nack);
            }
            self.io.relax();
        }
        Err(Error::Timeout)
    }

    fn wait_idle(&mut self) -> Result<(), Error> {
        for _ in 0..self.poll_limit {
            if self.io.read32(regs::STATUS) & regs::STATUS_ACTIVITY == 0 {
                return Ok(());
            }
            self.io.relax();
        }
        Err(Error::Timeout)
    }
}

/// The address field of an ACPI `I2cSerialBusV2` resource. The ACPICA adapter
/// supplies these values after validating the resource descriptor type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiI2cDevice {
    pub address: Address,
    pub speed_hz: u32,
    pub resource_source: &'static str,
}

impl AcpiI2cDevice {
    // upstream: ig4_acpi.c ig4iic_acpi_probe()
    pub const fn from_serial_bus(
        slave_address: u16,
        ten_bit: bool,
        speed_hz: u32,
        resource_source: &'static str,
    ) -> Result<Self, Error> {
        let valid = if ten_bit {
            slave_address <= 0x3ff
        } else {
            slave_address <= 0x7f
        };
        if !valid || speed_hz == 0 {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            address: Address {
                value: slave_address,
                ten_bit,
            },
            speed_hz,
            resource_source,
        })
    }
}

/// N305 Intel LPSS DesignWare controller IDs from the PCI device table.
// upstream: ig4_pci.c ig4iic_pci_probe()
pub const INTEL_LPSS_I2C_IDS: &[(u16, u16)] = &[(0x8086, 0x54e8), (0x8086, 0x54ea)];

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;
    struct Fake {
        regs: [u32; 64],
        writes: Vec<(u32, u32)>,
    }
    impl Fake {
        fn new() -> Self {
            let mut r = [0; 64];
            r[(regs::ENABLE_STATUS / 4) as usize] = 1;
            r[(regs::STATUS / 4) as usize] = regs::STATUS_TX_NOT_FULL | regs::STATUS_RX_NOT_EMPTY;
            Self {
                regs: r,
                writes: Vec::new(),
            }
        }
    }
    impl RegisterIo for Fake {
        fn read32(&mut self, o: u32) -> u32 {
            self.regs[(o / 4) as usize]
        }
        fn write32(&mut self, o: u32, v: u32) {
            self.writes.push((o, v));
            if o < 0x100 {
                self.regs[(o / 4) as usize] = v;
            }
        }
        fn relax(&mut self) {}
    }
    #[test]
    fn validates_acpi_addresses_and_n305_ids() {
        assert!(AcpiI2cDevice::from_serial_bus(0x50, false, 100_000, "\\_SB.I2C0").is_ok());
        assert!(AcpiI2cDevice::from_serial_bus(0x80, false, 100_000, "x").is_err());
        assert_eq!(INTEL_LPSS_I2C_IDS.len(), 2);
    }
    #[test]
    fn configures_designware_master() {
        let c = Controller::new(Fake::new(), Timing::N305, Speed::Fast400Khz, 8).unwrap();
        assert_eq!(c.timing(), Timing::N305);
        assert_eq!(c.speed(), Speed::Fast400Khz);
    }
    #[test]
    fn rejects_empty_and_out_of_range_transfers() {
        let mut c = Controller::new(Fake::new(), Timing::N305, Speed::Standard100Khz, 8).unwrap();
        assert_eq!(c.transfer(&mut []), Err(Error::EmptyTransfer));
        let mut b = [0];
        let mut ms = [Message {
            address: Address {
                value: 0x80,
                ten_bit: false,
            },
            direction: Direction::Write,
            data: &mut b,
        }];
        assert_eq!(c.transfer(&mut ms), Err(Error::InvalidAddress));
    }
}
