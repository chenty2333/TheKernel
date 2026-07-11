use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, Ordering};

use axerrno::{AxError, AxResult, ax_bail, ax_err_type};
use axsync::Mutex;
use smoltcp::{
    iface::SocketHandle,
    socket::AnySocket,
    wire::{IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
};

use crate::{
    device::{Device, DeviceStats, InterfaceInfo, LoopbackDevice},
    listen_table::ListenTable,
    router::{RouteInfo, Router, Rule},
    service::Service,
    unix::UnixNamespace,
    wrapper::{SocketSetWrapper, Transport},
};

/// A self-contained network stack instance.
///
/// Each `NetStack` holds its own socket set, listen table, service (interface +
/// router), and ephemeral port counters. Multiple `NetStack` instances can
/// coexist for network namespace isolation.
///
/// **Drop ordering:** `listen_table` is declared before `socket_set` so that
/// listen-table entries (which hold `Weak<SocketSetWrapper>`) are cleaned up
/// while the socket set is still alive.
pub struct NetStack {
    unix_namespace: Arc<UnixNamespace>,
    pub(crate) listen_table: Arc<ListenTable>,
    pub(crate) socket_set: Arc<SocketSetWrapper<'static>>,
    pub(crate) service: Mutex<Service>,
    pub(crate) tcp_ephemeral_port: Mutex<u16>,
    pub(crate) udp_ephemeral_port: Mutex<u16>,
    ipv4_conf_default_tag: AtomicI32,
    ipv4_conf_lo_tag: AtomicI32,
}

const PORT_START: u16 = 0xc000;
const PORT_END: u16 = 0xffff;

impl NetStack {
    /// Create a new `NetStack` from pre-built components.
    pub(crate) fn new(
        listen_table: Arc<ListenTable>,
        socket_set: Arc<SocketSetWrapper<'static>>,
        service: Service,
    ) -> Arc<Self> {
        Self::try_new(listen_table, socket_set, service).expect("failed to allocate network stack")
    }

    pub(crate) fn try_new(
        listen_table: Arc<ListenTable>,
        socket_set: Arc<SocketSetWrapper<'static>>,
        service: Service,
    ) -> AxResult<Arc<Self>> {
        let unix_namespace = UnixNamespace::try_new()?;
        Arc::try_new(Self {
            unix_namespace,
            listen_table,
            socket_set,
            service: Mutex::new(service),
            tcp_ephemeral_port: Mutex::new(PORT_START),
            udp_ephemeral_port: Mutex::new(PORT_START),
            ipv4_conf_default_tag: AtomicI32::new(0),
            ipv4_conf_lo_tag: AtomicI32::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Returns the AF_UNIX abstract-name namespace paired with this network
    /// stack. Pathname Unix sockets remain VFS namespace objects.
    pub fn unix_namespace(&self) -> Arc<UnixNamespace> {
        self.unix_namespace.clone()
    }

    /// Create a minimal network stack with only a loopback device.
    ///
    /// This is used for new network namespaces created via `CLONE_NEWNET`.
    /// The resulting stack has a single `lo` interface (127.0.0.1/8) and no
    /// external connectivity.
    pub fn new_loopback_only() -> Arc<Self> {
        Self::try_new_loopback_only().expect("failed to allocate loopback network namespace")
    }

    /// Fallibly creates a minimal loopback-only network namespace.
    pub fn try_new_loopback_only() -> AxResult<Arc<Self>> {
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

        let mut service = Service::new(router, socket_set.clone());
        service.iface.update_ip_addrs(|addrs| {
            let lo_ip = lo_ip.into();
            if !addrs.iter().any(|existing| *existing == lo_ip) {
                addrs
                    .push(lo_ip)
                    .expect("loopback address insertion should succeed");
            }
            let lo_ip6 = lo_ip6.into();
            if !addrs.iter().any(|existing| *existing == lo_ip6) {
                addrs
                    .push(lo_ip6)
                    .expect("loopback IPv6 address insertion should succeed");
            }
        });

        Self::try_new(listen_table, socket_set, service)
    }

    /// Add a network device to this stack's router.
    ///
    /// Returns the device index, needed for [`add_route`](Self::add_route).
    pub fn add_device(&self, device: Box<dyn Device>) -> usize {
        self.service.lock().router.add_device(device)
    }

    /// Add a routing rule to this stack.
    pub fn add_route(&self, rule: Rule) {
        self.service.lock().router.add_rule(rule);
    }

    /// Add an IP address to this stack's interface.
    pub fn add_ip_addr(&self, addr: IpCidr) -> AxResult {
        let mut result = Ok(());
        self.service.lock().iface.update_ip_addrs(|addrs| {
            if addrs.push(addr).is_err() {
                result = Err(ax_err_type!(BadState, "IP address list full"));
            }
        });
        result
    }

    /// Poll all network interfaces owned by this stack.
    pub fn poll_interfaces(&self) {
        while self.service.lock().poll(&mut self.socket_set.inner.lock()) {}
    }

    /// Snapshot per-interface packet and byte counters for this namespace.
    pub fn device_stats(&self) -> Vec<(String, DeviceStats)> {
        self.service.lock().router.device_stats()
    }

    /// Snapshot the interfaces currently owned by this network stack.
    pub fn interfaces(&self) -> Vec<InterfaceInfo> {
        self.service.lock().router.interfaces()
    }

    /// Snapshot the routes currently used by this network stack.
    pub fn routes(&self) -> Vec<RouteInfo> {
        self.service.lock().router.routes()
    }

    /// Return the Linux-compatible IPv4 interface `tag` sysctl value.
    pub fn ipv4_conf_tag(&self, iface: &str) -> Option<i32> {
        match iface {
            "default" => Some(self.ipv4_conf_default_tag.load(Ordering::Acquire)),
            "lo" => Some(self.ipv4_conf_lo_tag.load(Ordering::Acquire)),
            _ => None,
        }
    }

    /// Update the Linux-compatible IPv4 interface `tag` sysctl value.
    pub fn set_ipv4_conf_tag(&self, iface: &str, value: i32) -> AxResult {
        match iface {
            "default" => self.ipv4_conf_default_tag.store(value, Ordering::Release),
            "lo" => self.ipv4_conf_lo_tag.store(value, Ordering::Release),
            _ => ax_bail!(NotFound, "unknown IPv4 interface sysctl"),
        }
        Ok(())
    }

    /// Acquire a lock on the Service.
    pub(crate) fn get_service(&self) -> axsync::MutexGuard<'_, Service> {
        self.service.lock()
    }

    /// Lock the service before the socket set to avoid AB-BA deadlocks.
    pub(crate) fn with_service_and_socket_mut<T, R>(
        &self,
        handle: SocketHandle,
        f: impl FnOnce(&mut Service, &mut T) -> R,
    ) -> R
    where
        T: AnySocket<'static>,
    {
        let mut service = self.service.lock();
        let mut sockets = self.socket_set.inner.lock();
        let socket = sockets.get_mut(handle);
        f(&mut service, socket)
    }

    /// Allocate a TCP ephemeral port.
    pub(crate) fn tcp_ephemeral_port(&self) -> AxResult<u16> {
        let mut curr = self.tcp_ephemeral_port.lock();
        let mut tries = 0;
        while tries <= PORT_END - PORT_START {
            let port = *curr;
            if *curr == PORT_END {
                *curr = PORT_START;
            } else {
                *curr += 1;
            }
            if !self.socket_set.port_in_use(Transport::Tcp, port) {
                return Ok(port);
            }
            tries += 1;
        }
        ax_bail!(AddrInUse, "no available ports");
    }

    /// Allocate a UDP ephemeral port.
    pub(crate) fn udp_ephemeral_port(&self) -> AxResult<u16> {
        let mut curr = self.udp_ephemeral_port.lock();
        let mut tries = 0;
        while tries <= PORT_END - PORT_START {
            let port = *curr;
            if *curr == PORT_END {
                *curr = PORT_START;
            } else {
                *curr += 1;
            }
            if !self.socket_set.port_in_use(Transport::Udp, port) {
                return Ok(port);
            }
            tries += 1;
        }
        ax_bail!(AddrInUse, "no available ports");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InterfaceKind;

    #[test]
    fn loopback_stack_reports_real_interface_and_routes() {
        let stack = NetStack::new_loopback_only();
        let interfaces = stack.interfaces();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].index, 1);
        assert_eq!(interfaces[0].name, "lo");
        assert_eq!(interfaces[0].kind, InterfaceKind::Loopback);
        assert_eq!(interfaces[0].addresses.len(), 2);

        let routes = stack.routes();
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|route| route.interface_index == 1));
        assert!(routes.iter().all(|route| route.gateway.is_none()));
    }
}
