use alloc::{boxed::Box, string::String, sync::Arc, task::Wake, vec::Vec};
use core::{
    sync::atomic::{AtomicI32, Ordering},
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult, ax_bail, ax_err_type};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;
use smoltcp::{
    iface::SocketHandle,
    socket::AnySocket,
    wire::{IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
};

use crate::{
    device::{Device, DeviceStats, InterfaceInfo, LoopbackDevice, PacketSendProgress},
    listen_table::ListenTable,
    packet::{
        PacketBroker, PacketDeviceCapabilities, PacketEndpoint, PacketRecord, PacketResult,
        PacketSelector, PacketSendRequest,
    },
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
    packet_broker: Arc<PacketBroker>,
    pub(crate) listen_table: Arc<ListenTable>,
    pub(crate) socket_set: Arc<SocketSetWrapper<'static>>,
    pub(crate) service: Mutex<Service>,
    poll_source: Arc<PollSet>,
    poll_waker: Waker,
    pub(crate) tcp_ephemeral_port: Mutex<u16>,
    pub(crate) udp_ephemeral_port: Mutex<u16>,
    ipv4_conf_default_tag: AtomicI32,
    ipv4_conf_lo_tag: AtomicI32,
}

struct NetPollWake(Arc<PollSet>);

impl Wake for NetPollWake {
    fn wake(self: Arc<Self>) {
        self.0.as_ref().wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.as_ref().wake();
    }
}

const PORT_START: u16 = 0xc000;
const PORT_END: u16 = 0xffff;

impl NetStack {
    pub(crate) fn try_new(
        listen_table: Arc<ListenTable>,
        socket_set: Arc<SocketSetWrapper<'static>>,
        service: Service,
    ) -> AxResult<Arc<Self>> {
        let unix_namespace = UnixNamespace::try_new()?;
        let packet_broker = service.router.packet_broker();
        let poll_source = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let poll_wake =
            Arc::try_new(NetPollWake(poll_source.clone())).map_err(|_| AxError::NoMemory)?;
        let poll_waker = Waker::from(poll_wake);
        let stack = Arc::try_new(Self {
            unix_namespace,
            packet_broker,
            listen_table,
            socket_set,
            service: Mutex::new(service),
            poll_source,
            poll_waker,
            tcp_ephemeral_port: Mutex::new(PORT_START),
            udp_ephemeral_port: Mutex::new(PORT_START),
            ipv4_conf_default_tag: AtomicI32::new(0),
            ipv4_conf_lo_tag: AtomicI32::new(0),
        })
        .map_err(|_| AxError::NoMemory)?;
        #[cfg(target_os = "none")]
        stack.packet_broker.start_drain_worker()?;
        Ok(stack)
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

        let mut service = Service::try_new(router, socket_set.clone())?;
        service.iface.update_ip_addrs(|addrs| {
            let lo_ip = lo_ip.into();
            if !addrs.contains(&lo_ip) {
                addrs
                    .push(lo_ip)
                    .expect("loopback address insertion should succeed");
            }
            let lo_ip6 = lo_ip6.into();
            if !addrs.contains(&lo_ip6) {
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

    /// Subscribes a bounded link-packet endpoint to this network namespace.
    pub fn subscribe_packets(&self, selector: PacketSelector) -> PacketResult<Arc<PacketEndpoint>> {
        self.packet_broker.subscribe(selector)
    }

    /// Returns the packet I/O capabilities of one one-based interface index.
    pub fn packet_device_capabilities(
        &self,
        interface_index: u32,
    ) -> Option<PacketDeviceCapabilities> {
        self.service
            .lock()
            .router
            .packet_device_capabilities(interface_index)
    }

    /// Sends one raw or cooked packet through an interface.
    ///
    /// Outgoing capture delivery is drained only after the service mutex is
    /// released.  `origin` prevents a packet endpoint from receiving its own
    /// injection while allowing other endpoints to observe it.
    pub fn send_packet(
        &self,
        interface_index: u32,
        origin: &PacketEndpoint,
        request: PacketSendRequest<'_>,
    ) -> AxResult<()> {
        let result = self
            .service
            .lock()
            .send_packet(interface_index, origin, request);
        self.packet_broker.drain_staged();
        match result {
            Ok(PacketSendProgress::NoImmediateIngress) => Ok(()),
            Ok(PacketSendProgress::ImmediateIngressQueued) => {
                // Loopback injection queues an immediate ingress copy. Retire
                // it after the service lock and outgoing capture so observers
                // do not depend on an unrelated transport call.
                self.poll_interfaces();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Claims an already-published record without scanning devices, then polls
    /// device ingress once if the endpoint would otherwise block.
    pub fn try_receive_packet(
        &self,
        endpoint: &PacketEndpoint,
        peek: bool,
    ) -> AxResult<PacketRecord> {
        match endpoint.try_receive(peek) {
            Ok(record) => Ok(record),
            Err(AxError::WouldBlock) => {
                self.poll_interfaces();
                endpoint.try_receive(peek)
            }
            Err(error) => Err(error),
        }
    }

    /// Returns endpoint readiness directly when data or hangup is published,
    /// polling the network service only while receive readiness is absent.
    pub fn poll_packet_endpoint(&self, endpoint: &PacketEndpoint) -> IoEvents {
        let ready = endpoint.poll();
        if ready.intersects(IoEvents::READABLE | IoEvents::HANGUP) {
            return ready;
        }
        self.poll_interfaces();
        endpoint.poll()
    }

    /// Registers both endpoint publication and device-network wake sources.
    ///
    /// Endpoint readiness alone cannot wake a receive-only packet socket when
    /// a device has work that has not yet crossed the capture point. The
    /// aggregate retains both sources, while writable-only interest remains
    /// optimistic until devices expose completion-credit readiness.
    pub fn register_packet_endpoint<'a>(
        &'a self,
        endpoint: &'a PacketEndpoint,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let endpoint_interest = events.intersects(IoEvents::READABLE | IoEvents::HANGUP);
        let network_interest = events.contains(IoEvents::READABLE) && !endpoint.is_detached();
        let mut prepared = PreparedPollRegistration::try_new(
            usize::from(endpoint_interest) + usize::from(network_interest),
        )?;
        if endpoint_interest {
            endpoint.arm_readiness(&mut prepared, context.waker())?;
        }
        if network_interest {
            self.arm_readiness(&mut prepared, u64::MAX, context.waker())?;
            // Close the device-bridge -> stack-source arm window. A device
            // wake that arrived before the stack source was armed leaves real
            // receive work behind; polling after both registrations converts
            // that work into endpoint readiness before publication.
            self.poll_interfaces();
        }
        prepared.commit()
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
        loop {
            let progressed = self.service.lock().poll(&mut self.socket_set.inner.lock());
            self.packet_broker.drain_staged();
            if !progressed {
                break;
            }
        }
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

    /// Arms this stack's stable network source into caller-reserved aggregate
    /// storage after refreshing every selected hardware/protocol bridge.
    pub(crate) fn arm_readiness<'a>(
        &'a self,
        prepared: &mut PreparedPollRegistration<'a>,
        device_mask: u64,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        self.service
            .lock()
            .register_waker(device_mask, &self.poll_waker)?;
        prepared.arm(&self.poll_source, waker)?;
        Ok(())
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
    use alloc::task::Wake;
    use core::sync::atomic::AtomicUsize;

    use super::*;
    use crate::{
        InterfaceKind,
        packet::{LinkPacketType, PacketProtocol, PacketView},
    };

    const TEST_PROTOCOL: u16 = 0x88b5;
    const TEST_FRAME: [u8; 20] = [
        0x02, 0x11, 0x22, 0x33, 0x44, 0x55, // destination
        0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, // source
        0x88, 0xb5, // protocol
        b'p', b'a', b'c', b'k', b'e', b't',
    ];

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn packet_endpoint(stack: &NetStack) -> Arc<PacketEndpoint> {
        stack
            .subscribe_packets(PacketSelector::new(
                PacketProtocol::All,
                Some(1),
                PacketView::Raw,
                true,
            ))
            .unwrap()
    }

    fn ingress_only_endpoint(stack: &NetStack) -> Arc<PacketEndpoint> {
        stack
            .subscribe_packets(PacketSelector::new(
                PacketProtocol::Exact(TEST_PROTOCOL),
                Some(1),
                PacketView::Raw,
                false,
            ))
            .unwrap()
    }

    fn queue_loopback_without_poll(stack: &NetStack, source: &PacketEndpoint) {
        let progress = stack
            .service
            .lock()
            .send_packet(
                1,
                source,
                PacketSendRequest::Raw {
                    protocol: TEST_PROTOCOL,
                    frame: &TEST_FRAME,
                },
            )
            .unwrap();
        assert_eq!(progress, PacketSendProgress::ImmediateIngressQueued);
    }

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

    #[test]
    fn direct_loopback_packet_send_retires_outgoing_and_ingress() {
        let stack = NetStack::new_loopback_only();
        let source = packet_endpoint(&stack);
        let observer = packet_endpoint(&stack);

        stack
            .send_packet(
                1,
                source.as_ref(),
                PacketSendRequest::Raw {
                    protocol: TEST_PROTOCOL,
                    frame: &TEST_FRAME,
                },
            )
            .unwrap();

        assert_eq!(observer.queue_usage().0, 2);
        assert_eq!(source.queue_usage().0, 1);
        let outgoing = observer.try_receive(false).unwrap();
        let ingress = observer.try_receive(false).unwrap();
        assert_eq!(outgoing.metadata().packet_type, LinkPacketType::Outgoing);
        assert_eq!(ingress.metadata().packet_type, LinkPacketType::OtherHost);
        assert_eq!(outgoing.data(), TEST_FRAME);
        assert_eq!(ingress.data(), TEST_FRAME);
    }

    #[test]
    fn packet_receive_attempt_polls_pending_device_ingress() {
        let stack = NetStack::new_loopback_only();
        let source = packet_endpoint(&stack);
        let receiver = ingress_only_endpoint(&stack);
        queue_loopback_without_poll(&stack, source.as_ref());
        assert_eq!(receiver.queue_usage().0, 0);

        let record = stack.try_receive_packet(receiver.as_ref(), false).unwrap();
        assert_eq!(record.metadata().packet_type, LinkPacketType::OtherHost);
        assert_eq!(record.data(), TEST_FRAME);
    }

    #[test]
    fn packet_read_registration_closes_pending_device_wake_gap() {
        let stack = NetStack::new_loopback_only();
        let source = packet_endpoint(&stack);
        let receiver = ingress_only_endpoint(&stack);
        queue_loopback_without_poll(&stack, source.as_ref());
        assert_eq!(receiver.queue_usage().0, 0);

        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let registration = stack
            .register_packet_endpoint(receiver.as_ref(), &mut context, IoEvents::READABLE)
            .unwrap();

        assert!(wake_count.0.load(Ordering::Relaxed) > 0);
        assert_eq!(receiver.queue_usage().0, 1);
        drop(registration);
    }

    #[test]
    fn packet_read_wait_retains_endpoint_and_network_sources() {
        let stack = NetStack::new_loopback_only();
        let endpoint = packet_endpoint(&stack);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);

        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::READABLE)
            .unwrap();
        assert_eq!(registration.source_count(), 2);
        drop(registration);

        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::HANGUP)
            .unwrap();
        assert_eq!(registration.source_count(), 1);
        drop(registration);

        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::WRITABLE)
            .unwrap();
        assert_eq!(registration.source_count(), 0);
    }
}
