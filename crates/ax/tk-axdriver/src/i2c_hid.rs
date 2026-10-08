//! FreeBSD iichid child attachment mapped onto the existing HID decoder and
//! evdev input-driver interface.
use alloc::{collections::VecDeque, format, string::String, vec, vec::Vec};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_input::{AbsInfo, Event, EventType, InputDeviceId, InputDriverOps};
use spin::Mutex;
use tk_i2c::{
    Address, Error as BusError, IIC_M_NOSTART, IIC_M_NOSTOP, IIC_M_RD, IicError, IicMessage,
};
use tk_i2c_hid::{
    AcpiHidDescriptorAddress, Descriptor, Device, Error as HidError, Power, Transport,
};

const BUS_I2C: u16 = 0x18;
const MAX_HID_REPORT: usize = 4096;
const MAX_INPUT_REPORT: usize = 1024;

struct I2cTransport {
    bus: usize,
    address: Address,
}

fn bus_error(error: IicError) -> BusError {
    match error {
        IicError::Timeout => BusError::Timeout,
        IicError::NoAck => BusError::Nack,
        IicError::BusBusy => BusError::Busy,
        _ => BusError::InvalidConfiguration,
    }
}

impl Transport for I2cTransport {
    fn address(&self) -> Address {
        self.address
    }

    fn write_register(&mut self, register: u16, bytes: &[u8]) -> Result<(), BusError> {
        let length = bytes
            .len()
            .checked_add(2)
            .ok_or(BusError::InvalidConfiguration)?;
        let mut packet = vec![0; length];
        packet[..2].copy_from_slice(&register.to_le_bytes());
        packet[2..].copy_from_slice(bytes);
        let mut messages = [IicMessage {
            slave: self.address.value,
            flags: 0,
            buf: &mut packet,
        }];
        crate::i2c::transfer(self.bus, &mut messages).map_err(bus_error)
    }

    fn read_register(&mut self, register: u16, bytes: &mut [u8]) -> Result<usize, BusError> {
        let mut address = register.to_le_bytes();
        let mut messages = [
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_NOSTOP,
                buf: &mut address,
            },
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_RD,
                buf: bytes,
            },
        ];
        crate::i2c::transfer(self.bus, &mut messages).map_err(bus_error)?;
        Ok(bytes.len())
    }

    fn read_input(&mut self, bytes: &mut [u8]) -> Result<usize, BusError> {
        let mut messages = [IicMessage {
            slave: self.address.value,
            flags: IIC_M_RD,
            buf: bytes,
        }];
        crate::i2c::transfer(self.bus, &mut messages).map_err(bus_error)?;
        Ok(bytes.len())
    }

    fn read_report(&mut self, command: &[u8], bytes: &mut [u8]) -> Result<usize, BusError> {
        if bytes.len() < 2 {
            return Err(BusError::InvalidConfiguration);
        }
        let total = bytes.len();
        let (length, payload) = bytes.split_at_mut(2);
        let mut command = command.to_vec();
        let mut messages = [
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_NOSTOP,
                buf: &mut command,
            },
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_RD | IIC_M_NOSTOP,
                buf: length,
            },
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_RD | IIC_M_NOSTART,
                buf: payload,
            },
        ];
        crate::i2c::transfer(self.bus, &mut messages).map_err(bus_error)?;
        Ok(total)
    }

    fn write_report(&mut self, command: &[u8], bytes: &[u8]) -> Result<(), BusError> {
        let mut command = command.to_vec();
        let mut payload = bytes.to_vec();
        let mut messages = [
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_NOSTOP,
                buf: &mut command,
            },
            IicMessage {
                slave: self.address.value,
                flags: IIC_M_NOSTART,
                buf: &mut payload,
            },
        ];
        crate::i2c::transfer(self.bus, &mut messages).map_err(bus_error)
    }
}

struct InputState {
    device: Device<I2cTransport>,
    descriptor: Descriptor,
    parser: crate::hid_report::Report,
    events: VecDeque<Event>,
    input: [u8; MAX_INPUT_REPORT],
}

pub struct I2cInput {
    state: Mutex<InputState>,
    id: InputDeviceId,
    location: String,
    path: String,
}

