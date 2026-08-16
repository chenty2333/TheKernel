use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use smoltcp::{
    iface::SocketSet,
    phy::{DeviceCapabilities, Medium},
    storage::PacketMetadata,
    time::Instant,
    wire::{IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket},
};

use crate::{
    consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
    device::{Device, DeviceStats, InterfaceInfo, PacketSendProgress, RxStep},
    listen_table::ListenTable,
    packet::{
        PacketBroker, PacketDeviceCapabilities, PacketDeviceContext, PacketEndpoint, PacketError,
        PacketSendRequest,
    },
};

#[derive(Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
}

/// A point-in-time routing-table entry with a public interface index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteInfo {
    /// Destination prefix matched by this route.
    pub destination: IpCidr,
    /// Optional next-hop address.
    pub gateway: Option<IpAddress>,
    /// One-based index of the output interface.
    pub interface_index: u32,
    /// Source address selected for packets using this route.
    pub source: IpAddress,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
        }
    }
}

type PacketBuffer = smoltcp::storage::PacketBuffer<'static, ()>;

/// Maximum number of link frames admitted to one task-context receive pass.
pub const RX_PASS_BUDGET: usize = 32;

/// Maximum number of queued IP packets dispatched by one task-context egress
/// pass.
pub const EGRESS_PASS_BUDGET: usize = 32;

/// Result of one bounded router receive pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RxPass {
    Quiescent { consumed: usize, delivered: usize },
    Continuation { consumed: usize, delivered: usize },
}

impl RxPass {
    pub(crate) const fn consumed(self) -> usize {
        match self {
            Self::Quiescent { consumed, .. } | Self::Continuation { consumed, .. } => consumed,
        }
    }

    pub(crate) const fn delivered(self) -> usize {
        match self {
            Self::Quiescent { delivered, .. } | Self::Continuation { delivered, .. } => delivered,
        }
    }

    pub(crate) const fn is_continuation(self) -> bool {
        matches!(self, Self::Continuation { .. })
    }
}

/// Result of one bounded router egress pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EgressPass {
    Quiescent { dispatched: usize },
    Continuation { dispatched: usize },
}

impl EgressPass {
    pub(crate) const fn is_continuation(self) -> bool {
        matches!(self, Self::Continuation { .. })
    }
}

// TODO(mivik): optimize
pub struct RouteTable {
    rules: Vec<Rule>,
}
impl RouteTable {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        let idx = self
            .rules
            .partition_point(|it| it.filter.prefix_len() >= rule.filter.prefix_len());
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }
}

pub struct Router {
    rx_buffer: PacketBuffer,
    tx_buffer: PacketBuffer,
    mtu: usize,
    packet_broker: Arc<PacketBroker>,
    pub(crate) devices: Vec<Box<dyn Device>>,
    pub(crate) table: RouteTable,
    pub(crate) listen_table: Arc<ListenTable>,
    rx_cursor: usize,
}
impl Router {
    pub fn new_loopback_only(listen_table: Arc<ListenTable>) -> Self {
        Self::new_with_mtu(listen_table, LOOPBACK_MTU)
    }

    pub fn try_new_loopback_only(listen_table: Arc<ListenTable>) -> AxResult<Self> {
        let mut router = Self::try_new_with_mtu(listen_table, LOOPBACK_MTU)?;
        router
            .devices
            .try_reserve_exact(1)
            .map_err(|_| AxError::NoMemory)?;
        router
            .table
            .rules
            .try_reserve_exact(2)
            .map_err(|_| AxError::NoMemory)?;
        Ok(router)
    }

    fn new_with_mtu(listen_table: Arc<ListenTable>, mtu: usize) -> Self {
        Self::try_new_with_mtu(listen_table, mtu).expect("failed to allocate router packet queues")
    }

