//! Bluetooth HCI packet formats, USB endpoint contract and channel model.
//!
//! The endpoint routing follows FreeBSD `sys/netgraph/bluetooth/drivers/ubt/
//! ng_ubt.c` (BSD-2-Clause, Copyright (c) 2001-2009 Maksim Yevmenkin), with
//! no netgraph code carried over. HCI packet layouts and Linux socket channel
//! values follow the Bluetooth Core specification and UAPI semantics.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod iwmbt_fw;
use alloc::{collections::VecDeque, vec::Vec};

pub use iwmbt_fw::{
    BootParams, DeviceFamily, FirmwareError, PatchCommand, Version, VersionTlv, get_fwname,
    get_fwname_fallback, get_fwname_tlv, parse_patch, parse_tlv, parse_version_event,
    supported_device,
};

pub const AF_BLUETOOTH: i32 = 31;
pub const BTPROTO_HCI: i32 = 1;
pub const HCI_CHANNEL_RAW: u32 = 0;
pub const HCI_CHANNEL_USER: u32 = 1;
pub const HCI_CHANNEL_MONITOR: u32 = 2;
pub const HCI_CHANNEL_CONTROL: u32 = 3;
pub const BT_IOC_MAGIC: u8 = b'H';
pub const HCIDEVUP: u32 = ioctl(1, 201, 4);
pub const HCIDEVDOWN: u32 = ioctl(1, 202, 4);
pub const HCIGETDEVLIST: u32 = ioctl(2, 210, 4);
pub const HCIGETDEVINFO: u32 = ioctl(2, 211, 4);

/// Linux _IOC encoding for the fixed integer hci_dev ioctl arguments.
const fn ioctl(direction: u32, number: u32, size: u32) -> u32 {
    (direction << 30) | (size << 16) | ((BT_IOC_MAGIC as u32) << 8) | number
}

fn read_le32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let end = offset.checked_add(4).ok_or(Error::InvalidLength)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(Error::InvalidLength)?
        .try_into()
        .map_err(|_| Error::InvalidLength)?;
    Ok(u32::from_le_bytes(raw))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketType {
    Command,
    Acl,
    Sco,
    Event,
    Iso,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    InvalidLength,
    Unsupported,
    NoDevice,
    Busy,
    NotUp,
    Again,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    pub kind: PacketType,
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    /// Validate the HCI header and exact packet payload length before queueing.
    pub fn parse(kind: PacketType, bytes: &'a [u8]) -> Result<Self, Error> {
        let (header, length_offset, length_width) = match kind {
            PacketType::Command => (3, 2, 1),
            PacketType::Acl => (4, 2, 2),
            PacketType::Sco => (3, 2, 1),
            PacketType::Event => (2, 1, 1),
            PacketType::Iso => (4, 2, 2),
        };
        if bytes.len() < header {
            return Err(Error::Truncated);
        }
        let payload_len = if length_width == 1 {
            usize::from(bytes[length_offset])
        } else {
            usize::from(
                u16::from_le_bytes([bytes[length_offset], bytes[length_offset + 1]]) & 0x3fff,
            )
        };
        if bytes.len() != header + payload_len {
            return Err(Error::InvalidLength);
        }
        Ok(Self {
            kind,
            payload: bytes,
        })
    }
}

