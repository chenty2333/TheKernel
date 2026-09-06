use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::{self, Debug},
    net::SocketAddr,
    task::Context,
};

#[cfg(feature = "vsock")]
use axdriver::prelude::VsockAddr;
use axerrno::{AxError, AxResult};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use bitflags::bitflags;
use enum_dispatch::enum_dispatch;

#[cfg(feature = "vsock")]
use crate::vsock::{VsockSocket, VsockSocketAcceptReservation};
use crate::{
    dccp::DccpSocket,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketFault},
    raw::RawSocket,
    sctp::{SctpAcceptReservation, SctpSocket},
    tcp::{TcpAcceptReservation, TcpSocket},
    udp::UdpSocket,
    unix::{UnixAcceptReservation, UnixSocket, UnixSocketAddr},
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

/// A requested address does not belong to the socket's address family.
///
/// This is deliberately transport-neutral.  An operating-system ABI decides
/// which observable error a family mismatch represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressFamilyMismatch;

/// Why an IPv6 socket cannot be converted to an IPv4 socket.
///
/// These are socket-state facts, not operating-system errno values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ipv6AddrFormError {
    UnsupportedSocket,
    NotConnected,
    PeerIsNotIpv4,
}

impl SocketAddrEx {
    /// Convert into an IP socket address, or return an error if not IP.
    pub fn into_ip(self) -> Result<SocketAddr, AddressFamilyMismatch> {
        match self {
            SocketAddrEx::Ip(addr) => Ok(addr),
            SocketAddrEx::Unix(_) => Err(AddressFamilyMismatch),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(AddressFamilyMismatch),
        }
    }

    /// Convert into a Unix socket address, or return an error if not Unix.
    pub fn into_unix(self) -> Result<UnixSocketAddr, AddressFamilyMismatch> {
        match self {
            SocketAddrEx::Unix(addr) => Ok(addr),
            SocketAddrEx::Ip(_) => Err(AddressFamilyMismatch),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(AddressFamilyMismatch),
        }
    }