    fn try_new_with_mtu(listen_table: Arc<ListenTable>, mtu: usize) -> AxResult<Self> {
        let mut rx_metadata = Vec::new();
        rx_metadata
            .try_reserve_exact(PACKET_QUEUE_LEN)
            .map_err(|_| AxError::NoMemory)?;
        rx_metadata.resize(PACKET_QUEUE_LEN, PacketMetadata::EMPTY);
        let mut rx_storage = Vec::new();
        rx_storage
            .try_reserve_exact(mtu.saturating_mul(PACKET_QUEUE_LEN))
            .map_err(|_| AxError::NoMemory)?;
        rx_storage.resize(mtu.saturating_mul(PACKET_QUEUE_LEN), 0);
        let rx_buffer = PacketBuffer::new(rx_metadata, rx_storage);
        let mut tx_metadata = Vec::new();
        tx_metadata
            .try_reserve_exact(PACKET_QUEUE_LEN)
            .map_err(|_| AxError::NoMemory)?;
        tx_metadata.resize(PACKET_QUEUE_LEN, PacketMetadata::EMPTY);
        let mut tx_storage = Vec::new();
        tx_storage
            .try_reserve_exact(mtu.saturating_mul(PACKET_QUEUE_LEN))
            .map_err(|_| AxError::NoMemory)?;
        tx_storage.resize(mtu.saturating_mul(PACKET_QUEUE_LEN), 0);
        let tx_buffer = PacketBuffer::new(tx_metadata, tx_storage);
        let packet_broker = PacketBroker::try_new().map_err(map_packet_error)?;
        Ok(Self {
            rx_buffer,
            tx_buffer,
            mtu,
            packet_broker,
            devices: Vec::new(),
            table: RouteTable::new(),
            listen_table,
            rx_cursor: 0,
        })
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn add_device(&mut self, device: Box<dyn Device>) -> usize {
        self.devices.push(device);
        self.devices.len() - 1
    }

    pub(crate) fn has_rx_backlog(&self) -> bool {
        !self.rx_buffer.is_empty() || self.devices.iter().any(|device| device.has_rx_backlog())
    }

    pub(crate) fn has_rx_wake_capable_device(&self) -> bool {
        self.devices.iter().any(|device| device.rx_wake_capable())
    }

    pub(crate) fn register_rx_waker(
        &self,
        waker: &core::task::Waker,
    ) -> Result<(), axpoll::PollRegistrationError> {
        for device in &self.devices {
            if device.rx_wake_capable() {
                device.register_rx_waker(waker)?;
            }
        }
        Ok(())
    }

    pub(crate) fn stop_rx_waker(&self) {
        for device in &self.devices {
            if device.rx_wake_capable() {
                device.stop_rx_waker();
            }
        }
    }

    pub(crate) fn packet_broker(&self) -> Arc<PacketBroker> {
        Arc::clone(&self.packet_broker)
    }

    pub(crate) fn packet_device_capabilities(
        &self,
        interface_index: u32,
    ) -> Option<PacketDeviceCapabilities> {
        let index = usize::try_from(interface_index.checked_sub(1)?).ok()?;
        self.devices
            .get(index)
            .map(|device| device.packet_capabilities())
    }

    pub(crate) fn send_packet(
        &mut self,
        interface_index: u32,
        origin: &PacketEndpoint,
        request: PacketSendRequest<'_>,
        timestamp: Instant,
    ) -> AxResult<PacketSendProgress> {
        let origin = self
            .packet_broker
            .origin_id(origin)
            .map_err(map_packet_error)?;
        let index = usize::try_from(
            interface_index
                .checked_sub(1)
                .ok_or(AxError::InvalidInput)?,
        )
        .map_err(|_| AxError::InvalidInput)?;
        let device = self.devices.get_mut(index).ok_or(AxError::NotFound)?;
        let context =
            PacketDeviceContext::new(interface_index, self.packet_broker.as_ref(), Some(origin));
        device.send_packet(context, request, timestamp)
    }

    pub(crate) fn device_stats(&self) -> Vec<(String, DeviceStats)> {
        self.devices
            .iter()
            .map(|device| (device.name().into(), device.stats()))
            .collect()
    }

    pub(crate) fn interfaces(&self) -> Vec<InterfaceInfo> {
        self.devices
            .iter()
            .enumerate()
            .map(|(index, device)| InterfaceInfo {
                index: index as u32 + 1,
                name: device.name().into(),
                kind: device.interface_kind(),
                mtu: device.mtu(),
                hardware_address: device.hardware_address(),
                addresses: device.addresses(),
            })
            .collect()
    }

    pub(crate) fn routes(&self) -> Vec<RouteInfo> {
        self.table
            .rules
            .iter()
            .map(|rule| RouteInfo {
                destination: rule.filter,
                gateway: rule.via,
                interface_index: rule.dev as u32 + 1,
                source: rule.src,
            })
            .collect()
    }

    /// Consume at most [`RX_PASS_BUDGET`] link frames, starting at the
    /// persistent round-robin cursor.  A frame is counted even when the
    /// device drops it or handles it in ARP, so malformed traffic cannot
    /// bypass the task-context budget.
    pub fn poll(&mut self, timestamp: Instant) -> RxPass {
        let device_count = self.devices.len();
        if device_count == 0 {
            return RxPass::Quiescent {
                consumed: 0,
                delivered: 0,
            };
        }

        let mut consumed = 0;
        let mut delivered = 0;
        let mut idle_devices = 0;
        while consumed < RX_PASS_BUDGET && idle_devices < device_count {
            let index = self.rx_cursor % device_count;
            self.rx_cursor = (index + 1) % device_count;
            let context =
                PacketDeviceContext::new(index as u32 + 1, self.packet_broker.as_ref(), None);
            let step = self.devices[index].recv(context, &mut self.rx_buffer, timestamp);
            match step {
                RxStep::Idle => idle_devices += 1,
                RxStep::Consumed => {
                    consumed += 1;
                    idle_devices = 0;
                }
                RxStep::Delivered => {
                    consumed += 1;
                    delivered += 1;
                    idle_devices = 0;
                }
            }
        }

        if consumed == RX_PASS_BUDGET {
            RxPass::Continuation {
                consumed,
                delivered,
            }
        } else {
            RxPass::Quiescent {
                consumed,
                delivered,
            }
        }
    }

    /// Dispatch at most [`EGRESS_PASS_BUDGET`] queued IP packets.  The
    /// returned continuation also covers synchronous loopback ingress made by
    /// a send, so callers do not need to rely on another interrupt.
    pub fn dispatch(&mut self, timestamp: Instant) -> EgressPass {
        let mut poll_next = false;
        let mut dispatched = 0;
        while dispatched < EGRESS_PASS_BUDGET {
            let Ok(((), packet)) = self.tx_buffer.dequeue() else {
                break;
            };
            dispatched += 1;
            let Ok(version) = IpVersion::of_packet(packet) else {
                warn!("Dropping malformed IP packet from transmit queue");
                continue;
            };
            match version {
                IpVersion::Ipv4 => {
                    let Ok(packet) = smoltcp::wire::Ipv4Packet::new_checked(packet) else {
                        warn!("Dropping malformed IPv4 packet from transmit queue");
                        continue;
                    };
                    let dst_addr = IpAddress::Ipv4(packet.dst_addr());
                    if packet.dst_addr().is_broadcast() {
                        let buf = packet.into_inner();
                        for (index, dev) in self.devices.iter_mut().enumerate() {
                            let context = PacketDeviceContext::new(
                                index as u32 + 1,
                                self.packet_broker.as_ref(),
                                None,
                            );
                            poll_next |= dev.send(context, dst_addr, buf, timestamp);
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {dst_addr}");
                            continue;
                        };
                        let src_addr = IpAddress::Ipv4(packet.src_addr());
                        if rule.src != src_addr {
                            warn!(
                                "Dropping IPv4 packet to {} with mismatched source address: \
                                 expected {}, got {}",
                                dst_addr, rule.src, src_addr,
                            );
                            continue;
                        }

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let Some(dev) = self.devices.get_mut(rule.dev) else {
                            warn!("Dropping IPv4 packet for missing route device {}", rule.dev);
                            continue;
                        };
                        let context = PacketDeviceContext::new(
                            rule.dev as u32 + 1,
                            self.packet_broker.as_ref(),
                            None,
                        );
                        poll_next |= dev.send(context, next_hop, packet.into_inner(), timestamp);
                    }
                }
                IpVersion::Ipv6 => {
                    let Ok(packet) = smoltcp::wire::Ipv6Packet::new_checked(packet) else {
                        warn!("Dropping malformed IPv6 packet from transmit queue");
                        continue;
                    };
                    let dst_addr = IpAddress::Ipv6(packet.dst_addr());
                    if packet.dst_addr().is_multicast() {
                        let buf = packet.into_inner();
                        for (index, dev) in self.devices.iter_mut().enumerate() {
                            let context = PacketDeviceContext::new(
                                index as u32 + 1,
                                self.packet_broker.as_ref(),
                                None,
                            );
                            poll_next |= dev.send(context, dst_addr, buf, timestamp);
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {dst_addr}");
                            continue;
                        };
                        let src_addr = IpAddress::Ipv6(packet.src_addr());
                        if rule.src != src_addr {
                            warn!(
                                "Dropping IPv6 packet to {} with mismatched source address: \
                                 expected {}, got {}",
                                dst_addr, rule.src, src_addr,
                            );
                            continue;
                        }

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let Some(dev) = self.devices.get_mut(rule.dev) else {
                            warn!("Dropping IPv6 packet for missing route device {}", rule.dev);
                            continue;
                        };
                        let context = PacketDeviceContext::new(
                            rule.dev as u32 + 1,
                            self.packet_broker.as_ref(),
                            None,
                        );
                        poll_next |= dev.send(context, next_hop, packet.into_inner(), timestamp);
                    }
                }
            }
        }
        if poll_next || !self.tx_buffer.is_empty() {
            EgressPass::Continuation { dispatched }
        } else {
            EgressPass::Quiescent { dispatched }
        }
    }
}

fn map_packet_error(error: PacketError) -> AxError {
    match error {
        PacketError::Allocation => AxError::NoMemory,
        PacketError::InvalidInput => AxError::InvalidInput,
        PacketError::Detached => AxError::BadState,
        PacketError::SequenceExhausted => AxError::OutOfRange,
        PacketError::Capacity(_) => AxError::ResourceBusy,
    }
}

pub struct TxToken<'a>(&'a mut PacketBuffer);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self
            .0
            .enqueue(len, ())
            .expect("This was checked before creating the TxToken"))
    }
}

