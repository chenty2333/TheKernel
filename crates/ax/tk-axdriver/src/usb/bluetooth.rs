//! USB HCI transport following FreeBSD `sys/netgraph/bluetooth/drivers/ubt/`
//! `ng_ubt.c` (BSD-2-Clause, rev 1.16), with netgraph node management omitted.
//! Copyright (c) 2001-2009 Maksim Yevmenkin <m_evmenkin@yahoo.com>

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axpoll::PollSet;
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

const MAX_INTEL_FIRMWARE: usize = 16 * 1024 * 1024;
const MGMT_SETTING_POWERED: u32 = 1 << 0;
const MGMT_SETTING_CONNECTABLE: u32 = 1 << 1;
const MGMT_SETTING_DISCOVERABLE: u32 = 1 << 3;
const MGMT_SETTING_BONDABLE: u32 = 1 << 4;
const MGMT_SETTING_SSP: u32 = 1 << 6;
const MGMT_SETTING_LE: u32 = 1 << 9;
const MGMT_SETTING_BREDR: u32 = 1 << 7;
const MGMT_SETTING_PRIVACY: u32 = 1 << 13;
static FIRMWARE_CALLBACK_REGISTERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub struct UsbBluetoothHci {
    adapter: Adapter<Transport>,
    family_hint: tk_bt_hci::DeviceFamily,
    address: [u8; 6],
    capabilities: tk_bt_hci::HciCapabilities,
    management_settings: u32,
    discovery_type: u8,
    link_keys: Vec<[u8; 25]>,
    long_term_keys: Vec<[u8; 36]>,
    irks: Vec<[u8; 23]>,
    connections: Vec<([u8; 6], u8, u16)>,
    recently_disconnected: VecDeque<(u16, [u8; 6], u8)>,
    pending_pairing: Vec<([u8; 6], u8, u8)>,
    io_capability: u8,
    privacy_enabled: bool,
    local_irk: [u8; 16],
    management_events: VecDeque<Vec<u8>>,
    observed_to_raw: VecDeque<Vec<u8>>,
    receive_readiness: Arc<PollSet<32>>,
}

struct Transport {
    host: Arc<Host>,
    device: Arc<Mutex<Device>>,
    _session: InterfaceSession,
    interface: u8,
    event: crab_usb::EndpointHandle,
    acl_in: crab_usb::EndpointHandle,
    acl_out: crab_usb::EndpointHandle,
    // The buffers stay at stable addresses while their xHCI requests are
    // outstanding; only a reclaimed completion permits CPU access.
    event_buffer: [u8; 260],
    acl_buffer: [u8; 1028],
    event_request: Option<RequestId>,
    acl_request: Option<RequestId>,
    event_ready: Option<usize>,
    acl_ready: Option<usize>,
}

impl Transport {
    fn submit_event(&mut self) -> Result<(), Error> {
        if self.event_request.is_none() && self.event_ready.is_none() {
            let request = TransferRequest::interrupt_in(&mut self.event_buffer);
            self.event_request = Some(
                self.host
                    .submit(&self.event, request)
                    .map_err(|_| Error::NoDevice)?,
            );
        }
        Ok(())
    }

    fn submit_acl(&mut self) -> Result<(), Error> {
        if self.acl_request.is_none() && self.acl_ready.is_none() {
            let request = TransferRequest::bulk_in(&mut self.acl_buffer);
            self.acl_request = Some(
                self.host
                    .submit(&self.acl_in, request)
                    .map_err(|_| Error::NoDevice)?,
            );
        }
        Ok(())
    }

    fn reclaim_event(&mut self) -> Result<(), Error> {
        let Some(id) = self.event_request else {
            return Ok(());
        };
        if let Some(completion) = self
            .host
            .reclaim(&self.event, id)
            .map_err(|_| Error::NoDevice)?
        {
            self.event_request = None;
            if completion.status != TransferStatus::Completed {
                return Err(Error::NoDevice);
            }
            self.event_ready = Some(completion.actual_length);
        }
        Ok(())
    }

    fn reclaim_acl(&mut self) -> Result<(), Error> {
        let Some(id) = self.acl_request else {
            return Ok(());
        };
        if let Some(completion) = self
            .host
            .reclaim(&self.acl_in, id)
            .map_err(|_| Error::NoDevice)?
        {
            self.acl_request = None;
            if completion.status != TransferStatus::Completed {
                return Err(Error::NoDevice);
            }
            self.acl_ready = Some(completion.actual_length);
        }
        Ok(())
    }

