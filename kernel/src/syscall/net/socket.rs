use alloc::sync::Arc;
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
    general::{CAP_NET_BIND_SERVICE, O_CLOEXEC, O_NONBLOCK, O_RDWR},
    net::{
        AF_INET, AF_INET6, AF_NETLINK, AF_PACKET, AF_UNIX, AF_UNSPEC, AF_VSOCK, IPPROTO_TCP,
        IPPROTO_UDP, SHUT_RD, SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET,
        SOCK_STREAM, sockaddr, socklen_t,
    },
};
use starry_vm::{VmMutPtr, VmPtr, vm_write_slice};

use super::addr::SocketAddrExt;
use crate::{
    file::{
        AfAlgSocket, FileDescription, FileLike, NetlinkSocket, PinnedSocketDescription, Socket,
        SocketBackendKind, add_file_like_with_flags, af_alg, close_file_like,
        permission::VfsSecurityContext, reserve_fd,
    },
    mm::{UserConstPtr, UserPtr},
    task::{AsThread, NetworkNamespace, ns_capable},
};

const FIRST_UNPRIVILEGED_PORT: u16 = 1024;
const SOCK_DCCP: u32 = 6;
const SOCK_TYPE_MASK: u32 = 0xf;
const SOCK_CLOEXEC_NONBLOCK_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;

const fn socket_status_flags(nonblocking: bool) -> u32 {
    O_RDWR | if nonblocking { O_NONBLOCK } else { 0 }
}

fn add_new_socket_like<T: FileLike + 'static>(
    socket: T,
    nonblocking: bool,
    cloexec: bool,
) -> AxResult<i32> {
    if nonblocking {
        socket.set_nonblocking(true)?;
    }
    let socket = Arc::try_new(socket).map_err(|_| AxError::NoMemory)?;
    add_file_like_with_flags(socket, cloexec, socket_status_flags(nonblocking))
}

fn current_unix_credentials() -> UnixCredentials {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let ids = curr.as_thread().current_cred().ids();
    UnixCredentials::new(
        proc_data.proc.pid(),
        ids.euid.into_raw(),
        ids.egid.into_raw(),
    )
}

