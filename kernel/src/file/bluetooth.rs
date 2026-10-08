//! Linux AF_BLUETOOTH HCI socket shell for device discovery and monitor setup.
//! Protocol framing follows the Bluetooth Core and Linux UAPI; no GPL net stack
//! implementation is copied here.
use alloc::{borrow::Cow, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, PollRegistration, PollSet, Pollable};
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::net::{SOCK_RAW, sockaddr};
use spin::Mutex as SpinMutex;

use super::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, PseudoInode, try_pseudo_inode_path};
use crate::mm::UserMemoryCapability;

pub const AF_BLUETOOTH: u32 = 31;
pub const BTPROTO_HCI: u32 = 1;
const HCIDEVUP: u32 = 0x4004_48c9;
const HCIDEVDOWN: u32 = 0x4004_48ca;
const HCIGETDEVLIST: u32 = 0x8004_48d2;
const HCIGETDEVINFO: u32 = 0x8004_48d3;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_MONITOR: u16 = 2;
const HCI_CHANNEL_CONTROL: u16 = 3;
const MAX_HCI_PACKET: usize = 65_536;
const HCI_UP: u32 = 1 << 0;

#[cfg(feature = "input")]
type UsbAdapter = Arc<SpinMutex<axdriver::UsbBluetoothHci>>;
#[cfg(not(feature = "input"))]
type UsbAdapter = ();

struct BoundChannel {
    channel: u16,
    adapter: Option<UsbAdapter>,
}

#[cfg(feature = "input")]
fn map_transport_error(error: axdriver::BluetoothError) -> AxError {
    match error {
        axdriver::BluetoothError::NoDevice => LinuxError::ENODEV.into(),
        axdriver::BluetoothError::Again => LinuxError::EAGAIN.into(),
        axdriver::BluetoothError::Busy => LinuxError::EBUSY.into(),
        axdriver::BluetoothError::NotUp => LinuxError::ENETDOWN.into(),
        axdriver::BluetoothError::Truncated | axdriver::BluetoothError::InvalidLength => {
            AxError::InvalidInput
        }
        axdriver::BluetoothError::Unsupported => LinuxError::EOPNOTSUPP.into(),
    }
}

#[cfg(feature = "input")]
fn usb_adapter(index: u16) -> Option<UsbAdapter> {
    axdriver::bluetooth_devices()
        .into_iter()
        .find(|adapter| adapter.lock().index() == index)
}

#[cfg(feature = "input")]
fn monitor_adapter(index: u16) -> Option<UsbAdapter> {
    if index == HCI_DEV_NONE {
        // A globally bound monitor socket observes the first controller; the
        // target platform has one CNVi HCI interface.
        axdriver::bluetooth_devices().into_iter().next()
    } else {
        usb_adapter(index)
    }
}