    /// Convert into a vsock address, or return an error if not vsock.
    #[cfg(feature = "vsock")]
    pub fn into_vsock(self) -> Result<VsockAddr, AddressFamilyMismatch> {
        match self {
            SocketAddrEx::Ip(_) => Err(AddressFamilyMismatch),
            SocketAddrEx::Unix(_) => Err(AddressFamilyMismatch),
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
    peek_clone: Option<Arc<dyn Fn(&dyn Any) -> AxResult<Box<dyn Any + Send + Sync>> + Send + Sync>>,
}

impl CMsgData {
    /// Wrap already allocated, type-erased ancillary data.
    pub fn new<T: Any + Send + Sync>(value: Box<T>, charge: usize) -> Self {
        Self {
            value,
            charge,
            peek_clone: None,
        }
    }

    /// Wraps ancillary state that can be duplicated for a non-consuming
    /// `MSG_PEEK`.  The callback is fallible because cloning a queued resource
    /// may need its own admission (for example SCM_RIGHTS inflight custody).
    pub fn new_peekable<T: Any + Send + Sync>(
        value: Box<T>,
        charge: usize,
        clone: fn(&T) -> AxResult<T>,
    ) -> AxResult<Self> {
        let callback = Arc::try_new(move |value: &dyn Any| {
            let value = value.downcast_ref::<T>().ok_or(AxError::BadState)?;
            Ok(Box::new(clone(value)?) as Box<dyn Any + Send + Sync>)
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            value,
            charge,
            peek_clone: Some(callback),
        })
    }

    /// Returns the socket-buffer charge associated with this message.
    pub const fn charge(&self) -> usize {
        self.charge
    }

    /// Duplicates ancillary state without consuming the queued record.
    pub fn clone_for_peek(&self) -> AxResult<Self> {
        let clone = self
            .peek_clone
            .as_ref()
            .ok_or(AxError::OperationNotSupported)?;
        Ok(Self {
            value: clone(self.value.as_ref())?,
            charge: self.charge,
            peek_clone: Some(clone.clone()),
        })
    }

    /// Attempts to recover the concrete ancillary value.
    pub fn downcast<T: Any + Send + Sync>(self) -> Result<Box<T>, Self> {
        let Self {
            value,
            charge,
            peek_clone,
        } = self;
        match value.downcast::<T>() {
            Ok(value) => Ok(value),
            Err(value) => Err(Self {
                value,
                charge,
                peek_clone,
            }),
        }
    }

    /// Mutable type-erased access for kernel-owned ancillary lifecycle
    /// bookkeeping. Transports never interpret the concrete payload.
    pub fn downcast_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.value.downcast_mut::<T>()
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
    /// Sender credentials captured at the syscall boundary for automatic
    /// Unix `SO_PASSCRED` delivery.  Internal protocol traffic has none.
    pub credentials: Option<crate::options::SocketCredentials>,
    /// Exact per-operation nonblocking state captured by a caller.
    ///
    /// `None` samples the socket's ordinary mutable state at operation entry.
    /// `Some(false)` deliberately overrides a concurrently changed backend
    /// mirror; [`SendFlags::DONT_WAIT`] still forces nonblocking behavior.
    pub nonblocking_override: Option<bool>,
}

impl SendOptions {
    pub(crate) fn effective_nonblocking(&self, socket_nonblocking: bool) -> bool {
        self.flags.contains(SendFlags::DONT_WAIT)
            || self.nonblocking_override.unwrap_or(socket_nonblocking)
    }
}

/// Options for receiving data from a socket.
///
/// See [`SocketOps::recv`].
#[derive(Default)]
pub struct RecvOptions<'a> {
    /// If set, the sender's address is written here.
    ///
    /// This output choice does not control peer admission. A connected
    /// datagram endpoint filters by its peer whether or not an address is
    /// requested, while an unconnected endpoint accepts any sender.
    pub from: Option<&'a mut SocketAddrEx>,
    /// Receive flags.
    pub flags: RecvFlags,
    /// If set, ancillary control messages are appended here.
    pub cmsg: Option<&'a mut Vec<CMsgData>>,
    /// Exact per-operation nonblocking state captured by a caller.
    ///
    /// `None` samples the socket's ordinary mutable state at operation entry.
    /// [`RecvFlags::DONT_WAIT`] always forces nonblocking behavior.
    pub nonblocking_override: Option<bool>,
}
impl RecvOptions<'_> {
    pub(crate) fn effective_nonblocking(&self, socket_nonblocking: bool) -> bool {
        self.flags.contains(RecvFlags::DONT_WAIT)
            || self.nonblocking_override.unwrap_or(socket_nonblocking)
    }
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

/// Socket endpoint direction used by a composite transfer retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketTransferDirection {
    /// The socket supplies bytes to another endpoint.
    Receive,
    /// The socket accepts bytes from another endpoint.
    Send,
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
    /// Raw IPv4/IPv6 IP socket.
    Raw(RawSocket),
    /// UDP socket.
    Udp(UdpSocket),
    /// TCP socket.
    Tcp(TcpSocket),
    /// Datagram Congestion Control Protocol socket.
    Dccp(DccpSocket),
    /// Stream Control Transmission Protocol association socket.
    Sctp(SctpSocket),
    /// Unix domain socket.
    Unix(UnixSocket),
    /// Virtio socket.
    #[cfg(feature = "vsock")]
    Vsock(VsockSocket),
}

