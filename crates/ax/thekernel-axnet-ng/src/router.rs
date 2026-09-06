use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};

use axerrno::{AxError, AxResult};
use axpoll::PollRegistrationError;
use smoltcp::{
    iface::SocketSet,
    phy::{DeviceCapabilities, Medium, PacketMeta},
    storage::PacketMetadata,
    time::Instant,
    wire::{
        IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket,
    },
};

use crate::{
    consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
    device::{
        Device, DeviceStats, IngressPacketBuffer, InterfaceInfo, PacketSendProgress, RxStep,
        RxWakeSource,
    },
    fragment::{FragmentReassembler, ReassemblyOutcome},
    listen_table::ListenTable,
    packet::{
        PacketBroker, PacketChecksumContext, PacketDeviceCapabilities, PacketDeviceContext,
        PacketEndpoint, PacketError, PacketSendRequest,
    },
};

/// Linux-facing packet traversal points.  The generic router deliberately
/// knows no policy language; an owning OS may install one namespace-local
/// hook which can inspect, rewrite, or reject a complete IP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketHookPoint {
    Prerouting,
    Input,
    Forward,
    LocalOutput,
    Postrouting,
}

/// Immutable ingress/egress facts carried with one policy decision.  This is
/// copied out of the router's private device context so policy consumers never
/// retain a device lock or reconstruct checksum assumptions from packet bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketContext {
    pub point: PacketHookPoint,
    /// Stable Linux interface index when the seam is associated with a
    /// concrete device.  `0` denotes local output before route selection.
    pub ifindex: u32,
    pub protocol: Option<IpProtocol>,
    pub ingress: bool,
    /// The device checksum contract at this seam.  Policy which rewrites an
    /// IP/L4 header can make an informed choice between preserving hardware
    /// offload and forcing a later software checksum repair.
    pub checksum: PacketChecksumContext,
}

/// The only outcomes a policy hook can publish.  Keeping accept/drop as a
/// typed result rather than an error lets a caller distinguish policy drop
/// from a broken policy provider and leaves room for device/socket redirect
/// actions without changing the hook ABI again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketAction {
    Pass,
    Drop,
    RedirectConsumed,
    Tx,
}

/// A fallible mutable packet hook.  It is called under the stack service lock,
/// so policy publication and packet traversal have a single serialization
/// boundary without a global router hook.
pub type PacketHook =
    Arc<dyn Fn(&PacketContext, &mut [u8]) -> AxResult<PacketAction> + Send + Sync>;
pub type PreProtocolHook = Arc<dyn Fn(u32, &[u8]) -> AxResult<PacketAction> + Send + Sync>;
/// Policy asks for reassembly at a particular traversal seam.  The router
/// stays policy-agnostic: this is a pure admission query and never runs a
/// BPF/nft program against partial data.
pub type PacketDefragQuery = Arc<dyn Fn(PacketHookPoint, &[u8]) -> bool + Send + Sync>;

#[derive(Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    /// Stable namespace-local ifindex, never a storage-vector position.
    pub dev: u32,
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
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: u32, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
        }
    }
}

/// Router queue metadata. `RawRoute` is an opaque, one-use handle allocated
/// only after raw send has selected a concrete route.  It is deliberately not
/// a packet fingerprint: two byte-identical raw datagrams can coexist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TxRouteMetadata {
    Normal,
    RawRoute(u32),
}

type PacketBuffer = smoltcp::storage::PacketBuffer<'static, TxRouteMetadata>;

#[derive(Clone)]
pub(crate) struct RawRoutePlan {
    pub(crate) handle: u32,
    pub(crate) destination: IpAddress,
    pub(crate) source: IpAddress,
    pub(crate) next_hop: IpAddress,
    pub(crate) ifindex: u32,
    /// Admission controls must survive until deferred egress: LOCAL_OUT may
    /// change the destination and require a second lookup.
    pub(crate) dont_route: bool,
    pub(crate) bound_source: Option<IpAddress>,
    pub(crate) header_included: bool,
    pub(crate) completion: Weak<crate::raw::RawRouteCompletion>,
}

struct RawRouteSlot {
    handle: u32,
    plan: RawRoutePlan,
}

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

    /// Remove one exact forwarding rule.  Route mutation is deliberately an
    /// exact operation: a netlink delete must not silently remove a more
    /// specific rule that merely happens to cover the same destination.
    pub fn remove_rule(&mut self, rule: &Rule) -> bool {
        let Some(index) = self.rules.iter().position(|candidate| {
            candidate.filter == rule.filter
                && candidate.via == rule.via
                && candidate.dev == rule.dev
                && candidate.src == rule.src
        }) else {
            return false;
        };
        self.rules.remove(index);
        true
    }

    /// Replace the router's single modelled unicast route for a destination.
    /// Linux has a richer priority/metric key; those
    /// attributes are not represented by this stack and are consequently not
    /// accepted by its netlink adapter.
    pub fn replace_rule(&mut self, rule: Rule) {
        self.rules
            .retain(|candidate| candidate.filter != rule.filter);
        self.add_rule(rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }
}

