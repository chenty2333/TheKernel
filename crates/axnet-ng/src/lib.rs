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
mod device;
mod general;
mod listen_table;
/// The per-namespace network stack.
pub mod net_stack;
/// Socket option types and the [`Configurable`](options::Configurable) trait.
pub mod options;
mod router;
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
use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
pub use smoltcp::wire::{IpAddress, IpCidr};
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
    device::{DeviceStats, InterfaceInfo, InterfaceKind, VethEnd},
    net_stack::NetStack,
    router::{RouteInfo, Rule},
    socket::*,
};

static DEFAULT_STACK: Once<Arc<NetStack>> = Once::new();

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
    let lo_dev = router.add_device(loopback);

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
        let eth0_ip = Ipv4Cidr::new(IP.parse().expect("Invalid IPv4 address"), IP_PREFIX);

        let eth0_dev = router.add_device(Box::new(EthernetDevice::new(
            "eth0".to_owned(),
            dev,
            eth0_ip,
        )));

        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(GATEWAY.parse().expect("Invalid gateway address")),
            eth0_dev,
            eth0_ip.address().into(),
        ));

        info!("eth0:");
        info!("  mac:  {eth0_address}");
        info!("  ip:   {eth0_ip}");

        Some(eth0_ip)
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
