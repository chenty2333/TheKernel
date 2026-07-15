use alloc::sync::Arc;
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodePermission;
#[cfg(feature = "vsock")]
use axnet::vsock::{VsockSocket, VsockStreamTransport};
use axnet::{
    MAX_LISTEN_BACKLOG, Shutdown, Socket as SocketInner, SocketAddrEx, SocketOps,
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixSocket, UnixSocketAddr},
};
use linux_raw_sys::{
    general::{CAP_NET_BIND_SERVICE, O_CLOEXEC, O_NONBLOCK, O_RDWR},
    net::{
        AF_INET, AF_INET6, AF_MAX, AF_NETLINK, AF_PACKET, AF_UNIX, AF_UNSPEC, AF_VSOCK,
        IPPROTO_TCP, IPPROTO_UDP, SHUT_RD, SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW,
        SOCK_SEQPACKET, SOCK_STREAM, sockaddr, socklen_t,
    },
};
use starry_vm::{VmMutPtr, VmPtr, vm_write_slice};

use super::{SocketSyscallSnapshot, addr::SocketAddrExt};
use crate::{
    file::{
        AcceptedSocketSecurityRef, AfAlgSocket, FileDescription, FileLike, NetlinkSocket,
        PendingSocketSecurityRef, PinnedSocketDescription, PreparedSocketAddress, Socket,
        SocketBackendKind, af_alg, close_file_like, permission::VfsSecurityContext, reserve_fd,
    },
    mm::{UserConstPtr, UserPtr},
    task::{
        NetworkNamespace, ns_capable,
        security::{SocketCreateSpec, SocketListenBacklog, SocketSecurityContext, dispatch_socket},
    },
};

