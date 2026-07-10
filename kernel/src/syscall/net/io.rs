use alloc::{boxed::Box, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    net::Ipv4Addr,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axnet::{CMsgData, RecvFlags, RecvOptions, SendFlags, SendOptions, SocketAddrEx, SocketOps};
use linux_raw_sys::{
    general::timespec,
    net::{
        MSG_CMSG_CLOEXEC, MSG_DONTWAIT, MSG_ERRQUEUE, MSG_OOB, MSG_PEEK, MSG_TRUNC, MSG_WAITALL,
        SCM_RIGHTS, SOL_SOCKET, cmsghdr, mmsghdr, msghdr, sockaddr, socklen_t,
    },
};
use starry_vm::{vm_read_slice, vm_write_slice};

use super::addr::SocketAddrExt;
use crate::{
    file::{AfAlgSocket, FileLike, NetlinkSocket, Socket, add_file_description},
    mm::{
        IoVec, IoVectorBuf, UserConstPtr, UserPtr, VmBytes, VmBytesMut, check_user_readable,
        check_user_writable,
    },
    syscall::net::{CMsg, CMsgBuilder},
};

const MAX_RECVMSG_IOVCNT: usize = 1024;
const SUPPORTED_RECVMSG_FLAGS: u32 =
    MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT | MSG_WAITALL | MSG_CMSG_CLOEXEC | MSG_ERRQUEUE | MSG_OOB;

fn read_user_copy<T: Copy>(ptr: UserConstPtr<T>) -> AxResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    vm_read_slice(ptr.address().as_usize() as *const u8, unsafe {
        core::slice::from_raw_parts_mut(
            value.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            size_of::<T>(),
        )
    })?;
    Ok(unsafe { value.assume_init() })
}

fn write_user_copy<T: Copy>(ptr: UserPtr<T>, value: T) -> AxResult {
    Ok(vm_write_slice(
        ptr.address().as_usize() as *mut u8,
        unsafe { core::slice::from_raw_parts((&value as *const T).cast::<u8>(), size_of::<T>()) },
    )?)
}

fn validate_recvmsg_flags(flags: u32) -> AxResult<RecvFlags> {
    if flags & !SUPPORTED_RECVMSG_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MSG_OOB != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MSG_ERRQUEUE != 0 {
        return Err(AxError::from(LinuxError::EAGAIN));
    }

    let mut recv_flags = RecvFlags::empty();
    if flags & MSG_PEEK != 0 {
        recv_flags |= RecvFlags::PEEK;
    }
    if flags & MSG_TRUNC != 0 {
        recv_flags |= RecvFlags::TRUNCATE;
    }
    Ok(recv_flags)
}

fn validate_sendmsg_flags(flags: u32) -> AxResult {
    if flags & MSG_OOB != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    Ok(())
}

fn send_impl(
    fd: i32,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    cmsg: Vec<CMsgData>,
) -> AxResult<isize> {
    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        debug!("sys_send <= fd: {fd}, flags: {flags}, netlink");
        validate_sendmsg_flags(flags)?;
        return socket.write(&mut src).map(|sent| sent as isize);
    }

    let addr = if addr.is_null() || addrlen == 0 {
        None
    } else {
        Some(SocketAddrEx::read_from_user(addr, addrlen)?)
    };

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {addr:?}");

    let socket = Socket::from_fd(fd)?;
    validate_sendmsg_flags(flags)?;
    let sent = socket.send(
        &mut src,
        SendOptions {
            to: addr,
            flags: SendFlags::default(),
            cmsg,
        },
    )?;

    Ok(sent as isize)
}

pub fn sys_sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
) -> AxResult<isize> {
    if len != 0 {
        check_user_readable(buf as usize, len)?;
    }
    send_impl(fd, VmBytes::new(buf, len), flags, addr, addrlen, Vec::new())
}

