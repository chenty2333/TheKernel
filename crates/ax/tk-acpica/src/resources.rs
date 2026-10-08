//! Checked AML resource-template decoding for interrupt links and EC ports.
use alloc::vec::Vec;

use crate::{BAD_PARAMETER, NO_MEMORY, Status};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Irq {
    pub numbers: Vec<u32>,
    pub level: bool,
    pub active_low: bool,
    pub shared: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub irqs: Vec<Irq>,
    pub io: Vec<(u16, u8)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRange {
    pub base: u64,
    pub length: u64,
}

/// Decode assigned memory windows from an ACPI resource template. Address
/// descriptors are accepted only for memory space; ranges remain physical
/// addresses for the platform's MMIO mapper.
pub fn parse_memory_ranges(bytes: &[u8]) -> Result<Vec<MemoryRange>, Status> {
    let mut ranges = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let tag = *bytes.get(at).ok_or(BAD_PARAMETER)?;
        at += 1;
        let (kind, length) = if tag & 0x80 != 0 {
            let raw = bytes.get(at..at + 2).ok_or(BAD_PARAMETER)?;
            at += 2;
            (
                tag,
                usize::from(u16::from_le_bytes(raw.try_into().unwrap())),
            )
        } else {
            (tag >> 3, usize::from(tag & 7))
        };
        let end = at.checked_add(length).ok_or(BAD_PARAMETER)?;
        let item = bytes.get(at..end).ok_or(BAD_PARAMETER)?;
        at = end;
        if kind == 0x0f {
            if length != 1 || at != bytes.len() {
                return Err(BAD_PARAMETER);
            }
            if item[0] != 0 && bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
                return Err(BAD_PARAMETER);
            }
            return Ok(ranges);
        }
        let parsed = match kind {
            0x81 if length == 9 => Some((
                u64::from(u16::from_le_bytes(item[1..3].try_into().unwrap())),
                u64::from(u16::from_le_bytes(item[7..9].try_into().unwrap())),
            )),
            0x85 if length == 17 => {
                let minimum = u64::from(u32::from_le_bytes(item[1..5].try_into().unwrap()));
                let maximum = u64::from(u32::from_le_bytes(item[5..9].try_into().unwrap()));
                if minimum == maximum {
                    Some((
                        minimum,
                        u64::from(u32::from_le_bytes(item[13..17].try_into().unwrap())),
                    ))
                } else {
                    None
                }
            }
            0x86 if length == 9 => Some((
                u64::from(u32::from_le_bytes(item[1..5].try_into().unwrap())),
                u64::from(u32::from_le_bytes(item[5..9].try_into().unwrap())),
            )),
            0x87 if length == 23 && item[0] == 0 => {
                let minimum = u64::from(u32::from_le_bytes(item[7..11].try_into().unwrap()));
                let maximum = u64::from(u32::from_le_bytes(item[11..15].try_into().unwrap()));
                let translation = u64::from(u32::from_le_bytes(item[15..19].try_into().unwrap()));
                if minimum == maximum {
                    Some((
                        minimum.checked_add(translation).ok_or(BAD_PARAMETER)?,
                        u64::from(u32::from_le_bytes(item[19..23].try_into().unwrap())),
                    ))
                } else {
                    None
                }
            }
            0x8a if length == 43 && item[0] == 0 => {
                let minimum = u64::from_le_bytes(item[11..19].try_into().unwrap());
                let maximum = u64::from_le_bytes(item[19..27].try_into().unwrap());
                let translation = u64::from_le_bytes(item[27..35].try_into().unwrap());
                if minimum == maximum {
                    Some((
                        minimum.checked_add(translation).ok_or(BAD_PARAMETER)?,
                        u64::from_le_bytes(item[35..43].try_into().unwrap()),
                    ))
                } else {
                    None
                }
            }
            0x81 | 0x85 | 0x86 | 0x87 | 0x8a => return Err(BAD_PARAMETER),
            _ => None,
        };
        if let Some((base, length)) = parsed {
            if length == 0 || base.checked_add(length).is_none() {
                return Err(BAD_PARAMETER);
            }
            push(&mut ranges, MemoryRange { base, length })?;
        }
    }
    Err(BAD_PARAMETER)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioTrigger {
    Level,
    Edge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioPolarity {
    ActiveHigh,
    ActiveLow,
    Both,
}

/// Checked ACPI GpioInt descriptor. This is resource metadata only; it is not
/// an allocated GPIO pin or a routable kernel interrupt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpioInterrupt {
    pub resource_source: Vec<u8>,
    pub source_index: u8,
    pub pins: Vec<u16>,
    pub trigger: GpioTrigger,
    pub polarity: GpioPolarity,
    pub shared: bool,
    pub wake_capable: bool,
    pub debounce_timeout_us: u16,
    pub pin_config: u8,
}

