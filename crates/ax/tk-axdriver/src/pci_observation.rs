//! Read-only PCI observations over the already mapped firmware ECAM window.
//! No BAR sizing, device enabling, or configuration writes happen here.
use alloc::vec::Vec;
use core::fmt;

use crate::prelude::{DevError, DevResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Address {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{}",
            self.segment, self.bus, self.device, self.function
        )
    }
}
impl Address {
    pub fn parse(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        if bytes.len() != 12
            || bytes[4] != b':'
            || bytes[7] != b':'
            || bytes[10] != b'.'
            || !bytes
                .iter()
                .enumerate()
                .all(|(i, b)| matches!(i, 4 | 7 | 10) || b.is_ascii_hexdigit())
        {
            return None;
        }
        let value = Self {
            segment: u16::from_str_radix(&name[..4], 16).ok()?,
            bus: u8::from_str_radix(&name[5..7], 16).ok()?,
            device: u8::from_str_radix(&name[8..10], 16).ok()?,
            function: u8::from_str_radix(&name[11..], 16).ok()?,
        };
        (value.device < 32 && value.function < 8).then_some(value)
    }
    pub(crate) fn ecam_offset(self, segment: u16, range: (u8, u8), offset: usize) -> Option<usize> {
        if self.segment != segment
            || self.device >= 32
            || self.function >= 8
            || self.bus < range.0
            || self.bus > range.1
            || offset >= 4096
        {
            return None;
        }
        Some(
            (((self.bus - range.0) as usize) << 20)
                | ((self.device as usize) << 15)
                | ((self.function as usize) << 12)
                | offset,
        )
    }
}

pub fn inventory() -> DevResult<Vec<Address>> {
    #[cfg(bus = "pci")]
    {
        crate::bus::pci::observe_inventory()
    }
    #[cfg(not(bus = "pci"))]
    {
        Ok(Vec::new())
    }
}
fn word(address: Address, offset: usize) -> Option<u32> {
    #[cfg(bus = "pci")]
    {
        crate::bus::pci::observe_config_word(address, offset)
    }
    #[cfg(not(bus = "pci"))]
    {
        let _ = (address, offset);
        None
    }
}

pub fn header(address: Address) -> Option<[u8; 64]> {
    let mut out = [0; 64];
    for (offset, chunk) in out.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&word(address, offset * 4)?.to_le_bytes());
    }
    (u16::from_le_bytes([out[0], out[1]]) != 0xffff).then_some(out)
}
fn capability_with(
    address: Address,
    header: &[u8; 64],
    id: u8,
    mut read: impl FnMut(Address, usize) -> Option<u32>,
) -> Option<usize> {
    if header[6] & 0x10 == 0 {
        return None;
    }
    let mut pointer = usize::from(header[if header[14] & 0x7f == 2 { 0x14 } else { 0x34 }] & 0xfc);
    let mut visited = [false; 256];
    while pointer >= 0x40 && !visited[pointer] {
        visited[pointer] = true;
        let entry = read(address, pointer)?;
        if entry as u8 == id {
            return Some(pointer);
        }
        pointer = (entry >> 8) as usize & 0xfc;
    }
    None
}
fn capability(address: Address, header: &[u8; 64], id: u8) -> Option<usize> {
    capability_with(address, header, id, word)
}

pub fn subsystem(address: Address, header: &[u8; 64]) -> Option<(u16, u16)> {
    let value = match header[14] & 0x7f {
        0 => u32::from_le_bytes(header[0x2c..0x30].try_into().ok()?),
        1 => match capability(address, header, 0x0d) {
            Some(pos) => word(address, pos + 4)?,
            None => 0,
        },
        2 => word(address, 0x40)?,
        _ => return None,
    };
    Some((value as u16, (value >> 16) as u16))
}
fn config_size(address: Address, header: &[u8; 64]) -> usize {
    let host_bridge = header[11] == 6 && header[10] == 0;
    let express = capability(address, header, 0x10).is_some();
    let extended_pcix = capability(address, header, 7)
        .and_then(|pos| word(address, pos + 4))
        .is_some_and(|status| status & 0xc0000000 != 0);
    if !host_bridge && !express && !extended_pcix {
        return 256;
    }
    let Some(first) = word(address, 0x100) else {
        return 256;
    };
    if first == u32::MAX {
        return 256;
    }
    let vendor_device = u32::from_le_bytes(header[..4].try_into().unwrap());
    if (0x100..4096)
        .step_by(256)
        .all(|offset| word(address, offset) == Some(vendor_device))
    {
        return 256;
    }
    4096
}