    fn cancel_request(&mut self, endpoint: &crab_usb::EndpointHandle, id: RequestId) {
        if self.host.reclaim(endpoint, id).ok().flatten().is_some() {
            return;
        }
        if endpoint.cancel(id).is_err() {
            if self.host.reclaim(endpoint, id).ok().flatten().is_some() {
                return;
            }
            self.host.halt();
            return;
        }
        loop {
            if self.host.pump().is_err() {
                self.host.halt();
                return;
            }
            match self.host.reclaim(endpoint, id) {
                Ok(Some(_)) => return,
                Ok(None) => core::hint::spin_loop(),
                Err(_) => {
                    self.host.halt();
                    return;
                }
            }
        }
    }
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
        let (vendor_id, product_id) = {
            let device = device.lock();
            (device.vendor_id(), device.product_id())
        };
        let family_hint = tk_bt_hci::supported_device(vendor_id, product_id);
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
                    event_buffer: [0; 260],
                    acl_buffer: [0; 1028],
                    event_request: None,
                    acl_request: None,
                    event_ready: None,
                    acl_ready: None,
                },
                index,
            ),
            family_hint,
            address: [0; 6],
            capabilities: tk_bt_hci::HciCapabilities::default(),
            management_settings: 0,
            discovery_type: 0,
            link_keys: Vec::new(),
            long_term_keys: Vec::new(),
            irks: Vec::new(),
            connections: Vec::new(),
            recently_disconnected: VecDeque::new(),
            pending_pairing: Vec::new(),
            io_capability: 3,
            privacy_enabled: false,
            local_irk: [0; 16],
            management_events: VecDeque::new(),
            observed_to_raw: VecDeque::new(),
            receive_readiness: Arc::new(PollSet::new()),
        })
    }
    pub fn index(&self) -> u16 {
        self.adapter.index()
    }
    pub fn is_up(&self) -> bool {
        self.adapter.is_up()
    }
    pub fn address(&self) -> [u8; 6] {
        self.address
    }
    pub fn capabilities(&self) -> tk_bt_hci::HciCapabilities {
        self.capabilities
    }
    pub fn receive_readiness(&self) -> Arc<PollSet<32>> {
        self.receive_readiness.clone()
    }
    pub fn receive_ready(&self, channel: u16) -> bool {
        if channel == 2 {
            self.adapter.monitor_ready()
        } else {
            self.adapter.receive_ready()
        }
    }
    pub fn pump_receive(&mut self) -> Result<bool, Error> {
        let received = self.adapter.pump_receive()?;
        self.drain_observed_hci_events();
        Ok(received)
    }
    fn drain_observed_hci_events(&mut self) {
        while let Some(event) = self.adapter.pop_observed_event() {
            self.observe_hci_event(&event);
            if self.observed_to_raw.len() == 64 {
                self.observed_to_raw.pop_front();
            }
            self.observed_to_raw.push_back(event.clone());
            if self.management_events.len() == 64 {
                self.management_events.pop_front();
            }
            self.management_events.push_back(event);
        }
    }
    pub fn pop_management_event(&mut self) -> Option<Vec<u8>> {
        self.management_events.pop_front()
    }
    pub fn statistics(&self) -> tk_bt_hci::Statistics {
        self.adapter.statistics()
    }
    pub fn set_up(&mut self, up: bool) -> Result<(), Error> {
        self.adapter.set_up(up)
    }
    pub fn set_device_up(&mut self, up: bool) -> Result<(), Error> {
        self.adapter.set_up(up)?;
        if !up {
            self.management_settings &=
                !(MGMT_SETTING_POWERED | MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE);
            self.discovery_type = 0;
            return Ok(());
        }
        let initialization = (|| {
            if self.family_hint != tk_bt_hci::DeviceFamily::Unknown {
                self.adapter.intel_set_event_mask()?;
            }
            if let Ok(capabilities) = self.adapter.read_capabilities() {
                self.address = capabilities.address;
                self.capabilities = capabilities;
            }
            let mut event = [0u8; 16];
            if self
                .command_complete_raw(0x0c19, &[], &mut event)
                .is_ok_and(|length| length >= 7 && event[5] == 0)
            {
                self.management_settings &= !(MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE);
                if event[6] & 0x02 != 0 {
                    self.management_settings |= MGMT_SETTING_CONNECTABLE;
                }
                if event[6] & 0x01 != 0 {
                    self.management_settings |=
                        MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE;
                }
            }
            Ok::<(), Error>(())
        })();
        if let Err(error) = initialization {
            let _ = self.adapter.set_up(false);
            self.management_settings &=
                !(MGMT_SETTING_POWERED | MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE);
            return Err(error);
        }
        self.management_settings |= MGMT_SETTING_POWERED;
        if self.family_hint != tk_bt_hci::DeviceFamily::Unknown {
            self.management_settings |= MGMT_SETTING_BREDR | MGMT_SETTING_LE;
        }
        if self.privacy_enabled {
            self.sync_resolving_list()?;
        }
        Ok(())
    }
    pub fn management_set_privacy(
        &mut self,
        enabled: bool,
        local_irk: [u8; 16],
    ) -> Result<u32, Error> {
        let previous_enabled = self.privacy_enabled;
        let previous_irk = self.local_irk;
        self.privacy_enabled = enabled;
        self.local_irk = local_irk;
        if self.adapter.is_up() {
            if let Err(error) = self.sync_resolving_list() {
                self.privacy_enabled = previous_enabled;
                self.local_irk = previous_irk;
                return Err(error);
            }
        }
        if enabled {
            self.management_settings |= MGMT_SETTING_PRIVACY;
        } else {
            self.management_settings &= !MGMT_SETTING_PRIVACY;
        }
        Ok(self.management_settings)
    }
    fn sync_resolving_list(&mut self) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        if !self.privacy_enabled {
            return self.command_complete(0x202d, &[0]);
        }
        self.command_complete(0x2029, &[])?; // LE Clear Resolving List
        for index in 0..self.irks.len() {
            let record = self.irks[index];
            let address_type = match record[6] {
                1 => 0, // Public identity address
                2 => 1, // Random identity address
                _ => continue,
            };
            let mut parameters = [0u8; 39];
            parameters[0] = address_type;
            parameters[1..7].copy_from_slice(&record[..6]);
            parameters[7..23].copy_from_slice(&record[7..23]);
            parameters[23..39].copy_from_slice(&self.local_irk);
            self.command_complete(0x2027, &parameters)?; // LE Add Device To Resolving List
        }
        self.command_complete(0x202d, &[1]) // LE Set Address Resolution Enable
    }
    pub fn management_settings(&self) -> u32 {
        self.management_settings
    }
    /// Replace the host key database from a Linux mgmt LOAD_* command.
    /// The wire records are validated by the socket layer before this method.
    pub fn management_load_keys(&mut self, opcode: u16, parameters: &[u8]) -> Result<(), Error> {
        match opcode {
            0x0012 => {
                if parameters.len() < 3 || parameters[0] > 1 {
                    return Err(Error::InvalidLength);
                }
                let count = usize::from(u16::from_le_bytes([parameters[1], parameters[2]]));
                if count.checked_mul(25).and_then(|v| v.checked_add(3)) != Some(parameters.len()) {
                    return Err(Error::InvalidLength);
                }
                self.link_keys.clear();
                self.link_keys
                    .try_reserve(count)
                    .map_err(|_| Error::NoMemory)?;
                for record in parameters[3..].chunks_exact(25) {
                    let mut key = [0; 25];
                    key.copy_from_slice(record);
                    self.link_keys.push(key);
                }
            }
            0x0013 => {
                if parameters.len() < 2 {
                    return Err(Error::InvalidLength);
                }
                let count = usize::from(u16::from_le_bytes([parameters[0], parameters[1]]));
                if count.checked_mul(36).and_then(|v| v.checked_add(2)) != Some(parameters.len()) {
                    return Err(Error::InvalidLength);
                }
                self.long_term_keys.clear();
                self.long_term_keys
                    .try_reserve(count)
                    .map_err(|_| Error::NoMemory)?;
                for record in parameters[2..].chunks_exact(36) {
                    let mut key = [0; 36];
                    key.copy_from_slice(record);
                    self.long_term_keys.push(key);
                }
            }
            0x0030 => {
                if parameters.len() < 2 {
                    return Err(Error::InvalidLength);
                }
                let count = usize::from(u16::from_le_bytes([parameters[0], parameters[1]]));
                if count.checked_mul(23).and_then(|v| v.checked_add(2)) != Some(parameters.len()) {
                    return Err(Error::InvalidLength);
                }
                self.irks.clear();
                self.irks.try_reserve(count).map_err(|_| Error::NoMemory)?;
                for record in parameters[2..].chunks_exact(23) {
                    let mut key = [0; 23];
                    key.copy_from_slice(record);
                    self.irks.push(key);
                }
            }
            _ => return Err(Error::Unsupported),
        }
        if opcode == 0x0030 && self.adapter.is_up() && self.privacy_enabled {
            self.sync_resolving_list()?;
        }
        Ok(())
    }
    pub fn management_pair_device(
        &mut self,
        address: [u8; 6],
        address_type: u8,
        io_capability: u8,
    ) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        let mut command = [0u8; 28];
        let mut pairing = false;
        let length = match address_type {
            0 if self.management_settings & MGMT_SETTING_BREDR != 0 => {
                if io_capability > 4 {
                    return Err(Error::InvalidLength);
                }
                command[..2].copy_from_slice(&0x0405u16.to_le_bytes());
                command[2] = 13;
                command[3..9].copy_from_slice(&address);
                command[9..11].copy_from_slice(&0xcc18u16.to_le_bytes());
                command[11] = 1; // page scan repetition mode
                command[13..15].copy_from_slice(&0u16.to_le_bytes());
                command[15] = 1; // allow role switch
                pairing = true;
                16
            }
            1 | 2 if self.management_settings & MGMT_SETTING_LE != 0 => {
                command[..2].copy_from_slice(&0x200du16.to_le_bytes());
                command[2] = 25;
                command[3..5].copy_from_slice(&0x0060u16.to_le_bytes());
                command[5..7].copy_from_slice(&0x0030u16.to_le_bytes());
                command[7] = 0; // peer address filter policy
                command[8] = address_type - 1;
                command[9..15].copy_from_slice(&address);
                command[15] = 0; // public own address
                command[16..18].copy_from_slice(&0x0018u16.to_le_bytes());
                command[18..20].copy_from_slice(&0x0028u16.to_le_bytes());
                command[22..24].copy_from_slice(&0x01f4u16.to_le_bytes());
                28
            }
            0..=2 => return Err(Error::Unsupported),
            _ => return Err(Error::InvalidLength),
        };
        if pairing {
            self.pending_pairing
                .try_reserve(1)
                .map_err(|_| Error::NoMemory)?;
            self.pending_pairing
                .push((address, address_type, io_capability));
        }
        if let Err(error) = self.command_status(&command[..length]) {
            if pairing {
                self.pending_pairing
                    .retain(|(peer, kind, _)| *peer != address || *kind != address_type);
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn management_set_io_capability(&mut self, io_capability: u8) -> Result<(), Error> {
        if io_capability > 4 {
            return Err(Error::InvalidLength);
        }
        self.io_capability = io_capability;
        Ok(())
    }
    pub fn management_user_confirmation(
        &mut self,
        address: [u8; 6],
        accept: bool,
    ) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        self.command_complete(if accept { 0x042c } else { 0x042d }, &address)
    }
    pub fn management_pin_code_reply(
        &mut self,
        address: [u8; 6],
        pin: Option<&[u8]>,
    ) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        let mut parameters = [0u8; 23];
        parameters[..6].copy_from_slice(&address);
        let opcode = if let Some(pin) = pin {
            if pin.is_empty() || pin.len() > 16 {
                return Err(Error::InvalidLength);
            }
            parameters[6] = pin.len() as u8;
            parameters[7..7 + pin.len()].copy_from_slice(pin);
            0x040d
        } else {
            0x040e
        };
        self.command_complete(opcode, &parameters[..if pin.is_some() { 23 } else { 6 }])
    }
    pub fn management_disconnect(
        &mut self,
        address: [u8; 6],
        address_type: u8,
    ) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        let Some((_, _, handle)) = self
            .connections
            .iter()
            .find(|(peer, kind, _)| *peer == address && *kind == address_type)
            .copied()
        else {
            return Err(Error::Unsupported);
        };
        let mut command = [0u8; 6];
        command[..2].copy_from_slice(&0x0406u16.to_le_bytes());
        command[2] = 3;
        command[3..5].copy_from_slice(&handle.to_le_bytes());
        command[5] = 0x13; // Remote User Terminated Connection
        self.command_status(&command)
    }
    pub fn management_peer_for_handle(&self, handle: u16) -> Option<([u8; 6], u8)> {
        self.connections
            .iter()
            .find(|(_, _, connection)| *connection == handle)
            .map(|(address, kind, _)| (*address, *kind))
            .or_else(|| {
                self.recently_disconnected
                    .iter()
                    .find(|(connection, ..)| *connection == handle)
                    .map(|(_, address, kind)| (*address, *kind))
            })
    }
    pub fn take_management_peer_for_handle(&mut self, handle: u16) -> Option<([u8; 6], u8)> {
        let index = self
            .recently_disconnected
            .iter()
            .position(|(connection, ..)| *connection == handle)?;
        let (_, address, kind) = self.recently_disconnected.remove(index)?;
        Some((address, kind))
    }
    pub fn management_reset(&mut self) -> Result<(), Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        self.command_complete(0x0c03, &[])?;
        self.management_settings = MGMT_SETTING_POWERED;
        if self.family_hint != tk_bt_hci::DeviceFamily::Unknown {
            self.adapter.intel_set_event_mask()?;
        }
        if let Ok(capabilities) = self.adapter.read_capabilities() {
            self.address = capabilities.address;
            self.capabilities = capabilities;
        }
        let mut event = [0u8; 16];
        if self
            .command_complete_raw(0x0c19, &[], &mut event)
            .is_ok_and(|length| length >= 7 && event[5] == 0)
        {
            if event[6] & 0x02 != 0 {
                self.management_settings |= MGMT_SETTING_CONNECTABLE;
            }
            if event[6] & 0x01 != 0 {
                self.management_settings |= MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE;
            }
        }
        self.discovery_type = 0;
        Ok(())
    }
    pub fn management_discovery(&mut self, discovery_type: u8, start: bool) -> Result<bool, Error> {
        if !self.adapter.is_up() {
            return Err(Error::NotUp);
        }
        if !matches!(discovery_type, 1 | 6) {
            return Err(Error::Unsupported);
        }
        if start {
            if self.discovery_type != 0 {
                return if self.discovery_type == discovery_type {
                    Ok(false)
                } else {
                    Err(Error::Busy)
                };
            }
            if discovery_type == 1 {
                if self.management_settings & MGMT_SETTING_BREDR == 0 {
                    return Err(Error::Unsupported);
                }
                let command = [0x01, 0x04, 5, 0x33, 0x8b, 0x9e, 8, 0];
                self.command_status(&command)?;
            } else {
                self.command_complete(0x200b, &[1, 0x10, 0, 0x10, 0, 0, 0])?;
                self.command_complete(0x200c, &[1, 0])?;
            }
            self.discovery_type = discovery_type;
        } else {
            if self.discovery_type != discovery_type {
                return Err(Error::Unsupported);
            }
            if discovery_type == 1 {
                self.command_complete(0x0402, &[])?;
            } else {
                self.command_complete(0x200c, &[0, 0])?;
            }
            self.discovery_type = 0;
        }
        Ok(true)
    }
    /// Apply mgmt settings that map to a single acknowledged HCI operation.
    /// Host-only Bondable state is maintained here; scan flags, SSP, and LE
    /// are not published until the matching HCI Command Complete succeeds.
    pub fn set_management_setting(&mut self, setting: u32, enabled: bool) -> Result<u32, Error> {
        if !self.adapter.is_up() && setting != MGMT_SETTING_BREDR {
            return Err(Error::NotUp);
        }
        let mut next = self.management_settings;
        match setting {
            MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE => {
                let scan = if setting == MGMT_SETTING_CONNECTABLE {
                    if enabled {
                        next |= MGMT_SETTING_CONNECTABLE;
                    } else {
                        next &= !(MGMT_SETTING_CONNECTABLE | MGMT_SETTING_DISCOVERABLE);
                    }
                    if next & MGMT_SETTING_DISCOVERABLE != 0 {
                        0x03
                    } else if next & MGMT_SETTING_CONNECTABLE != 0 {
                        0x02
                    } else {
                        0
                    }
                } else {
                    if enabled {
                        next |= MGMT_SETTING_DISCOVERABLE | MGMT_SETTING_CONNECTABLE;
                    } else {
                        next &= !MGMT_SETTING_DISCOVERABLE;
                    }
                    if next & MGMT_SETTING_DISCOVERABLE != 0 {
                        0x03
                    } else if next & MGMT_SETTING_CONNECTABLE != 0 {
                        0x02
                    } else {
                        0
                    }
                };
                self.command_complete(0x0c1a, &[scan])?;
            }
            MGMT_SETTING_SSP => {
                self.command_complete(0x0c56, &[u8::from(enabled)])?;
                if enabled {
                    next |= setting
                } else {
                    next &= !setting
                }
            }
            MGMT_SETTING_LE => {
                self.command_complete(0x0c6d, &[u8::from(enabled), 0])?;
                if enabled {
                    next |= setting
                } else {
                    next &= !setting
                }
            }
            MGMT_SETTING_BREDR => {
                if enabled {
                    next |= setting
                } else {
                    next &= !setting;
                }
            }
            MGMT_SETTING_BONDABLE => {
                if enabled {
                    next |= setting
                } else {
                    next &= !setting
                }
            }
            _ => return Err(Error::Unsupported),
        }
        self.management_settings = next;
        Ok(next)
    }
    fn command_complete(&mut self, opcode: u16, parameters: &[u8]) -> Result<(), Error> {
        let mut event = [0u8; 260];
        let length = self.command_complete_raw(opcode, parameters, &mut event)?;
        if length < 6 || event[5] != 0 {
            return Err(Error::Unsupported);
        }
        Ok(())
    }
    fn command_complete_raw(
        &mut self,
        opcode: u16,
        parameters: &[u8],
        event: &mut [u8],
    ) -> Result<usize, Error> {
        if parameters.len() > u8::MAX as usize {
            return Err(Error::InvalidLength);
        }
        let mut command = [0u8; 258];
        command[..2].copy_from_slice(&opcode.to_le_bytes());
        command[2] = parameters.len() as u8;
        command[3..3 + parameters.len()].copy_from_slice(parameters);
        let result = self
            .adapter
            .command_complete(&command[..3 + parameters.len()], event);
        if self.adapter.receive_ready() {
            self.receive_readiness.wake();
        }
        result
    }
    fn command_status(&mut self, command: &[u8]) -> Result<(), Error> {
        let mut event = [0u8; 260];
        self.adapter.command_status(command, &mut event)?;
        if self.adapter.receive_ready() {
            self.receive_readiness.wake();
        }
        Ok(())
    }
    pub fn open(&mut self, channel: tk_bt_hci::Channel) -> Result<(), Error> {
        self.adapter.open(channel)
    }
    pub fn close(&mut self, channel: tk_bt_hci::Channel) {
        self.adapter.close(channel)
    }
    pub fn open_channel(&mut self, channel: u16) -> Result<(), Error> {
        let channel = match channel {
            0 => tk_bt_hci::Channel::Raw,
            1 => tk_bt_hci::Channel::User,
            2 => tk_bt_hci::Channel::Monitor,
            _ => return Err(Error::Unsupported),
        };
        self.adapter.open(channel)
    }
    pub fn close_channel(&mut self, channel: u16) {
        match channel {
            0 => self.adapter.close(tk_bt_hci::Channel::Raw),
            1 => self.adapter.close(tk_bt_hci::Channel::User),
            2 => self.adapter.close(tk_bt_hci::Channel::Monitor),
            _ => {}
        }
    }
    pub fn send_channel_packet(&mut self, channel: u16, packet: &[u8]) -> Result<(), Error> {
        let (&packet_type, payload) = packet.split_first().ok_or(Error::Truncated)?;
        let (channel, kind) = (
            match channel {
                0 => tk_bt_hci::Channel::Raw,
                1 => tk_bt_hci::Channel::User,
                _ => return Err(Error::Unsupported),
            },
            match packet_type {
                1 => tk_bt_hci::PacketType::Command,
                2 => tk_bt_hci::PacketType::Acl,
                3 => tk_bt_hci::PacketType::Sco,
                5 => tk_bt_hci::PacketType::Iso,
                _ => return Err(Error::Unsupported),
            },
        );
        self.adapter.submit(channel, kind, payload)
    }
    pub fn receive_channel_event(&mut self, channel: u16, out: &mut [u8]) -> Result<usize, Error> {
        if channel == 2 {
            if let Some(frame) = self.adapter.pop_monitor() {
                let length = 6 + usize::from(u16::from_le_bytes([frame[4], frame[5]]));
                if length > out.len() || length > frame.len() {
                    return Err(Error::InvalidLength);
                }
                out[..length].copy_from_slice(&frame[..length]);
                return Ok(length);
            }
        }
        if out.is_empty() {
            return Err(Error::InvalidLength);
        }
        let length = self.adapter.read_event(&mut out[1..])?;
        self.adapter.receive_event(&out[1..1 + length])?;
        if channel == 2 {
            if let Some(frame) = self.adapter.pop_monitor() {
                let length = 6 + usize::from(u16::from_le_bytes([frame[4], frame[5]]));
                if length > out.len() || length > frame.len() {
                    return Err(Error::InvalidLength);
                }
                out[..length].copy_from_slice(&frame[..length]);
                return Ok(length);
            }
        }
        out[0] = 4;
        Ok(length + 1)
    }
    /// Receive a Linux raw HCI packet or monitor record, multiplexing the USB
    /// event and ACL IN endpoints while keeping endpoint DMA buffers owned by
    /// the transport until each request completes.
    pub fn receive_channel_packet(
        &mut self,
        channel: u16,
        out: &mut [u8],
        nonblocking: bool,
    ) -> Result<usize, Error> {
        self.drain_observed_hci_events();
        if channel == 2 {
            if let Some(frame) = self.adapter.pop_monitor() {
                if frame.len() > out.len() {
                    return Err(Error::InvalidLength);
                }
                out[..frame.len()].copy_from_slice(&frame);
                return Ok(frame.len());
            }
        }
        if out.is_empty() {
            return Err(Error::InvalidLength);
        }
        let (kind, length) = self.adapter.receive_packet(&mut out[1..], nonblocking)?;
        if kind == tk_bt_hci::PacketType::Event {
            let event = &out[1..1 + length];
            if self
                .observed_to_raw
                .front()
                .is_some_and(|seen| seen.as_slice() == event)
            {
                self.observed_to_raw.pop_front();
            } else {
                self.observe_hci_event(event);
                if self.management_events.len() == 64 {
                    self.management_events.pop_front();
                }
                self.management_events.push_back(Vec::from(event));
            }
        }
        if channel == 2 {
            let frame = self.adapter.pop_monitor().ok_or(Error::Again)?;
            if frame.len() > out.len() {
                return Err(Error::InvalidLength);
            }
            out[..frame.len()].copy_from_slice(&frame);
            return Ok(frame.len());
        }
        out[0] = match kind {
            tk_bt_hci::PacketType::Event => 4,
            tk_bt_hci::PacketType::Acl => 2,
            _ => return Err(Error::Unsupported),
        };
        Ok(length + 1)
    }
    fn observe_hci_event(&mut self, event: &[u8]) {
        if event.len() < 2 || event.len() != usize::from(event[1]) + 2 {
            return;
        }
        match event[0] {
            // HCI IO Capability Request; use the per-pair mgmt override or
            // the persistent setting.
            0x31 if event.len() == 8 => {
                let mut address = [0; 6];
                address.copy_from_slice(&event[2..8]);
                let capability = self
                    .pending_pairing
                    .iter()
                    .find(|(peer, kind, _)| *peer == address && *kind == 0)
                    .map(|(_, _, capability)| *capability)
                    .unwrap_or(self.io_capability);
                let mut parameters = [0u8; 9];
                parameters[..6].copy_from_slice(&address);
                parameters[6] = capability;
                parameters[7] = 0; // no OOB data
                parameters[8] = 0; // no MITM requirement
                let _ = self.command_complete(0x042b, &parameters);
            }
            // HCI Link Key Request: answer from the host key database or
            // explicitly report that no stored key is available.
            0x17 if event.len() == 8 => {
                let mut address = [0; 6];
                address.copy_from_slice(&event[2..8]);
                if let Some(record) = self.link_keys.iter().find(|key| key[..6] == address) {
                    let mut parameters = [0u8; 22];
                    parameters[..6].copy_from_slice(&address);
                    parameters[6..].copy_from_slice(&record[8..24]);
                    let _ = self.command_complete(0x040b, &parameters);
                } else {
                    let _ = self.command_complete(0x040c, &address);
                }
            }
            0x03 if event.len() >= 11 => {
                let status = event[2];
                let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
                let mut address = [0; 6];
                address.copy_from_slice(&event[5..11]);
                self.update_connection(address, 0, handle, status == 0);
                if status == 0
                    && self
                        .pending_pairing
                        .iter()
                        .any(|(peer, kind, _)| *peer == address && *kind == 0)
                {
                    let mut command = [0u8; 5];
                    command[..2].copy_from_slice(&0x0411u16.to_le_bytes());
                    command[2] = 2;
                    command[3..5].copy_from_slice(&handle.to_le_bytes());
                    let _ = self.command_status(&command);
                }
            }
            0x3e if event.len() >= 14 && event[2] == 0x01 => {
                let status = event[3];
                let handle = u16::from_le_bytes([event[4], event[5]]) & 0x0fff;
                let kind = match event[7] {
                    0 => 1,
                    1 => 2,
                    _ => return,
                };
                let mut address = [0; 6];
                address.copy_from_slice(&event[8..14]);
                self.update_connection(address, kind, handle, status == 0);
            }
            // HCI LE Long Term Key Request: look up the exact address,
            // address type, EDIV and Rand tuple supplied in the mgmt key.
            0x3e if event.len() == 15 && event[2] == 0x05 => {
                let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
                let ediv = u16::from_le_bytes([event[13], event[14]]);
                let mut random = [0; 8];
                random.copy_from_slice(&event[5..13]);
                let peer = self.management_peer_for_handle(handle);
                let key = peer.and_then(|(address, address_type)| {
                    self.long_term_keys.iter().find(|record| {
                        record[..6] == address
                            && record[6] == address_type
                            && u16::from_le_bytes([record[10], record[11]]) == ediv
                            && record[12..20] == random
                    })
                });
                if let Some(record) = key {
                    let mut parameters = [0u8; 18];
                    parameters[..2].copy_from_slice(&handle.to_le_bytes());
                    parameters[2..].copy_from_slice(&record[20..36]);
                    let _ = self.command_complete(0x201a, &parameters);
                } else {
                    let _ = self.command_complete(0x201b, &handle.to_le_bytes());
                }
            }
            0x05 if event.len() >= 6 => {
                let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
                let peer = self.management_peer_for_handle(handle);
                self.connections
                    .retain(|(_, _, connected)| *connected != handle);
                if let Some((address, kind)) = peer {
                    if self.recently_disconnected.len() == 64 {
                        self.recently_disconnected.pop_front();
                    }
                    self.recently_disconnected
                        .push_back((handle, address, kind));
                }
            }
            0x06 if event.len() >= 5 => {
                let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
                if let Some((address, address_type)) = self.management_peer_for_handle(handle) {
                    self.pending_pairing
                        .retain(|(peer, kind, _)| *peer != address || *kind != address_type);
                }
            }
            _ => {}
        }
    }
    fn update_connection(
        &mut self,
        address: [u8; 6],
        address_type: u8,
        handle: u16,
        connected: bool,
    ) {
        self.connections
            .retain(|(peer, kind, _)| *peer != address || *kind != address_type);
        if connected {
            self.connections.push((address, address_type, handle));
        }
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
        let length = self.adapter.read_event(out)?;
        self.adapter.receive_event(&out[..length])?;
        Ok(length)
    }
    pub fn read_acl(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let length = self.adapter.read_acl(out)?;
        self.adapter.receive_acl(&out[..length])?;
        Ok(length)
    }
    pub fn pop_monitor(&mut self) -> Option<Vec<u8>> {
        self.adapter.pop_monitor()
    }
    pub fn monitor_ready(&self) -> bool {
        self.adapter.monitor_ready()
    }
    pub fn intel_get_version(&mut self) -> Result<tk_bt_hci::Version, Error> {
        self.adapter.intel_get_version()
    }
    pub fn intel_get_version_tlv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.adapter.intel_get_version_tlv(out)
    }
    pub fn intel_get_boot_params(&mut self) -> Result<tk_bt_hci::BootParams, Error> {
        self.adapter.intel_get_boot_params()
    }
    pub fn intel_identify(&mut self) -> Result<tk_bt_hci::DeviceFamily, Error> {
        self.adapter.intel_identify(self.family_hint)
    }
    pub fn run_intel_patch(&mut self, image: &[u8]) -> Result<bool, Error> {
        self.adapter.run_intel_patch(image)
    }
    pub fn intel_load_rsa_header(&mut self, firmware: &[u8]) -> Result<(), Error> {
        self.adapter.intel_load_rsa_header(firmware)
    }
    pub fn intel_load_ecdsa_header(&mut self, firmware: &[u8]) -> Result<(), Error> {
        self.adapter.intel_load_ecdsa_header(firmware)
    }
    pub fn intel_load_firmware(&mut self, firmware: &[u8], offset: usize) -> Result<u32, Error> {
        self.adapter.intel_load_firmware(firmware, offset)
    }
    pub fn intel_init_firmware(
        &mut self,
        firmware: &[u8],
        hw_variant: u8,
        sbe_type: u8,
    ) -> Result<u32, Error> {
        self.adapter
            .intel_init_firmware(firmware, hw_variant, sbe_type)
    }
    pub fn intel_load_ddc(&mut self, data: &[u8]) -> Result<(), Error> {
        self.adapter.intel_load_ddc(data)
    }
    pub fn intel_bt_reset(&mut self) -> Result<(), Error> {
        self.adapter.intel_bt_reset()
    }
    pub fn intel_reset(&mut self, boot_param: u32) -> Result<(), Error> {
        self.adapter.intel_reset(boot_param)
    }
    pub fn intel_enter_manufacturer(&mut self) -> Result<(), Error> {
        self.adapter.intel_enter_manufacturer()
    }
    pub fn intel_exit_manufacturer(&mut self, mode: u8) -> Result<(), Error> {
        self.adapter.intel_exit_manufacturer(mode)
    }
    pub fn intel_set_event_mask(&mut self) -> Result<(), Error> {
        self.adapter.intel_set_event_mask()
    }

    fn load_intel_firmware_after_rootfs(&mut self) -> Result<(), Error> {
        if self.family_hint == tk_bt_hci::DeviceFamily::Unknown {
            return Ok(());
        }
        self.set_up(true)?;
        match self.intel_identify()? {
            tk_bt_hci::DeviceFamily::I7260 => self.handle_7260_firmware(),
            tk_bt_hci::DeviceFamily::I8260 => self.handle_8260_firmware(),
            tk_bt_hci::DeviceFamily::I9260 => self.handle_9260_firmware(),
            tk_bt_hci::DeviceFamily::Unknown => Err(Error::Unsupported),
        }
    }

    // upstream: main.c handle_7260()
    fn handle_7260_firmware(&mut self) -> Result<(), Error> {
        let version = self.intel_get_version()?;
        if version.fw_patch_num != 0 {
            return Ok(());
        }
        let primary = tk_bt_hci::get_fwname(&version, None, "/lib/firmware/intel", "bseq")
            .ok_or(Error::Unsupported)?;
        let firmware = axdriver_base::firmware::request(&primary, MAX_INTEL_FIRMWARE)
            .or_else(|| {
                tk_bt_hci::get_fwname_fallback(&version, "/lib/firmware/intel", "bseq")
                    .and_then(|path| axdriver_base::firmware::request(&path, MAX_INTEL_FIRMWARE))
            })
            .ok_or(Error::NoDevice)?;
        self.intel_enter_manufacturer()?;
        let activate = match self.run_intel_patch(&firmware) {
            Ok(activate) => activate,
            Err(error) => {
                let _ = self.intel_exit_manufacturer(1);
                return Err(error);
            }
        };
        self.intel_exit_manufacturer(if activate { 2 } else { 0 })?;
        let _ = self.intel_get_version();
        if self.intel_enter_manufacturer().is_ok() {
            let _ = self.intel_set_event_mask();
            let _ = self.intel_exit_manufacturer(0);
        }
        Ok(())
    }

    // upstream: main.c handle_8260()
    fn handle_8260_firmware(&mut self) -> Result<(), Error> {
        let version = self.intel_get_version()?;
        if version.fw_variant == 0x23 {
            return Ok(());
        }
        if version.fw_variant != 0x06 {
            return Err(Error::Unsupported);
        }
        let params = self.intel_get_boot_params()?;
        self.address = params.otp_bdaddr;
        if params.limited_cce != 0 {
            return Err(Error::Unsupported);
        }
        let path = tk_bt_hci::get_fwname(&version, Some(&params), "/lib/firmware/intel", "sfi")
            .ok_or(Error::Unsupported)?;
        let firmware =
            axdriver_base::firmware::request(&path, MAX_INTEL_FIRMWARE).ok_or(Error::NoDevice)?;
        let boot_param = self.intel_init_firmware(&firmware, version.hw_variant, 0)?;
        self.intel_reset(boot_param)?;
        let operational = self.intel_get_version().unwrap_or(version);
        if let Some(path) =
            tk_bt_hci::get_fwname(&operational, Some(&params), "/lib/firmware/intel", "ddc")
            && let Some(ddc) = axdriver_base::firmware::request(&path, MAX_INTEL_FIRMWARE)
        {
            let _ = self.intel_load_ddc(&ddc);
        }
        let _ = self.intel_set_event_mask();
        Ok(())
    }

    // upstream: main.c handle_9260()
    fn handle_9260_firmware(&mut self) -> Result<(), Error> {
        let mut raw = [0u8; 255];
        let len = self.intel_get_version_tlv(&mut raw)?;
        let mut version = tk_bt_hci::VersionTlv::default();
        tk_bt_hci::parse_tlv(&raw[..len], &mut version).map_err(|_| Error::InvalidLength)?;
        self.address = version.otp_bd_addr;
        if version.img_type == 0x03 {
            return Ok(());
        }
        if version.img_type != 0x01 || version.limited_cce != 0 || version.sbe_type > 1 {
            return Err(Error::Unsupported);
        }
        let path = tk_bt_hci::get_fwname_tlv(&version, "/lib/firmware/intel", "sfi");
        let firmware =
            axdriver_base::firmware::request(&path, MAX_INTEL_FIRMWARE).ok_or(Error::NoDevice)?;
        let hw_variant = ((version.cnvi_bt >> 16) & 0x3f) as u8;
        let boot_param = self.intel_init_firmware(&firmware, hw_variant, version.sbe_type)?;
        self.intel_reset(boot_param)?;

        let mut operational = version;
        if let Ok(len) = self.intel_get_version_tlv(&mut raw) {
            let _ = tk_bt_hci::parse_tlv(&raw[..len], &mut operational);
        }
        let ddc_path = tk_bt_hci::get_fwname_tlv(&operational, "/lib/firmware/intel", "ddc");
        if let Some(ddc) = axdriver_base::firmware::request(&ddc_path, MAX_INTEL_FIRMWARE) {
            let _ = self.intel_load_ddc(&ddc);
        }
        let _ = self.intel_set_event_mask();
        Ok(())
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
    fn begin_command_transaction(&mut self) -> Result<(), Error> {
        // A persistent event-IN request may already own the endpoint. Reap
        // it first; otherwise cancel and wait for the exact request token
        // before issuing the synchronous command-complete read.
        self.reclaim_event()?;
        if let Some(id) = self.event_request.take() {
            if self.event.cancel(id).is_err() {
                if let Some(completion) = self.host.reclaim(&self.event, id).map_err(|_| {
                    self.host.halt();
                    Error::NoDevice
                })? {
                    if completion.status != TransferStatus::Completed {
                        return Err(Error::NoDevice);
                    }
                    self.event_ready = Some(completion.actual_length);
                } else {
                    self.host.halt();
                    return Err(Error::NoDevice);
                }
            } else {
                loop {
                    if self.host.pump().is_err() {
                        self.host.halt();
                        return Err(Error::NoDevice);
                    }
                    if let Some(completion) = self.host.reclaim(&self.event, id).map_err(|_| {
                        self.host.halt();
                        Error::NoDevice
                    })? {
                        if completion.status != TransferStatus::Completed {
                            return Err(Error::NoDevice);
                        }
                        self.event_ready = Some(completion.actual_length);
                        break;
                    }
                    core::hint::spin_loop();
                }
            }
        }
        Ok(())
    }
    fn end_command_transaction(&mut self) {
        let _ = self.submit_event();
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
        if let Some(length) = self.event_ready.take() {
            if length > out.len() {
                return Err(Error::Truncated);
            }
            out[..length].copy_from_slice(&self.event_buffer[..length]);
            return Ok(length);
        }
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
    fn read_packet(
        &mut self,
        out: &mut [u8],
        nonblocking: bool,
    ) -> Result<Option<(tk_bt_hci::PacketType, usize)>, Error> {
        if out.is_empty() {
            return Err(Error::InvalidLength);
        }
        loop {
            self.submit_event()?;
            self.submit_acl()?;
            self.host.pump().map_err(|_| Error::NoDevice)?;
            self.reclaim_event()?;
            self.reclaim_acl()?;
            if let Some(length) = self.event_ready {
                if length > out.len() {
                    return Err(Error::InvalidLength);
                }
                if length != 0 {
                    out[..length].copy_from_slice(&self.event_buffer[..length]);
                    self.event_ready = None;
                    return Ok(Some((tk_bt_hci::PacketType::Event, length)));
                }
                self.event_ready = None;
            }
            if let Some(length) = self.acl_ready {
                if length > out.len() {
                    return Err(Error::InvalidLength);
                }
                if length != 0 {
                    out[..length].copy_from_slice(&self.acl_buffer[..length]);
                    self.acl_ready = None;
                    return Ok(Some((tk_bt_hci::PacketType::Acl, length)));
                }
                self.acl_ready = None;
            }
            if nonblocking {
                return Ok(None);
            }
            core::hint::spin_loop();
        }
    }
    fn stop(&mut self) {
        if let Some(id) = self.event_request.take() {
            self.cancel_request(&self.event.clone(), id);
        }
        if let Some(id) = self.acl_request.take() {
            self.cancel_request(&self.acl_in.clone(), id);
        }
        self.event_ready = None;
        self.acl_ready = None;
    }
}

pub type RegisteredHci = Arc<Mutex<UsbBluetoothHci>>;
static DEVICES: spin::Once<Mutex<Vec<RegisteredHci>>> = spin::Once::new();
pub(super) fn register(device: UsbBluetoothHci) {
    let intel = device.family_hint != tk_bt_hci::DeviceFamily::Unknown;
    let Ok(device) = Arc::try_new(Mutex::new(device)) else {
        warn!("USB Bluetooth adapter registry allocation failed");
        return;
    };
    DEVICES
        .call_once(|| Mutex::new(Vec::new()))
        .lock()
        .push(device.clone());
    let readiness = device.lock().receive_readiness();
    let receive_device = device.clone();
    if let Err(error) = axtask::spawn(move || {
        loop {
            match receive_device.lock().pump_receive() {
                Ok(true) => {
                    readiness.wake();
                }
                Ok(false) | Err(_) => {
                    let _ = axtask::sleep(core::time::Duration::from_millis(2));
                }
            }
        }
    }) {
        warn!("USB Bluetooth receive worker could not start: {error:?}");
    }
    if intel && axdriver_base::firmware::rootfs_ready() {
        load_firmware_after_rootfs();
    } else if intel && !FIRMWARE_CALLBACK_REGISTERED.load(core::sync::atomic::Ordering::Acquire) {
        if FIRMWARE_CALLBACK_REGISTERED
            .compare_exchange(
                false,
                true,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
            && !axdriver_base::firmware::on_rootfs_ready(load_firmware_after_rootfs)
        {
            FIRMWARE_CALLBACK_REGISTERED.store(false, core::sync::atomic::Ordering::Release);
            warn!("USB Bluetooth firmware callback table full; Intel firmware is deferred");
        }
    }
}

fn load_firmware_after_rootfs() {
    let devices = DEVICES.call_once(|| Mutex::new(Vec::new())).lock().clone();
    for device in &devices {
        if let Err(error) = device.lock().load_intel_firmware_after_rootfs() {
            warn!("Intel Bluetooth firmware initialization failed: {error:?}");
        }
    }
}
pub fn bluetooth_devices() -> Vec<RegisteredHci> {
    DEVICES.call_once(|| Mutex::new(Vec::new())).lock().clone()
}
