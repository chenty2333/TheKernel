//! Bounded link-layer packet publication and endpoint queues.
//!
//! This module is intentionally Linux-ABI agnostic.  It publishes normalized
//! link metadata to namespace-local subscribers, bounds every retained frame,
//! and owns the check/arm/check readiness source.  `AF_PACKET` parsing,
//! network-byte-order rules, capability checks, and socket options belong in
//! the Linux ABI layer above this crate.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;

/// Maximum number of live packet endpoints in one network stack.
pub const MAX_PACKET_ENDPOINTS: usize = 64;
/// Maximum number of captured frames awaiting lock-external delivery.
pub const MAX_PACKET_CAPTURE_BACKLOG: usize = 128;
/// Maximum number of capture-failure records awaiting accounting.
pub const MAX_PACKET_DROP_BACKLOG: usize = 128;
/// Maximum selector transitions retained while capture delivery is in flight.
pub const MAX_PACKET_SELECTOR_EPOCHS: usize = 8;
/// Maximum filter transitions retained while capture delivery is in flight.
pub const MAX_PACKET_FILTER_EPOCHS: usize = 8;
/// Maximum number of queued frames retained by one endpoint.
pub const MAX_PACKET_QUEUE_FRAMES: usize = 256;
/// Default retained-byte budget for one endpoint.
pub const DEFAULT_PACKET_QUEUE_BYTES: usize = 256 * 1024;
/// Maximum retained-byte budget accepted from an adapter.
pub const MAX_PACKET_QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes retained by all captures in one network stack.
pub const MAX_PACKET_BROKER_BYTES: usize = 16 * 1024 * 1024;
/// Maximum complete link frame accepted by the generic capture path.
pub const MAX_PACKET_FRAME_BYTES: usize = 64 * 1024;

/// Stable identity used to suppress delivery back to an injecting endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketEndpointId(u64);

impl PacketEndpointId {
    /// Returns the opaque numeric identity for diagnostics.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Link-layer view exposed to one subscriber.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketView {
    /// Preserve the complete link-layer frame.
    Raw,
    /// Remove the link-layer header and expose the network-layer payload.
    Cooked,
}

/// Protocol selection used by a generic packet endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketProtocol {
    /// Do not receive any frames.
    Disabled,
    /// Receive every link protocol.
    All,
    /// Receive one host-order link protocol value.
    Exact(u16),
}

/// Immutable receive selector installed on an endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSelector {
    protocol: PacketProtocol,
    interface_index: Option<u32>,
    view: PacketView,
    capture_outgoing: bool,
}

impl PacketSelector {
    /// Creates a selector.  `None` matches every interface in this stack.
    pub const fn new(
        protocol: PacketProtocol,
        interface_index: Option<u32>,
        view: PacketView,
        capture_outgoing: bool,
    ) -> Self {
        Self {
            protocol,
            interface_index,
            view,
            capture_outgoing,
        }
    }

    /// Returns the selected protocol.
    pub const fn protocol(self) -> PacketProtocol {
        self.protocol
    }

    /// Returns the selected interface, or `None` for every interface.
    pub const fn interface_index(self) -> Option<u32> {
        self.interface_index
    }

    /// Returns the selected packet view.
    pub const fn view(self) -> PacketView {
        self.view
    }

    fn matches(self, metadata: PacketMetadata) -> bool {
        if self
            .interface_index
            .is_some_and(|index| index != metadata.interface_index)
        {
            return false;
        }
        if !self.capture_outgoing && metadata.packet_type == LinkPacketType::Outgoing {
            return false;
        }
        match self.protocol {
            PacketProtocol::Disabled => false,
            PacketProtocol::All => true,
            PacketProtocol::Exact(protocol) => protocol == metadata.protocol,
        }
    }
}

/// Link class associated with a published packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkHardwareType {
    /// Ethernet-compatible link.
    Ethernet,
    /// Software loopback link.
    Loopback,
}

/// Packet direction/address classification supplied by a link device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkPacketType {
    /// Addressed to this host.
    Host,
    /// Link broadcast.
    Broadcast,
    /// Link multicast.
    Multicast,
    /// Delivered by a device although addressed to another host.
    OtherHost,
    /// Produced locally for transmission.
    Outgoing,
}

/// Packet-specific capabilities exposed by one link device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketDeviceCapabilities {
    /// Hardware class reported in packet metadata.
    pub hardware_type: LinkHardwareType,
    /// Whether complete link frames can be observed.
    pub raw_receive: bool,
    /// Whether complete link frames can be injected.
    pub raw_send: bool,
    /// Whether network payloads can be observed.
    pub cooked_receive: bool,
    /// Whether the device can construct a link header for transmission.
    pub cooked_send: bool,
    /// Link-header length used by ordinary frames on this device.
    pub link_header_len: u16,
    /// Link-address length accepted and reported by this device.
    pub address_len: u8,
}

impl PacketDeviceCapabilities {
    /// A device that does not expose packet capture or injection.
    pub const fn unsupported(hardware_type: LinkHardwareType) -> Self {
        Self {
            hardware_type,
            raw_receive: false,
            raw_send: false,
            cooked_receive: false,
            cooked_send: false,
            link_header_len: 0,
            address_len: 0,
        }
    }
}

/// A complete raw frame or a cooked payload submitted to a link device.
pub enum PacketSendRequest<'a> {
    /// Transmit an already constructed link frame.
    Raw {
        /// Complete link frame, including its header.
        frame: &'a [u8],
    },
    /// Let the device construct the link header around a network payload.
    Cooked {
        /// Host-order link protocol value.
        protocol: u16,
        /// Destination link address.
        destination: &'a [u8],
        /// Network-layer payload.
        payload: &'a [u8],
    },
}

/// Normalized metadata for one complete link frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketMetadata {
    /// Stable one-based interface index within the owning stack.
    pub interface_index: u32,
    /// Host-order link protocol value (for example an Ethernet EtherType).
    pub protocol: u16,
    /// Link hardware class.
    pub hardware_type: LinkHardwareType,
    /// Packet direction/address class.
    pub packet_type: LinkPacketType,
    /// Bytes preceding the cooked network payload.
    pub link_header_len: u16,
    /// Source or destination link address as appropriate for the packet.
    pub address: [u8; 8],
    /// Number of valid bytes in `address`.
    pub address_len: u8,
}

