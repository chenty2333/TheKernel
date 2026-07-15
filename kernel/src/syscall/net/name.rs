use axerrno::AxResult;
use axnet::SocketOps;
use linux_raw_sys::net::{sockaddr, socklen_t};
use starry_vm::{VmMutPtr, VmPtr};

use super::{SocketSyscallSnapshot, addr::SocketAddrExt, import_socket_output_after_policy};
use crate::{
    file::{PinnedSocketDescription, SocketBackendKind},
    mm::UserPtr,
    task::security::{SocketSecurityContext, dispatch_socket},
};

fn read_socklen(addrlen: UserPtr<socklen_t>) -> AxResult<socklen_t> {
    let addrlen = (addrlen.address().as_usize() as *mut socklen_t).vm_read()?;
    if addrlen > i32::MAX as socklen_t {
        return Err(axerrno::AxError::InvalidInput);
    }
    Ok(addrlen)
}

fn write_socklen(addrlen: UserPtr<socklen_t>, value: socklen_t) -> AxResult<()> {
    (addrlen.address().as_usize() as *mut socklen_t)
        .vm_write(value)
        .map_err(Into::into)
}

pub fn sys_getsockname(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    let mut length = import_socket_output_after_policy(
        || {
            dispatch_socket(&SocketSecurityContext::get_sock_name(
                snapshot.actor(),
                &socket_ref,
            ))
        },
        || read_socklen(addrlen),
    )?;
    if pinned.backend()? == SocketBackendKind::Netlink {
        pinned.netlink()?.write_local_addr(addr, &mut length)?;
        write_socklen(addrlen, length)?;
        return Ok(0);
    }

    let socket = pinned.network()?;
    let local_addr = socket.local_addr()?;
    debug!("sys_getsockname <= fd: {fd}, addr: {local_addr:?}");

    local_addr.write_to_user(addr, &mut length)?;
    write_socklen(addrlen, length)?;
    Ok(0)
}

pub fn sys_getpeername(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    let mut length = import_socket_output_after_policy(
        || {
            dispatch_socket(&SocketSecurityContext::get_peer_name(
                snapshot.actor(),
                &socket_ref,
            ))
        },
        || read_socklen(addrlen),
    )?;
    let socket = pinned.network()?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    peer_addr.write_to_user(addr, &mut length)?;
    write_socklen(addrlen, length)?;
    Ok(0)
}
