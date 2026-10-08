//! USB HCI transport following FreeBSD `sys/netgraph/bluetooth/drivers/ubt/`
//! `ng_ubt.c` (BSD-2-Clause, rev 1.16), with netgraph node management omitted.
//! Copyright (c) 2001-2009 Maksim Yevmenkin <m_evmenkin@yahoo.com>

use alloc::{sync::Arc, vec::Vec};

use axdriver_base::{DevError, DevResult};
use crab_usb::{
    device::{Device, InterfaceSession},
    usb_if::{
        descriptor::{EndpointType, InterfaceDescriptor},
        host::ControlSetup,
        transfer::{Direction, Recipient, Request, RequestType},
    },
};
use spin::Mutex;
use tk_bt_hci::{Adapter, Error, UsbTransport};

use super::*;

pub struct UsbBluetoothHci {
    adapter: Adapter<Transport>,
}

struct Transport {
    host: Arc<Host>,
    device: Arc<Mutex<Device>>,
    _session: InterfaceSession,
    interface: u8,
    event: crab_usb::EndpointHandle,
    acl_in: crab_usb::EndpointHandle,
    acl_out: crab_usb::EndpointHandle,
}

fn endpoint(
    session: &InterfaceSession,
    interface: &InterfaceDescriptor,
    ty: EndpointType,
    dir: Direction,
) -> DevResult<crab_usb::EndpointHandle> {
    let desc = interface
        .endpoints
        .iter()
        .find(|e| e.transfer_type == ty && e.direction == dir)
        .ok_or(DevError::Unsupported)?;
    if desc.max_packet_size == 0 {
        return Err(DevError::Unsupported);
    }
    session.endpoint(desc.address).map_err(|_| DevError::Io)
}

impl UsbBluetoothHci {
    pub(super) fn new(
        host: Arc<Host>,
        device: Arc<Mutex<Device>>,
        session: InterfaceSession,
        interface: &InterfaceDescriptor,
        index: u16,
    ) -> DevResult<Self> {
        if interface.class != 0xe0
            || interface.subclass != 1
            || interface.protocol != 1
            || interface.alternate_setting != 0
        {
            return Err(DevError::Unsupported);
        }
        let event = endpoint(&session, interface, EndpointType::Interrupt, Direction::In)?;
        let acl_in = endpoint(&session, interface, EndpointType::Bulk, Direction::In)?;
        let acl_out = endpoint(&session, interface, EndpointType::Bulk, Direction::Out)?;
        Ok(Self {
            adapter: Adapter::new(
                Transport {
                    host,
                    device,
                    _session: session,
                    interface: interface.interface_number,
                    event,
                    acl_in,
                    acl_out,
                },
                index,
            ),
        })
    }
    pub fn index(&self) -> u16 {
        self.adapter.index()
    }
    pub fn set_up(&mut self, up: bool) -> Result<(), Error> {
        self.adapter.set_up(up)
    }
    pub fn submit(
        &mut self,
        channel: tk_bt_hci::Channel,
        kind: tk_bt_hci::PacketType,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.adapter.submit(channel, kind, bytes)
    }
    pub fn read_event(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.adapter.read_event(out)
    }
}

impl UsbTransport for Transport {
    // upstream: ng_ubt.c ubt_task_schedule()
    fn control_command(&mut self, command: &[u8]) -> Result<(), Error> {
        let setup = ControlSetup {
            request_type: RequestType::Class,
            recipient: Recipient::Device,
            request: Request::Other(0),
            value: 0,
            index: u16::from(self.interface),
        };
        let written = self
            .host
            .wait(self.device.lock().control_out(setup, command))
            .map_err(|_| Error::NoDevice)?
            .map_err(|_| Error::NoDevice)?;
        if written != command.len() {
            return Err(Error::InvalidLength);
        }
        Ok(())
    }
    // upstream: ng_ubt.c ubt_bulk_write_callback()
    fn bulk_acl_out(&mut self, packet: &[u8]) -> Result<(), Error> {
        self.host
            .transfer(&self.acl_out, TransferRequest::bulk_out(packet))
            .map_err(|_| Error::NoDevice)?;
        Ok(())
    }
    // upstream: ng_ubt.c ubt_intr_read_callback()
    fn read_interrupt_event(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.host
            .transfer(&self.event, TransferRequest::interrupt_in(out))
            .map_err(|_| Error::NoDevice)
    }
    // upstream: ng_ubt.c ubt_bulk_read_callback()
    fn read_bulk_acl(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.host
            .transfer(&self.acl_in, TransferRequest::bulk_in(out))
            .map_err(|_| Error::NoDevice)
    }
    fn stop(&mut self) {}
}

static DEVICES: spin::Once<Mutex<Vec<UsbBluetoothHci>>> = spin::Once::new();
pub(super) fn register(device: UsbBluetoothHci) {
    DEVICES
        .call_once(|| Mutex::new(Vec::new()))
        .lock()
        .push(device);
}
pub fn take_devices() -> Vec<UsbBluetoothHci> {
    DEVICES
        .call_once(|| Mutex::new(Vec::new()))
        .lock()
        .drain(..)
        .collect()
}
