use axerrno::AxResult;
use axnet::SocketOps;
use linux_raw_sys::net::{sockaddr, socklen_t};

use super::addr::SocketAddrExt;
use crate::{
    file::{FileLike, NetlinkSocket, Socket},
    mm::UserPtr,
};

fn checked_socklen_mut(addrlen: UserPtr<socklen_t>) -> AxResult<&'static mut socklen_t> {
    let addrlen = addrlen.get_as_mut()?;
    if *addrlen > i32::MAX as socklen_t {
        return Err(axerrno::AxError::InvalidInput);
    }
    Ok(addrlen)
}

pub fn sys_getsockname(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        socket.write_local_addr(addr, checked_socklen_mut(addrlen)?)?;
        return Ok(0);
    }

    let socket = Socket::from_fd(fd)?;
    let local_addr = socket.local_addr()?;
    debug!("sys_getsockname <= fd: {fd}, addr: {local_addr:?}");

    local_addr.write_to_user(addr, checked_socklen_mut(addrlen)?)?;
    Ok(0)
}

pub fn sys_getpeername(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let socket = Socket::from_fd(fd)?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    peer_addr.write_to_user(addr, checked_socklen_mut(addrlen)?)?;
    Ok(0)
}