fn snoop_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>, listen_table: &ListenTable) {
    let Ok(version) = IpVersion::of_packet(buf) else {
        return;
    };
    let (protocol, src_addr, dst_addr, payload) = match version {
        IpVersion::Ipv4 => {
            let Ok(packet) = Ipv4Packet::new_checked(buf) else {
                return;
            };
            (
                packet.next_header(),
                IpAddress::Ipv4(packet.src_addr()),
                IpAddress::Ipv4(packet.dst_addr()),
                packet.payload(),
            )
        }
        IpVersion::Ipv6 => {
            let Ok(packet) = Ipv6Packet::new_checked(buf) else {
                return;
            };
            (
                packet.next_header(),
                IpAddress::Ipv6(packet.src_addr()),
                IpAddress::Ipv6(packet.dst_addr()),
                packet.payload(),
            )
        }
    };
    if protocol == IpProtocol::Tcp {
        let Ok(tcp_packet) = TcpPacket::new_checked(payload) else {
            return;
        };
        let src_addr = (src_addr, tcp_packet.src_port()).into();
        let dst_addr = (dst_addr, tcp_packet.dst_port()).into();
        let is_first = tcp_packet.syn() && !tcp_packet.ack();
        if is_first {
            listen_table.incoming_tcp_packet(src_addr, dst_addr, sockets);
        }
    }
}