/// Exact backend object retained before an accept operation mutates its listen
/// queue. Dropping a reservation restores availability when the same listener
/// is still live.
pub enum SocketAcceptReservation<'a> {
    Tcp(TcpAcceptReservation),
    Dccp(crate::dccp::DccpAcceptReservation<'a>),
    Sctp(SctpAcceptReservation<'a>),
    Unix(UnixAcceptReservation<'a>),
    #[cfg(feature = "vsock")]
    Vsock(VsockSocketAcceptReservation),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SocketAcceptIdentity {
    Tcp(crate::tcp::TcpAcceptIdentity),
    Dccp(core::net::SocketAddr),
    Sctp(core::net::SocketAddr),
    Unix(crate::unix::UnixEndpointIdentity),
    #[cfg(feature = "vsock")]
    Vsock(crate::vsock::VsockConnId),
}

impl SocketAcceptReservation<'_> {
    pub fn identity(&self) -> SocketAcceptIdentity {
        match self {
            Self::Tcp(reservation) => SocketAcceptIdentity::Tcp(reservation.identity()),
            Self::Dccp(reservation) => SocketAcceptIdentity::Dccp(reservation.identity()),
            Self::Sctp(reservation) => SocketAcceptIdentity::Sctp(reservation.identity()),
            Self::Unix(reservation) => SocketAcceptIdentity::Unix(reservation.accepted_identity()),
            #[cfg(feature = "vsock")]
            Self::Vsock(reservation) => {
                SocketAcceptIdentity::Vsock(reservation.connection_identity())
            }
        }
    }

    pub fn commit(self) -> AxResult<Socket> {
        match self {
            Self::Tcp(reservation) => reservation.commit(),
            Self::Dccp(reservation) => reservation.commit(),
            Self::Sctp(reservation) => reservation.commit(),
            Self::Unix(reservation) => reservation.commit(),
            #[cfg(feature = "vsock")]
            Self::Vsock(reservation) => reservation.commit(),
        }
    }
}

impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        match self {
            Socket::Raw(raw) => raw.poll(),
            Socket::Tcp(tcp) => tcp.poll(),
            Socket::Dccp(dccp) => dccp.poll(),
            Socket::Sctp(sctp) => sctp.poll(),
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
            Socket::Raw(raw) => raw.register(context, events),
            Socket::Tcp(tcp) => tcp.register(context, events),
            Socket::Dccp(dccp) => dccp.register(context, events),
            Socket::Sctp(sctp) => sctp.register(context, events),
            Socket::Udp(udp) => udp.register(context, events),
            Socket::Unix(unix) => unix.register(context, events),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.register(context, events),
        }
    }
}

impl Socket {
    /// Returns a side-effect-free snapshot of data available to receive.
    ///
    /// Stream sockets report all currently queued bytes. Datagram-oriented
    /// sockets report only the payload length of the next queued datagram,
    /// never the sum of multiple queued datagrams. An empty receive queue is
    /// reported as zero. Implementations release their internal state locks
    /// before returning, so callers may safely fault while copying this value
    /// to userspace.
    pub fn recv_pending_len(&self) -> AxResult<usize> {
        match self {
            Self::Raw(raw) => raw.recv_pending_len(),
            Self::Tcp(tcp) => tcp.recv_pending_len(),
            Self::Dccp(dccp) => dccp.recv_pending_len(),
            Self::Sctp(sctp) => sctp.recv_pending_len(),
            Self::Udp(udp) => udp.recv_pending_len(),
            Self::Unix(unix) => unix.recv_pending_len(),
            #[cfg(feature = "vsock")]
            Self::Vsock(vsock) => vsock.recv_pending_len(),
        }
    }

