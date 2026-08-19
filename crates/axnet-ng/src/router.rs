use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axpoll::PollRegistrationError;
use smoltcp::{
    iface::SocketSet,
    phy::{DeviceCapabilities, Medium},
    storage::PacketMetadata,
    time::Instant,
    wire::{IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket},
};

use crate::{
    consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
    device::{Device, DeviceStats, InterfaceInfo, PacketSendProgress, RxStep, RxWakeSource},
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

/// Maximum number of link devices owned by one router.  This is also the
/// largest device mask supported by the readiness and route paths.
pub const MAX_DEVICES: usize = 64;

/// Maximum number of device polls attempted by one ingress pass.  The cursor
/// makes a larger device set continue from the next device instead of turning
/// one task-context pass into an unbounded scan.
pub const DEVICE_PASS_BUDGET: usize = 32;

/// Maximum number of device sends performed by one broadcast/multicast
/// fan-out pass.  A pending fan-out retains its packet and cursor for the next
/// continuation.
pub const FANOUT_PASS_BUDGET: usize = 32;

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

/// Aggregate admission result for one permanent receive-worker arm pass.
///
/// Registration is attempted for every live device while the router owns the
/// device list. A failed source is fenced in-place, but a healthy source that
/// follows it still contributes an owner for the worker. The first error is
/// retained only as diagnostic/status information; it must not discard the
/// successful owners.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RxWakeRegistration {
    pub(crate) armed: usize,
    pub(crate) unavailable: usize,
    pub(crate) failed: usize,
    pub(crate) first_error: Option<PollRegistrationError>,
}

impl RxWakeRegistration {
    pub(crate) const fn has_owner(self) -> bool {
        self.armed != 0
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
    /// Persistent start point for broadcast/multicast fan-out.  Every pass
    /// visits a bounded slice of devices, but a device at the front of a long
    /// fan-out cannot monopolize the first send opportunity forever.
    tx_cursor: usize,
    /// Preallocated storage for a broadcast/multicast packet retained across
    /// fan-out continuations.
    fanout_buffer: Vec<u8>,
    fanout_len: usize,
    fanout_next_hop: Option<IpAddress>,
    fanout_start: usize,
    fanout_index: usize,
    fanout_device_count: usize,
    /// Receive wake sources that failed registration are fenced permanently
    /// for this stack.  A one-shot token cannot be safely retried after its
    /// owner has been consumed; retaining the bit makes that failure typed
    /// and keeps later worker passes from spinning on the same source.
    rx_wake_quarantine: u64,
    /// Devices that explicitly have no source suitable for the permanent
    /// worker (for example an Ethernet device without an IRQ binding). This
    /// is distinct from protocol quarantine: the device remains pollable and
    /// its lack of an IRQ must not make a healthy software source terminal.
    rx_wake_unavailable: u64,
    /// Quarantine bits already observed by a bounded service pass. The first
    /// observation of a newly fenced source is an edge; subsequent passes
    /// keep reporting the sticky quarantine level without replaying a
    /// readiness wake.
    rx_quarantine_observed: u64,
    rx_quarantine_edge: bool,
}
impl Router {
    pub fn new_loopback_only(listen_table: Arc<ListenTable>) -> Self {
        Self::new_with_mtu(listen_table, LOOPBACK_MTU)
    }