/// Device-facing publication context supplied by the owning router.
///
/// [`stage`](Self::stage) performs only bounded capture staging.  Endpoint
/// filtering, queue admission, and wakeups occur later in
/// [`PacketBroker::drain_staged`], after the caller has released the network
/// service mutex.
#[derive(Clone, Copy)]
pub struct PacketDeviceContext<'a> {
    interface_index: u32,
    broker: &'a PacketBroker,
    origin: Option<PacketEndpointId>,
}

impl<'a> PacketDeviceContext<'a> {
    /// Creates the context for one stable, one-based interface index.
    pub const fn new(
        interface_index: u32,
        broker: &'a PacketBroker,
        origin: Option<PacketEndpointId>,
    ) -> Self {
        Self {
            interface_index,
            broker,
            origin,
        }
    }

    /// Returns the public interface index selected by the router.
    pub const fn interface_index(self) -> u32 {
        self.interface_index
    }

    /// Returns the optional injecting endpoint identity.
    pub const fn origin(self) -> Option<PacketEndpointId> {
        self.origin
    }

    /// Returns an otherwise identical context carrying a packet origin.
    pub const fn with_origin(self, origin: Option<PacketEndpointId>) -> Self {
        Self { origin, ..self }
    }

    /// Stages one complete frame for later lock-external fanout.
    pub fn stage(
        self,
        mut metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
    ) -> AxResult<()> {
        metadata.interface_index = self.interface_index;
        self.broker
            .stage_parts_from(metadata, header, payload, self.origin)
    }
}

/// Optional endpoint-local filter applied before queue admission.
pub trait PacketFilter: Send + Sync {
    /// Returns the maximum visible bytes to retain, or zero to reject.
    fn filter(&self, packet: &[u8]) -> AxResult<usize>;
}

struct PacketPoolBudget {
    retained_bytes: AtomicUsize,
}

impl PacketPoolBudget {
    fn try_charge(&self, charge: usize) -> AxResult<()> {
        let mut current = self.retained_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(charge) else {
                return Err(AxError::NoMemory);
            };
            if next > MAX_PACKET_BROKER_BYTES {
                return Err(LinuxError::ENOBUFS.into());
            }
            match self.retained_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, charge: usize) {
        let previous = self.retained_bytes.fetch_sub(charge, Ordering::AcqRel);
        debug_assert!(previous >= charge, "packet pool charge underflow");
    }
}

struct SharedPacketFrame {
    bytes: Vec<u8>,
    pool: Arc<PacketPoolBudget>,
    charge: usize,
}

impl Drop for SharedPacketFrame {
    fn drop(&mut self) {
        self.pool.release(self.charge);
    }
}

/// One immutable receive record removed from, or peeked in, an endpoint queue.
#[derive(Clone)]
pub struct PacketRecord {
    frame: Arc<SharedPacketFrame>,
    metadata: PacketMetadata,
    visible_offset: usize,
    captured_len: usize,
    wire_len: usize,
    charge: usize,
}

impl PacketRecord {
    /// Returns the bytes visible through the endpoint's raw or cooked view.
    pub fn data(&self) -> &[u8] {
        &self.frame.bytes[self.visible_offset..self.visible_offset + self.captured_len]
    }

    /// Returns the visible packet length before filter or userspace truncation.
    pub const fn wire_len(&self) -> usize {
        self.wire_len
    }

    /// Returns normalized link metadata.
    pub const fn metadata(&self) -> PacketMetadata {
        self.metadata
    }
}

/// Reset-on-read endpoint statistics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketEndpointStats {
    /// Frames accepted by selector and filter.
    pub packets: u64,
    /// Accepted frames dropped because allocation or queue admission failed.
    pub drops: u64,
    /// Frames rejected by an attached filter.
    pub filter_rejected: u64,
    /// Attached-filter executions that returned an error.
    pub filter_errors: u64,
}

struct EndpointState {
    selectors: VecDeque<SelectorEpoch>,
    filters: VecDeque<FilterEpoch>,
    queue: VecDeque<PacketRecord>,
    queued_bytes: usize,
    byte_budget: usize,
    stats: PacketEndpointStats,
}

#[derive(Clone, Copy)]
struct SelectorEpoch {
    starts_at: u64,
    selector: PacketSelector,
}

struct FilterEpoch {
    starts_at: u64,
    filter: Option<Arc<dyn PacketFilter>>,
}

/// Namespace-local packet endpoint with bounded queue and readiness ownership.
pub struct PacketEndpoint {
    id: PacketEndpointId,
    broker: Weak<PacketBroker>,
    state: Mutex<EndpointState>,
    readiness: PollSet,
}

