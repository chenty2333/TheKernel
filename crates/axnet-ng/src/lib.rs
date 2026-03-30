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

#[macro_use]
extern crate log;
extern crate alloc;

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
use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv4Cidr};
use spin::Once;

pub use self::net_stack::NetStack;
pub use self::router::Rule;
pub use self::socket::*;
pub use self::device::VethEnd;
use self::{
    consts::{GATEWAY, IP, IP_PREFIX},
    device::{EthernetDevice, LoopbackDevice},
    listen_table::ListenTable,
    router::Router,
    service::Service,
    wrapper::SocketSetWrapper,
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
pub fn init_network(mut net_devs: AxDeviceContainer<AxNetDevice>) -> Arc<NetStack> {
    info!("Initialize network subsystem...");

    let socket_set = Arc::new(SocketSetWrapper::new());
    let listen_table = Arc::new(ListenTable::new());

    let mut router = Router::new(listen_table.clone());
    let lo_dev = router.add_device(Box::new(LoopbackDevice::new()));

    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    router.add_rule(Rule::new(
        lo_ip.into(),
        None,
        lo_dev,
        lo_ip.address().into(),
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
        info!("  mac:  {}", eth0_address);
        info!("  ip:   {}", eth0_ip);

        Some(eth0_ip)
    } else {
        warn!("  No network device found!");
        None
    };

    for dev in &router.devices {
        info!("Device: {}", dev.name());
    }

    let mut service = Service::new(router, socket_set.clone());
    service.iface.update_ip_addrs(|ip_addrs| {
        ip_addrs
            .push(lo_ip.into())
            .expect("loopback address insertion should succeed");
        if let Some(eth0_ip) = eth0_ip {
            ip_addrs
                .push(eth0_ip.into())
                .expect("eth0 address insertion should succeed");
        }
    });

    let stack = NetStack::new(listen_table, socket_set, service);
    DEFAULT_STACK.call_once(|| stack.clone());
    stack
}

/// Init vsock subsystem by vsock devices.
#[cfg(feature = "vsock")]
pub fn init_vsock(mut vsock_devs: AxDeviceContainer<AxVsockDevice>) {
    use self::device::register_vsock_device;
    info!("Initialize vsock subsystem...");
    if let Some(dev) = vsock_devs.take_one() {
        info!("  use vsock 0: {:?}", dev.device_name());
        if let Err(e) = register_vsock_device(dev) {
            warn!("Failed to initialize vsock device: {:?}", e);
        }
    } else {
        warn!("  No vsock device found!");
    }
}

/// Poll all network interfaces on the default stack.
pub fn poll_interfaces() {
    default_stack().poll_interfaces();
}
