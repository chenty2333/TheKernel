use alloc::sync::Arc;
use core::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError, ax_bail, ax_err_type};
use axio::prelude::*;
use axpoll::{IoEvents, PollSet, Pollable};
use smoltcp::{
    iface::SocketHandle,
    phy::PacketMeta,
    socket::udp::{self as smol, UdpMetadata},
    storage::PacketMetadata,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint},
};
use spin::RwLock;

use crate::{
    RecvFlags, RecvOptions, SendFlags, SendOptions, Shutdown, SocketAddrEx, SocketOps,
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
    local_addr: RwLock<Option<IpEndpoint>>,
    reported_local_addr: RwLock<Option<IpEndpoint>>,
    peer_addr: RwLock<Option<(IpEndpoint, IpAddress)>>,
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

    /// Creates a new UDP socket bound to the given network stack.
    pub fn new(stack: Arc<NetStack>) -> AxResult<Self> {
        let socket = new_udp_socket()?;
        let handle = stack.socket_set.add(socket);

        Ok(Self {
            stack,
            handle,
            local_addr: RwLock::new(None),
            reported_local_addr: RwLock::new(None),
            peer_addr: RwLock::new(None),
            send_bytes_since_yield: AtomicUsize::new(0),
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
            poll_state: PollSet::new(),

            general: GeneralOptions::new(),
        })
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        self.stack
            .socket_set
            .with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    fn remote_endpoint(&self) -> AxResult<(IpEndpoint, IpAddress)> {
        match self.peer_addr.try_read() {
            Some(addr) => addr.ok_or(AxError::NotConnected),
            None => Err(AxError::NotConnected),
        }
    }

    fn bound_source_addr(&self) -> Option<IpAddress> {
        self.local_addr
            .read()
            .as_ref()
            .and_then(|endpoint| (!endpoint.addr.is_unspecified()).then_some(endpoint.addr))
    }

    fn effective_local_addr(&self) -> Option<IpEndpoint> {
        self.reported_local_addr
            .read()
            .as_ref()
            .copied()
            .or_else(|| self.local_addr.read().as_ref().copied())
    }

    fn remember_reported_local_addr(&self, addr: IpAddress) {
        let Some(bound) = self.local_addr.read().as_ref().copied() else {
            return;
        };
        if !bound.addr.is_unspecified() {
            return;
        }
        let mut reported = self.reported_local_addr.write();
        *reported = Some(IpEndpoint {
            addr,
            port: bound.port,
        });
    }

    fn note_udp_send_progress(&self, sent: usize) {
        let mut should_yield = false;
        let _ = self.send_bytes_since_yield.fetch_update(
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
        *self.peer_addr.write() = None;
        *self.reported_local_addr.write() = None;
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
        let mut guard = self.local_addr.write();

        if local_addr.port() == 0 {
            local_addr.set_port(self.stack.udp_ephemeral_port()?);
        }
        if guard.is_some() {
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
        self.general
            .set_device_mask(self.stack.get_service().device_mask_for(&endpoint));

        *guard = Some(local_endpoint);
        *self.reported_local_addr.write() = None;
        info!("UDP socket {}: bound on {}", self.handle, endpoint);
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let remote_addr = remote_addr.into_ip()?;
        let mut guard = self.peer_addr.write();
        if self.local_addr.read().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }

        let remote_addr = IpEndpoint::from(remote_addr);
        let outbound = self.stack.get_service().resolve_outbound_with_dont_route(
            &remote_addr.addr,
            self.bound_source_addr(),
            self.general.dont_route(),
        )?;
        self.general.set_device_mask(outbound.device_mask);
        // Linux reports the selected source address after a connected UDP
        // socket resolves its route. Keep the wildcard bind semantics in the
        // actual socket state, but surface the concrete source address through
        // getsockname/local_addr so user space can advertise a usable reply
        // endpoint after an implicit bind.
        self.remember_reported_local_addr(outbound.src_addr);
        *guard = Some((remote_addr, outbound.src_addr));
        debug!("UDP socket {}: connected to {}", self.handle, remote_addr);
        Ok(())
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if !options.cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        let per_call_nonblocking = options.flags.contains(SendFlags::DONT_WAIT);
        if self.tx_shutdown.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        let (remote_addr, source_addr) = match options.to {
            Some(addr) => {
                let addr = IpEndpoint::from(addr.into_ip()?);
                let outbound = self.stack.get_service().resolve_outbound_with_dont_route(
                    &addr.addr,
                    self.bound_source_addr(),
                    self.general.dont_route(),
                )?;
                self.general.set_device_mask(outbound.device_mask);
                (addr, outbound.src_addr)
            }
            None => self.remote_endpoint()?,
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

        if self.local_addr.read().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }
        self.remember_reported_local_addr(source_addr);
        let sent = self
            .general
            .send_poller_with_nonblocking(self, per_call_nonblocking, || {
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
            })?;

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
        if self.local_addr.read().is_none() {
            ax_bail!(NotConnected);
        }

        enum ExpectedRemote<'a> {
            Any(&'a mut SocketAddrEx),
            Expecting(IpEndpoint),
        }
        let mut expected_remote = match options.from {
            Some(addr) => ExpectedRemote::Any(addr),
            None => ExpectedRemote::Expecting(self.remote_endpoint()?.0),
        };

        let per_call_nonblocking = options.flags.contains(RecvFlags::DONT_WAIT);
        self.general
            .recv_poller_with_nonblocking(self, per_call_nonblocking, || {
                self.stack.poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.is_open() {
                        // not bound
                        Err(ax_err_type!(NotConnected))
                    } else if !socket.can_recv() {
                        Err(AxError::WouldBlock)
                    } else {
                        let result = if options.flags.contains(RecvFlags::PEEK) {
                            socket.peek().map(|(data, meta)| (data, *meta))
                        } else {
                            socket.recv()
                        };
                        match result {
                            Ok((src, meta)) => {
                                match &mut expected_remote {
                                    ExpectedRemote::Any(remote_addr) => {
                                        **remote_addr = SocketAddrEx::Ip(meta.endpoint.into());
                                    }
                                    ExpectedRemote::Expecting(expected) => {
                                        if (!expected.addr.is_unspecified()
                                            && expected.addr != meta.endpoint.addr)
                                            || (expected.port != 0
                                                && expected.port != meta.endpoint.port)
                                        {
                                            return Err(AxError::WouldBlock);
                                        }
                                    }
                                }

                                let read = dst.write(src)?;
                                if read < src.len() {
                                    warn!("UDP message truncated: {} -> {} bytes", src.len(), read);
                                }

                                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                                    src.len()
                                } else {
                                    read
                                })
                            }
                            Err(smol::RecvError::Exhausted) => Err(AxError::WouldBlock),
                            Err(smol::RecvError::Truncated) => Err(LinuxError::EMSGSIZE.into()),
                        }
                    }
                })
            })
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        match self.effective_local_addr() {
            Some(addr) => Ok(SocketAddrEx::Ip(addr.into())),
            None => Err(AxError::NotConnected),
        }
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        self.remote_endpoint()
            .map(|it| it.0.into())
            .map(SocketAddrEx::Ip)
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        if self.peer_addr.read().is_none() {
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
        if self.local_addr.read().is_none() {
            return self.general.add_pending_error_event(IoEvents::empty());
        }

        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            let rx_shutdown = self.rx_shutdown.load(Ordering::Acquire);
            events.set(IoEvents::IN, rx_shutdown || socket.can_recv());
            events.set(
                IoEvents::OUT,
                !self.tx_shutdown.load(Ordering::Acquire) && socket.can_send(),
            );
            events.set(IoEvents::RDHUP, rx_shutdown);
        });
        self.general.add_pending_error_event(events)
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.intersects(IoEvents::IN | IoEvents::OUT) {
            self.general.register_waker(&self.stack, context.waker());
        }
        self.poll_state.register(context.waker());
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
    use super::*;
    use crate::consts::{SOCKET_BUFFER_MAX, SOCKET_BUFFER_MIN};

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
}