impl PacketEndpoint {
    fn try_new(
        id: PacketEndpointId,
        broker: Weak<PacketBroker>,
        starts_at: u64,
        selector: PacketSelector,
    ) -> AxResult<Arc<Self>> {
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(MAX_PACKET_QUEUE_FRAMES)
            .map_err(|_| AxError::NoMemory)?;
        let mut selectors = VecDeque::new();
        selectors
            .try_reserve_exact(MAX_PACKET_SELECTOR_EPOCHS)
            .map_err(|_| AxError::NoMemory)?;
        selectors.push_back(SelectorEpoch {
            starts_at,
            selector,
        });
        let mut filters = VecDeque::new();
        filters
            .try_reserve_exact(MAX_PACKET_FILTER_EPOCHS)
            .map_err(|_| AxError::NoMemory)?;
        filters.push_back(FilterEpoch {
            starts_at,
            filter: None,
        });
        Arc::try_new(Self {
            id,
            broker,
            state: Mutex::new(EndpointState {
                selectors,
                filters,
                queue,
                queued_bytes: 0,
                byte_budget: DEFAULT_PACKET_QUEUE_BYTES,
                stats: PacketEndpointStats::default(),
            }),
            readiness: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn selector_for(&self, metadata: PacketMetadata, sequence: u64) -> Option<PacketSelector> {
        let state = self.state.lock();
        let selector = state
            .selectors
            .iter()
            .rev()
            .find(|epoch| epoch.starts_at <= sequence)?
            .selector;
        selector.matches(metadata).then_some(selector)
    }

    fn record_capture_drops(&self, count: u64) {
        let mut state = self.state.lock();
        state.stats.packets = state.stats.packets.saturating_add(count);
        state.stats.drops = state.stats.drops.saturating_add(count);
    }

    fn enqueue(
        &self,
        frame: Arc<SharedPacketFrame>,
        metadata: PacketMetadata,
        selector: PacketSelector,
        sequence: u64,
    ) {
        let visible_offset = match selector.view() {
            PacketView::Raw => 0,
            PacketView::Cooked => usize::from(metadata.link_header_len).min(frame.bytes.len()),
        };
        let visible = &frame.bytes[visible_offset..];
        let filter = {
            let state = self.state.lock();
            state
                .filters
                .iter()
                .rev()
                .find(|epoch| epoch.starts_at <= sequence)
                .and_then(|epoch| epoch.filter.as_ref().cloned())
        };
        let captured_len = if let Some(filter) = filter {
            match filter.filter(visible) {
                Ok(0) => {
                    let mut state = self.state.lock();
                    state.stats.filter_rejected = state.stats.filter_rejected.saturating_add(1);
                    return;
                }
                Ok(limit) => limit.min(visible.len()),
                Err(_) => {
                    let mut state = self.state.lock();
                    state.stats.filter_errors = state.stats.filter_errors.saturating_add(1);
                    return;
                }
            }
        } else {
            visible.len()
        };
        let wire_len = visible.len();

        let mut state = self.state.lock();
        state.stats.packets = state.stats.packets.saturating_add(1);
        let charge = frame.bytes.len().saturating_add(size_of::<PacketRecord>());
        let admitted_bytes = state
            .queued_bytes
            .checked_add(charge)
            .is_some_and(|bytes| bytes <= state.byte_budget);
        if !admitted_bytes || state.queue.len() >= MAX_PACKET_QUEUE_FRAMES {
            state.stats.drops = state.stats.drops.saturating_add(1);
            return;
        }

        let was_empty = state.queue.is_empty();
        state.queued_bytes += charge;
        state.queue.push_back(PacketRecord {
            frame,
            metadata,
            visible_offset,
            captured_len,
            wire_len,
            charge,
        });
        drop(state);
        if was_empty {
            self.readiness.wake();
        }
    }

    /// Publishes a future-frame selector transition without reallocating.
    ///
    /// A bounded history preserves the selector that was active when an
    /// already staged frame crossed the capture point. Rapid rebind churn can
    /// therefore return `ENOBUFS` instead of silently reclassifying packets.
    pub fn set_selector(&self, selector: PacketSelector) -> AxResult<()> {
        let broker = self.broker.upgrade().ok_or(AxError::BadState)?;
        broker.drain_staged();
        let quiescent = broker.capture_is_quiescent();
        let mut state = self.state.lock();
        if state
            .selectors
            .back()
            .is_some_and(|epoch| epoch.selector == selector)
        {
            return Ok(());
        }
        if quiescent {
            let current = state.selectors.back().copied().ok_or(AxError::BadState)?;
            state.selectors.clear();
            state.selectors.push_back(SelectorEpoch {
                starts_at: 0,
                selector: current.selector,
            });
        }
        let starts_at = broker.next_sequence.load(Ordering::Acquire);
        if let Some(last) = state.selectors.back_mut()
            && last.starts_at == starts_at
        {
            last.selector = selector;
            return Ok(());
        }
        if state.selectors.len() >= MAX_PACKET_SELECTOR_EPOCHS {
            return Err(LinuxError::ENOBUFS.into());
        }
        state.selectors.push_back(SelectorEpoch {
            starts_at,
            selector,
        });
        Ok(())
    }

    /// Returns the current selector.
    pub fn selector(&self) -> PacketSelector {
        self.state
            .lock()
            .selectors
            .back()
            .expect("packet endpoint always retains one selector")
            .selector
    }

    /// Returns the stable identity used for outgoing-source suppression.
    pub const fn id(&self) -> PacketEndpointId {
        self.id
    }

    /// Publishes an endpoint-local filter transition for future captures.
    pub fn set_filter(&self, filter: Option<Arc<dyn PacketFilter>>) -> AxResult<()> {
        let broker = self.broker.upgrade().ok_or(AxError::BadState)?;
        broker.drain_staged();
        let quiescent = broker.capture_is_quiescent();
        let mut state = self.state.lock();
        if quiescent {
            let current = state
                .filters
                .back()
                .ok_or(AxError::BadState)?
                .filter
                .clone();
            state.filters.clear();
            state.filters.push_back(FilterEpoch {
                starts_at: 0,
                filter: current,
            });
        }
        let starts_at = broker.next_sequence.load(Ordering::Acquire);
        if let Some(last) = state.filters.back_mut()
            && last.starts_at == starts_at
        {
            last.filter = filter;
            return Ok(());
        }
        if state.filters.len() >= MAX_PACKET_FILTER_EPOCHS {
            return Err(LinuxError::ENOBUFS.into());
        }
        state.filters.push_back(FilterEpoch { starts_at, filter });
        Ok(())
    }

    /// Sets the future receive byte budget.
    pub fn set_receive_budget(&self, bytes: usize) -> AxResult<()> {
        if !(1..=MAX_PACKET_QUEUE_BYTES).contains(&bytes) {
            return Err(AxError::InvalidInput);
        }
        self.state.lock().byte_budget = bytes;
        Ok(())
    }

    /// Returns the configured receive byte budget.
    pub fn receive_budget(&self) -> usize {
        self.state.lock().byte_budget
    }

    /// Removes or peeks the oldest frame.
    pub fn try_receive(&self, peek: bool) -> AxResult<PacketRecord> {
        let mut state = self.state.lock();
        let record = if peek {
            state.queue.front().cloned().ok_or(AxError::WouldBlock)?
        } else {
            let removed = state.queue.pop_front().ok_or(AxError::WouldBlock)?;
            state.queued_bytes = state.queued_bytes.saturating_sub(removed.charge);
            removed
        };
        Ok(record)
    }

    /// Returns and resets packet/drop/filter counters.
    pub fn take_stats(&self) -> PacketEndpointStats {
        let mut state = self.state.lock();
        core::mem::take(&mut state.stats)
    }

    /// Returns the currently queued frame and byte counts.
    pub fn queue_usage(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.queue.len(), state.queued_bytes)
    }
}

impl Pollable for PacketEndpoint {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::WRITABLE;
        if !self.state.lock().queue.is_empty() {
            events |= IoEvents::READABLE;
        }
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            let registration = PollRegistration::single(&self.readiness, context.waker())?;
            // Check after arming so an empty-to-nonempty transition racing the
            // registration cannot be lost.
            if !self.state.lock().queue.is_empty() {
                context.waker().wake_by_ref();
            }
            Ok(registration)
        } else {
            PollRegistration::empty()
        }
    }
}

impl Drop for PacketEndpoint {
    fn drop(&mut self) {
        if let Some(broker) = self.broker.upgrade() {
            broker.unregister(self.id);
        }
    }
}

