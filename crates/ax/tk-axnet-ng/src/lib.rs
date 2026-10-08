//! [ArceOS](https://github.com/rcore-os/arceos) network module.
//!
//! It provides unified networking primitives for TCP/UDP communication
//! using various underlying network stacks. Currently, only [smoltcp] is
//! supported.
//!
//! # Organization
//!
//! - [`tcp::TcpSocket`]: A TCP socket that provides POSIX-like APIs.
//! - [`udp::UdpSocket`]: A UDP socket that provides POSIX-like APIs.
//!
//! [smoltcp]: https://github.com/smoltcp-rs/smoltcp

#![no_std]
#![feature(allocator_api)]

#[macro_use]
extern crate log;
extern crate alloc;
#[cfg(test)]
extern crate std;

mod buffer;
mod consts;
mod boot_ipv4;
/// Datagram Congestion Control Protocol over raw IP.
pub mod dccp;
mod device;
mod fragment;
mod general;
mod listen_table;
/// The per-namespace network stack.
pub mod net_stack;
/// Socket option types and the [`Configurable`](options::Configurable) trait.
pub mod options;
/// Bounded namespace-local link-layer packet capture and injection primitives.
pub mod packet;
/// Raw IPv4/IPv6 sockets backed by the namespace socket set.
pub mod raw;
mod router;
/// Stream Control Transmission Protocol over the namespace raw-IP socket set.
pub mod sctp;
mod service;
mod socket;
pub(crate) mod state;
/// TCP socket implementation.
pub mod tcp;
/// UDP socket implementation.
pub mod udp;
/// Unix domain socket implementation.
pub mod unix;
/// Vsock socket implementation.
#[cfg(feature = "vsock")]
pub mod vsock;
mod wrapper;

use alloc::{borrow::ToOwned, boxed::Box, sync::Arc};

use axdriver::{AxDeviceContainer, prelude::*};
use axerrno::{AxError, AxResult};
use smoltcp::wire::{EthernetAddress, Ipv4Cidr, Ipv6Cidr};
pub use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address};
use spin::Once;

use self::{
    consts::{GATEWAY, IP, IP_PREFIX},
    device::{EthernetDevice, LoopbackDevice},
    listen_table::ListenTable,
    router::Router,
    service::Service,
    wrapper::SocketSetWrapper,
};
pub use self::{
    device::{
        DeviceStats, InterfaceInfo, InterfaceKind, RxStep, TapDevice, TapHandle, TunDevice,
        TunHandle, VethEnd,
    },
    net_stack::{NetPollStatus, NetRxTerminalReason, NetStack, NetStackServicePermit},
    packet::{PacketChecksum, PacketChecksumContext},
    router::{
        EgressPass, MAX_DEVICES, PacketAction, PacketContext, PacketDefragQuery, PacketHook,
        PacketHookPoint, RouteInfo, Rule, RxPass,
    },
    socket::*,
};

/// Maximum pending connection count implemented by the bounded listen queues.
pub const MAX_LISTEN_BACKLOG: usize = consts::LISTEN_QUEUE_SIZE;

static DEFAULT_STACK: Once<Arc<NetStack>> = Once::new();

/// One published 802.11 interface backed by an Ethernet-compatible netdev.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WirelessFrequencyInfo {
    pub frequency_mhz: u32,
    pub no_ir: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WirelessInterfaceInfo {
    pub name: alloc::string::String,
    pub ifindex: u32,
    pub phy_index: u32,
    pub rfkill_index: u32,
    pub mac_address: [u8; 6],
    pub frequencies: alloc::vec::Vec<WirelessFrequencyInfo>,
    pub soft_blocked: bool,
    pub hard_blocked: bool,
}

static WIRELESS_INTERFACES: spin::Mutex<alloc::vec::Vec<WirelessInterfaceInfo>> =
    spin::Mutex::new(alloc::vec::Vec::new());

