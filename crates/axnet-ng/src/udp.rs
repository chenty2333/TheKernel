use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError, ax_bail, ax_err_type};
use axio::prelude::*;
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;
use smoltcp::{
    iface::SocketHandle,
    phy::PacketMeta,
    socket::udp::{self as smol, UdpMetadata},
    storage::PacketMetadata,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address},
};
use spin::RwLock;

use crate::{
    RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    buffer::{
        normalized_socket_buffer_size, try_filled_buffer, try_zeroed_socket_buffer,
        udp_packet_slots,
    },
    consts::{UDP_RX_BUF_LEN, UDP_TX_BUF_LEN},
    general::GeneralOptions,
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
    wrapper::Transport,
};

const MAX_UDP_SEND_LEN: usize = u16::MAX as usize;
const UDP_SEND_COOPERATE_BYTES: usize = 256 * 1024;

/// Address family owned by one UDP socket from creation through teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UdpSocketFamily {
    Ipv4,
    Ipv6,
}

impl UdpSocketFamily {
    const fn unspecified_address(self) -> IpAddress {
        match self {
            Self::Ipv4 => IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            Self::Ipv6 => IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
        }
    }

    const fn accepts(self, address: IpAddress) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, IpAddress::Ipv4(_)) | (Self::Ipv6, IpAddress::Ipv6(_))
        )
    }
}

#[derive(Clone, Copy)]
struct UdpRouteState {
    peer_addr: Option<(IpEndpoint, IpAddress)>,
    reported_local_addr: Option<IpEndpoint>,
    device_mask: u64,
}

impl UdpRouteState {
    const fn new() -> Self {
        Self {
            peer_addr: None,
            reported_local_addr: None,
            device_mask: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct UdpLocalState {
    endpoint: Option<IpEndpoint>,
    address_locked: bool,
    port_locked: bool,
}

impl UdpLocalState {
    const fn new() -> Self {
        Self {
            endpoint: None,
            address_locked: false,
            port_locked: false,
        }
    }
}

fn new_udp_packet_buffer(requested: usize) -> AxResult<smol::PacketBuffer<'static>> {
    let size = normalized_socket_buffer_size(requested);
    Ok(smol::PacketBuffer::new(
        try_filled_buffer(udp_packet_slots(size), PacketMetadata::EMPTY)?,
        try_zeroed_socket_buffer(size)?,
    ))
}

pub(crate) fn new_udp_socket() -> AxResult<smol::Socket<'static>> {
    Ok(smol::Socket::new(
        new_udp_packet_buffer(UDP_RX_BUF_LEN)?,
        new_udp_packet_buffer(UDP_TX_BUF_LEN)?,
    ))
}

fn replace_udp_send_buffer(socket: &mut smol::Socket, requested: usize) -> AxResult<()> {
    socket
        .replace_send_buffer(new_udp_packet_buffer(requested)?)
        .map_err(|_| AxError::ResourceBusy)
}

fn replace_udp_recv_buffer(socket: &mut smol::Socket, requested: usize) -> AxResult<()> {
    socket
        .replace_recv_buffer(new_udp_packet_buffer(requested)?)
        .map_err(|_| AxError::ResourceBusy)
}

/// A UDP socket that provides POSIX-like APIs.
pub struct UdpSocket {
    stack: Arc<NetStack>,
    handle: SocketHandle,
    family: UdpSocketFamily,
    transition: Mutex<()>,
    local_state: RwLock<UdpLocalState>,
    route_state: RwLock<UdpRouteState>,
    send_bytes_since_yield: AtomicUsize,
    rx_shutdown: AtomicBool,
    tx_shutdown: AtomicBool,
    poll_state: PollSet,

    general: GeneralOptions,
}

impl UdpSocket {
    pub(crate) fn set_pending_error(&self, error: LinuxError) {
        self.general.set_pending_error(error);
    }

    /// Returns the next queued UDP payload length without consuming it.
    pub(crate) fn recv_pending_len(&self) -> AxResult<usize> {
        if self.rx_shutdown.load(Ordering::Acquire) {
            return Ok(0);
        }
        self.stack.poll_interfaces();
        Ok(self.with_smol_socket(|socket| socket.peek().map_or(0, |(data, _)| data.len())))
    }

    pub(crate) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.general
            .transfer_poller(self, direction, effective_nonblocking, attempt)
    }