fn encode_dev_req(index: u16, is_up: bool) -> [u8; 8] {
    // Linux `struct hci_dev_req` has u16 dev_id followed by aligned u32
    // dev_opt; preserve the two padding bytes in its native ABI layout.
    let mut request = [0u8; 8];
    request[..2].copy_from_slice(&index.to_ne_bytes());
    request[4..].copy_from_slice(&(if is_up { HCI_UP } else { 0 }).to_ne_bytes());
    request
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct SockaddrHci {
    family: u16,
    device: u16,
    channel: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HciDevStats {
    err_rx: u32,
    err_tx: u32,
    cmd_tx: u32,
    evt_rx: u32,
    acl_tx: u32,
    acl_rx: u32,
    sco_tx: u32,
    sco_rx: u32,
    byte_rx: u32,
    byte_tx: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HciDevInfo {
    dev_id: u16,
    name: [u8; 8],
    bdaddr: [u8; 6],
    flags: u32,
    device_type: u8,
    features: [u8; 8],
    reserved: [u8; 3],
    packet_type: u32,
    link_policy: u32,
    link_mode: u32,
    acl_mtu: u16,
    acl_packets: u16,
    sco_mtu: u16,
    sco_packets: u16,
    stats: HciDevStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_channel_binds_without_hardware_but_device_channels_fail() {
        let socket = HciSocket::new();
        assert!(
            socket
                .bind(SockaddrHci {
                    family: AF_BLUETOOTH as u16,
                    device: HCI_DEV_NONE,
                    channel: HCI_CHANNEL_MONITOR
                })
                .is_ok()
        );
        assert!(
            HciSocket::new()
                .bind(SockaddrHci {
                    family: AF_BLUETOOTH as u16,
                    device: 0,
                    channel: HCI_CHANNEL_MONITOR,
                })
                .is_ok()
        );
        assert_eq!(
            socket.bind(SockaddrHci {
                family: AF_BLUETOOTH as u16,
                device: 0,
                channel: 0
            }),
            Err(LinuxError::ENODEV.into())
        );
        assert_eq!(
            HciSocket::validate_socket_type(SOCK_RAW as u32, BTPROTO_HCI),
            Ok(())
        );
    }

    #[test]
    fn hci_dev_list_records_include_aligned_flags() {
        assert_eq!(
            encode_dev_req(0x1234, false),
            [0x34, 0x12, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encode_dev_req(0x1234, true), [0x34, 0x12, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn management_read_version_and_empty_index_list_match_wire_abi() {
        let version = management_response(&[1, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(&version[..9], &[1, 0, 0xff, 0xff, 6, 0, 1, 0, 0]);
        assert_eq!(&version[9..], &[1, 0, 0]);

        let indices = management_response(&[3, 0, 0xff, 0xff, 0, 0]).unwrap();
        assert_eq!(&indices[..9], &[1, 0, 0xff, 0xff, 5, 0, 3, 0, 0]);
        assert_eq!(&indices[9..], &[0, 0]);
    }

    #[test]
    fn management_read_info_returns_invalid_index_without_device() {
        let info = management_response(&[4, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(&info[..9], &[1, 0, 0, 0, 3, 0, 4, 0, 0x11]);
        assert_eq!(info.len(), 9);
    }

    #[test]
    fn management_commands_advertise_only_implemented_operations() {
        let commands = management_response(&[2, 0, 0xff, 0xff, 0, 0]).unwrap();
        assert_eq!(u16::from_le_bytes([commands[9], commands[10]]), 3);
        assert_eq!(u16::from_le_bytes([commands[11], commands[12]]), 0);
        assert_eq!(commands.len(), 9 + 4 + 2 * 3);
        assert_eq!(&commands[13..19], &[3, 0, 4, 0, 5, 0]);
    }
}

pub struct HciSocket {
    inode: PseudoInode,
    nonblocking: AtomicBool,
    binding: SpinMutex<Option<BoundChannel>>,
    control_rx: SpinMutex<VecDeque<Vec<u8>>>,
    control_readiness: Arc<PollSet<32>>,
}

impl HciSocket {
    pub(crate) fn new() -> Self {
        Self {
            inode: PseudoInode::socket(),
            nonblocking: AtomicBool::new(false),
            binding: SpinMutex::new(None),
            control_rx: SpinMutex::new(VecDeque::new()),
            control_readiness: Arc::new(PollSet::new()),
        }
    }

    pub(crate) fn validate_socket_type(ty: u32, protocol: u32) -> AxResult<()> {
        if ty != SOCK_RAW as u32 {
            return Err(LinuxError::ESOCKTNOSUPPORT.into());
        }
        if protocol != BTPROTO_HCI {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        Ok(())
    }

    pub(crate) fn bind(&self, address: SockaddrHci) -> AxResult<()> {
        if address.family as u32 != AF_BLUETOOTH {
            return Err(AxError::InvalidInput);
        }
        // HCI monitor is global and remains bindable without an adapter. The
        // HCI_DEV_NONE spelling is accepted as on Linux; raw/user need a device.
        if !matches!(
            address.channel,
            0 | 1 | HCI_CHANNEL_MONITOR | HCI_CHANNEL_CONTROL
        ) {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        let mut current = self.binding.lock();
        if current.is_some() {
            return Err(LinuxError::EINVAL.into());
        }
        let adapter = if address.channel == HCI_CHANNEL_CONTROL {
            if address.device != HCI_DEV_NONE {
                return Err(AxError::InvalidInput);
            }
            None
        } else if address.channel == HCI_CHANNEL_MONITOR {
            #[cfg(feature = "input")]
            let adapter = monitor_adapter(address.device);
            #[cfg(not(feature = "input"))]
            let adapter: Option<UsbAdapter> = None;
            #[cfg(feature = "input")]
            if let Some(adapter) = &adapter {
                adapter
                    .lock()
                    .open_channel(address.channel)
                    .map_err(map_transport_error)?;
            }
            adapter
        } else {
            if address.device == HCI_DEV_NONE {
                return Err(AxError::InvalidInput);
            }
            #[cfg(feature = "input")]
            let adapter = usb_adapter(address.device).ok_or(LinuxError::ENODEV)?;
            #[cfg(not(feature = "input"))]
            return Err(LinuxError::ENODEV.into());
            #[cfg(feature = "input")]
            {
                adapter
                    .lock()
                    .open_channel(address.channel)
                    .map_err(map_transport_error)?;
                Some(adapter)
            }
        };
        *current = Some(BoundChannel {
            channel: address.channel,
            adapter,
        });
        Ok(())
    }

    fn close_binding(&self) {
        if let Some(binding) = self.binding.lock().take() {
            #[cfg(feature = "input")]
            if let Some(adapter) = binding.adapter {
                adapter.lock().close_channel(binding.channel);
            }
        }
    }

    fn device_operation(&self, index: u16, up: bool) -> AxResult<()> {
        #[cfg(feature = "input")]
        {
            let adapter = usb_adapter(index).ok_or(LinuxError::ENODEV)?;
            adapter
                .lock()
                .set_device_up(up)
                .map_err(map_transport_error)
        }
        #[cfg(not(feature = "input"))]
        {
            let _ = (index, up);
            Err(LinuxError::ENODEV.into())
        }
    }

    fn device_info(&self, index: u16) -> AxResult<HciDevInfo> {
        #[cfg(feature = "input")]
        {
            let adapter = usb_adapter(index).ok_or(LinuxError::ENODEV)?;
            let adapter = adapter.lock();
            let capabilities = adapter.capabilities();
            let mut info = HciDevInfo::zeroed();
            info.dev_id = index;
            info.bdaddr = adapter.address();
            info.name[..3].copy_from_slice(b"hci");
            let mut value = index;
            let mut digits = [0u8; 5];
            let mut count = 0;
            if value == 0 {
                digits[0] = b'0';
                count = 1;
            }
            while value != 0 {
                digits[count] = b'0' + (value % 10) as u8;
                value /= 10;
                count += 1;
            }
            for i in 0..count {
                info.name[3 + i] = digits[count - i - 1];
            }
            info.flags = if adapter.is_up() { 1 } else { 0 };
            info.device_type = 1; // HCI_USB
            info.features = capabilities.features;
            info.acl_mtu = capabilities.acl_mtu;
            info.acl_packets = capabilities.acl_packets;
            info.sco_mtu = capabilities.sco_mtu;
            info.sco_packets = capabilities.sco_packets;
            let stats = adapter.statistics();
            info.stats = HciDevStats {
                err_rx: stats.err_rx,
                err_tx: stats.err_tx,
                cmd_tx: stats.cmd_tx,
                evt_rx: stats.evt_rx,
                acl_tx: stats.acl_tx,
                acl_rx: stats.acl_rx,
                sco_tx: stats.sco_tx,
                sco_rx: stats.sco_rx,
                byte_rx: stats.byte_rx,
                byte_tx: stats.byte_tx,
            };
            Ok(info)
        }
        #[cfg(not(feature = "input"))]
        {
            let _ = index;
            Err(LinuxError::ENODEV.into())
        }
    }

    pub(crate) fn read_from_user(
        &self,
        capability: &UserMemoryCapability,
        address: *const sockaddr,
        len: usize,
    ) -> AxResult<SockaddrHci> {
        if len < core::mem::size_of::<SockaddrHci>() {
            return Err(AxError::InvalidInput);
        }
        capability
            .read_value(address.cast())
            .map_err(crate::mm::map_usercopy_error)
    }
}

impl Pollable for HciSocket {
    fn poll(&self) -> IoEvents {
        let binding = self.binding.lock();
        let Some(binding) = binding.as_ref() else {
            return IoEvents::WRITABLE;
        };
        let mut events = IoEvents::WRITABLE;
        if binding.channel == HCI_CHANNEL_CONTROL {
            if !self.control_rx.lock().is_empty() {
                events |= IoEvents::READABLE;
            }
            return events;
        }
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter
            && adapter.lock().receive_ready(binding.channel)
        {
            events |= IoEvents::READABLE;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if !events.intersects(IoEvents::READABLE) {
            return axpoll::PollRegistration::empty();
        }
        let binding = self.binding.lock();
        let Some(binding) = binding.as_ref() else {
            return axpoll::PollRegistration::empty();
        };
        if binding.channel == HCI_CHANNEL_CONTROL {
            return PollRegistration::single(&self.control_readiness, context.waker());
        }
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter {
            let readiness = adapter.lock().receive_readiness();
            return PollRegistration::single_owned(readiness, context.waker());
        }
        axpoll::PollRegistration::empty()
    }
}

impl FileLike for HciSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let channel = self
            .binding
            .lock()
            .as_ref()
            .map(|binding| binding.channel)
            .ok_or(LinuxError::ENODEV)?;
        if channel == HCI_CHANNEL_CONTROL {
            let packet = self
                .control_rx
                .lock()
                .pop_front()
                .ok_or(LinuxError::EAGAIN)?;
            return dst.write(&packet);
        }
        let binding = self.binding.lock();
        let binding = binding.as_ref().ok_or(LinuxError::ENODEV)?;
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter {
            let capacity = MAX_HCI_PACKET.checked_add(6).ok_or(AxError::InvalidInput)?;
            let mut packet = Vec::new();
            packet
                .try_reserve_exact(capacity)
                .map_err(|_| AxError::NoMemory)?;
            packet.resize(capacity, 0);
            let length = adapter
                .lock()
                .receive_channel_packet(
                    binding.channel,
                    &mut packet,
                    self.nonblocking.load(Ordering::Acquire),
                )
                .map_err(map_transport_error)?;
            return dst.write(&packet[..length]);
        }
        let _ = binding;
        if binding.channel == HCI_CHANNEL_MONITOR {
            return Err(LinuxError::EAGAIN.into());
        }
        Err(if self.nonblocking.load(Ordering::Acquire) {
            LinuxError::EAGAIN.into()
        } else {
            LinuxError::ENODEV.into()
        })
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let channel = self
            .binding
            .lock()
            .as_ref()
            .map(|binding| binding.channel)
            .ok_or(LinuxError::ENODEV)?;
        if channel == HCI_CHANNEL_CONTROL {
            return self.write_management_command(src);
        }
        let binding = self.binding.lock();
        let binding = binding.as_ref().ok_or(LinuxError::ENODEV)?;
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter {
            if binding.channel == HCI_CHANNEL_MONITOR {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            let length = src.remaining().min(MAX_HCI_PACKET);
            let mut packet = Vec::new();
            packet
                .try_reserve_exact(length)
                .map_err(|_| AxError::NoMemory)?;
            packet.resize(length, 0);
            let read = src.read(&mut packet)?;
            packet.truncate(read);
            adapter
                .lock()
                .send_channel_packet(binding.channel, &packet)
                .map_err(map_transport_error)?;
            return Ok(read);
        }
        let _ = binding;
        Err(LinuxError::ENODEV.into())
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            HCIGETDEVLIST => {
                let requested = context
                    .user_memory()
                    .read_value(arg as *const u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                #[cfg(feature = "input")]
                let devices = axdriver::bluetooth_devices();
                #[cfg(not(feature = "input"))]
                let devices: Vec<()> = Vec::new();
                let count = usize::from(requested).min(devices.len());
                for (slot, adapter) in devices.iter().take(count).enumerate() {
                    #[cfg(feature = "input")]
                    let (index, is_up) = {
                        let adapter = adapter.lock();
                        (adapter.index(), adapter.is_up())
                    };
                    #[cfg(not(feature = "input"))]
                    let (index, is_up) = {
                        let _ = adapter;
                        (0u16, false)
                    };
                    let request = encode_dev_req(index, is_up);
                    context
                        .user_memory()
                        .write_bytes(
                            arg.checked_add(4 + slot * request.len())
                                .ok_or(AxError::InvalidInput)?,
                            &request,
                        )
                        .map_err(crate::mm::map_usercopy_error)?;
                }
                context
                    .user_memory()
                    .write_value(arg as *mut u16, count as u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            HCIDEVUP => self.device_operation(arg as u16, true).map(|()| 0),
            HCIDEVDOWN => self.device_operation(arg as u16, false).map(|()| 0),
            HCIGETDEVINFO => {
                let index = context
                    .user_memory()
                    .read_value(arg as *const u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                let info = self.device_info(index)?;
                context
                    .user_memory()
                    .write_value(arg as *mut HciDevInfo, info)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn final_close(&self) {
        self.close_binding();
    }
}

impl Default for HciSocket {
    fn default() -> Self {
        Self::new()
    }
}

impl HciSocket {
    fn write_management_command(&self, src: &mut IoSrc) -> AxResult<usize> {
        let length = src.remaining().min(MAX_HCI_PACKET);
        if length < 6 {
            return Err(AxError::InvalidInput);
        }
        let mut packet = Vec::new();
        packet
            .try_reserve_exact(length)
            .map_err(|_| AxError::NoMemory)?;
        packet.resize(length, 0);
        let read = src.read(&mut packet)?;
        packet.truncate(read);
        let response = management_response(&packet)?;
        let mut queue = self.control_rx.lock();
        if queue.len() >= 64 {
            return Err(LinuxError::ENOBUFS.into());
        }
        queue.push_back(response);
        drop(queue);
        self.control_readiness.wake();
        Ok(read)
    }
}

fn management_response(request: &[u8]) -> AxResult<Vec<u8>> {
    const MGMT_INDEX_NONE: u16 = 0xffff;
    const CMD_COMPLETE: u16 = 1;
    const READ_VERSION: u16 = 1;
    const READ_COMMANDS: u16 = 2;
    const READ_INDEX_LIST: u16 = 3;
    const READ_INFO: u16 = 4;
    const SET_POWERED: u16 = 5;
    const UNKNOWN_COMMAND: u8 = 1;
    const INVALID_PARAMS: u8 = 0x0d;
    const INVALID_INDEX: u8 = 0x11;
    if request.len() < 6 {
        return Err(AxError::InvalidInput);
    }
    let opcode = u16::from_le_bytes([request[0], request[1]]);
    let index = u16::from_le_bytes([request[2], request[3]]);
    let parameter_len = usize::from(u16::from_le_bytes([request[4], request[5]]));
    if request.len() != 6 + parameter_len {
        return Err(AxError::InvalidInput);
    }
    let parameters = &request[6..];
    let mut status = 0u8;
    let mut data = Vec::new();
    match opcode {
        READ_VERSION if parameters.is_empty() => data.extend_from_slice(&[1, 0, 0]),
        READ_COMMANDS if parameters.is_empty() => {
            // Linux excludes READ_VERSION/READ_COMMANDS from the advertised
            // per-controller command table. Keep this list truthful: only
            // operations implemented by this HCI socket are exposed.
            let commands = [READ_INDEX_LIST, READ_INFO, SET_POWERED];
            let events: [u16; 0] = [];
            data.extend_from_slice(&(commands.len() as u16).to_le_bytes());
            data.extend_from_slice(&(events.len() as u16).to_le_bytes());
            for item in commands.into_iter().chain(events) {
                data.extend_from_slice(&item.to_le_bytes());
            }
        }
        READ_INDEX_LIST if parameters.is_empty() => {
            #[cfg(feature = "input")]
            let devices = axdriver::bluetooth_devices();
            #[cfg(not(feature = "input"))]
            let devices: Vec<()> = Vec::new();
            data.extend_from_slice(&(devices.len() as u16).to_le_bytes());
            #[cfg(feature = "input")]
            for adapter in devices {
                data.extend_from_slice(&adapter.lock().index().to_le_bytes());
            }
        }
        READ_INFO if parameters.is_empty() => {
            #[cfg(feature = "input")]
            let adapter = axdriver::bluetooth_devices()
                .into_iter()
                .find(|adapter| adapter.lock().index() == index);
            #[cfg(feature = "input")]
            if let Some(adapter) = adapter {
                let adapter = adapter.lock();
                let address = adapter.address();
                let capabilities = adapter.capabilities();
                // mgmt_rp_read_info, with only currently observed controller
                // properties populated. Unimplemented capabilities stay clear.
                data.extend_from_slice(&address);
                data.push(capabilities.hci_version);
                data.extend_from_slice(&capabilities.manufacturer.to_le_bytes());
                data.extend_from_slice(&1u32.to_le_bytes()); // powered is controllable
                data.extend_from_slice(&(u32::from(adapter.is_up())).to_le_bytes());
                data.extend_from_slice(&[0; 3 + 249 + 11]);
            } else {
                status = INVALID_INDEX;
            }
            #[cfg(not(feature = "input"))]
            {
                let _ = index;
                status = INVALID_INDEX;
            }
        }
        SET_POWERED if parameters.len() == 1 && parameters[0] <= 1 => {
            #[cfg(feature = "input")]
            {
                let adapter = axdriver::bluetooth_devices()
                    .into_iter()
                    .find(|adapter| adapter.lock().index() == index);
                if let Some(adapter) = adapter {
                    if adapter.lock().set_device_up(parameters[0] != 0).is_err() {
                        status = 3;
                    }
                } else {
                    status = INVALID_INDEX;
                }
            }
            #[cfg(not(feature = "input"))]
            {
                status = INVALID_INDEX;
            }
        }
        _ if matches!(
            opcode,
            READ_VERSION | READ_COMMANDS | READ_INDEX_LIST | READ_INFO | SET_POWERED
        ) =>
        {
            status = INVALID_PARAMS;
        }
        _ => status = UNKNOWN_COMMAND,
    }
    if status != 0 {
        data.clear();
    }
    let mut response = Vec::new();
    response
        .try_reserve_exact(9 + data.len())
        .map_err(|_| AxError::NoMemory)?;
    response.extend_from_slice(&CMD_COMPLETE.to_le_bytes());
    response.extend_from_slice(&index.to_le_bytes());
    response.extend_from_slice(&(3 + data.len() as u16).to_le_bytes());
    response.extend_from_slice(&opcode.to_le_bytes());
    response.push(status);
    response.extend_from_slice(&data);
    Ok(response)
}