pub struct Router {
    rx_buffer: IngressPacketBuffer,
    tx_buffer: PacketBuffer,
    mtu: usize,
    packet_broker: Arc<PacketBroker>,
    pub(crate) devices: Vec<Box<dyn Device>>,
    ifindices: Vec<u32>,
    /// Administrative state is owned next to the device topology, rather than
    /// reconstructed by the netlink adapter.  This makes an ifindex stable
    /// across rename/flag changes and lets the datapath consult the same UP
    /// bit which RTM_GETLINK reports.
    links: Vec<LinkState>,
    next_ifindex: u32,
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
    packet_hook: Option<PacketHook>,
    pre_protocol_hook: Option<PreProtocolHook>,
    packet_defrag_query: Option<PacketDefragQuery>,
    /// Namespace-local reassembly is used only for hooks which explicitly
    /// asked for it; a non-defrag netfilter link still sees original
    /// fragments. It is router-owned so queues never cross namespaces.
    fragment_reassembler: FragmentReassembler,
    /// The receive queue yields an immutable slice.  Keep one bounded router
    /// scratch packet so PREROUTING/INPUT can rewrite packets before smoltcp
    /// receives them without allocating in the receive path.
    ingress_packet: Vec<u8>,
    /// One-use plans keyed by monotonically allocated handles. Handles are
    /// never reused (wrap is rejected), so an old queued packet can never
    /// acquire a newer route plan through ABA.
    raw_routes: Vec<RawRouteSlot>,
    next_raw_route: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct LinkState {
    pub(crate) name: String,
    pub(crate) mtu: usize,
    pub(crate) up: bool,
    pub(crate) peer: Option<u32>,
}
impl Router {
    pub fn try_new_loopback_only(listen_table: Arc<ListenTable>) -> AxResult<Self> {
        let mut router = Self::try_new_with_mtu(listen_table, LOOPBACK_MTU)?;
        router
            .table
            .rules
            .try_reserve_exact(2)
            .map_err(|_| AxError::NoMemory)?;
        Ok(router)
    }

