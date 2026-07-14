use alloc::{string::ToString, sync::Arc};
use core::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError, ax_bail, ax_err_type};
use axio::prelude::*;
use axpoll::{IoEvents, PollSet, Pollable};
use smoltcp::{
    iface::SocketHandle,
    socket::tcp as smol,
    time::Duration,
    wire::{IpEndpoint, IpListenEndpoint, IpVersion},
};

use crate::{
    RecvFlags, RecvOptions, SendFlags, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    buffer::try_zeroed_socket_buffer,
    consts::{LOOPBACK_TCP_MSS, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
    state::*,
    wrapper::Transport,
};

pub(crate) fn new_tcp_socket() -> AxResult<smol::Socket<'static>> {
    Ok(smol::Socket::new(
        smol::SocketBuffer::new(try_zeroed_socket_buffer(TCP_RX_BUF_LEN)?),
        smol::SocketBuffer::new(try_zeroed_socket_buffer(TCP_TX_BUF_LEN)?),
    ))
}

fn replace_tcp_send_buffer(socket: &mut smol::Socket, requested: usize) -> AxResult<()> {
    let buffer = smol::SocketBuffer::new(try_zeroed_socket_buffer(requested)?);
    socket
        .replace_send_buffer(buffer)
        .map_err(|_| AxError::ResourceBusy)
}

fn replace_tcp_recv_buffer(socket: &mut smol::Socket, requested: usize) -> AxResult<()> {
    let buffer = smol::SocketBuffer::new(try_zeroed_socket_buffer(requested)?);
    socket
        .replace_recv_buffer(buffer)
        .map_err(|_| AxError::ResourceBusy)
}

/// A TCP socket that provides POSIX-like APIs.
pub struct TcpSocket {
    stack: Arc<NetStack>,
    state: StateLock,
    handle: SocketHandle,

    general: GeneralOptions,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    poll_rx_closed: PollSet,
}

// SAFETY: All access to the underlying smoltcp socket goes through
// `SocketSetWrapper`'s mutex, so shared references to `TcpSocket` do not allow
// concurrent unsynchronized access to the socket state.
unsafe impl Sync for TcpSocket {}

impl TcpSocket {
    pub(crate) fn set_pending_error(&self, error: LinuxError) {
        self.general.set_pending_error(error);
    }

    /// Creates a new TCP socket bound to the given network stack.
    pub fn new(stack: Arc<NetStack>) -> AxResult<Self> {
        let handle = stack.socket_set.add(new_tcp_socket()?);
        Ok(Self {
            stack,
            state: StateLock::new(State::Idle),
            handle,

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: PollSet::new(),
        })
    }

    /// Creates a new TCP socket that is already connected.
    fn new_connected(stack: Arc<NetStack>, handle: SocketHandle) -> Self {
        let result = Self {
            stack,
            state: StateLock::new(State::Connected),
            handle,

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            poll_rx_closed: PollSet::new(),
        };
        let bound_endpoint = result.with_smol_socket(|socket| socket.get_bound_endpoint());
        let device_mask = result.stack.get_service().device_mask_for(&bound_endpoint);
        result.general.set_device_mask(device_mask);
        result
    }
}

/// Private methods
impl TcpSocket {
    fn state(&self) -> State {
        self.state.get()
    }

    #[inline]
    fn is_listening(&self) -> bool {
        self.state() == State::Listening
    }

    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        self.stack
            .socket_set
            .with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    fn with_service_and_smol_socket<R>(
        &self,
        f: impl FnOnce(&mut crate::service::Service, &mut smol::Socket) -> R,
    ) -> R {
        self.stack.with_service_and_socket_mut(self.handle, f)
    }

    fn bound_endpoint(&self) -> AxResult<IpListenEndpoint> {
        let endpoint = self.with_smol_socket(|socket| socket.get_bound_endpoint());
        if endpoint.port == 0 {
            ax_bail!(InvalidInput, "not bound");
        }
        Ok(endpoint)
    }

    fn uses_loopback_endpoint(&self) -> bool {
        fn is_loopback_addr(addr: smoltcp::wire::IpAddress) -> bool {
            match addr {
                smoltcp::wire::IpAddress::Ipv4(addr) => addr.is_loopback(),
                smoltcp::wire::IpAddress::Ipv6(addr) => addr.is_loopback(),
            }
        }

        self.with_smol_socket(|socket| {
            socket
                .remote_endpoint()
                .is_some_and(|endpoint| is_loopback_addr(endpoint.addr))
                || socket
                    .get_bound_endpoint()
                    .addr
                    .is_some_and(is_loopback_addr)
        })
    }

