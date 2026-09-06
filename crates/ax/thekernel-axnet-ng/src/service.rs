use alloc::{boxed::Box, sync::Arc};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult};
use axhal::time::{NANOS_PER_MICROS, TimeValue, wall_time_nanos};
use axpoll::PollRegistrationError;
use axtask::future::{TimerRegistrationError, sleep_until};
use smoltcp::{
    iface::{Interface, PollIngressSingleResult, PollResult, SocketSet},
    time::Instant,
    wire::{HardwareAddress, IpAddress, IpListenEndpoint},
};

use crate::{
    device::PacketSendProgress,
    packet::{PacketEndpoint, PacketSendRequest},
    router::{RX_PASS_BUDGET, Router, Rule, RxWakeRegistration},
    wrapper::SocketSetWrapper,
};

/// Transport-neutral route admission rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteReject {
    Unreachable,
    BoundSourceUnavailable,
}

impl RouteReject {
    pub(crate) const fn as_ax_error(self) -> AxError {
        match self {
            Self::Unreachable => AxError::NoSuchDevice,
            Self::BoundSourceUnavailable => AxError::NotFound,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboundRoute {
    pub(crate) src_addr: IpAddress,
    pub(crate) device_mask: u64,
    pub(crate) ifindex: u32,
    pub(crate) next_hop: IpAddress,
}

fn now() -> Instant {
    Instant::from_micros_const((wall_time_nanos() / NANOS_PER_MICROS) as i64)
}

pub struct Service {
    pub iface: Interface,
    pub(crate) router: Router,
    pub(crate) socket_set: Arc<SocketSetWrapper<'static>>,
    timeout: Option<ServiceTimeout>,
}

/// Result of one bounded task-context network pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServicePoll {
    /// No bounded source retained work for a subsequent pass.
    Quiescent,
    /// At least one bounded source retained work for a subsequent pass.
    Continuation,
    /// A link device fenced itself after a completion/ownership protocol
    /// error. The receive worker remains the software owner for all other
    /// devices while the fenced source retains its DMA owners for teardown.
    /// `continuation` is true when a healthy software source retained work
    /// after this bounded pass; callers must yield and poll it again while
    /// keeping the quarantine visible.
    Quarantined { continuation: bool },
}

struct ServiceTimeout {
    deadline: TimeValue,
    future: Pin<Box<dyn Future<Output = Result<(), TimerRegistrationError>> + Send>>,
}

impl Service {
    pub(crate) fn try_new(
        mut router: Router,
        socket_set: Arc<SocketSetWrapper<'static>>,
    ) -> AxResult<Self> {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, now());

        Ok(Self {
            iface,
            router,
            socket_set,
            timeout: None,
        })
    }

    /// Runs exactly one bounded network pass.
    ///
    /// Ingress is admitted by the router's fixed link-frame budget and then
    /// consumed through smoltcp's single-packet ingress API.  Egress uses the
    /// API's bounded `poll_egress` operation and a bounded router dispatch;
    /// the unbounded `Interface::poll` entry point is intentionally not used.
    pub fn poll(&mut self, sockets: &mut SocketSet) -> ServicePoll {
        let timestamp = now();

        let rx = self.router.poll(timestamp);
        let mut ingress = 0;
        while ingress < RX_PASS_BUDGET {
            match self
                .iface
                .poll_ingress_single(timestamp, &mut self.router, sockets)
            {
                PollIngressSingleResult::None => break,
                PollIngressSingleResult::PacketProcessed => ingress += 1,
                PollIngressSingleResult::SocketStateChanged => {
                    ingress += 1;
                }
            }
        }

        // `poll_egress` is bounded by the fixed socket-set capacity and
        // emits at most one packet per socket.  Preserve its continuation
        // signal so a socket with more queued data is revisited on the next
        // task-context pass instead of relying on a fresh IRQ.
        let egress_blocked = self.router.tx_queue_full();
        // Raw parser terminal discards must still publish their exact token
        // to the router. Do not dequeue any socket egress while the bounded
        // router TX queue has no slot; this preserves the raw entry and its
        // metadata for the next pass instead of losing its completion lease.
        let egress = if egress_blocked {
            PollResult::None
        } else {
            self.iface.poll_egress(timestamp, &mut self.router, sockets)
        };
        // Raw smoltcp parsing can terminally discard an entry before it ever
        // reaches a device TxToken. Its Weak completion handler releases the
        // handle synchronously; drain those stale plans in this same bounded
        // service transaction rather than waiting for route mutation.
        self.router.prune_dead_raw_routes();
        let tx = self.router.dispatch(timestamp);

        let continuation = rx.is_continuation()
            || tx.is_continuation()
            || matches!(egress, PollResult::SocketStateChanged)
            || egress_blocked
            || ingress == RX_PASS_BUDGET
            // Ingress can stop while the router's TX queue is full.  The
            // dispatch pass frees that queue, but must not make this pass
            // quiescent while a packet remains in the software RX queue.
            || self.router.has_rx_backlog();
        if self.router.has_quarantined_device() {
            return ServicePoll::Quarantined { continuation };
        }
        if continuation {
            ServicePoll::Continuation
        } else {
            ServicePoll::Quiescent
        }
    }

    pub(crate) fn send_packet(
        &mut self,
        interface_index: u32,
        origin: &PacketEndpoint,
        request: PacketSendRequest<'_>,
    ) -> AxResult<PacketSendProgress> {
        self.router
            .send_packet(interface_index, origin, request, now())
    }

    /// Sends a kernel-owned link frame which has no AF_PACKET capture origin.
    pub(crate) fn send_packet_unattributed(
        &mut self,
        interface_index: u32,
        request: PacketSendRequest<'_>,
    ) -> AxResult<PacketSendProgress> {
        self.router
            .send_packet_from(interface_index, None, request, now())
    }

    pub(crate) fn has_rx_backlog(&self) -> bool {
        self.router.has_rx_backlog()
    }

    pub(crate) fn has_rx_wake_capable_device(&self) -> bool {
        self.router.has_rx_wake_capable_device()
    }

    /// Arm the permanent receive-worker wake sources for every live device.
    pub(crate) fn register_rx_waker(&mut self, waker: &Waker) -> RxWakeRegistration {
        self.router.register_rx_waker(waker)
    }

    pub(crate) fn stop_rx_waker(&self) {
        self.router.stop_rx_waker();
    }

    fn route_for(&self, dst_addr: &IpAddress) -> Result<&Rule, RouteReject> {
        self.router
            .table
            .lookup(dst_addr)
            .ok_or(RouteReject::Unreachable)
    }

    fn device_mask(&self, rule: &Rule) -> u64 {
        // `Rule::dev` is a stable ifindex. Convert it at the router boundary
        // before constructing the private storage-slot readiness mask.
        // Route resolution has already validated the rule, so a disappeared
        // device is represented by an empty mask rather than retargeting it.
        match self.router.device_slot(rule.dev) {
            Some(slot) if slot < 64 => 1u64 << slot,
            _ => 0,
        }
    }

    pub(crate) fn resolve_outbound(
        &self,
        dst_addr: &IpAddress,
        bound_src: Option<IpAddress>,
    ) -> Result<OutboundRoute, RouteReject> {
        self.resolve_outbound_with_dont_route(dst_addr, bound_src, false)
    }

    pub(crate) fn resolve_outbound_with_dont_route(
        &self,
        dst_addr: &IpAddress,
        bound_src: Option<IpAddress>,
        dont_route: bool,
    ) -> Result<OutboundRoute, RouteReject> {
        let rule = self.route_for(dst_addr)?;
        if dont_route && rule.via.is_some() {
            return Err(RouteReject::Unreachable);
        }
        if let Some(bound_src) = bound_src
            && bound_src != rule.src
        {
            return Err(RouteReject::BoundSourceUnavailable);
        }
        Ok(OutboundRoute {
            src_addr: bound_src.unwrap_or(rule.src),
            device_mask: self.device_mask(rule),
            ifindex: rule.dev,
            next_hop: rule.via.unwrap_or(*dst_addr),
        })
    }

    pub(crate) fn validate_bind_addr(&self, addr: IpAddress) -> AxResult<()> {
        if addr.is_unspecified() {
            return Ok(());
        }
        self.resolve_outbound(&addr, Some(addr))
            .map(|_| ())
            .map_err(|_| RouteReject::BoundSourceUnavailable.as_ax_error())
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u64 {
        match endpoint.addr {
            Some(addr) => self
                .resolve_outbound(&addr, Some(addr))
                .map_or(0, |route| route.device_mask),
            None => u64::MAX,
        }
    }

    pub fn register_waker(
        &mut self,
        mask: u64,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        let next = self.iface.poll_at(now(), &self.socket_set.inner.lock());

        if let Some(t) = next {
            let next = TimeValue::from_micros(t.total_micros() as _);

            if self
                .timeout
                .as_ref()
                .is_none_or(|timeout| timeout.deadline != next)
            {
                let future =
                    Box::try_new(sleep_until(next)).map_err(|_| PollRegistrationError::NoMemory)?;
                self.timeout = Some(ServiceTimeout {
                    deadline: next,
                    future: Box::into_pin(future),
                });
            }

            let mut context = Context::from_waker(waker);
            let result = self
                .timeout
                .as_mut()
                .map(|timeout| timeout.future.as_mut().poll(&mut context));
            match result {
                Some(core::task::Poll::Ready(Ok(()))) => {
                    self.timeout = None;
                    waker.wake_by_ref();
                }
                Some(core::task::Poll::Ready(Err(error))) => {
                    self.timeout = None;
                    return Err(map_timer_registration_error(error));
                }
                Some(core::task::Poll::Pending) | None => {}
            }
        } else {
            self.timeout = None;
        }

        for (i, device) in self.router.devices.iter().enumerate() {
            if i >= 64 || mask & (1u64 << i) != 0 {
                device.register_waker(waker)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn map_timer_registration_error(error: TimerRegistrationError) -> PollRegistrationError {
    match error {
        TimerRegistrationError::CapacityExhausted => PollRegistrationError::Quota,
        TimerRegistrationError::TokenSpaceExhausted | TimerRegistrationError::DeadlineOverflow => {
            PollRegistrationError::InvalidState
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::task::Waker;

    use smoltcp::{
        phy::{Device as SmoltcpDevice, TxToken as SmoltcpTxToken},
        storage::PacketBuffer,
        wire::{IpAddress, Ipv4Address, Ipv4Cidr},
    };

    use super::*;
    use crate::{
        consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
        device::{Device, DeviceStats, InterfaceKind, LoopbackDevice, RxStep},
        listen_table::ListenTable,
        packet::PacketDeviceContext,
    };

    struct OnePacketDevice {
        pending: bool,
    }

    impl Device for OnePacketDevice {
        fn name(&self) -> &str {
            "test0"
        }

        fn stats(&self) -> DeviceStats {
            DeviceStats::default()
        }

        fn interface_kind(&self) -> InterfaceKind {
            InterfaceKind::Loopback
        }

        fn mtu(&self) -> usize {
            LOOPBACK_MTU
        }

        fn has_rx_backlog(&self) -> bool {
            self.pending
        }

        fn recv(
            &mut self,
            _context: PacketDeviceContext<'_>,
            buffer: &mut crate::device::IngressPacketBuffer,
            _timestamp: Instant,
        ) -> RxStep {
            if !self.pending {
                return RxStep::Idle;
            }
            self.pending = false;
            let packet = buffer.enqueue(1, 1).unwrap();
            packet[0] = 0;
            RxStep::Delivered
        }

        fn send(
            &mut self,
            _context: PacketDeviceContext<'_>,
            _next_hop: IpAddress,
            _packet: &[u8],
            _timestamp: Instant,
        ) -> bool {
            false
        }

        fn register_waker(&self, _waker: &Waker) -> Result<(), PollRegistrationError> {
            Ok(())
        }
    }

    fn test_service() -> Service {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let mut router = Router::try_new_loopback_only(listen_table).unwrap();
        let dev = router
            .try_add_device(Box::new(LoopbackDevice::try_new().unwrap()))
            .unwrap();
        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            None,
            dev,
            Ipv4Address::new(10, 0, 2, 15).into(),
        ));
        Service::try_new(router, socket_set).unwrap()
    }

    #[test]
    fn resolve_outbound_rejects_mismatched_bound_source() {
        let service = test_service();
        let err = service
            .resolve_outbound(
                &IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)),
                Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST)),
            )
            .unwrap_err();
        assert_eq!(err, RouteReject::BoundSourceUnavailable);
    }

    #[test]
    fn resolve_outbound_reports_missing_route() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let service = Service::try_new(
            Router::try_new_loopback_only(listen_table).unwrap(),
            socket_set,
        )
        .unwrap();
        let err = service
            .resolve_outbound(&IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)), None)
            .unwrap_err();
        assert_eq!(err, RouteReject::Unreachable);
    }

    #[test]
    fn dont_route_rejects_gateway_routes_but_accepts_direct_routes() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let mut router = Router::try_new_loopback_only(listen_table).unwrap();
        let dev = router
            .try_add_device(Box::new(LoopbackDevice::try_new().unwrap()))
            .unwrap();
        let source = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15));
        let destination = IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8));
        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2))),
            dev,
            source,
        ));
        let service = Service::try_new(router, socket_set).unwrap();

        assert!(
            service
                .resolve_outbound_with_dont_route(&destination, None, false)
                .is_ok()
        );
        assert_eq!(
            service
                .resolve_outbound_with_dont_route(&destination, None, true)
                .unwrap_err(),
            RouteReject::Unreachable
        );

        let direct = test_service();
        assert!(
            direct
                .resolve_outbound_with_dont_route(&destination, None, true)
                .is_ok()
        );
    }

    #[test]
    fn validate_bind_addr_accepts_local_source() {
        let service = test_service();
        assert_eq!(
            service.validate_bind_addr(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15))),
            Ok(())
        );
    }

    #[test]
    fn validate_bind_addr_rejects_nonlocal_source() {
        let service = test_service();
        let err = service
            .validate_bind_addr(IpAddress::Ipv4(Ipv4Address::LOCALHOST))
            .unwrap_err();
        assert_eq!(err, AxError::NotFound);
    }

    #[test]
    fn validate_bind_addr_maps_missing_route_to_not_available() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let service = Service::try_new(
            Router::try_new_loopback_only(listen_table).unwrap(),
            socket_set,
        )
        .unwrap();
        let err = service
            .validate_bind_addr(IpAddress::Ipv4(Ipv4Address::new(10, 255, 254, 253)))
            .unwrap_err();
        assert_eq!(err, AxError::NotFound);
    }

    #[test]
    fn software_rx_backlog_continues_after_full_tx_queue_drains() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let mut router = Router::try_new_loopback_only(listen_table).unwrap();
        router
            .try_add_device(Box::new(OnePacketDevice { pending: true }))
            .unwrap();
        let mut service = Service::try_new(router, socket_set.clone()).unwrap();
        assert!(!service.has_rx_wake_capable_device());

        for _ in 0..PACKET_QUEUE_LEN {
            SmoltcpDevice::transmit(&mut service.router, Instant::ZERO)
                .unwrap()
                .consume(1, |packet| packet[0] = 0);
        }
        assert!(service.has_rx_backlog());

        let mut sockets = socket_set.inner.lock();
        assert_eq!(service.poll(&mut sockets), ServicePoll::Continuation);
        assert!(service.has_rx_backlog());

        assert_eq!(service.poll(&mut sockets), ServicePoll::Quiescent);
        assert!(!service.has_rx_backlog());
    }
}