impl I2cInput {
    fn attach(bus: usize, child: crate::i2c::AcpiI2cChild) -> DevResult<Self> {
        let address = if child.ten_bit {
            Address::ten_bit(child.slave_address)
        } else {
            u8::try_from(child.slave_address)
                .ok()
                .and_then(Address::seven_bit)
        }
        .ok_or(DevError::InvalidParam)?;
        let mut transport = I2cTransport { bus, address };
        let descriptor_register = child.hid_descriptor_register.ok_or(DevError::Unsupported)?;
        let descriptor = Device::<I2cTransport>::read_descriptor(
            &mut transport,
            AcpiHidDescriptorAddress(descriptor_register),
        )
        .map_err(map_hid_error)?;
        if descriptor.version != 0x0100
            || descriptor.descriptor_length != tk_i2c_hid::HID_DESCRIPTOR_BYTES as u16
            || usize::from(descriptor.max_input_length) > MAX_INPUT_REPORT
        {
            return Err(DevError::Unsupported);
        }
        let mut device = Device::new(transport, descriptor);
        device.reset(256).map_err(map_hid_error)?;
        device.set_power(Power::On).map_err(map_hid_error)?;
        let report_length = usize::from(descriptor.report_descriptor_length);
        if report_length > MAX_HID_REPORT {
            return Err(DevError::Unsupported);
        }
        let mut report_descriptor = vec![0; report_length];
        let actual = device
            .report_descriptor(&mut report_descriptor)
            .map_err(map_hid_error)?;
        let parser = crate::hid_report::Report::parse(&report_descriptor[..actual])?;
        if parser.max_length() > usize::from(descriptor.max_input_length).saturating_sub(2) {
            return Err(DevError::Unsupported);
        }
        let id = InputDeviceId {
            bus_type: BUS_I2C,
            vendor: descriptor.vendor_id,
            product: descriptor.product_id,
            version: descriptor.version_id,
        };
        let path = child.path.clone();
        Ok(Self {
            state: Mutex::new(InputState {
                device,
                descriptor,
                parser,
                events: VecDeque::new(),
                input: [0; MAX_INPUT_REPORT],
            }),
            id,
            location: format!("i2c-{bus}/{}", child.slave_address),
            path,
        })
    }
}

fn map_hid_error(error: HidError) -> DevError {
    match error {
        HidError::PacketTooLarge => DevError::Unsupported,
        HidError::Unsupported => DevError::Unsupported,
        HidError::Bus(BusError::Nack) => DevError::Again,
        HidError::Bus(BusError::Busy | BusError::Timeout) => DevError::Again,
        HidError::Bus(_) | HidError::InvalidDescriptor | HidError::InvalidPacket => DevError::Io,
    }
}

impl BaseDriverOps for I2cInput {
    fn device_name(&self) -> &str {
        let state = self.state.lock();
        if state.parser.is_pointer() {
            "I2C HID pointer"
        } else {
            "I2C HID input"
        }
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Input
    }
}

impl InputDriverOps for I2cInput {
    fn device_id(&self) -> InputDeviceId {
        self.id
    }
    fn physical_location(&self) -> &str {
        &self.location
    }
    fn unique_id(&self) -> &str {
        &self.path
    }
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool> {
        Ok(self.state.get_mut().parser.event_bits(ty as u16, out))
    }
    fn get_property_bits(&mut self, out: &mut [u8]) -> DevResult<bool> {
        out.fill(0);
        let state = self.state.get_mut();
        if state.parser.is_pointer() && state.parser.absolute_range(0).is_some() {
            if let Some(byte) = out.first_mut() {
                *byte = 1;
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn get_abs_info(&mut self, axis: u8) -> DevResult<Option<AbsInfo>> {
        Ok(self
            .state
            .get_mut()
            .parser
            .absolute_range(axis)
            .map(|(min, max)| AbsInfo {
                min: min as u32,
                max: max as u32,
                fuzz: 0,
                flat: 0,
                res: 0,
            }))
    }
    fn read_event(&mut self) -> DevResult<Event> {
        let state = self.state.get_mut();
        if let Some(event) = state.events.pop_front() {
            return Ok(event);
        }
        let length = state
            .device
            .read_input(&mut state.input[..usize::from(state.descriptor.max_input_length)])
            .map_err(map_hid_error)?;
        if length == 0 {
            return Err(DevError::Again);
        }
        state
            .parser
            .decode(&state.input[..length], &mut state.events);
        state.events.pop_front().ok_or(DevError::Again)
    }
}

/// Probe the ACPI child devices attached to each successfully initialized ig4
/// controller.  The kernel's normal input registration owns the returned HID
/// drivers and exposes them through evdev.
pub(crate) fn probe_devices() -> Vec<I2cInput> {
    let mut devices = Vec::new();
    for bus in 0..crate::i2c::bus_count() {
        let Some((_, children)) = crate::i2c::children(bus) else {
            continue;
        };
        for child in children
            .into_iter()
            .filter(|child| matches!(child.hid.as_str(), "PNP0C50" | "ACPI0C50"))
        {
            match I2cInput::attach(bus, child) {
                Ok(device) => devices.push(device),
                Err(error) => warn!("i2c-hid: failed to attach ACPI child: {error:?}"),
            }
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i2c_bus_errors_have_nonblocking_evdev_mapping() {
        assert!(matches!(
            map_hid_error(HidError::Bus(BusError::Timeout)),
            DevError::Again
        ));
        assert!(matches!(
            map_hid_error(HidError::InvalidDescriptor),
            DevError::Io
        ));
    }

    #[test]
    fn i2c_bus_identity_is_linux_bus_i2c() {
        assert_eq!(BUS_I2C, 0x18);
    }
}
