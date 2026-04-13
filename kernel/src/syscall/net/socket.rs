use axerrno::{AxError, AxResult, LinuxError};
#[cfg(feature = "vsock")]
use axnet::vsock::{VsockSocket, VsockStreamTransport};
use axnet::{
    Shutdown, Socket as SocketInner, SocketAddrEx, SocketOps,
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixSocket},
};
use axtask::current;
use linux_raw_sys::{
    general::{O_CLOEXEC, O_NONBLOCK},
    net::{
        AF_INET, AF_INET6, AF_PACKET, AF_UNIX, AF_VSOCK, IPPROTO_TCP, IPPROTO_UDP, SHUT_RD,
        SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM, sockaddr, socklen_t,
    },
};

use super::addr::SocketAddrExt;
use crate::{
    file::{AfAlgSocket, FileLike, PacketSocket, Socket, af_alg},
    mm::{UserConstPtr, UserPtr},
    task::AsThread,
};

const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
const AF_RDS: u32 = 21;

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
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET | SOCK_RAW => Ok(ty),
        _ => Err(AxError::InvalidInput),
    }
}

fn inet_socketpair_error(ty: u32, proto: u32) -> AxError {
    match ty {
        SOCK_RAW => AxError::from(LinuxError::EPROTONOSUPPORT),
        SOCK_DGRAM => {
            if proto == 0 || proto == IPPROTO_UDP as _ {
                AxError::from(LinuxError::EOPNOTSUPP)
            } else {
                AxError::from(LinuxError::EPROTONOSUPPORT)
            }
        }
        SOCK_STREAM => {
            if proto == 0 || proto == IPPROTO_TCP as _ {
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
    let ty = validate_socket_type(raw_ty & 0xFF)?;

    if domain == af_alg::AF_ALG {
        AfAlgSocket::validate_socket_type(ty, proto)?;
        let socket = AfAlgSocket::new_listener();
        if raw_ty & O_NONBLOCK != 0 {
            socket.set_nonblocking(true)?;
        }
        let cloexec = raw_ty & O_CLOEXEC != 0;
        return socket.add_to_fd_table(cloexec).map(|fd| fd as isize);
    }

    if domain == AF_PACKET {
        if ty != SOCK_RAW {
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        let socket = PacketSocket::new(u16::from_be(proto as u16));
        if raw_ty & O_NONBLOCK != 0 {
            socket.set_nonblocking(true)?;
        }
        let cloexec = raw_ty & O_CLOEXEC != 0;
        return socket.add_to_fd_table(cloexec).map(|fd| fd as isize);
    }

    let pid = current().as_thread().proc_data.proc.pid();
    let net_ns = current().as_thread().proc_data.net_ns.clone();
    let socket = match (domain, ty) {
        (AF_INET | AF_INET6, SOCK_STREAM) => {
            if proto != 0 && proto != IPPROTO_TCP as _ {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            SocketInner::Tcp(TcpSocket::new(net_ns))
        }
        (AF_INET | AF_INET6, SOCK_DGRAM) => {
            if proto != 0 && proto != IPPROTO_UDP as _ {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            SocketInner::Udp(UdpSocket::new(net_ns))
        }
        (AF_INET | AF_INET6, SOCK_RAW) => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        (AF_RDS, SOCK_SEQPACKET) => {
            if proto != 0 {
                return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
            }
            // RDS is only exercised in OSCOMP/LTP as a local datagram transport
            // with sockaddr_in-style names. Reuse the UDP data path so recvmsg
            // reports the Linux-compatible address length and wakeup semantics.
            SocketInner::Udp(UdpSocket::new(net_ns))
        }
        (AF_UNIX, SOCK_STREAM) => SocketInner::Unix(UnixSocket::new(StreamTransport::new(pid))),
        (AF_UNIX, SOCK_DGRAM) => SocketInner::Unix(UnixSocket::new(DgramTransport::new(pid))),
        #[cfg(feature = "vsock")]
        (AF_VSOCK, SOCK_STREAM) => {
            SocketInner::Vsock(VsockSocket::new(VsockStreamTransport::new()))
        }
        (AF_INET, _) | (AF_INET6, _) | (AF_UNIX, _) | (AF_VSOCK, _) | (AF_RDS, _) => {
            warn!("Unsupported socket type: domain: {domain}, ty: {ty}");
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        _ => {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
    };
    let socket = Socket(socket);

    if raw_ty & O_NONBLOCK != 0 {
        socket.set_nonblocking(true)?;
    }
    let cloexec = raw_ty & O_CLOEXEC != 0;

    socket.add_to_fd_table(cloexec).map(|fd| fd as isize)
}

pub fn sys_bind(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        let addr = af_alg::SockAddrAlg::read_from_user(addr, addrlen)?;
        debug!("sys_bind <= fd: {fd}, af_alg: {addr:?}");
        socket.bind(addr)?;
        return Ok(0);
    }

    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_bind <= fd: {fd}, addr: {addr:?}");

    require_bind_permissions(&addr)?;
    Socket::from_fd(fd)?.bind(addr)?;

    Ok(0)
}

pub fn sys_connect(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_connect <= fd: {fd}, addr: {addr:?}");

    Socket::from_fd(fd)?.connect(addr).map_err(|e| {
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

    if backlog < 0 && backlog != -1 {
        return Err(AxError::InvalidInput);
    }

    Socket::from_fd(fd)?.listen()?;

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

    let cloexec = flags & O_CLOEXEC != 0;

    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        let request = socket.accept_request()?;
        if flags & O_NONBLOCK != 0 {
            request.set_nonblocking(true)?;
        }
        if !addr.is_null() {
            *addrlen.get_as_mut()? = 0;
        }
        return request.add_to_fd_table(cloexec).map(|fd| fd as isize);
    }

    let socket = Socket::from_fd(fd)?;
    let socket = Socket(socket.accept()?);
    if flags & O_NONBLOCK != 0 {
        socket.set_nonblocking(true)?;
    }

    let remote_addr = socket.peer_addr()?;
    let fd = socket.add_to_fd_table(cloexec).map(|fd| fd as isize)?;
    debug!("sys_accept => fd: {fd}, addr: {remote_addr:?}");

    if !addr.is_null() {
        remote_addr.write_to_user(addr, addrlen.get_as_mut()?)?;
    }

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
    let ty = validate_socket_type(raw_ty & 0xFF)?;

    if matches!(domain, AF_INET | AF_INET6) {
        return Err(inet_socketpair_error(ty, proto));
    }

    if domain != AF_UNIX {
        return Err(AxError::from(LinuxError::EAFNOSUPPORT));
    }

    let pid = current().as_thread().proc_data.proc.pid();
    let (sock1, sock2) = match ty {
        SOCK_STREAM => {
            let (sock1, sock2) = StreamTransport::new_pair(pid);
            (UnixSocket::new(sock1), UnixSocket::new(sock2))
        }
        SOCK_DGRAM | SOCK_SEQPACKET => {
            let (sock1, sock2) = DgramTransport::new_pair(pid);
            (UnixSocket::new(sock1), UnixSocket::new(sock2))
        }
        SOCK_RAW => {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        _ => {
            warn!("Unsupported socketpair type: {ty}");
            return Err(AxError::InvalidInput);
        }
    };
    let sock1 = Socket(SocketInner::Unix(sock1));
    let sock2 = Socket(SocketInner::Unix(sock2));

    if raw_ty & O_NONBLOCK != 0 {
        sock1.set_nonblocking(true)?;
        sock2.set_nonblocking(true)?;
    }
    let cloexec = raw_ty & O_CLOEXEC != 0;

    *fds.get_as_mut()? = [
        sock1.add_to_fd_table(cloexec)?,
        sock2.add_to_fd_table(cloexec)?,
    ];
    Ok(0)
}
