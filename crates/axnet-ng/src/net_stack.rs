#[cfg(target_os = "none")]
use alloc::sync::Weak;
use alloc::{boxed::Box, string::String, sync::Arc, task::Wake, vec::Vec};
use core::{
    sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering},
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
    device::{
        Device, DeviceStats, InterfaceInfo, LoopbackDevice, PacketSendProgress, RxWakeSource,
    },
    listen_table::ListenTable,
    packet::{
        PacketBroker, PacketDeviceCapabilities, PacketEndpoint, PacketRecord, PacketResult,
        PacketSelector, PacketSendRequest,
    },
    router::{RouteInfo, Router, Rule, RxWakeRegistration},
    service::{Service, ServicePoll},
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
    terminal_source: Arc<PollSet>,
    poll_waker: Waker,
    rx_worker: Option<Arc<NetRxWorker>>,
    pub(crate) tcp_ephemeral_port: Mutex<u16>,
    pub(crate) udp_ephemeral_port: Mutex<u16>,
    ipv4_conf_default_tag: AtomicI32,
    ipv4_conf_lo_tag: AtomicI32,
}

/// Result of one public [`NetStack::poll_interfaces`] pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetPollStatus {
    Quiescent,
    Continuation,
    /// A device or wake source reported a terminal completion/ownership
    /// quarantine. The dedicated worker remains alive and continues
    /// servicing non-fenced software devices; the fenced source is never
    /// polled or re-armed. `continuation` preserves a bounded follow-up pass
    /// for healthy software backlog without hiding the terminal quarantine.
    Quarantined {
        continuation: bool,
        /// True only for the pass that observed/published this quarantine
        /// edge. A sticky quarantine level does not replay readiness.
        edge: bool,
    },
    /// The receive worker/source owner reached a terminal lifecycle state.
    /// Readiness callers must observe this edge instead of parking for another
    /// hardware wake that can no longer be delivered.
    Terminal {
        reason: NetRxTerminalReason,
    },
}

impl NetPollStatus {
    pub const fn is_continuation(self) -> bool {
        matches!(
            self,
            Self::Continuation
                | Self::Quarantined {
                    continuation: true,
                    ..
                }
        )
    }

    pub const fn is_quarantined(self) -> bool {
        matches!(self, Self::Quarantined { .. })
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
}

/// Why a physical network receive worker stopped accepting wake ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetRxTerminalReason {
    /// The scheduler refused to park the worker.
    WaitFailure,
    /// A device wake-source rearm or completion path was quarantined.
    WakeSourceFailure,
}

impl NetRxTerminalReason {
    const WAIT_FAILURE: u8 = 1;
    const WAKE_SOURCE_FAILURE: u8 = 2;

    const fn code(self) -> u8 {
        match self {
            Self::WaitFailure => Self::WAIT_FAILURE,
            Self::WakeSourceFailure => Self::WAKE_SOURCE_FAILURE,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            Self::WAIT_FAILURE => Some(Self::WaitFailure),
            Self::WAKE_SOURCE_FAILURE => Some(Self::WakeSourceFailure),
            _ => None,
        }
    }
}

const RX_WORKER_RUNNING: u8 = 0;
const RX_WORKER_STOPPING: u8 = 1;
const RX_WORKER_TERMINAL: u8 = 2;
const RX_WORKER_FENCING: u8 = 3;

struct NetRxWorker {
    state: Arc<NetRxWorkerState>,
    irq_waker: Waker,
}

struct NetRxWorkerState {
    generation: AtomicU64,
    lifecycle: AtomicU8,
    terminal_reason: AtomicU8,
    ownership_stopped: AtomicU8,
    wait: axtask::WaitQueue,
}

