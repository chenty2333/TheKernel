use axerrno::{AxResult, LinuxError};
use axnet::{SocketAddrEx, SocketOps};
use linux_raw_sys::net::{sockaddr, socklen_t};

use super::{SocketSyscallSnapshot, addr::SocketAddrExt, packet::write_socket_name};
use crate::{
    file::{PinnedSocketDescription, SocketBackendKind},
    mm::{UserMemoryCapability, UserPtr, map_usercopy_error},
    task::security::{SocketSecurityContext, dispatch_socket},
};

fn read_socklen(
    capability: &UserMemoryCapability,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<socklen_t> {
    let addrlen = capability
        .read_value(addrlen.address().as_usize() as *const socklen_t)
        .map_err(map_usercopy_error)?;
    if addrlen > i32::MAX as socklen_t {
        return Err(axerrno::AxError::InvalidInput);
    }
    Ok(addrlen)
}

fn write_socklen(
    capability: &UserMemoryCapability,
    addrlen: UserPtr<socklen_t>,
    value: socklen_t,
) -> AxResult<()> {
    capability
        .write_value(addrlen.address().as_usize() as *mut socklen_t, value)
        .map_err(map_usercopy_error)
}

/// The tail of `move_addr_to_user()` (`net/socket.c:285-313`) for a provider
/// that produced raw address bytes: the caller's `*addrlen` supplies the
/// destination capacity (`get_user(len, ulen)`), a longer stored name is
/// truncated to the capacity, and the provider's true length — `klen` — is what
/// the kernel reports back.
///
/// `netlink_getname()` hands `do_getsockname()` a `struct sockaddr_nl` and its
/// `sizeof(struct sockaddr_nl)` return value (`net/netlink/af_netlink.c:
/// 1105-1127`); it never reads the caller's length itself.
fn move_addr_bytes_to_user(
    capability: &UserMemoryCapability,
    bytes: &[u8],
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<()> {
    let requested = read_socklen(capability, addrlen)? as i32;
    let copy_len = if requested > bytes.len() as i32 {
        bytes.len() as i32
    } else {
        requested
    };
    // `move_addr_to_user()` writes the provider's own length back before it
    // copies, so even a truncated or faulting copy reports `klen`
    // (`net/socket.c:288-303`).
    if copy_len >= 0 {
        write_socklen(capability, addrlen, bytes.len() as socklen_t)?;
    }
    if copy_len != 0 {
        if copy_len < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        capability
            .write_bytes(addr.address().as_usize(), &bytes[..copy_len as usize])
            .map_err(map_usercopy_error)?;
    }
    Ok(())
}

/// The same tail for a provider that hands back a decoded [`SocketAddrEx`].
fn move_addr_to_user(
    capability: &UserMemoryCapability,
    name: &SocketAddrEx,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<()> {
    let mut length = read_socklen(capability, addrlen)?;
    let result = name.write_to_user(capability, addr, &mut length);
    // `move_addr_to_user()` reports the provider's own length before it copies
    // the payload (`net/socket.c:288-303`), so a truncating or faulting copy
    // still leaves `*addrlen` at `klen`.  A negative request is refused before
    // either write and leaves the value alone.
    if (length as i32) >= 0 {
        write_socklen(capability, addrlen, length)?;
    }
    result
}

pub fn sys_getsockname(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    if matches!(
        pinned.backend()?,
        SocketBackendKind::Xdp | SocketBackendKind::AfAlg
    ) {
        // `xsk_proto_ops` and `alg_proto_ops` both route `.getname` to
        // `sock_no_getname`, which is `return -EOPNOTSUPP;` before Linux
        // imports either output pointer.  The security hook therefore still
        // runs first, and neither the address nor the length is touched.
        dispatch_socket(&SocketSecurityContext::get_sock_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if pinned.backend()? == SocketBackendKind::Packet {
        dispatch_socket(&SocketSecurityContext::get_sock_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        // The backend result precedes output-capacity import, as in Linux's
        // getname -> move_addr_to_user sequence.
        let name = pinned.packet()?.get_name()?;
        let mut length = read_socklen(&capability, addrlen)?;
        let result = write_socket_name(&capability, name, addr, &mut length);
        // The same length-before-payload rule as `move_addr_to_user()`.
        if (length as i32) >= 0 {
            write_socklen(&capability, addrlen, length)?;
        }
        result?;
        return Ok(0);
    }
    // `do_getsockname()` runs the security hook, then `sock->ops->getname()`,
    // and only then `move_addr_to_user()` (`net/socket.c:2160-2176`).  The
    // provider therefore decides the error before the caller's `*addrlen` is
    // read at all: a failing `getname` reports its own errno — ENOTCONN for an
    // unconnected `getpeername` — even when the length pointer is invalid, and
    // the address is never imported.
    dispatch_socket(&SocketSecurityContext::get_sock_name(
        snapshot.actor(),
        &socket_ref,
    ))?;
    if pinned.backend()? == SocketBackendKind::Netlink {
        let bytes = pinned.netlink()?.name_record(false).into_bytes();
        return move_addr_bytes_to_user(&capability, &bytes, addr, addrlen).map(|()| 0);
    }

    let socket = pinned.network()?;
    let local_addr = socket.local_addr()?;
    debug!("sys_getsockname <= fd: {fd}, addr: {local_addr:?}");

    move_addr_to_user(&capability, &local_addr, addr, addrlen)?;
    Ok(0)
}

pub fn sys_getpeername(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    if matches!(
        pinned.backend()?,
        SocketBackendKind::Xdp | SocketBackendKind::AfAlg
    ) {
        // `sock_no_getname` rejects before Linux imports either output pointer.
        // Keep the security hook in front of that rejection.
        dispatch_socket(&SocketSecurityContext::get_peer_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if pinned.backend()? == SocketBackendKind::Packet {
        // `packet_getname(peer=true)` rejects before Linux imports either
        // output pointer. Keep the security hook in front of that rejection.
        dispatch_socket(&SocketSecurityContext::get_peer_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    // The same getname-then-move_addr_to_user order as `getsockname`; the peer
    // provider is what answers ENOTCONN for an unconnected socket.
    dispatch_socket(&SocketSecurityContext::get_peer_name(
        snapshot.actor(),
        &socket_ref,
    ))?;
    if pinned.backend()? == SocketBackendKind::Netlink {
        let bytes = pinned.netlink()?.name_record(true).into_bytes();
        return move_addr_bytes_to_user(&capability, &bytes, addr, addrlen).map(|()| 0);
    }
    let socket = pinned.network()?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    move_addr_to_user(&capability, &peer_addr, addr, addrlen)?;
    Ok(0)
}
