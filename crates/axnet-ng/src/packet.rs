//! Bounded link-layer packet publication and endpoint queues.
//!
//! This module is intentionally Linux-ABI agnostic.  It publishes normalized
//! link metadata to namespace-local subscribers, bounds every retained frame,
//! and owns the check/arm/check readiness source.  `AF_PACKET` parsing,
//! network-byte-order rules, capability checks, and socket options belong in
//! the Linux ABI layer above this crate.

#[cfg(target_os = "none")]
use alloc::string::String;
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
    RegisterError,
};
use axsync::Mutex;
use axtask::WaitQueue;

/// Maximum number of live packet endpoints in one network stack.
pub const MAX_PACKET_ENDPOINTS: usize = 64;
/// Maximum number of captured frames awaiting lock-external delivery.
pub const MAX_PACKET_CAPTURE_BACKLOG: usize = 128;
/// Maximum number of capture-failure records awaiting accounting.
pub const MAX_PACKET_DROP_BACKLOG: usize = 128;
/// Maximum capture events delivered by one drain invocation.
pub const MAX_PACKET_DRAIN_EVENTS_PER_CALL: usize = 32;
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
/// Maximum accounted shared-frame bytes charged in one network stack.
pub const MAX_PACKET_BROKER_BYTES: usize = 16 * 1024 * 1024;
/// Maximum complete link frame accepted by the generic capture path.
pub const MAX_PACKET_FRAME_BYTES: usize = 64 * 1024;

/// The bounded resource that rejected a packet mechanism operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketCapacity {
    /// The per-stack endpoint registry is full.
    EndpointRegistry,
    /// The lock-external capture staging queue is full.
    CaptureBacklog,
    /// The per-stack accounted shared-frame byte budget is exhausted.
    SharedByteBudget,
    /// The endpoint's capture-time selector history is full.
    SelectorEpochs,
    /// The endpoint's capture-time filter history is full.
    FilterEpochs,
}

/// Typed failures reported by the generic packet mechanism.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    /// Backing storage could not be allocated.
    Allocation,
    /// The supplied frame metadata, range, or budget is invalid.
    InvalidInput,
    /// The endpoint is detached from its owning packet broker.
    Detached,
    /// A stable endpoint or capture sequence cannot advance further.
    SequenceExhausted,
    /// A named bounded mechanism resource is full.
    Capacity(PacketCapacity),
}

/// Result returned by packet mechanism admission and mutation operations.
pub type PacketResult<T> = Result<T, PacketError>;

/// Result of one bounded capture-drain invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketDrainStatus {
    /// Another caller owns capture delivery.
    Coalesced,
    /// This invocation consumed all currently staged events.
    Complete {
        /// Number of packet or drop events delivered by this invocation.
        processed: usize,
    },
    /// Staged work remains after this invocation exhausted its fixed credit.
    Continuation {
        /// Number of packet or drop events delivered by this invocation.
        processed: usize,
    },
}

