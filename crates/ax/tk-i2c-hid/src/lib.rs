//! I2C-HID wire protocol framing and descriptor validation.
//!
//! Translated from FreeBSD `sys/dev/iicbus/iichid.c` and informed by its
//! BSD-2-Clause `sys/dev/hid/{hid.c,hidbus.c,hmt.c}` dispatch paths (FreeBSD
//! snapshot 2026-10-08). Copyright (c) 2018-2019 Marc Priggemeyer
//! <marc.priggemeyer@gmail.com>; Copyright (c) 2019-2020 and 2014-2020
//! Vladimir Kondratyev <wulf@FreeBSD.org>; Copyright (c) 1998 The NetBSD
//! Foundation, Inc. The NetBSD-derived HID parser was contributed by Lennart
//! Augustsson <lennart@augustsson.net>. See `LICENSES/BSD-2-Clause.txt`.
#![no_std]

#[cfg(test)]
extern crate std;

use core::fmt;

use tk_i2c::{Address, Error as BusError};

pub const HID_DESCRIPTOR_BYTES: usize = 30;
pub const RESET_TIMEOUT_SECONDS: u8 = 5;
pub const INPUT_SAMPLING_FAST_HZ: u8 = 80;
pub const INPUT_SAMPLING_SLOW_HZ: u8 = 10;
pub const INPUT_SAMPLING_HYSTERESIS: u8 = 16;
pub const HID_DESC_ACPI: u16 = 0xffff;
pub const MAX_REPORT_DESCRIPTOR_BYTES: usize = 4096;
pub const MAX_REPORT_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidDescriptor,
    InvalidPacket,
    PacketTooLarge,
    Unsupported,
    Bus(BusError),
}

impl From<BusError> for Error {
    fn from(value: BusError) -> Self {
        Self::Bus(value)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "I2C-HID error: {self:?}")
    }
}

/// Decoded HID-over-I2C descriptor (HID over I2C specification §5.1.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Descriptor {
    pub descriptor_length: u16,
    pub version: u16,
    pub report_descriptor_length: u16,
    pub report_descriptor_register: u16,
    pub input_register: u16,
    pub max_input_length: u16,
    pub output_register: u16,
    pub max_output_length: u16,
    pub command_register: u16,
    pub data_register: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version_id: u16,
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let b = bytes
        .get(offset..offset + 2)
        .ok_or(Error::InvalidDescriptor)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