    /// Creates a new UDP socket bound to the given network stack.
    pub fn new(stack: Arc<NetStack>) -> AxResult<Self> {
        Self::new_with_family(stack, UdpSocketFamily::Ipv4)
    }

    /// Creates a UDP socket with an immutable Internet address family.
    pub fn new_with_family(stack: Arc<NetStack>, family: UdpSocketFamily) -> AxResult<Self> {
        let socket = new_udp_socket()?;
        let handle = stack.socket_set.add(socket)?;

        Ok(Self {
            stack,
            handle,
            family,
            transition: Mutex::new(()),
            local_state: RwLock::new(UdpLocalState::new()),
            route_state: RwLock::new(UdpRouteState::new()),
            send_bytes_since_yield: AtomicUsize::new(0),
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
            poll_state: PollSet::new(),

            general: GeneralOptions::new(),
        })
    }

    fn validate_family(&self, address: IpAddress) -> AxResult<()> {
        if self.family.accepts(address) {
            Ok(())
        } else {
            Err(LinuxError::EAFNOSUPPORT.into())
        }
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        self.stack
            .socket_set
            .with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    fn remote_endpoint(&self) -> AxResult<(IpEndpoint, IpAddress)> {
        self.route_state
            .read()
            .peer_addr
            .ok_or(AxError::NotConnected)
    }

    fn local_endpoint(&self) -> Option<IpEndpoint> {
        self.local_state.read().endpoint
    }

    fn publish_connected_route(&self, remote: IpEndpoint, source: IpAddress, device_mask: u64) {
        let reported_local_addr = self.reported_local_for(source);
        self.with_smol_socket(|socket| {
            socket.set_remote_endpoint(Some(remote));
        });
        let mut route = self.route_state.write();
        *route = UdpRouteState {
            peer_addr: Some((remote, source)),
            reported_local_addr,
            device_mask,
        };
        drop(route);
        self.poll_state.wake();
    }

    fn bound_source_addr(&self) -> Option<IpAddress> {
        self.local_state
            .read()
            .endpoint
            .as_ref()
            .and_then(|endpoint| (!endpoint.addr.is_unspecified()).then_some(endpoint.addr))
    }

    fn effective_local_addr(&self) -> Option<IpEndpoint> {
        let reported = self.route_state.read().reported_local_addr;
        reported.or_else(|| self.local_endpoint())
    }

    fn reported_local_for(&self, addr: IpAddress) -> Option<IpEndpoint> {
        let bound = self.local_endpoint()?;
        if bound.port == 0 {
            return None;
        }
        if bound.addr.is_unspecified() {
            Some(IpEndpoint {
                addr,
                port: bound.port,
            })
        } else {
            None
        }
    }

    fn record_unconnected_route(&self, device_mask: u64, source: IpAddress) {
        let reported_local_addr = self.reported_local_for(source);
        let mut route = self.route_state.write();
        let mut changed = false;
        if route.peer_addr.is_none() {
            changed = route.reported_local_addr != reported_local_addr
                || route.device_mask != device_mask;
            route.reported_local_addr = reported_local_addr;
            route.device_mask = device_mask;
        }
        drop(route);
        if changed {
            self.poll_state.wake();
        }
    }

    fn device_mask_for_local(&self, local: Option<IpEndpoint>) -> u64 {
        let Some(bound) = local else {
            return 0;
        };
        if bound.port == 0 {
            return 0;
        }
        let endpoint = IpListenEndpoint {
            addr: (!bound.addr.is_unspecified()).then_some(bound.addr),
            port: bound.port,
        };
        self.stack.get_service().device_mask_for(&endpoint)
    }

    fn ensure_autobound(&self) -> AxResult<()> {
        let _transition = self.transition.lock();
        self.ensure_autobound_locked()
    }

    fn ensure_autobound_locked(&self) -> AxResult<()> {
        let local = *self.local_state.read();
        if local.endpoint.is_some_and(|endpoint| endpoint.port != 0) {
            return Ok(());
        }

        let port = self.stack.udp_ephemeral_port()?;
        let addr = local
            .endpoint
            .map(|endpoint| endpoint.addr)
            .unwrap_or_else(|| self.family.unspecified_address());
        let local_endpoint = IpEndpoint { addr, port };
        self.stack.get_service().validate_bind_addr(addr)?;
        let endpoint = IpListenEndpoint {
            addr: (!addr.is_unspecified()).then_some(addr),
            port,
        };
        if !self.general.reuse_address() {
            self.stack.socket_set.bind_check(
                Transport::Udp,
                (!addr.is_unspecified()).then_some(addr),
                port,
            )?;
        }
        self.with_smol_socket(|socket| {
            socket.bind(endpoint).map_err(|error| match error {
                smol::BindError::InvalidState => ax_err_type!(InvalidInput, "already bound"),
                smol::BindError::Unaddressable => {
                    ax_err_type!(ConnectionRefused, "unaddressable")
                }
            })
        })?;
        let device_mask = self.device_mask_for_local(Some(local_endpoint));
        self.local_state.write().endpoint = Some(local_endpoint);
        let mut route = self.route_state.write();
        route.reported_local_addr = None;
        route.device_mask = device_mask;
        drop(route);
        self.poll_state.wake();
        Ok(())
    }

    fn unspecified_like(addr: IpAddress) -> IpAddress {
        match addr {
            IpAddress::Ipv4(_) => IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            IpAddress::Ipv6(_) => IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
        }
    }

    fn note_udp_send_progress(&self, sent: usize) {
        let mut should_yield = false;
        let _ = self.send_bytes_since_yield.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| {
                let next = current.saturating_add(sent);
                should_yield = next >= UDP_SEND_COOPERATE_BYTES;
                Some(if should_yield { 0 } else { next })
            },
        );
        if should_yield {
            axtask::yield_now();
        }
    }

