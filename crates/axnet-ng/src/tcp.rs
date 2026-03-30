use alloc::{sync::Arc, vec};
use core::{
    net::{Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, ax_bail, ax_err_type};
use axio::prelude::*;
use axpoll::{IoEvents, PollSet, Pollable};
use smoltcp::{
    iface::SocketHandle,
    socket::tcp as smol,
    time::Duration,
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{
    RecvFlags, RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    consts::{TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
    state::*,
};

pub(crate) fn new_tcp_socket() -> smol::Socket<'static> {
    smol::Socket::new(
        smol::SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
        smol::SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
    )
}

/// A TCP socket that provides POSIX-like APIs.
pub struct TcpSocket {
    stack: Arc<NetStack>,
    state: StateLock,
    handle: SocketHandle,

    general: GeneralOptions,
    rx_closed: AtomicBool,
    poll_rx_closed: PollSet,
}

// SAFETY: All access to the underlying smoltcp socket goes through
// `SocketSetWrapper`'s mutex, so shared references to `TcpSocket` do not allow
// concurrent unsynchronized access to the socket state.
unsafe impl Sync for TcpSocket {}

impl TcpSocket {
    /// Creates a new TCP socket bound to the given network stack.
    pub fn new(stack: Arc<NetStack>) -> Self {
        let handle = stack.socket_set.add(new_tcp_socket());
        Self {
            stack,
            state: StateLock::new(State::Idle),
            handle,

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
            poll_rx_closed: PollSet::new(),
        }
    }

    /// Creates a new TCP socket that is already connected.
    fn new_connected(stack: Arc<NetStack>, handle: SocketHandle) -> Self {
        let result = Self {
            stack,
            state: StateLock::new(State::Connected),
            handle,

            general: GeneralOptions::new(),
            rx_closed: AtomicBool::new(false),
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

    fn poll_connect(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let writable = self.with_smol_socket(|socket| match socket.state() {
            smol::State::SynSent => false, // wait for connection
            smol::State::Established => {
                self.state.set(State::Connected); // connected
                debug!(
                    "TCP socket {}: connected to {}",
                    self.handle,
                    socket.remote_endpoint().unwrap(),
                );
                true
            }
            _ => {
                self.state.set(State::Closed); // connection failed
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
        events.set(
            IoEvents::IN,
            self.stack
                .listen_table
                .can_accept(self.bound_endpoint().unwrap().port, &self.stack.socket_set)
                .unwrap(),
        );
        events
    }
}

impl Configurable for TcpSocket {
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
                // TODO(mivik): get actual MSS
                **max_segment = 1460;
            }
            O::SendBuffer(size) => {
                **size = TCP_TX_BUF_LEN;
            }
            O::ReceiveBuffer(size) => {
                **size = TCP_RX_BUF_LEN;
            }
            O::TcpInfo(_) => {
                // TODO(mivik): implement TCP_INFO
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
                // TODO: check addr is available
                if local_addr.port() == 0 {
                    local_addr.set_port(self.stack.tcp_ephemeral_port()?);
                }
                if !self.general.reuse_address() {
                    self.stack
                        .socket_set
                        .bind_check(local_addr.ip().into(), local_addr.port())?;
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
        let remote_addr = remote_addr.into_ip()?;
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
                // TODO: check remote addr unreachable
                let remote_endpoint = IpEndpoint::from(remote_addr);
                let mut bound_endpoint =
                    self.with_smol_socket(|socket| socket.get_bound_endpoint());
                let outbound = self
                    .stack
                    .get_service()
                    .resolve_outbound(&remote_endpoint.addr, bound_endpoint.addr)?;
                if bound_endpoint.addr.is_none() {
                    bound_endpoint.addr = Some(outbound.src_addr);
                }
                if bound_endpoint.port == 0 {
                    bound_endpoint.port = self.stack.tcp_ephemeral_port()?;
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
        self.general.send_poller(self, || {
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

    fn listen(&self) -> AxResult {
        if let Ok(guard) = self.state.lock(State::Idle) {
            guard.transit(State::Listening, || {
                let bound_endpoint = self.with_smol_socket(|socket| socket.get_bound_endpoint());
                self.stack
                    .listen_table
                    .listen(bound_endpoint, &self.stack.socket_set)?;
                debug!("listening on {}", bound_endpoint);
                Ok(())
            })?;
        } else {
            // ignore simultaneous `listen`s.
        }
        Ok(())
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
                        socket.with_smol_socket(|socket| socket.remote_endpoint().unwrap())
                    );
                    Socket::Tcp(socket)
                })
        })
    }

    fn send(&self, mut src: impl Read, _options: SendOptions) -> AxResult<usize> {
        self.general.send_poller(self, || {
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
            return Err(AxError::NotConnected);
        }
        self.general.recv_poller(self, || {
            self.stack.poll_interfaces();
            self.with_smol_socket(|socket| {
                if !socket.is_active() {
                    Err(AxError::NotConnected)
                } else if !socket.may_recv() {
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
        // TODO(mivik): shutdown
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
            self.poll_rx_closed.wake();
        }

        // stream
        if let Ok(guard) = self.state.lock(State::Connected) {
            guard.transit(State::Closed, || {
                if how.has_write() {
                    self.with_smol_socket(|socket| {
                        debug!("TCP socket {}: shutting down", self.handle);
                        socket.close();
                    });
                }
                self.stack.poll_interfaces();
                Ok(())
            })?;
        }

        // listener
        if let Ok(guard) = self.state.lock(State::Listening) {
            guard.transit(State::Closed, || {
                self.stack
                    .listen_table
                    .unlisten(self.bound_endpoint()?.port);
                self.stack.poll_interfaces();
                Ok(())
            })?;
        }

        // ignore for other states
        Ok(())
    }
}

impl Pollable for TcpSocket {
    fn poll(&self) -> IoEvents {
        self.stack.poll_interfaces();
        let mut events = match self.state() {
            State::Connecting => self.poll_connect(),
            State::Connected | State::Idle | State::Closed => self.poll_stream(),
            State::Listening => self.poll_listener(),
            State::Busy => IoEvents::empty(),
        };
        events.set(IoEvents::RDHUP, self.rx_closed.load(Ordering::Acquire));
        events
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
        if let Err(err) = self.shutdown(Shutdown::Both) {
            warn!("TCP socket {}: shutdown failed: {}", self.handle, err);
        }
        self.stack.socket_set.remove(self.handle);
        // This is crucial for the close messages to be sent.
        self.stack.poll_interfaces();
    }
}