impl Descriptor {
    /// Decode the fixed 30-byte little-endian `i2c_hid_desc` structure.
    // upstream: iichid.c iichid_attach()
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HID_DESCRIPTOR_BYTES {
            return Err(Error::InvalidDescriptor);
        }
        let d = Self {
            descriptor_length: le16(bytes, 0)?,
            version: le16(bytes, 2)?,
            report_descriptor_length: le16(bytes, 4)?,
            report_descriptor_register: le16(bytes, 6)?,
            input_register: le16(bytes, 8)?,
            max_input_length: le16(bytes, 10)?,
            output_register: le16(bytes, 12)?,
            max_output_length: le16(bytes, 14)?,
            command_register: le16(bytes, 16)?,
            data_register: le16(bytes, 18)?,
            vendor_id: le16(bytes, 20)?,
            product_id: le16(bytes, 22)?,
            version_id: le16(bytes, 24)?,
        };
        if usize::from(d.descriptor_length) < HID_DESCRIPTOR_BYTES
            || d.report_descriptor_length == 0
            || d.max_input_length < 2
            || d.input_register == 0
            || d.command_register == 0
            || d.data_register == 0
        {
            return Err(Error::InvalidDescriptor);
        }
        Ok(d)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Command {
    Reset       = 0x01,
    GetReport   = 0x02,
    SetReport   = 0x03,
    GetIdle     = 0x04,
    SetIdle     = 0x05,
    GetProtocol = 0x06,
    SetProtocol = 0x07,
    SetPower    = 0x08,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Power {
    On  = 0,
    Off = 1,
}

/// ACPI's _DSM result for the HID descriptor register address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiHidDescriptorAddress(pub u16);

impl AcpiHidDescriptorAddress {
    // upstream: iichid.c iichid_probe()
    pub const fn from_dsm_integer(value: u64) -> Result<Self, Error> {
        // FreeBSD masks the ACPICA integer to the 16-bit register field.
        Ok(Self(value as u16))
    }
}

/// I2C-HID transport interface. The platform adapter serializes all calls with
/// the I2C bus lock, as FreeBSD's iichid does.
pub trait Transport {
    fn address(&self) -> Address;
    fn write_register(&mut self, register: u16, bytes: &[u8]) -> Result<(), BusError>;
    fn read_register(&mut self, register: u16, bytes: &mut [u8]) -> Result<usize, BusError>;
    /// The input-report endpoint is read directly; unlike descriptor and
    /// command registers, the device returns the length-prefixed report at the
    /// current input endpoint without a register-address write.
    fn read_input(&mut self, bytes: &mut [u8]) -> Result<usize, BusError>;
    fn read_report(&mut self, command: &[u8], bytes: &mut [u8]) -> Result<usize, BusError>;
    fn write_report(&mut self, command: &[u8], bytes: &[u8]) -> Result<(), BusError>;
}

/// Minimal protocol operations. Report parsing and Linux input event creation
/// are intentionally handed to TheKernel's shared HID/input APIs by caller.
pub struct Device<T> {
    transport: T,
    descriptor: Descriptor,
}

impl<T: Transport> Device<T> {
    pub fn new(transport: T, descriptor: Descriptor) -> Self {
        Self {
            transport,
            descriptor,
        }
    }
    pub fn descriptor(&self) -> Descriptor {
        self.descriptor
    }
    pub fn transport_address(&self) -> Address {
        self.transport.address()
    }

    /// Execute RESET using the HID-over-I2C command register, then poll the
    /// input register for the reset-complete zero-length packet.
    // upstream: iichid.c iichid_reset()
    pub fn reset(&mut self, poll_limit: usize) -> Result<(), Error> {
        self.transport
            .write_register(self.descriptor.command_register, &[0, Command::Reset as u8])?;
        let mut status = [0u8; 2];
        for _ in 0..poll_limit {
            let n = self.transport.read_input(&mut status)?;
            if n >= 2 {
                let length = u16::from_le_bytes(status);
                if length == 0 {
                    return Ok(());
                }
            }
        }
        Err(Error::InvalidPacket)
    }

    /// Power-on/off through SET_POWER. The HID protocol's low byte is power
    /// state, high byte is reserved and remains zero.
    // upstream: iichid.c iichid_set_power()
    pub fn set_power(&mut self, power: Power) -> Result<(), Error> {
        let command = [power as u8, Command::SetPower as u8];
        self.transport
            .write_register(self.descriptor.command_register, &command)?;
        Ok(())
    }

    /// Read one input report. The two-byte length prefix is part of the I2C-HID
    /// packet; zero indicates no report, and lengths are checked before copy.
    // upstream: iichid.c iichid_intr()
    pub fn read_input(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let max = usize::from(self.descriptor.max_input_length);
        if out.len().saturating_add(2) > max {
            return Err(Error::PacketTooLarge);
        }
        let mut framed = [0u8; 1024];
        if max > framed.len() {
            return Err(Error::PacketTooLarge);
        }
        let n = self.transport.read_input(&mut framed[..max])?;
        if n < 2 {
            return Err(Error::InvalidPacket);
        }
        let length = usize::from(u16::from_le_bytes([framed[0], framed[1]]));
        if length == 0 {
            return Ok(0);
        }
        if length < 2 || length > n || length > max {
            return Err(Error::InvalidPacket);
        }
        let payload = length - 2;
        if payload > out.len() {
            return Err(Error::PacketTooLarge);
        }
        out[..payload].copy_from_slice(&framed[2..length]);
        Ok(payload)
    }

    /// Fetch the HID report descriptor from the descriptor's report register.
    // upstream: iichid.c iichid_cmd_get_report_desc()
    pub fn report_descriptor(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let length = usize::from(self.descriptor.report_descriptor_length);
        if length == 0 || length > MAX_REPORT_DESCRIPTOR_BYTES || out.len() < length {
            return Err(Error::PacketTooLarge);
        }
        let n = self.transport.read_register(
            self.descriptor.report_descriptor_register,
            &mut out[..length],
        )?;
        if n != length {
            return Err(Error::InvalidPacket);
        }
        Ok(n)
    }

    /// Retrieve the fixed HID-over-I2C descriptor before creating a Device.
    // upstream: iichid.c iichid_cmd_get_hid_desc()
    pub fn read_descriptor(
        transport: &mut T,
        address: AcpiHidDescriptorAddress,
    ) -> Result<Descriptor, Error> {
        let mut raw = [0u8; HID_DESCRIPTOR_BYTES];
        let n = transport.read_register(address.0, &mut raw)?;
        if n != raw.len() {
            return Err(Error::InvalidDescriptor);
        }
        Descriptor::decode(&raw)
    }

    /// Send a HID output report through the Output Register (HID-I2C §6.2.3).
    // upstream: iichid.c iichid_cmd_write()
    pub fn write_output(&mut self, report: &[u8]) -> Result<(), Error> {
        if self.descriptor.output_register == 0
            || self.descriptor.max_output_length == 0
            || report.len() < 2
            || report.len() > MAX_REPORT_BYTES
            || report
                .len()
                .checked_add(2)
                .is_none_or(|n| n > usize::from(self.descriptor.max_output_length))
        {
            return Err(Error::PacketTooLarge);
        }
        let length = u16::try_from(report.len() + 2).map_err(|_| Error::PacketTooLarge)?;
        let mut packet = [0u8; MAX_REPORT_BYTES + 2];
        packet[..2].copy_from_slice(&length.to_le_bytes());
        packet[2..2 + report.len()].copy_from_slice(report);
        self.transport
            .write_register(self.descriptor.output_register, &packet[..2 + report.len()])?;
        Ok(())
    }

    /// GET_REPORT command and response transfer (HID-I2C §7.2.2.2).
    // upstream: iichid.c iichid_cmd_get_report()
    pub fn get_report(
        &mut self,
        report_type: u8,
        report_id: u8,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        if !(1..=3).contains(&report_type) || out.is_empty() || out.len() > MAX_REPORT_BYTES {
            return Err(Error::PacketTooLarge);
        }
        let extended = report_id >= 15;
        let mut command = [0u8; 7];
        command[..2].copy_from_slice(&self.descriptor.command_register.to_le_bytes());
        command[2] = (if extended { 15 } else { report_id }) | (report_type << 4);
        command[3] = Command::GetReport as u8;
        if extended {
            command[4] = report_id;
            command[5..7].copy_from_slice(&self.descriptor.data_register.to_le_bytes());
        } else {
            command[4..6].copy_from_slice(&self.descriptor.data_register.to_le_bytes());
        }
        let command_len = if extended { 7 } else { 6 };
        let mut framed = [0u8; MAX_REPORT_BYTES + 2];
        let n = self
            .transport
            .read_report(&command[..command_len], &mut framed[..out.len() + 2])?;
        if n < 2 {
            return Err(Error::InvalidPacket);
        }
        let length = usize::from(u16::from_le_bytes([framed[0], framed[1]]));
        if length <= 2 || length > n || length > out.len() + 2 || length == usize::from(u16::MAX) {
            return Err(Error::InvalidPacket);
        }
        let payload_len = length - 2;
        let actual_id = if payload_len == 0 { 0 } else { framed[2] };
        if report_id != 0 && actual_id != report_id {
            return Err(Error::InvalidPacket);
        }
        out[..payload_len].copy_from_slice(&framed[2..length]);
        Ok(payload_len)
    }

    /// SET_REPORT command followed by its report payload (HID-I2C §7.2.2.3).
    // upstream: iichid.c iichid_cmd_set_report()
    pub fn set_report(
        &mut self,
        report_type: u8,
        report_id: u8,
        report: &[u8],
    ) -> Result<(), Error> {
        if !(1..=3).contains(&report_type)
            || report.len() < 2
            || report.len() > MAX_REPORT_BYTES
            || report
                .len()
                .checked_add(2)
                .is_none_or(|n| n > usize::from(u16::MAX))
        {
            return Err(Error::PacketTooLarge);
        }
        let extended = report_id >= 15;
        let report_length = u16::try_from(report.len() + 2).map_err(|_| Error::PacketTooLarge)?;
        let mut command = [0u8; 9];
        command[..2].copy_from_slice(&self.descriptor.command_register.to_le_bytes());
        command[2] = (if extended { 15 } else { report_id }) | (report_type << 4);
        command[3] = Command::SetReport as u8;
        if extended {
            command[4] = report_id;
            command[5..7].copy_from_slice(&self.descriptor.data_register.to_le_bytes());
            command[7..9].copy_from_slice(&report_length.to_le_bytes());
        } else {
            command[4..6].copy_from_slice(&self.descriptor.data_register.to_le_bytes());
            command[6..8].copy_from_slice(&report_length.to_le_bytes());
        }
        self.transport
            .write_report(&command[..if extended { 9 } else { 8 }], report)?;
        Ok(())
    }

    /// FreeBSD returns ENOTSUPP for the unused idle/protocol controls.
    // upstream: iichid.c iichid_set_idle()
    pub fn set_idle(&mut self, _duration: u16, _report_id: u8) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// FreeBSD returns ENOTSUPP; HID-over-I2C devices remain in report mode.
    // upstream: iichid.c iichid_set_protocol()
    pub fn set_protocol(&mut self, _protocol: u16) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::{vec, vec::Vec};

    use super::*;
    struct Fake {
        writes: Vec<(u16, Vec<u8>)>,
        read: Vec<u8>,
    }
    impl Transport for Fake {
        fn address(&self) -> Address {
            Address::seven_bit(0x2c).unwrap()
        }
        fn write_register(&mut self, r: u16, b: &[u8]) -> Result<(), BusError> {
            self.writes.push((r, b.into()));
            Ok(())
        }
        fn read_register(&mut self, _: u16, b: &mut [u8]) -> Result<usize, BusError> {
            let n = b.len().min(self.read.len());
            b[..n].copy_from_slice(&self.read[..n]);
            Ok(n)
        }
        fn read_input(&mut self, b: &mut [u8]) -> Result<usize, BusError> {
            let n = b.len().min(self.read.len());
            b[..n].copy_from_slice(&self.read[..n]);
            Ok(n)
        }
        fn read_report(&mut self, _: &[u8], b: &mut [u8]) -> Result<usize, BusError> {
            let n = b.len().min(self.read.len());
            b[..n].copy_from_slice(&self.read[..n]);
            Ok(n)
        }
        fn write_report(&mut self, command: &[u8], bytes: &[u8]) -> Result<(), BusError> {
            let mut payload = command.to_vec();
            payload.extend_from_slice(bytes);
            self.writes
                .push((u16::from_le_bytes([command[0], command[1]]), payload));
            Ok(())
        }
    }
    fn desc() -> Descriptor {
        Descriptor {
            descriptor_length: 30,
            version: 0x0100,
            report_descriptor_length: 100,
            report_descriptor_register: 0x20,
            input_register: 0x21,
            max_input_length: 64,
            output_register: 0x22,
            max_output_length: 32,
            command_register: 0x23,
            data_register: 0x24,
            vendor_id: 0x1234,
            product_id: 0x5678,
            version_id: 1,
        }
    }
    #[test]
    fn decodes_i2c_hid_descriptor_and_dsm_value() {
        let d = desc();
        let mut raw = [0u8; 30];
        for (i, v) in [
            d.descriptor_length,
            d.version,
            d.report_descriptor_length,
            d.report_descriptor_register,
            d.input_register,
            d.max_input_length,
            d.output_register,
            d.max_output_length,
            d.command_register,
            d.data_register,
            d.vendor_id,
            d.product_id,
            d.version_id,
        ]
        .into_iter()
        .enumerate()
        {
            raw[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        assert_eq!(Descriptor::decode(&raw).unwrap(), d);
        assert_eq!(
            AcpiHidDescriptorAddress::from_dsm_integer(0xabcd),
            Ok(AcpiHidDescriptorAddress(0xabcd))
        );
        assert_eq!(
            AcpiHidDescriptorAddress::from_dsm_integer(0x1_0000),
            Ok(AcpiHidDescriptorAddress(0))
        );
    }
    #[test]
    fn frames_input_reports_and_powers_up() {
        let mut raw = vec![5, 0, 9, 8, 7];
        let mut d = Device::new(
            Fake {
                writes: Vec::new(),
                read: raw.clone(),
            },
            desc(),
        );
        let mut out = [0u8; 8];
        assert_eq!(d.read_input(&mut out), Ok(3));
        assert_eq!(&out[..3], &[9, 8, 7]);
        d.set_power(Power::On).unwrap();
        let fake = d.transport;
        assert_eq!(fake.writes, vec![(0x23, vec![0, 8])]);
        raw.clear();
    }
    #[test]
    fn rejects_short_descriptor_and_oversized_output() {
        assert_eq!(Descriptor::decode(&[0; 29]), Err(Error::InvalidDescriptor));
        let mut d = Device::new(
            Fake {
                writes: Vec::new(),
                read: vec![4, 0, 1, 2],
            },
            desc(),
        );
        let mut out = [0; 1];
        assert_eq!(d.read_input(&mut out), Err(Error::PacketTooLarge));
    }

    #[test]
    fn reset_uses_command_parameter_order_and_zero_length_ack() {
        let mut device = Device::new(
            Fake {
                writes: Vec::new(),
                read: vec![0, 0],
            },
            desc(),
        );
        assert_eq!(device.reset(1), Ok(()));
        assert_eq!(device.transport.writes, vec![(0x23, vec![0, 1])]);
    }

    #[test]
    fn report_descriptor_uses_declared_size_and_rejects_short_read() {
        let mut device = Device::new(
            Fake {
                writes: Vec::new(),
                read: vec![0x05; 100],
            },
            desc(),
        );
        let mut bytes = [0; 100];
        assert_eq!(device.report_descriptor(&mut bytes), Ok(100));
        assert_eq!(bytes, [0x05; 100]);
        let mut short = Device::new(
            Fake {
                writes: Vec::new(),
                read: vec![0x05; 8],
            },
            desc(),
        );
        assert_eq!(
            short.report_descriptor(&mut bytes),
            Err(Error::InvalidPacket)
        );
    }

    #[test]
    fn get_and_set_report_encode_short_and_extended_report_ids() {
        let mut get = Device::new(
            Fake {
                writes: Vec::new(),
                read: vec![4, 0, 1, 0xaa],
            },
            desc(),
        );
        let mut report = [0; 4];
        assert_eq!(get.get_report(1, 1, &mut report), Ok(2));
        assert_eq!(&report[..2], &[1, 0xaa]);

        let mut set = Device::new(
            Fake {
                writes: Vec::new(),
                read: Vec::new(),
            },
            desc(),
        );
        assert_eq!(set.set_report(3, 15, &[15, 0xaa]), Ok(()));
        assert_eq!(
            set.transport.writes,
            vec![(0x23, vec![0x23, 0, 0x3f, 3, 15, 0x24, 0, 4, 0, 15, 0xaa])]
        );
    }

    #[test]
    fn idle_and_protocol_controls_match_upstream_not_supported_results() {
        let mut device = Device::new(
            Fake {
                writes: Vec::new(),
                read: Vec::new(),
            },
            desc(),
        );
        assert_eq!(device.set_idle(0, 0), Err(Error::Unsupported));
        assert_eq!(device.set_protocol(1), Err(Error::Unsupported));
    }
}