pub struct RxToken<'a> {
    data: &'a [u8],
    listen_table: &'a ListenTable,
}

impl<'a> smoltcp::phy::RxToken for RxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.data)
    }

    fn preprocess(&self, sockets: &mut SocketSet) {
        snoop_tcp_packet(self.data, sockets, self.listen_table);
    }
}

impl smoltcp::phy::Device for Router {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_buffer.is_empty() || self.tx_buffer.is_full() {
            None
        } else {
            Some((
                RxToken {
                    data: self.rx_buffer.dequeue().unwrap().1,
                    listen_table: &self.listen_table,
                },
                TxToken(&mut self.tx_buffer),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx_buffer.is_full() {
            None
        } else {
            Some(TxToken(&mut self.tx_buffer))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(PACKET_QUEUE_LEN);
        caps
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::VecDeque, sync::Arc, vec};
    use core::task::Waker;
    use std::sync::Mutex;

    use smoltcp::{storage::PacketBuffer, wire::IpAddress};

    use super::*;
    use crate::packet::PacketDeviceContext;

    struct FakeDevice {
        name: &'static str,
        steps: VecDeque<RxStep>,
        seen: Arc<Mutex<Vec<u32>>>,
    }

    impl FakeDevice {
        fn new(
            name: &'static str,
            steps: impl IntoIterator<Item = RxStep>,
        ) -> (Self, Arc<Mutex<Vec<u32>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    name,
                    steps: steps.into_iter().collect(),
                    seen: seen.clone(),
                },
                seen,
            )
        }
    }

    impl Device for FakeDevice {
        fn name(&self) -> &str {
            self.name
        }

        fn stats(&self) -> DeviceStats {
            DeviceStats::default()
        }

        fn interface_kind(&self) -> crate::device::InterfaceKind {
            crate::device::InterfaceKind::Ethernet
        }

        fn mtu(&self) -> usize {
            LOOPBACK_MTU
        }

        fn recv(
            &mut self,
            context: PacketDeviceContext<'_>,
            buffer: &mut PacketBuffer<()>,
            _timestamp: Instant,
        ) -> RxStep {
            self.seen.lock().unwrap().push(context.interface_index());
            let Some(step) = self.steps.pop_front() else {
                return RxStep::Idle;
            };
            if step == RxStep::Delivered {
                let dst = buffer.enqueue(1, ()).unwrap();
                dst[0] = 0;
            }
            step
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

        fn register_waker(&self, _waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
            Ok(())
        }
    }

    fn router_with_devices(devices: impl IntoIterator<Item = Box<dyn Device>>) -> Router {
        let listen_table = Arc::new(ListenTable::new());
        let mut router = Router::new_loopback_only(listen_table);
        for device in devices {
            router.add_device(device);
        }
        router
    }

    #[test]
    fn receive_budget_counts_mixed_deliveries_and_drops() {
        let steps = (0..40).map(|index| {
            if index % 3 == 0 {
                RxStep::Delivered
            } else {
                RxStep::Consumed
            }
        });
        let (device, seen) = FakeDevice::new("fake0", steps);
        let mut router = router_with_devices([Box::new(device) as Box<dyn Device>]);

        let first = router.poll(Instant::ZERO);
        assert_eq!(first.consumed(), RX_PASS_BUDGET);
        assert_eq!(first.delivered(), 11);
        assert!(first.is_continuation());
        assert_eq!(seen.lock().unwrap().len(), RX_PASS_BUDGET);

        let second = router.poll(Instant::ZERO);
        assert_eq!(
            second,
            RxPass::Quiescent {
                consumed: 8,
                delivered: 3
            }
        );
        assert_eq!(seen.lock().unwrap().len(), 41);
    }

    #[test]
    fn software_receive_queue_counts_as_backlog_without_device_input() {
        let mut router = router_with_devices(core::iter::empty::<Box<dyn Device>>());
        assert!(!router.has_rx_backlog());

        let packet = router.rx_buffer.enqueue(1, ()).unwrap();
        packet[0] = 0;

        assert!(router.devices.is_empty());
        assert!(router.has_rx_backlog());
    }

    #[test]
    fn receive_round_robin_prevents_one_device_from_monopolizing_budget() {
        let (first, first_seen) = FakeDevice::new("first", vec![RxStep::Consumed; 40]);
        let (second, second_seen) = FakeDevice::new("second", vec![RxStep::Consumed; 40]);
        let mut router = router_with_devices([
            Box::new(first) as Box<dyn Device>,
            Box::new(second) as Box<dyn Device>,
        ]);

        assert!(router.poll(Instant::ZERO).is_continuation());
        assert_eq!(first_seen.lock().unwrap().len(), 16);
        assert_eq!(second_seen.lock().unwrap().len(), 16);

        // The persistent cursor keeps the same fairness on the next bounded
        // pass rather than restarting a device-local drain loop.
        assert!(router.poll(Instant::ZERO).is_continuation());
        assert_eq!(
            first_seen.lock().unwrap().len() + second_seen.lock().unwrap().len(),
            64
        );
        assert_eq!(first_seen.lock().unwrap().len(), 32);
        assert_eq!(second_seen.lock().unwrap().len(), 32);
    }
}