impl NetRxWorkerState {
    fn stop(&self) {
        let _ = self.lifecycle.compare_exchange(
            RX_WORKER_RUNNING,
            RX_WORKER_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.wait.notify_all(false);
    }

    fn is_stopping(&self) -> bool {
        self.lifecycle.load(Ordering::Acquire) != RX_WORKER_RUNNING
    }

    fn terminal_reason(&self) -> Option<NetRxTerminalReason> {
        if self.lifecycle.load(Ordering::Acquire) != RX_WORKER_TERMINAL {
            return None;
        }
        NetRxTerminalReason::from_code(self.terminal_reason.load(Ordering::Acquire))
    }

    fn claim_ownership_stop(&self) -> bool {
        self.ownership_stopped
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Publishes a terminal reason before exposing the terminal lifecycle.
    /// Readers that observe `RX_WORKER_TERMINAL` therefore also observe a
    /// valid reason, and the generation edge wakes task-context waiters.
    fn fence_terminal(&self, reason: NetRxTerminalReason) -> bool {
        if self
            .lifecycle
            .compare_exchange(
                RX_WORKER_RUNNING,
                RX_WORKER_FENCING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.terminal_reason.store(reason.code(), Ordering::Release);
        self.lifecycle.store(RX_WORKER_TERMINAL, Ordering::Release);
        self.publish_generation_edge();
        self.wait.notify_all(false);
        true
    }

    fn publish_generation_edge(&self) {
        let mut current = self.generation.load(Ordering::Acquire);
        loop {
            let next = current
                .checked_add(1)
                .expect("network receive generation exhausted");
            match self.generation.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn publish_wake(&self) {
        if self.is_stopping() {
            self.wait.notify_all(false);
            return;
        }
        self.publish_generation_edge();
        self.wait.notify_all(false);
    }
}

struct NetRxIrqWake(Arc<NetRxWorkerState>);

impl Wake for NetRxIrqWake {
    fn wake(self: Arc<Self>) {
        self.0.publish_wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.publish_wake();
    }
}

impl NetRxWorker {
    fn try_new() -> AxResult<Arc<Self>> {
        let state = Arc::try_new(NetRxWorkerState {
            generation: AtomicU64::new(0),
            lifecycle: AtomicU8::new(RX_WORKER_RUNNING),
            terminal_reason: AtomicU8::new(0),
            ownership_stopped: AtomicU8::new(0),
            wait: axtask::WaitQueue::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let wake = Arc::try_new(NetRxIrqWake(state.clone())).map_err(|_| AxError::NoMemory)?;
        let irq_waker = Waker::from(wake);
        Arc::try_new(Self { state, irq_waker }).map_err(|_| AxError::NoMemory)
    }
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
        let terminal_source = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let poll_wake =
            Arc::try_new(NetPollWake(poll_source.clone())).map_err(|_| AxError::NoMemory)?;
        let poll_waker = Waker::from(poll_wake);
        let rx_worker = if service.has_rx_wake_capable_device() {
            Some(NetRxWorker::try_new()?)
        } else {
            None
        };
        let stack = Arc::try_new(Self {
            unix_namespace,
            packet_broker,
            listen_table,
            socket_set,
            service: Mutex::new(service),
            poll_source,
            terminal_source,
            poll_waker,
            rx_worker,
            tcp_ephemeral_port: Mutex::new(PORT_START),
            udp_ephemeral_port: Mutex::new(PORT_START),
            ipv4_conf_default_tag: AtomicI32::new(0),
            ipv4_conf_lo_tag: AtomicI32::new(0),
        })
        .map_err(|_| AxError::NoMemory)?;
        #[cfg(target_os = "none")]
        stack.packet_broker.start_drain_worker()?;
        #[cfg(target_os = "none")]
        if let Some(worker) = stack.rx_worker.clone() {
            if let Err(error) = stack.arm_rx_worker(&worker.irq_waker) {
                warn!("network receive wake source quarantined at startup: {error:?}");
            }
            if worker.state.terminal_reason().is_none() {
                Self::start_rx_worker(&stack, worker)?;
            }
        }
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
        let lo_dev = router.try_add_device(loopback)?;

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
        self.try_add_device(device)
            .expect("network device admission failed")
    }

    /// Fallibly add a network device without exceeding the router's fixed
    /// device capacity.
    pub fn try_add_device(&self, device: Box<dyn Device>) -> AxResult<usize> {
        // Perform the RX-ring admission check before taking any wake-source
        // ownership. A real Ethernet device without an IRQ cannot be
        // serviced by this stack, and must never become a published but
        // unwakeable interface. Software devices keep the ordinary bridge
        // path because they do not require an IRQ-backed owner.
        if device.rx_wake_required() && !device.rx_wake_capable() {
            return Err(AxError::Unsupported);
        }
        if self.rx_worker.is_none() && device.rx_wake_capable() {
            return Err(AxError::Unsupported);
        }
        let mut service = self.service.lock();
        if let Some(worker) = self.rx_worker.as_ref() {
            // The service lock is also the publication boundary used by the
            // terminal fence. No new device may register against a waker that
            // the fenced worker has already stopped owning.
            if worker.state.terminal_reason().is_some() {
                return Err(AxError::BadState);
            }
        }
        // Device publication is the only router admission step after source
        // registration. Reserve its fixed slot first while holding the same
        // service lock that serializes all add/poll operations; otherwise a
        // full router could consume a non-recyclable IRQ/source slot before
        // `try_add_device` rejects the device.
        if !service.router.has_device_capacity() {
            return Err(AxError::ResourceBusy);
        }
        let worker = self.rx_worker.as_ref();
        let pre_backlog = service.router.has_rx_backlog();
        let pre_generation = worker.map(|worker| worker.state.generation.load(Ordering::Acquire));

        // Reserve the new device's only wake owner before publishing the
        // device into the router. If admission fails, dropping this still
        // private device cancels any partial token and leaves no phantom
        // interface behind for a later retry.
        if let Some(worker) = worker {
            match device.register_rx_waker(&worker.irq_waker) {
                Ok(RxWakeSource::Armed) => {}
                Ok(RxWakeSource::Unavailable) => {
                    device.stop_rx_waker();
                    return Err(AxError::Unsupported);
                }
                Err(error) => {
                    device.stop_rx_waker();
                    return Err(crate::general::poll_registration_error(error));
                }
            }
        } else if let Err(error) = device.register_waker(&self.poll_waker) {
            device.stop_rx_waker();
            return Err(crate::general::poll_registration_error(error));
        }

        let index = service.router.try_add_device(device)?;
        if let Some(worker) = worker {
            let post_generation = worker.state.generation.load(Ordering::Acquire);
            let post_backlog = service.router.has_rx_backlog();
            let pre_generation = pre_generation.expect("worker snapshot missing");
            // The service lock is the publication boundary: no worker poll
            // can observe the new device before its wake source is armed.
            // Recheck both predicates after arming so work arriving from the
            // device callback during registration cannot leave a parked worker
            // waiting for a generation that the callback did not publish.
            if (!pre_backlog && post_backlog) || post_generation != pre_generation {
                worker.state.publish_wake();
            }
        } else {
            // Loopback-only stacks have no permanent worker; the user
            // readiness source owns each software-device bridge instead.
            // The bridge was reserved before publication, so a failure above
            // cannot leave an installed device without an owner.
            let post_backlog = service.router.has_rx_backlog();
            // A source can become readable while this reservation/publication
            // transaction is in flight. Recheck after arming and replay the
            // edge through the stable stack source so a waiter registered
            // before add_device cannot sleep through the publication.
            if !pre_backlog && post_backlog {
                self.poll_source.as_ref().wake();
            }
        }
        Ok(index)
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
        // A task-context transmit can fence an Ethernet source (for example
        // on a TX ownership error) while the permanent RX worker is parked.
        // Always run one bounded service pass after the lock is released so
        // the quarantine status and its replayable readiness edge reach both
        // the worker and user waiters, including the error path.
        self.poll_interfaces();
        match result {
            Ok(PacketSendProgress::NoImmediateIngress)
            | Ok(PacketSendProgress::ImmediateIngressQueued) => Ok(()),
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
            return self.add_terminal_events(ready);
        }
        self.poll_interfaces();
        self.add_terminal_events(endpoint.poll())
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
        let endpoint_interest =
            events.intersects(IoEvents::READABLE | IoEvents::HANGUP | IoEvents::ERROR);
        let terminal_interest = self.rx_worker.is_some() && !events.is_empty();
        let network_interest =
            (events.contains(IoEvents::READABLE) && !endpoint.is_detached()) || terminal_interest;
        let mut prepared = PreparedPollRegistration::try_new(
            usize::from(endpoint_interest) + usize::from(network_interest),
        )?;
        if endpoint_interest {
            endpoint.arm_readiness(&mut prepared, context.waker())?;
        }
        if network_interest {
            self.arm_readiness(&mut prepared, u64::MAX, context.waker())?;
            // Close the registration window for software devices too. A
            // device wake that arrived before this call leaves real receive
            // work behind; polling after both registrations converts that
            // work into endpoint readiness before publication.
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
    pub fn poll_interfaces(&self) -> NetPollStatus {
        if let Some(reason) = self.rx_terminal_reason() {
            return NetPollStatus::Terminal { reason };
        }
        let (status, quarantine_edge) = {
            let mut service = self.service.lock();
            let mut sockets = self.socket_set.inner.lock();
            let status = service.poll(&mut sockets);
            let quarantine_edge = service.router.take_quarantine_edge();
            (status, quarantine_edge)
        };
        self.packet_broker.drain_staged();
        if let Some(reason) = self.rx_terminal_reason() {
            return NetPollStatus::Terminal { reason };
        }
        match status {
            ServicePoll::Quiescent => NetPollStatus::Quiescent,
            ServicePoll::Continuation => NetPollStatus::Continuation,
            ServicePoll::Quarantined { continuation } => {
                // Source quarantine is a task-context event as well as an
                // IRQ event. Replay only the newly observed edge: the level
                // remains visible to pollers, but repeatedly waking an idle
                // healthy source would turn a blocking wait into a spin.
                if quarantine_edge {
                    if let Some(worker) = self.rx_worker.as_ref() {
                        worker.state.publish_wake();
                    }
                    self.poll_source.as_ref().wake();
                }
                NetPollStatus::Quarantined {
                    continuation,
                    edge: quarantine_edge,
                }
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
            _ => {
                ax_bail!(NotFound, "unknown IPv4 interface sysctl");
            }
        }
        Ok(())
    }

    /// Acquire a lock on the Service.
    pub(crate) fn get_service(&self) -> axsync::MutexGuard<'_, Service> {
        self.service.lock()
    }

    /// Arms this stack's stable task-context readiness source into
    /// caller-reserved aggregate storage. Physical stacks leave device wake
    /// ownership exclusively with their permanent receive worker; a
    /// loopback-only stack refreshes its ordinary bridge here.
    pub(crate) fn arm_readiness<'a>(
        &'a self,
        prepared: &mut PreparedPollRegistration<'a>,
        device_mask: u64,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        // Arm the stable stack source before refreshing device bridges. The
        // worker may finish a task-context pass while a user registration is
        // being assembled; publishing the source first prevents that wake
        // from landing in an unarmed window.
        prepared.arm(&self.poll_source, waker)?;
        if self.rx_terminal_reason().is_some() {
            // Consume this registration immediately. This is the level
            // trigger for terminal state: it also works with the readiness
            // adapter's no-op kernel waker, whose first update observes the
            // consumed token and cannot sleep forever on a stale source.
            self.poll_source.as_ref().wake();
            return Ok(());
        }
        if self.rx_worker.is_none() {
            self.service
                .lock()
                .register_waker(device_mask, &self.poll_waker)?;
        }
        // Close the terminal race after ordinary bridge admission as well.
        // The terminal fence publishes a poll-source wake after this check's
        // predecessor, so either edge is observable by the caller.
        if self.rx_terminal_reason().is_some() {
            self.poll_source.as_ref().wake();
        }
        Ok(())
    }

    /// Arms only the level-triggered terminal/removal source. Ordinary link
    /// and socket traffic never publishes this source, so a REMOVED-only
    /// waiter cannot be woken by unrelated readiness transitions.
    pub(crate) fn arm_terminal_readiness<'a>(
        &'a self,
        prepared: &mut PreparedPollRegistration<'a>,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        prepared.arm(&self.terminal_source, waker)?;
        if self.rx_terminal_reason().is_some() {
            // Check after arm: a terminal publication racing the registration
            // either consumes this token here or wakes it from the fencing
            // path, preserving the check-arm-check protocol.
            self.terminal_source.as_ref().wake();
        }
        Ok(())
    }

    fn arm_rx_worker(&self, waker: &Waker) -> Result<RxWakeRegistration, PollRegistrationError> {
        let Some(worker) = self.rx_worker.as_ref() else {
            return Err(PollRegistrationError::InvalidState);
        };
        let mut service = self.service.lock();
        if worker.state.terminal_reason().is_some() {
            return Err(PollRegistrationError::InvalidState);
        }
        let registration = service.register_rx_waker(waker);
        let quarantine_edge = service.router.take_quarantine_edge();
        if registration.has_owner() || service.has_rx_backlog() {
            // A source error is already represented in the router's
            // per-device quarantine bitmap. Keep the worker alive whenever a
            // healthy source or retained software backlog can make progress;
            // the next ServicePoll reports Quarantined without discarding the
            // healthy owners.
            drop(service);
            if quarantine_edge {
                // The source failed during this arm transaction, after the
                // worker's pre-arm generation snapshot. Replay the edge once
                // before the caller's post-arm check so it cannot park on the
                // generation that preceded the failure.
                worker.state.publish_wake();
                self.poll_source.as_ref().wake();
            }
            return Ok(registration);
        }

        // No source and no retained work means this worker has no future wake
        // owner. Fence only this aggregate lifecycle, after the service lock
        // has serialized all device admission/rearm operations.
        let error = registration
            .first_error
            .unwrap_or(PollRegistrationError::InvalidState);
        let fenced = worker
            .state
            .fence_terminal(NetRxTerminalReason::WakeSourceFailure);
        if fenced && worker.state.claim_ownership_stop() {
            service.stop_rx_waker();
        }
        drop(service);
        if fenced {
            // The lifecycle/reason are visible before the terminal readiness
            // edge is delivered to task-context waiters.
            self.poll_source.as_ref().wake();
            self.terminal_source.as_ref().wake();
        }
        Err(error)
    }

    fn fence_rx_terminal(&self, reason: NetRxTerminalReason) -> bool {
        let Some(worker) = self.rx_worker.as_ref() else {
            return false;
        };
        let service = self.service.lock();
        let fenced = worker.state.fence_terminal(reason);
        let terminal = fenced || worker.state.terminal_reason().is_some();
        if terminal && worker.state.claim_ownership_stop() {
            service.stop_rx_waker();
        }
        drop(service);
        if fenced {
            self.poll_source.as_ref().wake();
            self.terminal_source.as_ref().wake();
        }
        fenced
    }

    fn rx_terminal_reason(&self) -> Option<NetRxTerminalReason> {
        self.rx_worker
            .as_ref()
            .and_then(|worker| worker.state.terminal_reason())
    }

    pub(crate) fn add_terminal_events(&self, events: IoEvents) -> IoEvents {
        if self.rx_terminal_reason().is_some() {
            events | IoEvents::ERROR | IoEvents::HANGUP | IoEvents::REMOVED
        } else {
            events
        }
    }

    #[cfg(target_os = "none")]
    fn start_rx_worker(stack: &Arc<Self>, worker: Arc<NetRxWorker>) -> AxResult {
        let weak = Arc::downgrade(stack);
        let mut name = String::new();
        name.try_reserve_exact("net-rx".len())
            .map_err(|_| AxError::NoMemory)?;
        name.push_str("net-rx");
        axtask::try_spawn_with_name(move || net_rx_worker_loop(weak, worker), name).map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn has_rx_worker(&self) -> bool {
        self.rx_worker.is_some()
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

impl Drop for NetStack {
    fn drop(&mut self) {
        if let Some(worker) = &self.rx_worker {
            // The worker only holds a Weak<NetStack>, so stopping is the sole
            // normal exit. Publish it before cancelling device sources; the
            // wait predicate then cannot sleep through teardown.
            worker.state.stop();
            let service = self.service.lock();
            if worker.state.claim_ownership_stop() {
                service.stop_rx_waker();
            }
        }
    }
}

#[cfg(target_os = "none")]
fn net_rx_worker_loop(stack: Weak<NetStack>, worker: Arc<NetRxWorker>) {
    loop {
        let Some(stack_ref) = stack.upgrade() else {
            return;
        };
        if worker.state.is_stopping() {
            return;
        }

        let status = stack_ref.poll_interfaces();
        // IRQ and software-device callbacks only wake this task. User
        // readiness is published from task context after the bounded pass.
        // A quarantine edge was already replayed by poll_interfaces; only a
        // retained software continuation needs another wake. Sticky idle
        // quarantine must not manufacture a second edge for a new waiter.
        let publish_readiness = match status {
            NetPollStatus::Quarantined { continuation, edge } => continuation && !edge,
            _ => true,
        };
        if publish_readiness {
            stack_ref.poll_source.as_ref().wake();
        }
        if status.is_terminal() || worker.state.terminal_reason().is_some() {
            return;
        }
        if status.is_continuation() {
            drop(stack_ref);
            axtask::yield_now();
            continue;
        }

        let snapshot = worker.state.generation.load(Ordering::Acquire);
        // Keep all device wake sources owned by this worker. A registration
        // failure is terminal for that source, but never hands readiness back
        // to users or exits the worker; the service reports quarantine and
        // software devices continue to wake the same generation.
        match stack_ref.arm_rx_worker(&worker.irq_waker) {
            Ok(registration) => {
                if let Some(error) = registration.first_error {
                    warn!(
                        "network receive wake source quarantined; {} owner(s) remain: {error:?}",
                        registration.armed
                    );
                }
            }
            Err(error) => {
                warn!("network receive wake source aggregate fenced: {error:?}");
                return;
            }
        }
        let backlog = stack_ref.service.lock().has_rx_backlog();
        let generation_after = worker.state.generation.load(Ordering::Acquire);
        if backlog || generation_after != snapshot {
            drop(stack_ref);
            continue;
        }

        // Do not keep the upgraded Arc alive while parked. The task owns only
        // the worker state and a Weak<NetStack>; NetStack::drop must be able
        // to run, publish stopping, and wake this wait.
        drop(stack_ref);
        let state = worker.state.clone();
        let weak_stack = stack.clone();
        match state.wait.wait_until(|| {
            if state.is_stopping() || state.generation.load(Ordering::Acquire) != snapshot {
                return true;
            }
            weak_stack
                .upgrade()
                .is_some_and(|stack_ref| stack_ref.service.lock().has_rx_backlog())
        }) {
            Ok(()) => {}
            Err(error) => {
                warn!("network worker wait could not block: {error:?}");
                // A scheduler refusal is terminal for this worker. Release
                // the IRQ ownership before returning so the task remains
                // quiescent without an unbounded retry/yield loop.
                if let Some(stack_ref) = weak_stack.upgrade() {
                    stack_ref.fence_rx_terminal(NetRxTerminalReason::WaitFailure);
                } else {
                    worker
                        .state
                        .fence_terminal(NetRxTerminalReason::WaitFailure);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use smoltcp::{storage::PacketBuffer, time::Instant, wire::IpAddress};

    use super::*;
    use crate::{
        device::{Device, DeviceStats, InterfaceKind, LoopbackDevice, RxStep, RxWakeSource},
        listen_table::ListenTable,
        packet::{LinkPacketType, PacketDeviceContext, PacketProtocol, PacketSelector, PacketView},
        router::{MAX_DEVICES, RX_PASS_BUDGET, Router, Rule},
        service::Service,
        tcp::TcpSocket,
        udp::UdpSocket,
        wrapper::SocketSetWrapper,
    };

    const TEST_PROTOCOL: u16 = 0x88b5;
    const TEST_FRAME: [u8; 20] = [
        0x02, 0x11, 0x22, 0x33, 0x44, 0x55, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0x88, 0xb5, b'p',
        b'a', b'c', b'k', b'e', b't',
    ];

    struct CountingWake(AtomicUsize);

    struct ProbeDevice {
        quarantined: Arc<AtomicBool>,
        backlog: Arc<AtomicBool>,
        rx_wake_capable: bool,
        fail_rx_registration: bool,
        fail_ordinary_registration: bool,
        arrive_on_register: bool,
        rx_registrations: Arc<AtomicUsize>,
        ordinary_registrations: Arc<AtomicUsize>,
        receive_attempts: Arc<AtomicUsize>,
    }

    impl Device for ProbeDevice {
        fn name(&self) -> &str {
            "probe0"
        }

        fn stats(&self) -> DeviceStats {
            DeviceStats::default()
        }

        fn interface_kind(&self) -> InterfaceKind {
            InterfaceKind::Ethernet
        }

        fn mtu(&self) -> usize {
            1500
        }

        fn is_quarantined(&self) -> bool {
            self.quarantined.load(Ordering::Acquire)
        }

        fn rx_wake_capable(&self) -> bool {
            self.rx_wake_capable && !self.is_quarantined()
        }

        fn has_rx_backlog(&self) -> bool {
            self.backlog.load(Ordering::Acquire)
        }

        fn recv(
            &mut self,
            _context: PacketDeviceContext<'_>,
            _buffer: &mut PacketBuffer<()>,
            _timestamp: Instant,
        ) -> RxStep {
            self.receive_attempts.fetch_add(1, Ordering::Relaxed);
            if self.backlog.swap(false, Ordering::AcqRel) {
                RxStep::Consumed
            } else {
                RxStep::Idle
            }
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
            self.ordinary_registrations.fetch_add(1, Ordering::Relaxed);
            if self.arrive_on_register {
                self.backlog.store(true, Ordering::Release);
            }
            if self.fail_ordinary_registration {
                Err(PollRegistrationError::Quota)
            } else {
                Ok(())
            }
        }

        fn register_rx_waker(&self, _waker: &Waker) -> Result<RxWakeSource, PollRegistrationError> {
            self.rx_registrations.fetch_add(1, Ordering::Relaxed);
            if self.arrive_on_register {
                self.backlog.store(true, Ordering::Release);
            }
            if self.fail_rx_registration {
                Err(PollRegistrationError::Quota)
            } else {
                Ok(RxWakeSource::Armed)
            }
        }
    }

    struct NoIrqRxRingDevice;

    impl Device for NoIrqRxRingDevice {
        fn name(&self) -> &str {
            "no-irq-rx"
        }

        fn stats(&self) -> DeviceStats {
            DeviceStats::default()
        }

        fn interface_kind(&self) -> InterfaceKind {
            InterfaceKind::Ethernet
        }

        fn mtu(&self) -> usize {
            1500
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

        fn register_waker(&self, _waker: &Waker) -> Result<(), PollRegistrationError> {
            panic!("no-IRQ RX ring must be rejected before wake registration")
        }
    }

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn packet_endpoint(
        stack: &NetStack,
        protocol: PacketProtocol,
        capture_outgoing: bool,
    ) -> Arc<PacketEndpoint> {
        stack
            .subscribe_packets(PacketSelector::new(
                protocol,
                Some(1),
                PacketView::Raw,
                capture_outgoing,
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

    fn physical_stack_with_probe(
        fail_rx_registration: bool,
    ) -> (
        Arc<NetStack>,
        Arc<AtomicBool>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let socket_set = Arc::try_new(SocketSetWrapper::new()).unwrap();
        let listen_table = Arc::try_new(ListenTable::try_new().unwrap()).unwrap();
        let mut router = Router::new_loopback_only(listen_table.clone());
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        let quarantined = Arc::new(AtomicBool::new(false));
        let rx_registrations = Arc::new(AtomicUsize::new(0));
        let ordinary_registrations = Arc::new(AtomicUsize::new(0));
        let receive_attempts = Arc::new(AtomicUsize::new(0));
        router.add_device(Box::new(ProbeDevice {
            quarantined: quarantined.clone(),
            backlog: Arc::new(AtomicBool::new(false)),
            rx_wake_capable: true,
            fail_rx_registration,
            fail_ordinary_registration: false,
            arrive_on_register: false,
            rx_registrations: rx_registrations.clone(),
            ordinary_registrations: ordinary_registrations.clone(),
            receive_attempts: receive_attempts.clone(),
        }));
        router.add_rule(Rule::new(
            smoltcp::wire::Ipv4Cidr::new(smoltcp::wire::Ipv4Address::new(127, 0, 0, 1), 8).into(),
            None,
            loopback,
            smoltcp::wire::Ipv4Address::new(127, 0, 0, 1).into(),
        ));
        let service = Service::try_new(router, socket_set.clone()).unwrap();
        let stack = NetStack::try_new(listen_table, socket_set, service).unwrap();
        (
            stack,
            quarantined,
            rx_registrations,
            ordinary_registrations,
            receive_attempts,
        )
    }

    #[test]
    fn loopback_stack_has_no_worker_and_keeps_ordinary_bridge() {
        let stack = NetStack::new_loopback_only();
        assert!(!stack.has_rx_worker());

        let endpoint = packet_endpoint(&stack, PacketProtocol::All, true);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::READABLE)
            .unwrap();

        assert_eq!(registration.source_count(), 2);
        drop(registration);
    }

    #[test]
    fn no_irq_rx_ring_is_rejected_before_loopback_publication() {
        let stack = NetStack::new_loopback_only();
        let interfaces_before = stack.interfaces();

        assert_eq!(
            stack.try_add_device(Box::new(NoIrqRxRingDevice)),
            Err(AxError::Unsupported)
        );
        assert_eq!(stack.interfaces(), interfaces_before);
    }

    #[test]
    fn adding_device_rechecks_arrival_during_worker_arm() {
        let (stack, ..) = physical_stack_with_probe(false);
        let worker = stack.rx_worker.as_ref().unwrap();
        let generation_before = worker.state.generation.load(Ordering::Acquire);
        let backlog = Arc::new(AtomicBool::new(false));
        let rx_registrations = Arc::new(AtomicUsize::new(0));
        let receives = Arc::new(AtomicUsize::new(0));

        let index = stack
            .try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: backlog.clone(),
                rx_wake_capable: true,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: true,
                rx_registrations: rx_registrations.clone(),
                ordinary_registrations: Arc::new(AtomicUsize::new(0)),
                receive_attempts: receives.clone(),
            }))
            .unwrap();

        assert_eq!(index, 2);
        assert_eq!(rx_registrations.load(Ordering::Acquire), 1);
        assert_eq!(
            worker.state.generation.load(Ordering::Acquire),
            generation_before + 1
        );
        assert!(backlog.load(Ordering::Acquire));

        assert_eq!(stack.poll_interfaces(), NetPollStatus::Quiescent);
        assert_eq!(receives.load(Ordering::Acquire), 1);
        assert!(!backlog.load(Ordering::Acquire));
    }

    #[test]
    fn loopback_stack_reports_real_interface_and_routes() {
        let stack = NetStack::new_loopback_only();
        assert!(!stack.has_rx_worker());
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
    fn loopback_add_device_arms_bridge_and_replays_arrival() {
        let stack = NetStack::new_loopback_only();
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::READABLE)
            .unwrap();
        assert_eq!(registration.source_count(), 2);

        let backlog = Arc::new(AtomicBool::new(false));
        let ordinary_registrations = Arc::new(AtomicUsize::new(0));
        let receives = Arc::new(AtomicUsize::new(0));
        let index = stack
            .try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: backlog.clone(),
                rx_wake_capable: false,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: true,
                rx_registrations: Arc::new(AtomicUsize::new(0)),
                ordinary_registrations: ordinary_registrations.clone(),
                receive_attempts: receives.clone(),
            }))
            .unwrap();

        assert_eq!(index, 1);
        assert_eq!(ordinary_registrations.load(Ordering::Acquire), 1);
        assert!(backlog.load(Ordering::Acquire));
        assert!(wake_count.0.load(Ordering::Acquire) > 0);

        assert_eq!(stack.poll_interfaces(), NetPollStatus::Quiescent);
        assert_eq!(receives.load(Ordering::Acquire), 1);
        assert!(!backlog.load(Ordering::Acquire));
        drop(registration);
    }

    #[test]
    fn ordinary_add_failure_is_transactional_and_retryable() {
        let stack = NetStack::new_loopback_only();
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::READABLE)
            .unwrap();
        let interfaces_before = stack.interfaces();
        let routes_before = stack.routes();
        let wake_count_before = wake_count.0.load(Ordering::Acquire);
        let failed_registrations = Arc::new(AtomicUsize::new(0));

        let result = stack.try_add_device(Box::new(ProbeDevice {
            quarantined: Arc::new(AtomicBool::new(false)),
            backlog: Arc::new(AtomicBool::new(false)),
            rx_wake_capable: false,
            fail_rx_registration: false,
            fail_ordinary_registration: true,
            arrive_on_register: false,
            rx_registrations: Arc::new(AtomicUsize::new(0)),
            ordinary_registrations: failed_registrations.clone(),
            receive_attempts: Arc::new(AtomicUsize::new(0)),
        }));
        assert_eq!(result, Err(AxError::ResourceBusy));
        assert_eq!(failed_registrations.load(Ordering::Acquire), 1);
        assert_eq!(stack.interfaces(), interfaces_before);
        assert_eq!(stack.routes(), routes_before);
        assert!(stack.packet_device_capabilities(2).is_none());
        assert_eq!(registration.source_count(), 2);
        assert_eq!(wake_count.0.load(Ordering::Acquire), wake_count_before);

        let retry_registrations = Arc::new(AtomicUsize::new(0));
        let index = stack
            .try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: Arc::new(AtomicBool::new(false)),
                rx_wake_capable: false,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: false,
                rx_registrations: Arc::new(AtomicUsize::new(0)),
                ordinary_registrations: retry_registrations.clone(),
                receive_attempts: Arc::new(AtomicUsize::new(0)),
            }))
            .unwrap();
        assert_eq!(index, 1);
        assert_eq!(retry_registrations.load(Ordering::Acquire), 1);
        assert_eq!(stack.interfaces().len(), 2);
        assert!(stack.packet_device_capabilities(2).is_some());
        drop(registration);
    }

    #[test]
    fn over_capacity_adds_do_not_consume_source_slots() {
        let (stack, ..) = physical_stack_with_probe(false);
        while stack.interfaces().len() < MAX_DEVICES {
            stack
                .try_add_device(Box::new(ProbeDevice {
                    quarantined: Arc::new(AtomicBool::new(false)),
                    backlog: Arc::new(AtomicBool::new(false)),
                    rx_wake_capable: true,
                    fail_rx_registration: false,
                    fail_ordinary_registration: false,
                    arrive_on_register: false,
                    rx_registrations: Arc::new(AtomicUsize::new(0)),
                    ordinary_registrations: Arc::new(AtomicUsize::new(0)),
                    receive_attempts: Arc::new(AtomicUsize::new(0)),
                }))
                .unwrap();
        }

        for _ in 0..4 {
            let rx_registrations = Arc::new(AtomicUsize::new(0));
            let result = stack.try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: Arc::new(AtomicBool::new(false)),
                rx_wake_capable: true,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: false,
                rx_registrations: rx_registrations.clone(),
                ordinary_registrations: Arc::new(AtomicUsize::new(0)),
                receive_attempts: Arc::new(AtomicUsize::new(0)),
            }));
            assert_eq!(result, Err(AxError::ResourceBusy));
            assert_eq!(rx_registrations.load(Ordering::Acquire), 0);
            assert_eq!(stack.interfaces().len(), MAX_DEVICES);
        }

        // A later non-full router can still admit a distinct source after the
        // rejected attempts; the preflight never consumed their source slot.
        let (retry_stack, ..) = physical_stack_with_probe(false);
        let rx_registrations = Arc::new(AtomicUsize::new(0));
        let index = retry_stack
            .try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: Arc::new(AtomicBool::new(false)),
                rx_wake_capable: true,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: false,
                rx_registrations: rx_registrations.clone(),
                ordinary_registrations: Arc::new(AtomicUsize::new(0)),
                receive_attempts: Arc::new(AtomicUsize::new(0)),
            }))
            .unwrap();
        assert_eq!(index, 2);
        assert_eq!(rx_registrations.load(Ordering::Acquire), 1);
    }