/// Stable identity used to suppress delivery back to an injecting endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PacketEndpointId(u64);

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
        /// Host-order link protocol associated with this transmission.
        ///
        /// This value is deliberately independent of the protocol field in
        /// `frame`: the frame bytes are transmitted unchanged, while outgoing
        /// observers receive the caller-supplied protocol metadata.
        protocol: u16,
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
    pub(crate) const fn new(
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
    pub(crate) const fn origin(self) -> Option<PacketEndpointId> {
        self.origin
    }

    /// Returns an otherwise identical context carrying a packet origin.
    pub(crate) const fn with_origin(self, origin: Option<PacketEndpointId>) -> Self {
        Self { origin, ..self }
    }

    /// Stages one complete frame for later lock-external fanout.
    pub fn stage(
        self,
        mut metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
    ) -> PacketResult<()> {
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
    charged_shared_bytes: AtomicUsize,
}

impl PacketPoolBudget {
    fn try_charge(&self, charge: usize) -> PacketResult<()> {
        let mut current = self.charged_shared_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(charge) else {
                return Err(PacketError::Capacity(PacketCapacity::SharedByteBudget));
            };
            if next > MAX_PACKET_BROKER_BYTES {
                return Err(PacketError::Capacity(PacketCapacity::SharedByteBudget));
            }
            match self.charged_shared_bytes.compare_exchange_weak(
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
        let previous = self
            .charged_shared_bytes
            .fetch_sub(charge, Ordering::AcqRel);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureDropDisposition {
    Ignore,
    Account,
    Unattributed,
}

fn same_filter(
    left: Option<&Arc<dyn PacketFilter>>,
    right: Option<&Arc<dyn PacketFilter>>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

/// Namespace-local packet endpoint with bounded queue and readiness ownership.
pub struct PacketEndpoint {
    id: PacketEndpointId,
    broker: Weak<PacketBroker>,
    state: Mutex<EndpointState>,
    readiness: PollSet,
    detached: AtomicBool,
}

impl PacketEndpoint {
    fn try_new(
        id: PacketEndpointId,
        broker: Weak<PacketBroker>,
        starts_at: u64,
        selector: PacketSelector,
    ) -> PacketResult<Arc<Self>> {
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(MAX_PACKET_QUEUE_FRAMES)
            .map_err(|_| PacketError::Allocation)?;
        let mut selectors = VecDeque::new();
        selectors
            .try_reserve_exact(MAX_PACKET_SELECTOR_EPOCHS)
            .map_err(|_| PacketError::Allocation)?;
        selectors.push_back(SelectorEpoch {
            starts_at,
            selector,
        });
        let mut filters = VecDeque::new();
        filters
            .try_reserve_exact(MAX_PACKET_FILTER_EPOCHS)
            .map_err(|_| PacketError::Allocation)?;
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
            detached: AtomicBool::new(false),
        })
        .map_err(|_| PacketError::Allocation)
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

    fn capture_drop_disposition(
        &self,
        metadata: PacketMetadata,
        sequence: u64,
    ) -> CaptureDropDisposition {
        let state = self.state.lock();
        let Some(selector) = state
            .selectors
            .iter()
            .rev()
            .find(|epoch| epoch.starts_at <= sequence)
            .map(|epoch| epoch.selector)
        else {
            return CaptureDropDisposition::Ignore;
        };
        if !selector.matches(metadata) {
            return CaptureDropDisposition::Ignore;
        }
        match state
            .filters
            .iter()
            .rev()
            .find(|epoch| epoch.starts_at <= sequence)
            .and_then(|epoch| epoch.filter.as_ref())
        {
            None => CaptureDropDisposition::Account,
            Some(_) => CaptureDropDisposition::Unattributed,
        }
    }

    fn detach(&self) {
        let first_detach = {
            let _state = self.state.lock();
            !self.detached.swap(true, Ordering::AcqRel)
        };
        if first_detach {
            self.readiness.close();
        }
    }

    fn detach_after_broker_drop(&self) {
        // PacketBroker::drop owns the final broker reference, so no broker-side
        // enqueue can still race this publication. Receivers retain the endpoint
        // state mutex and may drain records queued before the broker disappeared.
        if !self.detached.swap(true, Ordering::AcqRel) {
            self.readiness.close();
        }
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
        if self.detached.load(Ordering::Acquire) {
            return;
        }
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
    /// therefore report [`PacketCapacity::SelectorEpochs`] instead of silently
    /// reclassifying packets.
    ///
    /// Returns [`PacketError::Detached`] after the owning broker is gone.
    pub fn set_selector(&self, selector: PacketSelector) -> PacketResult<()> {
        if selector.interface_index() == Some(0) {
            return Err(PacketError::InvalidInput);
        }
        let broker = self.broker.upgrade().ok_or(PacketError::Detached)?;
        if self.detached.load(Ordering::Acquire) || broker.terminal.load(Ordering::Acquire) {
            return Err(PacketError::Detached);
        }
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
            let current = state
                .selectors
                .back()
                .copied()
                .expect("packet endpoint always retains one selector");
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
            return Err(PacketError::Capacity(PacketCapacity::SelectorEpochs));
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

    /// Publishes an endpoint-local filter transition for future captures.
    ///
    /// Returns [`PacketError::Detached`] after the owning broker is gone, or
    /// [`PacketCapacity::FilterEpochs`] while in-flight captures still require
    /// every retained filter generation.
    pub fn set_filter(&self, filter: Option<Arc<dyn PacketFilter>>) -> PacketResult<()> {
        let broker = self.broker.upgrade().ok_or(PacketError::Detached)?;
        if self.detached.load(Ordering::Acquire) || broker.terminal.load(Ordering::Acquire) {
            return Err(PacketError::Detached);
        }
        let quiescent = broker.capture_is_quiescent();
        let mut retired: [Option<Arc<dyn PacketFilter>>; MAX_PACKET_FILTER_EPOCHS] =
            core::array::from_fn(|_| None);
        let result = 'update: {
            let mut state = self.state.lock();
            if same_filter(
                state.filters.back().and_then(|epoch| epoch.filter.as_ref()),
                filter.as_ref(),
            ) {
                break 'update Ok(());
            }
            if quiescent {
                let current = state
                    .filters
                    .back_mut()
                    .expect("packet endpoint always retains one filter epoch")
                    .filter
                    .take();
                for (slot, epoch) in retired.iter_mut().zip(state.filters.iter_mut()) {
                    *slot = epoch.filter.take();
                }
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
                let old = core::mem::replace(&mut last.filter, filter);
                if let Some(slot) = retired.iter_mut().find(|slot| slot.is_none()) {
                    *slot = old;
                } else {
                    debug_assert!(old.is_none(), "filter retirement storage exhausted");
                }
                break 'update Ok(());
            }
            if state.filters.len() >= MAX_PACKET_FILTER_EPOCHS {
                break 'update Err(PacketError::Capacity(PacketCapacity::FilterEpochs));
            }
            state.filters.push_back(FilterEpoch { starts_at, filter });
            Ok(())
        };
        drop(retired);
        result
    }

    /// Sets the future receive byte budget.
    ///
    /// Values outside the public nonzero range return
    /// [`PacketError::InvalidInput`].
    pub fn set_receive_budget(&self, bytes: usize) -> PacketResult<()> {
        if !(1..=MAX_PACKET_QUEUE_BYTES).contains(&bytes) {
            return Err(PacketError::InvalidInput);
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
            state.queue.front().cloned()
        } else {
            let removed = state.queue.pop_front();
            if let Some(removed) = &removed {
                state.queued_bytes = state.queued_bytes.saturating_sub(removed.charge);
            }
            removed
        };
        record.ok_or_else(|| {
            if self.detached.load(Ordering::Acquire) {
                AxError::BadState
            } else {
                AxError::WouldBlock
            }
        })
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

    pub(crate) fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    pub(crate) fn arm_readiness<'a>(
        &'a self,
        prepared: &mut PreparedPollRegistration<'a>,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        if self.is_detached() {
            waker.wake_by_ref();
            return Ok(());
        }
        match prepared.arm(&self.readiness, waker) {
            Ok(()) => {}
            Err(PollRegistrationError::Source {
                error: RegisterError::Closed,
                ..
            }) if self.is_detached() => {
                waker.wake_by_ref();
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        // Check after arming so queue publication or broker teardown racing
        // registration cannot lose its terminal edge.
        if self.is_detached() || !self.state.lock().queue.is_empty() {
            waker.wake_by_ref();
        }
        Ok(())
    }
}

impl Pollable for PacketEndpoint {
    fn poll(&self) -> IoEvents {
        let detached = self.detached.load(Ordering::Acquire);
        let mut events = if detached {
            IoEvents::HANGUP
        } else {
            IoEvents::WRITABLE
        };
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
        let terminal_interest = events.intersects(IoEvents::READABLE | IoEvents::HANGUP);
        if !terminal_interest {
            return PollRegistration::empty();
        }
        let mut prepared = PreparedPollRegistration::try_new(1)?;
        self.arm_readiness(&mut prepared, context.waker())?;
        prepared.commit()
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
    prefer_drop: bool,
}

#[derive(Clone, Copy)]
struct CaptureReservation {
    sequence: u64,
    subscription_epoch: u64,
}

enum StagedCapture {
    Packet(PendingPacket),
    Drop(PendingDrop),
}

impl CaptureState {
    fn pop_next(&mut self) -> Option<StagedCapture> {
        let staged = if self.prefer_drop {
            self.drops
                .pop_front()
                .map(StagedCapture::Drop)
                .or_else(|| self.pending.pop_front().map(StagedCapture::Packet))
        } else {
            self.pending
                .pop_front()
                .map(StagedCapture::Packet)
                .or_else(|| self.drops.pop_front().map(StagedCapture::Drop))
        };
        if staged.is_some() {
            self.prefer_drop = matches!(&staged, Some(StagedCapture::Packet(_)));
        }
        staged
    }
}

/// Per-network-stack registry and publication point for link frames.
pub struct PacketBroker {
    state: Mutex<BrokerState>,
    capture: Mutex<CaptureState>,
    pool: Arc<PacketPoolBudget>,
    drain_wait: Arc<WaitQueue>,
    terminal: AtomicBool,
    endpoint_count: AtomicUsize,
    subscription_epoch: AtomicU64,
    subscription_epoch_exhausted: AtomicBool,
    draining: AtomicBool,
    #[cfg(target_os = "none")]
    drain_worker_running: AtomicBool,
    unattributed_drops: AtomicU64,
    next_sequence: AtomicU64,
    #[cfg(test)]
    handoff_pause: AtomicBool,
    #[cfg(test)]
    handoff_reached: AtomicBool,
    #[cfg(test)]
    reservation_pause: AtomicBool,
    #[cfg(test)]
    reservation_reached: AtomicBool,
    #[cfg(test)]
    subscription_snapshot_pause: AtomicBool,
    #[cfg(test)]
    subscription_snapshot_reached: AtomicBool,
}

impl PacketBroker {
    /// Creates a broker with all registry storage preallocated.
    ///
    /// Returns [`PacketError::Allocation`] if any fixed backing allocation
    /// cannot be established.
    pub fn try_new() -> PacketResult<Arc<Self>> {
        let mut endpoints = Vec::new();
        endpoints
            .try_reserve_exact(MAX_PACKET_ENDPOINTS)
            .map_err(|_| PacketError::Allocation)?;
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(MAX_PACKET_CAPTURE_BACKLOG)
            .map_err(|_| PacketError::Allocation)?;
        let mut drops = VecDeque::new();
        drops
            .try_reserve_exact(MAX_PACKET_DROP_BACKLOG)
            .map_err(|_| PacketError::Allocation)?;
        let pool = Arc::try_new(PacketPoolBudget {
            charged_shared_bytes: AtomicUsize::new(0),
        })
        .map_err(|_| PacketError::Allocation)?;
        let drain_wait = Arc::try_new(WaitQueue::new()).map_err(|_| PacketError::Allocation)?;
        Arc::try_new(Self {
            state: Mutex::new(BrokerState {
                next_id: 1,
                endpoints,
            }),
            capture: Mutex::new(CaptureState {
                pending,
                drops,
                reserved: 0,
                prefer_drop: false,
            }),
            pool,
            drain_wait,
            terminal: AtomicBool::new(false),
            endpoint_count: AtomicUsize::new(0),
            subscription_epoch: AtomicU64::new(1),
            subscription_epoch_exhausted: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            #[cfg(target_os = "none")]
            drain_worker_running: AtomicBool::new(false),
            unattributed_drops: AtomicU64::new(0),
            next_sequence: AtomicU64::new(1),
            #[cfg(test)]
            handoff_pause: AtomicBool::new(false),
            #[cfg(test)]
            handoff_reached: AtomicBool::new(false),
            #[cfg(test)]
            reservation_pause: AtomicBool::new(false),
            #[cfg(test)]
            reservation_reached: AtomicBool::new(false),
            #[cfg(test)]
            subscription_snapshot_pause: AtomicBool::new(false),
            #[cfg(test)]
            subscription_snapshot_reached: AtomicBool::new(false),
        })
        .map_err(|_| PacketError::Allocation)
    }

    /// Registers one bounded endpoint in this broker.
    ///
    /// Preserves allocation, endpoint-registry capacity, and stable-identity
    /// exhaustion as distinct [`PacketError`] values.
    pub fn subscribe(
        self: &Arc<Self>,
        selector: PacketSelector,
    ) -> PacketResult<Arc<PacketEndpoint>> {
        if selector.interface_index() == Some(0) {
            return Err(PacketError::InvalidInput);
        }
        let mut state = self.state.lock();
        if self.terminal.load(Ordering::Acquire) {
            return Err(PacketError::Detached);
        }
        if state.endpoints.is_empty() && self.subscription_epoch_exhausted.load(Ordering::Acquire) {
            return Err(PacketError::SequenceExhausted);
        }
        if state.endpoints.len() >= MAX_PACKET_ENDPOINTS {
            return Err(PacketError::Capacity(PacketCapacity::EndpointRegistry));
        }
        let id = PacketEndpointId(state.next_id);
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(PacketError::SequenceExhausted)?;
        let starts_at = self.next_sequence.load(Ordering::Acquire);
        let endpoint = PacketEndpoint::try_new(id, Arc::downgrade(self), starts_at, selector)?;
        state.endpoints.push((id, Arc::downgrade(&endpoint)));
        self.endpoint_count.fetch_add(1, Ordering::Release);
        Ok(endpoint)
    }

    /// Permanently retires capture after its sole deferred-work owner fails.
    ///
    /// The registry lock is the admission boundary for subscriptions. Taking
    /// capture second preserves the existing final-unregister lock order and
    /// makes every old reservation fail its terminal-state recheck. Endpoint
    /// wakeups happen after both locks are released.
    fn enter_terminal(&self) {
        let mut endpoints: [Option<Arc<PacketEndpoint>>; MAX_PACKET_ENDPOINTS] =
            core::array::from_fn(|_| None);
        let mut endpoint_count = 0usize;
        {
            let mut state = self.state.lock();
            if self.terminal.swap(true, Ordering::AcqRel) {
                return;
            }
            self.subscription_epoch_exhausted
                .store(true, Ordering::Release);
            self.endpoint_count.store(0, Ordering::Release);
            for (_, endpoint) in &state.endpoints {
                if let Some(endpoint) = endpoint.upgrade() {
                    endpoints[endpoint_count] = Some(endpoint);
                    endpoint_count += 1;
                }
            }
            state.endpoints.clear();

            let mut capture = self.capture.lock();
            capture.pending.clear();
            capture.drops.clear();
        }
        for endpoint in endpoints[..endpoint_count].iter().flatten() {
            endpoint.detach();
        }
        self.drain_wait.notify_all(false);
    }

    #[cfg(test)]
    fn inject_drain_worker_wait_failure(&self) {
        self.enter_terminal();
    }

    fn unregister(&self, id: PacketEndpointId) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .endpoints
            .iter()
            .position(|(candidate, _)| *candidate == id)
        {
            state.endpoints.swap_remove(index);
            let previous = self.endpoint_count.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "packet endpoint count underflow");
            if previous == 1 {
                if self
                    .subscription_epoch
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                        epoch.checked_add(1)
                    })
                    .is_err()
                {
                    self.subscription_epoch_exhausted
                        .store(true, Ordering::Release);
                }
                // Holding the registry lock prevents a new subscription era
                // from starting until every publication from the old era has
                // either been removed here or invalidated by the epoch change.
                let mut capture = self.capture.lock();
                capture.pending.clear();
                capture.drops.clear();
            }
        }
    }

    pub(crate) fn origin_id(
        self: &Arc<Self>,
        endpoint: &PacketEndpoint,
    ) -> PacketResult<PacketEndpointId> {
        let owner = endpoint.broker.upgrade().ok_or(PacketError::Detached)?;
        if !Arc::ptr_eq(self, &owner) {
            return Err(PacketError::InvalidInput);
        }
        if self.terminal.load(Ordering::Acquire) || endpoint.detached.load(Ordering::Acquire) {
            return Err(PacketError::Detached);
        }
        Ok(endpoint.id)
    }

    fn record_unattributed_drop(&self) {
        let _ =
            self.unattributed_drops
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    Some(current.saturating_add(1))
                });
    }

    fn reservation_is_live(&self, reservation: CaptureReservation) -> bool {
        !self.terminal.load(Ordering::Acquire)
            && !self.subscription_epoch_exhausted.load(Ordering::Acquire)
            && self.endpoint_count.load(Ordering::Acquire) > 0
            && self.subscription_epoch.load(Ordering::Acquire) == reservation.subscription_epoch
    }

    fn capture_subscription_epoch(&self) -> Option<u64> {
        if self.terminal.load(Ordering::Acquire)
            || self.subscription_epoch_exhausted.load(Ordering::Acquire)
        {
            return None;
        }
        // Read the era before its live count. Last-unregister publishes the
        // zero count before advancing the era, while a new subscriber cannot
        // publish its count until that transition releases the registry lock.
        // A concurrent transition is therefore either observed as zero, as a
        // changed era below, or as an old era which later publication rejects.
        let epoch = self.subscription_epoch.load(Ordering::Acquire);
        #[cfg(test)]
        if self.subscription_snapshot_pause.load(Ordering::Acquire) {
            self.subscription_snapshot_reached
                .store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.subscription_snapshot_pause.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "packet subscription snapshot test hook release timed out"
                );
                std::thread::yield_now();
            }
        }
        if self.terminal.load(Ordering::Acquire)
            || self.endpoint_count.load(Ordering::Acquire) == 0
            || self.subscription_epoch_exhausted.load(Ordering::Acquire)
            || self.subscription_epoch.load(Ordering::Acquire) != epoch
        {
            return None;
        }
        Some(epoch)
    }

    fn stage_drop_locked(
        &self,
        capture: &mut CaptureState,
        metadata: PacketMetadata,
        origin: Option<PacketEndpointId>,
        reservation: CaptureReservation,
    ) -> bool {
        if !self.reservation_is_live(reservation) {
            return false;
        }
        if capture.drops.len() < MAX_PACKET_DROP_BACKLOG {
            capture.drops.push_back(PendingDrop {
                metadata,
                origin,
                sequence: reservation.sequence,
            });
        } else {
            self.record_unattributed_drop();
        }
        true
    }

    fn reserve_capture_slot(
        &self,
        metadata: PacketMetadata,
        origin: Option<PacketEndpointId>,
    ) -> PacketResult<Option<CaptureReservation>> {
        let mut capture = self.capture.lock();
        let Some(subscription_epoch) = self.capture_subscription_epoch() else {
            return Ok(None);
        };
        // Sequence publication and its first in-flight representation share
        // this lock. Epoch compaction can therefore never observe an allocated
        // sequence before either a reservation or its accounted drop exists.
        let sequence =
            match self
                .next_sequence
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                }) {
                Ok(sequence) => sequence,
                Err(_) => {
                    return if self.reservation_is_live(CaptureReservation {
                        sequence: u64::MAX,
                        subscription_epoch,
                    }) {
                        Err(PacketError::SequenceExhausted)
                    } else {
                        Ok(None)
                    };
                }
            };
        let reservation = CaptureReservation {
            sequence,
            subscription_epoch,
        };
        let Some(occupied) = capture.pending.len().checked_add(capture.reserved) else {
            return if self.stage_drop_locked(&mut capture, metadata, origin, reservation) {
                Err(PacketError::Capacity(PacketCapacity::CaptureBacklog))
            } else {
                Ok(None)
            };
        };
        if occupied >= MAX_PACKET_CAPTURE_BACKLOG {
            return if self.stage_drop_locked(&mut capture, metadata, origin, reservation) {
                Err(PacketError::Capacity(PacketCapacity::CaptureBacklog))
            } else {
                Ok(None)
            };
        }
        capture.reserved += 1;
        Ok(Some(reservation))
    }

    fn fail_capture_slot(
        &self,
        metadata: PacketMetadata,
        origin: Option<PacketEndpointId>,
        reservation: CaptureReservation,
    ) -> bool {
        let mut capture = self.capture.lock();
        debug_assert!(capture.reserved > 0, "packet capture reservation underflow");
        // Keep the old sequence represented continuously: reservation removal
        // and failure-ledger publication are one capture-state transition.
        capture.reserved = capture.reserved.saturating_sub(1);
        self.stage_drop_locked(&mut capture, metadata, origin, reservation)
    }

    fn deliver_drop(&self, pending: PendingDrop) {
        let mut targets: [Option<Arc<PacketEndpoint>>; MAX_PACKET_ENDPOINTS] =
            core::array::from_fn(|_| None);
        let mut target_count = 0usize;
        let mut unattributed = false;
        {
            let state = self.state.lock();
            for (id, endpoint) in &state.endpoints {
                if Some(*id) == pending.origin {
                    continue;
                }
                let Some(endpoint) = endpoint.upgrade() else {
                    continue;
                };
                match endpoint.capture_drop_disposition(pending.metadata, pending.sequence) {
                    CaptureDropDisposition::Ignore => {}
                    CaptureDropDisposition::Account => {
                        targets[target_count] = Some(endpoint);
                        target_count += 1;
                    }
                    CaptureDropDisposition::Unattributed => unattributed = true,
                }
            }
        }
        for endpoint in targets[..target_count].iter().flatten() {
            endpoint.record_capture_drops(1);
        }
        if unattributed {
            self.record_unattributed_drop();
        }
    }

    /// Stages a complete frame assembled from a link header and payload.
    ///
    /// This method never filters, mutates endpoint queues, or wakes tasks.  A
    /// caller holding a broader network-service lock must invoke
    /// [`drain_staged`](Self::drain_staged) only after releasing that lock.
    /// One immutable frame object and its byte buffer are shared by every
    /// matching subscriber instead of copying bytes per endpoint.
    /// Invalid metadata, sequence exhaustion, capture backlog, accounted
    /// shared-byte pressure, and allocation failure remain distinguishable.
    pub(crate) fn stage_parts_from(
        &self,
        metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
        origin: Option<PacketEndpointId>,
    ) -> PacketResult<()> {
        if self.terminal.load(Ordering::Acquire) {
            return Ok(());
        }
        let total_len = header
            .len()
            .checked_add(payload.len())
            .ok_or(PacketError::InvalidInput)?;
        if total_len > MAX_PACKET_FRAME_BYTES
            || metadata.interface_index == 0
            || usize::from(metadata.link_header_len) > total_len
            || usize::from(metadata.address_len) > metadata.address.len()
        {
            return Err(PacketError::InvalidInput);
        }
        if self.endpoint_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let Some(reservation) = self.reserve_capture_slot(metadata, origin)? else {
            return Ok(());
        };
        #[cfg(test)]
        if self.reservation_pause.load(Ordering::Acquire) {
            self.reservation_reached.store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.reservation_pause.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "packet reservation test hook release timed out"
                );
                std::thread::yield_now();
            }
        }
        let reserved_charge = match total_len.checked_add(size_of::<SharedPacketFrame>()) {
            Some(charge) => charge,
            None => {
                return if self.fail_capture_slot(metadata, origin, reservation) {
                    Err(PacketError::Allocation)
                } else {
                    Ok(())
                };
            }
        };
        if let Err(error) = self.pool.try_charge(reserved_charge) {
            return if self.fail_capture_slot(metadata, origin, reservation) {
                Err(error)
            } else {
                Ok(())
            };
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(total_len).is_err() {
            self.pool.release(reserved_charge);
            return if self.fail_capture_slot(metadata, origin, reservation) {
                Err(PacketError::Allocation)
            } else {
                Ok(())
            };
        }
        let charge = match bytes.capacity().checked_add(size_of::<SharedPacketFrame>()) {
            Some(charge) => charge,
            None => {
                self.pool.release(reserved_charge);
                return if self.fail_capture_slot(metadata, origin, reservation) {
                    Err(PacketError::Allocation)
                } else {
                    Ok(())
                };
            }
        };
        if charge > reserved_charge {
            if let Err(error) = self.pool.try_charge(charge - reserved_charge) {
                self.pool.release(reserved_charge);
                return if self.fail_capture_slot(metadata, origin, reservation) {
                    Err(error)
                } else {
                    Ok(())
                };
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
                return if self.fail_capture_slot(metadata, origin, reservation) {
                    Err(PacketError::Allocation)
                } else {
                    Ok(())
                };
            }
        };
        let mut capture = self.capture.lock();
        debug_assert!(capture.reserved > 0, "packet capture reservation lost");
        capture.reserved = capture.reserved.saturating_sub(1);
        if !self.reservation_is_live(reservation) {
            return Ok(());
        }
        capture.pending.push_back(PendingPacket {
            frame,
            metadata,
            origin,
            sequence: reservation.sequence,
        });
        Ok(())
    }

    /// Convenience staging entry point for traffic with no injecting endpoint.
    pub fn stage_parts(
        &self,
        metadata: PacketMetadata,
        header: &[u8],
        payload: &[u8],
    ) -> PacketResult<()> {
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

    /// Delivers a bounded number of staged captures without the service lock.
    ///
    /// Concurrent callers coalesce behind a single drainer. When the fixed
    /// credit is exhausted, the broker's explicit deferred-work owner is
    /// scheduled; socket readiness remains exclusively about endpoint state.
    /// Pending frames and accounted drops alternate whenever both classes are
    /// available; this is class fairness, not a global sequence FIFO.
    pub fn drain_staged(&self) -> PacketDrainStatus {
        if self.terminal.load(Ordering::Acquire) {
            return PacketDrainStatus::Complete { processed: 0 };
        }
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return PacketDrainStatus::Coalesced;
        }

        let mut processed = 0usize;
        #[cfg(test)]
        let mut observed_empty = false;
        while processed < MAX_PACKET_DRAIN_EVENTS_PER_CALL {
            let staged = { self.capture.lock().pop_next() };
            match staged {
                Some(StagedCapture::Packet(pending)) => self.deliver(pending),
                Some(StagedCapture::Drop(dropped)) => self.deliver_drop(dropped),
                None => {
                    #[cfg(test)]
                    {
                        observed_empty = true;
                    }
                    break;
                }
            }
            processed += 1;
        }
        #[cfg(test)]
        if observed_empty && self.handoff_pause.load(Ordering::Acquire) {
            self.handoff_reached.store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.handoff_pause.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "packet handoff test hook release timed out"
                );
                std::thread::yield_now();
            }
        }
        self.draining.store(false, Ordering::Release);
        let remaining = {
            let capture = self.capture.lock();
            !capture.pending.is_empty() || !capture.drops.is_empty()
        };
        // This queue carries both new staged work and drain-owner handoff.
        // Always publish owner release so a coalesced deferred worker can
        // sleep instead of polling a still-running owner.
        self.drain_wait.notify_one(false);
        if remaining {
            PacketDrainStatus::Continuation { processed }
        } else {
            PacketDrainStatus::Complete { processed }
        }
    }

    #[cfg(target_os = "none")]
    fn has_staged_work(&self) -> bool {
        if self.terminal.load(Ordering::Acquire) {
            return true;
        }
        let capture = self.capture.lock();
        !capture.pending.is_empty() || !capture.drops.is_empty()
    }

    #[cfg(target_os = "none")]
    pub(crate) fn start_drain_worker(self: &Arc<Self>) -> AxResult<()> {
        if self.terminal.load(Ordering::Acquire) {
            return Err(AxError::BadState);
        }
        if self
            .drain_worker_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        if self.terminal.load(Ordering::Acquire) {
            self.drain_worker_running.store(false, Ordering::Release);
            return Err(AxError::BadState);
        }
        let broker = Arc::downgrade(self);
        let wait = Arc::clone(&self.drain_wait);
        let mut name = String::new();
        if name.try_reserve_exact("packet-drain".len()).is_err() {
            self.drain_worker_running.store(false, Ordering::Release);
            return Err(AxError::NoMemory);
        }
        name.push_str("packet-drain");
        if let Err(error) =
            axtask::spawn_with_name(move || Self::drain_worker_loop(broker, wait), name)
        {
            self.drain_worker_running.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(target_os = "none")]
    fn drain_worker_loop(broker: Weak<Self>, wait: Arc<WaitQueue>) {
        loop {
            let Some(owner) = broker.upgrade() else {
                break;
            };
            if owner.terminal.load(Ordering::Acquire) {
                break;
            }
            let status = owner.drain_staged();
            drop(owner);
            match status {
                PacketDrainStatus::Continuation { .. } => axtask::yield_now(),
                PacketDrainStatus::Complete { .. } => {
                    let waited = wait.wait_until(|| {
                        broker.upgrade().is_none_or(|owner| {
                            owner.terminal.load(Ordering::Acquire) || owner.has_staged_work()
                        })
                    });
                    if let Err(error) = waited {
                        warn!(
                            "packet drain worker could not retain its wait state; retiring \
                             capture: {error}"
                        );
                        if let Some(owner) = broker.upgrade() {
                            owner.enter_terminal();
                        }
                        break;
                    }
                }
                PacketDrainStatus::Coalesced => {
                    let waited = wait.wait_until(|| {
                        broker.upgrade().is_none_or(|owner| {
                            owner.terminal.load(Ordering::Acquire)
                                || !owner.draining.load(Ordering::Acquire)
                        })
                    });
                    if let Err(error) = waited {
                        warn!(
                            "packet drain worker lost its owner-handoff wait; retiring capture: \
                             {error}"
                        );
                        if let Some(owner) = broker.upgrade() {
                            owner.enter_terminal();
                        }
                        break;
                    }
                }
            }
        }
        if let Some(owner) = broker.upgrade() {
            owner.drain_worker_running.store(false, Ordering::Release);
        }
    }

    /// Returns the accounted shared-frame charge held by staging and endpoints.
    ///
    /// The charge covers `Vec` capacity plus [`SharedPacketFrame`] storage. It
    /// deliberately does not claim allocator metadata, `Arc` control blocks,
    /// or the broker's fixed preallocated queue and registry backing.
    pub fn charged_shared_bytes(&self) -> usize {
        self.pool.charged_shared_bytes.load(Ordering::Acquire)
    }

    /// Returns capture failures that overflowed the bounded accounting ledger.
    pub fn unattributed_drops(&self) -> u64 {
        self.unattributed_drops.load(Ordering::Acquire)
    }

    fn capture_is_quiescent(&self) -> bool {
        let capture = self.capture.lock();
        !self.draining.load(Ordering::Acquire)
            && capture.reserved == 0
            && capture.pending.is_empty()
            && capture.drops.is_empty()
    }
}