/// Returns a reference to the default (init) network stack.
///
/// Panics if [`init_network`] has not been called yet.
pub fn default_stack() -> &'static Arc<NetStack> {
    DEFAULT_STACK
        .get()
        .expect("Network not initialized; call init_network first")
}

/// Initializes the network subsystem by NIC devices.
///
/// Returns the default [`NetStack`] and also stores it internally so it can
/// be retrieved later via [`default_stack`].
pub fn init_network(mut net_devs: AxDeviceContainer<AxNetDevice>) -> AxResult<Arc<NetStack>> {
    info!("Initialize network subsystem...");

    let socket_set = Arc::try_new(SocketSetWrapper::new()).map_err(|_| AxError::NoMemory)?;
    let listen_table = Arc::try_new(ListenTable::try_new()?).map_err(|_| AxError::NoMemory)?;

    let mut router = Router::try_new_loopback_only(listen_table.clone())?;
    let loopback = Box::try_new(LoopbackDevice::try_new()?).map_err(|_| AxError::NoMemory)?;
    let lo_dev = router.try_add_device(loopback)?;

    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    let lo_ip6 = Ipv6Cidr::new(Ipv6Address::LOCALHOST, 128);
    router.add_rule(Rule::new(
        lo_ip.into(),
        None,
        lo_dev,
        lo_ip.address().into(),
    ));
    router.add_rule(Rule::new(
        lo_ip6.into(),
        None,
        lo_dev,
        lo_ip6.address().into(),
    ));

    let eth0_ip = if let Some(dev) = net_devs.take_one() {
        info!("  use NIC 0: {:?}", dev.device_name());

        let eth0_address = EthernetAddress(dev.mac_address().0);
        let config = boot_ipv4::parse(IP, GATEWAY, IP_PREFIX)?;
        let eth0_ip = config.address.unwrap_or_else(|| Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0));

        let eth0_dev = router.try_add_device(Box::new(EthernetDevice::new(
            "eth0".to_owned(),
            dev,
            eth0_ip,
        )))?;

        if let Some(gateway) = config.gateway {
            router.add_rule(Rule::new(
                Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
                Some(gateway.into()),
                eth0_dev,
                eth0_ip.address().into(),
            ));
        }

        info!("eth0:");
        info!("  mac:  {eth0_address}");
        if let Some(ip) = config.address {
            info!("  ip:   {ip}");
        } else {
            info!("  IPv4 unconfigured: awaiting DHCP/manual address; no boot default route");
        }

        config.address
    } else {
        warn!("  No network device found!");
        None
    };

    for dev in &router.devices {
        info!("Device: {}", dev.name());
    }

    let mut service = Service::try_new(router, socket_set.clone())?;
    service.iface.update_ip_addrs(|ip_addrs| {
        let lo_ip = lo_ip.into();
        if !ip_addrs.contains(&lo_ip) {
            ip_addrs
                .push(lo_ip)
                .expect("loopback address insertion should succeed");
        }
        let lo_ip6 = lo_ip6.into();
        if !ip_addrs.contains(&lo_ip6) {
            ip_addrs
                .push(lo_ip6)
                .expect("loopback IPv6 address insertion should succeed");
        }
        if let Some(eth0_ip) = eth0_ip {
            let eth0_ip = eth0_ip.into();
            if !ip_addrs.contains(&eth0_ip) {
                ip_addrs
                    .push(eth0_ip)
                    .expect("eth0 address insertion should succeed");
            }
        }
    });

    let stack = NetStack::try_new(listen_table, socket_set, service)?;
    DEFAULT_STACK.call_once(|| stack.clone());
    Ok(stack)
}

/// Initializes the default network stack in loopback-only mode.
///
/// This keeps the full socket stack available for localhost-based tests while
/// intentionally ignoring any discovered NIC devices.
pub fn init_network_loopback_only() -> AxResult<Arc<NetStack>> {
    info!("Initialize network subsystem (loopback-only)...");
    let stack = NetStack::try_new_loopback_only()?;
    DEFAULT_STACK.call_once(|| stack.clone());
    Ok(stack)
}

