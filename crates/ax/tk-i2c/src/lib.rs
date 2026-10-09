//! Intel LPSS DesignWare I2C platform bus driver.
//!
//! Function-level translation sources: FreeBSD `sys/dev/ichiic/ig4_iic.c`,
//! `ig4_pci.c`, `ig4_acpi.c`, `ig4_reg.h`, and `ig4_var.h`, source snapshot
//! 2026-10-08. `ig4_iic.c`, `ig4_pci.c`, `ig4_reg.h`, and `ig4_var.h` carry
//! DragonFly-derived BSD-3-Clause notices (Copyright (c) 2014 The DragonFly
//! Project); `ig4_acpi.c` is BSD-2-Clause (Copyright (c) 2016 Oleksandr
//! Tymoshenko <gonzo@FreeBSD.org>). Original copyright texts and licenses are
//! in `LICENSES/`.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod acpi;
pub mod i2cdev;
pub mod ig4;
pub mod pci;
pub mod reg;

pub use ig4::{
    Backend, Config, Hardware, IIC_M_NOSTART, IIC_M_NOSTOP, IIC_M_RD, Ig4, IicError, IicMessage,
    Version,
};

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
pub enum Error {
    InvalidAddress,
    EmptyTransfer,
    Busy,
    Timeout,
    Nack,
    InvalidConfiguration,
}

/// ACPI `I2cSerialBusV2` child address after ACPICA has validated descriptor kind.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_acpi_i2c_addresses() {
        assert!(AcpiI2cDevice::from_serial_bus(0x50, false, 100_000, "\\_SB.I2C0").is_ok());
        assert!(AcpiI2cDevice::from_serial_bus(0x80, false, 100_000, "x").is_err());
    }

    #[test]
    fn i2c_dev_ioctl_layouts_match_linux_x86_64_abi() {
        assert_eq!(i2cdev::I2C_FUNCS, 0x8008_0705);
        assert_eq!(i2cdev::I2C_RDWR, 0xc010_0707);
        assert_eq!(i2cdev::I2C_SMBUS, 0xc010_0720);
        assert_eq!(core::mem::size_of::<i2cdev::I2cMsg>(), 16);
        assert_eq!(core::mem::size_of::<i2cdev::I2cRdwrIoctlData>(), 16);
        assert_eq!(core::mem::size_of::<i2cdev::I2cSmbusIoctlData>(), 16);
    }
}