    fn poll_connect(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let writable = self.with_smol_socket(|socket| match socket.state() {
            smol::State::SynSent => false, // wait for connection
            smol::State::Established
            | smol::State::FinWait1
            | smol::State::FinWait2
            | smol::State::CloseWait
            | smol::State::Closing
            | smol::State::LastAck
            | smol::State::TimeWait => {
                // Linux connect() succeeds once the handshake completed, even
                // if the peer closes immediately afterwards. Preserve a
                // successful handshake across that post-connect close race.
                self.state.set(State::Connected); // connected
                self.general.clear_pending_error();
                if let Some(remote) = socket.remote_endpoint() {
                    debug!("TCP socket {}: connected to {}", self.handle, remote);
                }
                true
            }
            _ => {
                self.state.set(State::Closed); // connection failed
                self.general
                    .set_pending_error(axerrno::LinuxError::ECONNREFUSED);
                true
            }
        });
        events.set(IoEvents::OUT, writable);
        events
    }

    fn poll_stream(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            events.set(
                IoEvents::IN,
                !self.rx_closed.load(Ordering::Acquire)
                    && (!socket.may_recv() || socket.can_recv()),
            );
            events.set(IoEvents::OUT, !socket.may_send() || socket.can_send());
        });
        events
    }

    fn poll_listener(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let can_accept = self.bound_endpoint().ok().is_some_and(|endpoint| {
            self.stack
                .listen_table
                .can_accept(endpoint.port, &self.stack.socket_set)
                .unwrap_or(false)
        });
        events.set(IoEvents::IN, can_accept);
        events
    }

    fn wait_for_close_handshake(&self) {
        for _ in 0..16 {
            self.stack.poll_interfaces();
            let closed = self.with_smol_socket(|socket| {
                !socket.is_active() && !socket.may_recv() && !socket.may_send()
            });
            if closed {
                break;
            }
            axtask::yield_now();
        }
    }

    pub fn set_filter(
        &self,
        _filter: Option<alloc::sync::Arc<dyn crate::SocketFilter>>,
    ) -> AxResult<()> {
        Err(AxError::Unsupported)
    }

    pub fn set_ipv6_addrform_to_ipv4(&self) -> AxResult<()> {
        if self.state() != State::Connected {
            return Err(AxError::from(LinuxError::ENOTCONN));
        }

        let is_ipv4_connection = self.with_smol_socket(|socket| {
            socket
                .remote_endpoint()
                .is_some_and(|endpoint| matches!(endpoint.addr.version(), IpVersion::Ipv4))
        });
        if !is_ipv4_connection {
            return Err(AxError::from(LinuxError::EADDRNOTAVAIL));
        }

        Ok(())
    }

    pub fn disconnect(&self) -> AxResult<()> {
        if let Ok(guard) = self.state.lock(State::Listening) {
            return guard.transit(State::Idle, || {
                self.stack
                    .listen_table
                    .unlisten(self.bound_endpoint()?.port);
                self.with_smol_socket(|socket| {
                    socket.abort();
                    socket.set_bound_endpoint(IpListenEndpoint::default());
                });
                self.rx_closed.store(false, Ordering::Release);
                self.tx_closed.store(false, Ordering::Release);
                self.poll_rx_closed.wake();
                self.stack.poll_interfaces();
                Ok(())
            });
        }

        let guard = match self.state.lock(State::Connected) {
            Ok(guard) => guard,
            Err(State::Closed) => self
                .state
                .lock(State::Closed)
                .map_err(|_| ax_err_type!(InvalidInput, "busy"))?,
            Err(State::Idle) => return Ok(()),
            Err(State::Connecting) => ax_bail!(InvalidInput, "connect in progress"),
            Err(State::Connected) | Err(State::Listening) | Err(State::Busy) => {
                ax_bail!(InvalidInput, "busy")
            }
        };

        guard.transit(State::Idle, || {
            self.with_smol_socket(|socket| {
                socket.abort();
                socket.set_bound_endpoint(IpListenEndpoint::default());
            });
            self.rx_closed.store(false, Ordering::Release);
            self.tx_closed.store(false, Ordering::Release);
            self.poll_rx_closed.wake();
            self.stack.poll_interfaces();
            Ok(())
        })
    }
}