struct BrokerState {
    next_id: u64,
    endpoints: Vec<(PacketEndpointId, Weak<PacketEndpoint>)>,
}

struct PendingPacket {
    frame: Arc<SharedPacketFrame>,
    metadata: PacketMetadata,
    origin: Option<PacketEndpointId>,
    sequence: u64,
}

#[derive(Clone, Copy)]
struct PendingDrop {
    metadata: PacketMetadata,
    origin: Option<PacketEndpointId>,
    sequence: u64,
}

struct CaptureState {
    pending: VecDeque<PendingPacket>,
    drops: VecDeque<PendingDrop>,
    reserved: usize,
}

/// Per-network-stack registry and publication point for link frames.
pub struct PacketBroker {
    state: Mutex<BrokerState>,
    capture: Mutex<CaptureState>,
    pool: Arc<PacketPoolBudget>,
    endpoint_count: AtomicUsize,
    draining: AtomicBool,
    unattributed_drops: AtomicU64,
    next_sequence: AtomicU64,
}

impl PacketBroker {
    /// Creates a broker with all registry storage preallocated.
    pub fn try_new() -> AxResult<Arc<Self>> {
        let mut endpoints = Vec::new();
        endpoints
            .try_reserve_exact(MAX_PACKET_ENDPOINTS)
            .map_err(|_| AxError::NoMemory)?;
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(MAX_PACKET_CAPTURE_BACKLOG)
            .map_err(|_| AxError::NoMemory)?;
        let mut drops = VecDeque::new();
        drops
            .try_reserve_exact(MAX_PACKET_DROP_BACKLOG)
            .map_err(|_| AxError::NoMemory)?;
        let pool = Arc::try_new(PacketPoolBudget {
            retained_bytes: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            state: Mutex::new(BrokerState {
                next_id: 1,
                endpoints,
            }),
            capture: Mutex::new(CaptureState {
                pending,
                drops,
                reserved: 0,
            }),
            pool,
            endpoint_count: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            unattributed_drops: AtomicU64::new(0),
            next_sequence: AtomicU64::new(1),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Registers one bounded endpoint in this broker.
    pub fn subscribe(self: &Arc<Self>, selector: PacketSelector) -> AxResult<Arc<PacketEndpoint>> {
        let mut state = self.state.lock();
        if state.endpoints.len() >= MAX_PACKET_ENDPOINTS {
            return Err(LinuxError::ENOBUFS.into());
        }
        let id = PacketEndpointId(state.next_id);
        state.next_id = state.next_id.checked_add(1).ok_or(AxError::BadState)?;
        let starts_at = self.next_sequence.load(Ordering::Acquire);
        let endpoint = PacketEndpoint::try_new(id, Arc::downgrade(self), starts_at, selector)?;
        state.endpoints.push((id, Arc::downgrade(&endpoint)));
        self.endpoint_count.fetch_add(1, Ordering::Release);
        Ok(endpoint)
    }

    fn unregister(&self, id: PacketEndpointId) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .endpoints
            .iter()
            .position(|(candidate, _)| *candidate == id)
        {
            state.endpoints.swap_remove(index);
            self.endpoint_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn reserve_capture_slot(&self) -> AxResult<()> {
        let mut capture = self.capture.lock();
        let occupied = capture
            .pending
            .len()
            .checked_add(capture.reserved)
            .ok_or(AxError::BadState)?;
        if occupied >= MAX_PACKET_CAPTURE_BACKLOG {
            return Err(LinuxError::ENOBUFS.into());
        }
        capture.reserved += 1;
        Ok(())
    }

    fn cancel_capture_slot(&self) {
        let mut capture = self.capture.lock();
        debug_assert!(capture.reserved > 0, "packet capture reservation underflow");
        capture.reserved = capture.reserved.saturating_sub(1);
    }

    fn stage_drop(
        &self,
        metadata: PacketMetadata,
        origin: Option<PacketEndpointId>,
        sequence: u64,
    ) {
        let mut capture = self.capture.lock();
        if capture.drops.len() < MAX_PACKET_DROP_BACKLOG {
            capture.drops.push_back(PendingDrop {
                metadata,
                origin,
                sequence,
            });
        } else {
            self.unattributed_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn deliver_drop(&self, pending: PendingDrop) {
        let mut targets: [Option<Arc<PacketEndpoint>>; MAX_PACKET_ENDPOINTS] =
            core::array::from_fn(|_| None);
        let mut target_count = 0usize;
        {
            let state = self.state.lock();
            for (id, endpoint) in &state.endpoints {
                if Some(*id) == pending.origin {
                    continue;
                }
                let Some(endpoint) = endpoint.upgrade() else {
                    continue;
                };
                if endpoint
                    .selector_for(pending.metadata, pending.sequence)
                    .is_some()
                {
                    targets[target_count] = Some(endpoint);
                    target_count += 1;
                }
            }
        }
        for endpoint in targets[..target_count].iter().flatten() {
            endpoint.record_capture_drops(1);
        }
    }

    /// Stages a complete frame assembled from a link header and payload.
    ///
    /// This method never filters, mutates endpoint queues, or wakes tasks.  A
    /// caller holding a broader network-service lock must invoke
    /// [`drain_staged`](Self::drain_staged) only after releasing that lock.
    /// At most one allocation is retained and shared by every subscriber.
    pub fn stage_parts_from(
        &self,
        metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
        origin: Option<PacketEndpointId>,
    ) -> AxResult<()> {
        if self.endpoint_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let total_len = header
            .len()
            .checked_add(payload.len())
            .ok_or(AxError::InvalidInput)?;
        if total_len > MAX_PACKET_FRAME_BYTES
            || usize::from(metadata.link_header_len) > total_len
            || usize::from(metadata.address_len) > metadata.address.len()
        {
            return Err(AxError::InvalidInput);
        }
        let sequence = self
            .next_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AxError::BadState)?;

        if let Err(error) = self.reserve_capture_slot() {
            self.stage_drop(metadata, origin, sequence);
            return Err(error);
        }
        let reserved_charge = match total_len.checked_add(size_of::<SharedPacketFrame>()) {
            Some(charge) => charge,
            None => {
                self.cancel_capture_slot();
                self.stage_drop(metadata, origin, sequence);
                return Err(AxError::NoMemory);
            }
        };
        if let Err(error) = self.pool.try_charge(reserved_charge) {
            self.cancel_capture_slot();
            self.stage_drop(metadata, origin, sequence);
            return Err(error);
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(total_len).is_err() {
            self.pool.release(reserved_charge);
            self.cancel_capture_slot();
            self.stage_drop(metadata, origin, sequence);
            return Err(AxError::NoMemory);
        }
        let charge = match bytes.capacity().checked_add(size_of::<SharedPacketFrame>()) {
            Some(charge) => charge,
            None => {
                self.pool.release(reserved_charge);
                self.cancel_capture_slot();
                self.stage_drop(metadata, origin, sequence);
                return Err(AxError::NoMemory);
            }
        };
        if charge > reserved_charge {
            if let Err(error) = self.pool.try_charge(charge - reserved_charge) {
                self.pool.release(reserved_charge);
                self.cancel_capture_slot();
                self.stage_drop(metadata, origin, sequence);
                return Err(error);
            }
        } else if charge < reserved_charge {
            self.pool.release(reserved_charge - charge);
        }
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(payload);
        let frame = match Arc::try_new(SharedPacketFrame {
            bytes,
            pool: Arc::clone(&self.pool),
            charge,
        }) {
            Ok(frame) => frame,
            Err(_) => {
                // `Arc::try_new` drops its input value on allocation failure;
                // `SharedPacketFrame::drop` releases the pool charge.
                self.cancel_capture_slot();
                self.stage_drop(metadata, origin, sequence);
                return Err(AxError::NoMemory);
            }
        };
        let mut capture = self.capture.lock();
        debug_assert!(capture.reserved > 0, "packet capture reservation lost");
        capture.reserved = capture.reserved.saturating_sub(1);
        capture.pending.push_back(PendingPacket {
            frame,
            metadata,
            origin,
            sequence,
        });
        Ok(())
    }

    /// Convenience staging entry point for traffic with no injecting endpoint.
    pub fn stage_parts(
        &self,
        metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
    ) -> AxResult<()> {
        self.stage_parts_from(metadata, header, payload, None)
    }

    fn deliver(&self, pending: PendingPacket) {
        let mut targets: [Option<(Arc<PacketEndpoint>, PacketSelector)>; MAX_PACKET_ENDPOINTS] =
            core::array::from_fn(|_| None);
        let mut target_count = 0usize;
        {
            let state = self.state.lock();
            for (id, endpoint) in &state.endpoints {
                if Some(*id) == pending.origin {
                    continue;
                }
                let Some(endpoint) = endpoint.upgrade() else {
                    continue;
                };
                if let Some(selector) = endpoint.selector_for(pending.metadata, pending.sequence) {
                    targets[target_count] = Some((endpoint, selector));
                    target_count += 1;
                }
            }
        }
        for (endpoint, selector) in targets[..target_count].iter().flatten() {
            endpoint.enqueue(
                Arc::clone(&pending.frame),
                pending.metadata,
                *selector,
                pending.sequence,
            );
        }
    }

    /// Delivers all currently staged captures without holding the service lock.
    ///
    /// Concurrent callers coalesce behind a single drainer.  The handoff loop
    /// closes the empty-queue/owner-release race so a staged frame is never
    /// stranded solely because another drainer was exiting.
    pub fn drain_staged(&self) {
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        loop {
            loop {
                let pending = { self.capture.lock().pending.pop_front() };
                if let Some(pending) = pending {
                    self.deliver(pending);
                    continue;
                }
                let dropped = { self.capture.lock().drops.pop_front() };
                if let Some(dropped) = dropped {
                    self.deliver_drop(dropped);
                    continue;
                }
                break;
            }
            self.draining.store(false, Ordering::Release);
            let empty = {
                let capture = self.capture.lock();
                capture.pending.is_empty() && capture.drops.is_empty()
            };
            if empty
                || self
                    .draining
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                return;
            }
        }
    }

    /// Returns actual shared frame bytes retained by staging and endpoints.
    pub fn retained_bytes(&self) -> usize {
        self.pool.retained_bytes.load(Ordering::Acquire)
    }

    /// Returns capture failures that overflowed the bounded accounting ledger.
    pub fn unattributed_drops(&self) -> u64 {
        self.unattributed_drops.load(Ordering::Acquire)
    }

    fn capture_is_quiescent(&self) -> bool {
        if self.draining.load(Ordering::Acquire) {
            return false;
        }
        let capture = self.capture.lock();
        capture.reserved == 0 && capture.pending.is_empty() && capture.drops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(protocol: u16, packet_type: LinkPacketType) -> PacketMetadata {
        PacketMetadata {
            interface_index: 1,
            protocol,
            hardware_type: LinkHardwareType::Ethernet,
            packet_type,
            link_header_len: 14,
            address: [1, 2, 3, 4, 5, 6, 0, 0],
            address_len: 6,
        }
    }

    #[test]
    fn selector_protocol_interface_and_outgoing_rules_are_explicit() {
        let selector = PacketSelector::new(
            PacketProtocol::Exact(0x0800),
            Some(1),
            PacketView::Raw,
            false,
        );
        assert!(selector.matches(metadata(0x0800, LinkPacketType::Host)));
        assert!(!selector.matches(metadata(0x0806, LinkPacketType::Host)));
        assert!(!selector.matches(metadata(0x0800, LinkPacketType::Outgoing)));
        let mut other = metadata(0x0800, LinkPacketType::Host);
        other.interface_index = 2;
        assert!(!selector.matches(other));
    }

    #[test]
    fn raw_and_cooked_endpoints_share_one_frame_but_keep_distinct_views() {
        let broker = PacketBroker::try_new().unwrap();
        let raw = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let cooked = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Cooked,
                true,
            ))
            .unwrap();
        let header = [0x11; 14];
        let payload = [0x22; 32];
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &header, &payload)
            .unwrap();
        assert!(raw.try_receive(true).is_err());
        broker.drain_staged();

        let raw_record = raw.try_receive(false).unwrap();
        let cooked_record = cooked.try_receive(false).unwrap();
        let complete = [&header[..], &payload[..]].concat();
        assert_eq!(raw_record.data(), complete.as_slice());
        assert_eq!(cooked_record.data(), payload);
        assert!(Arc::ptr_eq(&raw_record.frame, &cooked_record.frame));
    }

    struct Snaplen(usize);

    impl PacketFilter for Snaplen {
        fn filter(&self, _packet: &[u8]) -> AxResult<usize> {
            Ok(self.0)
        }
    }

    #[test]
    fn filter_snaplen_queue_budget_and_reset_stats_are_bounded() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Cooked,
                true,
            ))
            .unwrap();
        endpoint.set_filter(Some(Arc::new(Snaplen(4)))).unwrap();
        let payload = [7u8; 64];
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &payload)
            .unwrap();
        broker.drain_staged();
        let record = endpoint.try_receive(false).unwrap();
        assert_eq!(record.data(), &[7; 4]);
        assert_eq!(record.wire_len(), payload.len());
        assert_eq!(endpoint.take_stats().packets, 1);
        assert_eq!(endpoint.take_stats(), PacketEndpointStats::default());
        assert!(
            endpoint
                .set_receive_budget(MAX_PACKET_QUEUE_BYTES + 1)
                .is_err()
        );
    }

    #[test]
    fn endpoint_drop_releases_registry_capacity() {
        let broker = PacketBroker::try_new().unwrap();
        for _ in 0..MAX_PACKET_ENDPOINTS {
            let endpoint = broker
                .subscribe(PacketSelector::new(
                    PacketProtocol::Disabled,
                    None,
                    PacketView::Raw,
                    true,
                ))
                .unwrap();
            drop(endpoint);
        }
        assert_eq!(broker.state.lock().endpoints.len(), 0);
        assert_eq!(broker.endpoint_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn injecting_endpoint_is_suppressed_but_peer_observes_outgoing_frame() {
        let broker = PacketBroker::try_new().unwrap();
        let source = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let observer = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();

        broker
            .stage_parts_from(
                metadata(0x0800, LinkPacketType::Outgoing),
                &[0; 14],
                &[1, 2, 3],
                Some(source.id()),
            )
            .unwrap();
        broker.drain_staged();

        assert!(matches!(
            source.try_receive(false),
            Err(AxError::WouldBlock)
        ));
        let expected = [&[0; 14][..], &[1, 2, 3][..]].concat();
        assert_eq!(
            observer.try_receive(false).unwrap().data(),
            expected.as_slice()
        );
    }

    #[test]
    fn subscription_does_not_receive_frames_captured_before_publication() {
        let broker = PacketBroker::try_new().unwrap();
        let _existing = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::Disabled,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[9])
            .unwrap();
        let later = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();

        broker.drain_staged();
        assert!(matches!(later.try_receive(false), Err(AxError::WouldBlock)));
    }

    #[test]
    fn selector_transition_preserves_capture_time_classification() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::Exact(0x0800),
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();
        endpoint
            .set_selector(PacketSelector::new(
                PacketProtocol::Exact(0x0806),
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        assert_eq!(endpoint.try_receive(false).unwrap().data().last(), Some(&1));

        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[2])
            .unwrap();
        broker
            .stage_parts(metadata(0x0806, LinkPacketType::Host), &[0; 14], &[3])
            .unwrap();
        broker.drain_staged();
        assert_eq!(endpoint.try_receive(false).unwrap().data().last(), Some(&3));
        assert!(matches!(
            endpoint.try_receive(false),
            Err(AxError::WouldBlock)
        ));
    }

    #[test]
    fn capture_backlog_is_bounded_and_delivery_releases_shared_pool_bytes() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        for _ in 0..MAX_PACKET_CAPTURE_BACKLOG {
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
                .unwrap();
        }
        assert!(broker.retained_bytes() > 0);
        assert_eq!(
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
                .unwrap_err(),
            AxError::from(LinuxError::ENOBUFS)
        );
        assert_eq!(endpoint.take_stats().drops, 0);

        broker.drain_staged();
        assert_eq!(endpoint.queue_usage().0, MAX_PACKET_CAPTURE_BACKLOG);
        assert_eq!(endpoint.take_stats().drops, 1);
        drop(endpoint);
        assert_eq!(broker.retained_bytes(), 0);
    }

    #[test]
    fn oversized_frame_is_rejected_before_capture_reservation() {
        let broker = PacketBroker::try_new().unwrap();
        let _endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let oversized = alloc::vec![0u8; MAX_PACKET_FRAME_BYTES + 1];
        assert_eq!(
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &[], &oversized)
                .unwrap_err(),
            AxError::InvalidInput
        );
        assert_eq!(broker.capture.lock().reserved, 0);
        assert_eq!(broker.retained_bytes(), 0);
    }

    const PERF_WARMUP_ITERATIONS: usize = 2_000;
    const PERF_ZERO_SUBSCRIBER_ITERATIONS: usize = 100_000;
    const PERF_FANOUT_ITERATIONS: usize = 10_000;
    const PERF_MULTI_SUBSCRIBERS: usize = 8;
    const PERF_CONCURRENT_PRODUCERS: usize = 4;
    const PERF_CONCURRENT_ITERATIONS_PER_PRODUCER: usize = 5_000;

    struct PerfObservation {
        case: &'static str,
        subscribers: usize,
        count: u64,
        elapsed_ns: u64,
        latency_scope: &'static str,
        latencies_ns: Vec<u64>,
        expected_events: u64,
        packet_events: u64,
        received: u64,
        stage_errors: u64,
        drops: u64,
        unattributed_drops: u64,
        retained_bytes: usize,
    }

    fn duration_ns(duration: std::time::Duration) -> u64 {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }

    fn percentile(sorted: &[u64], per_mille: usize) -> u64 {
        assert!(!sorted.is_empty());
        assert!((1..=1_000).contains(&per_mille));
        let rank = sorted
            .len()
            .saturating_mul(per_mille)
            .div_ceil(1_000)
            .max(1);
        sorted[rank - 1]
    }

    fn performance_run_id() -> u64 {
        std::env::var("THEKERNEL_PACKET_BROKER_PERF_RUN")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    fn emit_performance_observation(mut observation: PerfObservation) {
        assert_eq!(observation.latencies_ns.len() as u64, observation.count);
        assert_eq!(observation.retained_bytes, 0);
        assert_eq!(
            observation
                .packet_events
                .checked_add(observation.unattributed_drops),
            Some(observation.expected_events)
        );
        assert_eq!(
            observation
                .received
                .checked_add(observation.drops)
                .and_then(|value| value.checked_add(observation.unattributed_drops)),
            Some(observation.expected_events)
        );
        observation.latencies_ns.sort_unstable();
        let elapsed_ns = observation.elapsed_ns.max(1);
        let throughput_per_sec =
            u128::from(observation.count) * 1_000_000_000u128 / u128::from(elapsed_ns);
        std::println!(
            "THEKERNEL_PACKET_BROKER_PERF schema=1 run={} case={} subscribers={} count={} \
             elapsed_ns={} throughput_per_sec={} latency_scope={} p50_ns={} p99_ns={} p999_ns={} \
             expected_events={} packet_events={} received={} stage_errors={} drops={} \
             unattributed_drops={} retained_bytes={} invariant=ok",
            performance_run_id(),
            observation.case,
            observation.subscribers,
            observation.count,
            observation.elapsed_ns,
            throughput_per_sec,
            observation.latency_scope,
            percentile(&observation.latencies_ns, 500),
            percentile(&observation.latencies_ns, 990),
            percentile(&observation.latencies_ns, 999),
            observation.expected_events,
            observation.packet_events,
            observation.received,
            observation.stage_errors,
            observation.drops,
            observation.unattributed_drops,
            observation.retained_bytes,
        );
    }

    fn performance_selector() -> PacketSelector {
        PacketSelector::new(PacketProtocol::All, None, PacketView::Raw, true)
    }

    fn warm_fanout_path(
        broker: &PacketBroker,
        endpoints: &[Arc<PacketEndpoint>],
        iterations: usize,
    ) {
        let header = [0x11; 14];
        let payload = [0x22; 64];
        for _ in 0..iterations {
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &header, &payload)
                .unwrap();
            broker.drain_staged();
            for endpoint in endpoints {
                let record = endpoint.try_receive(false).unwrap();
                std::hint::black_box(record.data());
            }
        }
        for endpoint in endpoints {
            let stats = endpoint.take_stats();
            assert_eq!(stats.packets, iterations as u64);
            assert_eq!(stats.drops, 0);
        }
        assert_eq!(broker.retained_bytes(), 0);
    }

    fn benchmark_zero_subscriber() -> PerfObservation {
        let broker = PacketBroker::try_new().unwrap();
        let header = [0x11; 14];
        let payload = [0x22; 64];
        let packet_metadata = metadata(0x0800, LinkPacketType::Host);

        for _ in 0..PERF_WARMUP_ITERATIONS {
            std::hint::black_box(broker.stage_parts(packet_metadata, &header, &payload)).unwrap();
        }

        let mut latencies_ns = Vec::with_capacity(PERF_ZERO_SUBSCRIBER_ITERATIONS);
        let total_start = std::time::Instant::now();
        for _ in 0..PERF_ZERO_SUBSCRIBER_ITERATIONS {
            let start = std::time::Instant::now();
            std::hint::black_box(broker.stage_parts(packet_metadata, &header, &payload)).unwrap();
            latencies_ns.push(duration_ns(start.elapsed()));
        }
        let elapsed_ns = duration_ns(total_start.elapsed());

        assert_eq!(broker.next_sequence.load(Ordering::Acquire), 1);
        assert!(broker.capture_is_quiescent());
        PerfObservation {
            case: "zero_subscriber",
            subscribers: 0,
            count: PERF_ZERO_SUBSCRIBER_ITERATIONS as u64,
            elapsed_ns,
            latency_scope: "stage_call",
            latencies_ns,
            expected_events: 0,
            packet_events: 0,
            received: 0,
            stage_errors: 0,
            drops: 0,
            unattributed_drops: broker.unattributed_drops(),
            retained_bytes: broker.retained_bytes(),
        }
    }

    fn benchmark_fanout(
        case: &'static str,
        subscriber_count: usize,
        iterations: usize,
    ) -> PerfObservation {
        let broker = PacketBroker::try_new().unwrap();
        let endpoints = (0..subscriber_count)
            .map(|_| broker.subscribe(performance_selector()).unwrap())
            .collect::<Vec<_>>();
        warm_fanout_path(&broker, &endpoints, PERF_WARMUP_ITERATIONS);

        let header = [0x11; 14];
        let payload = [0x22; 64];
        let packet_metadata = metadata(0x0800, LinkPacketType::Host);
        let mut latencies_ns = Vec::with_capacity(iterations);
        let mut received = 0u64;
        let total_start = std::time::Instant::now();
        for _ in 0..iterations {
            let start = std::time::Instant::now();
            broker
                .stage_parts(packet_metadata, &header, &payload)
                .unwrap();
            broker.drain_staged();
            for endpoint in &endpoints {
                let record = endpoint.try_receive(false).unwrap();
                std::hint::black_box(record.data());
                received += 1;
            }
            latencies_ns.push(duration_ns(start.elapsed()));
        }
        let elapsed_ns = duration_ns(total_start.elapsed());

        let mut packet_events = 0u64;
        let mut drops = 0u64;
        for endpoint in &endpoints {
            let stats = endpoint.take_stats();
            assert_eq!(stats.filter_rejected, 0);
            assert_eq!(stats.filter_errors, 0);
            packet_events += stats.packets;
            drops += stats.drops;
            assert_eq!(endpoint.queue_usage(), (0, 0));
        }
        PerfObservation {
            case,
            subscribers: subscriber_count,
            count: iterations as u64,
            elapsed_ns,
            latency_scope: "stage_drain_consume",
            latencies_ns,
            expected_events: (iterations * subscriber_count) as u64,
            packet_events,
            received,
            stage_errors: 0,
            drops,
            unattributed_drops: broker.unattributed_drops(),
            retained_bytes: broker.retained_bytes(),
        }
    }

    fn benchmark_saturation_accounting() -> PerfObservation {
        let warm_broker = PacketBroker::try_new().unwrap();
        let warm_endpoint = warm_broker.subscribe(performance_selector()).unwrap();
        warm_fanout_path(
            &warm_broker,
            core::slice::from_ref(&warm_endpoint),
            PERF_WARMUP_ITERATIONS,
        );

        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker.subscribe(performance_selector()).unwrap();
        let attempts = MAX_PACKET_CAPTURE_BACKLOG + MAX_PACKET_DROP_BACKLOG / 2;
        let header = [0x11; 14];
        let payload = [0x22; 64];
        let packet_metadata = metadata(0x0800, LinkPacketType::Host);
        let mut latencies_ns = Vec::with_capacity(attempts);
        let mut stage_errors = 0u64;
        let total_start = std::time::Instant::now();
        for _ in 0..attempts {
            let start = std::time::Instant::now();
            match broker.stage_parts(packet_metadata, &header, &payload) {
                Ok(()) => {}
                Err(error) => {
                    assert_eq!(error, AxError::from(LinuxError::ENOBUFS));
                    stage_errors += 1;
                }
            }
            latencies_ns.push(duration_ns(start.elapsed()));
        }
        let elapsed_ns = duration_ns(total_start.elapsed());

        broker.drain_staged();
        let mut received = 0u64;
        while let Ok(record) = endpoint.try_receive(false) {
            std::hint::black_box(record.data());
            received += 1;
        }
        let stats = endpoint.take_stats();
        assert_eq!(stats.filter_rejected, 0);
        assert_eq!(stats.filter_errors, 0);
        assert_eq!(stage_errors, (attempts - MAX_PACKET_CAPTURE_BACKLOG) as u64);
        assert_eq!(endpoint.queue_usage(), (0, 0));
        PerfObservation {
            case: "saturation_accounting",
            subscribers: 1,
            count: attempts as u64,
            elapsed_ns,
            latency_scope: "stage_call",
            latencies_ns,
            expected_events: attempts as u64,
            packet_events: stats.packets,
            received,
            stage_errors,
            drops: stats.drops,
            unattributed_drops: broker.unattributed_drops(),
            retained_bytes: broker.retained_bytes(),
        }
    }

    fn benchmark_concurrent_pipeline() -> PerfObservation {
        use std::{
            sync::{Barrier, atomic::AtomicBool},
            thread,
        };

        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker.subscribe(performance_selector()).unwrap();
        warm_fanout_path(
            &broker,
            core::slice::from_ref(&endpoint),
            PERF_WARMUP_ITERATIONS,
        );

        let barrier = Arc::new(Barrier::new(PERF_CONCURRENT_PRODUCERS + 3));
        let producers_done = Arc::new(AtomicUsize::new(0));
        let drainer_done = Arc::new(AtomicBool::new(false));
        let mut producers = Vec::with_capacity(PERF_CONCURRENT_PRODUCERS);
        for _ in 0..PERF_CONCURRENT_PRODUCERS {
            let broker = Arc::clone(&broker);
            let barrier = Arc::clone(&barrier);
            let producers_done = Arc::clone(&producers_done);
            producers.push(thread::spawn(move || {
                let header = [0x11; 14];
                let payload = [0x22; 64];
                let packet_metadata = metadata(0x0800, LinkPacketType::Host);
                let mut latencies_ns = Vec::with_capacity(PERF_CONCURRENT_ITERATIONS_PER_PRODUCER);
                let mut stage_errors = 0u64;
                barrier.wait();
                for _ in 0..PERF_CONCURRENT_ITERATIONS_PER_PRODUCER {
                    let start = std::time::Instant::now();
                    match broker.stage_parts(packet_metadata, &header, &payload) {
                        Ok(()) => {}
                        Err(error) => {
                            assert_eq!(error, AxError::from(LinuxError::ENOBUFS));
                            stage_errors += 1;
                        }
                    }
                    latencies_ns.push(duration_ns(start.elapsed()));
                }
                producers_done.fetch_add(1, Ordering::Release);
                (latencies_ns, stage_errors)
            }));
        }

        let drainer = {
            let broker = Arc::clone(&broker);
            let barrier = Arc::clone(&barrier);
            let producers_done = Arc::clone(&producers_done);
            let drainer_done = Arc::clone(&drainer_done);
            thread::spawn(move || {
                barrier.wait();
                loop {
                    broker.drain_staged();
                    if producers_done.load(Ordering::Acquire) == PERF_CONCURRENT_PRODUCERS
                        && broker.capture_is_quiescent()
                    {
                        break;
                    }
                    thread::yield_now();
                }
                drainer_done.store(true, Ordering::Release);
            })
        };

        let consumer = {
            let endpoint = Arc::clone(&endpoint);
            let barrier = Arc::clone(&barrier);
            let drainer_done = Arc::clone(&drainer_done);
            thread::spawn(move || {
                let mut received = 0u64;
                barrier.wait();
                loop {
                    match endpoint.try_receive(false) {
                        Ok(record) => {
                            std::hint::black_box(record.data());
                            received += 1;
                        }
                        Err(AxError::WouldBlock) if drainer_done.load(Ordering::Acquire) => break,
                        Err(AxError::WouldBlock) => thread::yield_now(),
                        Err(error) => panic!("unexpected concurrent receive error: {error:?}"),
                    }
                }
                received
            })
        };

        let total_start = std::time::Instant::now();
        barrier.wait();
        let mut latencies_ns =
            Vec::with_capacity(PERF_CONCURRENT_PRODUCERS * PERF_CONCURRENT_ITERATIONS_PER_PRODUCER);
        let mut stage_errors = 0u64;
        for producer in producers {
            let (mut producer_latencies, producer_errors) = producer.join().unwrap();
            latencies_ns.append(&mut producer_latencies);
            stage_errors += producer_errors;
        }
        drainer.join().unwrap();
        let received = consumer.join().unwrap();
        let elapsed_ns = duration_ns(total_start.elapsed());

        let stats = endpoint.take_stats();
        let attempts = PERF_CONCURRENT_PRODUCERS * PERF_CONCURRENT_ITERATIONS_PER_PRODUCER;
        let unattributed_drops = broker.unattributed_drops();
        assert_eq!(stats.filter_rejected, 0);
        assert_eq!(stats.filter_errors, 0);
        assert!(stats.drops.saturating_add(unattributed_drops) >= stage_errors);
        assert_eq!(endpoint.queue_usage(), (0, 0));
        assert!(broker.capture_is_quiescent());
        PerfObservation {
            case: "concurrent_pipeline",
            subscribers: 1,
            count: attempts as u64,
            elapsed_ns,
            latency_scope: "producer_stage",
            latencies_ns,
            expected_events: attempts as u64,
            packet_events: stats.packets,
            received,
            stage_errors,
            drops: stats.drops,
            unattributed_drops,
            retained_bytes: broker.retained_bytes(),
        }
    }

    #[test]
    #[ignore = "host evidence harness; values are observations, not portable thresholds"]
    fn packet_broker_performance_evidence() {
        // Start on a fresh line even under libtest's inline `test ...` prefix,
        // allowing the evidence runner to consume only schema-tagged records.
        std::println!();
        emit_performance_observation(benchmark_zero_subscriber());
        emit_performance_observation(benchmark_fanout(
            "single_subscriber",
            1,
            PERF_FANOUT_ITERATIONS,
        ));
        emit_performance_observation(benchmark_fanout(
            "multi_subscriber",
            PERF_MULTI_SUBSCRIBERS,
            PERF_FANOUT_ITERATIONS,
        ));
        emit_performance_observation(benchmark_saturation_accounting());
        emit_performance_observation(benchmark_concurrent_pipeline());
        std::println!("THEKERNEL_PACKET_BROKER_PERF_OK schema=1 cases=5");
    }
}