impl Drop for PacketBroker {
    fn drop(&mut self) {
        self.drain_wait.notify_all(false);
        let mut endpoints: [Option<Arc<PacketEndpoint>>; MAX_PACKET_ENDPOINTS] =
            core::array::from_fn(|_| None);
        let mut endpoint_count = 0usize;
        {
            // Drop has exclusive access after the final strong broker owner is
            // gone. Do not enter a sleeping mutex from an arbitrary destructor
            // context when its protected state is already exclusively owned.
            let state = self.state.get_mut();
            for (_, endpoint) in &state.endpoints {
                if let Some(endpoint) = endpoint.upgrade() {
                    endpoints[endpoint_count] = Some(endpoint);
                    endpoint_count += 1;
                }
            }
            state.endpoints.clear();
            self.endpoint_count.store(0, Ordering::Release);
        }
        for endpoint in endpoints[..endpoint_count].iter().flatten() {
            endpoint.detach_after_broker_drop();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::task::Wake;
    use core::task::Waker;

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

    fn drain_all(broker: &PacketBroker) {
        let attempts = (MAX_PACKET_CAPTURE_BACKLOG + MAX_PACKET_DROP_BACKLOG)
            .div_ceil(MAX_PACKET_DRAIN_EVENTS_PER_CALL)
            + 2;
        for _ in 0..attempts {
            match broker.drain_staged() {
                PacketDrainStatus::Complete { .. } => return,
                PacketDrainStatus::Continuation { .. } => {}
                PacketDrainStatus::Coalesced => std::thread::yield_now(),
            }
        }
        panic!("bounded packet drain did not reach quiescence");
    }

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(
            broker.drain_staged(),
            PacketDrainStatus::Complete { processed: 1 }
        );

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
        assert_eq!(
            endpoint
                .set_receive_budget(MAX_PACKET_QUEUE_BYTES + 1)
                .unwrap_err(),
            PacketError::InvalidInput
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
    fn endpoint_registry_and_detachment_keep_typed_reasons() {
        let broker = PacketBroker::try_new().unwrap();
        let selector = PacketSelector::new(PacketProtocol::All, None, PacketView::Raw, true);
        let mut endpoints = Vec::new();
        for _ in 0..MAX_PACKET_ENDPOINTS {
            endpoints.push(broker.subscribe(selector).unwrap());
        }
        assert!(matches!(
            broker.subscribe(selector),
            Err(PacketError::Capacity(PacketCapacity::EndpointRegistry))
        ));

        let detached = endpoints.pop().unwrap();
        drop(endpoints);
        drop(broker);
        assert_eq!(detached.set_selector(selector), Err(PacketError::Detached));
        assert_eq!(detached.set_filter(None), Err(PacketError::Detached));
    }

    #[test]
    fn capture_reservations_preserve_epochs_and_failure_sequences() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::Exact(0x0800),
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let ipv4 = metadata(0x0800, LinkPacketType::Host);
        let ipv4_sequence = broker.reserve_capture_slot(ipv4, None).unwrap().unwrap();
        assert_eq!(ipv4_sequence.sequence, 1);
        assert!(!broker.capture_is_quiescent());

        endpoint
            .set_selector(PacketSelector::new(
                PacketProtocol::Exact(0x0806),
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        endpoint.set_filter(Some(Arc::new(Snaplen(4)))).unwrap();

        let arp = metadata(0x0806, LinkPacketType::Host);
        let arp_sequence = broker.reserve_capture_slot(arp, None).unwrap().unwrap();
        assert_eq!(arp_sequence.sequence, 2);
        endpoint
            .set_selector(PacketSelector::new(
                PacketProtocol::Exact(0x86dd),
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        endpoint.set_filter(Some(Arc::new(Snaplen(8)))).unwrap();

        {
            let state = endpoint.state.lock();
            assert_eq!(
                state
                    .selectors
                    .iter()
                    .map(|epoch| epoch.starts_at)
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(
                state
                    .filters
                    .iter()
                    .map(|epoch| epoch.starts_at)
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
        }

        broker.fail_capture_slot(ipv4, None, ipv4_sequence);
        broker.fail_capture_slot(arp, None, arp_sequence);
        {
            let capture = broker.capture.lock();
            assert_eq!(capture.reserved, 0);
            assert_eq!(
                capture
                    .drops
                    .iter()
                    .map(|dropped| dropped.sequence)
                    .collect::<Vec<_>>(),
                [1, 2]
            );
        }
        assert!(!broker.capture_is_quiescent());

        broker.drain_staged();
        assert_eq!(
            endpoint.take_stats(),
            PacketEndpointStats {
                packets: 1,
                drops: 1,
                ..PacketEndpointStats::default()
            }
        );
        assert_eq!(broker.unattributed_drops(), 1);
        assert!(broker.capture_is_quiescent());
    }

    #[test]
    fn popped_capture_remains_nonquiescent_while_drainer_owns_delivery() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();

        broker.draining.store(true, Ordering::Release);
        let in_delivery = {
            let mut capture = broker.capture.lock();
            let Some(StagedCapture::Packet(pending)) = capture.pop_next() else {
                panic!("staged packet missing");
            };
            assert!(capture.pending.is_empty());
            assert!(capture.drops.is_empty());
            pending
        };
        assert!(!broker.capture_is_quiescent());

        broker.deliver(in_delivery);
        broker.draining.store(false, Ordering::Release);
        assert!(broker.capture_is_quiescent());
        assert_eq!(endpoint.try_receive(false).unwrap().data().last(), Some(&1));
    }

    #[test]
    fn staged_drain_alternates_classes_without_claiming_global_fifo() {
        let broker = PacketBroker::try_new().unwrap();
        let _endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[2])
            .unwrap();
        let dropped_metadata = metadata(0x0800, LinkPacketType::Host);
        let dropped_sequence = broker
            .reserve_capture_slot(dropped_metadata, None)
            .unwrap()
            .unwrap();
        broker.fail_capture_slot(dropped_metadata, None, dropped_sequence);

        let (first, second, third) = {
            let mut capture = broker.capture.lock();
            let first = capture.pop_next().unwrap();
            assert!(capture.prefer_drop);
            let second = capture.pop_next().unwrap();
            assert!(!capture.prefer_drop);
            let third = capture.pop_next().unwrap();
            assert!(capture.prefer_drop);
            (first, second, third)
        };
        assert!(matches!(
            &first,
            StagedCapture::Packet(pending) if pending.sequence == 1
        ));
        assert!(matches!(
            &second,
            StagedCapture::Drop(dropped) if dropped.sequence == 3
        ));
        assert!(matches!(
            &third,
            StagedCapture::Packet(pending) if pending.sequence == 2
        ));
        drop((first, second, third));
        assert_eq!(broker.charged_shared_bytes(), 0);
        assert!(broker.capture_is_quiescent());
    }

    #[test]
    fn selector_and_filter_histories_report_their_own_capacity() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let reserved_metadata = metadata(0x0800, LinkPacketType::Host);
        let reserved_sequence = broker
            .reserve_capture_slot(reserved_metadata, None)
            .unwrap()
            .unwrap();
        for transition in 0..MAX_PACKET_SELECTOR_EPOCHS - 1 {
            broker
                .stage_parts(
                    metadata(0x0800, LinkPacketType::Host),
                    &[0; 14],
                    &[transition as u8],
                )
                .unwrap();
            endpoint
                .set_selector(PacketSelector::new(
                    PacketProtocol::Exact(0x1000 + transition as u16),
                    None,
                    PacketView::Raw,
                    true,
                ))
                .unwrap();
        }
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[0xff])
            .unwrap();
        assert_eq!(
            endpoint.set_selector(PacketSelector::new(
                PacketProtocol::Exact(0x2000),
                None,
                PacketView::Raw,
                true,
            )),
            Err(PacketError::Capacity(PacketCapacity::SelectorEpochs))
        );
        broker.fail_capture_slot(reserved_metadata, None, reserved_sequence);

        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let reserved_metadata = metadata(0x0800, LinkPacketType::Host);
        let reserved_sequence = broker
            .reserve_capture_slot(reserved_metadata, None)
            .unwrap()
            .unwrap();
        for transition in 0..MAX_PACKET_FILTER_EPOCHS - 1 {
            broker
                .stage_parts(
                    metadata(0x0800, LinkPacketType::Host),
                    &[0; 14],
                    &[transition as u8],
                )
                .unwrap();
            endpoint
                .set_filter(Some(Arc::new(Snaplen(transition + 1))))
                .unwrap();
        }
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[0xff])
            .unwrap();
        assert_eq!(
            endpoint.set_filter(Some(Arc::new(Snaplen(1)))),
            Err(PacketError::Capacity(PacketCapacity::FilterEpochs))
        );
        broker.fail_capture_slot(reserved_metadata, None, reserved_sequence);
    }

    #[test]
    fn staging_reports_sequence_and_shared_byte_budget_exhaustion() {
        let broker = PacketBroker::try_new().unwrap();
        let _endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker.next_sequence.store(u64::MAX, Ordering::Release);
        assert_eq!(
            broker.stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1]),
            Err(PacketError::SequenceExhausted)
        );

        let budget = PacketPoolBudget {
            charged_shared_bytes: AtomicUsize::new(MAX_PACKET_BROKER_BYTES),
        };
        assert_eq!(
            budget.try_charge(1),
            Err(PacketError::Capacity(PacketCapacity::SharedByteBudget))
        );
        budget
            .charged_shared_bytes
            .store(usize::MAX, Ordering::Release);
        assert_eq!(
            budget.try_charge(1),
            Err(PacketError::Capacity(PacketCapacity::SharedByteBudget))
        );
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
                Some(source.id),
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
        assert!(endpoint.state.lock().queue.is_empty());
        broker.drain_staged();
        assert_eq!(endpoint.try_receive(false).unwrap().data().last(), Some(&1));

        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[2])
            .unwrap();
        broker
            .stage_parts(metadata(0x0806, LinkPacketType::Host), &[0; 14], &[3])
            .unwrap();
        drain_all(&broker);
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
        assert!(broker.charged_shared_bytes() > 0);
        assert_eq!(
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
                .unwrap_err(),
            PacketError::Capacity(PacketCapacity::CaptureBacklog)
        );
        assert_eq!(endpoint.take_stats().drops, 0);

        drain_all(&broker);
        assert_eq!(endpoint.queue_usage().0, MAX_PACKET_CAPTURE_BACKLOG);
        assert_eq!(endpoint.take_stats().drops, 1);
        drop(endpoint);
        assert_eq!(broker.charged_shared_bytes(), 0);
    }

