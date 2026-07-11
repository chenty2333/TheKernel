use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::NodePermission;
#[cfg(feature = "vsock")]
use axnet::vsock::{VsockSocket, VsockStreamTransport};
use axnet::{
    Shutdown, Socket as SocketInner, SocketAddrEx, SocketOps,
    options::UnixCredentials,
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixSocket, UnixSocketAddr},
};
use axtask::current;
use linux_raw_sys::{
    general::{O_CLOEXEC, O_NONBLOCK},
    net::{
        AF_INET, AF_INET6, AF_NETLINK, AF_PACKET, AF_UNIX, AF_UNSPEC, AF_VSOCK, IPPROTO_TCP,
        IPPROTO_UDP, SHUT_RD, SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET,
        SOCK_STREAM, sockaddr, socklen_t,
    },
};

use super::addr::SocketAddrExt;
use crate::{
    file::{
        AfAlgSocket, FileLike, NetlinkSocket, Socket, add_file_like, af_alg, close_file_like,
        get_file_like,
    },
    mm::{UserConstPtr, UserPtr},
    task::AsThread,
};

const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
const SOCK_DCCP: u32 = 6;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_CLOEXEC_NONBLOCK_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;

fn current_unix_credentials() -> UnixCredentials {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    UnixCredentials::new(proc_data.proc.pid(), proc_data.euid(), proc_data.egid())
}