/// USB Bluetooth transport uses control endpoint for HCI commands, interrupt
/// IN for events, bulk IN/OUT for ACL, and optional isochronous for SCO/ISO.
pub trait UsbTransport {
    fn control_command(&mut self, command: &[u8]) -> Result<(), Error>;
    fn bulk_acl_out(&mut self, packet: &[u8]) -> Result<(), Error>;
    fn read_interrupt_event(&mut self, out: &mut [u8]) -> Result<usize, Error>;
    fn read_bulk_acl(&mut self, out: &mut [u8]) -> Result<usize, Error>;
    /// Nonblocking, multiplexed receive of HCI events and ACL packets. The
    /// transport may retain endpoint requests between calls.
    fn read_packet(
        &mut self,
        _out: &mut [u8],
        _nonblocking: bool,
    ) -> Result<Option<(PacketType, usize)>, Error> {
        Ok(None)
    }
    fn stop(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    Raw,
    User,
    Monitor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub id: u16,
    pub address: [u8; 6],
    pub flags: u32,
    pub name: [u8; 8],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Statistics {
    pub err_rx: u32,
    pub err_tx: u32,
    pub cmd_tx: u32,
    pub evt_rx: u32,
    pub acl_tx: u32,
    pub acl_rx: u32,
    pub sco_tx: u32,
    pub sco_rx: u32,
    pub byte_rx: u32,
    pub byte_tx: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HciCapabilities {
    pub address: [u8; 6],
    pub features: [u8; 8],
    pub acl_mtu: u16,
    pub acl_packets: u16,
    pub sco_mtu: u16,
    pub sco_packets: u16,
}

/// Small HCI device/channel owner independent of AF_BLUETOOTH socket plumbing.
pub struct Adapter<T> {
    transport: T,
    index: u16,
    up: bool,
    user_owner: bool,
    raw_users: u16,
    monitor_users: u16,
    monitor: VecDeque<Vec<u8>>,
    stats: Statistics,
}

impl<T: UsbTransport> Adapter<T> {
    pub fn new(transport: T, index: u16) -> Self {
        Self {
            transport,
            index,
            up: false,
            user_owner: false,
            raw_users: 0,
            monitor_users: 0,
            monitor: VecDeque::new(),
            stats: Statistics::default(),
        }
    }
    pub fn index(&self) -> u16 {
        self.index
    }
    pub fn is_up(&self) -> bool {
        self.up
    }
    pub fn statistics(&self) -> Statistics {
        self.stats
    }
    /// Query the standard controller address, feature bitmap and ACL/SCO
    /// buffer limits for HCIGETDEVINFO. Values are device replies, not guesses.
    pub fn read_capabilities(&mut self) -> Result<HciCapabilities, Error> {
        let mut capabilities = HciCapabilities::default();
        let mut event = [0u8; 32];
        let length = self.command_complete(&[0x09, 0x10, 0], &mut event)?;
        if length < 12 || event[0] != 0x0e || event[5] != 0 {
            return Err(Error::InvalidLength);
        }
        capabilities.address.copy_from_slice(&event[6..12]);

        let length = self.command_complete(&[0x03, 0x10, 0], &mut event)?;
        if length < 14 || event[0] != 0x0e || event[5] != 0 {
            return Err(Error::InvalidLength);
        }
        capabilities.features.copy_from_slice(&event[6..14]);

        let length = self.command_complete(&[0x05, 0x10, 0], &mut event)?;
        if length < 13 || event[0] != 0x0e || event[5] != 0 {
            return Err(Error::InvalidLength);
        }
        capabilities.acl_mtu = u16::from_le_bytes([event[6], event[7]]);
        capabilities.sco_mtu = u16::from(event[8]);
        capabilities.acl_packets = u16::from_le_bytes([event[9], event[10]]);
        capabilities.sco_packets = u16::from_le_bytes([event[11], event[12]]);
        Ok(capabilities)
    }
    pub fn open(&mut self, channel: Channel) -> Result<(), Error> {
        match channel {
            Channel::Raw => {
                if self.user_owner {
                    return Err(Error::Busy);
                }
                self.raw_users = self.raw_users.saturating_add(1);
            }
            Channel::Monitor => self.monitor_users = self.monitor_users.saturating_add(1),
            Channel::User => {
                if self.user_owner || self.raw_users != 0 {
                    return Err(Error::Busy);
                }
                self.user_owner = true;
            }
        }
        Ok(())
    }
    pub fn close(&mut self, channel: Channel) {
        match channel {
            Channel::Raw => self.raw_users = self.raw_users.saturating_sub(1),
            Channel::Monitor => self.monitor_users = self.monitor_users.saturating_sub(1),
            Channel::User => self.user_owner = false,
        }
    }
    pub fn set_up(&mut self, up: bool) -> Result<(), Error> {
        if up == self.up {
            return Ok(());
        }
        if !up {
            self.transport.stop();
        }
        self.up = up;
        Ok(())
    }
    pub fn submit(
        &mut self,
        channel: Channel,
        kind: PacketType,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if channel == Channel::Monitor {
            return Err(Error::Unsupported);
        }
        if channel == Channel::User && !self.user_owner {
            return Err(Error::Busy);
        }
        if !self.up {
            return Err(Error::NotUp);
        }
        if let Err(error) = Packet::parse(kind, bytes) {
            self.stats.err_tx = self.stats.err_tx.saturating_add(1);
            return Err(error);
        }
        match kind {
            PacketType::Command => {
                if let Err(error) = self.transport.control_command(bytes) {
                    self.stats.err_tx = self.stats.err_tx.saturating_add(1);
                    return Err(error);
                }
                self.queue_monitor(2, bytes);
                self.stats.cmd_tx = self.stats.cmd_tx.saturating_add(1);
                self.stats.byte_tx = self.stats.byte_tx.saturating_add(bytes.len() as u32);
                Ok(())
            }
            PacketType::Acl => {
                if let Err(error) = self.transport.bulk_acl_out(bytes) {
                    self.stats.err_tx = self.stats.err_tx.saturating_add(1);
                    return Err(error);
                }
                self.queue_monitor(4, bytes);
                self.stats.acl_tx = self.stats.acl_tx.saturating_add(1);
                self.stats.byte_tx = self.stats.byte_tx.saturating_add(bytes.len() as u32);
                Ok(())
            }
            _ => Err(Error::Unsupported),
        }
    }
    /// HCI event capture is copied to monitor observers with a leading packet
    /// type byte. This bounded queue drops the oldest packet on overflow.
    pub fn receive_event(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let Err(error) = Packet::parse(PacketType::Event, bytes) {
            self.stats.err_rx = self.stats.err_rx.saturating_add(1);
            return Err(error);
        }
        self.stats.evt_rx = self.stats.evt_rx.saturating_add(1);
        self.stats.byte_rx = self.stats.byte_rx.saturating_add(bytes.len() as u32);
        if self.monitor_users != 0 {
            self.queue_monitor(3, bytes);
        }
        Ok(())
    }
    pub fn receive_acl(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let Err(error) = Packet::parse(PacketType::Acl, bytes) {
            self.stats.err_rx = self.stats.err_rx.saturating_add(1);
            return Err(error);
        }
        self.stats.acl_rx = self.stats.acl_rx.saturating_add(1);
        self.stats.byte_rx = self.stats.byte_rx.saturating_add(bytes.len() as u32);
        if self.monitor_users != 0 {
            self.queue_monitor(5, bytes);
        }
        Ok(())
    }
    fn queue_monitor(&mut self, opcode: u16, bytes: &[u8]) {
        if self.monitor_users == 0 {
            return;
        }
        let length = bytes.len().min(1028);
        let mut frame = Vec::new();
        if frame.try_reserve_exact(6 + length).is_err() {
            return;
        }
        frame.resize(6 + length, 0);
        frame[..2].copy_from_slice(&opcode.to_le_bytes());
        frame[2..4].copy_from_slice(&self.index.to_le_bytes());
        frame[4..6].copy_from_slice(&(length as u16).to_le_bytes());
        frame[6..6 + length].copy_from_slice(&bytes[..length]);
        if self.monitor.len() == 64 {
            self.monitor.pop_front();
        }
        self.monitor.push_back(frame);
    }
    pub fn pop_monitor(&mut self) -> Option<Vec<u8>> {
        self.monitor.pop_front()
    }
    pub fn monitor_ready(&self) -> bool {
        !self.monitor.is_empty()
    }
    pub fn read_event(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        match self.transport.read_interrupt_event(out) {
            Ok(length) => Ok(length),
            Err(error) => {
                self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                Err(error)
            }
        }
    }
    pub fn read_acl(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        match self.transport.read_bulk_acl(out) {
            Ok(length) => Ok(length),
            Err(error) => {
                self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                Err(error)
            }
        }
    }
    /// Receive a single event or ACL packet with Linux HCI packet type
    /// framing left to the socket adapter. Monitor capture is updated here.
    pub fn receive_packet(
        &mut self,
        out: &mut [u8],
        nonblocking: bool,
    ) -> Result<(PacketType, usize), Error> {
        let Some((kind, length)) = self.transport.read_packet(out, nonblocking)? else {
            return Err(Error::Again);
        };
        if length > out.len() {
            self.stats.err_rx = self.stats.err_rx.saturating_add(1);
            return Err(Error::InvalidLength);
        }
        let bytes = &out[..length];
        let parsed = Packet::parse(kind, bytes);
        if let Err(error) = parsed {
            self.stats.err_rx = self.stats.err_rx.saturating_add(1);
            return Err(error);
        }
        match kind {
            PacketType::Event => {
                self.stats.evt_rx = self.stats.evt_rx.saturating_add(1);
                self.queue_monitor(3, bytes);
            }
            PacketType::Acl => {
                self.stats.acl_rx = self.stats.acl_rx.saturating_add(1);
                self.queue_monitor(5, bytes);
            }
            _ => return Err(Error::Unsupported),
        }
        self.stats.byte_rx = self.stats.byte_rx.saturating_add(length as u32);
        Ok((kind, length))
    }
    // upstream: iwmbt_hw.c iwmbt_hci_command()
    /// Send an HCI command and read its matching Command Complete event. This
    /// is used by the Intel boot-time firmware query sequence.
    pub fn command_complete(&mut self, command: &[u8], event: &mut [u8]) -> Result<usize, Error> {
        let parsed = match Packet::parse(PacketType::Command, command) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.stats.err_tx = self.stats.err_tx.saturating_add(1);
                return Err(error);
            }
        };
        if !self.up {
            return Err(Error::NotUp);
        }
        let opcode = u16::from_le_bytes([parsed.payload[0], parsed.payload[1]]);
        if let Err(error) = self.transport.control_command(command) {
            self.stats.err_tx = self.stats.err_tx.saturating_add(1);
            return Err(error);
        }
        self.stats.cmd_tx = self.stats.cmd_tx.saturating_add(1);
        self.stats.byte_tx = self.stats.byte_tx.saturating_add(command.len() as u32);
        self.queue_monitor(2, command);
        let length = match self.transport.read_interrupt_event(event) {
            Ok(length) => length,
            Err(error) => {
                self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                return Err(error);
            }
        };
        if length > event.len() {
            self.stats.err_rx = self.stats.err_rx.saturating_add(1);
            return Err(Error::InvalidLength);
        }
        let received = match Packet::parse(PacketType::Event, &event[..length]) {
            Ok(received) => received,
            Err(error) => {
                self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                return Err(error);
            }
        };
        self.stats.evt_rx = self.stats.evt_rx.saturating_add(1);
        self.stats.byte_rx = self.stats.byte_rx.saturating_add(length as u32);
        self.queue_monitor(3, &event[..length]);
        if received.payload[0] == 0xff {
            return Ok(length);
        }
        if received.payload[0] != 0x0e || length < 5 {
            return Err(Error::InvalidLength);
        }
        let completed = u16::from_le_bytes([received.payload[3], received.payload[4]]);
        if completed != opcode {
            return Err(Error::Unsupported);
        }
        Ok(length)
    }
    /// Query the Intel firmware version using vendor command 0xfc05.
    // upstream: iwmbt_hw.c iwmbt_get_version()
    pub fn intel_get_version(&mut self) -> Result<Version, Error> {
        let mut event = [0u8; 32];
        let length = self.command_complete(&[0x05, 0xfc, 0], &mut event)?;
        iwmbt_fw::parse_version_event(&event[..length]).map_err(|_| Error::InvalidLength)
    }
    /// Query the extended Intel TLV firmware-version record (vendor command
    /// 0xfc05 with the 0xff selector).
    // upstream: iwmbt_hw.c iwmbt_read_version_tlv()
    pub fn intel_get_version_tlv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let mut event = [0u8; 260];
        let length = self.command_complete(&[0x05, 0xfc, 1, 0xff], &mut event)?;
        if length < 6 || event[5] != 0 {
            return Err(Error::InvalidLength);
        }
        let payload = &event[5..length];
        if payload.len() > out.len() {
            return Err(Error::InvalidLength);
        }
        out[..payload.len()].copy_from_slice(payload);
        Ok(payload.len())
    }
    /// Read Intel bootloader secure-boot parameters with opcode 0xfc0d.
    // upstream: iwmbt_hw.c iwmbt_get_boot_params()
    pub fn intel_get_boot_params(&mut self) -> Result<BootParams, Error> {
        let mut event = [0u8; 32];
        let length = self.command_complete(&[0x0d, 0xfc, 0], &mut event)?;
        if length < 28 || event[5] != 0 {
            return Err(Error::InvalidLength);
        }
        BootParams::parse(&event[5..28]).map_err(|_| Error::InvalidLength)
    }
    /// Determine the Intel firmware generation from its fixed or TLV version
    /// record, retaining the USB VID/PID family as the initial device class.
    // upstream: main.c iwmbt_identify()
    pub fn intel_identify(&mut self, usb_family: DeviceFamily) -> Result<DeviceFamily, Error> {
        if usb_family == DeviceFamily::Unknown {
            return Err(Error::Unsupported);
        }
        if usb_family == DeviceFamily::I7260 {
            self.intel_bt_reset()?;
        }
        let mut data = [0u8; 255];
        let length = self.intel_get_version_tlv(&mut data)?;
        if length == 10 && data[1] == 0x37 {
            return match data[2] {
                0x07 | 0x08 => Ok(DeviceFamily::I7260),
                0x0b | 0x0c | 0x11..=0x14 => Ok(DeviceFamily::I8260),
                _ => Err(Error::Unsupported),
            };
        }
        let mut version = VersionTlv::default();
        iwmbt_fw::parse_tlv(&data[..length], &mut version).map_err(|_| Error::InvalidLength)?;
        let hw_platform = ((version.cnvi_bt >> 8) & 0xff) as u8;
        let hw_variant = ((version.cnvi_bt >> 16) & 0x3f) as u8;
        if hw_platform != 0x37 {
            return Err(Error::Unsupported);
        }
        Ok(if hw_variant < 0x17 {
            DeviceFamily::I8260
        } else {
            DeviceFamily::I9260
        })
    }
    /// Execute an Intel firmware HCI command/event stream and compare each
    /// returned event payload with the image's expected bytes.
    // upstream: iwmbt_hw.c iwmbt_patch_fwfile()
    pub fn run_intel_patch(&mut self, image: &[u8]) -> Result<bool, Error> {
        if !self.up {
            return Err(Error::NotUp);
        }
        let (commands, activate) =
            iwmbt_fw::parse_patch(image).map_err(|_| Error::InvalidLength)?;
        let mut command = [0u8; 258];
        let mut event = [0u8; 260];
        for record in commands {
            command[0..2].copy_from_slice(&record.opcode.to_le_bytes());
            command[2] = record.parameters.len() as u8;
            command[3..3 + record.parameters.len()].copy_from_slice(record.parameters);
            let command_length = 3 + record.parameters.len();
            if let Err(error) = self.transport.control_command(&command[..command_length]) {
                self.stats.err_tx = self.stats.err_tx.saturating_add(1);
                return Err(error);
            }
            self.stats.cmd_tx = self.stats.cmd_tx.saturating_add(1);
            self.stats.byte_tx = self.stats.byte_tx.saturating_add(command_length as u32);
            self.queue_monitor(2, &command[..command_length]);
            for (expected_code, expected) in record.expected_events {
                let length = match self.transport.read_interrupt_event(&mut event) {
                    Ok(length) => length,
                    Err(error) => {
                        self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                        return Err(error);
                    }
                };
                if length > event.len() {
                    self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                    return Err(Error::InvalidLength);
                }
                let received = match Packet::parse(PacketType::Event, &event[..length]) {
                    Ok(received) => received,
                    Err(error) => {
                        self.stats.err_rx = self.stats.err_rx.saturating_add(1);
                        return Err(error);
                    }
                };
                self.stats.evt_rx = self.stats.evt_rx.saturating_add(1);
                self.stats.byte_rx = self.stats.byte_rx.saturating_add(length as u32);
                self.queue_monitor(3, &event[..length]);
                if received.payload[0] != expected_code || received.payload[2..] != *expected {
                    return Err(Error::InvalidLength);
                }
            }
        }
        Ok(activate)
    }
    /// Send one proprietary Intel bulk firmware fragment and drain its bulk
    /// response, preserving the upstream 0xfc09 command format.
    // upstream: iwmbt_hw.c iwmbt_send_fragment()
    fn send_intel_fragment(&mut self, fragment_type: u8, data: &[u8]) -> Result<(), Error> {
        if data.len() > 0xfc {
            return Err(Error::InvalidLength);
        }
        let mut command = [0u8; 256];
        command[0..2].copy_from_slice(&0xfc09u16.to_le_bytes());
        command[2] = (data.len() + 1) as u8;
        command[3] = fragment_type;
        command[4..4 + data.len()].copy_from_slice(data);
        self.transport.bulk_acl_out(&command[..4 + data.len()])?;
        let mut response = [0u8; 256];
        self.transport.read_bulk_acl(&mut response)?;
        Ok(())
    }
    /// Transfer the RSA secure-boot header segments.
    // upstream: iwmbt_hw.c iwmbt_load_rsa_header()
    pub fn intel_load_rsa_header(&mut self, firmware: &[u8]) -> Result<(), Error> {
        if firmware.len() < 644 {
            return Err(Error::InvalidLength);
        }
        self.send_intel_fragment(0x00, &firmware[0..0x80])?;
        self.send_intel_fragment(0x03, &firmware[0x80..0x100])?;
        self.send_intel_fragment(0x03, &firmware[0x100..0x180])?;
        self.send_intel_fragment(0x02, &firmware[0x184..0x204])?;
        self.send_intel_fragment(0x02, &firmware[0x204..0x284])?;
        Ok(())
    }
    /// Transfer the ECDSA secure-boot header segments at the upstream RSA
    /// header offset.
    // upstream: iwmbt_hw.c iwmbt_load_ecdsa_header()
    pub fn intel_load_ecdsa_header(&mut self, firmware: &[u8]) -> Result<(), Error> {
        const OFFSET: usize = 644;
        if firmware.len() < OFFSET + 0x140 {
            return Err(Error::InvalidLength);
        }
        self.send_intel_fragment(0x00, &firmware[OFFSET..OFFSET + 0x80])?;
        self.send_intel_fragment(0x03, &firmware[OFFSET + 0x80..OFFSET + 0xe0])?;
        self.send_intel_fragment(0x02, &firmware[OFFSET + 0xe0..OFFSET + 0x140])?;
        Ok(())
    }
    /// Transfer firmware HCI commands in 0xfc-byte/4-byte-aligned chunks and
    /// wait for the Intel vendor download-complete event.
    // upstream: iwmbt_hw.c iwmbt_load_fwfile()
    pub fn intel_load_firmware(&mut self, firmware: &[u8], offset: usize) -> Result<u32, Error> {
        if offset > firmware.len() {
            return Err(Error::InvalidLength);
        }
        let mut sent = offset;
        let mut ready = 0usize;
        let mut boot_param = 0u32;
        while firmware.len().saturating_sub(sent + ready) >= 3 {
            let command = &firmware[sent + ready..];
            let opcode = u16::from_le_bytes([command[0], command[1]]);
            let length = usize::from(command[2]);
            if command.len() < 3 + length {
                return Err(Error::InvalidLength);
            }
            if opcode == 0xfc0e {
                if length < 4 {
                    return Err(Error::InvalidLength);
                }
                boot_param =
                    u32::from_le_bytes(command[3..7].try_into().map_err(|_| Error::InvalidLength)?);
            }
            ready += 3 + length;
            while ready >= 0xfc {
                self.send_intel_fragment(0x01, &firmware[sent..sent + 0xfc])?;
                sent += 0xfc;
                ready -= 0xfc;
            }
            if ready > 0 && ready.is_multiple_of(4) {
                self.send_intel_fragment(0x01, &firmware[sent..sent + ready])?;
                sent += ready;
                ready = 0;
            }
        }
        let mut event = [0u8; 16];
        let length = self.transport.read_interrupt_event(&mut event)?;
        if length < 3 || event[0] != 0xff || event[2] != 0x06 {
            return Err(Error::Unsupported);
        }
        Ok(boot_param)
    }
    /// Validate the Intel CSS headers, stream RSA/ECDSA segments, then upload
    /// the remaining command buffer. Returns the Intel reset boot parameter.
    // upstream: main.c iwmbt_init_firmware()
    pub fn intel_init_firmware(
        &mut self,
        firmware: &[u8],
        hw_variant: u8,
        sbe_type: u8,
    ) -> Result<u32, Error> {
        let header_len = if hw_variant <= 0x14 {
            if firmware.len() < 644 || read_le32(firmware, 8)? != 0x0001_0000 || sbe_type != 0 {
                return Err(Error::InvalidLength);
            }
            644
        } else if hw_variant >= 0x17 {
            if firmware.len() < 964
                || firmware[644] != 0x06
                || read_le32(firmware, 652)? != 0x0002_0000
            {
                return Err(Error::InvalidLength);
            }
            964
        } else {
            return Err(Error::Unsupported);
        };
        match sbe_type {
            0 => self.intel_load_rsa_header(firmware)?,
            1 => self.intel_load_ecdsa_header(firmware)?,
            _ => return Err(Error::Unsupported),
        }
        self.intel_load_firmware(firmware, header_len)
    }
    /// Standard HCI Reset command.
    // upstream: iwmbt_hw.c iwmbt_bt_reset()
    pub fn intel_bt_reset(&mut self) -> Result<(), Error> {
        self.command_complete(&[0x03, 0x0c, 0], &mut [0u8; 16])?;
        Ok(())
    }
    /// Enter Intel manufacturer mode.
    // upstream: iwmbt_hw.c iwmbt_enter_manufacturer()
    pub fn intel_enter_manufacturer(&mut self) -> Result<(), Error> {
        self.command_complete(&[0x11, 0xfc, 2, 1, 0], &mut [0u8; 16])?;
        Ok(())
    }
    /// Exit Intel manufacturer mode with the selected reset/patch policy.
    // upstream: iwmbt_hw.c iwmbt_exit_manufacturer()
    pub fn intel_exit_manufacturer(&mut self, mode: u8) -> Result<(), Error> {
        if mode > 2 {
            return Err(Error::InvalidLength);
        }
        self.command_complete(&[0x11, 0xfc, 2, 0, mode], &mut [0u8; 16])?;
        Ok(())
    }
    /// Intel vendor reset followed by the vendor-specific completion event.
    // upstream: iwmbt_hw.c iwmbt_intel_reset()
    pub fn intel_reset(&mut self, boot_param: u32) -> Result<(), Error> {
        let mut command = [0x01, 0xfc, 8, 0, 0, 0, 1, 0, 0, 0, 0];
        command[7..11].copy_from_slice(&boot_param.to_le_bytes());
        let mut event = [0u8; 16];
        let length = self.command_complete(&command, &mut event)?;
        if length < 3 || event[0] != 0xff || event[2] != 0x02 {
            return Err(Error::Unsupported);
        }
        Ok(())
    }
    /// Send Intel Write DDC commands, each prefixed by its one-byte length.
    // upstream: iwmbt_hw.c iwmbt_load_ddc()
    pub fn intel_load_ddc(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = usize::from(data[offset]);
            if chunk == 0
                || offset
                    .checked_add(chunk + 1)
                    .is_none_or(|end| end > data.len())
                || chunk > 254
            {
                return Err(Error::InvalidLength);
            }
            let mut command = [0u8; 258];
            command[0..2].copy_from_slice(&0xfc8bu16.to_le_bytes());
            command[2] = (chunk + 1) as u8;
            command[3..4 + chunk].copy_from_slice(&data[offset..offset + chunk + 1]);
            self.command_complete(&command[..4 + chunk], &mut [0u8; 16])?;
            offset += chunk + 1;
        }
        Ok(())
    }
    /// Program the Intel HCI event mask.
    // upstream: iwmbt_hw.c iwmbt_set_event_mask()
    pub fn intel_set_event_mask(&mut self) -> Result<(), Error> {
        self.command_complete(
            &[0x52, 0xfc, 8, 0x87, 0x0c, 0, 0, 0, 0, 0, 0],
            &mut [0u8; 16],
        )?;
        Ok(())
    }
    pub fn into_transport(self) -> T {
        self.transport
    }
}