    #[test]
    fn physical_quarantine_does_not_stop_software_backlog() {
        let (stack, quarantined, ..) = physical_stack_with_probe(false);
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        {
            let worker = stack.rx_worker.as_ref().unwrap();
            stack.arm_rx_worker(&worker.irq_waker).unwrap();
        }

        quarantined.store(true, Ordering::Release);
        queue_loopback_without_poll(&stack, endpoint.as_ref());
        assert_eq!(
            stack.poll_interfaces(),
            NetPollStatus::Quarantined {
                continuation: false,
                edge: true,
            }
        );
        assert_eq!(worker_terminal(&stack), None);
        let worker = stack.rx_worker.as_ref().unwrap();
        let rearm = stack.arm_rx_worker(&worker.irq_waker).unwrap();
        assert_eq!(rearm.armed, 1);
        assert_eq!(endpoint.queue_usage().0, 1);
    }

    #[test]
    fn sticky_quarantine_replays_readiness_only_on_new_edge() {
        let (stack, quarantined, ..) = physical_stack_with_probe(false);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake_count.clone());

        let mut first = PreparedPollRegistration::try_new(1).unwrap();
        stack.arm_readiness(&mut first, u64::MAX, &waker).unwrap();
        let first = first.commit().unwrap();
        quarantined.store(true, Ordering::Release);