    pub fn set_filter(
        &self,
        _filter: Option<alloc::sync::Arc<dyn crate::SocketFilter>>,
    ) -> AxResult<()> {
        Err(AxError::Unsupported)
    }

    pub fn disconnect(&self) {
        let _transition = self.transition.lock();
        let local = *self.local_state.read();
        let retained = local.endpoint.map(|endpoint| {
            if local.port_locked {
                IpEndpoint {
                    addr: if local.address_locked {
                        endpoint.addr
                    } else {
                        Self::unspecified_like(endpoint.addr)
                    },
                    port: endpoint.port,
                }
            } else if local.address_locked {
                IpEndpoint {
                    addr: endpoint.addr,
                    port: 0,
                }
            } else {
                IpEndpoint {
                    addr: Self::unspecified_like(endpoint.addr),
                    port: 0,
                }
            }
        });
        let device_mask = self.device_mask_for_local(retained);
        self.with_smol_socket(|socket| {
            if local.port_locked {
                socket.set_remote_endpoint(None);
            } else {
                socket.unbind();
            }
        });
        self.local_state.write().endpoint = retained;
        let mut route = self.route_state.write();
        *route = UdpRouteState {
            peer_addr: None,
            reported_local_addr: None,
            device_mask,
        };
        drop(route);
        self.poll_state.wake();
    }
}