impl Configurable for TcpSocket {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, option: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(option)? {
            return Ok(true);
        }

        match option {
            O::NoDelay(no_delay) => {
                **no_delay = self.with_smol_socket(|socket| !socket.nagle_enabled());
            }
            O::KeepAlive(keep_alive) => {
                **keep_alive = self.with_smol_socket(|socket| socket.keep_alive().is_some());
            }
            O::MaxSegment(max_segment) => {
                **max_segment = if self.uses_loopback_endpoint() {
                    LOOPBACK_TCP_MSS
                } else {
                    1460
                };
            }
            O::SendBuffer(size) => {
                **size = self.with_smol_socket(|socket| socket.send_capacity());
            }
            O::ReceiveBuffer(size) => {
                **size = self.with_smol_socket(|socket| socket.recv_capacity());
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
            O::NoDelay(no_delay) => {
                self.with_smol_socket(|socket| {
                    socket.set_nagle_enabled(!no_delay);
                });
            }
            O::KeepAlive(keep_alive) => {
                self.with_smol_socket(|socket| {
                    socket.set_keep_alive(keep_alive.then(|| Duration::from_secs(75)));
                });
            }
            O::SendBuffer(size) | O::SendBufferForce(size) => {
                self.with_smol_socket(|socket| replace_tcp_send_buffer(socket, *size))?;
            }
            O::ReceiveBuffer(size) | O::ReceiveBufferForce(size) => {
                self.with_smol_socket(|socket| replace_tcp_recv_buffer(socket, *size))?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
impl SocketOps for TcpSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult {
        let mut local_addr = local_addr.into_ip()?;
        self.state
            .lock(State::Idle)
            .map_err(|_| ax_err_type!(InvalidInput, "already bound"))?
            .transit(State::Idle, || {
                if local_addr.port() == 0 {
                    local_addr.set_port(self.stack.tcp_ephemeral_port()?);
                }
                self.stack
                    .get_service()
                    .validate_bind_addr(local_addr.ip().into())?;
                if !self.general.reuse_address() {
                    self.stack.socket_set.bind_check(
                        Transport::Tcp,
                        (!local_addr.ip().is_unspecified()).then_some(local_addr.ip().into()),
                        local_addr.port(),
                    )?;
                }
                let endpoint = IpListenEndpoint {
                    addr: if local_addr.ip().is_unspecified() {
                        None
                    } else {
                        Some(local_addr.ip().into())
                    },
                    port: local_addr.port(),
                };
                let device_mask = self.stack.get_service().device_mask_for(&endpoint);

                self.with_smol_socket(|socket| {
                    if socket.get_bound_endpoint().port != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    socket.set_bound_endpoint(endpoint);
                    Ok(())
                })?;
                self.general.set_device_mask(device_mask);
                debug!("TCP socket {}: binding to {}", self.handle, local_addr);
                Ok(())
            })
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let remote_addr = match remote_addr.into_ip()? {
            SocketAddr::V4(addr) if addr.ip().is_unspecified() => {
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port())
            }
            SocketAddr::V6(addr) if addr.ip().is_unspecified() => {
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port())
            }
            addr => addr,
        };
        self.state
            .lock(State::Idle)
            .map_err(|state| {
                if state == State::Connecting {
                    AxError::InProgress
                } else {
                    // TODO(mivik): error code
                    ax_err_type!(AlreadyConnected)
                }
            })?
            .transit(State::Connecting, || {
                self.general.clear_pending_error();
                // TODO: check remote addr unreachable
                let remote_endpoint = IpEndpoint::from(remote_addr);
                let mut bound_endpoint =
                    self.with_smol_socket(|socket| socket.get_bound_endpoint());
                let outbound = self.stack.get_service().resolve_outbound_with_dont_route(
                    &remote_endpoint.addr,
                    bound_endpoint.addr,
                    self.general.dont_route(),
                )?;
                if bound_endpoint.addr.is_none() {
                    bound_endpoint.addr = Some(outbound.src_addr);
                }
                if bound_endpoint.port == 0 {
                    loop {
                        bound_endpoint.port = self.stack.tcp_ephemeral_port()?;
                        if bound_endpoint.port != remote_endpoint.port {
                            break;
                        }
                    }
                } else if bound_endpoint.port == remote_endpoint.port {
                    ax_bail!(ConnectionRefused, "same local/remote port");
                }
                info!(
                    "TCP connection from {} to {}",
                    bound_endpoint, remote_endpoint
                );

                self.with_service_and_smol_socket(|service, socket| {
                    socket.set_bound_endpoint(bound_endpoint);
                    socket
                        .connect(service.iface.context(), remote_endpoint, bound_endpoint)
                        .map_err(|e| match e {
                            smol::ConnectError::InvalidState => {
                                ax_err_type!(AlreadyConnected)
                            }
                            smol::ConnectError::Unaddressable => {
                                ax_err_type!(ConnectionRefused, "unaddressable")
                            }
                        })?;
                    Ok::<(), AxError>(())
                })?;
                self.general.set_device_mask(outbound.device_mask);
                Ok(())
            })?;

        // Yield once so a newly started listener can run before we poll the
        // connection state.
        axtask::yield_now();

        // Here our state must be `CONNECTING`, and only one thread can run here.
        self.general.connect_poller(self, || {
            self.stack.poll_interfaces();
            let events = self.poll_connect();
            if !events.contains(IoEvents::OUT) {
                Err(AxError::WouldBlock)
            } else if self.state() == State::Connected {
                Ok(())
            } else {
                Err(ax_err_type!(ConnectionRefused, "connection refused"))
            }
        })
    }

    fn listen(&self, backlog: usize) -> AxResult {
        let guard = match self.state.lock(State::Idle) {
            Ok(guard) => guard,
            Err(State::Listening) => {
                return self
                    .stack
                    .listen_table
                    .set_backlog(self.bound_endpoint()?.port, backlog);
            }
            Err(_) => return Err(AxError::InvalidInput),
        };

        guard.transit(State::Listening, || {
            let mut bound_endpoint = self.with_smol_socket(|socket| socket.get_bound_endpoint());
            if bound_endpoint.port == 0 {
                bound_endpoint.port = self.stack.tcp_ephemeral_port()?;
                self.with_smol_socket(|socket| socket.set_bound_endpoint(bound_endpoint));
                let device_mask = self.stack.get_service().device_mask_for(&bound_endpoint);
                self.general.set_device_mask(device_mask);
            }
            self.stack
                .listen_table
                .listen(bound_endpoint, backlog, &self.stack.socket_set)?;
            debug!("listening on {}", bound_endpoint);
            Ok(())
        })
    }

    fn accept(&self) -> AxResult<Socket> {
        if !self.is_listening() {
            ax_bail!(InvalidInput, "not listening");
        }

        let bound_port = self.bound_endpoint()?.port;
        self.general.recv_poller(self, || {
            self.stack.poll_interfaces();
            self.stack
                .listen_table
                .accept(bound_port, &self.stack.socket_set)
                .map(|handle| {
                    let socket = TcpSocket::new_connected(self.stack.clone(), handle);
                    debug!(
                        "accepted connection from {}, {}",
                        handle,
                        socket
                            .with_smol_socket(|socket| socket.remote_endpoint())
                            .map_or_else(|| "unknown".into(), |remote| remote.to_string())
                    );
                    Socket::Tcp(socket)
                })
        })
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> AxResult<usize> {
        if !options.cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        let per_call_nonblocking = options.flags.contains(SendFlags::DONT_WAIT);
        if self.tx_closed.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        self.general
            .send_poller_with_nonblocking(self, per_call_nonblocking, || {
                self.stack.poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.is_active() {
                        Err(AxError::NotConnected)
                    } else if !socket.can_send() {
                        Err(AxError::WouldBlock)
                    } else {
                        // connected, and the tx buffer is not full
                        let len = socket
                            .send(|buffer| {
                                let result = src.read(buffer);
                                let len = result.unwrap_or(0);
                                (len, result)
                            })
                            .map_err(|_| ax_err_type!(NotConnected, "not connected?"))??;
                        Ok(len)
                    }
                })
            })
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> AxResult<usize> {
        if self.rx_closed.load(Ordering::Acquire) {
            return Ok(0);
        }
        match self.state() {
            State::Idle | State::Connecting => return Err(AxError::NotConnected),
            State::Listening => ax_bail!(InvalidInput, "not connected"),
            State::Connected | State::Closed | State::Busy => {}
        }
        let per_call_nonblocking = options.flags.contains(RecvFlags::DONT_WAIT);
        self.general
            .recv_poller_with_nonblocking(self, per_call_nonblocking, || {
                self.stack.poll_interfaces();
                self.with_smol_socket(|socket| {
                    if !socket.may_recv() {
                        Ok(0)
                    } else if socket.recv_queue() == 0 {
                        Err(AxError::WouldBlock)
                    } else if options.flags.contains(RecvFlags::PEEK) {
                        dst.write(
                            socket
                                .peek(dst.remaining_mut())
                                .map_err(|_| ax_err_type!(NotConnected, "not connected?"))?,
                        )
                    } else {
                        socket
                            .recv(|buf| {
                                let result = dst.write(buf);
                                let len = result.unwrap_or(0);
                                (len, result)
                            })
                            .map_err(|_| ax_err_type!(NotConnected, "not connected?"))?
                    }
                })
            })
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        self.with_smol_socket(|socket| {
            let endpoint = socket.get_bound_endpoint();
            Ok(SocketAddrEx::Ip(SocketAddr::new(
                endpoint
                    .addr
                    .map_or_else(|| Ipv4Addr::UNSPECIFIED.into(), Into::into),
                endpoint.port,
            )))
        })
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        self.with_smol_socket(|socket| {
            Ok(SocketAddrEx::Ip(
                socket
                    .remote_endpoint()
                    .ok_or(AxError::NotConnected)?
                    .into(),
            ))
        })
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        if !matches!(self.state(), State::Connected | State::Closed) {
            return Err(AxError::NotConnected);
        }

        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
        }
        if how.has_write() && !self.tx_closed.swap(true, Ordering::AcqRel) {
            self.with_smol_socket(|socket| {
                debug!("TCP socket {}: shutting down write half", self.handle);
                socket.close();
            });
        }
        if self.rx_closed.load(Ordering::Acquire) && self.tx_closed.load(Ordering::Acquire) {
            self.state.set(State::Closed);
        }
        self.poll_rx_closed.wake();
        self.stack.poll_interfaces();
        Ok(())
    }
}

