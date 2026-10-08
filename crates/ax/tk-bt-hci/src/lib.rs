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

use alloc::collections::VecDeque;

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

/// Small HCI device/channel owner independent of AF_BLUETOOTH socket plumbing.
pub struct Adapter<T> {
    transport: T,
    index: u16,
    up: bool,
    user_owner: bool,
    raw_users: u16,
    monitor_users: u16,
    monitor: VecDeque<[u8; 260]>,
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
        }
    }
    pub fn index(&self) -> u16 {
        self.index
    }
    pub fn is_up(&self) -> bool {
        self.up
    }
    pub fn open(&mut self, channel: Channel) -> Result<(), Error> {
        match channel {
            Channel::Raw => self.raw_users = self.raw_users.saturating_add(1),
            Channel::Monitor => self.monitor_users = self.monitor_users.saturating_add(1),
            Channel::User => {
                if self.user_owner {
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
        Packet::parse(kind, bytes)?;
        match kind {
            PacketType::Command => self.transport.control_command(bytes),
            PacketType::Acl => self.transport.bulk_acl_out(bytes),
            _ => Err(Error::Unsupported),
        }
    }
    /// HCI event capture is copied to monitor observers with a leading packet
    /// type byte. This bounded queue drops the oldest packet on overflow.
    pub fn receive_event(&mut self, bytes: &[u8]) -> Result<(), Error> {
        Packet::parse(PacketType::Event, bytes)?;
        if self.monitor_users != 0 {
            let mut frame = [0u8; 260];
            let len = bytes.len().min(259);
            frame[0] = 4;
            frame[1..=len].copy_from_slice(&bytes[..len]);
            if self.monitor.len() == 64 {
                self.monitor.pop_front();
            }
            self.monitor.push_back(frame);
        }
        Ok(())
    }
    pub fn pop_monitor(&mut self) -> Option<[u8; 260]> {
        self.monitor.pop_front()
    }
    pub fn into_transport(self) -> T {
        self.transport
    }
}

/// Device operations against an absent controller use ENODEV semantics.
pub fn absent_device_ioctl(_command: u32) -> Result<(), Error> {
    Err(Error::NoDevice)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        stopped: bool,
        commands: usize,
        acl: usize,
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
        fn read_interrupt_event(&mut self, _: &mut [u8]) -> Result<usize, Error> {
            Ok(0)
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
        a.submit(Channel::User, PacketType::Command, &[1, 0, 1, 0xaa])
            .unwrap();
        a.receive_event(&[0x0e, 0]).unwrap();
        assert!(a.pop_monitor().is_none());
        a.open(Channel::Monitor).unwrap();
        a.receive_event(&[0x0e, 0]).unwrap();
        assert_eq!(a.pop_monitor().unwrap()[0], 4);
        a.set_up(false).unwrap();
        assert!(a.transport.stopped);
        assert_eq!(a.transport.commands, 1);
    }
    #[test]
    fn empty_controller_reports_no_device() {
        assert_eq!(absent_device_ioctl(HCIDEVUP), Err(Error::NoDevice));
        assert_eq!(AF_BLUETOOTH, 31);
        assert_eq!(BTPROTO_HCI, 1);
    }
}