impl Configurable for UdpSocket {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, option: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(option)? {
            return Ok(true);
        }
        match option {
            O::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    **ttl = socket.hop_limit().unwrap_or(64);
                });
            }
            O::SendBuffer(size) => {
                **size = self.with_smol_socket(|socket| socket.payload_send_capacity());
            }
            O::ReceiveBuffer(size) => {
                **size = self.with_smol_socket(|socket| socket.payload_recv_capacity());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> AxResult<bool> {
        use SetSocketOption as O;

        if self.general.set_option_inner(option)? {
            return Ok(true);
        }
        match option {
            O::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    socket.set_hop_limit(Some(*ttl));
                });
            }
            O::SendBuffer(size) | O::SendBufferForce(size) => {
                self.with_smol_socket(|socket| replace_udp_send_buffer(socket, *size))?;
            }
            O::ReceiveBuffer(size) | O::ReceiveBufferForce(size) => {
                self.with_smol_socket(|socket| replace_udp_recv_buffer(socket, *size))?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
impl SocketOps for UdpSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult {
        let mut local_addr = local_addr.into_ip()?;
        self.validate_family(local_addr.ip().into())?;
        let requested_port = local_addr.port();
        let _transition = self.transition.lock();
        let current_local = self.local_state.read().endpoint;

        if local_addr.port() == 0 {
            local_addr.set_port(self.stack.udp_ephemeral_port()?);
        }
        if current_local.is_some_and(|endpoint| endpoint.port != 0) {
            ax_bail!(InvalidInput, "already bound");
        }

        let local_endpoint = IpEndpoint::from(local_addr);
        self.stack
            .get_service()
            .validate_bind_addr(local_endpoint.addr)?;
        let endpoint = IpListenEndpoint {
            addr: (!local_endpoint.addr.is_unspecified()).then_some(local_endpoint.addr),
            port: local_endpoint.port,
        };

        if !self.general.reuse_address() {
            // Check if the address is already in use
            self.stack.socket_set.bind_check(
                Transport::Udp,
                (!local_endpoint.addr.is_unspecified()).then_some(local_endpoint.addr),
                local_endpoint.port,
            )?;
        }

        self.with_smol_socket(|socket| {
            socket.bind(endpoint).map_err(|e| match e {
                smol::BindError::InvalidState => ax_err_type!(InvalidInput, "already bound"),
                smol::BindError::Unaddressable => ax_err_type!(ConnectionRefused, "unaddressable"),
            })
        })?;
        let device_mask = self.device_mask_for_local(Some(local_endpoint));

        *self.local_state.write() = UdpLocalState {
            endpoint: Some(local_endpoint),
            address_locked: !local_endpoint.addr.is_unspecified(),
            port_locked: requested_port != 0,
        };
        let mut route = self.route_state.write();
        route.reported_local_addr = None;
        route.device_mask = device_mask;
        drop(route);
        self.poll_state.wake();
        info!("UDP socket {}: bound on {}", self.handle, endpoint);
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let remote_addr = remote_addr.into_ip()?;
        self.validate_family(remote_addr.ip().into())?;
        let _transition = self.transition.lock();
        self.ensure_autobound_locked()?;

        let remote_addr = IpEndpoint::from(remote_addr);
        let bound_source = self.bound_source_addr();
        let dont_route = self.general.dont_route();
        let outbound = self.stack.get_service().resolve_outbound_with_dont_route(
            &remote_addr.addr,
            bound_source,
            dont_route,
        )?;
        // Linux reports the selected source address after a connected UDP
        // socket resolves its route. Keep the wildcard bind semantics in the
        // actual socket state, but surface the concrete source address through
        // getsockname/local_addr so user space can advertise a usable reply
        // endpoint after an implicit bind.
        self.publish_connected_route(remote_addr, outbound.src_addr, outbound.device_mask);
        debug!("UDP socket {}: connected to {}", self.handle, remote_addr);
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if !options.cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        if self.tx_shutdown.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        let (remote_addr, source_addr, explicit_device_mask) = match options.to {
            Some(addr) => {
                let addr = IpEndpoint::from(addr.into_ip()?);
                self.validate_family(addr.addr)?;
                let bound_source = self.bound_source_addr();
                let dont_route = self.general.dont_route();
                let outbound = self.stack.get_service().resolve_outbound_with_dont_route(
                    &addr.addr,
                    bound_source,
                    dont_route,
                )?;
                (addr, outbound.src_addr, Some(outbound.device_mask))
            }
            None => {
                let (remote, source) = self.remote_endpoint()?;
                (remote, source, None)
            }
        };
        if remote_addr.port == 0 || remote_addr.addr.is_unspecified() {
            ax_bail!(InvalidInput, "invalid address");
        }
        let payload_len = src.remaining();
        if payload_len > MAX_UDP_SEND_LEN {
            return Err(LinuxError::EMSGSIZE.into());
        }
        // Datagram publication is atomic. Copy the bounded payload into
        // fallibly allocated storage before asking smoltcp to enqueue a packet;
        // a short/erroring reader must never leave a partially initialized
        // datagram in the transmit queue or reach a user-triggerable assert.
        let mut payload = try_filled_buffer(payload_len, 0u8)?;
        src.read_exact(&mut payload)?;

        self.ensure_autobound()?;
        if let Some(device_mask) = explicit_device_mask {
            self.record_unconnected_route(device_mask, source_addr);
        }
        let sent = self.general.send_poller_with_effective_nonblocking(
            self,
            effective_nonblocking,
            || {
                self.stack.poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.is_open() {
                        // not connected
                        Err(ax_err_type!(NotConnected))
                    } else if !socket.can_send() {
                        Err(AxError::WouldBlock)
                    } else {
                        let buf = socket
                            .send(
                                payload.len(),
                                UdpMetadata {
                                    endpoint: remote_addr,
                                    local_address: Some(source_addr),
                                    meta: PacketMeta::default(),
                                },
                            )
                            .map_err(|e| match e {
                                smol::SendError::BufferFull => AxError::WouldBlock,
                                smol::SendError::Unaddressable => {
                                    ax_err_type!(ConnectionRefused, "unaddressable")
                                }
                            })?;
                        buf.copy_from_slice(&payload);
                        Ok(payload.len())
                    }
                })
            },
        )?;

        // Push freshly queued datagrams through the interface/router path so
        // loopback receivers observe readiness immediately.
        self.stack.poll_interfaces();
        if sent > 0 {
            self.note_udp_send_progress(sent);
        }
        Ok(sent)
    }

    fn recv(&self, mut dst: impl Write, options: RecvOptions) -> AxResult<usize> {
        if self.rx_shutdown.load(Ordering::Acquire) {
            return Ok(0);
        }

        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        let flags = options.flags;
        let mut sender_output = options.from;

        self.general
            .recv_poller_with_effective_nonblocking(self, effective_nonblocking, || {
                self.stack.poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.can_recv() {
                        return Err(AxError::WouldBlock);
                    }
                    let result = if flags.contains(RecvFlags::PEEK) {
                        socket.peek().map(|(data, meta)| (data, *meta))
                    } else {
                        socket.recv()
                    };
                    match result {
                        Ok((src, meta)) => {
                            if let Some(remote_addr) = sender_output.as_deref_mut() {
                                *remote_addr = SocketAddrEx::Ip(meta.endpoint.into());
                            }

                            let read = dst.write(src)?;
                            if read < src.len() {
                                warn!("UDP message truncated: {} -> {} bytes", src.len(), read);
                            }

                            Ok(if flags.contains(RecvFlags::TRUNCATE) {
                                src.len()
                            } else {
                                read
                            })
                        }
                        Err(smol::RecvError::Exhausted) => Err(AxError::WouldBlock),
                        Err(smol::RecvError::Truncated) => Err(LinuxError::EMSGSIZE.into()),
                    }
                })
            })
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        let addr = self.effective_local_addr().unwrap_or(IpEndpoint {
            addr: self.family.unspecified_address(),
            port: 0,
        });
        Ok(SocketAddrEx::Ip(addr.into()))
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        self.remote_endpoint()
            .map(|it| it.0.into())
            .map(SocketAddrEx::Ip)
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        if self.route_state.read().peer_addr.is_none() {
            return Err(AxError::NotConnected);
        }
        if how.has_read() {
            self.rx_shutdown.store(true, Ordering::Release);
        }
        if how.has_write() {
            self.tx_shutdown.store(true, Ordering::Release);
        }
        self.poll_state.wake();
        Ok(())
    }
}

