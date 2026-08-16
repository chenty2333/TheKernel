use axerrno::{AxResult, LinuxError};
use axnet::SocketOps;
use linux_raw_sys::net::{sockaddr, socklen_t};

use super::{
    SocketSyscallSnapshot, addr::SocketAddrExt, import_socket_output_after_policy,
    packet::write_socket_name,
};
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

pub fn sys_getsockname(
    capability: UserMemoryCapability,
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    if pinned.backend()? == SocketBackendKind::Packet {
        dispatch_socket(&SocketSecurityContext::get_sock_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        // The backend result precedes output-capacity import, as in Linux's
        // getname -> move_addr_to_user sequence.
        let name = pinned.packet()?.get_name()?;
        let mut length = read_socklen(&capability, addrlen)?;
        write_socket_name(&capability, name, addr, &mut length)?;
        write_socklen(&capability, addrlen, length)?;
        return Ok(0);
    }
    let mut length = import_socket_output_after_policy(
        || {
            dispatch_socket(&SocketSecurityContext::get_sock_name(
                snapshot.actor(),
                &socket_ref,
            ))
        },
        || read_socklen(&capability, addrlen),
    )?;
    if pinned.backend()? == SocketBackendKind::Netlink {
        pinned
            .netlink()?
            .write_local_addr(&capability, addr, &mut length)?;
        write_socklen(&capability, addrlen, length)?;
        return Ok(0);
    }

    let socket = pinned.network()?;
    let local_addr = socket.local_addr()?;
    debug!("sys_getsockname <= fd: {fd}, addr: {local_addr:?}");

    local_addr.write_to_user(&capability, addr, &mut length)?;
    write_socklen(&capability, addrlen, length)?;
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
    if pinned.backend()? == SocketBackendKind::Packet {
        // `packet_getname(peer=true)` rejects before Linux imports either
        // output pointer. Keep the security hook in front of that rejection.
        dispatch_socket(&SocketSecurityContext::get_peer_name(
            snapshot.actor(),
            &socket_ref,
        ))?;
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let mut length = import_socket_output_after_policy(
        || {
            dispatch_socket(&SocketSecurityContext::get_peer_name(
                snapshot.actor(),
                &socket_ref,
            ))
        },
        || read_socklen(&capability, addrlen),
    )?;
    let socket = pinned.network()?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    peer_addr.write_to_user(&capability, addr, &mut length)?;
    write_socklen(&capability, addrlen, length)?;
    Ok(0)
}