/// Device operations against an absent controller use ENODEV semantics.
pub fn absent_device_ioctl(_command: u32) -> Result<(), Error> {
    Err(Error::NoDevice)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoctlOutcome {
    EmptyDeviceList,
}

/// Linux HCI_GETDEVLIST succeeds with zero entries on a machine with no HCI
/// controller; ioctls that name an individual device return ENODEV.
pub fn no_device_ioctl(command: u32) -> Result<IoctlOutcome, Error> {
    if command == HCIGETDEVLIST {
        Ok(IoctlOutcome::EmptyDeviceList)
    } else {
        Err(Error::NoDevice)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, vec, vec::Vec};

    use super::*;
    struct Fake {
        stopped: bool,
        commands: usize,
        acl: usize,
    }

    struct FirmwareFake {
        events: VecDeque<Vec<u8>>,
        bulk: Vec<Vec<u8>>,
    }
    struct RxFake {
        packet: Option<(PacketType, Vec<u8>)>,
    }
    impl UsbTransport for RxFake {
        fn control_command(&mut self, _: &[u8]) -> Result<(), Error> {
            Ok(())
        }
        fn bulk_acl_out(&mut self, _: &[u8]) -> Result<(), Error> {
            Ok(())
        }
        fn read_interrupt_event(&mut self, _: &mut [u8]) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        fn read_bulk_acl(&mut self, _: &mut [u8]) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        fn read_packet(
            &mut self,
            out: &mut [u8],
            _: bool,
        ) -> Result<Option<(PacketType, usize)>, Error> {
            let Some((kind, packet)) = self.packet.take() else {
                return Ok(None);
            };
            if packet.len() > out.len() {
                return Err(Error::InvalidLength);
            }
            out[..packet.len()].copy_from_slice(&packet);
            Ok(Some((kind, packet.len())))
        }
        fn stop(&mut self) {}
    }
    impl UsbTransport for FirmwareFake {
        fn control_command(&mut self, _: &[u8]) -> Result<(), Error> {
            Ok(())
        }
        fn bulk_acl_out(&mut self, packet: &[u8]) -> Result<(), Error> {
            self.bulk.push(packet.to_vec());
            Ok(())
        }
        fn read_interrupt_event(&mut self, out: &mut [u8]) -> Result<usize, Error> {
            let event = self.events.pop_front().ok_or(Error::NoDevice)?;
            if event.len() > out.len() {
                return Err(Error::InvalidLength);
            }
            out[..event.len()].copy_from_slice(&event);
            Ok(event.len())
        }
        fn read_bulk_acl(&mut self, _: &mut [u8]) -> Result<usize, Error> {
            Ok(0)
        }
        fn stop(&mut self) {}
    }
    impl UsbTransport for Fake {
        fn control_command(&mut self, _: &[u8]) -> Result<(), Error> {
            self.commands += 1;
            Ok(())
        }
        fn bulk_acl_out(&mut self, _: &[u8]) -> Result<(), Error> {
            self.acl += 1;
            Ok(())
        }
        fn read_interrupt_event(&mut self, out: &mut [u8]) -> Result<usize, Error> {
            let event = [0x0e, 4, 1, 1, 0x10, 0];
            if out.len() < event.len() {
                return Err(Error::InvalidLength);
            }
            out[..event.len()].copy_from_slice(&event);
            Ok(event.len())
        }
        fn read_bulk_acl(&mut self, _: &mut [u8]) -> Result<usize, Error> {
            Ok(0)
        }
        fn stop(&mut self) {
            self.stopped = true
        }
    }
    #[test]
    fn validates_command_and_event_lengths() {
        assert!(Packet::parse(PacketType::Command, &[1, 0, 1, 0xaa]).is_ok());
        assert_eq!(
            Packet::parse(PacketType::Command, &[1, 0, 2, 0xaa]),
            Err(Error::InvalidLength)
        );
        assert!(Packet::parse(PacketType::Event, &[0x0e, 0]).is_ok());
    }
    #[test]
    fn channels_and_usb_paths_are_owned() {
        let mut a = Adapter::new(
            Fake {
                stopped: false,
                commands: 0,
                acl: 0,
            },
            0,
        );
        assert_eq!(a.open(Channel::User), Ok(()));
        assert_eq!(a.open(Channel::User), Err(Error::Busy));
        a.set_up(true).unwrap();
        assert_eq!(
            a.submit(Channel::User, PacketType::Command, &[1, 0, 2, 0xaa]),
            Err(Error::InvalidLength)
        );
        assert_eq!(a.receive_event(&[0x0e, 1]), Err(Error::InvalidLength));
        a.submit(Channel::User, PacketType::Command, &[1, 0, 1, 0xaa])
            .unwrap();
        a.receive_event(&[0x0e, 0]).unwrap();
        assert!(a.pop_monitor().is_none());
        a.open(Channel::Monitor).unwrap();
        a.receive_event(&[0x0e, 0]).unwrap();
        assert_eq!(&a.pop_monitor().unwrap()[..2], &3u16.to_le_bytes());
        assert_eq!(
            a.statistics(),
            Statistics {
                err_rx: 1,
                err_tx: 1,
                cmd_tx: 1,
                evt_rx: 2,
                byte_rx: 4,
                byte_tx: 4,
                ..Statistics::default()
            }
        );
        a.set_up(false).unwrap();
        assert!(a.transport.stopped);
        assert_eq!(a.transport.commands, 1);
    }
    #[test]
    fn user_channel_excludes_raw_sockets_but_allows_monitoring() {
        let mut adapter = Adapter::new(
            Fake {
                stopped: false,
                commands: 0,
                acl: 0,
            },
            0,
        );
        adapter.open(Channel::Raw).unwrap();
        assert_eq!(adapter.open(Channel::User), Err(Error::Busy));
        adapter.close(Channel::Raw);
        adapter.open(Channel::User).unwrap();
        assert_eq!(adapter.open(Channel::Raw), Err(Error::Busy));
        assert_eq!(adapter.open(Channel::Monitor), Ok(()));
    }
    #[test]
    fn monitor_preserves_maximum_sized_acl_frames() {
        let mut adapter = Adapter::new(
            Fake {
                stopped: false,
                commands: 0,
                acl: 0,
            },
            7,
        );
        adapter.open(Channel::User).unwrap();
        adapter.open(Channel::Monitor).unwrap();
        adapter.set_up(true).unwrap();
        let mut acl = vec![0u8; 1028];
        acl[0] = 1;
        acl[2] = 0;
        acl[3] = 4;
        acl[1027] = 0xaa;
        adapter
            .submit(Channel::User, PacketType::Acl, &acl)
            .unwrap();
        let frame = adapter.pop_monitor().unwrap();
        assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), 4);
        assert_eq!(u16::from_le_bytes([frame[2], frame[3]]), 7);
        assert_eq!(u16::from_le_bytes([frame[4], frame[5]]), 1028);
        assert_eq!(frame.len(), 1034);
        assert_eq!(frame[1033], 0xaa);
    }
    #[test]
    fn received_acl_is_counted_and_captured_for_monitors() {
        let mut adapter = Adapter::new(
            RxFake {
                packet: Some((PacketType::Acl, vec![1, 0, 2, 0, 0xaa, 0xbb])),
            },
            3,
        );
        adapter.open(Channel::Monitor).unwrap();
        adapter.set_up(true).unwrap();
        let mut packet = [0u8; 16];
        assert_eq!(
            adapter.receive_packet(&mut packet, true),
            Ok((PacketType::Acl, 6))
        );
        let monitor = adapter.pop_monitor().unwrap();
        assert_eq!(u16::from_le_bytes([monitor[0], monitor[1]]), 5);
        assert_eq!(u16::from_le_bytes([monitor[2], monitor[3]]), 3);
        assert_eq!(&monitor[6..], &[1, 0, 2, 0, 0xaa, 0xbb]);
        assert_eq!(adapter.statistics().acl_rx, 1);
    }
    #[test]
    fn empty_controller_reports_no_device() {
        assert_eq!(absent_device_ioctl(HCIDEVUP), Err(Error::NoDevice));
        assert_eq!(AF_BLUETOOTH, 31);
        assert_eq!(BTPROTO_HCI, 1);
    }

    #[test]
    fn intel_firmware_tlv_and_name_selection() {
        let mut v = VersionTlv::default();
        parse_tlv(
            &[
                0, 0x10, 4, 0x12, 0x34, 0x56, 0x78, 0x11, 4, 0xef, 0xcd, 0xab, 0x90,
            ],
            &mut v,
        )
        .unwrap();
        assert_eq!(
            get_fwname_tlv(&v, "/lib/firmware/intel", "sfi"),
            "/lib/firmware/intel/ibt-2841-f0de.sfi"
        );
        assert_eq!(
            get_fwname(
                &Version {
                    hw_variant: 0x0c,
                    ..Version::default()
                },
                None,
                "intel",
                "sfi"
            ),
            None
        );
        assert_eq!(
            get_fwname(
                &Version {
                    hw_variant: 0x0c,
                    ..Version::default()
                },
                Some(&BootParams {
                    dev_revid: 42,
                    ..BootParams::default()
                }),
                "intel",
                "sfi"
            )
            .unwrap(),
            "intel/ibt-12-42.sfi"
        );
        assert_eq!(
            get_fwname_fallback(
                &Version {
                    hw_platform: 0x37,
                    hw_variant: 7,
                    ..Version::default()
                },
                "intel",
                "bseq"
            )
            .as_deref(),
            Some("intel/ibt-hw-37.7.bseq")
        );
    }

    #[test]
    fn intel_firmware_tlv_rejects_short_and_bad_status() {
        assert_eq!(
            parse_tlv(&[], &mut VersionTlv::default()),
            Err(FirmwareError::TruncatedTlv)
        );
        assert_eq!(
            parse_tlv(&[1], &mut VersionTlv::default()),
            Err(FirmwareError::InvalidStatus)
        );
        assert_eq!(
            parse_tlv(&[0, 0x10, 4, 1], &mut VersionTlv::default()),
            Err(FirmwareError::TruncatedTlv)
        );
    }

    #[test]
    fn intel_usb_firmware_id_table_includes_n305_cnvi() {
        assert_eq!(supported_device(0x8087, 0x0033), DeviceFamily::I9260);
        assert_eq!(supported_device(0x8087, 0x0029), DeviceFamily::I8260);
        assert_eq!(supported_device(0x1234, 0x0033), DeviceFamily::Unknown);
    }

    #[test]
    fn no_controller_ioctl_results_match_hci_device_list_semantics() {
        assert_eq!(
            no_device_ioctl(HCIGETDEVLIST),
            Ok(IoctlOutcome::EmptyDeviceList)
        );
        assert_eq!(no_device_ioctl(HCIGETDEVINFO), Err(Error::NoDevice));
        assert_eq!(no_device_ioctl(HCIDEVUP), Err(Error::NoDevice));
        assert_eq!(no_device_ioctl(HCIDEVDOWN), Err(Error::NoDevice));
    }

    #[test]
    fn intel_patch_stream_parses_command_event_pairs_and_activation() {
        let (commands, activate) = parse_patch(&[1, 0x8e, 0xfc, 1, 0xaa, 2, 0x0e, 1, 0]).unwrap();
        assert!(activate);
        assert_eq!(
            commands,
            [PatchCommand {
                opcode: 0xfc8e,
                parameters: &[0xaa],
                expected_events: std::vec![(0x0e, &[0][..])]
            }]
        );
        assert_eq!(parse_patch(&[1, 0x01]), Err(FirmwareError::InvalidPatch));
    }

    #[test]
    fn intel_patch_validates_expected_event_code_and_updates_hci_stats() {
        let image = [1, 0x8e, 0xfc, 1, 0xaa, 2, 0x0e, 1, 0];
        let mut adapter = Adapter::new(
            FirmwareFake {
                events: VecDeque::from([vec![0x0e, 1, 0]]),
                bulk: Vec::new(),
            },
            0,
        );
        adapter.set_up(true).unwrap();
        assert_eq!(adapter.run_intel_patch(&image), Ok(true));
        assert_eq!(
            adapter.statistics(),
            Statistics {
                cmd_tx: 1,
                evt_rx: 1,
                byte_tx: 4,
                byte_rx: 3,
                ..Statistics::default()
            }
        );

        let mut adapter = Adapter::new(
            FirmwareFake {
                events: VecDeque::from([vec![0x0f, 1, 0]]),
                bulk: Vec::new(),
            },
            0,
        );
        adapter.set_up(true).unwrap();
        assert_eq!(adapter.run_intel_patch(&image), Err(Error::InvalidLength));
    }

    #[test]
    fn intel_hci_command_waits_for_matching_command_complete() {
        let mut adapter = Adapter::new(
            Fake {
                stopped: false,
                commands: 0,
                acl: 0,
            },
            0,
        );
        adapter.open(Channel::User).unwrap();
        adapter.set_up(true).unwrap();
        let mut event = [0; 16];
        assert_eq!(adapter.command_complete(&[1, 0x10, 0], &mut event), Ok(6));
        assert_eq!(&event[..6], &[0x0e, 4, 1, 1, 0x10, 0]);
    }
    #[test]
    fn hci_capabilities_are_decoded_from_standard_command_replies() {
        let mut adapter = Adapter::new(
            FirmwareFake {
                events: VecDeque::from([
                    vec![0x0e, 10, 1, 0x09, 0x10, 0, 1, 2, 3, 4, 5, 6],
                    vec![0x0e, 12, 1, 0x03, 0x10, 0, 1, 2, 3, 4, 5, 6, 7, 8],
                    vec![0x0e, 11, 1, 0x05, 0x10, 0, 0x40, 0, 0x20, 2, 0, 1, 0],
                ]),
                bulk: Vec::new(),
            },
            0,
        );
        adapter.set_up(true).unwrap();
        assert_eq!(
            adapter.read_capabilities(),
            Ok(HciCapabilities {
                address: [1, 2, 3, 4, 5, 6],
                features: [1, 2, 3, 4, 5, 6, 7, 8],
                acl_mtu: 64,
                acl_packets: 2,
                sco_mtu: 32,
                sco_packets: 1,
            })
        );
    }

    #[test]
    fn intel_version_event_is_decoded_from_command_complete_parameters() {
        let event = [0x0e, 13, 1, 0x05, 0xfc, 0, 1, 0x12, 3, 0x23, 4, 5, 6, 7, 8];
        let version = parse_version_event(&event).unwrap();
        assert_eq!(version.hw_variant, 0x12);
        assert_eq!(version.fw_variant, 0x23);
        assert_eq!(version.fw_patch_num, 8);
        assert_eq!(
            parse_version_event(&[0; 15]),
            Err(FirmwareError::InvalidVersionEvent)
        );
    }

    #[test]
    fn intel_rsa_firmware_header_and_aligned_command_chunks_are_transferred() {
        let mut firmware = std::vec![0u8; 644];
        firmware[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        firmware.extend_from_slice(&[0x0e, 0xfc, 5, 1, 2, 3, 4, 0x55]);
        let transport = FirmwareFake {
            events: VecDeque::from([std::vec![0xff, 1, 6]]),
            bulk: Vec::new(),
        };
        let mut adapter = Adapter::new(transport, 0);
        adapter.set_up(true).unwrap();
        assert_eq!(
            adapter.intel_init_firmware(&firmware, 0x12, 0),
            Ok(0x0403_0201)
        );
        assert_eq!(adapter.transport.bulk.len(), 6);
        assert_eq!(&adapter.transport.bulk[5][..4], &[0x09, 0xfc, 9, 1]);
        assert_eq!(
            &adapter.transport.bulk[5][4..],
            &[0x0e, 0xfc, 5, 1, 2, 3, 4, 0x55]
        );
    }

    #[test]
    fn intel_ecdsa_css_header_is_transferred_at_rsa_offset() {
        let mut firmware = std::vec![0u8; 964];
        firmware[644] = 0x06;
        firmware[652..656].copy_from_slice(&0x0002_0000u32.to_le_bytes());
        let transport = FirmwareFake {
            events: VecDeque::from([std::vec![0xff, 1, 6]]),
            bulk: Vec::new(),
        };
        let mut adapter = Adapter::new(transport, 0);
        adapter.set_up(true).unwrap();
        assert_eq!(adapter.intel_init_firmware(&firmware, 0x17, 1), Ok(0));
        assert_eq!(adapter.transport.bulk.len(), 3);
        assert_eq!(&adapter.transport.bulk[0][..4], &[0x09, 0xfc, 129, 0]);
    }

    #[test]
    fn intel_boot_params_decode_packed_revision_and_limits() {
        let mut bytes = [0u8; 23];
        bytes[4..6].copy_from_slice(&0x1234u16.to_le_bytes());
        bytes[21] = 7;
        let params = BootParams::parse(&bytes).unwrap();
        assert_eq!(params.dev_revid, 0x1234);
        assert_eq!(params.limited_cce, 7);
        assert_eq!(
            BootParams::parse(&bytes[..22]),
            Err(FirmwareError::InvalidVersionEvent)
        );
    }
}
