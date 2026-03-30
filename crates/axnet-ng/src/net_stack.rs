use alloc::{boxed::Box, sync::Arc};

use axerrno::{AxResult, ax_bail, ax_err_type};
use axsync::Mutex;
use smoltcp::{
    iface::SocketHandle,
    socket::AnySocket,
    wire::{IpCidr, Ipv4Address, Ipv4Cidr},
};

use crate::{
    device::{Device, LoopbackDevice},
    listen_table::ListenTable,
    router::{Router, Rule},
    service::Service,
    wrapper::SocketSetWrapper,
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
    pub(crate) listen_table: Arc<ListenTable>,
    pub(crate) socket_set: Arc<SocketSetWrapper<'static>>,
    pub(crate) service: Mutex<Service>,
    pub(crate) tcp_ephemeral_port: Mutex<u16>,
    pub(crate) udp_ephemeral_port: Mutex<u16>,
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
        Arc::new(Self {
            listen_table,
            socket_set,
            service: Mutex::new(service),
            tcp_ephemeral_port: Mutex::new(PORT_START),
            udp_ephemeral_port: Mutex::new(PORT_START),
        })
    }

    /// Create a minimal network stack with only a loopback device.
    ///
    /// This is used for new network namespaces created via `CLONE_NEWNET`.
    /// The resulting stack has a single `lo` interface (127.0.0.1/8) and no
    /// external connectivity.
    pub fn new_loopback_only() -> Arc<Self> {
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

        let mut service = Service::new(router, socket_set.clone());
        service.iface.update_ip_addrs(|addrs| {
            addrs
                .push(lo_ip.into())
                .expect("loopback address insertion should succeed");
        });

        Self::new(listen_table, socket_set, service)
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
            if self.listen_table.can_listen(port) {
                return Ok(port);
            }
            tries += 1;
        }
        ax_bail!(AddrInUse, "no available ports");
    }

    /// Allocate a UDP ephemeral port.
    pub(crate) fn udp_ephemeral_port(&self) -> AxResult<u16> {
        let mut curr = self.udp_ephemeral_port.lock();
        let port = *curr;
        if *curr == PORT_END {
            *curr = PORT_START;
        } else {
            *curr += 1;
        }
        Ok(port)
    }
}