fn require_bind_permissions(addr: &SocketAddrEx, net_ns: &NetworkNamespace) -> AxResult<()> {
    let SocketAddrEx::Ip(ip_addr) = addr else {
        return Ok(());
    };

    if ip_addr.port() != 0 && ip_addr.port() < FIRST_UNPRIVILEGED_PORT {
        let current = current();
        let cred = current.as_thread().current_cred();
        if !ns_capable(&cred, net_ns.owner_user_ns(), CAP_NET_BIND_SERVICE) {
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
        return add_new_socket_like(socket, nonblocking, cloexec).map(|fd| fd as isize);
    }

    if domain == AF_PACKET {
        return Err(LinuxError::EAFNOSUPPORT.into());
    }

    let net_ns = current().as_thread().proc_data.net_ns.clone();

    if domain == AF_NETLINK {
        NetlinkSocket::validate_socket_type(ty, proto)?;
        let socket = NetlinkSocket::try_new(proto, net_ns)?;
        if nonblocking {
            socket.set_nonblocking(true)?;
        }
        return add_file_like_with_flags(socket as _, cloexec, socket_status_flags(nonblocking))
            .map(|fd| fd as isize);
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

    add_new_socket_like(socket, nonblocking, cloexec).map(|fd| fd as isize)
}

pub fn sys_bind(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    match pinned.backend()? {
        SocketBackendKind::AfAlg => {
            let addr = af_alg::SockAddrAlg::read_from_user(addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, af_alg: {addr:?}");
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
                current().as_thread().proc_data.proc.pid() as u32
            } else {
                addr.nl_pid
            };
            pinned.netlink()?.bind(port_id, addr.nl_groups)?;
        }
        SocketBackendKind::Network => {
            let socket = pinned.network()?;
            let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
            debug!("sys_bind <= fd: {fd}, addr: {addr:?}");

            if let (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) =
                (&socket.inner, &addr)
            {
                let curr = current();
                let proc_data = &curr.as_thread().proc_data;
                let security = VfsSecurityContext::new(curr.as_thread().current_cred());
                crate::file::unix_socket::bind_path(
                    unix,
                    path.clone(),
                    &security,
                    NodePermission::from_bits_truncate(0o777),
                    proc_data.umask(),
                )?;
            } else {
                require_bind_permissions(&addr, socket.net_namespace())?;
                socket.bind(addr)?;
            }
        }
    }

    Ok(0)
}

pub fn sys_connect(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> AxResult<isize> {
    // Pin the open file description once. Address decoding intentionally
    // remains before the ENOTSOCK downcast for the ordinary connect path, but
    // a sibling sharing the fd table can no longer redirect the operation by
    // closing and reusing the numeric descriptor between those two steps.
    let pinned = PinnedSocketDescription::pin_fd(fd)?;

    if addrlen as usize >= size_of::<linux_raw_sys::net::__kernel_sa_family_t>()
        && super::addr::read_family(addr, addrlen)? as u32 == AF_UNSPEC
    {
        debug!("sys_connect <= fd: {fd}, addr: AF_UNSPEC");
        pinned.network()?.disconnect()?;
        return Ok(0);
    }

    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;
    debug!("sys_connect <= fd: {fd}, addr: {addr:?}");

    let socket = pinned.network()?;
    let result = match (&socket.inner, &addr) {
        (SocketInner::Unix(unix), SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
            let curr = current();
            let credentials = curr.as_thread().fs_dac_credentials();
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
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket = pinned.network()?;
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

    let pinned = PinnedSocketDescription::from_fd(fd)?;
    if pinned.backend()? == SocketBackendKind::AfAlg {
        let request = pinned.af_alg()?.accept_request()?;
        if !addr.is_null() {
            (addrlen.address().as_usize() as *mut socklen_t).vm_write(0)?;
        }
        return add_new_socket_like(request, nonblocking, cloexec).map(|fd| fd as isize);
    }

    let socket = pinned.network()?;
    let net_ns = socket.net_namespace().clone();
    let socket = Socket::new(socket.accept()?, net_ns);

    let remote_addr = socket.peer_addr()?;
    if !addr.is_null() {
        let addrlen_ptr = addrlen.address().as_usize() as *mut socklen_t;
        let mut value = addrlen_ptr.vm_read()?;
        remote_addr.write_to_user(addr, &mut value)?;
        addrlen_ptr.vm_write(value)?;
    }

    let fd = add_new_socket_like(socket, nonblocking, cloexec).map(|fd| fd as isize)?;
    debug!("sys_accept => fd: {fd}, addr: {remote_addr:?}");

    Ok(fd)
}

pub fn sys_shutdown(fd: i32, how: u32) -> AxResult<isize> {
    debug!("sys_shutdown <= fd: {fd}, how: {how:?}");

    let pinned = PinnedSocketDescription::from_fd(fd)?;
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

    let credentials = current_unix_credentials();
    let net_ns = current().as_thread().proc_data.net_ns.clone();
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

    // Reserve both numbers before exposing either descriptor. The user copy
    // operates on a kernel-owned array and completes before fd publication,
    // so a concurrent unmap/remap cannot invalidate a retained Rust reference
    // and EFAULT leaves no partially installed socket behind.
    let reserved1 = reserve_fd(cloexec)?;
    let reserved2 = reserve_fd(cloexec)?;
    let status_flags = socket_status_flags(nonblocking);
    let description1 = FileDescription::new_with_flags(
        Arc::try_new(sock1).map_err(|_| AxError::NoMemory)? as Arc<dyn FileLike>,
        status_flags,
    )?;
    let description2 = FileDescription::new_with_flags(
        Arc::try_new(sock2).map_err(|_| AxError::NoMemory)? as Arc<dyn FileLike>,
        status_flags,
    )?;
    let fd_pair = [reserved1.fd(), reserved2.fd()];
    vm_write_slice(fds.address().as_usize() as *mut i32, &fd_pair)?;

    let fd1 = reserved1.publish(description1)?;
    if let Err(error) = reserved2.publish(description2) {
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
}