        assert_eq!(
            stack.poll_interfaces(),
            NetPollStatus::Quarantined {
                continuation: false,
                edge: true,
            }
        );
        let wake_after_edge = wake_count.0.load(Ordering::Acquire);
        let generation_after_edge = stack
            .rx_worker
            .as_ref()
            .unwrap()
            .state
            .generation
            .load(Ordering::Acquire);
        assert_eq!(wake_after_edge, 1);
        drop(first);

        // Re-arm a blocking waiter after the sticky level is visible. An idle
        // healthy loopback source must be allowed to park; the quarantined
        // physical source is not a reason to replay the same wake forever.
        let mut second = PreparedPollRegistration::try_new(1).unwrap();
        stack.arm_readiness(&mut second, u64::MAX, &waker).unwrap();
        let second = second.commit().unwrap();
        assert_eq!(
            stack.poll_interfaces(),
            NetPollStatus::Quarantined {
                continuation: false,
                edge: false,
            }
        );
        assert_eq!(wake_count.0.load(Ordering::Acquire), wake_after_edge);
        assert_eq!(
            stack
                .rx_worker
                .as_ref()
                .unwrap()
                .state
                .generation
                .load(Ordering::Acquire),
            generation_after_edge
        );
        drop(second);
    }

    #[test]
    fn quarantined_physical_source_yields_for_software_backlog() {
        let (stack, quarantined, ..) = physical_stack_with_probe(false);
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        {
            let worker = stack.rx_worker.as_ref().unwrap();
            stack.arm_rx_worker(&worker.irq_waker).unwrap();
        }

        quarantined.store(true, Ordering::Release);
        for _ in 0..=RX_PASS_BUDGET {
            queue_loopback_without_poll(&stack, endpoint.as_ref());
        }

        let first = stack.poll_interfaces();
        assert_eq!(
            first,
            NetPollStatus::Quarantined {
                continuation: true,
                edge: true,
            }
        );
        assert!(first.is_quarantined());
        assert!(first.is_continuation());

        let second = stack.poll_interfaces();
        assert_eq!(
            second,
            NetPollStatus::Quarantined {
                continuation: false,
                edge: false,
            }
        );
        assert_eq!(endpoint.queue_usage().0, RX_PASS_BUDGET + 1);
    }

    #[test]
    fn failed_worker_rearm_quarantines_one_source_and_keeps_software_owner() {
        let (stack, _, rx_registrations, ordinary_registrations, receive_attempts) =
            physical_stack_with_probe(true);
        let worker = stack.rx_worker.as_ref().unwrap();
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let mut prepared = PreparedPollRegistration::try_new(1).unwrap();
        stack
            .arm_readiness(&mut prepared, u64::MAX, &Waker::from(wake_count.clone()))
            .unwrap();
        let registration = prepared.commit().unwrap();
        let generation_before_arm = worker.state.generation.load(Ordering::Acquire);
        let first = stack.arm_rx_worker(&worker.irq_waker).unwrap();
        assert_eq!(first.armed, 1);
        assert_eq!(first.failed, 1);
        assert_eq!(first.unavailable, 0);
        assert_eq!(first.first_error, Some(PollRegistrationError::Quota));
        // The failure is discovered during rearm, before the worker's wait
        // predicate is installed. It must replay one generation/source edge
        // into the already-armed waiter instead of relying on a later poll.
        assert_eq!(wake_count.0.load(Ordering::Acquire), 1);
        assert_eq!(
            worker.state.generation.load(Ordering::Acquire),
            generation_before_arm + 1
        );
        drop(registration);
        assert_eq!(rx_registrations.load(Ordering::Relaxed), 1);
        assert_eq!(ordinary_registrations.load(Ordering::Relaxed), 0);

        assert_eq!(worker.state.terminal_reason(), None);
        let second = stack.arm_rx_worker(&worker.irq_waker).unwrap();
        assert_eq!(second.armed, 1);
        assert_eq!(second.failed, 0);
        assert_eq!(second.first_error, None);
        assert_eq!(rx_registrations.load(Ordering::Relaxed), 1);
        assert_eq!(
            stack.poll_interfaces(),
            NetPollStatus::Quarantined {
                continuation: false,
                edge: false,
            }
        );
        assert_eq!(wake_count.0.load(Ordering::Acquire), 1);
        assert_eq!(receive_attempts.load(Ordering::Relaxed), 0);
        assert_eq!(ordinary_registrations.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn task_context_send_replays_quarantine_to_worker_and_readiness() {
        let (stack, quarantined, ..) = physical_stack_with_probe(false);
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), true);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let mut prepared = PreparedPollRegistration::try_new(1).unwrap();
        stack
            .arm_readiness(&mut prepared, u64::MAX, &Waker::from(wake_count.clone()))
            .unwrap();
        let registration = prepared.commit().unwrap();
        let wake_before = wake_count.0.load(Ordering::Acquire);

        // A task-context TX attempt against a fenced source still runs one
        // bounded service pass. The original operation remains unsupported,
        // but quarantine is visible and its replayable readiness edge wakes
        // the parked worker/user registration.
        quarantined.store(true, Ordering::Release);
        let result = stack.send_packet(
            2,
            endpoint.as_ref(),
            PacketSendRequest::Raw {
                protocol: TEST_PROTOCOL,
                frame: &TEST_FRAME,
            },
        );
        assert_eq!(result, Err(AxError::Unsupported));
        assert!(wake_count.0.load(Ordering::Acquire) > wake_before);
        assert_eq!(worker_terminal(&stack), None);
        drop(registration);
    }

    #[test]
    fn partial_rearm_preserves_healthy_nic_and_software_sources() {
        let (stack, _, failed_rx, ..) = physical_stack_with_probe(true);
        let healthy_rx = Arc::new(AtomicUsize::new(0));
        let index = stack
            .try_add_device(Box::new(ProbeDevice {
                quarantined: Arc::new(AtomicBool::new(false)),
                backlog: Arc::new(AtomicBool::new(false)),
                rx_wake_capable: true,
                fail_rx_registration: false,
                fail_ordinary_registration: false,
                arrive_on_register: false,
                rx_registrations: healthy_rx.clone(),
                ordinary_registrations: Arc::new(AtomicUsize::new(0)),
                receive_attempts: Arc::new(AtomicUsize::new(0)),
            }))
            .unwrap();
        assert_eq!(index, 2);
        assert_eq!(healthy_rx.load(Ordering::Acquire), 1);

        let worker = stack.rx_worker.as_ref().unwrap();
        let first = stack.arm_rx_worker(&worker.irq_waker).unwrap();
        assert_eq!(first.armed, 2); // loopback + healthy NIC
        assert_eq!(first.failed, 1);
        assert_eq!(failed_rx.load(Ordering::Acquire), 1);
        assert_eq!(healthy_rx.load(Ordering::Acquire), 2);
        assert_eq!(worker_terminal(&stack), None);

        let second = stack.arm_rx_worker(&worker.irq_waker).unwrap();
        assert_eq!(second.armed, 2);
        assert_eq!(second.failed, 0);
        assert_eq!(healthy_rx.load(Ordering::Acquire), 3);
    }

    #[test]
    fn terminal_before_packet_registration_is_replayed_by_stable_source() {
        let (stack, ..) = physical_stack_with_probe(false);
        let endpoint = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        assert!(stack.fence_rx_terminal(NetRxTerminalReason::WaitFailure));

        // The no-op waker is the synchronous readiness path's actual input.
        // Registration must still retain a source and consume it on the
        // terminal level edge instead of returning an empty wait forever.
        let mut context = Context::from_waker(Waker::noop());
        let registration = stack
            .register_packet_endpoint(&endpoint, &mut context, IoEvents::READABLE)
            .unwrap();
        assert_eq!(registration.source_count(), 2);
        let events = stack.poll_packet_endpoint(&endpoint);
        assert!(events.intersects(IoEvents::ERROR | IoEvents::HANGUP | IoEvents::REMOVED));
        drop(registration);
    }

    fn worker_terminal(stack: &NetStack) -> Option<NetRxTerminalReason> {
        stack
            .rx_worker
            .as_ref()
            .and_then(|worker| worker.state.terminal_reason())
    }

    #[test]
    fn terminal_readiness_wakes_waiters_before_and_after_publication() {
        let (stack, ..) = physical_stack_with_probe(false);
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake_count.clone());

        let mut prepared = PreparedPollRegistration::try_new(1).unwrap();
        stack
            .arm_readiness(&mut prepared, u64::MAX, &waker)
            .unwrap();
        let registration = prepared.commit().unwrap();
        assert_eq!(registration.source_count(), 1);

        assert!(stack.fence_rx_terminal(NetRxTerminalReason::WaitFailure));
        assert_eq!(wake_count.0.load(Ordering::Acquire), 1);
        drop(registration);

        let mut after = PreparedPollRegistration::try_new(1).unwrap();
        stack.arm_readiness(&mut after, u64::MAX, &waker).unwrap();
        let after = after.commit().unwrap();
        // A terminal registration retains the stable stack source and
        // consumes its token immediately. This is replayable for a no-op
        // kernel waker: the first update observes the consumed token.
        assert_eq!(after.source_count(), 1);
        assert_eq!(wake_count.0.load(Ordering::Acquire), 2);
        drop(after);
    }

    #[test]
    fn mixed_tcp_udp_interests_retain_terminal_source() {
        let (stack, ..) = physical_stack_with_probe(false);
        let tcp = TcpSocket::new(stack.clone()).unwrap();
        let udp = UdpSocket::new(stack.clone()).unwrap();
        let wake_count = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake_count.clone());
        let mut context = Context::from_waker(&waker);

        let tcp_registration = tcp
            .register(&mut context, IoEvents::READABLE | IoEvents::REMOVED)
            .unwrap();
        let udp_registration = udp
            .register(&mut context, IoEvents::READABLE | IoEvents::REMOVED)
            .unwrap();
        // Both sockets retain ordinary readiness and the dedicated terminal
        // source; TCP also retains its local-close source for READABLE.
        assert_eq!(tcp_registration.source_count(), 3);
        assert_eq!(udp_registration.source_count(), 3);

        assert!(stack.fence_rx_terminal(NetRxTerminalReason::WaitFailure));
        assert!(wake_count.0.load(Ordering::Acquire) > 0);
        assert!(tcp.poll().contains(IoEvents::REMOVED));
        assert!(udp.poll().contains(IoEvents::REMOVED));
        drop(tcp_registration);
        drop(udp_registration);
    }

    #[test]
    fn add_device_after_terminal_does_not_register_stopped_waker() {
        let (stack, ..) = physical_stack_with_probe(false);
        assert!(stack.fence_rx_terminal(NetRxTerminalReason::WakeSourceFailure));

        let rx_registrations = Arc::new(AtomicUsize::new(0));
        let result = stack.try_add_device(Box::new(ProbeDevice {
            quarantined: Arc::new(AtomicBool::new(false)),
            backlog: Arc::new(AtomicBool::new(false)),
            rx_wake_capable: true,
            fail_rx_registration: false,
            fail_ordinary_registration: false,
            arrive_on_register: false,
            rx_registrations: rx_registrations.clone(),
            ordinary_registrations: Arc::new(AtomicUsize::new(0)),
            receive_attempts: Arc::new(AtomicUsize::new(0)),
        }));
        assert_eq!(result, Err(AxError::BadState));
        assert_eq!(rx_registrations.load(Ordering::Acquire), 0);
        assert_eq!(stack.interfaces().len(), 2);
    }

    #[test]
    fn tcp_and_udp_poll_expose_terminal_network_state() {
        let (stack, ..) = physical_stack_with_probe(false);
        let tcp = TcpSocket::new(stack.clone()).unwrap();
        let udp = UdpSocket::new(stack.clone()).unwrap();
        assert!(stack.fence_rx_terminal(NetRxTerminalReason::WakeSourceFailure));

        for events in [tcp.poll(), udp.poll()] {
            assert!(events.contains(IoEvents::ERROR));
            assert!(events.contains(IoEvents::HANGUP));
            assert!(events.contains(IoEvents::REMOVED));
        }
    }

    #[test]
    fn irq_wake_only_publishes_generation_and_stop_is_terminal() {
        let state = Arc::new(NetRxWorkerState {
            generation: AtomicU64::new(0),
            lifecycle: AtomicU8::new(RX_WORKER_RUNNING),
            terminal_reason: AtomicU8::new(0),
            ownership_stopped: AtomicU8::new(0),
            wait: axtask::WaitQueue::new(),
        });
        let wake = Arc::new(NetRxIrqWake(state.clone()));
        Wake::wake_by_ref(&wake);
        assert_eq!(state.generation.load(Ordering::Acquire), 1);

        state.stop();
        Wake::wake_by_ref(&wake);
        assert_eq!(state.generation.load(Ordering::Acquire), 1);
        assert!(state.is_stopping());
    }

    #[test]
    fn persistent_wait_refusal_becomes_terminal_without_retry() {
        let state = NetRxWorkerState {
            generation: AtomicU64::new(0),
            lifecycle: AtomicU8::new(RX_WORKER_RUNNING),
            terminal_reason: AtomicU8::new(0),
            ownership_stopped: AtomicU8::new(0),
            wait: axtask::WaitQueue::new(),
        };

        let mut attempts = 0;
        for _ in 0..4 {
            let reason = if attempts == 0 {
                NetRxTerminalReason::WaitFailure
            } else {
                NetRxTerminalReason::WakeSourceFailure
            };
            if !state.fence_terminal(reason) {
                break;
            }
            attempts += 1;
        }

        assert_eq!(attempts, 1);
        assert_eq!(
            state.terminal_reason(),
            Some(NetRxTerminalReason::WaitFailure)
        );
        assert!(state.is_stopping());
        assert!(state.claim_ownership_stop());
        assert!(!state.claim_ownership_stop());
        state.publish_wake();
        assert_eq!(state.generation.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropped_physical_stack_releases_worker_weak_reference() {
        let (stack, ..) = physical_stack_with_probe(false);
        let weak = Arc::downgrade(&stack);
        assert!(weak.upgrade().is_some());
        drop(stack);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    #[should_panic(expected = "network receive generation exhausted")]
    fn irq_generation_does_not_wrap_into_a_lost_wake() {
        let state = Arc::new(NetRxWorkerState {
            generation: AtomicU64::new(u64::MAX),
            lifecycle: AtomicU8::new(RX_WORKER_RUNNING),
            terminal_reason: AtomicU8::new(0),
            ownership_stopped: AtomicU8::new(0),
            wait: axtask::WaitQueue::new(),
        });
        let wake = Arc::new(NetRxIrqWake(state));
        Wake::wake_by_ref(&wake);
    }

    #[test]
    fn direct_loopback_packet_send_retires_outgoing_and_ingress() {
        let stack = NetStack::new_loopback_only();
        let source = packet_endpoint(&stack, PacketProtocol::All, true);
        let observer = packet_endpoint(&stack, PacketProtocol::All, true);

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
        let source = packet_endpoint(&stack, PacketProtocol::All, true);
        let receiver = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
        queue_loopback_without_poll(&stack, source.as_ref());
        assert_eq!(receiver.queue_usage().0, 0);

        let record = stack.try_receive_packet(receiver.as_ref(), false).unwrap();
        assert_eq!(record.metadata().packet_type, LinkPacketType::OtherHost);
        assert_eq!(record.data(), TEST_FRAME);
    }

    #[test]
    fn packet_read_registration_closes_pending_device_wake_gap() {
        let stack = NetStack::new_loopback_only();
        let source = packet_endpoint(&stack, PacketProtocol::All, true);
        let receiver = packet_endpoint(&stack, PacketProtocol::Exact(TEST_PROTOCOL), false);
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
        let endpoint = packet_endpoint(&stack, PacketProtocol::All, true);
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
        drop(registration);
    }
}
