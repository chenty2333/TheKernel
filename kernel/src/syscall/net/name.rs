use axerrno::AxResult;
use axnet::SocketOps;
use linux_raw_sys::net::{sockaddr, socklen_t};
use starry_vm::{VmMutPtr, VmPtr};

use super::addr::SocketAddrExt;
use crate::{
    file::{FileLike, NetlinkSocket, Socket},
    mm::UserPtr,
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
    let mut length = read_socklen(addrlen)?;
    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        socket.write_local_addr(addr, &mut length)?;
        write_socklen(addrlen, length)?;
        return Ok(0);
    }

    let socket = Socket::from_fd(fd)?;
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
    let mut length = read_socklen(addrlen)?;
    let socket = Socket::from_fd(fd)?;
    let peer_addr = socket.peer_addr()?;
    debug!("sys_getpeername <= fd: {fd}, addr: {peer_addr:?}");

    peer_addr.write_to_user(addr, &mut length)?;
    write_socklen(addrlen, length)?;
    Ok(0)
}