    fn try_new_with_mtu(listen_table: Arc<ListenTable>, mtu: usize) -> AxResult<Self> {
        let mut rx_metadata: Vec<PacketMetadata<u32>> = Vec::new();
        rx_metadata
            .try_reserve_exact(PACKET_QUEUE_LEN)
            .map_err(|_| AxError::NoMemory)?;
        rx_metadata.resize(PACKET_QUEUE_LEN, PacketMetadata::EMPTY);
        let mut rx_storage = Vec::new();
        rx_storage
            .try_reserve_exact(mtu.saturating_mul(PACKET_QUEUE_LEN))
            .map_err(|_| AxError::NoMemory)?;
        rx_storage.resize(mtu.saturating_mul(PACKET_QUEUE_LEN), 0);
        let rx_buffer = IngressPacketBuffer::new(rx_metadata, rx_storage);
        let mut tx_metadata: Vec<PacketMetadata<TxRouteMetadata>> = Vec::new();
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
        let mut ifindices = Vec::new();
        ifindices
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
            ifindices,
            links: Vec::new(),
            next_ifindex: 1,
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
            packet_hook: None,
            pre_protocol_hook: None,
            packet_defrag_query: None,
            fragment_reassembler: FragmentReassembler::try_new()?,
            // No jumbograms: IPv6 can nevertheless carry 65535 bytes after
            // its fixed header, so its ordinary maximum is 65575.
            ingress_packet: vec![0; 40 + u16::MAX as usize],
            raw_routes: Vec::new(),
            next_raw_route: 1,
        })
    }

    pub(crate) fn set_packet_hook(&mut self, hook: Option<PacketHook>) {
        self.packet_hook = hook;
    }
    pub(crate) fn set_pre_protocol_hook(&mut self, hook: Option<PreProtocolHook>) {
        self.pre_protocol_hook = hook;
    }

    pub(crate) fn set_packet_defrag_query(&mut self, query: Option<PacketDefragQuery>) {
        self.packet_defrag_query = query;
    }

    #[inline]
    fn packet_context(
        &self,
        point: PacketHookPoint,
        ifindex: u32,
        ingress: bool,
        packet: &[u8],
    ) -> PacketContext {
        let checksum = self
            .device_slot(ifindex)
            .map(|slot| self.devices[slot].packet_checksum_context())
            .unwrap_or(PacketChecksumContext::UNKNOWN);
        PacketContext {
            point,
            ifindex,
            protocol: ip_protocol(packet),
            ingress,
            checksum,
        }
    }

    #[inline]
    fn apply_hook(&self, context: PacketContext, packet: &mut [u8]) -> bool {
        let Some(hook) = self.packet_hook.as_ref() else {
            return true;
        };
        // IPv4 permits an all-zero UDP checksum. Preserve that wire-level
        // choice across an accepting policy traversal; a filter that merely
        // observes the packet must not manufacture a checksum.
        let preserve_ipv4_udp_zero = ipv4_udp_checksum_is_zero(packet);
        let accepted = matches!(hook(&context, packet), Ok(PacketAction::Pass));
        if accepted {
            repair_software_checksums(packet, context.checksum, preserve_ipv4_udp_zero);
        }
        accepted
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    /// Atomically admit a forwarding rule against the current device set.
    /// The router stores zero-based device slots internally; callers exposing
    /// Linux ifindex values translate them before entering this mechanism.
    pub fn try_add_rule(&mut self, rule: Rule) -> AxResult {
        if self.device_slot(rule.dev).is_none() {
            return Err(AxError::NoSuchDevice);
        }
        let family_matches = matches!(
            (rule.filter.address(), rule.via, rule.src),
            (IpAddress::Ipv4(_), None, IpAddress::Ipv4(_))
                | (
                    IpAddress::Ipv4(_),
                    Some(IpAddress::Ipv4(_)),
                    IpAddress::Ipv4(_)
                )
                | (IpAddress::Ipv6(_), None, IpAddress::Ipv6(_))
                | (
                    IpAddress::Ipv6(_),
                    Some(IpAddress::Ipv6(_)),
                    IpAddress::Ipv6(_)
                )
        );
        if !family_matches {
            return Err(AxError::InvalidInput);
        }
        // This router deliberately has no metric/priority model. Publishing
        // two equal-prefix routes would make lookup order, rather than an
        // explicit Linux route key, decide forwarding; reject that request
        // instead of claiming multipath semantics we do not implement.
        if self
            .table
            .rules
            .iter()
            .any(|candidate| candidate.filter == rule.filter)
        {
            return Err(AxError::AlreadyExists);
        }
        self.table.add_rule(rule);
        Ok(())
    }

    /// Remove one exact routing rule.  It is important that deletion goes
    /// through the same router lock as lookup/egress so no packet observes a
    /// partially changed table.
    pub fn remove_rule(&mut self, rule: &Rule) -> AxResult {
        if self.table.remove_rule(rule) {
            Ok(())
        } else {
            Err(AxError::NotFound)
        }
    }

    pub fn replace_rule(&mut self, rule: Rule) -> AxResult {
        if self.device_slot(rule.dev).is_none() {
            return Err(AxError::NoSuchDevice);
        }
        let family_matches = matches!(
            (rule.filter.address(), rule.via, rule.src),
            (IpAddress::Ipv4(_), None, IpAddress::Ipv4(_))
                | (
                    IpAddress::Ipv4(_),
                    Some(IpAddress::Ipv4(_)),
                    IpAddress::Ipv4(_)
                )
                | (IpAddress::Ipv6(_), None, IpAddress::Ipv6(_))
                | (
                    IpAddress::Ipv6(_),
                    Some(IpAddress::Ipv6(_)),
                    IpAddress::Ipv6(_)
                )
        );
        if !family_matches {
            return Err(AxError::InvalidInput);
        }
        self.table.replace_rule(rule);
        Ok(())
    }

    pub fn try_add_device(&mut self, device: Box<dyn Device>) -> AxResult<u32> {
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
        let name = String::from(device.name());
        if name.is_empty() || self.links.iter().any(|link| link.name == name) {
            return Err(AxError::AlreadyExists);
        }
        // Allocate every publication vector before consuming an ifindex or
        // making a device visible. This is the router half of transactional
        // TUNSETIFF/RTM_NEWLINK admission.
        self.devices.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        self.ifindices
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let ifindex = self.next_ifindex;
        if ifindex == 0 {
            return Err(AxError::OutOfRange);
        }
        self.next_ifindex = self.next_ifindex.checked_add(1).unwrap_or(0);
        let mtu = device.mtu();
        self.devices.push(device);
        self.ifindices.push(ifindex);
        self.links.push(LinkState {
            name,
            mtu,
            up: true,
            peer: None,
        });
        Ok(ifindex)
    }

    pub(crate) fn has_device_capacity(&self) -> bool {
        self.devices.len() < MAX_DEVICES
    }

    /// Resolve a stable public ifindex to the transient storage slot used by
    /// the polling engine.
    pub(crate) fn device_slot(&self, ifindex: u32) -> Option<usize> {
        self.ifindices
            .iter()
            .position(|candidate| *candidate == ifindex)
    }

    /// Unpublish a device and revoke its routes as one router transaction.
    /// A retained multicast fanout is slot-indexed, so it is discarded rather
    /// than accidentally delivered to a shifted successor slot.
    pub fn remove_device(&mut self, ifindex: u32) -> AxResult {
        let peer = self
            .device_slot(ifindex)
            .and_then(|slot| self.links.get(slot))
            .and_then(|link| link.peer);
        self.remove_device_single(ifindex)?;
        if let Some(peer) = peer {
            // A veth endpoint's lifetime is paired.  Resolve its stable
            // ifindex after removing the first slot, rather than retaining a
            // storage-vector position which removal invalidates.
            self.remove_device_single(peer)?;
        }
        Ok(())
    }

    fn remove_device_single(&mut self, ifindex: u32) -> AxResult {
        self.prune_dead_raw_routes();
        // A queued tokenized raw datagram owns this interface selection.  Do
        // not turn a successful NOWAIT send into a later silent drop: make
        // topology mutation retry after the deferred egress consumes it.
        if self
            .raw_routes
            .iter()
            .any(|slot| slot.plan.ifindex == ifindex)
        {
            return Err(AxError::ResourceBusy);
        }
        let slot = self.device_slot(ifindex).ok_or(AxError::NoSuchDevice)?;
        self.devices[slot].stop_rx_waker();
        self.devices.remove(slot);
        self.ifindices.remove(slot);
        self.links.remove(slot);
        self.table.rules.retain(|rule| rule.dev != ifindex);
        self.rx_cursor = if self.devices.is_empty() {
            0
        } else {
            self.rx_cursor % self.devices.len()
        };
        self.tx_cursor = if self.devices.is_empty() {
            0
        } else {
            self.tx_cursor % self.devices.len()
        };
        self.fanout_len = 0;
        self.fanout_next_hop = None;
        self.fanout_index = 0;
        self.fanout_device_count = 0;
        Ok(())
    }

    /// Reclaim plans whose raw socket parser has terminally discarded their
    /// exact queue metadata. Called on every bounded service egress pass, so
    /// malformed traffic cannot accumulate stale route leases until a later
    /// topology mutation happens to visit this router.
    pub(crate) fn prune_dead_raw_routes(&mut self) {
        self.raw_routes.retain(|slot| {
            slot.plan
                .completion
                .upgrade()
                .is_some_and(|completion| completion.owns_handle(slot.plan.handle))
        });
    }

    pub(crate) fn connect_veth_pair(&mut self, left: u32, right: u32) -> AxResult {
        if left == right {
            return Err(AxError::InvalidInput);
        }
        let left_slot = self.device_slot(left).ok_or(AxError::NoSuchDevice)?;
        let right_slot = self.device_slot(right).ok_or(AxError::NoSuchDevice)?;
        if self.links[left_slot].peer.is_some() || self.links[right_slot].peer.is_some() {
            return Err(AxError::BadState);
        }
        self.links[left_slot].peer = Some(right);
        self.links[right_slot].peer = Some(left);
        Ok(())
    }

    pub(crate) fn has_rx_backlog(&self) -> bool {
        !self.rx_buffer.is_empty()
            || self.devices.iter().enumerate().any(|(index, device)| {
                !self.rx_wake_quarantined(index)
                    && !device.is_quarantined()
                    && self.links[index].up
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

    /// Reserve an exact raw-IP route plan before its socket entry is queued.
    /// The caller holds the service transaction while invoking this and the
    /// matching smoltcp queue publication, so failure cannot leave an orphan
    /// plan. A handle is never recycled; queue discard and dispatch consume it.
    pub(crate) fn reserve_raw_route(&mut self, plan: RawRoutePlan) -> AxResult<u32> {
        self.raw_routes
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        let handle = self.next_raw_route;
        if handle == 0 {
            return Err(AxError::OutOfRange);
        }
        self.next_raw_route = self.next_raw_route.checked_add(1).unwrap_or(0);
        let mut plan = plan;
        plan.handle = handle;
        self.raw_routes.push(RawRouteSlot { handle, plan });
        Ok(handle)
    }

    pub(crate) fn discard_raw_route(&mut self, handle: u32) {
        if let Some(index) = self
            .raw_routes
            .iter()
            .position(|slot| slot.handle == handle)
        {
            self.raw_routes.swap_remove(index);
        }
    }

    fn take_raw_route(&mut self, handle: u32) -> Option<RawRoutePlan> {
        let index = self
            .raw_routes
            .iter()
            .position(|slot| slot.handle == handle)?;
        Some(self.raw_routes.swap_remove(index).plan)
    }

    pub(crate) fn has_rx_wake_capable_device(&self) -> bool {
        self.devices.iter().enumerate().any(|(index, device)| {
            !self.rx_wake_quarantined(index)
                && !device.is_quarantined()
                && self.links[index].up
                && device.rx_wake_capable()
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
                || !self.links[index].up
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
        let index = self.device_slot(interface_index)?;
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
        let origin = Some(
            self.packet_broker
                .origin_id(origin)
                .map_err(map_packet_error)?,
        );
        self.send_packet_from(interface_index, origin, request, timestamp)
    }

    /// Sends a packet with an optional packet-capture origin.  Kernel-owned
    /// producers such as AF_XDP have no capture endpoint of their own, but
    /// must still traverse the exact device transmit path (and expose the
    /// frame to ordinary packet observers) without fabricating one.
    pub(crate) fn send_packet_from(
        &mut self,
        interface_index: u32,
        origin: Option<crate::packet::PacketEndpointId>,
        request: PacketSendRequest<'_>,
        timestamp: Instant,
    ) -> AxResult<PacketSendProgress> {
        let index = self.device_slot(interface_index).ok_or(AxError::NotFound)?;
        let device = self.devices.get_mut(index).ok_or(AxError::NotFound)?;
        let context =
            PacketDeviceContext::new(interface_index, self.packet_broker.as_ref(), origin);
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
                index: self.ifindices[index],
                name: self.links[index].name.clone(),
                kind: device.interface_kind(),
                mtu: self.links[index].mtu,
                administrative_up: self.links[index].up,
                hardware_address: device.hardware_address(),
                addresses: device.addresses(),
            })
            .collect()
    }

    /// Apply a complete link-admin proposal after callers have copied and
    /// validated every netlink attribute.  The replacement is all-or-nothing:
    /// duplicate names and invalid MTUs are rejected before publication.
    pub(crate) fn configure_link(
        &mut self,
        ifindex: u32,
        name: Option<String>,
        mtu: Option<usize>,
        up: Option<bool>,
    ) -> AxResult {
        let slot = self.device_slot(ifindex).ok_or(AxError::NoSuchDevice)?;
        self.prune_dead_raw_routes();
        if let Some(ref name) = name {
            if name.is_empty()
                || name.as_bytes().contains(&0)
                || name.len() > 15
                || self
                    .links
                    .iter()
                    .enumerate()
                    .any(|(candidate, link)| candidate != slot && link.name == *name)
            {
                return Err(AxError::InvalidInput);
            }
        }
        if let Some(mtu) = mtu {
            // Router packet buffers are the concrete maximum that this
            // namespace can honour.  Do not publish a control-plane MTU the
            // datapath cannot carry.
            if mtu < 68 || mtu > self.mtu {
                return Err(AxError::InvalidInput);
            }
        }
        let link = &mut self.links[slot];
        if let Some(name) = name {
            link.name = name;
        }
        if let Some(mtu) = mtu {
            link.mtu = mtu;
        }
        if let Some(up) = up {
            if !up
                && self
                    .raw_routes
                    .iter()
                    .any(|slot| slot.plan.ifindex == ifindex)
            {
                return Err(AxError::ResourceBusy);
            }
            if link.up && !up {
                // The device bridge is a one-shot retained owner.  Dropping
                // it while administratively down prevents queued ingress
                // from waking a disabled link.
                self.devices[slot].stop_rx_waker();
            }
            link.up = up;
        }
        Ok(())
    }

    pub(crate) fn routes(&self) -> Vec<RouteInfo> {
        self.table
            .rules
            .iter()
            .map(|rule| RouteInfo {
                destination: rule.filter,
                gateway: rule.via,
                interface_index: rule.dev,
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
                PacketDeviceContext::new(self.ifindices[index], self.packet_broker.as_ref(), None);
            let step = if self.rx_wake_quarantined(index)
                || self.devices[index].is_quarantined()
                || !self.links[index].up
            {
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
            // `dequeue` lends the queue storage.  Copy it before mutating
            // any other router state so route-plan consumption cannot alias
            // that outstanding mutable borrow.
            let (metadata, mut outbound) = {
                let Ok((metadata, packet)) = self.tx_buffer.dequeue() else {
                    break;
                };
                (metadata, packet.to_vec())
            };
            let tokenized = matches!(metadata, TxRouteMetadata::RawRoute(_));
            let raw_plan = match metadata {
                TxRouteMetadata::Normal => None,
                TxRouteMetadata::RawRoute(handle) => self.take_raw_route(handle),
            };
            if tokenized && raw_plan.is_none() {
                // A stale/closed handle must never degrade into ordinary
                // header-based routing. Handles are monotonic, so this is a
                // terminal discard rather than an ABA retry.
                warn!("Dropping raw packet with unknown route-plan handle");
                continue;
            }
            if let (TxRouteMetadata::RawRoute(handle), Some(plan)) = (metadata, raw_plan.as_ref()) {
                if let Some(completion) = plan.completion.upgrade() {
                    completion.release_handle(handle);
                }
            }
            dispatched += 1;
            // Local output has already acquired a complete IP packet from
            // smoltcp.  Run LOCAL_OUT before route selection; POSTROUTING is
            // deliberately deferred until a concrete egress device exists so
            // policy receives that device's actual checksum contract.
            let local_output =
                self.packet_context(PacketHookPoint::LocalOutput, 0, false, &outbound);
            if !self.apply_hook(local_output, &mut outbound) {
                fail_raw_route(raw_plan.as_ref());
                continue;
            }
            let packet = outbound.as_slice();
            let Ok(version) = IpVersion::of_packet(packet) else {
                warn!("Dropping malformed IP packet from transmit queue");
                fail_raw_route(raw_plan.as_ref());
                continue;
            };
            match version {
                IpVersion::Ipv4 => {
                    let Ok(packet) = smoltcp::wire::Ipv4Packet::new_checked(packet) else {
                        warn!("Dropping malformed IPv4 packet from transmit queue");
                        fail_raw_route(raw_plan.as_ref());
                        continue;
                    };
                    let dst_addr = IpAddress::Ipv4(packet.dst_addr());
                    if raw_plan.is_none() && packet.dst_addr().is_broadcast() {
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
                        let (ifindex, next_hop, repair_source) = if let Some(plan) =
                            raw_plan.as_ref()
                        {
                            if !matches!(plan.destination, IpAddress::Ipv4(_)) {
                                fail_raw_route(raw_plan.as_ref());
                                continue;
                            }
                            if plan.destination == dst_addr {
                                // A NOWAIT packet was accepted against this
                                // exact route generation. Do not retarget it
                                // merely because a later table replacement is
                                // visible at deferred egress.
                                (
                                    plan.ifindex,
                                    plan.next_hop,
                                    (!plan.header_included && plan.bound_source.is_none())
                                        .then_some(plan.source),
                                )
                            } else {
                                let Some(rule) = self.table.lookup(&dst_addr) else {
                                    warn!("No route found for rewritten destination: {dst_addr}");
                                    fail_raw_route(raw_plan.as_ref());
                                    continue;
                                };
                                if (plan.dont_route && rule.via.is_some())
                                    || plan.bound_source.is_some_and(|source| source != rule.src)
                                {
                                    fail_raw_route(raw_plan.as_ref());
                                    continue;
                                }
                                let source = plan.bound_source.unwrap_or(rule.src);
                                (
                                    rule.dev,
                                    rule.via.unwrap_or(dst_addr),
                                    (!plan.header_included && plan.bound_source.is_none())
                                        .then_some(source),
                                )
                            }
                        } else {
                            let Some(rule) = self.table.lookup(&dst_addr) else {
                                warn!("No route found for destination: {dst_addr}");
                                continue;
                            };
                            // Ordinary traffic remains protected from a raw
                            // header source spoofing a route-selected source.
                            if IpAddress::Ipv4(packet.src_addr()) != rule.src {
                                continue;
                            }
                            (rule.dev, rule.via.unwrap_or(dst_addr), None)
                        };
                        let needs_checksum_repair = repair_source.is_some();
                        if let Some(source) = repair_source
                            && !set_route_selected_source(&mut outbound, source)
                        {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let Ok(packet) =
                            smoltcp::wire::Ipv4Packet::new_checked(outbound.as_slice())
                        else {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        };
                        let Some(slot) = self.device_slot(ifindex) else {
                            warn!("Dropping IPv4 packet for missing route device {ifindex}");
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        };
                        let postrouting = self.packet_context(
                            PacketHookPoint::Postrouting,
                            ifindex,
                            false,
                            packet.as_ref(),
                        );
                        let packet = packet.into_inner();
                        let mut packet = packet.to_vec();
                        if needs_checksum_repair {
                            let checksum = self.devices[slot].packet_checksum_context();
                            let preserve_ipv4_udp_zero = ipv4_udp_checksum_is_zero(&packet);
                            repair_software_checksums(
                                &mut packet,
                                checksum,
                                preserve_ipv4_udp_zero,
                            );
                        }
                        if !self.apply_hook(postrouting, &mut packet) {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let dev = &mut self.devices[slot];
                        if dev.is_quarantined()
                            || !self.links[slot].up
                            || packet.len() > self.links[slot].mtu
                        {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let context =
                            PacketDeviceContext::new(ifindex, self.packet_broker.as_ref(), None);
                        poll_next |= dev.send(context, next_hop, &packet, timestamp);
                    }
                }
                IpVersion::Ipv6 => {
                    let Ok(packet) = smoltcp::wire::Ipv6Packet::new_checked(packet) else {
                        warn!("Dropping malformed IPv6 packet from transmit queue");
                        fail_raw_route(raw_plan.as_ref());
                        continue;
                    };
                    let dst_addr = IpAddress::Ipv6(packet.dst_addr());
                    if raw_plan.is_none() && packet.dst_addr().is_multicast() {
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
                        let (ifindex, next_hop, repair_source) = if let Some(plan) =
                            raw_plan.as_ref()
                        {
                            if !matches!(plan.destination, IpAddress::Ipv6(_)) {
                                fail_raw_route(raw_plan.as_ref());
                                continue;
                            }
                            if plan.destination == dst_addr {
                                (
                                    plan.ifindex,
                                    plan.next_hop,
                                    (!plan.header_included && plan.bound_source.is_none())
                                        .then_some(plan.source),
                                )
                            } else {
                                let Some(rule) = self.table.lookup(&dst_addr) else {
                                    warn!("No route found for rewritten destination: {dst_addr}");
                                    fail_raw_route(raw_plan.as_ref());
                                    continue;
                                };
                                if (plan.dont_route && rule.via.is_some())
                                    || plan.bound_source.is_some_and(|source| source != rule.src)
                                {
                                    fail_raw_route(raw_plan.as_ref());
                                    continue;
                                }
                                let source = plan.bound_source.unwrap_or(rule.src);
                                (
                                    rule.dev,
                                    rule.via.unwrap_or(dst_addr),
                                    (!plan.header_included && plan.bound_source.is_none())
                                        .then_some(source),
                                )
                            }
                        } else {
                            let Some(rule) = self.table.lookup(&dst_addr) else {
                                warn!("No route found for destination: {dst_addr}");
                                continue;
                            };
                            if IpAddress::Ipv6(packet.src_addr()) != rule.src {
                                continue;
                            }
                            (rule.dev, rule.via.unwrap_or(dst_addr), None)
                        };
                        let needs_checksum_repair = repair_source.is_some();
                        if let Some(source) = repair_source
                            && !set_route_selected_source(&mut outbound, source)
                        {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let Ok(packet) =
                            smoltcp::wire::Ipv6Packet::new_checked(outbound.as_slice())
                        else {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        };
                        let Some(slot) = self.device_slot(ifindex) else {
                            warn!("Dropping IPv6 packet for missing route device {ifindex}");
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        };
                        let postrouting = self.packet_context(
                            PacketHookPoint::Postrouting,
                            ifindex,
                            false,
                            packet.as_ref(),
                        );
                        let packet = packet.into_inner();
                        let mut packet = packet.to_vec();
                        if needs_checksum_repair {
                            let checksum = self.devices[slot].packet_checksum_context();
                            let preserve_ipv4_udp_zero = ipv4_udp_checksum_is_zero(&packet);
                            repair_software_checksums(
                                &mut packet,
                                checksum,
                                preserve_ipv4_udp_zero,
                            );
                        }
                        if !self.apply_hook(postrouting, &mut packet) {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let dev = &mut self.devices[slot];
                        if dev.is_quarantined()
                            || !self.links[slot].up
                            || packet.len() > self.links[slot].mtu
                        {
                            fail_raw_route(raw_plan.as_ref());
                            continue;
                        }
                        let context =
                            PacketDeviceContext::new(ifindex, self.packet_broker.as_ref(), None);
                        poll_next |= dev.send(context, next_hop, &packet, timestamp);
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
        while index < device_count && *budget != 0 {
            let device_index = (start + index) % device_count;
            if !self.devices[device_index].is_quarantined()
                && self.links[device_index].up
                && packet_len <= self.links[device_index].mtu
            {
                // Broadcast/multicast has one POSTROUTING traversal per
                // concrete egress device.  That is required both for an
                // interface-specific policy and for accurate checksum
                // ownership; sharing the unmodified fanout backing would
                // silently bypass this seam.
                let ifindex = self.ifindices[device_index];
                let postrouting =
                    self.packet_context(PacketHookPoint::Postrouting, ifindex, false, packet);
                let mut outbound = packet.to_vec();
                if !self.apply_hook(postrouting, &mut outbound) {
                    index += 1;
                    *budget -= 1;
                    continue;
                }
                let context = PacketDeviceContext::new(ifindex, packet_broker, None);
                poll_next |=
                    self.devices[device_index].send(context, next_hop, &outbound, timestamp);
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

    /// The smoltcp interface consumes locally addressed traffic only.  Route
    /// every other valid unicast packet through the Linux FORWARD and
    /// POSTROUTING seam directly from ingress, preserving the packet bytes
    /// rewritten by policy instead of manufacturing a local socket egress.
    /// Admit a packet to the reassembler exactly at the hook seam that asked
    /// for complete L4 data.  Earlier hooks therefore retain their normal
    /// fragment visibility, while a later INPUT/FORWARD link cannot be
    /// bypassed by a non-initial fragment.
    ///
    /// `false` means this fragment has been retained (or rejected) and must
    /// not continue through any later hook during this receive pass.
    fn defragment_for_hook(
        &mut self,
        point: PacketHookPoint,
        packet: &mut Vec<u8>,
        packet_len: &mut usize,
    ) -> bool {
        let Some(query) = self.packet_defrag_query.as_ref() else {
            return true;
        };
        if !query(point, &packet[..*packet_len]) {
            return true;
        }
        match self.fragment_reassembler.ingest(&packet[..*packet_len]) {
            Ok(ReassemblyOutcome::Pass) => true,
            Ok(ReassemblyOutcome::Pending) | Err(_) => false,
            Ok(ReassemblyOutcome::Complete(reassembled)) => {
                if reassembled.len() > packet.len() {
                    return false;
                }
                *packet_len = reassembled.len();
                packet[..*packet_len].copy_from_slice(&reassembled);
                true
            }
        }
    }

    fn forward_ingress(
        &mut self,
        ingress_ifindex: u32,
        packet: &mut Vec<u8>,
        packet_len: &mut usize,
        timestamp: Instant,
    ) -> bool {
        let bytes = &packet[..*packet_len];
        let Ok(version) = IpVersion::of_packet(bytes) else {
            return true;
        };
        let destination = match version {
            IpVersion::Ipv4 => match Ipv4Packet::new_checked(bytes) {
                Ok(packet) => IpAddress::Ipv4(packet.dst_addr()),
                Err(_) => return true,
            },
            IpVersion::Ipv6 => match Ipv6Packet::new_checked(bytes) {
                Ok(packet) => IpAddress::Ipv6(packet.dst_addr()),
                Err(_) => return true,
            },
        };
        if self.table.rules.iter().any(|rule| rule.src == destination) {
            return false;
        }
        if !self.defragment_for_hook(PacketHookPoint::Forward, packet, packet_len) {
            return true;
        }
        let packet = &mut packet[..*packet_len];
        let forward = self.packet_context(PacketHookPoint::Forward, ingress_ifindex, true, packet);
        if !self.apply_hook(forward, packet) {
            return true;
        }
        // FORWARD policy is allowed to rewrite the destination.  Route and
        // next-hop selection therefore use a freshly parsed packet, never
        // the pre-hook address that only classified local delivery above.
        let destination = match IpVersion::of_packet(packet) {
            Ok(IpVersion::Ipv4) => match Ipv4Packet::new_checked(&*packet) {
                Ok(packet) => IpAddress::Ipv4(packet.dst_addr()),
                Err(_) => return true,
            },
            Ok(IpVersion::Ipv6) => match Ipv6Packet::new_checked(&*packet) {
                Ok(packet) => IpAddress::Ipv6(packet.dst_addr()),
                Err(_) => return true,
            },
            Err(_) => return true,
        };
        let Some(rule) = self.table.lookup(&destination) else {
            return true;
        };
        let Some(slot) = self.device_slot(rule.dev) else {
            return true;
        };
        if self.devices[slot].is_quarantined()
            || !self.links[slot].up
            || packet.len() > self.links[slot].mtu
        {
            return true;
        }
        let postrouting =
            self.packet_context(PacketHookPoint::Postrouting, rule.dev, false, packet);
        if !self.apply_hook(postrouting, packet) {
            return true;
        }
        let context = PacketDeviceContext::new(rule.dev, self.packet_broker.as_ref(), None);
        self.devices[slot].send(context, rule.via.unwrap_or(destination), packet, timestamp);
        true
    }
}

fn ip_protocol(packet: &[u8]) -> Option<IpProtocol> {
    match packet.first().copied()? >> 4 {
        4 => Ipv4Packet::new_checked(packet)
            .ok()
            .map(|packet| packet.next_header()),
        6 => Ipv6Packet::new_checked(packet)
            .ok()
            .map(|packet| packet.next_header()),
        _ => None,
    }
}

/// Policy hooks receive mutable packet bytes. Software-owned devices must
/// publish valid checksums after that mutation, before smoltcp parses ingress
/// or a concrete device transmits egress.
fn ipv4_udp_checksum_is_zero(packet: &[u8]) -> bool {
    let Ok(ip) = Ipv4Packet::new_checked(packet) else {
        return false;
    };
    ip.frag_offset() == 0
        && !ip.more_frags()
        && ip.next_header() == IpProtocol::Udp
        && UdpPacket::new_checked(ip.payload()).is_ok_and(|udp| udp.checksum() == 0)
}

fn repair_software_checksums(
    packet: &mut [u8],
    checksum: PacketChecksumContext,
    preserve_ipv4_udp_zero: bool,
) {
    if packet.first().is_none() {
        return;
    }
    match packet[0] >> 4 {
        4 => {
            let Ok(mut ip) = Ipv4Packet::new_checked(packet) else {
                return;
            };
            let source = IpAddress::Ipv4(ip.src_addr());
            let destination = IpAddress::Ipv4(ip.dst_addr());
            let protocol = ip.next_header();
            // A fragmented datagram cannot be checksummed from one fragment;
            // in particular, a non-initial payload must never be reinterpreted
            // as a UDP/TCP header merely because its first bytes look valid.
            let complete_l4 = ip.frag_offset() == 0 && !ip.more_frags();
            if complete_l4
                && !preserve_ipv4_udp_zero
                && checksum.udp == crate::packet::PacketChecksum::Software
                && protocol == IpProtocol::Udp
            {
                if let Ok(mut udp) = UdpPacket::new_checked(ip.payload_mut()) {
                    udp.fill_checksum(&source, &destination);
                }
            } else if complete_l4
                && checksum.tcp == crate::packet::PacketChecksum::Software
                && protocol == IpProtocol::Tcp
            {
                if let Ok(mut tcp) = TcpPacket::new_checked(ip.payload_mut()) {
                    tcp.fill_checksum(&source, &destination);
                }
            }
            if checksum.ipv4 == crate::packet::PacketChecksum::Software {
                ip.fill_checksum();
            }
        }
        6 => {
            let Ok(mut ip) = Ipv6Packet::new_checked(packet) else {
                return;
            };
            let source = IpAddress::Ipv6(ip.src_addr());
            let destination = IpAddress::Ipv6(ip.dst_addr());
            let protocol = ip.next_header();
            if checksum.udp == crate::packet::PacketChecksum::Software
                && protocol == IpProtocol::Udp
            {
                if let Ok(mut udp) = UdpPacket::new_checked(ip.payload_mut()) {
                    udp.fill_checksum(&source, &destination);
                }
            } else if checksum.tcp == crate::packet::PacketChecksum::Software
                && protocol == IpProtocol::Tcp
            {
                if let Ok(mut tcp) = TcpPacket::new_checked(ip.payload_mut()) {
                    tcp.fill_checksum(&source, &destination);
                }
            }
        }
        _ => {}
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

fn fail_raw_route(plan: Option<&RawRoutePlan>) {
    if let Some(completion) = plan.and_then(|plan| plan.completion.upgrade()) {
        completion.fail(crate::options::SocketFault::Other);
    }
}

/// Apply the source selected by a raw route admission without allocating. This
/// is only used for non-HDRINCL, unbound packets: their outer header is owned
/// by the stack, so a LOCAL_OUT destination rewrite must not leave the old
/// route's source or IPv4 checksum behind.
fn set_route_selected_source(packet: &mut [u8], source: IpAddress) -> bool {
    match (packet.first().map(|first| first >> 4), source) {
        (Some(4), IpAddress::Ipv4(source)) => {
            if packet.len() < 20 {
                return false;
            }
            let header_len = usize::from(packet[0] & 0x0f) * 4;
            if header_len < 20 || header_len > packet.len() {
                return false;
            }
            packet[12..16].copy_from_slice(&source.octets());
            packet[10..12].fill(0);
            let mut sum = 0u32;
            for word in packet[..header_len].chunks_exact(2) {
                sum = sum.wrapping_add(u16::from_be_bytes([word[0], word[1]]) as u32);
            }
            while sum >> 16 != 0 {
                sum = (sum & 0xffff).wrapping_add(sum >> 16);
            }
            packet[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
            true
        }
        (Some(6), IpAddress::Ipv6(source)) if packet.len() >= 40 => {
            packet[8..24].copy_from_slice(&source.octets());
            true
        }
        _ => false,
    }
}

pub struct TxToken<'a> {
    buffer: &'a mut PacketBuffer,
    metadata: TxRouteMetadata,
}

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self
            .buffer
            .enqueue(len, self.metadata)
            .expect("This was checked before creating the TxToken"))
    }

    fn set_meta(&mut self, meta: PacketMeta) {
        if meta.id != 0 {
            self.metadata = TxRouteMetadata::RawRoute(meta.id);
        }
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

    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_buffer.is_empty() || self.tx_buffer.is_full() {
            None
        } else {
            let (ifindex, packet) = self.rx_buffer.dequeue().unwrap();
            let mut packet_len = packet.len();
            if packet_len > self.ingress_packet.len() {
                return None;
            }
            let mut ingress = core::mem::take(&mut self.ingress_packet);
            ingress[..packet_len].copy_from_slice(packet);
            if let Some(hook) = self.pre_protocol_hook.as_ref() {
                match hook(ifindex, &ingress[..packet_len]) {
                    Ok(PacketAction::Pass) => {}
                    // A policy-provider failure has no Result channel through
                    // smoltcp::Device::receive.  Treat it as a bounded drop,
                    // exactly like a rejecting pre-protocol policy decision.
                    Ok(PacketAction::Tx) => {
                        // XDP_TX returns the ingress frame through this exact
                        // device while the router already owns its service
                        // lock; routing it back through NetStack would
                        // recurse on that lock.
                        if let Some(slot) = self.device_slot(ifindex) {
                            let protocol = if packet_len >= 14 {
                                u16::from_be_bytes([ingress[12], ingress[13]])
                            } else {
                                0
                            };
                            let context = PacketDeviceContext::new(
                                ifindex,
                                self.packet_broker.as_ref(),
                                None,
                            );
                            let _ = self.devices[slot].send_packet(
                                context,
                                PacketSendRequest::Raw {
                                    protocol,
                                    frame: &ingress[..packet_len],
                                },
                                timestamp,
                            );
                        }
                        self.ingress_packet = ingress;
                        return None;
                    }
                    Ok(PacketAction::Drop | PacketAction::RedirectConsumed) | Err(_) => {
                        self.ingress_packet = ingress;
                        return None;
                    }
                }
            }
            if !self.defragment_for_hook(PacketHookPoint::Prerouting, &mut ingress, &mut packet_len)
            {
                self.ingress_packet = ingress;
                return None;
            }
            let prerouting = self.packet_context(
                PacketHookPoint::Prerouting,
                ifindex,
                true,
                &ingress[..packet_len],
            );
            if !self.apply_hook(prerouting, &mut ingress[..packet_len]) {
                self.ingress_packet = ingress;
                return None;
            }
            if self.forward_ingress(ifindex, &mut ingress, &mut packet_len, timestamp) {
                self.ingress_packet = ingress;
                return None;
            }
            // INPUT is strictly local-only. Forwarding has already consumed
            // non-local frames through FORWARD and POSTROUTING above.
            if !self.defragment_for_hook(PacketHookPoint::Input, &mut ingress, &mut packet_len) {
                self.ingress_packet = ingress;
                return None;
            }
            let input = self.packet_context(
                PacketHookPoint::Input,
                ifindex,
                true,
                &ingress[..packet_len],
            );
            if !self.apply_hook(input, &mut ingress[..packet_len]) {
                self.ingress_packet = ingress;
                return None;
            }
            self.ingress_packet = ingress;
            Some((
                RxToken {
                    data: &self.ingress_packet[..packet_len],
                    listen_table: &self.listen_table,
                },
                TxToken {
                    buffer: &mut self.tx_buffer,
                    metadata: TxRouteMetadata::Normal,
                },
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx_buffer.is_full() {
            None
        } else {
            Some(TxToken {
                buffer: &mut self.tx_buffer,
                metadata: TxRouteMetadata::Normal,
            })
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
            buffer: &mut IngressPacketBuffer,
            _timestamp: Instant,
        ) -> RxStep {
            self.seen.lock().unwrap().push(context.interface_index());
            let Some(step) = self.steps.pop_front() else {
                return RxStep::Idle;
            };
            if step == RxStep::Delivered {
                let dst = buffer.enqueue(1, context.interface_index()).unwrap();
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
            _buffer: &mut IngressPacketBuffer,
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
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let mut router = Router::try_new_loopback_only(listen_table).unwrap();
        for device in devices {
            router.try_add_device(device).unwrap();
        }
        router
    }

    #[test]
    fn no_irq_rx_ring_is_rejected_before_publication() {
        let listen_table = Arc::new(ListenTable::try_new().unwrap());
        let mut router = Router::try_new_loopback_only(listen_table).unwrap();

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
        assert_eq!(
            first,
            RxPass::Continuation {
                consumed: RX_PASS_BUDGET,
                delivered: 11,
            }
        );
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

        let packet = router.rx_buffer.enqueue(1, 1).unwrap();
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

        let packet = router
            .tx_buffer
            .enqueue(20, TxRouteMetadata::Normal)
            .unwrap();
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
