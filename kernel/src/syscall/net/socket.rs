use alloc::sync::Arc;
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodePermission;
#[cfg(feature = "vsock")]
use axnet::vsock::{VsockSocket, VsockStreamTransport};
use axnet::{
    MAX_LISTEN_BACKLOG, Shutdown, Socket as SocketInner, SocketAddrEx, SocketOps,
    dccp::DccpSocket,
    raw::{RawSocket, RawSocketFamily},
    sctp::SctpSocket,
    tcp::TcpSocket,
    udp::{UdpSocket, UdpSocketFamily},
    unix::{DgramTransport, SeqPacketTransport, StreamTransport, UnixSocket, UnixSocketAddr},
};
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    general::{CAP_NET_BIND_SERVICE, CAP_NET_RAW, O_CLOEXEC, O_NONBLOCK, O_RDWR},
    net::{
        AF_INET, AF_INET6, AF_MAX, AF_NETLINK, AF_PACKET, AF_UNIX, AF_UNSPEC, AF_VSOCK,
        IPPROTO_TCP, IPPROTO_UDP, SHUT_RD, SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW,
        SOCK_SEQPACKET, SOCK_STREAM, sockaddr, socklen_t,
    },
};
use thekernel_linux_net::{SocketFailure, socket_failure_errno};

