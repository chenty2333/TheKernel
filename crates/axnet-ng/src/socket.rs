use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::{self, Debug},
    net::SocketAddr,
    task::Context,
};

#[cfg(feature = "vsock")]
use axdriver::prelude::VsockAddr;
use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use bitflags::bitflags;
use enum_dispatch::enum_dispatch;

#[cfg(feature = "vsock")]
use crate::vsock::VsockSocket;
use crate::{
    options::{Configurable, GetSocketOption, SetSocketOption},
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{UnixSocket, UnixSocketAddr},
};

pub trait SocketFilter: Send + Sync {
    fn filter(&self, data: &mut [u8]) -> AxResult<usize>;
}

/// Extended socket address supporting IP, Unix, and vsock address families.
#[derive(Clone, Debug)]
pub enum SocketAddrEx {
    /// An IP (v4/v6) socket address.
    Ip(SocketAddr),
    /// A Unix domain socket address.
    Unix(UnixSocketAddr),
    /// A vsock socket address.
    #[cfg(feature = "vsock")]
    Vsock(VsockAddr),
}

impl SocketAddrEx {
    /// Convert into an IP socket address, or return an error if not IP.
    pub fn into_ip(self) -> AxResult<SocketAddr> {
        match self {
            SocketAddrEx::Ip(addr) => Ok(addr),
            SocketAddrEx::Unix(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    /// Convert into a Unix socket address, or return an error if not Unix.
    pub fn into_unix(self) -> AxResult<UnixSocketAddr> {
        match self {
            SocketAddrEx::Unix(addr) => Ok(addr),
            SocketAddrEx::Ip(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    /// Convert into a vsock address, or return an error if not vsock.
    #[cfg(feature = "vsock")]
    pub fn into_vsock(self) -> AxResult<VsockAddr> {
        match self {
            SocketAddrEx::Ip(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Unix(_) => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Vsock(addr) => Ok(addr),
        }
    }
}

bitflags! {
    /// Flags for sending data to a socket.
    ///
    /// See [`SocketOps::send`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SendFlags: u32 {
        /// Do not wait for transmit capacity for this operation.
        const DONT_WAIT = 0x01;
    }
}

bitflags! {
    /// Flags for receiving data from a socket.
    ///
    /// See [`SocketOps::recv`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct RecvFlags: u32 {
        /// Receive data without removing it from the queue.
        const PEEK = 0x01;
        /// For datagram-like sockets, requires [`SocketOps::recv`] to return
        /// the real size of the datagram, even when it is larger than the
        /// buffer.
        const TRUNCATE = 0x02;
        /// Do not wait for receive data for this operation.
        const DONT_WAIT = 0x04;
    }
}

/// Type alias for ancillary control message data.
/// Opaque ancillary data plus the amount of socket-buffer capacity retained
/// by it.  The transport must account the charge for as long as the message is
/// queued; otherwise zero-length datagrams carrying resource references can
/// bypass ordinary payload-byte limits.
pub struct CMsgData {
    value: Box<dyn Any + Send + Sync>,
    charge: usize,
}

impl CMsgData {
    /// Wrap already allocated, type-erased ancillary data.
    pub fn new<T: Any + Send + Sync>(value: Box<T>, charge: usize) -> Self {
        Self { value, charge }
    }

    /// Returns the socket-buffer charge associated with this message.
    pub const fn charge(&self) -> usize {
        self.charge
    }

    /// Attempts to recover the concrete ancillary value.
    pub fn downcast<T: Any + Send + Sync>(self) -> Result<Box<T>, Self> {
        let Self { value, charge } = self;
        match value.downcast::<T>() {
            Ok(value) => Ok(value),
            Err(value) => Err(Self { value, charge }),
        }
    }
}

impl Debug for CMsgData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CMsgData")
            .field("charge", &self.charge)
            .finish_non_exhaustive()
    }
}

/// Options for sending data to a socket.
///
/// See [`SocketOps::send`].
#[derive(Default, Debug)]
pub struct SendOptions {
    /// Destination address for the message.
    pub to: Option<SocketAddrEx>,
    /// Send flags.
    pub flags: SendFlags,
    /// Ancillary control messages.
    pub cmsg: Vec<CMsgData>,
}

/// Options for receiving data from a socket.
///
/// See [`SocketOps::recv`].
#[derive(Default)]
pub struct RecvOptions<'a> {
    /// If set, the sender's address is written here.
    pub from: Option<&'a mut SocketAddrEx>,
    /// Receive flags.
    pub flags: RecvFlags,
    /// If set, ancillary control messages are appended here.
    pub cmsg: Option<&'a mut Vec<CMsgData>>,
}
impl Debug for RecvOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvOptions")
            .field("from", &self.from)
            .field("flags", &self.flags)
            .finish()
    }
}

