//! Boot-enumerated USB facts, without control transfers during sysfs access.
use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU16, Ordering};

use crab_usb::{
    device::{ObservedLocation, ProbedDevice},
    usb_if::{descriptor::DeviceDescriptor, host::hub::Speed},
};
use spin::Mutex;

use crate::{
    UsbInputIdentity,
    prelude::{DevError, DevResult},
};

static NEXT_BUS: AtomicU16 = AtomicU16::new(1);
static OBSERVATIONS: Mutex<Vec<Observation>> = Mutex::new(Vec::new());

#[derive(Clone)]
pub struct Observation {
    pub bus: u8,
    pub location: ObservedLocation,
    pub descriptor: DeviceDescriptor,
    pub descriptors: Arc<Vec<u8>>,
}

pub(super) fn allocate_bus() -> Option<u8> {
    u8::try_from(NEXT_BUS.fetch_add(1, Ordering::Relaxed))
        .ok()
        .filter(|bus| *bus != 0)
}

fn device_bytes(desc: &DeviceDescriptor) -> [u8; 18] {
    let mut bytes = [0; 18];
    bytes[0] = 18;
    bytes[1] = 1;
    bytes[2..4].copy_from_slice(&desc.usb_version.to_le_bytes());
    bytes[4] = desc.class;
    bytes[5] = desc.subclass;
    bytes[6] = desc.protocol;
    bytes[7] = desc.max_packet_size_0;
    bytes[8..10].copy_from_slice(&desc.vendor_id.to_le_bytes());
    bytes[10..12].copy_from_slice(&desc.product_id.to_le_bytes());
    bytes[12..14].copy_from_slice(&desc.device_version.to_le_bytes());
    bytes[14] = desc
        .manufacturer_string_index
        .map_or(0, |value| value.get());
    bytes[15] = desc.product_string_index.map_or(0, |value| value.get());
    bytes[16] = desc
        .serial_number_string_index
        .map_or(0, |value| value.get());
    bytes[17] = desc.num_configurations;
    bytes
}

pub(super) fn observe(bus: u8, probed: &ProbedDevice) -> DevResult<Option<Observation>> {
    let Some(location) = probed.observed_location().copied() else {
        return Ok(None);
    };
    let mut descriptors = Vec::new();
    let length = probed
        .configurations()
        .iter()
        .try_fold(18usize, |length, config| {
            length.checked_add(config.raw.len())
        })
        .ok_or(DevError::InvalidParam)?;
    descriptors
        .try_reserve_exact(length)
        .map_err(|_| DevError::NoMemory)?;
    descriptors.extend_from_slice(&device_bytes(probed.descriptor()));
    for config in probed.configurations() {
        descriptors.extend_from_slice(&config.raw);
    }
    Ok(Some(Observation {
        bus,
        location,
        descriptor: probed.descriptor().clone(),
        descriptors: Arc::try_new(descriptors).map_err(|_| DevError::NoMemory)?,
    }))
}

pub(super) fn publish(observation: Observation) {
    let mut entries = OBSERVATIONS.lock();
    if entries.try_reserve(1).is_err() {
        warn!("USB observation allocation unavailable");
        return;
    }
    entries.push(observation);
}

pub fn snapshot() -> DevResult<Vec<Observation>> {
    let entries = OBSERVATIONS.lock();
    let mut out = Vec::new();
    out.try_reserve_exact(entries.len())
        .map_err(|_| DevError::NoMemory)?;
    out.extend(entries.iter().cloned());
    Ok(out)
}

pub fn name(observation: &Observation) -> String {
    let mut name = format!("{}-", observation.bus);
    for (index, port) in observation.location.ports[..usize::from(observation.location.depth)]
        .iter()
        .enumerate()
    {
        if index != 0 {
            name.push('.');
        }
        name.push_str(&format!("{port}"));
    }
    name
}

pub fn speed(observation: &Observation) -> Option<&'static str> {
    match observation.location.speed {
        Speed::Low => Some("1.5\n"),
        Speed::Full => Some("12\n"),
        Speed::High => Some("480\n"),
        Speed::SuperSpeed => Some("5000\n"),
        Speed::SuperSpeedPlus => Some("10000\n"),
        Speed::Wireless => None,
    }
}

pub(super) fn input_identity(
    bus: u8,
    location: ObservedLocation,
    configuration: u8,
    interface: u8,
) -> Option<UsbInputIdentity> {
    let depth = usize::from(location.depth);
    if depth == 0
        || depth > location.ports.len()
        || location.ports[..depth].contains(&0)
        || configuration == 0
    {
        return None;
    }
    Some(UsbInputIdentity {
        bus,
        ports: location.ports,
        depth: location.depth,
        configuration,
        interface,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_standard_fields_encode_bus_endian_without_a_synthetic_identity() {
        let raw = [
            18, 1, 0x10, 2, 0, 0, 0, 64, 0x27, 6, 1, 0, 0x34, 0x12, 1, 2, 3, 1,
        ];
        let desc = DeviceDescriptor::parse(&raw).unwrap();
        assert_eq!(device_bytes(&desc), raw);
        let observation = Observation {
            bus: 3,
            location: ObservedLocation {
                address: 7,
                ports: [4, 2, 0, 0, 0, 0],
                depth: 2,
                speed: Speed::High,
                configuration: Some(1),
            },
            descriptor: desc,
            descriptors: Arc::new(Vec::from(raw)),
        };
        assert_eq!(name(&observation), "3-4.2");
        assert_eq!(speed(&observation), Some("480\n"));
        assert_eq!(observation.location.address, 7);
    }

    #[test]
    fn input_identity_retains_bus_port_chain_configuration_and_interface() {
        let location = ObservedLocation {
            address: 7,
            ports: [4, 2, 0, 0, 0, 0],
            depth: 2,
            speed: Speed::High,
            configuration: Some(1),
        };
        assert_eq!(
            input_identity(3, location, 1, 2),
            Some(UsbInputIdentity {
                bus: 3,
                ports: [4, 2, 0, 0, 0, 0],
                depth: 2,
                configuration: 1,
                interface: 2,
            })
        );
        assert_eq!(
            input_identity(
                3,
                ObservedLocation {
                    depth: 7,
                    ..location
                },
                1,
                2,
            ),
            None
        );
    }
}
