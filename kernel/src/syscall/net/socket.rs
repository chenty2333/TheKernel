use alloc::sync::Arc;
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodePermission;
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
use tk_linux_net::{SocketFailure, socket_failure_errno};

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
            LANDLOCK_ACCESS_NET_BIND_TCP, LANDLOCK_ACCESS_NET_BIND_UDP,
            LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP, LANDLOCK_ACCESS_NET_CONNECT_TCP,
            SocketCreateSpec, SocketListenBacklog, SocketSecurityContext,
            check_current_landlock_net_port, dispatch_socket,
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
    let socket = PinnedSocketDescription::from_description(description)?;
    register_socket_endpoint_owner(&socket)?;
    Ok(socket)
}

fn register_socket_endpoint_owner(socket: &PinnedSocketDescription) -> AxResult<()> {
    if let Ok(network) = socket.network()
        && let SocketInner::Unix(unix) = &network.inner
        && let Some(endpoint) = unix.connected_endpoint_identity()
    {
        super::cmsg::register_unix_endpoint_owner(endpoint.raw(), socket.description())?;
    }
    Ok(())
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

/// Linux `hook_socket_bind()`: `LANDLOCK_ACCESS_NET_BIND_TCP` for TCP and
/// `LANDLOCK_ACCESS_NET_BIND_UDP` for UDP.  Other families are unhandled and
/// therefore unrestricted.  Port 0 is an ordinary rule key here: a
/// `LANDLOCK_ACCESS_NET_BIND_UDP` rule on port 0 is exactly what admits
/// `bind(2)` on an ephemeral port (`net_test.c`: `bind_ephemeral`).
fn check_landlock_bind_port(socket: &SocketInner, addr: &SocketAddrEx) -> AxResult<()> {
    let access = match socket {
        SocketInner::Tcp(_) => LANDLOCK_ACCESS_NET_BIND_TCP,
        SocketInner::Udp(_) => LANDLOCK_ACCESS_NET_BIND_UDP,
        _ => return Ok(()),
    };
    if let SocketAddrEx::Ip(ip_addr) = addr {
        check_current_landlock_net_port(ip_addr.port(), access)?;
    }
    Ok(())
}

/// Linux `hook_socket_connect()`: `LANDLOCK_ACCESS_NET_CONNECT_TCP` for TCP and
/// `LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP` for UDP, followed for UDP by
/// `current_check_autobind_udp_socket()`.
fn check_landlock_connect_port(socket: &SocketInner, addr: &SocketAddrEx) -> AxResult<()> {
    let access = match socket {
        SocketInner::Tcp(_) => LANDLOCK_ACCESS_NET_CONNECT_TCP,
        SocketInner::Udp(_) => LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP,
        _ => return Ok(()),
    };
    if let SocketAddrEx::Ip(ip_addr) = addr {
        check_current_landlock_net_port(ip_addr.port(), access)?;
    }
    if let SocketInner::Udp(udp) = socket {
        check_landlock_udp_autobind(udp)?;
    }
    Ok(())
}

/// Linux `hook_socket_sendmsg()`: a datagram sent to an explicit destination
/// consumes `LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP`, and every datagram send
/// may auto-bind an ephemeral local port.
pub(super) fn check_landlock_sendmsg(
    socket: &SocketInner,
    address: Option<&SocketAddrEx>,
) -> AxResult<()> {
    let SocketInner::Udp(udp) = socket else {
        return Ok(());
    };
    if let Some(SocketAddrEx::Ip(ip_addr)) = address {
        check_current_landlock_net_port(ip_addr.port(), LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP)?;
    }
    check_landlock_udp_autobind(udp)
}

/// Linux `current_check_autobind_udp_socket()`: connecting or sending on an
/// unbound UDP socket auto-binds an ephemeral port, which must be admitted as
/// an explicit `bind(0)` would be.  A socket that already owns a local port
/// never triggers the check.
fn check_landlock_udp_autobind(udp: &axnet::udp::UdpSocket) -> AxResult<()> {
    if let SocketAddrEx::Ip(local) = udp.local_addr()?
        && local.port() != 0
    {
        return Ok(());
    }
    check_current_landlock_net_port(0, LANDLOCK_ACCESS_NET_BIND_UDP)
}

/// `__sys_socket_create`'s flag test followed by `type &= SOCK_TYPE_MASK`.
///
/// Linux deliberately keeps this separate from the type *range* test: the flag
/// test is the very first thing `socket(2)` does, while the range test lives in
/// `__sock_create` after the family test, so `socket(9999, 11, 0)` reports
/// `EAFNOSUPPORT` and not `EINVAL`.
fn parse_socket_type(raw_ty: u32) -> AxResult<(u32, bool, bool)> {
    let flags = raw_ty & !SOCK_TYPE_MASK;
    if flags & !SOCK_CLOEXEC_NONBLOCK_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    Ok((
        raw_ty & SOCK_TYPE_MASK,
        flags & O_NONBLOCK != 0,
        flags & O_CLOEXEC != 0,
    ))
}

/// `__sock_create`'s own range test.  It runs after the family range test and
/// before the obsolete `(PF_INET, SOCK_PACKET)` rewrite, and it admits every
/// masked value below `SOCK_MAX` — including zero, `tk_linux_net::SOCK_RDM` and
/// `SOCK_PACKET`.  Whether an in-range type is usable is the selected family's
/// `create` decision, which reports `ESOCKTNOSUPPORT` for the types it has no
/// protocol table entry for.
fn validate_socket_type_range(ty: u32) -> AxResult<u32> {
    if tk_linux_net::socket_type_in_range(ty) {
        Ok(ty)
    } else {
        Err(AxError::InvalidInput)
    }
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
    publish_new_socket_like(accepted, cloexec).map(|fd| fd as isize)
}

fn validate_pre_create_domain(domain: u32) -> AxResult<()> {
    // `NPROTO` is `AF_MAX`; `__sock_create` tests the family range before the
    // type range, so an out-of-range family outranks an out-of-range type.
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
    tk_linux_packet::PacketSocketType,
    tk_linux_packet::ProtocolSelector,
)> {
    if !capability_granted {
        return Err(LinuxError::EPERM.into());
    }
    let socket_type =
        tk_linux_packet::PacketSocketType::from_raw(ty as i32).map_err(packet_error)?;
    let protocol = tk_linux_packet::ProtocolSelector::from_network_order_i32(proto as i32);
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

/// `IPPROTO_MPTCP`, the last protocol below `IPPROTO_MAX`.
#[cfg(test)]
const IPPROTO_MPTCP_BOUNDARY: u32 = 262;

fn supported_stream_protocol(proto: u32) -> bool {
    proto == 0 || proto == IPPROTO_TCP as u32
}

fn supported_datagram_protocol(proto: u32) -> bool {
    proto == 0 || proto == IPPROTO_UDP as u32
}

/// `IPPROTO_MAX` (`include/uapi/linux/in.h`): the exclusive upper bound
/// `inet_create`/`inet6_create` place on the protocol argument.  The enum ends
/// with `IPPROTO_SMC = 256` and `IPPROTO_MPTCP = 262`, so the bound is 263 and
/// not 256 — `socket(AF_INET, SOCK_STREAM, 256)` is a *table lookup* that
/// reports `EPROTONOSUPPORT`, not the `EINVAL` of an out-of-range protocol.
const IPPROTO_MAX: u32 = 263;

/// The full admission decision `inet_create`/`inet6_create` reach for one
/// already range-checked request.
///
/// Linux applies them in this order, and `inet_create` reaches the capability
/// gate only after the protocol tables have answered:
///
/// 1. `if (protocol < 0 || protocol >= IPPROTO_MAX) return -EINVAL;`
/// 2. `list_empty(&inetsw[type])` is what leaves `err = -ESOCKTNOSUPPORT`;
/// 3. the wildcard lookup over `inetsw[type]` — a zero protocol selects the
///    list head's own protocol, and the `SOCK_RAW` list head is the
///    `IPPROTO_IP` wildcard entry, so every protocol below `IPPROTO_MAX`
///    matches it;
/// 4. `if (sock->type == SOCK_RAW && !kern && !ns_capable(net->user_ns,
///    CAP_NET_RAW)) return -EPERM;`.
///
/// TheKernel implements one transport per type, so a request that Linux would
/// satisfy from a table entry TheKernel has no provider for (ICMP or UDPLITE
/// datagrams, for example) still reports `EPROTONOSUPPORT` here.
fn validate_inet_create(capability_granted: bool, ty: u32, proto: u32) -> AxResult<()> {
    if proto >= IPPROTO_MAX {
        return Err(AxError::InvalidInput);
    }
    let admitted = match ty {
        SOCK_STREAM => supported_stream_protocol(proto),
        SOCK_DGRAM => supported_datagram_protocol(proto),
        SOCK_DCCP => proto == 0 || proto == IPPROTO_DCCP as u32,
        SOCK_SEQPACKET => proto == 0 || proto == IPPROTO_SCTP as u32,
        SOCK_RAW => {
            // The wildcard `inetsw[SOCK_RAW]` entry admits every in-range
            // protocol, so only the capability can refuse this type.
            if !capability_granted {
                return Err(LinuxError::EPERM.into());
            }
            return Ok(());
        }
        // `inetsw[type]` has no entry at all for these, which is the one case
        // `inet_create` reports as ESOCKTNOSUPPORT rather than EPROTONOSUPPORT.
        _ => return Err(LinuxError::ESOCKTNOSUPPORT.into()),
    };
    if admitted {
        Ok(())
    } else {
        Err(LinuxError::EPROTONOSUPPORT.into())
    }
}

pub fn sys_socket(domain: u32, raw_ty: u32, proto: u32) -> AxResult<isize> {
    debug!("sys_socket <= domain: {domain}, ty: {raw_ty}, proto: {proto}");
    // `__sys_socket_create` strips the creation flags first, then `__sock_create`
    // applies the family range test, the type range test and the obsolete
    // `(PF_INET, SOCK_PACKET)` rewrite — in that order and all before the LSM
    // hook.  Linux therefore reports EAFNOSUPPORT for a bad family even when the
    // type is also out of range, and EINVAL for a bad type only once the family
    // is known good.
    let (ty, nonblocking, cloexec) = parse_socket_type(raw_ty)?;
    validate_pre_create_domain(domain)?;
    validate_socket_type_range(ty)?;
    let domain = u32::from(tk_linux_net::socket_creation_family(domain as u16, ty));
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
        if !ns_capable(actor, snapshot.net_namespace().owner_user_ns(), CAP_NET_RAW) {
            return Err(AxError::OperationNotPermitted);
        }
        // `xsk_create` checks the capability before the type, so an
        // unprivileged caller sees EPERM whatever the type was.
        if ty != SOCK_RAW {
            return Err(LinuxError::ESOCKTNOSUPPORT.into());
        }
        if proto != 0 {
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
        let socket = NetlinkSocket::try_new(proto, ty, net_ns)?;
        let socket = prepare_new_socket_arc(socket, nonblocking)?;
        dispatch_socket_post_create(actor, &socket, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    let net_stack = net_ns.stack().clone();

    // `unix_create` rewrites its BSD-compatibility spelling to `SOCK_DGRAM`
    // before it selects a transport, so the raw type never reaches the provider
    // table.  The security hook above still saw the requested type, exactly as
    // `security_socket_create` does.
    let ty = if domain == AF_UNIX {
        if !tk_linux_net::unix_protocol_admitted(proto) {
            // `unix_create` checks the protocol before the type switch.
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        tk_linux_net::unix_creation_type(ty).ok_or(AxError::from(LinuxError::ESOCKTNOSUPPORT))?
    } else {
        ty
    };

    if matches!(domain, AF_INET | AF_INET6) {
        validate_inet_create(
            ns_capable(actor, net_ns.owner_user_ns(), CAP_NET_RAW),
            ty,
            proto,
        )?;
    }

    let socket = match (domain, ty) {
        (AF_INET | AF_INET6, SOCK_STREAM) => SocketInner::Tcp(TcpSocket::new(net_stack.clone())?),
        (AF_INET | AF_INET6, SOCK_DGRAM) => {
            let family = if domain == AF_INET6 {
                UdpSocketFamily::Ipv6
            } else {
                UdpSocketFamily::Ipv4
            };
            SocketInner::Udp(UdpSocket::new_with_family(net_stack.clone(), family)?)
        }
        (AF_INET | AF_INET6, SOCK_DCCP) => {
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
            let family = if domain == AF_INET6 {
                RawSocketFamily::Ipv6
            } else {
                RawSocketFamily::Ipv4
            };
            SocketInner::Sctp(SctpSocket::new(net_stack.clone(), family)?)
        }
        (AF_INET | AF_INET6, SOCK_RAW) => {
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
        (AF_INET, _) | (AF_INET6, _) | (AF_UNIX, _) | (AF_VSOCK, _) => {
            debug!("Unsupported socket type: domain: {domain}, ty: {ty}");
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
    // `__sys_bind` resolves the descriptor first and only then runs
    // `move_addr_to_kernel()` (`net/socket.c:1943-1948`), whose bound is
    // `if (ulen < 0 || ulen > sizeof(struct sockaddr_storage)) return -EINVAL;`
    // (`:249-250`).  Every family below therefore sees an address no longer
    // than `struct sockaddr_storage`.
    if !tk_linux_net::address_import_admitted(addrlen as i32) {
        return Err(AxError::InvalidInput);
    }
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
            // `netlink_bind` requires `addr_len >= sizeof(struct sockaddr_nl)`;
            // the upper bound is the `move_addr_to_kernel` test already applied
            // above, so a longer address is a legal prefix rather than an error.
            if (addrlen as usize) < size_of::<crate::file::netlink::SockaddrNl>() {
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
            // `if (nladdr->nl_family != AF_NETLINK) return -EINVAL;` — the
            // netlink family check reports EINVAL, not EAFNOSUPPORT.
            if addr.nl_family as u32 != AF_NETLINK {
                return Err(AxError::InvalidInput);
            }
            let prepared = PreparedSocketAddress::Netlink(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            let authority = super::netlink_option_authority(&snapshot, pinned.netlink()?);
            if addr.nl_pid == 0 {
                pinned
                    .netlink()?
                    .bind_auto(snapshot.pid(), addr.nl_groups, authority)?;
            } else {
                pinned
                    .netlink()?
                    .bind(addr.nl_pid, addr.nl_groups, authority)?;
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
            // `unix_bind()` autobinds a family-only AF_UNIX address before it
            // validates that address at all:
            //
            // ```c
            // 	if (addr_len == offsetof(struct sockaddr_un, sun_path) &&
            // 	    sunaddr->sun_family == AF_UNIX)
            // 		return unix_autobind(sk);
            // ```
            //
            // (`net/unix/af_unix.c:1463-1470`; `offsetof(sun_path)` is 2 on
            // x86_64, so `bind(fd, {AF_UNIX}, 2)` succeeds with an abstract
            // name).  Any other two-byte record falls through to
            // `unix_validate_addr()` and its `-EINVAL`.  The security hook has
            // already run in the generic bind path above; it sees the raw
            // family-only address, so no name is invented for it here.
            if let SocketInner::Unix(unix) = &socket.inner
                && addrlen as usize >= size_of::<linux_raw_sys::net::__kernel_sa_family_t>()
                && tk_linux_net::unix_autobind_request(
                    addrlen as usize,
                    super::addr::read_family(&capability, addr, addrlen)?,
                )
            {
                debug!("sys_bind <= fd: {fd}, autobind");
                let prepared = PreparedSocketAddress::Unspecified;
                dispatch_socket(&SocketSecurityContext::bind(
                    actor,
                    &socket_ref,
                    &prepared,
                    addrlen as usize,
                ))?;
                unix_autobind(unix, &pinned.description().clone())?;
                return Ok(0);
            }
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
                // Linux reads the domain from the socket file's `f_cred`,
                // which was captured when this socket was created, not from
                // the task that happens to call bind(2).
                let creator_domain = socket.creator_landlock_domain()?;
                crate::file::unix_socket::bind_path(
                    unix,
                    path.clone(),
                    &security,
                    creator_domain,
                    NodePermission::from_bits_truncate(0o777),
                    snapshot.umask(),
                    |endpoint| {
                        super::cmsg::prepare_unix_endpoint_owner(
                            endpoint.raw(),
                            pinned.description(),
                        )
                    },
                )?
                .commit();
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
                    |endpoint| super::cmsg::prepare_unix_endpoint_owner(endpoint.raw(), &owner),
                )?
                .commit();
            } else {
                require_bind_permissions(&addr, socket.net_namespace(), actor)?;
                validate_network_address(&socket.inner, &addr)?;
                check_landlock_bind_port(&socket.inner, &addr)?;
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

    if pinned.backend() == Ok(SocketBackendKind::Netlink) {
        let family = super::addr::read_family(&capability, addr, addrlen)? as u32;
        let address = if family == AF_UNSPEC {
            None
        } else {
            if family != AF_NETLINK
                || (addrlen as usize) < size_of::<crate::file::netlink::SockaddrNl>()
            {
                return Err(AxError::InvalidInput);
            }
            Some(unsafe {
                capability
                    .read_value_uninit(
                        addr.address().as_usize() as *const crate::file::netlink::SockaddrNl
                    )
                    .map_err(map_usercopy_error)?
                    .assume_init()
            })
        };
        let prepared = address.map_or(
            PreparedSocketAddress::Unspecified,
            PreparedSocketAddress::Netlink,
        );
        let socket_ref = pinned.security_ref()?;
        dispatch_socket(&SocketSecurityContext::connect(
            snapshot.actor(),
            &socket_ref,
            &prepared,
            addrlen as usize,
        ))?;
        pinned
            .netlink()?
            .connect(address, snapshot.actor(), snapshot.pid())?;
        return Ok(0);
    }

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

    if matches!(
        pinned.backend()?,
        SocketBackendKind::AfAlg | SocketBackendKind::Xdp
    ) {
        // `alg_proto_ops.connect` (`crypto/af_alg.c:480`) and
        // `xsk_proto_ops.connect` (`net/xdp/xsk.c:2140`) are both
        // `sock_no_connect`, which is `return -EOPNOTSUPP;`
        // (`net/core/sock.c:3536-3539`).  Linux's generic connect layer only
        // copies the bounded address (move_addr_to_kernel) and runs
        // `security_socket_connect()` first; the family's own address decode
        // never runs.
        let address = snapshot_address(&capability, addr, addrlen)?;
        let socket_ref = pinned.security_ref()?;
        let prepared = PreparedSocketAddress::Packet(address);
        dispatch_socket(&SocketSecurityContext::connect(
            snapshot.actor(),
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
    if matches!(socket.inner, SocketInner::Unix(_)) {
        // `unix_dgram_connect` and `unix_stream_connect` both run
        // `unix_validate_addr`, which rejects a name no longer than the
        // two-byte family and any family other than AF_UNIX.  Unlike
        // `unix_bind`, a two-byte AF_UNIX address is not an autobind request on
        // the connect path.  Linux orders this after security_socket_connect,
        // which has already run above.
        if !matches!(addr, SocketAddrEx::Unix(_))
            || (addrlen as usize) <= tk_linux_net::SOCKADDR_UN_PATH_OFFSET
        {
            return Err(AxError::InvalidInput);
        }
    }
    validate_network_address(&socket.inner, &addr)?;
    check_landlock_connect_port(&socket.inner, &addr)?;
    let result = match (&socket.inner, &addr) {
        (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
            let security = VfsSecurityContext::new(actor.clone());
            let target = crate::file::unix_socket::resolve_peer(
                path.clone(),
                &security,
                crate::file::unix_socket::UnixPeerKind::of(unix),
            )?;
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
                let owners = super::cmsg::register_unix_connection_owners(
                    reservation.listening_identity().raw(),
                    reservation.connecting_identity().raw(),
                    reservation.accepted_identity().raw(),
                    pinned.description(),
                )?;
                reservation.commit()?;
                owners.0.commit();
                owners.1.commit();
                Ok(())
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
                let owners = super::cmsg::register_unix_connection_owners(
                    reservation.listening_identity().raw(),
                    reservation.connecting_identity().raw(),
                    reservation.accepted_identity().raw(),
                    pinned.description(),
                )?;
                reservation.commit()?;
                owners.0.commit();
                owners.1.commit();
                Ok(())
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
                let owners = super::cmsg::register_unix_connection_owners(
                    reservation.listening_identity().raw(),
                    reservation.connecting_identity().raw(),
                    reservation.accepted_identity().raw(),
                    pinned.description(),
                )?;
                reservation.commit()?;
                owners.0.commit();
                owners.1.commit();
                Ok(())
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
                let owners = super::cmsg::register_unix_connection_owners(
                    reservation.listening_identity().raw(),
                    reservation.connecting_identity().raw(),
                    reservation.accepted_identity().raw(),
                    pinned.description(),
                )?;
                reservation.commit()?;
                owners.0.commit();
                owners.1.commit();
                Ok(())
            }
        }
        _ => socket.connect(addr.clone()),
    };
    result.map_err(|error| {
        if error == AxError::WouldBlock && !matches!(&socket.inner, SocketInner::Unix(_)) {
            AxError::InProgress
        } else {
            map_connect_error(&socket.inner, error)
        }
    })?;

    Ok(0)
}

/// `unix_autobind()` (`net/unix/af_unix.c:1287-1337`).
///
/// A family-only `bind` asks the socket to name itself: Linux builds the
/// six-byte abstract name `\0%05x` from a 20-bit order number seeded by
/// `get_random_u32()`, inserts it into the namespace, and retries with the next
/// number when the name is already taken.  Exhausting the whole space reports
/// `-ENOSPC`.  A socket that already has a name keeps it and reports success,
/// because the function returns the `mutex_lock_interruptible()` result zero
/// through `if (u->addr) goto out;`.
fn unix_autobind(unix: &UnixSocket, owner: &Arc<FileDescription>) -> AxResult<()> {
    if unix.is_bound() {
        return Ok(());
    }
    let mut ordernum = unix_autobind_seed() & tk_linux_net::UNIX_AUTOBIND_ORDERNUM_MASK;
    let lastnum = ordernum;
    loop {
        ordernum = (ordernum + 1) & tk_linux_net::UNIX_AUTOBIND_ORDERNUM_MASK;
        // `unix_autobind_name()` returns Linux's stored `sun_path` bytes, which
        // begin with the abstract marker.  This kernel's `UnixSocketAddr::
        // Abstract` payload is what follows that marker (the decode side splits
        // the same way), so the record `unix_getname()` exports carries exactly
        // one leading NUL and `addr->len` is `offsetof(sun_path) + 6 = 8`
        // (`net/unix/af_unix.c:1309-1318`).
        let address = UnixSocketAddr::Abstract(super::addr::try_arc_bytes(
            &tk_linux_net::unix_autobind_name(ordernum)[1..],
        )?);
        match unix.bind_abstract_with_publish(address, |endpoint| {
            super::cmsg::prepare_unix_endpoint_owner(endpoint.raw(), owner)
        }) {
            Ok(prepared) => {
                prepared.commit();
                return Ok(());
            }
            Err(AxError::AddrInUse) => {
                if ordernum == lastnum {
                    // Every name in the 20-bit space is taken.
                    return Err(LinuxError::ENOSPC.into());
                }
            }
            Err(error @ AxError::InvalidInput) => {
                // A concurrent bind won the race between the is_bound() check
                // and the reservation; Linux's `if (u->addr) goto out;` makes
                // the loser report success too.
                if unix.is_bound() {
                    return Ok(());
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

/// The `get_random_u32()` that seeds `unix_autobind()`'s order number
/// (`net/unix/af_unix.c:1313`).  The value is not a secret — it only picks the
/// first candidate name — so a not-yet-seeded entropy pool falls back to a
/// counter that keeps the retry loop making progress instead of failing a bind
/// that Linux would complete.
fn unix_autobind_seed() -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};

    static FALLBACK: AtomicU32 = AtomicU32::new(1);
    let mut bytes = [0_u8; size_of::<u32>()];
    if crate::random::fill_secure(&mut bytes).is_ok() {
        u32::from_ne_bytes(bytes)
    } else {
        FALLBACK.fetch_add(1, Ordering::Relaxed)
    }
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
    // AF_PACKET, AF_NETLINK, AF_ALG and AF_XDP all install a null listen
    // operation, which Linux answers with EOPNOTSUPP after the security hook.
    if matches!(
        pinned.backend()?,
        SocketBackendKind::Packet
            | SocketBackendKind::Netlink
            | SocketBackendKind::AfAlg
            | SocketBackendKind::Xdp
    ) {
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

    // `__sys_accept4` resolves the descriptor first (`net/socket.c:2082-2087`)
    // and only `__sys_accept4_file` tests `flags & ~(SOCK_CLOEXEC |
    // SOCK_NONBLOCK)` with EINVAL (`:2061-2062`), so `accept4(-1, …, 0x40)` is
    // EBADF rather than EINVAL.
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let (nonblocking, cloexec) = parse_accept4_flags(flags)?;
    let actor = snapshot.actor();
    let listening_ref = pinned.security_ref()?;
    if matches!(
        pinned.backend()?,
        SocketBackendKind::Packet | SocketBackendKind::Netlink | SocketBackendKind::Xdp
    ) {
        // AF_PACKET, AF_NETLINK and AF_XDP install a null accept operation, but
        // Linux invokes security_socket_accept() with an otherwise bare
        // `newsock` first.  Preserve that policy ordering without
        // allocating/subscribing an endpoint which can never be published.
        // AF_ALG is absent here because `alg_proto_ops.accept` is real.
        let bare_ref =
            BareAcceptedSocketSecurityRef::new(pinned.backend()?, listening_ref.net_namespace());
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
    if matches!(
        pinned.backend()?,
        SocketBackendKind::Packet
            | SocketBackendKind::Netlink
            | SocketBackendKind::AfAlg
            | SocketBackendKind::Xdp
    ) {
        // `sock_no_shutdown` returns EOPNOTSUPP without inspecting `how`, so an
        // invalid direction on one of these providers is EOPNOTSUPP rather than
        // the EINVAL an INET socket reports.
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
    // `__sys_socketpair` validates the type flags, then reserves both descriptor
    // numbers *and publishes them into `usockvec`* before it creates anything:
    //
    // ```c
    // 	fd1 = get_unused_fd_flags(flags);
    // 	fd2 = get_unused_fd_flags(flags);
    // 	err = put_user(fd1, &usockvec[0]);
    // 	err = put_user(fd2, &usockvec[1]);
    // 	err = sock_create(family, type, protocol, &sock1);
    // ```
    //
    // (`net/socket.c:1817-1864`).  An unknown family, an unsupported type, a
    // failed capability test, `sock_no_socketpair`, a rejected security hook or
    // a failed `sock_alloc_file()` therefore all leave the two reserved numbers
    // visible in the caller's vector and hand them back to the allocator at
    // `out:` (`:1896-1899`).  Only the type-flag test above, the two
    // `get_unused_fd_flags()` calls and the `put_user()` calls themselves can
    // fail before the vector is written.
    let (ty, nonblocking, cloexec) = parse_socket_type(raw_ty)?;
    let reserved1 = reserve_fd(cloexec)?;
    let reserved2 = reserve_fd(cloexec)?;
    let fd_pair = [reserved1.fd(), reserved2.fd()];
    capability
        .write_slice(fds.address().as_usize() as *mut i32, &fd_pair)
        .map_err(map_usercopy_error)?;

    // `__sock_create` repeats `__sys_socket_create`'s family and type range
    // tests and the obsolete `(PF_INET, SOCK_PACKET)` rewrite before either
    // family's `create` hook.
    validate_pre_create_domain(domain)?;
    validate_socket_type_range(ty)?;
    let domain = u32::from(tk_linux_net::socket_creation_family(domain as u16, ty));

    let snapshot = SocketSyscallSnapshot::capture();
    let spec = SocketCreateSpec::try_new(domain as i32, ty as i32, proto as i32, false)
        .ok_or(AxError::InvalidInput)?;
    let actor = snapshot.actor();

    if domain == AF_PACKET {
        let net_ns = snapshot.net_namespace();

        // `packet_create` still owns the CAP_NET_RAW test and the protocol
        // validation here, and the generic pair operation still answers
        // `EOPNOTSUPP`; both paths run with the descriptors of `usockvec`
        // already reserved and published, exactly as in Linux.
        return packet_socketpair_after_parse(actor, net_ns, ty, proto, nonblocking, spec);
    }

    // Every remaining family creates both endpoints through `__sock_create`
    // before `__sys_socketpair` consults `ops->socketpair`, so the family's own
    // creation errno always outranks the generic unsupported-operation answer
    // that `sock_no_socketpair` supplies.
    if matches!(domain, AF_INET | AF_INET6) {
        validate_inet_create(
            ns_capable(actor, snapshot.net_namespace().owner_user_ns(), CAP_NET_RAW),
            ty,
            proto,
        )?;
        // `inet_stream_ops`, `inet_dgram_ops`, `inet_dccp_ops` and
        // `inet_sctp_ops` all route `.socketpair` to `sock_no_socketpair`.
        return Err(AxError::from(LinuxError::EOPNOTSUPP));
    }

    if domain == af_xdp::AF_XDP {
        if !ns_capable(actor, snapshot.net_namespace().owner_user_ns(), CAP_NET_RAW) {
            return Err(AxError::OperationNotPermitted);
        }
        if ty != SOCK_RAW {
            return Err(LinuxError::ESOCKTNOSUPPORT.into());
        }
        if proto != 0 {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        return Err(AxError::from(LinuxError::EOPNOTSUPP));
    }

    if domain == af_alg::AF_ALG {
        AfAlgSocket::validate_socket_type(ty, proto)?;
        return Err(AxError::from(LinuxError::EOPNOTSUPP));
    }

    if domain == AF_NETLINK {
        NetlinkSocket::validate_socket_type(ty, proto)?;
        if proto == crate::file::netlink::NETLINK_AUDIT
            && !NetlinkSocket::audit_socket_creation_authorized(actor)
        {
            return Err(AxError::from(LinuxError::EPERM));
        }
        return Err(AxError::from(LinuxError::EOPNOTSUPP));
    }

    if domain != AF_UNIX {
        return Err(AxError::from(LinuxError::EAFNOSUPPORT));
    }

    // `unix_create` checks the protocol before its type switch, then rewrites
    // the BSD compatibility spelling `SOCK_RAW` to `SOCK_DGRAM` before it picks
    // a transport.  The security hook above still saw the requested type.
    if !tk_linux_net::unix_protocol_admitted(proto) {
        return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
    }
    let ty =
        tk_linux_net::unix_creation_type(ty).ok_or(AxError::from(LinuxError::ESOCKTNOSUPPORT))?;

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
        _ => {
            // `unix_create`'s switch has no arm for any other type.
            debug!("Unsupported socketpair type: {ty}");
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
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
    register_socket_endpoint_owner(&socket1)?;
    register_socket_endpoint_owner(&socket2)?;
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

    // The descriptors were reserved and published before any creation step, so
    // the only work left is `fd_install()`: publish the two private endpoints
    // into the numbers the caller already holds.
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

    /// `__sock_create` order: the family range test outranks the type range
    /// test, and a masked type at or above `SOCK_MAX` never reaches a family.
    #[test]
    fn creation_ranges_follow_family_then_type() {
        assert_eq!(
            validate_pre_create_domain(AF_MAX),
            Err(LinuxError::EAFNOSUPPORT.into())
        );
        assert_eq!(
            validate_socket_type_range(tk_linux_net::SOCK_MAX),
            Err(AxError::InvalidInput)
        );
        // Zero, `tk_linux_net::SOCK_RDM` and `SOCK_PACKET` are all in range; only a family's
        // own `create` hook may refuse them.
        for admitted in [
            0,
            tk_linux_net::SOCK_RDM,
            SOCK_SEQPACKET,
            tk_linux_net::SOCK_PACKET,
        ] {
            assert!(validate_socket_type_range(admitted).is_ok());
        }
    }

    /// `__sock_create` rewrites the obsolete `(PF_INET, SOCK_PACKET)` pair
    /// before the security hook, so the hook and the provider both see
    /// `PF_PACKET`.
    #[test]
    fn obsolete_pf_inet_sock_packet_becomes_af_packet() {
        assert_eq!(
            validate_socket_type_range(tk_linux_net::SOCK_PACKET).unwrap(),
            tk_linux_net::SOCK_PACKET
        );
        assert_eq!(
            tk_linux_net::socket_creation_family(AF_INET as u16, tk_linux_net::SOCK_PACKET),
            AF_PACKET as u16
        );
        // Every other pairing keeps its family, including the same type on a
        // different family.
        for (family, ty) in [
            (AF_INET6 as u16, tk_linux_net::SOCK_PACKET),
            (AF_INET as u16, SOCK_RAW),
            (AF_PACKET as u16, tk_linux_net::SOCK_PACKET),
        ] {
            assert_eq!(tk_linux_net::socket_creation_family(family, ty), family);
        }
    }

    /// `inet_create`'s decision tree, in its own order: the protocol range test
    /// is `EINVAL` before any table lookup, an empty `inetsw[type]` list is
    /// `ESOCKTNOSUPPORT`, a protocol that no `inetsw[type]` entry matches is
    /// `EPROTONOSUPPORT`, and the `SOCK_RAW` capability gate reports `EPERM`
    /// only after the wildcard entry has admitted the protocol.
    #[test]
    fn inet_creation_errno_precedence_matches_inet_create() {
        // Protocol range: `protocol >= IPPROTO_MAX` is EINVAL even for a type
        // whose table is empty and for a raw socket without CAP_NET_RAW.
        for ty in [SOCK_STREAM, SOCK_RAW, tk_linux_net::SOCK_RDM, 0] {
            assert_eq!(
                validate_inet_create(false, ty, IPPROTO_MAX),
                Err(AxError::InvalidInput)
            );
        }
        // Empty `inetsw[type]`: zero and `tk_linux_net::SOCK_RDM` have no table.
        for ty in [0, tk_linux_net::SOCK_RDM] {
            assert_eq!(
                validate_inet_create(true, ty, 0),
                Err(LinuxError::ESOCKTNOSUPPORT.into())
            );
        }
        // Protocol table misses.
        for (ty, proto) in [
            (SOCK_STREAM, IPPROTO_UDP as u32),
            (SOCK_DGRAM, IPPROTO_TCP as u32),
            (SOCK_DCCP, IPPROTO_TCP as u32),
            (SOCK_SEQPACKET, IPPROTO_TCP as u32),
        ] {
            assert_eq!(
                validate_inet_create(true, ty, proto),
                Err(LinuxError::EPROTONOSUPPORT.into())
            );
        }
        // The range bound is 263, so IPPROTO_SMC(256) and IPPROTO_MPTCP(262)
        // reach the table lookup: a reachable protocol with no TheKernel
        // provider is EPROTONOSUPPORT, never the EINVAL reserved for values at
        // or above the bound.
        for proto in [256, IPPROTO_MPTCP_BOUNDARY] {
            assert_eq!(
                validate_inet_create(true, SOCK_STREAM, proto),
                Err(LinuxError::EPROTONOSUPPORT.into())
            );
        }
        assert_eq!(
            validate_inet_create(true, SOCK_STREAM, IPPROTO_MAX),
            Err(AxError::InvalidInput)
        );
        // Admitted pairings, including the zero-protocol wildcard.
        for (ty, proto) in [
            (SOCK_STREAM, 0),
            (SOCK_STREAM, IPPROTO_TCP as u32),
            (SOCK_DGRAM, 0),
            (SOCK_DGRAM, IPPROTO_UDP as u32),
            (SOCK_DCCP, 0),
            (SOCK_DCCP, IPPROTO_DCCP as u32),
            (SOCK_SEQPACKET, 0),
            (SOCK_SEQPACKET, IPPROTO_SCTP as u32),
        ] {
            assert!(validate_inet_create(false, ty, proto).is_ok());
        }
        // The `inetsw[SOCK_RAW]` wildcard entry matches every in-range
        // protocol, so only the capability can refuse the type.
        for proto in [0, IPPROTO_TCP as u32, 255] {
            assert_eq!(
                validate_inet_create(false, SOCK_RAW, proto),
                Err(LinuxError::EPERM.into())
            );
            assert!(validate_inet_create(true, SOCK_RAW, proto).is_ok());
        }
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