impl Pollable for UdpSocket {
    fn poll(&self) -> IoEvents {
        self.stack.poll_interfaces();
        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            let rx_shutdown = self.rx_shutdown.load(Ordering::Acquire);
            events.set(IoEvents::READABLE, rx_shutdown || socket.can_recv());
            events.set(
                IoEvents::WRITABLE,
                !self.tx_shutdown.load(Ordering::Acquire) && socket.can_send(),
            );
            events.set(IoEvents::READ_HANGUP, rx_shutdown);
        });
        let events = self.general.add_pending_error_event(events);
        self.stack.add_terminal_events(events)
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let network = events.intersects(
            IoEvents::READABLE
                | IoEvents::WRITABLE
                | IoEvents::READ_HANGUP
                | IoEvents::ERROR
                | IoEvents::HANGUP,
        );
        let terminal = events.contains(IoEvents::REMOVED);
        let socket_state = network;
        let mut prepared = PreparedPollRegistration::try_new(
            usize::from(socket_state) + usize::from(network) + usize::from(terminal),
        )?;
        if socket_state {
            prepared.arm(&self.poll_state, context.waker())?;
        }
        if network {
            let device_mask = self.route_state.read().device_mask;
            self.stack
                .arm_readiness(&mut prepared, device_mask, context.waker())?;
        }
        if terminal {
            self.stack
                .arm_terminal_readiness(&mut prepared, context.waker())?;
        }
        prepared.commit()
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.with_smol_socket(|socket| socket.close());
        self.stack.socket_set.remove(self.handle);
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use super::*;
    use crate::consts::{SOCKET_BUFFER_MAX, SOCKET_BUFFER_MIN};

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn endpoint(port: u16) -> SocketAddrEx {
        SocketAddrEx::Ip(SocketAddr::new(LOOPBACK, port))
    }

    fn wire_endpoint(port: u16) -> IpEndpoint {
        IpEndpoint::from(SocketAddr::new(LOOPBACK, port))
    }

    fn endpoint_v6(port: u16) -> SocketAddrEx {
        SocketAddrEx::Ip(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port))
    }

    fn send_datagram(sender: &UdpSocket, destination: u16, payload: &[u8]) {
        assert_eq!(
            sender
                .send(
                    payload,
                    SendOptions {
                        to: Some(endpoint(destination)),
                        ..SendOptions::default()
                    },
                )
                .unwrap(),
            payload.len()
        );
    }

    #[test]
    fn replacement_changes_udp_storage_capacity() {
        let mut socket = new_udp_socket().unwrap();

        replace_udp_send_buffer(&mut socket, 64 * 1024).unwrap();
        replace_udp_recv_buffer(&mut socket, 0).unwrap();
        assert_eq!(socket.payload_send_capacity(), 64 * 1024);
        assert_eq!(socket.payload_recv_capacity(), SOCKET_BUFFER_MIN);

        replace_udp_recv_buffer(&mut socket, usize::MAX).unwrap();
        assert_eq!(socket.payload_recv_capacity(), SOCKET_BUFFER_MAX);
    }

    #[test]
    fn removed_only_registration_arms_udp_terminal_source() {
        let stack = NetStack::new_loopback_only();
        let socket = UdpSocket::new(stack).unwrap();
        let mut context = Context::from_waker(core::task::Waker::noop());
        let registration = socket.register(&mut context, IoEvents::REMOVED).unwrap();
        // REMOVED-only waits retain only the dedicated terminal source; the
        // ordinary UDP state source would wake on unrelated traffic.
        assert_eq!(registration.source_count(), 1);
    }

    #[test]
    fn removed_only_udp_wait_ignores_ordinary_socket_traffic() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        receiver.bind(endpoint(31_206)).unwrap();
        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let registration = receiver.register(&mut context, IoEvents::REMOVED).unwrap();

        let sender = UdpSocket::new(stack).unwrap();
        send_datagram(&sender, 31_206, b"ordinary");
        assert_eq!(wake.0.load(Ordering::Acquire), 0);
        drop(registration);
    }

    #[test]
    fn bound_unconnected_udp_receive_does_not_require_sender_output() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let sender = UdpSocket::new(stack).unwrap();
        receiver.bind(endpoint(31_100)).unwrap();

        send_datagram(&sender, 31_100, b"without-address");
        let mut first = [0_u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut first[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"without-address".len()
        );
        assert_eq!(&first[..b"without-address".len()], b"without-address");

        send_datagram(&sender, 31_100, b"with-address");
        let mut source = endpoint(1);
        let mut second = [0_u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut second[..],
                    RecvOptions {
                        from: Some(&mut source),
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"with-address".len()
        );
        assert_eq!(&second[..b"with-address".len()], b"with-address");
        assert!(matches!(source, SocketAddrEx::Ip(address) if address.ip() == LOOPBACK));
    }

    #[test]
    fn udp_recv_pending_len_reports_only_the_next_datagram_without_consuming() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let sender = UdpSocket::new(stack).unwrap();
        receiver.bind(endpoint(31_130)).unwrap();

        send_datagram(&sender, 31_130, b"first");
        send_datagram(&sender, 31_130, b"second-packet");
        assert_eq!(receiver.recv_pending_len().unwrap(), 5);
        assert_eq!(receiver.recv_pending_len().unwrap(), 5);

        let mut bytes = [0u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut bytes[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            5
        );
        assert_eq!(&bytes[..5], b"first");
        assert_eq!(receiver.recv_pending_len().unwrap(), 13);

        assert_eq!(
            receiver
                .recv(
                    &mut bytes[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            13
        );
        assert_eq!(&bytes[..13], b"second-packet");
        assert_eq!(receiver.recv_pending_len().unwrap(), 0);
    }

    #[test]
    fn connected_udp_filters_peer_independently_of_sender_output() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let expected = UdpSocket::new(stack.clone()).unwrap();
        let unexpected = UdpSocket::new(stack).unwrap();
        receiver.bind(endpoint(31_110)).unwrap();
        expected.bind(endpoint(31_111)).unwrap();
        unexpected.bind(endpoint(31_112)).unwrap();
        receiver.connect(endpoint(31_111)).unwrap();

        let receive_slots = receiver.with_smol_socket(|socket| socket.packet_recv_capacity());
        for _ in 0..receive_slots.saturating_add(2) {
            send_datagram(&unexpected, 31_110, b"reject");
        }
        assert!(!receiver.poll().contains(IoEvents::READABLE));
        assert_eq!(receiver.with_smol_socket(|socket| socket.recv_queue()), 0);

        send_datagram(&expected, 31_110, b"accept");
        assert!(receiver.poll().contains(IoEvents::READABLE));

        let mut source = endpoint(1);
        let mut output = [0_u8; 16];
        assert_eq!(
            receiver
                .recv(
                    &mut output[..],
                    RecvOptions {
                        from: Some(&mut source),
                        flags: RecvFlags::PEEK | RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"accept".len()
        );
        assert_eq!(&output[..b"accept".len()], b"accept");
        assert!(matches!(source, SocketAddrEx::Ip(address) if address.port() == 31_111));

        let mut consumed = [0_u8; 16];
        assert_eq!(
            receiver
                .recv(
                    &mut consumed[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"accept".len()
        );
        assert_eq!(&consumed[..b"accept".len()], b"accept");
    }

    #[test]
    fn udp_connection_changes_preserve_admission_time_queue_order() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let first_peer = UdpSocket::new(stack.clone()).unwrap();
        let second_peer = UdpSocket::new(stack.clone()).unwrap();
        let unconnected_peer = UdpSocket::new(stack).unwrap();
        receiver.bind(endpoint(31_120)).unwrap();
        first_peer.bind(endpoint(31_121)).unwrap();
        second_peer.bind(endpoint(31_122)).unwrap();
        unconnected_peer.bind(endpoint(31_123)).unwrap();

        receiver.connect(endpoint(31_121)).unwrap();
        assert_eq!(
            receiver.with_smol_socket(|socket| socket.remote_endpoint()),
            Some(wire_endpoint(31_121))
        );
        {
            let route = receiver.route_state.read();
            assert_eq!(
                route.peer_addr,
                Some((wire_endpoint(31_121), LOOPBACK.into()))
            );
            assert_eq!(route.reported_local_addr, None);
            assert_ne!(route.device_mask, 0);
        }
        send_datagram(&first_peer, 31_120, b"first-epoch");

        receiver.connect(endpoint(31_122)).unwrap();
        assert_eq!(
            receiver.with_smol_socket(|socket| socket.remote_endpoint()),
            Some(wire_endpoint(31_122))
        );
        send_datagram(&first_peer, 31_120, b"rejected-after-reconnect");
        send_datagram(&second_peer, 31_120, b"second-epoch");

        let mut old_source = endpoint(1);
        let mut old_output = [0_u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut old_output[..],
                    RecvOptions {
                        from: Some(&mut old_source),
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"first-epoch".len()
        );
        assert_eq!(&old_output[..b"first-epoch".len()], b"first-epoch");
        assert!(matches!(old_source, SocketAddrEx::Ip(address) if address.port() == 31_121));

        receiver.disconnect();
        assert_eq!(
            receiver.with_smol_socket(|socket| socket.remote_endpoint()),
            None
        );
        let expected_device_mask = receiver.device_mask_for_local(receiver.local_endpoint());
        {
            let route = receiver.route_state.read();
            assert_eq!(route.peer_addr, None);
            assert_eq!(route.reported_local_addr, None);
            assert_eq!(route.device_mask, expected_device_mask);
        }
        send_datagram(&unconnected_peer, 31_120, b"unconnected-epoch");

        for (payload, source_port) in [
            (&b"second-epoch"[..], 31_122),
            (&b"unconnected-epoch"[..], 31_123),
        ] {
            let mut source = endpoint(1);
            let mut output = [0_u8; 32];
            assert_eq!(
                receiver
                    .recv(
                        &mut output[..],
                        RecvOptions {
                            from: Some(&mut source),
                            flags: RecvFlags::DONT_WAIT,
                            ..RecvOptions::default()
                        },
                    )
                    .unwrap(),
                payload.len()
            );
            assert_eq!(&output[..payload.len()], payload);
            assert!(matches!(source, SocketAddrEx::Ip(address) if address.port() == source_port));
        }
        assert!(!receiver.poll().contains(IoEvents::READABLE));
    }

    #[test]
    fn implicit_bind_disconnect_releases_port_but_preserves_rx_queue() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let peer = UdpSocket::new(stack).unwrap();
        peer.bind(endpoint(31_131)).unwrap();

        receiver.connect(endpoint(31_131)).unwrap();
        let assigned_port = match receiver.local_addr().unwrap() {
            SocketAddrEx::Ip(address) => address.port(),
            SocketAddrEx::Unix(_) => unreachable!(),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => unreachable!(),
        };
        assert_ne!(assigned_port, 0);
        send_datagram(&peer, assigned_port, b"admitted-before-disconnect");
        assert!(receiver.poll().contains(IoEvents::READABLE));

        receiver.disconnect();

        assert_eq!(
            receiver.local_addr().unwrap().into_ip().unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        );
        assert_eq!(
            receiver.with_smol_socket(|socket| (socket.endpoint(), socket.remote_endpoint())),
            (IpListenEndpoint::default(), None)
        );
        assert_eq!(receiver.route_state.read().device_mask, 0);
        assert!(receiver.poll().contains(IoEvents::READABLE));
        assert!(receiver.poll().contains(IoEvents::WRITABLE));

        let mut output = [0_u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut output[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"admitted-before-disconnect".len()
        );
        assert_eq!(
            &output[..b"admitted-before-disconnect".len()],
            b"admitted-before-disconnect"
        );

        receiver.connect(endpoint(31_131)).unwrap();
        assert_ne!(receiver.local_endpoint().unwrap().port, 0);
    }

    #[test]
    fn address_only_bind_disconnect_retains_address_and_reautobinds() {
        let stack = NetStack::new_loopback_only();
        let receiver = UdpSocket::new(stack.clone()).unwrap();
        let peer = UdpSocket::new(stack).unwrap();
        peer.bind(endpoint(31_141)).unwrap();
        receiver.bind(endpoint(0)).unwrap();
        receiver.connect(endpoint(31_141)).unwrap();

        let assigned_port = receiver.local_endpoint().unwrap().port;
        assert_ne!(assigned_port, 0);
        send_datagram(&peer, assigned_port, b"address-locked");

        receiver.disconnect();

        assert_eq!(
            receiver.local_addr().unwrap().into_ip().unwrap(),
            SocketAddr::new(LOOPBACK, 0)
        );
        assert_eq!(
            receiver.with_smol_socket(|socket| (socket.endpoint(), socket.remote_endpoint())),
            (IpListenEndpoint::default(), None)
        );
        assert!(receiver.poll().contains(IoEvents::READABLE));

        let mut output = [0_u8; 32];
        assert_eq!(
            receiver
                .recv(
                    &mut output[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            b"address-locked".len()
        );
        assert_eq!(&output[..b"address-locked".len()], b"address-locked");

        receiver.connect(endpoint(31_141)).unwrap();
        let rebound = receiver.local_endpoint().unwrap();
        assert_eq!(rebound.addr, LOOPBACK.into());
        assert_ne!(rebound.port, 0);
    }

    #[test]
    fn ipv6_family_owns_unbound_and_autobind_addresses() {
        let stack = NetStack::new_loopback_only();
        let socket = UdpSocket::new_with_family(stack, UdpSocketFamily::Ipv6).unwrap();

        assert_eq!(
            socket.local_addr().unwrap().into_ip().unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        );
        assert_eq!(
            socket.bind(endpoint(0)),
            Err(LinuxError::EAFNOSUPPORT.into())
        );

        socket.connect(endpoint_v6(31_151)).unwrap();
        let bound = socket.local_endpoint().unwrap();
        assert_eq!(bound.addr, IpAddress::Ipv6(Ipv6Address::UNSPECIFIED));
        assert_ne!(bound.port, 0);

        socket.disconnect();
        assert_eq!(
            socket.local_addr().unwrap().into_ip().unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        );
    }
}