    #[test]
    fn bounded_drain_reports_continuation_without_using_socket_readiness_as_work_queue() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let _registration = endpoint.register(&mut context, IoEvents::READABLE).unwrap();

        for value in 0..=MAX_PACKET_DRAIN_EVENTS_PER_CALL {
            broker
                .stage_parts(
                    metadata(0x0800, LinkPacketType::Host),
                    &[0; 14],
                    &[value as u8],
                )
                .unwrap();
        }
        assert_eq!(
            broker.drain_staged(),
            PacketDrainStatus::Continuation {
                processed: MAX_PACKET_DRAIN_EVENTS_PER_CALL
            }
        );
        assert_eq!(endpoint.queue_usage().0, MAX_PACKET_DRAIN_EVENTS_PER_CALL);
        assert!(!broker.capture_is_quiescent());
        assert_eq!(wake_count.0.load(Ordering::Acquire), 1);

        assert!(endpoint.poll().contains(IoEvents::READABLE));
        assert!(!broker.capture_is_quiescent());
        assert_eq!(endpoint.queue_usage().0, MAX_PACKET_DRAIN_EVENTS_PER_CALL);
        drain_all(&broker);
        assert!(broker.capture_is_quiescent());
        assert_eq!(
            endpoint.queue_usage().0,
            MAX_PACKET_DRAIN_EVENTS_PER_CALL + 1
        );
    }

    #[test]
    fn coalesced_producer_at_owner_release_is_carried_by_continuation() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker.handoff_pause.store(true, Ordering::Release);
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();

        let drainer = {
            let broker = Arc::clone(&broker);
            std::thread::spawn(move || broker.drain_staged())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !broker.handoff_reached.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "packet drainer did not reach owner-release handoff"
            );
            std::thread::yield_now();
        }
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[2])
            .unwrap();
        assert_eq!(broker.drain_staged(), PacketDrainStatus::Coalesced);
        broker.handoff_pause.store(false, Ordering::Release);

        assert_eq!(
            drainer.join().unwrap(),
            PacketDrainStatus::Continuation { processed: 1 }
        );
        assert!(!broker.capture_is_quiescent());
        assert!(endpoint.poll().contains(IoEvents::READABLE));
        assert!(!broker.capture_is_quiescent());
        drain_all(&broker);
        assert!(broker.capture_is_quiescent());
        assert_eq!(endpoint.queue_usage().0, 2);
    }

    #[test]
    fn final_endpoint_drop_reclaims_continuation_backlog_and_shared_charge() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        for value in 0..=MAX_PACKET_DRAIN_EVENTS_PER_CALL {
            broker
                .stage_parts(
                    metadata(0x0800, LinkPacketType::Host),
                    &[0; 14],
                    &[value as u8],
                )
                .unwrap();
        }
        assert!(matches!(
            broker.drain_staged(),
            PacketDrainStatus::Continuation { .. }
        ));
        assert_eq!(broker.capture.lock().pending.len(), 1);
        assert!(broker.charged_shared_bytes() > 0);

        drop(endpoint);

        let capture = broker.capture.lock();
        assert!(capture.pending.is_empty());
        assert!(capture.drops.is_empty());
        drop(capture);
        assert_eq!(broker.charged_shared_bytes(), 0);
    }

    #[test]
    fn drain_worker_failure_retires_capture_and_releases_staged_charge() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();
        assert!(broker.charged_shared_bytes() > 0);

        broker.inject_drain_worker_wait_failure();

        assert!(broker.terminal.load(Ordering::Acquire));
        assert!(endpoint.poll().contains(IoEvents::HANGUP));
        assert_eq!(broker.endpoint_count.load(Ordering::Acquire), 0);
        assert!(broker.state.lock().endpoints.is_empty());
        let capture = broker.capture.lock();
        assert!(capture.pending.is_empty());
        assert!(capture.drops.is_empty());
        assert_eq!(capture.reserved, 0);
        drop(capture);
        assert_eq!(broker.charged_shared_bytes(), 0);

        let selector = PacketSelector::new(PacketProtocol::All, None, PacketView::Raw, true);
        assert!(matches!(
            broker.subscribe(selector),
            Err(PacketError::Detached)
        ));

        let mut invalid = metadata(0x0800, LinkPacketType::Host);
        invalid.interface_index = 0;
        assert_eq!(broker.stage_parts(invalid, &[], &[]), Ok(()));
        assert_eq!(broker.next_sequence.load(Ordering::Acquire), 2);
        assert_eq!(broker.charged_shared_bytes(), 0);
    }

    #[test]
    fn drain_worker_failure_invalidates_inflight_capture_reservation() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker.reservation_pause.store(true, Ordering::Release);
        let producer = {
            let broker = Arc::clone(&broker);
            std::thread::spawn(move || {
                broker.stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !broker.reservation_reached.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "packet producer did not publish its terminal test reservation"
            );
            std::thread::yield_now();
        }

        broker.inject_drain_worker_wait_failure();
        assert!(endpoint.poll().contains(IoEvents::HANGUP));
        assert_eq!(broker.capture.lock().reserved, 1);
        broker.reservation_pause.store(false, Ordering::Release);
        assert_eq!(producer.join().unwrap(), Ok(()));

        let capture = broker.capture.lock();
        assert_eq!(capture.reserved, 0);
        assert!(capture.pending.is_empty());
        assert!(capture.drops.is_empty());
        drop(capture);
        assert_eq!(broker.charged_shared_bytes(), 0);
        assert_eq!(endpoint.queue_usage(), (0, 0));
        assert!(matches!(
            broker.subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            )),
            Err(PacketError::Detached)
        ));
    }

    #[test]
    fn terminal_endpoint_rejects_capture_already_owned_by_drainer() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            .unwrap();
        broker.draining.store(true, Ordering::Release);
        let pending = {
            let mut capture = broker.capture.lock();
            let Some(StagedCapture::Packet(pending)) = capture.pop_next() else {
                panic!("staged packet missing from terminal delivery test");
            };
            pending
        };

        broker.inject_drain_worker_wait_failure();
        broker.deliver(pending);
        broker.draining.store(false, Ordering::Release);

        assert!(endpoint.poll().contains(IoEvents::HANGUP));
        assert_eq!(endpoint.queue_usage(), (0, 0));
        assert_eq!(broker.charged_shared_bytes(), 0);
        assert!(broker.capture_is_quiescent());
    }

    #[test]
    fn reservation_from_retired_subscription_epoch_cannot_republish_to_new_endpoint() {
        let broker = PacketBroker::try_new().unwrap();
        let old_endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker.reservation_pause.store(true, Ordering::Release);
        let producer = {
            let broker = Arc::clone(&broker);
            std::thread::spawn(move || {
                broker.stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !broker.reservation_reached.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "packet producer did not publish its reservation"
            );
            std::thread::yield_now();
        }

        drop(old_endpoint);
        let new_endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker.reservation_pause.store(false, Ordering::Release);
        assert_eq!(producer.join().unwrap(), Ok(()));
        drain_all(&broker);

        assert_eq!(new_endpoint.queue_usage(), (0, 0));
        assert_eq!(broker.capture.lock().reserved, 0);
        assert_eq!(broker.charged_shared_bytes(), 0);
    }

    #[test]
    fn subscription_snapshot_cannot_join_a_later_endpoint_era() {
        let broker = PacketBroker::try_new().unwrap();
        let old_endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let old_epoch = broker.subscription_epoch.load(Ordering::Acquire);
        broker
            .subscription_snapshot_pause
            .store(true, Ordering::Release);
        let producer = {
            let broker = Arc::clone(&broker);
            std::thread::spawn(move || {
                broker.stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !broker.subscription_snapshot_reached.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "packet producer did not reach its subscription snapshot"
            );
            std::thread::yield_now();
        }

        let retiring = std::thread::spawn(move || drop(old_endpoint));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while broker.subscription_epoch.load(Ordering::Acquire) == old_epoch {
            assert!(
                std::time::Instant::now() < deadline,
                "last endpoint did not publish its retired subscription era"
            );
            std::thread::yield_now();
        }
        assert_eq!(broker.endpoint_count.load(Ordering::Acquire), 0);
        broker
            .subscription_snapshot_pause
            .store(false, Ordering::Release);
        assert_eq!(producer.join().unwrap(), Ok(()));
        retiring.join().unwrap();

        let new_endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        drain_all(&broker);
        assert_eq!(new_endpoint.queue_usage(), (0, 0));
        assert_eq!(broker.capture.lock().reserved, 0);
        assert_eq!(broker.charged_shared_bytes(), 0);
    }

    struct RejectAll;

    impl PacketFilter for RejectAll {
        fn filter(&self, _packet: &[u8]) -> AxResult<usize> {
            Ok(0)
        }
    }

    #[test]
    fn filtered_capture_pressure_is_unattributed_instead_of_false_acceptance() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        endpoint.set_filter(Some(Arc::new(RejectAll))).unwrap();
        for _ in 0..MAX_PACKET_CAPTURE_BACKLOG {
            broker
                .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1])
                .unwrap();
        }
        assert_eq!(
            broker.stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[1]),
            Err(PacketError::Capacity(PacketCapacity::CaptureBacklog))
        );

        assert!(matches!(
            broker.drain_staged(),
            PacketDrainStatus::Continuation { .. }
        ));
        let stats = endpoint.take_stats();
        assert_eq!(stats.packets, 0);
        assert_eq!(stats.drops, 0);
        assert!(stats.filter_rejected > 0);
        assert_eq!(broker.unattributed_drops(), 1);
        drain_all(&broker);
        assert_eq!(endpoint.queue_usage(), (0, 0));
    }

    #[test]
    fn identical_filter_updates_do_not_consume_epoch_capacity() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let reserved_metadata = metadata(0x0800, LinkPacketType::Host);
        let reserved_sequence = broker
            .reserve_capture_slot(reserved_metadata, None)
            .unwrap()
            .unwrap();
        for value in 0..MAX_PACKET_FILTER_EPOCHS + 2 {
            broker
                .stage_parts(
                    metadata(0x0800, LinkPacketType::Host),
                    &[0; 14],
                    &[value as u8],
                )
                .unwrap();
            endpoint.set_filter(None).unwrap();
        }
        assert_eq!(endpoint.state.lock().filters.len(), 1);
        broker.fail_capture_slot(reserved_metadata, None, reserved_sequence);
        drain_all(&broker);
    }

    struct ReentrantDropFilter {
        endpoint: Weak<PacketEndpoint>,
        dropped: Arc<AtomicBool>,
    }

    impl PacketFilter for ReentrantDropFilter {
        fn filter(&self, packet: &[u8]) -> AxResult<usize> {
            Ok(packet.len())
        }
    }

    impl Drop for ReentrantDropFilter {
        fn drop(&mut self) {
            if let Some(endpoint) = self.endpoint.upgrade() {
                std::hint::black_box(endpoint.receive_budget());
            }
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn retired_filter_owner_is_dropped_outside_endpoint_lock() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        endpoint
            .set_filter(Some(Arc::new(ReentrantDropFilter {
                endpoint: Arc::downgrade(&endpoint),
                dropped: Arc::clone(&dropped),
            })))
            .unwrap();
        endpoint.set_filter(Some(Arc::new(Snaplen(4)))).unwrap();
        endpoint.set_filter(Some(Arc::new(Snaplen(8)))).unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn broker_drop_detaches_wakes_and_hangs_up_endpoint() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let _registration = endpoint.register(&mut context, IoEvents::READABLE).unwrap();

        drop(broker);
        assert!(wake_count.0.load(Ordering::Acquire) > 0);
        let events = endpoint.poll();
        assert!(events.contains(IoEvents::HANGUP));
        assert!(!events.contains(IoEvents::WRITABLE));
        assert!(matches!(
            endpoint.try_receive(false),
            Err(AxError::BadState)
        ));
    }

    #[test]
    fn broker_drop_does_not_reenter_endpoint_state_mutex() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();

        let state = endpoint.state.lock();
        drop(broker);
        assert!(endpoint.detached.load(Ordering::Acquire));
        drop(state);

        assert!(endpoint.poll().contains(IoEvents::HANGUP));
    }

    #[test]
    fn hangup_only_registration_observes_broker_teardown() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let _registration = endpoint.register(&mut context, IoEvents::HANGUP).unwrap();

        drop(broker);

        assert_eq!(wake_count.0.load(Ordering::Acquire), 1);
        assert!(endpoint.poll().contains(IoEvents::HANGUP));
    }

    #[test]
    fn detached_endpoint_drains_queued_records_before_bad_state() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        broker
            .stage_parts(metadata(0x0800, LinkPacketType::Host), &[0; 14], &[7])
            .unwrap();
        broker.drain_staged();
        drop(broker);

        let events = endpoint.poll();
        assert!(events.contains(IoEvents::READABLE | IoEvents::HANGUP));
        assert_eq!(endpoint.try_receive(false).unwrap().data().last(), Some(&7));
        assert!(matches!(
            endpoint.try_receive(false),
            Err(AxError::BadState)
        ));
    }

    #[test]
    fn packet_origin_is_bound_to_its_owning_broker() {
        let first = PacketBroker::try_new().unwrap();
        let second = PacketBroker::try_new().unwrap();
        let endpoint = first
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        assert_eq!(first.origin_id(endpoint.as_ref()), Ok(endpoint.id));
        assert_eq!(
            second.origin_id(endpoint.as_ref()),
            Err(PacketError::InvalidInput)
        );
    }

    #[test]
    fn unattributed_drop_counter_saturates() {
        let broker = PacketBroker::try_new().unwrap();
        broker
            .unattributed_drops
            .store(u64::MAX - 1, Ordering::Release);
        broker.record_unattributed_drop();
        broker.record_unattributed_drop();
        assert_eq!(broker.unattributed_drops(), u64::MAX);
    }

    #[test]
    fn zero_subscriber_fast_path_still_validates_metadata() {
        let broker = PacketBroker::try_new().unwrap();
        let mut invalid = metadata(0x0800, LinkPacketType::Host);
        invalid.address_len = 9;
        assert_eq!(
            broker.stage_parts(invalid, &[0; 14], &[1]),
            Err(PacketError::InvalidInput)
        );
        assert_eq!(broker.next_sequence.load(Ordering::Acquire), 1);

        let mut invalid_interface = metadata(0x0800, LinkPacketType::Host);
        invalid_interface.interface_index = 0;
        assert_eq!(
            broker.stage_parts(invalid_interface, &[0; 14], &[1]),
            Err(PacketError::InvalidInput)
        );
        assert_eq!(broker.next_sequence.load(Ordering::Acquire), 1);
    }

    #[test]
    fn zero_interface_selector_is_rejected_before_registry_or_epoch_publication() {
        let broker = PacketBroker::try_new().unwrap();
        assert!(matches!(
            broker.subscribe(PacketSelector::new(
                PacketProtocol::All,
                Some(0),
                PacketView::Raw,
                true,
            )),
            Err(PacketError::InvalidInput)
        ));
        assert_eq!(broker.endpoint_count.load(Ordering::Acquire), 0);

        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                None,
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let original = endpoint.selector();
        assert_eq!(
            endpoint.set_selector(PacketSelector::new(
                PacketProtocol::Exact(0x0800),
                Some(0),
                PacketView::Cooked,
                false,
            )),
            Err(PacketError::InvalidInput)
        );
        assert_eq!(endpoint.selector(), original);
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
            PacketError::InvalidInput
        );
        assert_eq!(broker.capture.lock().reserved, 0);
        assert_eq!(broker.charged_shared_bytes(), 0);
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
        charged_shared_bytes: usize,
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
        assert_eq!(observation.charged_shared_bytes, 0);
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
            "THEKERNEL_PACKET_BROKER_PERF schema=2 run={} case={} subscribers={} count={} \
             elapsed_ns={} throughput_per_sec={} latency_scope={} p50_ns={} p99_ns={} p999_ns={} \
             expected_events={} packet_events={} received={} stage_errors={} drops={} \
             unattributed_drops={} charged_shared_bytes={} invariant=ok",
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
            observation.charged_shared_bytes,
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
        assert_eq!(broker.charged_shared_bytes(), 0);
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
            charged_shared_bytes: broker.charged_shared_bytes(),
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
            charged_shared_bytes: broker.charged_shared_bytes(),
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
                    assert_eq!(error, PacketError::Capacity(PacketCapacity::CaptureBacklog));
                    stage_errors += 1;
                }
            }
            latencies_ns.push(duration_ns(start.elapsed()));
        }
        let elapsed_ns = duration_ns(total_start.elapsed());

        drain_all(&broker);
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
            charged_shared_bytes: broker.charged_shared_bytes(),
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
                            assert_eq!(
                                error,
                                PacketError::Capacity(PacketCapacity::CaptureBacklog)
                            );
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
            charged_shared_bytes: broker.charged_shared_bytes(),
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
        std::println!("THEKERNEL_PACKET_BROKER_PERF_OK schema=2 cases=5");
    }
}
