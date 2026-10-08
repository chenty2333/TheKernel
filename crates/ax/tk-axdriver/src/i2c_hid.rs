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

    // upstream: iichid.c iichid_cmd_get_report() bus-message transfer
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

    fn delay_ms(&mut self, milliseconds: u32) {
        axhal::time::busy_wait(core::time::Duration::from_millis(u64::from(milliseconds)));
    }
}

struct InputState {
    device: Device<I2cTransport>,
    descriptor: Descriptor,
    parser: crate::hid_report::Report,
    hmt: Option<crate::hmt::MultiTouch>,
    events: VecDeque<Event>,
    input: Vec<u8>,
    previous_input: Vec<u8>,
    next_sample_ns: u64,
    sample_rate_hz: u8,
    missing_samples: u8,
    duplicate_samples: u8,
    opened: bool,
    suspended: bool,
}

pub struct I2cInput {
    state: Mutex<InputState>,
    info: crate::hidbus::DeviceInfo,
    location: String,
}

impl I2cInput {
    // upstream: iichid.c iichid_probe() and iichid_attach()
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
        {
            return Err(DevError::Unsupported);
        }
        let mut device = Device::new(transport, descriptor);
        device.set_power(Power::On).map_err(map_hid_error)?;
        device.delay_ms(1);
        // upstream logs a warning and continues attachment on RESET timeout.
        match device.reset(tk_i2c_hid::RESET_TIMEOUT_SECONDS) {
            Ok(()) => {}
            Err(HidError::ResetTimeout) => warn!(
                "i2c-hid: device at {} did not acknowledge RESET",
                child.slave_address
            ),
            Err(error) => return Err(map_hid_error(error)),
        }
        let report_length = usize::from(descriptor.report_descriptor_length);
        let mut report_descriptor = Vec::new();
        report_descriptor
            .try_reserve_exact(report_length)
            .map_err(|_| DevError::NoMemory)?;
        report_descriptor.resize(report_length, 0);
        let actual = device
            .report_descriptor(&mut report_descriptor)
            .map_err(map_hid_error)?;
        let (mut parser, report_info) =
            crate::hidbus::attach_report_descriptor(&report_descriptor[..actual])?;
        if report_info.input.bytes > usize::from(descriptor.max_input_length).saturating_sub(2) {
            return Err(DevError::Unsupported);
        }
        let mut hmt = crate::hmt::MultiTouch::probe(&parser);
        if let Some(info) = &mut hmt {
            if let Some(location) =
                parser.locate_usage(crate::hid_report::ReportKind::Input, 0x0d, 0x54, 0)
            {
                parser.configure_mt_contact_count(location);
            }
            let location =
                parser.locate_usage(crate::hid_report::ReportKind::Feature, 0x0d, 0x55, 0);
            if let Some(location) = location {
                let mut feature = Vec::new();
                feature
                    .try_reserve_exact(report_info.feature.bytes)
                    .map_err(|_| DevError::NoMemory)?;
                feature.resize(report_info.feature.bytes, 0);
                if device
                    .get_report(3, location.report_id, &mut feature)
                    .is_ok_and(|actual| {
                        actual.saturating_mul(8)
                            >= usize::from(location.bit_offset) + usize::from(location.size)
                    })
                    && let Some(max_contacts) =
                        crate::hid_report::get_hid_data(&feature, location, false)
                    && max_contacts > 0
                {
                    info.slots = u16::try_from(max_contacts).unwrap_or(32).min(32);
                }
            }
            // upstream: hmt_attach() fetches HUD_BUTTON_TYPE unless it shares
            // the Contact Count Maximum report; value zero means integrated
            // clickpad, and a failed/absent report falls back to ordinary pad.
            if let Some(location) =
                parser.locate_usage(crate::hid_report::ReportKind::Feature, 0x0d, 0x59, 0)
                && parser
                    .locate_usage(crate::hid_report::ReportKind::Feature, 0x0d, 0x55, 0)
                    .is_none_or(|contacts| contacts.report_id != location.report_id)
            {
                let length =
                    parser.report_size(crate::hid_report::ReportKind::Feature, location.report_id);
                let mut feature = Vec::new();
                feature
                    .try_reserve_exact(length)
                    .map_err(|_| DevError::NoMemory)?;
                feature.resize(length, 0);
                if device
                    .get_report(3, location.report_id, &mut feature)
                    .is_ok_and(|actual| {
                        actual.saturating_mul(8)
                            >= usize::from(location.bit_offset) + usize::from(location.size)
                    })
                {
                    info.set_button_type(crate::hid_report::get_hid_data(
                        &feature, location, false,
                    ));
                }
            }
            if let Err(error) = info.set_input_mode(&mut device, &parser, 3) {
                warn!("i2c-hid: failed to select multitouch input mode: {error:?}");
            }
            parser.set_mt_slot_limit(info.slots);
        }
        let mut input = Vec::new();
        let input_length = usize::from(descriptor.max_input_length).saturating_sub(2);
        input
            .try_reserve_exact(input_length)
            .map_err(|_| DevError::NoMemory)?;
        input.resize(input_length, 0);
        let mut previous_input = Vec::new();
        previous_input
            .try_reserve_exact(input_length)
            .map_err(|_| DevError::NoMemory)?;
        previous_input.resize(input_length, 0);
        if let Err(error) = device.set_power(Power::Off) {
            warn!("i2c-hid: failed to power off after attach: {error:?}");
        }
        let id = InputDeviceId {
            bus_type: BUS_I2C,
            vendor: descriptor.vendor_id,
            product: descriptor.product_id,
            version: descriptor.version_id,
        };
        let info = crate::hidbus::DeviceInfo::new(
            id,
            child.hid.clone(),
            child.path.clone(),
            report_descriptor,
            report_info,
            descriptor,
        );
        Ok(Self {
            state: Mutex::new(InputState {
                device,
                descriptor,
                parser,
                hmt,
                events: VecDeque::new(),
                input,
                previous_input,
                next_sample_ns: 0,
                sample_rate_hz: tk_i2c_hid::INPUT_SAMPLING_FAST_HZ,
                missing_samples: 0,
                duplicate_samples: 0,
                opened: false,
                suspended: false,
            }),
            info,
            location: format!("i2c-{bus}/{}", child.slave_address),
        })
    }

    // upstream: hidbus.c hidbus_get_rdesc() / iichid.c iichid_ioctl()
    pub fn get_report_descriptor(&self, out: &mut [u8]) -> DevResult<usize> {
        self.info.report_descriptor(out)
    }

    // upstream: iichid.c iichid_get_report()
    pub fn get_report(
        &mut self,
        report_type: u8,
        report_id: u8,
        out: &mut [u8],
    ) -> DevResult<usize> {
        self.state
            .get_mut()
            .device
            .get_report(report_type, report_id, out)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_read() / iichid_intr_poll()
    pub fn read_input_report(&mut self, out: &mut [u8]) -> DevResult<usize> {
        self.state
            .get_mut()
            .device
            .read_input(out)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_set_report()
    pub fn set_report(&mut self, report_type: u8, report_id: u8, report: &[u8]) -> DevResult<()> {
        self.state
            .get_mut()
            .device
            .set_report(report_type, report_id, report)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_write()
    pub fn write_output(&mut self, report: &[u8]) -> DevResult<()> {
        self.state
            .get_mut()
            .device
            .write_output(report)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_set_idle()
    pub fn set_idle(&mut self, duration: u16, report_id: u8) -> DevResult<()> {
        self.state
            .get_mut()
            .device
            .set_idle(duration, report_id)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_set_protocol()
    pub fn set_protocol(&mut self, protocol: u16) -> DevResult<()> {
        self.state
            .get_mut()
            .device
            .set_protocol(protocol)
            .map_err(map_hid_error)
    }

    // upstream: iichid.c iichid_ioctl(I2CRDWR)
    pub fn ioctl_rdwr(&mut self, messages: &mut [IicMessage<'_>]) -> DevResult<()> {
        let transport = self.state.get_mut().device.transport_mut();
        crate::i2c::transfer(transport.bus, messages).map_err(|error| match error {
            IicError::NoAck | IicError::BusBusy | IicError::Timeout => DevError::Again,
            _ => DevError::Io,
        })
    }

    /// ACPI hardware ID and FreeBSD-compatible quirk bitmap from hidbus probe.
    pub fn hardware_id(&self) -> &str {
        &self.info.hardware_id
    }

    pub fn quirks(&self) -> u32 {
        self.info.quirks
    }

    // upstream: hidbus.c hidbus_get_report_info()
    pub fn report_descriptor_sizes(&self) -> [(u8, usize); 3] {
        [
            (
                self.info.report_info.input.report_id,
                self.info.report_info.input.bytes,
            ),
            (
                self.info.report_info.output.report_id,
                self.info.report_info.output.bytes,
            ),
            (
                self.info.report_info.feature.report_id,
                self.info.report_info.feature.bytes,
            ),
        ]
    }
}

fn map_hid_error(error: HidError) -> DevError {
    match error {
        HidError::PacketTooLarge => DevError::Unsupported,
        HidError::Unsupported => DevError::Unsupported,
        HidError::Bus(BusError::Nack) => DevError::Again,
        HidError::Bus(BusError::Busy | BusError::Timeout) => DevError::Again,
        HidError::Bus(_)
        | HidError::InvalidDescriptor
        | HidError::InvalidPacket
        | HidError::ResetTimeout => DevError::Io,
    }
}

impl BaseDriverOps for I2cInput {
    // upstream: hmt.c hmt_probe() / hidbus.c hidbus_probe()
    fn device_name(&self) -> &str {
        let state = self.state.lock();
        if let Some(hmt) = state.hmt {
            match hmt.kind {
                crate::hmt::Type::Touchpad => return "I2C HID multitouch touchpad",
                crate::hmt::Type::Touchscreen => return "I2C HID multitouch touchscreen",
            }
        }
        if state.parser.is_keyboard() {
            return "I2C HID keyboard";
        }
        if state.parser.is_mouse() {
            return "I2C HID mouse";
        }
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
    // upstream: hmt.c hmt_ev_open() / iichid.c iichid_set_power_state()
    fn open_input(&mut self) -> DevResult<()> {
        let state = self.state.get_mut();
        if !state.opened && !state.suspended {
            state.device.set_power(Power::On).map_err(map_hid_error)?;
            state.opened = true;
        }
        Ok(())
    }

    // upstream: hmt.c hmt_ev_close() / iichid.c iichid_set_power_state()
    fn close_input(&mut self) -> DevResult<()> {
        let state = self.state.get_mut();
        if state.opened && !state.suspended {
            state.device.set_power(Power::Off).map_err(map_hid_error)?;
        }
        state.opened = false;
        Ok(())
    }

    fn device_id(&self) -> InputDeviceId {
        self.info.id
    }
    fn physical_location(&self) -> &str {
        &self.location
    }
    fn unique_id(&self) -> &str {
        &self.info.path
    }
    // upstream: hmt.c hmt_attach()
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool> {
        Ok(self.state.get_mut().parser.event_bits(ty as u16, out))
    }
    // upstream: hmt.c hmt_attach()
    fn get_property_bits(&mut self, out: &mut [u8]) -> DevResult<bool> {
        out.fill(0);
        let state = self.state.get_mut();
        if state
            .hmt
            .is_some_and(|hmt| hmt.kind == crate::hmt::Type::Touchscreen)
        {
            if let Some(byte) = out.first_mut() {
                *byte = 1 << 1; // INPUT_PROP_DIRECT
            }
            return Ok(true);
        }
        if state.parser.is_pointer() && state.parser.absolute_range(0).is_some() {
            if let Some(byte) = out.first_mut() {
                *byte = 1;
            }
            return Ok(true);
        }
        if state
            .hmt
            .is_some_and(|hmt| hmt.kind == crate::hmt::Type::Touchpad)
        {
            if let Some(byte) = out.first_mut() {
                *byte = 1 | (u8::from(state.hmt.is_some_and(|hmt| hmt.clickpad)) << 2);
            }
            return Ok(true);
        }
        Ok(false)
    }
    // upstream: hmt.c hmt_attach()
    fn get_abs_info(&mut self, axis: u8) -> DevResult<Option<AbsInfo>> {
        let state = self.state.get_mut();
        if axis == 0x2f
            && let Some(hmt) = state.hmt
        {
            return Ok(Some(AbsInfo {
                min: 0,
                max: u32::from(hmt.slots.saturating_sub(1)),
                fuzz: 0,
                flat: 0,
                res: 0,
            }));
        }
        let range = state.parser.absolute_range(axis);
        let resolution = state.parser.absolute_resolution(axis).max(0) as u32;
        Ok(range.map(|(min, max)| AbsInfo {
            min: min as u32,
            max: max as u32,
            fuzz: 0,
            flat: 0,
            res: resolution,
        }))
    }
    // upstream: iichid.c iichid_intr() and hmt.c hmt_intr()
    fn read_event(&mut self) -> DevResult<Event> {
        let state = self.state.get_mut();
        if !state.opened || state.suspended {
            return Err(DevError::Again);
        }
        if let Some(event) = state.events.pop_front() {
            return Ok(event);
        }
        // upstream: iichid_sampling_task() - adaptive 80/10 Hz sampling when
        // this platform cannot deliver a GPIO interrupt into the HID child.
        let now = axhal::time::monotonic_time_nanos();
        if now < state.next_sample_ns {
            return Err(DevError::Again);
        }
        let length = state
            .device
            .read_input(
                &mut state.input
                    [..usize::from(state.descriptor.max_input_length).saturating_sub(2)],
            )
            .map_err(map_hid_error)?;
        if length == 0 {
            state.missing_samples = state.missing_samples.saturating_add(1);
            state.duplicate_samples = 0;
            if state.missing_samples >= tk_i2c_hid::INPUT_SAMPLING_HYSTERESIS {
                state.sample_rate_hz = tk_i2c_hid::INPUT_SAMPLING_SLOW_HZ;
            }
        } else {
            state.missing_samples = 0;
            if state.previous_input[..length] == state.input[..length] {
                state.duplicate_samples = state.duplicate_samples.saturating_add(1);
                if state.duplicate_samples >= tk_i2c_hid::INPUT_SAMPLING_HYSTERESIS {
                    state.sample_rate_hz = tk_i2c_hid::INPUT_SAMPLING_SLOW_HZ;
                }
            } else {
                state.previous_input[..length].copy_from_slice(&state.input[..length]);
                state.duplicate_samples = 0;
                state.sample_rate_hz = tk_i2c_hid::INPUT_SAMPLING_FAST_HZ;
            }
        }
        state.next_sample_ns =
            now.saturating_add(1_000_000_000u64 / u64::from(state.sample_rate_hz.max(1)));
        if length == 0 {
            state.parser.release_all_contacts(&mut state.events);
            return state.events.pop_front().ok_or(DevError::Again);
        }
        state
            .parser
            .decode(&state.input[..length], &mut state.events);
        state.events.pop_front().ok_or(DevError::Again)
    }
}

impl I2cInput {
    // upstream: iichid.c iichid_suspend()
    pub fn suspend(&mut self) -> DevResult<()> {
        let state = self.state.get_mut();
        state.suspended = true;
        if state.opened {
            if let Err(error) = state.device.set_power(Power::Off) {
                warn!("i2c-hid: suspend power transition failed: {error:?}");
            }
        }
        Ok(())
    }

    // upstream: iichid.c iichid_resume()
    pub fn resume(&mut self) -> DevResult<()> {
        let state = self.state.get_mut();
        state.suspended = false;
        if state.opened {
            let device = &mut state.device;
            if let Err(error) = device.set_power(Power::On) {
                warn!("i2c-hid: resume power transition failed: {error:?}");
            } else {
                device.delay_ms(1);
                match device.reset(tk_i2c_hid::RESET_TIMEOUT_SECONDS) {
                    Ok(()) => {}
                    Err(HidError::ResetTimeout) => {
                        warn!("i2c-hid: reset acknowledgement timeout on resume");
                    }
                    Err(error) => warn!("i2c-hid: reset failed on resume: {error:?}"),
                }
            }
        }
        Ok(())
    }
}

impl Drop for I2cInput {
    // upstream: iichid.c iichid_detach()
    fn drop(&mut self) {
        let state = self.state.get_mut();
        if state.opened && !state.suspended {
            let _ = state.device.set_power(Power::Off);
        }
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
