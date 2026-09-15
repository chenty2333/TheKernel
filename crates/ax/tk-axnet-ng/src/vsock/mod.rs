// pub(crate) mod dgram; todo

pub(crate) mod connection_manager;
pub(crate) mod stream;

use alloc::sync::Arc;
use core::task::Context;

pub use axdriver::prelude::{VsockAddr, VsockConnId};
use axerrno::{AxError, AxResult};
use axio::{IoBuf, IoBufMut, Read, Write};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use enum_dispatch::enum_dispatch;

pub use self::stream::VsockStreamTransport;
use crate::{
    RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketFault},
};

/// Abstract transport trait for vsock.
#[enum_dispatch]
pub trait VsockTransportOps: Configurable + Pollable + Send + Sync {
    /// Stores a deferred socket error.
    fn set_pending_error(&self, error: SocketFault);
    /// Bind the transport to a local address.
    fn bind(&self, local_addr: VsockAddr) -> AxResult;
    /// Start listening for incoming connections.
    fn listen(&self, backlog: usize) -> AxResult;
    /// Connect to a remote peer address.
    fn connect(&self, peer_addr: VsockAddr) -> AxResult;
    /// Accept an incoming connection.
    fn accept(&self) -> AxResult<(VsockTransport, VsockAddr)>;
    /// Send data through the transport.
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize>;
    /// Receive data from the transport.
    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> AxResult<usize>;
    /// Shutdown the transport.
    fn shutdown(&self, _how: Shutdown) -> AxResult;
    /// Get the local address, if bound.
    fn local_addr(&self) -> AxResult<Option<VsockAddr>>;
    /// Get the peer address, if connected.
    fn peer_addr(&self) -> AxResult<Option<VsockAddr>>;
}

/// Vsock transport type.
#[enum_dispatch(Configurable, VsockTransportOps)]
pub enum VsockTransport {
    /// Stream-oriented vsock transport.
    Stream(VsockStreamTransport),
    // Dgram(VsockDgramVsockTransport),
}

impl Pollable for VsockTransport {
    fn poll(&self) -> IoEvents {
        match self {
            VsockTransport::Stream(stream) => stream.poll(),
            // VsockTransport::Dgram(dgram) => dgram.poll(),
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut core::task::Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        match self {
            VsockTransport::Stream(stream) => stream.register(context, events),
            // VsockTransport::Dgram(dgram) => dgram.register(context, events),
        }
    }
}

impl VsockTransport {
    fn recv_pending_len(&self) -> AxResult<usize> {
        match self {
            Self::Stream(stream) => stream.recv_pending_len(),
        }
    }

    fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        match self {
            Self::Stream(stream) => {
                stream.retry_transfer(direction, effective_nonblocking, attempt)
            }
        }
    }
}

/// A network socket using the vsock protocol.
pub struct VsockSocket {
    transport: VsockTransport,
}

/// Prepared accept on a vsock socket.
pub struct VsockSocketAcceptReservation {
    inner: stream::VsockAcceptReservation,
}

impl VsockSocketAcceptReservation {
    pub fn connection_identity(&self) -> VsockConnId {
        self.inner.connection_identity()
    }

    pub fn commit(self) -> AxResult<Socket> {
        self.inner.commit().map(|(transport, _)| {
            let socket = VsockSocket::new(transport);
            Socket::Vsock(socket)
        })
    }
}

impl VsockSocket {
    /// Create a new vsock socket with the given transport.
    pub fn new(transport: impl Into<VsockTransport>) -> Self {
        Self {
            transport: transport.into(),
        }
    }

    pub fn set_filter(&self, _filter: Option<Arc<dyn crate::SocketFilter>>) -> AxResult<()> {
        Err(AxError::Unsupported)
    }

    pub(crate) fn recv_pending_len(&self) -> AxResult<usize> {
        self.transport.recv_pending_len()
    }

    pub fn prepare_accept(&self) -> AxResult<VsockSocketAcceptReservation> {
        let inner = match &self.transport {
            VsockTransport::Stream(stream) => stream.prepare_accept()?,
        };
        Ok(VsockSocketAcceptReservation { inner })
    }

    pub(crate) fn set_pending_error(&self, error: SocketFault) {
        self.transport.set_pending_error(error);
    }

    pub(crate) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.transport
            .retry_transfer(direction, effective_nonblocking, attempt)
    }
}

impl Configurable for VsockSocket {
    fn nonblocking(&self) -> bool {
        self.transport.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool> {
        self.transport.set_option_inner(opt)
    }
}

impl SocketOps for VsockSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult {
        let local_addr = local_addr.into_vsock().map_err(|_| AxError::InvalidInput)?;
        self.transport.bind(local_addr)
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let remote_addr = remote_addr
            .into_vsock()
            .map_err(|_| AxError::InvalidInput)?;
        self.transport.connect(remote_addr)
    }

    fn listen(&self, backlog: usize) -> AxResult {
        self.transport.listen(backlog)
    }

    fn accept(&self) -> AxResult<Socket> {
        self.transport.accept().map(|(transport, _addr)| {
            let socket = VsockSocket::new(transport);
            Socket::Vsock(socket)
        })
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> AxResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Vsock(
            self.transport.local_addr()?.ok_or(AxError::NotFound)?,
        ))
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Vsock(
            self.transport.peer_addr()?.ok_or(AxError::NotFound)?,
        ))
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        self.transport.shutdown(how)
    }
}

impl Pollable for VsockSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.transport.register(context, events)
    }
}