/// Decode ACPI GPIO interrupt descriptors while retaining the controller
/// source and electrical/timing metadata needed by a future GPIO provider.
pub fn parse_gpio_interrupts(bytes: &[u8]) -> Result<Vec<GpioInterrupt>, Status> {
    let mut interrupts = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let tag = *bytes.get(at).ok_or(BAD_PARAMETER)?;
        at += 1;
        let (kind, length) = if tag & 0x80 != 0 {
            let size = bytes.get(at..at + 2).ok_or(BAD_PARAMETER)?;
            at += 2;
            (
                tag,
                usize::from(u16::from_le_bytes(size.try_into().unwrap())),
            )
        } else {
            (tag >> 3, usize::from(tag & 7))
        };
        let end = at.checked_add(length).ok_or(BAD_PARAMETER)?;
        let item = bytes.get(at..end).ok_or(BAD_PARAMETER)?;
        at = end;
        if kind == 0x0f {
            if length != 1 || at != bytes.len() {
                return Err(BAD_PARAMETER);
            }
            if item[0] != 0 && bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
                return Err(BAD_PARAMETER);
            }
            return Ok(interrupts);
        }
        if kind != 0x8c {
            continue;
        }
        if length < 20 || item[0] != 1 || item[1] != 0 {
            return Err(BAD_PARAMETER);
        }
        let flags = u16::from_le_bytes(item[2..4].try_into().unwrap());
        let int_flags = u16::from_le_bytes(item[4..6].try_into().unwrap());
        let pin_config = item[6];
        if pin_config > 3 || flags & !0x07 != 0 || int_flags & !0x0f != 0 {
            return Err(BAD_PARAMETER);
        }
        let pin_table_offset = usize::from(u16::from_le_bytes(item[11..13].try_into().unwrap()));
        let source_index = item[13];
        let source_offset = usize::from(u16::from_le_bytes(item[14..16].try_into().unwrap()));
        let vendor_offset = usize::from(u16::from_le_bytes(item[16..18].try_into().unwrap()));
        let vendor_length = usize::from(u16::from_le_bytes(item[18..20].try_into().unwrap()));
        let pin_start = pin_table_offset.checked_sub(3).ok_or(BAD_PARAMETER)?;
        let source_start = source_offset.checked_sub(3).ok_or(BAD_PARAMETER)?;
        if pin_start < 20 || source_start <= pin_start || source_start > item.len() {
            return Err(BAD_PARAMETER);
        }
        let pin_bytes = source_start - pin_start;
        if pin_bytes == 0 || pin_bytes % 2 != 0 {
            return Err(BAD_PARAMETER);
        }
        let source_end = if vendor_offset == 0 {
            item.len()
        } else {
            vendor_offset.checked_sub(3).ok_or(BAD_PARAMETER)?
        };
        if source_end <= source_start || source_end > item.len() {
            return Err(BAD_PARAMETER);
        }
        if vendor_length != 0
            && (vendor_offset == 0 || source_end.checked_add(vendor_length) != Some(item.len()))
        {
            return Err(BAD_PARAMETER);
        }
        let source = &item[source_start..source_end];
        let nul = source
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(BAD_PARAMETER)?;
        if nul == 0 || source[nul + 1..].iter().any(|byte| *byte != 0) {
            return Err(BAD_PARAMETER);
        }
        let polarity = match (int_flags >> 1) & 3 {
            0 => GpioPolarity::ActiveHigh,
            1 => GpioPolarity::ActiveLow,
            2 => GpioPolarity::Both,
            _ => return Err(BAD_PARAMETER),
        };
        let mut resource_source = Vec::new();
        resource_source
            .try_reserve_exact(nul)
            .map_err(|_| NO_MEMORY)?;
        resource_source.extend_from_slice(&source[..nul]);
        let mut pins = Vec::new();
        for pin in item[pin_start..source_start].chunks_exact(2) {
            push(&mut pins, u16::from_le_bytes(pin.try_into().unwrap()))?;
        }
        push(
            &mut interrupts,
            GpioInterrupt {
                resource_source,
                source_index,
                pins,
                trigger: if int_flags & 1 == 0 {
                    GpioTrigger::Level
                } else {
                    GpioTrigger::Edge
                },
                polarity,
                shared: flags & 2 != 0 || int_flags & 8 != 0,
                wake_capable: flags & 4 != 0,
                debounce_timeout_us: u16::from_le_bytes(item[9..11].try_into().unwrap()),
                pin_config,
            },
        )?;
    }
    Err(BAD_PARAMETER)
}
fn push<T>(v: &mut Vec<T>, item: T) -> Result<(), Status> {
    v.try_reserve(1).map_err(|_| NO_MEMORY)?;
    v.push(item);
    Ok(())
}
pub fn parse(bytes: &[u8]) -> Result<Resources, Status> {
    let mut result = Resources::default();
    let mut at = 0;
    while at < bytes.len() {
        let tag = bytes[at];
        at += 1;
        let (kind, len) = if tag & 0x80 != 0 {
            let size = bytes.get(at..at + 2).ok_or(BAD_PARAMETER)?;
            at += 2;
            (
                tag,
                usize::from(u16::from_le_bytes(size.try_into().unwrap())),
            )
        } else {
            (tag >> 3, usize::from(tag & 7))
        };
        let b = bytes
            .get(at..at.checked_add(len).ok_or(BAD_PARAMETER)?)
            .ok_or(BAD_PARAMETER)?;
        at += len;
        match kind {
            0xf => {
                if len != 1 || at != bytes.len() {
                    return Err(BAD_PARAMETER);
                }
                if b[0] != 0 && bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
                    return Err(BAD_PARAMETER);
                }
                return Ok(result);
            }
            4 => {
                if !matches!(len, 2 | 3) {
                    return Err(BAD_PARAMETER);
                }
                let mask = u16::from_le_bytes(b[..2].try_into().unwrap());
                let flags = b.get(2).copied().unwrap_or(1);
                let mut nums = Vec::new();
                for i in 0..16 {
                    if mask & (1 << i) != 0 {
                        push(&mut nums, i)?;
                    }
                }
                push(
                    &mut result.irqs,
                    Irq {
                        numbers: nums,
                        level: flags & 1 == 0,
                        active_low: flags & 8 != 0,
                        shared: flags & 16 != 0,
                    },
                )?;
            }
            8 => {
                if len != 7 {
                    return Err(BAD_PARAMETER);
                }
                let min = u16::from_le_bytes(b[1..3].try_into().unwrap());
                let max = u16::from_le_bytes(b[3..5].try_into().unwrap());
                if min != max || u32::from(min) + u32::from(b[6]) > 65536 {
                    return Err(BAD_PARAMETER);
                }
                push(&mut result.io, (min, b[6]))?;
            }
            9 => {
                if len != 3 {
                    return Err(BAD_PARAMETER);
                }
                let min = u16::from_le_bytes(b[..2].try_into().unwrap());
                if u32::from(min) + u32::from(b[2]) > 65536 {
                    return Err(BAD_PARAMETER);
                }
                push(&mut result.io, (min, b[2]))?;
            }
            0x89 => {
                if len < 2 {
                    return Err(BAD_PARAMETER);
                }
                let flags = b[0];
                let count = usize::from(b[1]);
                if count == 0 || len < 2 + count * 4 {
                    return Err(BAD_PARAMETER);
                }
                let mut nums = Vec::new();
                for v in b[2..2 + count * 4].chunks_exact(4) {
                    push(&mut nums, u32::from_le_bytes(v.try_into().unwrap()))?;
                }
                push(
                    &mut result.irqs,
                    Irq {
                        numbers: nums,
                        level: flags & 2 == 0,
                        active_low: flags & 4 != 0,
                        shared: flags & 8 != 0,
                    },
                )?;
            }
            // Other descriptors are length-checked and skipped. They cannot
            // fabricate an IRQ/EC port when an unsupported resource is present.
            _ => {}
        }
    }
    Err(BAD_PARAMETER)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    #[test]
    fn ec_io_and_irq() {
        let b = [
            0x47, 1, 0x62, 0, 0x62, 0, 1, 1, 0x4b, 0x66, 0, 1, 0x23, 0, 2, 0x18, 0x79, 0,
        ];
        let r = parse(&b).unwrap();
        assert_eq!(r.io, [(0x62, 1), (0x66, 1)]);
        assert_eq!(r.irqs[0].numbers, [9]);
        assert!(r.irqs[0].level && r.irqs[0].active_low && r.irqs[0].shared);
        for end in 0..b.len() {
            assert!(parse(&b[..end]).is_err());
        }
    }
    #[test]
    fn rejects_overflow_and_variable_ports() {
        assert!(parse(&[0x4b, 0xff, 0xff, 2, 0x79, 0]).is_err());
        assert!(parse(&[0x47, 1, 0x62, 0, 0x63, 0, 1, 1, 0x79, 0]).is_err());
    }
    #[test]
    fn extended_irq() {
        let r = parse(&[0x89, 6, 0, 0x0c, 1, 0x15, 0, 0, 0, 0x79, 0]).unwrap();
        assert_eq!(r.irqs[0].numbers, [21]);
        assert!(r.irqs[0].level && r.irqs[0].active_low && r.irqs[0].shared);
    }

    #[test]
    fn gpio_int_preserves_controller_pin_and_interrupt_configuration() {
        let source = b"\\_SB.GPI0\0";
        let mut payload = vec![
            1, 0, 6, 0, 0x0c, 0, 1, 0, 0, 0x20, 0, 23, 0, 2, 27, 0, 0, 0, 0, 0,
        ];
        payload.extend_from_slice(&4u16.to_le_bytes());
        payload.extend_from_slice(&7u16.to_le_bytes());
        payload.extend_from_slice(source);
        let mut descriptor = vec![0x8c];
        descriptor.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        descriptor.extend_from_slice(&payload);
        descriptor.extend_from_slice(&[0x79, 0]);
        let irq = parse_gpio_interrupts(&descriptor).unwrap().remove(0);
        assert_eq!(irq.resource_source, b"\\_SB.GPI0");
        assert_eq!(irq.source_index, 2);
        assert_eq!(irq.pins, [4, 7]);
        assert_eq!(irq.trigger, GpioTrigger::Level);
        assert_eq!(irq.polarity, GpioPolarity::Both);
        assert!(irq.shared && irq.wake_capable);
        assert_eq!(irq.debounce_timeout_us, 0x20);
        assert_eq!(irq.pin_config, 1);
    }

    #[test]
    fn gpio_int_rejects_invalid_offsets_and_polarity() {
        let mut payload = vec![1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 23, 0, 0, 25, 0, 0, 0, 0, 0];
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&[b'X', 0]);
        let mut descriptor = vec![0x8c];
        descriptor.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        descriptor.extend_from_slice(&payload);
        descriptor.extend_from_slice(&[0x79, 0]);
        assert!(parse_gpio_interrupts(&descriptor).is_err());
        for end in 0..descriptor.len() {
            assert!(parse_gpio_interrupts(&descriptor[..end]).is_err());
        }
    }

    #[test]
    fn decodes_fixed_memory_and_dword_address_windows() {
        let fixed = [0x86, 9, 0, 0, 0, 0, 0x34, 0x12, 0, 0x10, 0, 0, 0x79, 0];
        let ranges = parse_memory_ranges(&fixed).unwrap();
        assert_eq!(
            ranges,
            [MemoryRange {
                base: 0x1234_0000,
                length: 0x1000
            }]
        );

        let mut dword = [0u8; 28];
        dword[..3].copy_from_slice(&[0x87, 23, 0]);
        dword[3] = 0; // Memory space
        dword[10..14].copy_from_slice(&0xfedc_0000u32.to_le_bytes());
        dword[14..18].copy_from_slice(&0xfedc_0000u32.to_le_bytes());
        dword[22..26].copy_from_slice(&0x1000u32.to_le_bytes());
        dword[26..].copy_from_slice(&[0x79, 0]);
        assert_eq!(
            parse_memory_ranges(&dword),
            Ok(vec![MemoryRange {
                base: 0xfedc_0000,
                length: 0x1000
            }])
        );
    }
}