/// Publish an Ethernet-compatible wireless NIC as a separately named link.
///
/// Runtime calls this after the ordinary network stack has started and after
/// rootfs firmware callbacks have completed, so wireless firmware and the
/// link's RX wake owner exist before the interface becomes visible.
pub fn register_wireless_device(dev: AxNetDevice) -> AxResult<u32> {
    let Some(name) = dev.interface_name() else {
        return Err(AxError::InvalidInput);
    };
    if !dev.is_wireless() || name.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let mac_address = dev.mac_address().0;
    let soft_blocked = dev.rfkill_soft_blocked();
    let hard_blocked = dev.rfkill_hard_blocked();
    let frequencies = dev
        .wireless_frequencies()
        .into_iter()
        .map(|frequency| WirelessFrequencyInfo {
            frequency_mhz: frequency.frequency_mhz,
            no_ir: frequency.no_ir,
        })
        .collect();
    let stack = default_stack();
    let interface = Box::new(EthernetDevice::new(
        name.to_owned(),
        dev,
        Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0),
    ));
    let ifindex = stack.try_add_device(interface)?;
    let mut interfaces = WIRELESS_INTERFACES.lock();
    if interfaces.iter().any(|entry| entry.name == name) {
        let _ = stack.remove_device(ifindex);
        return Err(AxError::AlreadyExists);
    }
    if interfaces.try_reserve(1).is_err() {
        let _ = stack.remove_device(ifindex);
        return Err(AxError::NoMemory);
    }
    let phy_index = match interfaces.iter().map(|entry| entry.phy_index).max() {
        Some(index) => match index.checked_add(1) {
            Some(next) => next,
            None => {
                let _ = stack.remove_device(ifindex);
                return Err(AxError::ResourceBusy);
            }
        },
        None => 0,
    };
    interfaces.push(WirelessInterfaceInfo {
        name: name.to_owned(),
        ifindex,
        phy_index,
        rfkill_index: phy_index,
        mac_address,
        frequencies,
        soft_blocked,
        hard_blocked,
    });
    Ok(ifindex)
}

/// Snapshot the wireless links published to init-net.
pub fn wireless_interfaces() -> alloc::vec::Vec<WirelessInterfaceInfo> {
    WIRELESS_INTERFACES.lock().clone()
}

/// Set the software RF-kill state for one published radio index.
pub fn set_wireless_rfkill_soft_blocked(rfkill_index: u32, blocked: bool) -> AxResult {
    let ifindex = WIRELESS_INTERFACES
        .lock()
        .iter()
        .find(|interface| interface.rfkill_index == rfkill_index)
        .map(|interface| interface.ifindex)
        .ok_or(AxError::NoSuchDevice)?;
    default_stack().set_wireless_rfkill_soft_blocked(ifindex, blocked)?;
    let mut interfaces = WIRELESS_INTERFACES.lock();
    let interface = interfaces
        .iter_mut()
        .find(|interface| interface.rfkill_index == rfkill_index)
        .ok_or(AxError::NoSuchDevice)?;
    interface.soft_blocked = blocked;
    Ok(())
}

/// Init vsock subsystem by vsock devices.
#[cfg(feature = "vsock")]
pub fn init_vsock(mut vsock_devs: AxDeviceContainer<AxVsockDevice>) {
    use self::device::register_vsock_device;
    info!("Initialize vsock subsystem...");
    if let Some(dev) = vsock_devs.take_one() {
        info!("  use vsock 0: {:?}", dev.device_name());
        if let Err(e) = register_vsock_device(dev) {
            warn!("Failed to initialize vsock device: {e:?}");
        }
    } else {
        debug!("  No vsock device found!");
    }
}

/// Poll all network interfaces on the default stack.
pub fn poll_interfaces() {
    default_stack().poll_interfaces();
}
