//! USB HID report protocol using the bounded shared descriptor decoder.
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering};

use axdriver_base::{BaseDriverOps, DeviceType};
use axdriver_input::{Event, EventType, InputDeviceId, InputDriverOps};
use crab_usb::usb_if::{descriptor::EndpointType, endpoint::RequestId, transfer::Direction};

use super::*;

pub struct UsbInput {
    state: Mutex<InputState>,
    id: InputDeviceId,
}
struct InputState {
    host: Arc<Host>,
    _owner: DeviceOwner,
    endpoint: crab_usb::EndpointHandle,
    report: Box<[u8; 64]>,
    parser: crate::hid_report::Report,
    report_len: usize,
    pending: Option<RequestId>,
    events: VecDeque<Event>,
}

impl UsbInput {
    pub(super) fn new(
        host: Arc<Host>,
        device: Arc<Mutex<Device>>,
        session: InterfaceSession,
        interface: &InterfaceDescriptor,
        dma_quiesced: Arc<AtomicBool>,
    ) -> DevResult<Self> {
        let descriptor = interface
            .endpoints
            .iter()
            .find(|ep| ep.transfer_type == EndpointType::Interrupt && ep.direction == Direction::In)
            .ok_or(DevError::Unsupported)?;
        if descriptor.max_packet_size == 0 {
            return Err(DevError::Unsupported);
        }
        let endpoint = session
            .endpoint(descriptor.address)
            .map_err(|_| DevError::Io)?;
        let mut device_guard = device.lock();
        let mut descriptor_bytes = [0u8; 4096];
        let length = host
            .wait(device_guard.control_in(
                ControlSetup {
                    request_type: RequestType::Standard,
                    recipient: Recipient::Interface,
                    request: Request::Other(6),
                    value: 0x2200,
                    index: u16::from(interface.interface_number),
                },
                &mut descriptor_bytes,
            ))?
            .map_err(|_| DevError::Io)?;
        let parser = crate::hid_report::Report::parse(
            &descriptor_bytes[..length.min(descriptor_bytes.len())],
        )?;
        let id = InputDeviceId {
            bus_type: 3,
            vendor: device_guard.vendor_id(),
            product: device_guard.product_id(),
            version: device_guard.descriptor().device_version,
        };
        drop(device_guard);
        let report_len = usize::from(descriptor.max_packet_size).min(64);
        if report_len < parser.max_length() {
            return Err(DevError::Unsupported);
        }
        let mut report = Box::new([0; 64]);
        let pending = Some(host.submit(
            &endpoint,
            TransferRequest::interrupt_in(&mut report[..report_len]),
        )?);
        Ok(Self {
            id,
            state: Mutex::new(InputState {
                host,
                _owner: DeviceOwner {
                    _device: device,
                    _session: session,
                    dma_quiesced,
                },
                endpoint,
                report,
                parser,
                report_len,
                pending,
                events: VecDeque::new(),
            }),
        })
    }
}
impl Drop for InputState {
    fn drop(&mut self) {
        // Boot devices normally live forever. If ownership is dropped, halt
        // the controller before freeing the report buffer still owned by DMA.
        if self.pending.is_some() && !self._owner.dma_quiesced.load(Ordering::Acquire) {
            self.host.halt();
        }
    }
}
impl BaseDriverOps for UsbInput {
    fn device_name(&self) -> &str {
        let parser = &self.state.lock().parser;
        if parser.has_code(1, 30) {
            "USB HID keyboard"
        } else if parser.is_pointer() {
            "USB HID pointer"
        } else {
            "USB HID input"
        }
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Input
    }
}
impl InputDriverOps for UsbInput {
    fn device_id(&self) -> InputDeviceId {
        self.id
    }
    fn physical_location(&self) -> &str {
        // The input subsystem derives the stable physical path from the
        // retained USB bus/port/interface identity rather than a generic tag.
        ""
    }
    fn unique_id(&self) -> &str {
        ""
    }
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool> {
        Ok(self.state.get_mut().parser.event_bits(ty as u16, out))
    }
    fn get_property_bits(&mut self, out: &mut [u8]) -> DevResult<bool> {
        out.fill(0);
        if self.state.get_mut().parser.is_pointer()
            && self.state.get_mut().parser.absolute_range(0).is_some()
        {
            if let Some(byte) = out.first_mut() {
                *byte = 1;
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn get_abs_info(&mut self, axis: u8) -> DevResult<Option<axdriver_input::AbsInfo>> {
        Ok(self
            .state
            .get_mut()
            .parser
            .absolute_range(axis)
            .map(|(min, max)| axdriver_input::AbsInfo {
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
        state.host.pump()?;
        if let Some(id) = state.pending
            && let Some(completion) = state.host.reclaim(&state.endpoint, id)?
        {
            state.pending = None;
            if completion.status != TransferStatus::Completed {
                return Err(DevError::Io);
            }
            let length = completion.actual_length.min(state.report.len());
            state
                .parser
                .decode(&state.report[..length], &mut state.events);
            state.pending = Some(
                state
                    .host
                    .submit(
                        &state.endpoint,
                        TransferRequest::interrupt_in(&mut state.report[..state.report_len]),
                    )
                    .map_err(|_| DevError::Io)?,
            );
        }
        state.events.pop_front().ok_or(DevError::Again)
    }
}