/// An ACPI I2cSerialBus resource after validating the raw AML resource item.
/// The address is in the mode indicated by `ten_bit`; `resource_source` is the
/// optional NUL-terminated ACPI path bytes without the terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cSerialBus {
    pub slave_address: u16,
    pub ten_bit: bool,
    pub connection_speed_hz: u32,
    pub source_index: u8,
    pub resource_source: Vec<u8>,
}

/// Parse the I2cSerialBus large resource descriptor from a raw `_CRS` buffer.
/// ACPI's serial descriptor places its six type-data bytes before the optional
/// resource-source string; TypeDataLength includes those six bytes and any
/// vendor bytes, while the resource-source string occupies the remaining
/// bytes. Other descriptor classes are validated/skipped.
pub fn parse_i2c_serial_buses(bytes: &[u8]) -> Result<Vec<I2cSerialBus>, Status> {
    let mut buses = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let tag = bytes[at];
        at += 1;
        let (kind, length) = if tag & 0x80 != 0 {
            let size = bytes.get(at..at + 2).ok_or(BAD_PARAMETER)?;
            at += 2;
            (
                tag,
                usize::from(u16::from_le_bytes(size.try_into().unwrap())),
            )
        } else {
            (tag >> 3, usize::from(tag & 7))
        };
        let end = at.checked_add(length).ok_or(BAD_PARAMETER)?;
        let item = bytes.get(at..end).ok_or(BAD_PARAMETER)?;
        at = end;
        if kind == 0x0f {
            if length != 1 || at != bytes.len() {
                return Err(BAD_PARAMETER);
            }
            if item[0] != 0 && bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
                return Err(BAD_PARAMETER);
            }
            return Ok(buses);
        }
        if kind != 0x8e {
            continue;
        }
        // Large-item length omits the 3-byte tag/length header.
        if length < 15 {
            return Err(BAD_PARAMETER);
        }
        let serial_type = item[2];
        if serial_type != 1 {
            continue;
        }
        let type_data_length = usize::from(u16::from_le_bytes([item[7], item[8]]));
        if type_data_length < 6
            || 9usize
                .checked_add(type_data_length)
                .is_none_or(|v| v > length)
        {
            return Err(BAD_PARAMETER);
        }
        let resource_source_length = length - 9 - type_data_length;
        let resource_source = if resource_source_length == 0 {
            Vec::new()
        } else {
            let source_start = 9usize.checked_add(type_data_length).ok_or(BAD_PARAMETER)?;
            let source_end = source_start
                .checked_add(resource_source_length)
                .ok_or(BAD_PARAMETER)?;
            let source = item.get(source_start..source_end).ok_or(BAD_PARAMETER)?;
            if source.last() != Some(&0) || source[..source.len() - 1].contains(&0) {
                return Err(BAD_PARAMETER);
            }
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(source.len() - 1)
                .map_err(|_| NO_MEMORY)?;
            owned.extend_from_slice(&source[..source.len() - 1]);
            owned
        };
        let type_flags = u16::from_le_bytes([item[4], item[5]]);
        let access_mode = type_flags & 1 != 0;
        let address = u16::from_le_bytes([item[13], item[14]]);
        if (!access_mode && address > 0x7f) || (access_mode && address > 0x3ff) {
            return Err(BAD_PARAMETER);
        }
        let connection_speed_hz = u32::from_le_bytes(item[9..13].try_into().unwrap());
        if connection_speed_hz == 0 {
            return Err(BAD_PARAMETER);
        }
        let mut path = Vec::new();
        path.try_reserve_exact(resource_source.len())
            .map_err(|_| NO_MEMORY)?;
        path.extend_from_slice(&resource_source);
        push(
            &mut buses,
            I2cSerialBus {
                slave_address: address,
                ten_bit: access_mode,
                connection_speed_hz,
                source_index: item[1],
                resource_source: path,
            },
        )?;
    }
    Err(BAD_PARAMETER)
}