use super::{
    SocketSyscallSnapshot,
    addr::SocketAddrExt,
    packet::{decode_bind_address, snapshot_address},
};
use crate::{
    file::{
        AcceptedSocketSecurityRef, AfAlgSocket, BareAcceptedSocketSecurityRef, FileDescription,
        FileHandle, FileLike, NetlinkSocket, PacketSocket, PendingSocketSecurityRef,
        PinnedSocketDescription, PreparedSocketAddress, Socket, SocketBackendKind, XdpSocket,
        af_alg, af_xdp, close_file_like, packet_socket::packet_error,
        permission::VfsSecurityContext, reserve_fd,
    },
    mm::{UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
    task::{
        NetworkNamespace, ns_capable,
        security::{
            LANDLOCK_ACCESS_NET_BIND_TCP, LANDLOCK_ACCESS_NET_CONNECT_TCP, SocketCreateSpec,
            SocketListenBacklog, SocketSecurityContext, check_current_landlock_net_port,
            dispatch_socket,
        },
    },
};

const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
const SOCK_DCCP: u32 = 6;
const IPPROTO_DCCP: u32 = 33;
const IPPROTO_SCTP: u32 = 132;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_CLOEXEC_NONBLOCK_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;

/// Creates a fresh kernel-owned TCP OFD in `net_ns` for a transport that must
/// reconnect without reusing a userspace FD or its retired socket state.
pub(crate) fn reconnect_tcp_socket(
    net_ns: Arc<NetworkNamespace>,
    peer: SocketAddrEx,
    creator: (
        Arc<crate::task::Cred>,
        crate::task::security::LandlockDomain,
    ),
) -> AxResult<FileHandle<Socket>> {
    let SocketAddrEx::Ip(ip_peer) = peer else {
        return Err(AxError::InvalidInput);
    };
    let peer = SocketAddrEx::Ip(ip_peer);
    let domain = if ip_peer.is_ipv6() { AF_INET6 } else { AF_INET };
    let spec =
        SocketCreateSpec::try_new(domain as i32, SOCK_STREAM as i32, IPPROTO_TCP as i32, false)
            .expect("fixed reconnect TCP socket spec must be valid");
    dispatch_socket(&SocketSecurityContext::create(&creator.0, spec))?;
    let socket = Socket::new(
        SocketInner::Tcp(TcpSocket::new(net_ns.stack().clone())?),
        net_ns,
    );
    socket.capture_creator_security(creator.0.clone(), creator.1.clone());
    let pinned = prepare_new_socket_like(socket, false)?;
    dispatch_socket_post_create(&creator.0, &pinned, spec)?;
    let socket_ref = pinned.security_ref()?;
    let prepared = PreparedSocketAddress::Network(peer.clone());
    dispatch_socket(&SocketSecurityContext::connect(
        &creator.0,
        &socket_ref,
        &prepared,
        0,
    ))?;
    if ip_peer.port() != 0 {
        creator
            .1
            .check_net_port(ip_peer.port(), LANDLOCK_ACCESS_NET_CONNECT_TCP)?;
    }
    let handle = pinned
        .into_description()
        .file_handle()
        .downcast::<Socket>()?;
    handle.connect(peer)?;
    Ok(handle)
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct SockaddrXdp {
    family: u16,
    flags: u16,
    ifindex: u32,
    queue_id: u32,
    shared_umem_fd: u32,
}

pub(crate) fn socket_failure(failure: SocketFailure) -> AxError {
    LinuxError::try_from(socket_failure_errno(failure))
        .expect("linux ABI socket failure errno must be valid")
        .into()
}

pub(crate) fn map_socket_send_error(socket: &SocketInner, error: AxError) -> AxError {
    match socket {
        SocketInner::Raw(_)
        | SocketInner::Tcp(_)
        | SocketInner::Udp(_)
        | SocketInner::Dccp(_)
        | SocketInner::Sctp(_)
            if error == AxError::NoSuchDevice =>
        {
            socket_failure(SocketFailure::NetworkUnreachable)
        }
        SocketInner::Raw(_)
        | SocketInner::Tcp(_)
        | SocketInner::Udp(_)
        | SocketInner::Dccp(_)
        | SocketInner::Sctp(_)
            if error == AxError::NotFound =>
        {
            socket_failure(SocketFailure::AddressUnavailable)
        }
        SocketInner::Unix(_) if error == AxError::OperationNotSupported => {
            socket_failure(SocketFailure::PeerTypeMismatch)
        }
        _ if error == AxError::OutOfRange => socket_failure(SocketFailure::MessageTooLarge),
        _ => error,
    }
}

fn map_bind_error(socket: &SocketInner, error: AxError) -> AxError {
    if matches!(
        socket,
        SocketInner::Raw(_)
            | SocketInner::Tcp(_)
            | SocketInner::Udp(_)
            | SocketInner::Dccp(_)
            | SocketInner::Sctp(_)
    ) && error == AxError::NotFound
    {
        socket_failure(SocketFailure::AddressUnavailable)
    } else {
        error
    }
}

fn map_connect_error(socket: &SocketInner, error: AxError) -> AxError {
    match socket {
        SocketInner::Raw(_)
        | SocketInner::Tcp(_)
        | SocketInner::Udp(_)
        | SocketInner::Dccp(_)
        | SocketInner::Sctp(_)
            if error == AxError::NoSuchDevice =>
        {
            socket_failure(SocketFailure::NetworkUnreachable)
        }
        SocketInner::Raw(_)
        | SocketInner::Tcp(_)
        | SocketInner::Udp(_)
        | SocketInner::Dccp(_)
        | SocketInner::Sctp(_)
            if error == AxError::NotFound =>
        {
            socket_failure(SocketFailure::AddressUnavailable)
        }
        SocketInner::Unix(_) if error == AxError::OperationNotSupported => {
            socket_failure(SocketFailure::PeerTypeMismatch)
        }
        _ => error,
    }
}

/// Validates a transport-neutral address/socket pairing at the Linux ABI
/// boundary. AX intentionally does not assign an errno to this fact.
pub(super) fn validate_network_address(socket: &SocketInner, address: &SocketAddrEx) -> AxResult {
    let supported = match (socket, address) {
        (SocketInner::Raw(_), SocketAddrEx::Ip(_)) => true,
        (SocketInner::Tcp(_), SocketAddrEx::Ip(_)) => true,
        (SocketInner::Dccp(_), SocketAddrEx::Ip(_)) => true,
        (SocketInner::Sctp(_), SocketAddrEx::Ip(_)) => true,
        (SocketInner::Udp(udp), SocketAddrEx::Ip(address)) => {
            udp.family().accepts_socket_addr(*address)
        }
        (SocketInner::Unix(_), SocketAddrEx::Unix(_)) => true,
        #[cfg(feature = "vsock")]
        (SocketInner::Vsock(_), SocketAddrEx::Vsock(_)) => true,
        _ => false,
    };
    supported
        .then_some(())
        .ok_or_else(|| socket_failure(SocketFailure::AddressFamilyUnsupported))
}

const fn socket_status_flags(nonblocking: bool) -> u32 {
    O_RDWR | if nonblocking { O_NONBLOCK } else { 0 }
}

fn prepare_new_socket_like<T: FileLike + 'static>(
    socket: T,
    nonblocking: bool,
) -> AxResult<PinnedSocketDescription> {
    prepare_new_socket_arc(
        Arc::try_new(socket).map_err(|_| AxError::NoMemory)?,
        nonblocking,
    )
}

fn prepare_new_socket_arc(
    socket: Arc<dyn FileLike>,
    nonblocking: bool,
) -> AxResult<PinnedSocketDescription> {
    if nonblocking {
        socket.set_nonblocking(true)?;
    }
    let description = FileDescription::new_with_flags(socket, socket_status_flags(nonblocking))?;
    PinnedSocketDescription::from_description(description)
}

fn publish_new_socket_like(socket: PinnedSocketDescription, cloexec: bool) -> AxResult<i32> {
    reserve_fd(cloexec)?.publish(socket.into_description())
}

fn dispatch_socket_post_create(
    actor: &crate::task::Cred,
    socket: &PinnedSocketDescription,
    spec: SocketCreateSpec,
) -> AxResult<()> {
    let socket_ref = socket.security_ref()?;
    dispatch_socket(&SocketSecurityContext::post_create(
        actor,
        &socket_ref,
        spec,
    ))
}

fn clamp_listen_backlog(backlog: i32) -> usize {
    (backlog as u32 as usize).min(MAX_LISTEN_BACKLOG)
}

fn require_bind_permissions(
    addr: &SocketAddrEx,
    net_ns: &NetworkNamespace,
    actor: &crate::task::Cred,
) -> AxResult<()> {
    let SocketAddrEx::Ip(ip_addr) = addr else {
        return Ok(());
    };

    if ip_addr.port() != 0
        && ip_addr.port() < FIRST_UNPRIVILEGED_PORT
        && !ns_capable(actor, net_ns.owner_user_ns(), CAP_NET_BIND_SERVICE)
    {
        return Err(AxError::from(LinuxError::EACCES));
    }
    Ok(())
}

fn check_landlock_tcp_port(socket: &SocketInner, addr: &SocketAddrEx, access: u64) -> AxResult<()> {
    if let (SocketInner::Tcp(_), SocketAddrEx::Ip(ip_addr)) = (socket, addr)
        && ip_addr.port() != 0
    {
        check_current_landlock_net_port(ip_addr.port(), access)?;
    }
    Ok(())
}

fn validate_socket_type(ty: u32) -> AxResult<u32> {
    match ty {
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET | SOCK_RAW | SOCK_DCCP => Ok(ty),
        _ => Err(AxError::InvalidInput),
    }
}

fn parse_socket_type(raw_ty: u32) -> AxResult<(u32, bool, bool)> {
    let flags = raw_ty & !SOCK_TYPE_MASK;
    if flags & !SOCK_CLOEXEC_NONBLOCK_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let ty = validate_socket_type(raw_ty & SOCK_TYPE_MASK)?;
    Ok((ty, flags & O_NONBLOCK != 0, flags & O_CLOEXEC != 0))
}

fn parse_accept4_flags(flags: u32) -> AxResult<(bool, bool)> {
    if flags & !SOCK_CLOEXEC_NONBLOCK_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    Ok((flags & O_NONBLOCK != 0, flags & O_CLOEXEC != 0))
}

/// Accept through an already retained listener description.  io_uring uses
/// this entry point so fd close/reuse cannot redirect a queued accept.
pub(crate) fn accept_pinned(
    pinned: &PinnedSocketDescription,
    actor: &Arc<crate::task::Cred>,
    flags: u32,
) -> AxResult<isize> {
    let (nonblocking, cloexec) = parse_accept4_flags(flags)?;
    let listening_ref = pinned.security_ref()?;
    if pinned.backend()? != SocketBackendKind::Network {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let listener = pinned.network()?;
    let net_ns = listener.net_namespace().clone();
    let reservation = listener.prepare_accept()?;
    let unix_endpoint = match reservation.identity() {
        axnet::SocketAcceptIdentity::Unix(endpoint) => Some(endpoint),
        _ => None,
    };
    let pending_ref = PendingSocketSecurityRef::new(&reservation, &net_ns);
    dispatch_socket(&SocketSecurityContext::accept(
        actor,
        &listening_ref,
        &AcceptedSocketSecurityRef::Pending(pending_ref),
    ))?;
    let mut accepted = Socket::new(reservation.commit()?, net_ns);
    accepted.inherit_creator_security_from(listener);
    accepted.inherit_inet_identity_from(listener)?;
    let accepted = prepare_new_socket_like(accepted, nonblocking)?;
    if let Some(endpoint) = unix_endpoint {
        super::cmsg::register_unix_endpoint_owner(endpoint.raw(), accepted.description())?;
    }
    publish_new_socket_like(accepted, cloexec).map(|fd| fd as isize)
}

fn validate_pre_create_domain(domain: u32) -> AxResult<()> {
    if domain >= AF_MAX {
        Err(LinuxError::EAFNOSUPPORT.into())
    } else {
        Ok(())
    }
}

fn validate_packet_create_after_capability(
    capability_granted: bool,
    ty: u32,
    proto: u32,
) -> AxResult<(
    thekernel_linux_packet::PacketSocketType,
    thekernel_linux_packet::ProtocolSelector,
)> {
    if !capability_granted {
        return Err(LinuxError::EPERM.into());
    }
    let socket_type =
        thekernel_linux_packet::PacketSocketType::from_raw(ty as i32).map_err(packet_error)?;
    let protocol = thekernel_linux_packet::ProtocolSelector::from_network_order_i32(proto as i32);
    Ok((socket_type, protocol))
}

/// Completes one AF_PACKET creation after its create hook has admitted the
/// normalized request, but before any descriptor can be published.
fn prepare_packet_socket_after_create(
    actor: &crate::task::Cred,
    net_ns: Arc<NetworkNamespace>,
    ty: u32,
    proto: u32,
    nonblocking: bool,
    spec: SocketCreateSpec,
) -> AxResult<PinnedSocketDescription> {
    // Linux checks CAP_NET_RAW in the user namespace governing this exact
    // network namespace before AF_PACKET-specific type validation.
    let (socket_type, protocol) = validate_packet_create_after_capability(
        ns_capable(actor, net_ns.owner_user_ns(), CAP_NET_RAW),
        ty,
        proto,
    )?;
    let socket = PacketSocket::try_new(socket_type, protocol, net_ns)?;
    let socket = prepare_new_socket_arc(socket, nonblocking)?;
    dispatch_socket_post_create(actor, &socket, spec)?;
    Ok(socket)
}

/// Runs Linux's two independent socket-creation leaves and the pair hook over
/// unpublished AF_PACKET descriptions.
///
/// `packet_ops` has no pair mechanism.  Keeping both descriptions private
/// means every denial or final `EOPNOTSUPP` drops both lower endpoints without
/// reserving or publishing an fd.
fn prepare_packet_socket_pair(
    actor: &crate::task::Cred,
    net_ns: &Arc<NetworkNamespace>,
    ty: u32,
    proto: u32,
    nonblocking: bool,
    spec: SocketCreateSpec,
) -> AxResult<(PinnedSocketDescription, PinnedSocketDescription)> {
    dispatch_socket(&SocketSecurityContext::create(actor, spec))?;
    let first =
        prepare_packet_socket_after_create(actor, net_ns.clone(), ty, proto, nonblocking, spec)?;

    dispatch_socket(&SocketSecurityContext::create(actor, spec))?;
    let second =
        prepare_packet_socket_after_create(actor, net_ns.clone(), ty, proto, nonblocking, spec)?;

    {
        let first_ref = first.security_ref()?;
        let second_ref = second.security_ref()?;
        dispatch_socket(&SocketSecurityContext::pair(actor, &first_ref, &second_ref))?;
    }
    Ok((first, second))
}

fn packet_socketpair_after_parse(
    actor: &crate::task::Cred,
    net_ns: &Arc<NetworkNamespace>,
    ty: u32,
    proto: u32,
    nonblocking: bool,
    spec: SocketCreateSpec,
) -> AxResult<isize> {
    let (_first, _second) =
        prepare_packet_socket_pair(actor, net_ns, ty, proto, nonblocking, spec)?;
    Err(LinuxError::EOPNOTSUPP.into())
}

fn supported_stream_protocol(proto: u32) -> bool {
    proto == 0 || proto == IPPROTO_TCP as u32
}

fn supported_datagram_protocol(proto: u32) -> bool {
    proto == 0 || proto == IPPROTO_UDP as u32
}

fn inet_socketpair_error(ty: u32, proto: u32) -> AxError {
    match ty {
        SOCK_RAW => AxError::from(LinuxError::EPROTONOSUPPORT),
        SOCK_DGRAM => {
            if supported_datagram_protocol(proto) {
                AxError::from(LinuxError::EOPNOTSUPP)
            } else {
                AxError::from(LinuxError::EPROTONOSUPPORT)
            }
        }
        SOCK_DCCP => AxError::from(LinuxError::EPROTONOSUPPORT),
        SOCK_STREAM => {
            if supported_stream_protocol(proto) {
                AxError::from(LinuxError::EOPNOTSUPP)
            } else {
                AxError::from(LinuxError::EPROTONOSUPPORT)
            }
        }
        _ => AxError::InvalidInput,
    }
}

pub fn sys_socket(domain: u32, raw_ty: u32, proto: u32) -> AxResult<isize> {
    debug!("sys_socket <= domain: {domain}, ty: {raw_ty}, proto: {proto}");
    let (ty, nonblocking, cloexec) = parse_socket_type(raw_ty)?;
    validate_pre_create_domain(domain)?;
    let snapshot = SocketSyscallSnapshot::capture();
    let spec = SocketCreateSpec::try_new(domain as i32, ty as i32, proto as i32, false)
        .ok_or(AxError::InvalidInput)?;
    let actor = snapshot.actor();
    dispatch_socket(&SocketSecurityContext::create(actor, spec))?;

    if domain == AF_PACKET {
        let net_ns = snapshot.net_namespace().clone();
        let socket =
            prepare_packet_socket_after_create(actor, net_ns, ty, proto, nonblocking, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    if domain == af_xdp::AF_XDP {
        // AF_XDP is SOCK_RAW/protocol 0 only.  It is a dedicated FileLike
        // backend because its ABI is setsockopt/bind/mmap rings, not axnet
        // byte-stream socket operations.
        if ty != SOCK_RAW || proto != 0 {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        let socket = XdpSocket::try_new(snapshot.net_namespace().clone())?;
        let socket = prepare_new_socket_arc(socket, nonblocking)?;
        dispatch_socket_post_create(actor, &socket, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    if domain == af_alg::AF_ALG {
        AfAlgSocket::validate_socket_type(ty, proto)?;
        let socket = prepare_new_socket_like(AfAlgSocket::new_listener(), nonblocking)?;
        dispatch_socket_post_create(actor, &socket, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    let net_ns = snapshot.net_namespace().clone();

    if domain == AF_NETLINK {
        NetlinkSocket::validate_socket_type(ty, proto)?;
        if proto == crate::file::netlink::NETLINK_AUDIT {
            // Audit records contain cross-process security decisions.  Linux
            // keeps creation in initial-user-namespace CAP_AUDIT_READ
            // authority.  Bind and membership make the separate init-net
            // listener check at their state-mutation points.
            if !NetlinkSocket::audit_socket_creation_authorized(actor) {
                return Err(LinuxError::EPERM.into());
            }
        }
        let socket = NetlinkSocket::try_new(proto, net_ns)?;
        let socket = prepare_new_socket_arc(socket, nonblocking)?;
        dispatch_socket_post_create(actor, &socket, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    let net_stack = net_ns.stack().clone();

    let socket = match (domain, ty) {
        (AF_INET | AF_INET6, SOCK_STREAM) => {
            if !supported_stream_protocol(proto) {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            SocketInner::Tcp(TcpSocket::new(net_stack.clone())?)
        }
        (AF_INET | AF_INET6, SOCK_DGRAM) => {
            if !supported_datagram_protocol(proto) {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            let family = if domain == AF_INET6 {
                UdpSocketFamily::Ipv6
            } else {
                UdpSocketFamily::Ipv4
            };
            SocketInner::Udp(UdpSocket::new_with_family(net_stack.clone(), family)?)
        }
        (AF_INET | AF_INET6, SOCK_DCCP) => {
            if proto != 0 && proto != IPPROTO_DCCP {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            let family = if domain == AF_INET6 {
                RawSocketFamily::Ipv6
            } else {
                RawSocketFamily::Ipv4
            };
            SocketInner::Dccp(DccpSocket::new(net_stack.clone(), family)?)
        }
        // Linux exposes SCTP as a sequenced-packet transport.  Protocol zero
        // selects SCTP for this socket type just as it selects TCP/UDP for the
        // stream/datagram cases.
        (AF_INET | AF_INET6, SOCK_SEQPACKET) => {
            if proto != 0 && proto != IPPROTO_SCTP {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            let family = if domain == AF_INET6 {
                RawSocketFamily::Ipv6
            } else {
                RawSocketFamily::Ipv4
            };
            SocketInner::Sctp(SctpSocket::new(net_stack.clone(), family)?)
        }
        (AF_INET | AF_INET6, SOCK_RAW) => {
            if !ns_capable(actor, net_ns.owner_user_ns(), CAP_NET_RAW) {
                return Err(AxError::from(LinuxError::EPERM));
            }
            if proto == 0 || proto > u8::MAX as u32 {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            let family = if domain == AF_INET6 {
                RawSocketFamily::Ipv6
            } else {
                RawSocketFamily::Ipv4
            };
            SocketInner::Raw(RawSocket::new(net_stack.clone(), family, proto as u8)?)
        }
        (AF_UNIX, SOCK_STREAM) => SocketInner::Unix(UnixSocket::new(
            StreamTransport::new()?,
            net_stack.unix_namespace(),
        )),
        (AF_UNIX, SOCK_DGRAM) => SocketInner::Unix(UnixSocket::new(
            DgramTransport::new()?,
            net_stack.unix_namespace(),
        )),
        (AF_UNIX, SOCK_SEQPACKET) => SocketInner::Unix(UnixSocket::new(
            SeqPacketTransport::new()?,
            net_stack.unix_namespace(),
        )),
        #[cfg(feature = "vsock")]
        (AF_VSOCK, SOCK_STREAM) => {
            SocketInner::Vsock(VsockSocket::new(VsockStreamTransport::new()))
        }
        (AF_INET, _) | (AF_INET6, _) | (AF_UNIX, _) | (AF_VSOCK, _) => {
            warn!("Unsupported socket type: domain: {domain}, ty: {ty}");
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        _ => {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
    };
    let mut socket = Socket::new(socket, net_ns);
    socket.capture_creator_security(Arc::clone(actor), snapshot.landlock_domain().clone());
    if matches!(domain, AF_INET | AF_INET6) {
        let effective_protocol = match ty {
            SOCK_STREAM if proto == 0 => IPPROTO_TCP as u32,
            SOCK_DGRAM if proto == 0 => IPPROTO_UDP as u32,
            SOCK_DCCP if proto == 0 => IPPROTO_DCCP,
            SOCK_SEQPACKET if proto == 0 => IPPROTO_SCTP,
            _ => proto,
        };
        socket.register_sock_diag(domain as u16, ty as u8, effective_protocol as u8)?;
    }
    let socket = prepare_new_socket_like(socket, nonblocking)?;
    dispatch_socket_post_create(actor, &socket, spec)?;
    publish_new_socket_like(socket, cloexec).map(|fd| fd as isize)
}

pub fn sys_bind(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserConstPtr<sockaddr>,
    addrlen: u32,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    match pinned.backend()? {
        SocketBackendKind::AfAlg => {
            let addr = af_alg::SockAddrAlg::read_from_user(&capability, addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, af_alg: {addr:?}");
            let prepared = PreparedSocketAddress::AfAlg(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            let PreparedSocketAddress::AfAlg(addr) = prepared else {
                unreachable!();
            };
            pinned.af_alg()?.bind(addr)?;
        }
        SocketBackendKind::Netlink => {
            if addrlen as usize != size_of::<crate::file::netlink::SockaddrNl>() {
                return Err(AxError::InvalidInput);
            }
            let addr = unsafe {
                // Every byte is copied into the MaybeUninit storage before
                // success, and SockaddrNl contains only integer fields for which
                // every bit pattern is valid.
                capability
                    .read_value_uninit(
                        addr.address().as_usize() as *const crate::file::netlink::SockaddrNl
                    )
                    .map_err(map_usercopy_error)?
                    .assume_init()
            };
            if addr.nl_family as u32 != AF_NETLINK {
                return Err(AxError::from(LinuxError::EAFNOSUPPORT));
            }
            let prepared = PreparedSocketAddress::Netlink(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            if addr.nl_pid == 0 {
                pinned
                    .netlink()?
                    .bind_auto(snapshot.pid(), addr.nl_groups)?;
            } else {
                pinned.netlink()?.bind(addr.nl_pid, addr.nl_groups)?;
            }
        }
        SocketBackendKind::Packet => {
            let address = snapshot_address(&capability, addr, addrlen)?;
            let prepared = PreparedSocketAddress::Packet(address);
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            let PreparedSocketAddress::Packet(address) = &prepared else {
                unreachable!();
            };
            let request = decode_bind_address(address)?;
            pinned.packet()?.bind(request)?;
        }
        SocketBackendKind::Xdp => {
            if addrlen as usize != size_of::<SockaddrXdp>() {
                return Err(AxError::InvalidInput);
            }
            let address = capability
                .read_value(addr.address().as_usize() as *const SockaddrXdp)
                .map_err(map_usercopy_error)?;
            if address.family as u32 != af_xdp::AF_XDP {
                return Err(LinuxError::EAFNOSUPPORT.into());
            }
            // Sharing an existing socket's UMEM needs a second retained MM
            // pin owner and is not yet admitted; the ordinary bind form
            // ignores this padding field exactly as Linux does.
            if address.flags & af_xdp::XDP_SHARED_UMEM != 0 {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            let prepared = PreparedSocketAddress::Unspecified;
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            pinned
                .xdp()?
                .endpoint()
                .bind(address.ifindex, address.queue_id, address.flags)?;
        }
        SocketBackendKind::Network => {
            let socket = pinned.network()?;
            let addr = SocketAddrEx::read_from_user(&capability, addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, addr: {addr:?}");
            let prepared = PreparedSocketAddress::Network(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            let PreparedSocketAddress::Network(addr) = prepared else {
                unreachable!();
            };

            if let (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) =
                (&socket.inner, &addr)
            {
                let security = VfsSecurityContext::new(actor.clone());
                crate::file::unix_socket::bind_path(
                    unix,
                    path.clone(),
                    &security,
                    NodePermission::from_bits_truncate(0o777),
                    snapshot.umask(),
                    |endpoint| {
                        let _ = super::cmsg::register_unix_endpoint_owner(
                            endpoint.raw(),
                            pinned.description(),
                        );
                    },
                )?;
            } else if let (
                SocketInner::Unix(unix),
                SocketAddrEx::Unix(UnixSocketAddr::Abstract(_)),
            ) = (&socket.inner, &addr)
            {
                let owner = pinned.description().clone();
                unix.bind_abstract_with_publish(
                    addr.clone()
                        .into_unix()
                        .map_err(|_| AxError::InvalidInput)?,
                    |endpoint| {
                        let _ = super::cmsg::register_unix_endpoint_owner(endpoint.raw(), &owner);
                    },
                )?;
            } else {
                require_bind_permissions(&addr, socket.net_namespace(), actor)?;
                validate_network_address(&socket.inner, &addr)?;
                check_landlock_tcp_port(&socket.inner, &addr, LANDLOCK_ACCESS_NET_BIND_TCP)?;
                socket
                    .bind(addr)
                    .map_err(|error| map_bind_error(&socket.inner, error))?;
            }
        }
    }

    Ok(0)
}

pub fn sys_connect(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserConstPtr<sockaddr>,
    addrlen: u32,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    // Pin the open file description once. Address decoding intentionally
    // remains before the ENOTSOCK downcast for the ordinary connect path, but
    // a sibling sharing the fd table can no longer redirect the operation by
    // closing and reusing the numeric descriptor between those two steps.
    let pinned = PinnedSocketDescription::pin_fd(fd)?;

    if pinned.backend() == Ok(SocketBackendKind::Packet) {
        // Linux's generic connect layer copies the complete bounded address,
        // runs the security hook, and only then reaches sock_no_connect.
        // It does not impose sockaddr_ll bind/send validation here.
        let address = snapshot_address(&capability, addr, addrlen)?;
        let actor = snapshot.actor();
        let socket_ref = pinned.security_ref()?;
        let prepared = PreparedSocketAddress::Packet(address);
        dispatch_socket(&SocketSecurityContext::connect(
            actor,
            &socket_ref,
            &prepared,
            addrlen as usize,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }

    if addrlen as usize >= size_of::<linux_raw_sys::net::__kernel_sa_family_t>()
        && super::addr::read_family(&capability, addr, addrlen)? as u32 == AF_UNSPEC
    {
        debug!("sys_connect <= fd: {fd}, addr: AF_UNSPEC");
        let socket = pinned.network()?;
        let actor = snapshot.actor();
        let socket_ref = pinned.security_ref()?;
        let prepared = PreparedSocketAddress::Unspecified;
        dispatch_socket(&SocketSecurityContext::connect(
            actor,
            &socket_ref,
            &prepared,
            addrlen as usize,
        ))?;
        socket.disconnect()?;
        return Ok(0);
    }

    let addr = SocketAddrEx::read_from_user(&capability, addr, addrlen)?;
    debug!("sys_connect <= fd: {fd}, addr: {addr:?}");

    let socket = pinned.network()?;
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    let prepared = PreparedSocketAddress::Network(addr);
    dispatch_socket(&SocketSecurityContext::connect(
        actor,
        &socket_ref,
        &prepared,
        addrlen as usize,
    ))?;
    let PreparedSocketAddress::Network(addr) = prepared else {
        unreachable!();
    };
    validate_network_address(&socket.inner, &addr)?;
    check_landlock_tcp_port(&socket.inner, &addr, LANDLOCK_ACCESS_NET_CONNECT_TCP)?;
    let result = match (&socket.inner, &addr) {
        (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
            let security = VfsSecurityContext::new(actor.clone());
            let target = crate::file::unix_socket::resolve_peer(path.clone(), &security)?;
            if unix.is_datagram() {
                unix.connect_resolved_as(target, snapshot.unix_credentials())
            } else if unix.is_seqpacket() {
                let reservation = unix
                    .prepare_seqpacket_connect_resolved_as(target, snapshot.unix_credentials())?;
                let listening = crate::file::UnixEndpointSecurityRef::new(
                    reservation.listening_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                let accepted = crate::file::UnixEndpointSecurityRef::new(
                    reservation.accepted_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                dispatch_socket(&SocketSecurityContext::unix_stream_connect(
                    actor,
                    &socket_ref,
                    &listening,
                    &accepted,
                ))?;
                reservation.commit()
            } else {
                let reservation =
                    unix.prepare_stream_connect_resolved_as(target, snapshot.unix_credentials())?;
                let listening = crate::file::UnixEndpointSecurityRef::new(
                    reservation.listening_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                let accepted = crate::file::UnixEndpointSecurityRef::new(
                    reservation.accepted_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                dispatch_socket(&SocketSecurityContext::unix_stream_connect(
                    actor,
                    &socket_ref,
                    &listening,
                    &accepted,
                ))?;
                reservation.commit()
            }
        }
        (SocketInner::Unix(unix), SocketAddrEx::Unix(_)) => {
            if unix.is_datagram() {
                unix.connect_as(addr.clone(), snapshot.unix_credentials())
            } else if unix.is_seqpacket() {
                let reservation =
                    unix.prepare_seqpacket_connect_as(addr.clone(), snapshot.unix_credentials())?;
                let listening = crate::file::UnixEndpointSecurityRef::new(
                    reservation.listening_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                let accepted = crate::file::UnixEndpointSecurityRef::new(
                    reservation.accepted_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                dispatch_socket(&SocketSecurityContext::unix_stream_connect(
                    actor,
                    &socket_ref,
                    &listening,
                    &accepted,
                ))?;
                reservation.commit()
            } else {
                let reservation =
                    unix.prepare_stream_connect_as(addr.clone(), snapshot.unix_credentials())?;
                let listening = crate::file::UnixEndpointSecurityRef::new(
                    reservation.listening_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                let accepted = crate::file::UnixEndpointSecurityRef::new(
                    reservation.accepted_identity(),
                    socket.net_namespace(),
                    &reservation,
                );
                dispatch_socket(&SocketSecurityContext::unix_stream_connect(
                    actor,
                    &socket_ref,
                    &listening,
                    &accepted,
                ))?;
                reservation.commit()
            }
        }
        _ => socket.connect(addr.clone()),
    };
    result.map_err(|error| {
        if error == AxError::WouldBlock {
            AxError::InProgress
        } else {
            map_connect_error(&socket.inner, error)
        }
    })?;

    Ok(0)
}

pub fn sys_listen(fd: i32, backlog: i32) -> AxResult<isize> {
    debug!("sys_listen <= fd: {fd}, backlog: {backlog}");

    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    // Linux compares `(unsigned int)backlog` with somaxconn. Negative values
    // therefore clamp to the namespace maximum rather than to zero.
    let backlog = clamp_listen_backlog(backlog);
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    let prepared_backlog =
        SocketListenBacklog::try_from_clamped(backlog as i32).ok_or(AxError::InvalidInput)?;
    dispatch_socket(&SocketSecurityContext::listen(
        actor,
        &socket_ref,
        prepared_backlog,
    ))?;
    if pinned.backend()? == SocketBackendKind::Packet {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let socket = pinned.network()?;
    if let SocketInner::Unix(unix) = &socket.inner {
        unix.listen_as(backlog, snapshot.unix_credentials())?;
    } else {
        socket.listen(backlog)?;
    }

    Ok(0)
}

pub fn sys_accept(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    sys_accept4(capability, fd, addr, addrlen, 0)
}

pub fn sys_accept4(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
    flags: u32,
) -> AxResult<isize> {
    debug!("sys_accept <= fd: {fd}, flags: {flags}");

    let (nonblocking, cloexec) = parse_accept4_flags(flags)?;
    let snapshot = SocketSyscallSnapshot::capture();

    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let actor = snapshot.actor();
    let listening_ref = pinned.security_ref()?;
    if pinned.backend()? == SocketBackendKind::Packet {
        // AF_PACKET installs `sock_no_accept`, but Linux invokes
        // security_socket_accept() with an otherwise bare `newsock` first.
        // Preserve that policy ordering without allocating/subscribing an
        // endpoint which can never be published.
        let bare_ref = BareAcceptedSocketSecurityRef::new(
            SocketBackendKind::Packet,
            listening_ref.net_namespace(),
        );
        let accepted_ref = AcceptedSocketSecurityRef::Bare(bare_ref);
        dispatch_socket(&SocketSecurityContext::accept(
            actor,
            &listening_ref,
            &accepted_ref,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if pinned.backend()? == SocketBackendKind::AfAlg {
        let request = pinned.af_alg()?.accept_request()?;
        let request = prepare_new_socket_like(request, nonblocking)?;
        let accepted_ref = request.security_ref()?;
        let accepted_ref = AcceptedSocketSecurityRef::Description(accepted_ref);
        dispatch_socket(&SocketSecurityContext::accept(
            actor,
            &listening_ref,
            &accepted_ref,
        ))?;
        if !addr.is_null() {
            capability
                .write_value(addrlen.address().as_usize() as *mut socklen_t, 0)
                .map_err(map_usercopy_error)?;
        }
        return publish_new_socket_like(request, cloexec).map(|fd| fd as isize);
    }

    let listener = pinned.network()?;
    let net_ns = listener.net_namespace().clone();
    let reservation = listener.prepare_accept()?;
    let unix_endpoint = match reservation.identity() {
        axnet::SocketAcceptIdentity::Unix(endpoint) => Some(endpoint),
        _ => None,
    };
    let pending_ref = PendingSocketSecurityRef::new(&reservation, &net_ns);
    let accepted_ref = AcceptedSocketSecurityRef::Pending(pending_ref);
    dispatch_socket(&SocketSecurityContext::accept(
        actor,
        &listening_ref,
        &accepted_ref,
    ))?;
    let mut socket = Socket::new(reservation.commit()?, net_ns);
    socket.inherit_creator_security_from(listener);
    socket.inherit_inet_identity_from(listener)?;

    let remote_addr = socket.peer_addr()?;
    if !addr.is_null() {
        let mut value = capability
            .read_value(addrlen.address().as_usize() as *const socklen_t)
            .map_err(map_usercopy_error)?;
        remote_addr.write_to_user(&capability, addr, &mut value)?;
        capability
            .write_value(addrlen.address().as_usize() as *mut socklen_t, value)
            .map_err(map_usercopy_error)?;
    }

    let socket = prepare_new_socket_like(socket, nonblocking)?;
    if let Some(endpoint) = unix_endpoint {
        super::cmsg::register_unix_endpoint_owner(endpoint.raw(), socket.description())?;
    }
    let fd = publish_new_socket_like(socket, cloexec).map(|fd| fd as isize)?;
    debug!("sys_accept => fd: {fd}, addr: {remote_addr:?}");

    Ok(fd)
}

pub fn sys_shutdown(fd: i32, how: u32) -> AxResult<isize> {
    debug!("sys_shutdown <= fd: {fd}, how: {how:?}");

    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    shutdown_pinned_socket(&pinned, snapshot.actor(), how)
}

/// Shuts down one already pinned socket using the actor captured when the
/// operation was admitted.  This is shared by io_uring so neither an fd-table
/// reuse nor a later credential replacement can change its target or LSM
/// subject after SQE acceptance.
pub(crate) fn shutdown_pinned_socket(
    pinned: &PinnedSocketDescription,
    actor: &Arc<crate::task::Cred>,
    how: u32,
) -> AxResult<isize> {
    let socket_ref = pinned.security_ref()?;
    dispatch_socket(&SocketSecurityContext::shutdown(
        actor,
        &socket_ref,
        how as i32,
    ))?;
    if pinned.backend()? == SocketBackendKind::Packet {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let socket = pinned.network()?;
    let how = match how {
        SHUT_RD => Shutdown::Read,
        SHUT_WR => Shutdown::Write,
        SHUT_RDWR => Shutdown::Both,
        _ => return Err(AxError::InvalidInput),
    };
    socket.shutdown(how).map(|_| 0)
}

pub fn sys_socketpair(
    capability: UserMemoryCapability,
    domain: u32,
    raw_ty: u32,
    proto: u32,
    fds: UserPtr<[i32; 2]>,
) -> AxResult<isize> {
    debug!("sys_socketpair <= domain: {domain}, ty: {raw_ty}, proto: {proto}");
    let (ty, nonblocking, cloexec) = parse_socket_type(raw_ty)?;

    if domain == AF_PACKET {
        let snapshot = SocketSyscallSnapshot::capture();
        let spec = SocketCreateSpec::try_new(domain as i32, ty as i32, proto as i32, false)
            .ok_or(AxError::InvalidInput)?;
        let actor = snapshot.actor();
        let net_ns = snapshot.net_namespace();

        // Linux's generic socketpair path also exposes pre-reserved descriptor
        // numbers before the backend pair operation.  TheKernel's existing
        // generic publication order does not yet model that behavior.  This
        // unsupported AF_PACKET path deliberately publishes and writes none.
        return packet_socketpair_after_parse(actor, net_ns, ty, proto, nonblocking, spec);
    }

    if matches!(domain, AF_INET | AF_INET6) {
        return Err(inet_socketpair_error(ty, proto));
    }

    if domain != AF_UNIX {
        return Err(AxError::from(LinuxError::EAFNOSUPPORT));
    }

    let snapshot = SocketSyscallSnapshot::capture();
    let spec = SocketCreateSpec::try_new(domain as i32, ty as i32, proto as i32, false)
        .ok_or(AxError::InvalidInput)?;
    let actor = snapshot.actor();
    dispatch_socket(&SocketSecurityContext::create(actor, spec))?;
    dispatch_socket(&SocketSecurityContext::create(actor, spec))?;

    let credentials = snapshot.unix_credentials();
    let net_ns = snapshot.net_namespace().clone();
    let unix_namespace = net_ns.stack().unix_namespace();
    let (sock1, sock2) = match ty {
        SOCK_STREAM => {
            let (sock1, sock2) = StreamTransport::new_pair(credentials)?;
            (
                UnixSocket::new(sock1, unix_namespace.clone()),
                UnixSocket::new(sock2, unix_namespace.clone()),
            )
        }
        SOCK_DGRAM => {
            let (sock1, sock2) = DgramTransport::new_pair(credentials)?;
            (
                UnixSocket::new(sock1, unix_namespace.clone()),
                UnixSocket::new(sock2, unix_namespace),
            )
        }
        SOCK_SEQPACKET => {
            let (sock1, sock2) = SeqPacketTransport::new_pair(credentials)?;
            (
                UnixSocket::new(sock1, unix_namespace.clone()),
                UnixSocket::new(sock2, unix_namespace),
            )
        }
        SOCK_RAW => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        _ => {
            warn!("Unsupported socketpair type: {ty}");
            return Err(AxError::InvalidInput);
        }
    };
    let sock1 = Socket::new(SocketInner::Unix(sock1), net_ns.clone());
    let sock2 = Socket::new(SocketInner::Unix(sock2), net_ns);

    if nonblocking {
        sock1.set_nonblocking(true)?;
        sock2.set_nonblocking(true)?;
    }

    let status_flags = socket_status_flags(nonblocking);
    let description1 = FileDescription::new_with_flags(
        Arc::try_new(sock1).map_err(|_| AxError::NoMemory)? as Arc<dyn FileLike>,
        status_flags,
    )?;
    let description2 = FileDescription::new_with_flags(
        Arc::try_new(sock2).map_err(|_| AxError::NoMemory)? as Arc<dyn FileLike>,
        status_flags,
    )?;
    let socket1 = PinnedSocketDescription::from_description(description1)?;
    let socket2 = PinnedSocketDescription::from_description(description2)?;
    dispatch_socket_post_create(actor, &socket1, spec)?;
    dispatch_socket_post_create(actor, &socket2, spec)?;
    {
        let socket1_ref = socket1.security_ref()?;
        let socket2_ref = socket2.security_ref()?;
        dispatch_socket(&SocketSecurityContext::pair(
            actor,
            &socket1_ref,
            &socket2_ref,
        ))?;
    }

    // No descriptor number or userspace output exists before every create,
    // post-create, and pair hook has admitted both private endpoints.
    let reserved1 = reserve_fd(cloexec)?;
    let reserved2 = reserve_fd(cloexec)?;
    let fd_pair = [reserved1.fd(), reserved2.fd()];
    capability
        .write_slice(fds.address().as_usize() as *mut i32, &fd_pair)
        .map_err(map_usercopy_error)?;

    let fd1 = reserved1.publish(socket1.into_description())?;
    if let Err(error) = reserved2.publish(socket2.into_description()) {
        let _ = close_file_like(fd1);
        return Err(error);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::security::{
        SocketPairSecurityTestProbe, socket_pair_security_test_credential,
    };

    #[test]
    fn socket_creation_flags_keep_read_write_and_nonblocking_on_the_ofd() {
        assert_eq!(socket_status_flags(false), O_RDWR);
        assert_eq!(socket_status_flags(true), O_RDWR | O_NONBLOCK);
    }

    #[test]
    fn linux_abi_socket_failures_are_mapped_only_at_the_syscall_boundary() {
        assert_eq!(
            socket_failure(SocketFailure::AddressFamilyUnsupported),
            LinuxError::EAFNOSUPPORT.into()
        );
        assert_eq!(
            socket_failure(SocketFailure::ProtocolOptionUnsupported),
            LinuxError::ENOPROTOOPT.into()
        );
        assert_eq!(
            socket_failure(SocketFailure::MessageTooLarge),
            LinuxError::EMSGSIZE.into()
        );
    }

    #[test]
    fn packet_reaches_family_create_while_out_of_range_families_do_not() {
        assert!(validate_pre_create_domain(AF_PACKET).is_ok());
        assert_eq!(
            validate_pre_create_domain(AF_MAX),
            Err(LinuxError::EAFNOSUPPORT.into())
        );
        assert!(validate_pre_create_domain(AF_INET).is_ok());
    }

    #[test]
    fn packet_capability_precedes_family_specific_type_and_protocol_validation() {
        assert_eq!(
            validate_packet_create_after_capability(false, SOCK_STREAM, u32::MAX),
            Err(LinuxError::EPERM.into())
        );
        assert_eq!(
            validate_packet_create_after_capability(true, SOCK_STREAM, 0),
            Err(LinuxError::ESOCKTNOSUPPORT.into())
        );
        assert!(validate_packet_create_after_capability(true, SOCK_RAW, u32::MAX).is_ok());
        assert!(
            validate_packet_create_after_capability(
                true,
                SOCK_DGRAM,
                u32::from(0x0800_u16.to_be())
            )
            .is_ok()
        );
    }

    #[test]
    fn packet_socketpair_reaches_pair_then_drops_both_private_endpoints() {
        let _context = crate::file::packet_socket::packet_test_context();
        let user_namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let net_namespace =
            NetworkNamespace::try_new_loopback_only(user_namespace.clone()).unwrap();
        let probe = SocketPairSecurityTestProbe::new(net_namespace.clone(), false);
        let actor = socket_pair_security_test_credential(user_namespace, probe.clone());
        let raw_spec =
            SocketCreateSpec::try_new(AF_PACKET as i32, SOCK_RAW as i32, 0, false).unwrap();

        assert_eq!(
            packet_socketpair_after_parse(&actor, &net_namespace, SOCK_RAW, 0, false, raw_spec,),
            Err(LinuxError::EOPNOTSUPP.into())
        );

        let dgram_spec =
            SocketCreateSpec::try_new(AF_PACKET as i32, SOCK_DGRAM as i32, 0, false).unwrap();
        assert_eq!(
            packet_socketpair_after_parse(&actor, &net_namespace, SOCK_DGRAM, 0, true, dgram_spec,),
            Err(LinuxError::EOPNOTSUPP.into())
        );
        probe.assert_complete_cycles(2);
    }

    #[test]
    fn packet_socketpair_pair_denial_drops_both_private_endpoints() {
        let _context = crate::file::packet_socket::packet_test_context();
        let user_namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let net_namespace =
            NetworkNamespace::try_new_loopback_only(user_namespace.clone()).unwrap();
        let probe = SocketPairSecurityTestProbe::new(net_namespace.clone(), true);
        let actor = socket_pair_security_test_credential(user_namespace, probe.clone());
        let spec = SocketCreateSpec::try_new(AF_PACKET as i32, SOCK_RAW as i32, 0, false).unwrap();

        // Pair denial happens only after both endpoints exist. Repeating past
        // the broker's 64-endpoint bound proves every denied transaction drops
        // both unpublished descriptions and unregisters their lower endpoints.
        for _ in 0..65 {
            assert_eq!(
                packet_socketpair_after_parse(&actor, &net_namespace, SOCK_RAW, 0, false, spec),
                Err(AxError::PermissionDenied)
            );
        }
        probe.assert_complete_cycles(65);
    }

    #[test]
    fn packet_socketpair_keeps_generic_and_capability_error_precedence() {
        assert_eq!(parse_socket_type(0x7f), Err(AxError::InvalidInput));
        assert_eq!(
            validate_packet_create_after_capability(false, SOCK_RAW, 0),
            Err(LinuxError::EPERM.into())
        );
        assert_eq!(
            validate_packet_create_after_capability(false, SOCK_STREAM, 0),
            Err(LinuxError::EPERM.into())
        );
        assert_eq!(
            validate_packet_create_after_capability(true, SOCK_STREAM, 0),
            Err(LinuxError::ESOCKTNOSUPPORT.into())
        );
    }
}