    pub fn try_new_loopback_only(listen_table: Arc<ListenTable>) -> AxResult<Self> {
        let mut router = Self::try_new_with_mtu(listen_table, LOOPBACK_MTU)?;
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
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(MAX_DEVICES)
            .map_err(|_| AxError::NoMemory)?;
        let mut fanout_buffer = Vec::new();
        fanout_buffer
            .try_reserve_exact(mtu)
            .map_err(|_| AxError::NoMemory)?;
        fanout_buffer.resize(mtu, 0);
        Ok(Self {
            rx_buffer,
            tx_buffer,
            mtu,
            packet_broker,
            devices,
            table: RouteTable::new(),
            listen_table,
            rx_cursor: 0,
            tx_cursor: 0,
            fanout_buffer,
            fanout_len: 0,
            fanout_next_hop: None,
            fanout_start: 0,
            fanout_index: 0,
            fanout_device_count: 0,
            rx_wake_quarantine: 0,
            rx_wake_unavailable: 0,
            rx_quarantine_observed: 0,
            rx_quarantine_edge: false,
        })
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn try_add_device(&mut self, device: Box<dyn Device>) -> AxResult<usize> {
        // A real receive ring has no safe task-context polling fallback in
        // this stack. Reject a no-IRQ device before it becomes visible in the
        // router, while software devices (loopback/veth) retain their bridge
        // based admission path.
        if device.rx_wake_required() && !device.rx_wake_capable() {
            return Err(AxError::Unsupported);
        }
        if self.devices.len() >= MAX_DEVICES {
            return Err(AxError::ResourceBusy);
        }
        self.devices.push(device);
        Ok(self.devices.len() - 1)
    }

    pub(crate) fn has_device_capacity(&self) -> bool {
        self.devices.len() < MAX_DEVICES
    }

    pub fn add_device(&mut self, device: Box<dyn Device>) -> usize {
        self.try_add_device(device)
            .expect("network router device admission failed")
    }

    pub(crate) fn has_rx_backlog(&self) -> bool {
        !self.rx_buffer.is_empty()
            || self.devices.iter().enumerate().any(|(index, device)| {
                !self.rx_wake_quarantined(index)
                    && !device.is_quarantined()
                    && device.has_rx_backlog()
            })
    }

    /// Reports a terminal link-device or receive-source quarantine. The
    /// router must surface this state to the service instead of asking a
    /// fenced source for another completion.
    pub(crate) fn has_quarantined_device(&mut self) -> bool {
        let mut current = self.rx_wake_quarantine;
        for (index, device) in self.devices.iter().enumerate() {
            if index < 64 && device.is_quarantined() {
                current |= 1u64 << index;
            }
        }
        let new_sources = current & !self.rx_quarantine_observed;
        if new_sources != 0 {
            self.rx_quarantine_observed |= new_sources;
            self.rx_quarantine_edge = true;
        }
        current != 0
    }

    /// Consumes the one-shot readiness edge for a newly observed quarantine.
    /// The level remains visible through [`has_quarantined_device`], but an
    /// idle healthy source must be allowed to park after the edge is replayed.
    pub(crate) fn take_quarantine_edge(&mut self) -> bool {
        let edge = self.rx_quarantine_edge;
        self.rx_quarantine_edge = false;
        edge
    }

    pub(crate) fn tx_queue_full(&self) -> bool {
        self.tx_buffer.is_full()
    }

    pub(crate) fn has_rx_wake_capable_device(&self) -> bool {
        self.devices.iter().enumerate().any(|(index, device)| {
            !self.rx_wake_quarantined(index) && !device.is_quarantined() && device.rx_wake_capable()
        })
    }

    #[inline]
    fn rx_wake_quarantined(&self, index: usize) -> bool {
        index < 64 && self.rx_wake_quarantine & (1u64 << index) != 0
    }

    #[inline]
    fn rx_wake_unavailable(&self, index: usize) -> bool {
        index < 64 && self.rx_wake_unavailable & (1u64 << index) != 0
    }

    pub(crate) fn register_rx_waker(&mut self, waker: &core::task::Waker) -> RxWakeRegistration {
        // The dedicated worker owns every source in a physical stack,
        // including software bridges (loopback/veth). Quarantined devices
        // and sources that already failed one-shot rearm must not be
        // re-armed.  Continue the walk after an error so healthy software
        // sources still acquire the worker owner in this same transaction.
        let mut result = RxWakeRegistration {
            armed: 0,
            unavailable: 0,
            failed: 0,
            first_error: None,
        };
        for index in 0..self.devices.len() {
            let device = &self.devices[index];
            if self.rx_wake_quarantined(index)
                || self.rx_wake_unavailable(index)
                || device.is_quarantined()
            {
                continue;
            }
            match device.register_rx_waker(waker) {
                Ok(RxWakeSource::Armed) => result.armed += 1,
                Ok(RxWakeSource::Unavailable) => {
                    // No source was consumed, so this is not a protocol
                    // quarantine. Remember it to keep later worker passes
                    // bounded while leaving task-context polling available.
                    self.rx_wake_unavailable |= 1u64 << index;
                    result.unavailable += 1;
                }
                Err(error) => {
                    // Registration failure consumes the source's one-shot
                    // admission path. Mask/cancel it while the device is
                    // still owned, then retain the exact index as a terminal
                    // source quarantine so no later pass retries it. Continue
                    // walking: healthy NIC/software sources still acquire the
                    // worker owner in this same transaction.
                    device.stop_rx_waker();
                    let bit = 1u64 << index;
                    self.rx_wake_quarantine |= bit;
                    // Rearm failure is discovered while the worker is in its
                    // check-arm-check window, before the next Service::poll
                    // can observe the sticky level. Publish the edge now;
                    // the worker will consume it once and replay it below.
                    if self.rx_quarantine_observed & bit == 0 {
                        self.rx_quarantine_observed |= bit;
                        self.rx_quarantine_edge = true;
                    }
                    result.failed += 1;
                    result.first_error.get_or_insert(error);
                }
            }
        }
        result
    }

    pub(crate) fn stop_rx_waker(&self) {
        for device in &self.devices {
            device.stop_rx_waker();
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
        let mut attempts = 0;
        while consumed < RX_PASS_BUDGET
            && attempts < DEVICE_PASS_BUDGET
            && idle_devices < device_count
        {
            attempts += 1;
            let index = self.rx_cursor % device_count;
            self.rx_cursor = (index + 1) % device_count;
            let context =
                PacketDeviceContext::new(index as u32 + 1, self.packet_broker.as_ref(), None);
            let step = if self.rx_wake_quarantined(index) || self.devices[index].is_quarantined() {
                // A fenced queue is terminal for this device only. Do not
                // touch its used ring, but continue the bounded pass so
                // healthy interfaces remain serviceable.
                RxStep::Idle
            } else {
                self.devices[index].recv(context, &mut self.rx_buffer, timestamp)
            };
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

        // A scan that only observed idle/fenced devices is quiescent even
        // when the fixed device budget stopped before the end of the vector.
        // Returning continuation for an all-idle large device set would make
        // the receive worker yield forever with no source capable of waking
        // useful work.  A productive bounded slice is continued; the
        // persistent cursor lets the next pass visit the remaining devices.
        if consumed == RX_PASS_BUDGET
            || (attempts == DEVICE_PASS_BUDGET && attempts < device_count && consumed != 0)
        {
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
        let mut fanout_budget = FANOUT_PASS_BUDGET;

        // Finish a fan-out retained by the previous pass before dequeuing a
        // new packet.  The packet storage and cursor are router-owned, so no
        // descriptor or unbounded temporary allocation is needed.
        let (fanout_poll, fanout_pending) = self.drain_fanout(timestamp, &mut fanout_budget);
        poll_next |= fanout_poll;
        if fanout_pending {
            return EgressPass::Continuation { dispatched };
        }

        while dispatched < EGRESS_PASS_BUDGET {
            // A pass that exhausted its device-send budget must leave the
            // software queue intact for the next continuation.  Unicast work
            // is deliberately bounded by the same pass boundary after a
            // fan-out, keeping the total device work predictable.
            if fanout_budget == 0 {
                break;
            }
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
                        let fanout_len = {
                            let packet = packet.into_inner();
                            let len = packet.len();
                            if self.devices.is_empty() || len > self.fanout_buffer.len() {
                                None
                            } else {
                                self.fanout_buffer[..len].copy_from_slice(packet);
                                Some(len)
                            }
                        };
                        if let Some(fanout_len) = fanout_len
                            && self.begin_fanout(dst_addr, fanout_len)
                        {
                            let (fanout_poll, fanout_pending) =
                                self.drain_fanout(timestamp, &mut fanout_budget);
                            poll_next |= fanout_poll;
                            if fanout_pending {
                                return EgressPass::Continuation { dispatched };
                            }
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
                        if dev.is_quarantined() {
                            continue;
                        }
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
                        let fanout_len = {
                            let packet = packet.into_inner();
                            let len = packet.len();
                            if self.devices.is_empty() || len > self.fanout_buffer.len() {
                                None
                            } else {
                                self.fanout_buffer[..len].copy_from_slice(packet);
                                Some(len)
                            }
                        };
                        if let Some(fanout_len) = fanout_len
                            && self.begin_fanout(dst_addr, fanout_len)
                        {
                            let (fanout_poll, fanout_pending) =
                                self.drain_fanout(timestamp, &mut fanout_budget);
                            poll_next |= fanout_poll;
                            if fanout_pending {
                                return EgressPass::Continuation { dispatched };
                            }
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
                        if dev.is_quarantined() {
                            continue;
                        }
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
        if poll_next || !self.tx_buffer.is_empty() || self.fanout_len != 0 {
            EgressPass::Continuation { dispatched }
        } else {
            EgressPass::Quiescent { dispatched }
        }
    }

    fn begin_fanout(&mut self, next_hop: IpAddress, packet_len: usize) -> bool {
        let device_count = self.devices.len();
        if device_count == 0 || packet_len > self.fanout_buffer.len() {
            return false;
        }
        self.fanout_len = packet_len;
        self.fanout_next_hop = Some(next_hop);
        self.fanout_start = self.tx_cursor % device_count;
        self.fanout_index = 0;
        self.fanout_device_count = device_count;
        true
    }

    fn drain_fanout(&mut self, timestamp: Instant, budget: &mut usize) -> (bool, bool) {
        let Some(next_hop) = self.fanout_next_hop else {
            return (false, false);
        };
        let device_count = self.fanout_device_count.min(self.devices.len());
        if device_count == 0 {
            self.fanout_len = 0;
            self.fanout_next_hop = None;
            self.fanout_index = 0;
            self.fanout_device_count = 0;
            return (false, false);
        }
        let start = self.fanout_start % device_count;
        let mut index = self.fanout_index;
        let mut poll_next = false;
        let packet_len = self.fanout_len;
        let packet_broker = self.packet_broker.as_ref();
        let packet = &self.fanout_buffer[..packet_len];
        let devices = &mut self.devices;
        while index < device_count && *budget != 0 {
            let device_index = (start + index) % device_count;
            if !devices[device_index].is_quarantined() {
                let context =
                    PacketDeviceContext::new(device_index as u32 + 1, packet_broker, None);
                poll_next |= devices[device_index].send(context, next_hop, packet, timestamp);
            }
            index += 1;
            *budget -= 1;
        }
        self.fanout_index = index;
        if index == device_count {
            self.tx_cursor = (start + 1) % self.devices.len();
            self.fanout_len = 0;
            self.fanout_next_hop = None;
            self.fanout_index = 0;
            self.fanout_device_count = 0;
            (poll_next, false)
        } else {
            (poll_next, true)
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
            context: PacketDeviceContext<'_>,
            _next_hop: IpAddress,
            _packet: &[u8],
            _timestamp: Instant,
        ) -> bool {
            self.seen.lock().unwrap().push(context.interface_index());
            false
        }

        fn register_waker(&self, _waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
            Ok(())
        }
    }

    /// Represents a real RX-ring device whose driver did not expose an IRQ.
    /// It intentionally has no polling fallback: admission must reject it
    /// before the router publishes an interface with no consumer.
    struct NoIrqRxRingDevice;

    impl Device for NoIrqRxRingDevice {
        fn name(&self) -> &str {
            "no-irq-rx"
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

        fn rx_wake_required(&self) -> bool {
            true
        }

        fn recv(
            &mut self,
            _context: PacketDeviceContext<'_>,
            _buffer: &mut PacketBuffer<()>,
            _timestamp: Instant,
        ) -> RxStep {
            RxStep::Idle
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
    fn no_irq_rx_ring_is_rejected_before_publication() {
        let listen_table = Arc::new(ListenTable::new());
        let mut router = Router::new_loopback_only(listen_table);

        assert_eq!(
            router.try_add_device(Box::new(NoIrqRxRingDevice)),
            Err(AxError::Unsupported)
        );
        assert!(router.devices.is_empty());

        // Software devices continue to use their bridge-based path and are
        // not mistaken for a physical RX ring merely because they are
        // Ethernet-shaped interfaces.
        let (software, _) = FakeDevice::new("software", core::iter::empty());
        assert_eq!(router.try_add_device(Box::new(software)), Ok(0));
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

    #[test]
    fn device_scan_budget_does_not_spin_on_an_idle_full_device_set() {
        let mut router = router_with_devices((0..MAX_DEVICES).map(|_| {
            let (device, _) = FakeDevice::new("full", core::iter::empty());
            Box::new(device) as Box<dyn Device>
        }));

        assert_eq!(
            router.poll(Instant::ZERO),
            RxPass::Quiescent {
                consumed: 0,
                delivered: 0
            }
        );
        assert_eq!(
            router.poll(Instant::ZERO),
            RxPass::Quiescent {
                consumed: 0,
                delivered: 0
            }
        );
        assert_eq!(router.devices.len(), MAX_DEVICES);
    }

    #[test]
    fn broadcast_fanout_is_bounded_and_resumed_by_cursor() {
        let mut seen = Vec::new();
        let mut router = router_with_devices((0..MAX_DEVICES).map(|_| {
            let (device, device_seen) = FakeDevice::new("fanout", core::iter::empty());
            seen.push(device_seen);
            Box::new(device) as Box<dyn Device>
        }));

        let packet = router.tx_buffer.enqueue(20, ()).unwrap();
        packet.fill(0);
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&20u16.to_be_bytes());
        packet[8] = 64;
        packet[16..20].fill(0xff);

        assert_eq!(
            router.dispatch(Instant::ZERO),
            EgressPass::Continuation { dispatched: 1 }
        );
        assert_eq!(
            seen.iter()
                .map(|device| device.lock().unwrap().len())
                .sum::<usize>(),
            FANOUT_PASS_BUDGET
        );
        assert_eq!(
            router.dispatch(Instant::ZERO),
            EgressPass::Quiescent { dispatched: 0 }
        );
        assert_eq!(
            seen.iter()
                .map(|device| device.lock().unwrap().len())
                .sum::<usize>(),
            MAX_DEVICES
        );
    }
}