/// Kind of shutdown operation to perform on a socket.
#[derive(Debug, Clone, Copy)]
pub enum Shutdown {
    /// Shut down the read half.
    Read,
    /// Shut down the write half.
    Write,
    /// Shut down both halves.
    Both,
}
impl Shutdown {
    /// Returns `true` if the read half should be shut down.
    pub fn has_read(&self) -> bool {
        matches!(self, Shutdown::Read | Shutdown::Both)
    }

    /// Returns `true` if the write half should be shut down.
    pub fn has_write(&self) -> bool {
        matches!(self, Shutdown::Write | Shutdown::Both)
    }
}

/// Operations that can be performed on a socket.
#[enum_dispatch]
pub trait SocketOps: Configurable {
    /// Binds an unbound socket to the given address and port.
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult;
    /// Connects the socket to a remote address.
    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult;

    /// Starts listening on the bound address and port.
    fn listen(&self, _backlog: usize) -> AxResult {
        Err(AxError::OperationNotSupported)
    }
    /// Accepts a connection on a listening socket, returning a new socket.
    fn accept(&self) -> AxResult<Socket> {
        Err(AxError::OperationNotSupported)
    }

    /// Send data to the socket, optionally to a specific address.
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize>;
    /// Receive data from the socket.
    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> AxResult<usize>;

    /// Get the local endpoint of the socket.
    fn local_addr(&self) -> AxResult<SocketAddrEx>;
    /// Get the remote endpoint of the socket.
    fn peer_addr(&self) -> AxResult<SocketAddrEx>;

    /// Shutdown the socket, closing the connection.
    fn shutdown(&self, how: Shutdown) -> AxResult;
}

/// Network socket abstraction.
#[enum_dispatch(Configurable, SocketOps)]
pub enum Socket {
    /// UDP socket.
    Udp(UdpSocket),
    /// TCP socket.
    Tcp(TcpSocket),
    /// Unix domain socket.
    Unix(UnixSocket),
    /// Virtio socket.
    #[cfg(feature = "vsock")]
    Vsock(VsockSocket),
}

impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        match self {
            Socket::Tcp(tcp) => tcp.poll(),
            Socket::Udp(udp) => udp.poll(),
            Socket::Unix(unix) => unix.poll(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.poll(),
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        match self {
            Socket::Tcp(tcp) => tcp.register(context, events),
            Socket::Udp(udp) => udp.register(context, events),
            Socket::Unix(unix) => unix.register(context, events),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.register(context, events),
        }
    }
}

impl Socket {
    /// Stores an error for one-shot SO_ERROR reporting and the next operation
    /// that consumes deferred socket errors.
    pub fn set_pending_error(&self, error: LinuxError) {
        match self {
            Socket::Tcp(tcp) => tcp.set_pending_error(error),
            Socket::Udp(udp) => udp.set_pending_error(error),
            Socket::Unix(unix) => unix.set_pending_error(error),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.set_pending_error(error),
        }
    }

    pub fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        match self {
            Socket::Unix(unix) => unix.set_filter(filter),
            Socket::Udp(udp) => udp.set_filter(filter),
            Socket::Tcp(tcp) => tcp.set_filter(filter),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.set_filter(filter),
        }
    }

    pub fn disconnect(&self) -> AxResult<()> {
        match self {
            Socket::Tcp(tcp) => tcp.disconnect(),
            Socket::Udp(udp) => {
                udp.disconnect();
                Ok(())
            }
            Socket::Unix(unix) => unix.disconnect(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(_) => Err(AxError::OperationNotSupported),
        }
    }

    pub fn set_ipv6_addrform_to_ipv4(&self) -> AxResult<()> {
        match self {
            Socket::Tcp(tcp) => tcp.set_ipv6_addrform_to_ipv4(),
            Socket::Udp(_) | Socket::Unix(_) => Err(AxError::from(LinuxError::ENOPROTOOPT)),
            #[cfg(feature = "vsock")]
            Socket::Vsock(_) => Err(AxError::from(LinuxError::ENOPROTOOPT)),
        }
    }
}
