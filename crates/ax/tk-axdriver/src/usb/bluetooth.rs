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

const MAX_INTEL_FIRMWARE: usize = 16 * 1024 * 1024;
static FIRMWARE_CALLBACK_REGISTERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub struct UsbBluetoothHci {
    adapter: Adapter<Transport>,
    family_hint: tk_bt_hci::DeviceFamily,
    address: [u8; 6],
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
                },
                index,
            ),
            family_hint,
            address: [0; 6],
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
    pub fn set_up(&mut self, up: bool) -> Result<(), Error> {
        self.adapter.set_up(up)
    }
    pub fn set_device_up(&mut self, up: bool) -> Result<(), Error> {
        self.adapter.set_up(up)?;
        if up && self.family_hint != tk_bt_hci::DeviceFamily::Unknown {
            self.adapter.intel_set_event_mask()?;
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
                if length > out.len() {
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
                let length =
                    (6 + usize::from(u16::from_le_bytes([frame[4], frame[5]]))).min(out.len());
                out[..length].copy_from_slice(&frame[..length]);
                return Ok(length);
            }
        }
        out[0] = 4;
        Ok(length + 1)
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
    pub fn pop_monitor(&mut self) -> Option<[u8; 272]> {
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
        .push(device);
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