fn require_bind_permissions(addr: &SocketAddrEx) -> AxResult<()> {
    let SocketAddrEx::Ip(ip_addr) = addr else {
        return Ok(());
    };

    if ip_addr.port() != 0
        && ip_addr.port() < FIRST_UNPRIVILEGED_PORT
        && current().as_thread().proc_data.euid() != 0
    {
        return Err(AxError::from(LinuxError::EACCES));
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

    if domain == af_alg::AF_ALG {
        AfAlgSocket::validate_socket_type(ty, proto)?;
        let socket = AfAlgSocket::new_listener();
        if nonblocking {
            socket.set_nonblocking(true)?;
        }
        return socket.add_to_fd_table(cloexec).map(|fd| fd as isize);
    }

    if domain == AF_PACKET {
        return Err(LinuxError::EAFNOSUPPORT.into());
    }

    let net_ns = current().as_thread().proc_data.net_ns.clone();

    if domain == AF_NETLINK {
        NetlinkSocket::validate_socket_type(ty, proto)?;
        let socket = NetlinkSocket::new(proto, net_ns);
        if nonblocking {
            socket.set_nonblocking(true)?;
        }
        return add_file_like(socket as _, cloexec).map(|fd| fd as isize);
    }

    let socket = match (domain, ty) {
        (AF_INET | AF_INET6, SOCK_STREAM) => {
            if !supported_stream_protocol(proto) {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            SocketInner::Tcp(TcpSocket::new(net_ns.clone())?)
        }
        (AF_INET | AF_INET6, SOCK_DGRAM) => {
            if !supported_datagram_protocol(proto) {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            SocketInner::Udp(UdpSocket::new(net_ns.clone())?)
        }
        (AF_INET | AF_INET6, SOCK_DCCP) => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        (AF_INET | AF_INET6, SOCK_RAW) => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        (AF_UNIX, SOCK_STREAM) => SocketInner::Unix(UnixSocket::new(
            StreamTransport::new()?,
            net_ns.unix_namespace(),
        )),
        (AF_UNIX, SOCK_DGRAM) => SocketInner::Unix(UnixSocket::new(
            DgramTransport::new()?,
            net_ns.unix_namespace(),
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

    if nonblocking {
        socket.set_nonblocking(true)?;
    }

    socket.add_to_fd_table(cloexec).map(|fd| fd as isize)
}

pub fn sys_bind(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        let addr = af_alg::SockAddrAlg::read_from_user(addr, addrlen)?;
        debug!("sys_bind <= fd: {fd}, af_alg: {addr:?}");
        socket.bind(addr)?;
        return Ok(0);
    }

    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        if addrlen as usize != size_of::<crate::file::netlink::SockaddrNl>() {
            return Err(AxError::InvalidInput);
        }
        let addr = *addr
            .cast::<crate::file::netlink::SockaddrNl>()
            .get_as_ref()?;
        if addr.nl_family as u32 != AF_NETLINK {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        let port_id = if addr.nl_pid == 0 {
            current().as_thread().proc_data.proc.pid() as u32
        } else {
            addr.nl_pid
        };
        socket.bind(port_id, addr.nl_groups)?;
        return Ok(0);
    }

    let socket = Socket::from_fd(fd)?;
    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_bind <= fd: {fd}, addr: {addr:?}");

    if let (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) =
        (&socket.inner, &addr)
    {
        let curr = current();
        let proc_data = &curr.as_thread().proc_data;
        let credentials = proc_data.fs_dac_credentials();
        crate::file::unix_socket::bind_path(
            unix,
            path.clone(),
            &credentials,
            NodePermission::from_bits_truncate(0o777),
            proc_data.umask(),
        )?;
    } else {
        require_bind_permissions(&addr)?;
        socket.bind(addr)?;
    }

    Ok(0)
}

pub fn sys_connect(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    // Pin the open file description once. Address decoding intentionally
    // remains before the ENOTSOCK downcast for the ordinary connect path, but
    // a sibling sharing the fd table can no longer redirect the operation by
    // closing and reusing the numeric descriptor between those two steps.
    let file = get_file_like(fd)?;

    if addrlen as usize >= size_of::<linux_raw_sys::net::__kernel_sa_family_t>()
        && super::addr::read_family(addr, addrlen)? as u32 == AF_UNSPEC
    {
        debug!("sys_connect <= fd: {fd}, addr: AF_UNSPEC");
        Socket::from_file_handle(&file)?.disconnect()?;
        return Ok(0);
    }

    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_connect <= fd: {fd}, addr: {addr:?}");

    let socket = Socket::from_file_handle(&file)?;
    let result = match (&socket.inner, &addr) {
        (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
            let curr = current();
            let credentials = curr.as_thread().proc_data.fs_dac_credentials();
            let target = crate::file::unix_socket::resolve_peer(path.clone(), &credentials)?;
            unix.connect_resolved_as(target, current_unix_credentials())
        }
        (SocketInner::Unix(unix), SocketAddrEx::Unix(_)) => {
            unix.connect_as(addr.clone(), current_unix_credentials())
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

    // Linux treats a negative backlog as zero. The transport applies its own
    // finite queue cap, analogous to net.core.somaxconn.
    let socket = Socket::from_fd(fd)?;
    let backlog = backlog.max(0) as usize;
    if let SocketInner::Unix(unix) = &socket.inner {
        unix.listen_as(backlog, current_unix_credentials())?;
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

    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        let request = socket.accept_request()?;
        if nonblocking {
            request.set_nonblocking(true)?;
        }
        if !addr.is_null() {
            *addrlen.get_as_mut()? = 0;
        }
        return request.add_to_fd_table(cloexec).map(|fd| fd as isize);
    }

    let socket = Socket::from_fd(fd)?;
    let net_stack = socket.net_stack().clone();
    let socket = Socket::new(socket.accept()?, net_stack);
    if nonblocking {
        socket.set_nonblocking(true)?;
    }

    let remote_addr = socket.peer_addr()?;
    if !addr.is_null() {
        remote_addr.write_to_user(addr, addrlen.get_as_mut()?)?;
    }

    let fd = socket.add_to_fd_table(cloexec).map(|fd| fd as isize)?;
    debug!("sys_accept => fd: {fd}, addr: {remote_addr:?}");

    Ok(fd)
}

pub fn sys_shutdown(fd: i32, how: u32) -> AxResult<isize> {
    debug!("sys_shutdown <= fd: {fd}, how: {how:?}");

    let socket = Socket::from_fd(fd)?;
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
    let fds = fds.get_as_mut()?;

    if matches!(domain, AF_INET | AF_INET6) {
        return Err(inet_socketpair_error(ty, proto));
    }

    if domain != AF_UNIX {
        return Err(AxError::from(LinuxError::EAFNOSUPPORT));
    }

    let credentials = current_unix_credentials();
    let net_stack = current().as_thread().proc_data.net_ns.clone();
    let unix_namespace = net_stack.unix_namespace();
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
    let sock1 = Socket::new(SocketInner::Unix(sock1), net_stack.clone());
    let sock2 = Socket::new(SocketInner::Unix(sock2), net_stack);

    if nonblocking {
        sock1.set_nonblocking(true)?;
        sock2.set_nonblocking(true)?;
    }

    let fd1 = sock1.add_to_fd_table(cloexec)?;
    let fd2 = match sock2.add_to_fd_table(cloexec) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = close_file_like(fd1);
            return Err(err);
        }
    };
    *fds = [fd1, fd2];
    Ok(0)
}