    /// Retains the exact next accepted transport before queue mutation becomes
    /// visible. Policy can inspect this opaque reservation and then either drop
    /// it or commit the same backend object.
    pub fn prepare_accept(&self) -> AxResult<SocketAcceptReservation<'_>> {
        match self {
            Self::Tcp(tcp) => tcp.prepare_accept().map(SocketAcceptReservation::Tcp),
            Self::Dccp(dccp) => dccp.prepare_accept().map(SocketAcceptReservation::Dccp),
            Self::Sctp(sctp) => sctp.prepare_accept().map(SocketAcceptReservation::Sctp),
            Self::Unix(unix) => unix.prepare_accept().map(SocketAcceptReservation::Unix),
            Self::Raw(_) | Self::Udp(_) => Err(AxError::OperationNotSupported),
            #[cfg(feature = "vsock")]
            Self::Vsock(vsock) => vsock.prepare_accept().map(SocketAcceptReservation::Vsock),
        }
    }

    /// Runs a composite transfer attempt under this socket's directional
    /// blocking, timeout, pending-error, and readiness policy.
    ///
    /// `Err(AxError::WouldBlock)` from `attempt` means that this socket endpoint
    /// needs readiness and is retried by the socket poller. A caller can encode
    /// an opposite-endpoint `WouldBlock` as any successful `T`; that completes
    /// the poller so the outer transfer driver can attribute and wait on the
    /// other endpoint without resetting this socket operation's deadline.
    pub fn retry_transfer<T>(
        &self,
        direction: SocketTransferDirection,
        effective_nonblocking: bool,
        mut attempt: impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        match self {
            Socket::Raw(_) => attempt(),
            Socket::Tcp(tcp) => tcp.retry_transfer(direction, effective_nonblocking, &mut attempt),
            Socket::Dccp(dccp) => {
                dccp.retry_transfer(direction, effective_nonblocking, &mut attempt)
            }
            Socket::Sctp(sctp) => {
                sctp.retry_transfer(direction, effective_nonblocking, &mut attempt)
            }
            Socket::Udp(udp) => udp.retry_transfer(direction, effective_nonblocking, &mut attempt),
            Socket::Unix(unix) => {
                unix.retry_transfer(direction, effective_nonblocking, &mut attempt)
            }
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => {
                vsock.retry_transfer(direction, effective_nonblocking, &mut attempt)
            }
        }
    }

    /// Stores an error for one-shot SO_ERROR reporting and the next operation
    /// that consumes deferred socket errors.
    pub fn set_pending_error(&self, error: SocketFault) {
        match self {
            Socket::Raw(raw) => raw.set_pending_error(error),
            Socket::Tcp(tcp) => tcp.set_pending_error(error),
            Socket::Dccp(dccp) => dccp.set_pending_error(error),
            Socket::Sctp(sctp) => sctp.set_pending_error(error),
            Socket::Udp(udp) => udp.set_pending_error(error),
            Socket::Unix(unix) => unix.set_pending_error(error),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.set_pending_error(error),
        }
    }

    pub fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        match self {
            Socket::Raw(_) => Err(AxError::OperationNotSupported),
            Socket::Unix(unix) => unix.set_filter(filter),
            Socket::Udp(udp) => udp.set_filter(filter),
            Socket::Tcp(tcp) => tcp.set_filter(filter),
            Socket::Dccp(dccp) => dccp.set_filter(filter),
            Socket::Sctp(_) => Err(AxError::OperationNotSupported),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.set_filter(filter),
        }
    }

    pub fn disconnect(&self) -> AxResult<()> {
        match self {
            Socket::Raw(raw) => {
                raw.disconnect();
                Ok(())
            }
            Socket::Tcp(tcp) => tcp.disconnect(),
            Socket::Dccp(dccp) => dccp.disconnect(),
            Socket::Sctp(_) => Err(AxError::OperationNotSupported),
            Socket::Udp(udp) => {
                udp.disconnect();
                Ok(())
            }
            Socket::Unix(unix) => unix.disconnect(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(_) => Err(AxError::OperationNotSupported),
        }
    }

    pub fn set_ipv6_addrform_to_ipv4(&self) -> Result<(), Ipv6AddrFormError> {
        match self {
            Socket::Tcp(tcp) => tcp.set_ipv6_addrform_to_ipv4(),
            Socket::Raw(_)
            | Socket::Udp(_)
            | Socket::Unix(_)
            | Socket::Dccp(_)
            | Socket::Sctp(_) => Err(Ipv6AddrFormError::UnsupportedSocket),
            #[cfg(feature = "vsock")]
            Socket::Vsock(_) => Err(Ipv6AddrFormError::UnsupportedSocket),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_family_conversion_reports_a_transport_neutral_fact() {
        assert_eq!(
            SocketAddrEx::Unix(UnixSocketAddr::Unnamed).into_ip(),
            Err(AddressFamilyMismatch)
        );
        assert!(matches!(
            SocketAddrEx::Ip("127.0.0.1:1".parse().unwrap()).into_unix(),
            Err(AddressFamilyMismatch)
        ));
    }
}
