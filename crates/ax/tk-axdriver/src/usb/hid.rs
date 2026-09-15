//! USB HID boot keyboard/mouse reports translated into Linux input events.
use alloc::collections::VecDeque;

use axdriver_base::{BaseDriverOps, DeviceType};
use axdriver_input::{Event, EventType, InputDeviceId, InputDriverOps};
use crab_usb::usb_if::{descriptor::EndpointType, endpoint::RequestId, transfer::Direction};

use super::*;

pub struct UsbInput {
    state: Mutex<InputState>,
    id: InputDeviceId,
    keyboard: bool,
}
struct InputState {
    host: Arc<Host>,
    _owner: DeviceOwner,
    endpoint: crab_usb::EndpointHandle,
    report: Box<[u8; 8]>,
    pending: Option<RequestId>,
    previous: [u8; 8],
    events: VecDeque<Event>,
}

fn key(usage: u8) -> u16 {
    const KEYS: [u16; 100] = [
        0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20,
        22, 47, 17, 45, 21, 44, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, 27,
        43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, 99, 70,
        119, 110, 102, 104, 111, 107, 109, 106, 105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81,
        75, 76, 77, 71, 72, 73, 82, 83,
    ];
    match usage {
        100 => 86,
        101 => 127,
        103 => 117,
        _ => KEYS.get(usage as usize).copied().unwrap_or(0),
    }
}
const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];
fn push(events: &mut VecDeque<Event>, ty: u16, code: u16, value: i32) {
    events.push_back(Event {
        event_type: ty,
        code,
        value: value as u32,
    });
}
fn decode(keyboard: bool, old: &[u8; 8], report: &[u8], events: &mut VecDeque<Event>) -> bool {
    if keyboard {
        if report.len() < 8 || report[2..8].iter().any(|key| (1..=3).contains(key)) {
            return false;
        }
        for (bit, code) in MODIFIERS.iter().enumerate() {
            if (old[0] ^ report[0]) & (1 << bit) != 0 {
                push(events, 1, *code, i32::from(report[0] & (1 << bit) != 0));
            }
        }
        for &usage in &old[2..8] {
            let code = key(usage);
            if code != 0 && !report[2..8].contains(&usage) {
                push(events, 1, code, 0);
            }
        }
        for (index, &usage) in report[2..8].iter().enumerate() {
            let code = key(usage);
            if code != 0 && !old[2..8].contains(&usage) && !report[2..2 + index].contains(&usage) {
                push(events, 1, code, 1);
            }
        }
    } else {
        if report.len() < 3 {
            return false;
        }
        for bit in 0..3 {
            if (old[0] ^ report[0]) & (1 << bit) != 0 {
                push(
                    events,
                    1,
                    0x110 + bit,
                    i32::from(report[0] & (1 << bit) != 0),
                );
            }
        }
        for axis in 0..2 {
            if report[axis + 1] != 0 {
                push(events, 2, axis as u16, report[axis + 1] as i8 as i32);
            }
        }
    }
    if !events.is_empty() {
        push(events, 0, 0, 0);
    }
    true
}