impl Pollable for TcpSocket {
    fn poll(&self) -> IoEvents {
        self.stack.poll_interfaces();
        let state = self.state();
        let mut events = match state {
            State::Connecting => self.poll_connect(),
            State::Connected | State::Idle | State::Closed => self.poll_stream(),
            State::Listening => self.poll_listener(),
            State::Busy => IoEvents::empty(),
        };
        let local_read_closed = self.rx_closed.load(Ordering::Acquire);
        events.set(
            IoEvents::IN,
            events.contains(IoEvents::IN) || local_read_closed,
        );
        let peer_write_closed = matches!(state, State::Connected | State::Closed)
            && self.with_smol_socket(|socket| !socket.may_recv());
        events.set(IoEvents::RDHUP, peer_write_closed);
        self.general.add_pending_error_event(events)
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.intersects(IoEvents::IN | IoEvents::OUT | IoEvents::RDHUP) {
            self.general.register_waker(&self.stack, context.waker());
        }
        if events.contains(IoEvents::RDHUP) {
            self.poll_rx_closed.register(context.waker());
        }
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        if self.is_listening() {
            if let Ok(endpoint) = self.bound_endpoint() {
                self.stack.listen_table.unlisten(endpoint.port);
            }
        } else {
            self.with_smol_socket(|socket| socket.close());
        }
        // Give loopback peers a short chance to observe a graceful close
        // before we tear the socket out of the set.
        self.wait_for_close_handshake();
        self.stack.socket_set.remove(self.handle);
        self.stack.poll_interfaces();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{SOCKET_BUFFER_MAX, SOCKET_BUFFER_MIN};

    #[test]
    fn replacement_changes_tcp_storage_capacity() {
        let mut socket = new_tcp_socket().unwrap();

        replace_tcp_send_buffer(&mut socket, 64 * 1024).unwrap();
        replace_tcp_recv_buffer(&mut socket, 0).unwrap();
        assert_eq!(socket.send_capacity(), 64 * 1024);
        assert_eq!(socket.recv_capacity(), SOCKET_BUFFER_MIN);

        replace_tcp_recv_buffer(&mut socket, usize::MAX).unwrap();
        assert_eq!(socket.recv_capacity(), SOCKET_BUFFER_MAX);
    }
}