#[cfg(test)]
mod i2c_serial_bus_tests {
    use alloc::vec;

    use super::*;

    fn i2c_resource(address: u16, access_mode: u8, source: &[u8]) -> Vec<u8> {
        let mut item = vec![0u8; 15 + source.len() + 1];
        item[0] = 1; // ACPI resource revision
        item[1] = 2; // ResourceSourceIndex
        item[2] = 1; // I2C serial bus type
        item[4] = access_mode;
        item[6] = 1; // I2C type revision
        item[7..9].copy_from_slice(&6u16.to_le_bytes());
        item[9..13].copy_from_slice(&400_000u32.to_le_bytes());
        item[13..15].copy_from_slice(&address.to_le_bytes());
        item[15..15 + source.len()].copy_from_slice(source);
        // AML source strings are NUL terminated.
        let mut resource = vec![0x8e, item.len() as u8, 0];
        resource.extend_from_slice(&item);
        resource.extend_from_slice(&[0x79, 0]);
        resource
    }

    #[test]
    fn parses_i2c_serial_bus_address_speed_and_controller_path() {
        let buses = parse_i2c_serial_buses(&i2c_resource(0x2c, 0, b"\\_SB.I2C0")).unwrap();
        assert_eq!(buses.len(), 1);
        assert_eq!(buses[0].slave_address, 0x2c);
        assert!(!buses[0].ten_bit);
        assert_eq!(buses[0].connection_speed_hz, 400_000);
        assert_eq!(buses[0].source_index, 2);
        assert_eq!(buses[0].resource_source, b"\\_SB.I2C0");
        let buses = parse_i2c_serial_buses(&i2c_resource(0x234, 1, b"\\_SB.I2C1")).unwrap();
        assert!(buses[0].ten_bit);
        assert_eq!(buses[0].slave_address, 0x234);
    }

    #[test]
    fn rejects_bad_i2c_descriptor_lengths_and_addresses() {
        let mut resource = i2c_resource(0x2c, 0, b"\\_SB.I2C0");
        resource[3 + 7..3 + 9].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(parse_i2c_serial_buses(&resource), Err(BAD_PARAMETER));
        let invalid = i2c_resource(0x80, 0, b"\\_SB.I2C0");
        assert_eq!(parse_i2c_serial_buses(&invalid), Err(BAD_PARAMETER));
    }
}