impl UsbInput {
    pub(super) fn new(
        host: Arc<Host>,
        device: Device,
        session: InterfaceSession,
        interface: &InterfaceDescriptor,
    ) -> DevResult<Self> {
        let keyboard = interface.protocol == 1;
        let descriptor = interface
            .endpoints
            .iter()
            .find(|ep| ep.transfer_type == EndpointType::Interrupt && ep.direction == Direction::In)
            .ok_or(DevError::Unsupported)?;
        if descriptor.max_packet_size < if keyboard { 8 } else { 3 } {
            return Err(DevError::Unsupported);
        }
        let endpoint = session
            .endpoint(descriptor.address)
            .map_err(|_| DevError::Io)?;
        let id = InputDeviceId {
            bus_type: 3,
            vendor: device.vendor_id(),
            product: device.product_id(),
            version: device.descriptor().device_version,
        };
        let mut report = Box::new([0; 8]);
        let pending = Some(host.submit(
            &endpoint,
            TransferRequest::interrupt_in(&mut report[..if keyboard { 8 } else { 3 }]),
        )?);
        Ok(Self {
            keyboard,
            id,
            state: Mutex::new(InputState {
                host,
                _owner: DeviceOwner {
                    _device: device,
                    _session: session,
                },
                endpoint,
                report,
                pending,
                previous: [0; 8],
                events: VecDeque::new(),
            }),
        })
    }
}
impl Drop for InputState {
    fn drop(&mut self) {
        // Boot devices normally live forever. If ownership is dropped, halt
        // the controller before freeing the report buffer still owned by DMA.
        if self.pending.is_some() {
            self.host.halt();
        }
    }
}
impl BaseDriverOps for UsbInput {
    fn device_name(&self) -> &str {
        if self.keyboard {
            "USB HID keyboard"
        } else {
            "USB HID mouse"
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
        "usb/xhci"
    }
    fn unique_id(&self) -> &str {
        ""
    }
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DevResult<bool> {
        out.fill(0);
        let mut set = |code: u16| {
            if let Some(byte) = out.get_mut(code as usize / 8) {
                *byte |= 1 << (code % 8);
            }
        };
        match ty {
            EventType::Synchronization => set(0),
            EventType::Key if self.keyboard => {
                for usage in 4..=103 {
                    let code = key(usage);
                    if code != 0 {
                        set(code);
                    }
                }
                for code in MODIFIERS {
                    set(code);
                }
            }
            EventType::Key => {
                for code in 0x110..=0x112 {
                    set(code);
                }
            }
            EventType::Relative if !self.keyboard => {
                set(0);
                set(1);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn read_event(&mut self) -> DevResult<Event> {
        let state = self.state.get_mut();
        if let Some(event) = state.events.pop_front() {
            return Ok(event);
        }
        state.host.pump()?;
        if let Some(id) = state.pending {
            if let Some(completion) = state.host.reclaim(&state.endpoint, id)? {
                state.pending = None;
                if completion.status != TransferStatus::Completed {
                    return Err(DevError::Io);
                }
                let length = completion.actual_length.min(state.report.len());
                if decode(
                    self.keyboard,
                    &state.previous,
                    &state.report[..length],
                    &mut state.events,
                ) {
                    state.previous.fill(0);
                    state.previous[..length].copy_from_slice(&state.report[..length]);
                }
                state.pending = Some(
                    state
                        .host
                        .submit(
                            &state.endpoint,
                            TransferRequest::interrupt_in(
                                &mut state.report[..if self.keyboard { 8 } else { 3 }],
                            ),
                        )
                        .map_err(|_| DevError::Io)?,
                );
            }
        }
        state.events.pop_front().ok_or(DevError::Again)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_transitions_and_rollover() {
        let mut events = VecDeque::new();
        assert!(decode(
            true,
            &[0; 8],
            &[2, 0, 4, 0, 0, 0, 0, 0],
            &mut events
        ));
        assert_eq!(
            events.iter().map(|e| (e.code, e.value)).collect::<Vec<_>>(),
            [(42, 1), (30, 1), (0, 0)]
        );
        events.clear();
        assert!(!decode(
            true,
            &[2, 0, 4, 0, 0, 0, 0, 0],
            &[0, 0, 1, 1, 1, 1, 1, 1],
            &mut events
        ));
        assert!(events.is_empty());
    }
    #[test]
    fn mouse_signed_motion_and_buttons() {
        let mut events = VecDeque::new();
        decode(false, &[0; 8], &[1, 255, 4], &mut events);
        assert_eq!(
            events
                .iter()
                .map(|e| (e.event_type, e.code, e.value as i32))
                .collect::<Vec<_>>(),
            [(1, 0x110, 1), (2, 0, -1), (2, 1, 4), (0, 0, 0)]
        );
    }
}