const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
const SOCK_DCCP: u32 = 6;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_CLOEXEC_NONBLOCK_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;

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

    if ip_addr.port() != 0 && ip_addr.port() < FIRST_UNPRIVILEGED_PORT {
        if !ns_capable(actor, net_ns.owner_user_ns(), CAP_NET_BIND_SERVICE) {
            return Err(AxError::from(LinuxError::EACCES));
        }
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

fn validate_pre_create_domain(domain: u32) -> AxResult<()> {
    if domain == AF_PACKET || domain >= AF_MAX {
        Err(LinuxError::EAFNOSUPPORT.into())
    } else {
        Ok(())
    }
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
    dispatch_socket(&SocketSecurityContext::create(&actor, spec))?;

    if domain == af_alg::AF_ALG {
        AfAlgSocket::validate_socket_type(ty, proto)?;
        let socket = prepare_new_socket_like(AfAlgSocket::new_listener(), nonblocking)?;
        dispatch_socket_post_create(&actor, &socket, spec)?;
        return publish_new_socket_like(socket, cloexec).map(|fd| fd as isize);
    }

    let net_ns = snapshot.net_namespace().clone();

    if domain == AF_NETLINK {
        NetlinkSocket::validate_socket_type(ty, proto)?;
        let socket = NetlinkSocket::try_new(proto, net_ns)?;
        let socket = prepare_new_socket_arc(socket, nonblocking)?;
        dispatch_socket_post_create(&actor, &socket, spec)?;
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
            SocketInner::Udp(UdpSocket::new(net_stack.clone())?)
        }
        (AF_INET | AF_INET6, SOCK_DCCP) => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        (AF_INET | AF_INET6, SOCK_RAW) => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        (AF_UNIX, SOCK_STREAM) => SocketInner::Unix(UnixSocket::new(
            StreamTransport::new()?,
            net_stack.unix_namespace(),
        )),
        (AF_UNIX, SOCK_DGRAM) => SocketInner::Unix(UnixSocket::new(
            DgramTransport::new()?,
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
    let socket = Socket::new(socket, net_ns);
    let socket = prepare_new_socket_like(socket, nonblocking)?;
    dispatch_socket_post_create(&actor, &socket, spec)?;
    publish_new_socket_like(socket, cloexec).map(|fd| fd as isize)
}

pub fn sys_bind(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    match pinned.backend()? {
        SocketBackendKind::AfAlg => {
            let addr = af_alg::SockAddrAlg::read_from_user(addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, af_alg: {addr:?}");
            let prepared = PreparedSocketAddress::AfAlg(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                &actor,
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
                (addr.address().as_usize() as *const crate::file::netlink::SockaddrNl)
                    .vm_read_uninit()?
                    .assume_init()
            };
            if addr.nl_family as u32 != AF_NETLINK {
                return Err(AxError::from(LinuxError::EAFNOSUPPORT));
            }
            let port_id = if addr.nl_pid == 0 {
                snapshot.pid()
            } else {
                addr.nl_pid
            };
            let prepared = PreparedSocketAddress::Netlink(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                &actor,
                &socket_ref,
                &prepared,
                addrlen as usize,
            ))?;
            pinned.netlink()?.bind(port_id, addr.nl_groups)?;
        }
        SocketBackendKind::Network => {
            let socket = pinned.network()?;
            let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, addr: {addr:?}");
            let prepared = PreparedSocketAddress::Network(addr);
            dispatch_socket(&SocketSecurityContext::bind(
                &actor,
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
                )?;
            } else {
                require_bind_permissions(&addr, socket.net_namespace(), actor)?;
                socket.bind(addr)?;
            }
        }
    }

    Ok(0)
}

pub fn sys_connect(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    // Pin the open file description once. Address decoding intentionally
    // remains before the ENOTSOCK downcast for the ordinary connect path, but
    // a sibling sharing the fd table can no longer redirect the operation by
    // closing and reusing the numeric descriptor between those two steps.
    let pinned = PinnedSocketDescription::pin_fd(fd)?;

    if addrlen as usize >= size_of::<linux_raw_sys::net::__kernel_sa_family_t>()
        && super::addr::read_family(addr, addrlen)? as u32 == AF_UNSPEC
    {
        debug!("sys_connect <= fd: {fd}, addr: AF_UNSPEC");
        let socket = pinned.network()?;
        let actor = snapshot.actor();
        let socket_ref = pinned.security_ref()?;
        let prepared = PreparedSocketAddress::Unspecified;
        dispatch_socket(&SocketSecurityContext::connect(
            &actor,
            &socket_ref,
            &prepared,
            addrlen as usize,
        ))?;
        socket.disconnect()?;
        return Ok(0);
    }

    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_connect <= fd: {fd}, addr: {addr:?}");

    let socket = pinned.network()?;
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    let prepared = PreparedSocketAddress::Network(addr);
    dispatch_socket(&SocketSecurityContext::connect(
        &actor,
        &socket_ref,
        &prepared,
        addrlen as usize,
    ))?;
    let PreparedSocketAddress::Network(addr) = prepared else {
        unreachable!();
    };
    let result = match (&socket.inner, &addr) {
        (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
            let security = VfsSecurityContext::new(actor.clone());
            let target = crate::file::unix_socket::resolve_peer(path.clone(), &security)?;
            if unix.is_datagram() {
                unix.connect_resolved_as(target, snapshot.unix_credentials())
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
                    &actor,
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
                    &actor,
                    &socket_ref,
                    &listening,
                    &accepted,
                ))?;
                reservation.commit()
            }
        }
        _ => socket.connect(addr.clone()),
    };
    result.map_err(|e| {
        if e == AxError::WouldBlock {
            AxError::InProgress
        } else {
            e
        }
    })?;

    Ok(0)
}

pub fn sys_listen(fd: i32, backlog: i32) -> AxResult<isize> {
    debug!("sys_listen <= fd: {fd}, backlog: {backlog}");

    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket = pinned.network()?;
    // Linux compares `(unsigned int)backlog` with somaxconn. Negative values
    // therefore clamp to the namespace maximum rather than to zero.
    let backlog = clamp_listen_backlog(backlog);
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    let prepared_backlog =
        SocketListenBacklog::try_from_clamped(backlog as i32).ok_or(AxError::InvalidInput)?;
    dispatch_socket(&SocketSecurityContext::listen(
        &actor,
        &socket_ref,
        prepared_backlog,
    ))?;
    if let SocketInner::Unix(unix) = &socket.inner {
        unix.listen_as(backlog, snapshot.unix_credentials())?;
    } else {
        socket.listen(backlog)?;
    }

    Ok(0)
}

pub fn sys_accept(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    sys_accept4(fd, addr, addrlen, 0)
}

pub fn sys_accept4(
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
    if pinned.backend()? == SocketBackendKind::AfAlg {
        let request = pinned.af_alg()?.accept_request()?;
        let request = prepare_new_socket_like(request, nonblocking)?;
        let accepted_ref = request.security_ref()?;
        let accepted_ref = AcceptedSocketSecurityRef::Description(accepted_ref);
        dispatch_socket(&SocketSecurityContext::accept(
            &actor,
            &listening_ref,
            &accepted_ref,
        ))?;
        if !addr.is_null() {
            (addrlen.address().as_usize() as *mut socklen_t).vm_write(0)?;
        }
        return publish_new_socket_like(request, cloexec).map(|fd| fd as isize);
    }

    let socket = pinned.network()?;
    let net_ns = socket.net_namespace().clone();
    let reservation = socket.prepare_accept()?;
    let pending_ref = PendingSocketSecurityRef::new(&reservation, &net_ns);
    let accepted_ref = AcceptedSocketSecurityRef::Pending(pending_ref);
    dispatch_socket(&SocketSecurityContext::accept(
        &actor,
        &listening_ref,
        &accepted_ref,
    ))?;
    let socket = Socket::new(reservation.commit()?, net_ns);

    let remote_addr = socket.peer_addr()?;
    if !addr.is_null() {
        let addrlen_ptr = addrlen.address().as_usize() as *mut socklen_t;
        let mut value = addrlen_ptr.vm_read()?;
        remote_addr.write_to_user(addr, &mut value)?;
        addrlen_ptr.vm_write(value)?;
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
    let socket = pinned.network()?;
    let actor = snapshot.actor();
    let socket_ref = pinned.security_ref()?;
    dispatch_socket(&SocketSecurityContext::shutdown(
        &actor,
        &socket_ref,
        how as i32,
    ))?;
    let how = match how {
        SHUT_RD => Shutdown::Read,
        SHUT_WR => Shutdown::Write,
        SHUT_RDWR => Shutdown::Both,
        _ => return Err(AxError::InvalidInput),
    };
    socket.shutdown(how).map(|_| 0)
}

pub fn sys_socketpair(
    domain: u32,
    raw_ty: u32,
    proto: u32,
    fds: UserPtr<[i32; 2]>,
) -> AxResult<isize> {
    debug!("sys_socketpair <= domain: {domain}, ty: {raw_ty}, proto: {proto}");
    let (ty, nonblocking, cloexec) = parse_socket_type(raw_ty)?;

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
    dispatch_socket(&SocketSecurityContext::create(&actor, spec))?;
    dispatch_socket(&SocketSecurityContext::create(&actor, spec))?;

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
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
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
    dispatch_socket_post_create(&actor, &socket1, spec)?;
    dispatch_socket_post_create(&actor, &socket2, spec)?;
    {
        let socket1_ref = socket1.security_ref()?;
        let socket2_ref = socket2.security_ref()?;
        dispatch_socket(&SocketSecurityContext::pair(
            &actor,
            &socket1_ref,
            &socket2_ref,
        ))?;
    }

    // No descriptor number or userspace output exists before every create,
    // post-create, and pair hook has admitted both private endpoints.
    let reserved1 = reserve_fd(cloexec)?;
    let reserved2 = reserve_fd(cloexec)?;
    let fd_pair = [reserved1.fd(), reserved2.fd()];
    vm_write_slice(fds.address().as_usize() as *mut i32, &fd_pair)?;

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

    #[test]
    fn socket_creation_flags_keep_read_write_and_nonblocking_on_the_ofd() {
        assert_eq!(socket_status_flags(false), O_RDWR);
        assert_eq!(socket_status_flags(true), O_RDWR | O_NONBLOCK);
    }

    #[test]
    fn unsupported_packet_and_out_of_range_families_fail_before_create_policy() {
        assert_eq!(
            validate_pre_create_domain(AF_PACKET),
            Err(LinuxError::EAFNOSUPPORT.into())
        );
        assert_eq!(
            validate_pre_create_domain(AF_MAX),
            Err(LinuxError::EAFNOSUPPORT.into())
        );
        assert!(validate_pre_create_domain(AF_INET).is_ok());
    }
}