pub fn sys_sendmsg(fd: i32, msg: UserConstPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let msg = read_user_copy(msg)?;
    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        debug!("sys_sendmsg <= fd: {fd}, flags: {flags}, af_alg");
        return socket.sendmsg(&msg).map(|sent| sent as isize);
    }

    let mut cmsg = Vec::new();
    if !msg.msg_control.is_null() {
        let mut ptr = msg.msg_control as usize;
        let ptr_end = ptr + msg.msg_controllen;
        while ptr + size_of::<cmsghdr>() <= ptr_end {
            let hdr = UserConstPtr::<cmsghdr>::from(ptr).get_as_ref()?;
            if ptr_end - ptr < hdr.cmsg_len {
                return Err(AxError::InvalidInput);
            }
            cmsg.push(Box::new(CMsg::parse(hdr)?) as CMsgData);
            ptr += hdr.cmsg_len;
        }
    }
    let send_iov = IoVectorBuf::new(msg.msg_iov.cast::<IoVec>(), msg.msg_iovlen)?;
    send_iov.check_readable()?;

    send_impl(
        fd,
        send_iov.into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        cmsg,
    )
}

fn recv_impl(
    fd: i32,
    mut dst: impl Write + IoBufMut,
    recv_flags: RecvFlags,
    addr: UserPtr<sockaddr>,
    addrlen: Option<&mut socklen_t>,
    cmsg_builder: Option<CMsgBuilder>,
    cloexec_rights: bool,
) -> AxResult<isize> {
    debug!("sys_recv <= fd: {fd}, flags: {recv_flags:?}");

    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        let recv = socket.recv_from(&mut dst, recv_flags, addr, addrlen)?;
        debug!("sys_recv => fd: {fd}, netlink recv: {recv}");
        return Ok(recv as isize);
    }

    let socket = Socket::from_fd(fd)?;
    let mut cmsg = Vec::new();

    let mut remote_addr =
        (!addr.is_null()).then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let recv = socket.recv(
        &mut dst,
        RecvOptions {
            from: remote_addr.as_mut(),
            flags: recv_flags,
            cmsg: Some(&mut cmsg),
        },
    )?;

    if let (Some(remote_addr), Some(addrlen)) = (remote_addr, addrlen) {
        remote_addr.write_to_user(addr, addrlen)?;
    }

    if let Some(mut builder) = cmsg_builder {
        for cmsg in cmsg {
            let Ok(cmsg) = cmsg.downcast::<CMsg>() else {
                warn!("received unexpected cmsg");
                continue;
            };

            let pushed = match *cmsg {
                CMsg::Rights { fds } => builder.push(SOL_SOCKET, SCM_RIGHTS, |data| {
                    let mut written = 0;
                    for (f, chunk) in fds.into_iter().zip(data.chunks_exact_mut(size_of::<i32>())) {
                        let fd = add_file_description(f, cloexec_rights)?;
                        chunk.copy_from_slice(&fd.to_ne_bytes());
                        written += size_of::<i32>();
                    }
                    Ok(written)
                })?,
            };
            if !pushed {
                break;
            }
        }
    }

    debug!("sys_recv => fd: {fd}, recv: {recv}");
    Ok(recv as isize)
}

pub fn sys_recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: u32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    if len != 0 {
        check_user_writable(buf as usize, len)?;
    }
    let recv_flags = validate_recvmsg_flags(flags)?;
    let addrlen_ptr = addrlen;
    let mut user_addrlen = if addr.is_null() {
        None
    } else {
        Some(read_user_copy(UserConstPtr::<socklen_t>::from(
            addrlen_ptr.address().as_usize(),
        ))?)
    };
    let recv = recv_impl(
        fd,
        VmBytesMut::new(buf, len),
        recv_flags,
        addr,
        user_addrlen.as_mut(),
        None,
        flags & MSG_CMSG_CLOEXEC != 0,
    )?;
    if let Some(new_addrlen) = user_addrlen {
        write_user_copy(addrlen_ptr, new_addrlen)?;
    }
    Ok(recv)
}

pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let recv_flags = validate_recvmsg_flags(flags)?;
    let mut msg_hdr = read_user_copy(UserConstPtr::<msghdr>::from(msg.address().as_usize()))?;
    if (msg_hdr.msg_namelen as i32) < 0 || (msg_hdr.msg_controllen as isize) < 0 {
        return Err(AxError::InvalidInput);
    }
    if msg_hdr.msg_iovlen == 0 || msg_hdr.msg_iovlen > MAX_RECVMSG_IOVCNT {
        return Err(AxError::from(LinuxError::EMSGSIZE));
    }

    let recv_iov = IoVectorBuf::new(msg_hdr.msg_iov.cast::<IoVec>(), msg_hdr.msg_iovlen)?;
    recv_iov.check_writable()?;

    let mut name_len = msg_hdr.msg_namelen as socklen_t;
    let recv = recv_impl(
        fd,
        recv_iov.into_io(),
        recv_flags,
        UserPtr::from(msg_hdr.msg_name as usize),
        (!msg_hdr.msg_name.is_null()).then_some(&mut name_len),
        (!msg_hdr.msg_control.is_null()).then(|| {
            CMsgBuilder::new(
                UserPtr::from(msg_hdr.msg_control.cast::<cmsghdr>()),
                &mut msg_hdr.msg_controllen,
            )
        }),
        flags & MSG_CMSG_CLOEXEC != 0,
    )?;
    msg_hdr.msg_namelen = name_len as _;
    write_user_copy(msg, msg_hdr)?;
    Ok(recv)
}

pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> AxResult<isize> {
    if vlen == 0 {
        return Err(AxError::InvalidInput);
    }

    let mut sent = 0usize;
    let base = msgvec.address().as_usize();
    for idx in 0..vlen as usize {
        let ptr = base + idx * size_of::<mmsghdr>();
        let msg = UserConstPtr::<mmsghdr>::from(ptr).cast::<msghdr>();
        match sys_sendmsg(fd, msg, flags) {
            Ok(len) => {
                let mut header = read_user_copy(UserConstPtr::<mmsghdr>::from(ptr))?;
                header.msg_len = len as u32;
                write_user_copy(UserPtr::<mmsghdr>::from(ptr), header)?;
                sent += 1;
            }
            Err(_err) if sent != 0 => return Ok(sent as isize),
            Err(err) => return Err(err),
        }
    }
    Ok(sent as isize)
}

fn validate_recvmmsg_timeout(timeout: UserConstPtr<timespec>) -> AxResult {
    if timeout.is_null() {
        return Ok(());
    }
    let timeout = read_user_copy(timeout)?;
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

pub fn sys_recvmmsg(
    fd: i32,
    msgvec: UserPtr<mmsghdr>,
    vlen: u32,
    flags: u32,
    timeout: UserConstPtr<timespec>,
) -> AxResult<isize> {
    if vlen == 0 {
        return Err(AxError::InvalidInput);
    }
    validate_recvmmsg_timeout(timeout)?;
    UserConstPtr::<mmsghdr>::from(msgvec.address().as_usize()).get_as_slice(vlen as usize)?;

    let mut received = 0usize;
    let base = msgvec.address().as_usize();
    for idx in 0..vlen as usize {
        let ptr = base + idx * size_of::<mmsghdr>();
        let msg = UserPtr::<mmsghdr>::from(ptr).cast::<msghdr>();
        match sys_recvmsg(fd, msg, flags) {
            Ok(len) => {
                let mut header = read_user_copy(UserConstPtr::<mmsghdr>::from(ptr))?;
                header.msg_len = len as u32;
                write_user_copy(UserPtr::<mmsghdr>::from(ptr), header)?;
                received += 1;
                if len == 0 {
                    break;
                }
            }
            Err(_err) if received != 0 => return Ok(received as isize),
            Err(AxError::OperationNotSupported) => {
                return Err(AxError::from(LinuxError::ENOSYS));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(received as isize)
}