/// Configuration length discovered from conventional capabilities and ECAM
/// reachability. This is a kernel inventory observation, not an access grant.
pub fn configuration_size(address: Address) -> Option<usize> {
    Some(config_size(address, &header(address)?))
}

fn read_bytes_with(
    offset: usize,
    output: &mut [u8],
    mut read: impl FnMut(usize, usize) -> Option<u32>,
) -> DevResult<usize> {
    if offset > 4096 || output.len() > 4096 - offset {
        return Err(DevError::InvalidParam);
    }
    let mut done = 0;
    while done < output.len() {
        let at = offset + done;
        let remaining = output.len() - done;
        let width = if at.is_multiple_of(4) && remaining >= 4 {
            4
        } else if at.is_multiple_of(2) && remaining >= 2 {
            2
        } else {
            1
        };
        let value = read(at, width).ok_or(DevError::BadState)?.to_le_bytes();
        output[done..done + width].copy_from_slice(&value[..width]);
        done += width;
    }
    Ok(done)
}

/// Read exactly the admitted byte interval, with width-correct volatile loads.
/// The caller must apply the opener's configuration-space access limit first.
pub fn read_configuration(address: Address, offset: usize, output: &mut [u8]) -> DevResult<usize> {
    read_bytes_with(offset, output, |at, width| {
        #[cfg(bus = "pci")]
        {
            crate::bus::pci::observe_config_value(address, at, width)
        }
        #[cfg(not(bus = "pci"))]
        {
            let _ = (address, at, width);
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_reads_never_touch_bytes_outside_the_requested_interval() {
        let mut output = [0; 8];
        let mut reads = Vec::new();
        assert_eq!(
            read_bytes_with(3, &mut output, |at, width| {
                reads.push((at, width));
                Some(u32::from_le_bytes(core::array::from_fn(|i| (at + i) as u8)))
            })
            .unwrap(),
            8
        );
        assert_eq!(reads, [(3, 1), (4, 4), (8, 2), (10, 1)]);
        assert_eq!(output, [3, 4, 5, 6, 7, 8, 9, 10]);
        assert!(
            read_bytes_with(4095, &mut [0; 2], |_, _| panic!("out of function range")).is_err()
        );
        assert_eq!(
            read_bytes_with(63, &mut [0; 1], |at, width| {
                assert_eq!((at, width), (63, 1));
                Some(0xaa)
            })
            .unwrap(),
            1
        );
    }
    #[test]
    fn pci_names_and_ecam_bounds_reject_cross_function_reads() {
        let address = Address::parse("0002:03:1f.7").unwrap();
        assert_eq!(alloc::format!("{address}"), "0002:03:1f.7");
        assert_eq!(
            address.ecam_offset(2, (2, 3), 4092),
            Some(2 * 1024 * 1024 - 4)
        );
        for name in [
            "0000:00:20.0",
            "0000:00:01.8",
            "0000:00:01.0/",
            "0000:00:01.é",
            "ffff:ff:ff.f",
        ] {
            assert!(Address::parse(name).is_none());
        }
        assert_eq!(address.ecam_offset(0, (2, 3), 0), None);
        assert_eq!(address.ecam_offset(2, (0, 2), 0), None);
        assert_eq!(address.ecam_offset(2, (2, 3), 4096), None);
        assert_eq!(
            address.ecam_offset(2, (2, 3), 1),
            Some(2 * 1024 * 1024 - 4095)
        );
    }
    #[test]
    fn capability_walk_is_bounded_and_does_not_follow_malformed_cycles() {
        let address = Address::parse("0000:00:01.0").unwrap();
        let mut header = [0; 64];
        header[6] = 0x10;
        header[0x34] = 0x40;
        assert_eq!(
            capability_with(address, &header, 0x10, |_, _| Some(0x4005)),
            None
        );
        assert_eq!(
            capability_with(address, &header, 0x10, |_, offset| Some(
                if offset == 0x40 { 0x5005 } else { 0x10 }
            )),
            Some(0x50)
        );
        header[0x34] = 0x20;
        assert_eq!(
            capability_with(address, &header, 0x10, |_, _| panic!(
                "invalid capability must not be read"
            )),
            None
        );
    }
}
