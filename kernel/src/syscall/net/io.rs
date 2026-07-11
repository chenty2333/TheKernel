use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    net::Ipv4Addr,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axnet::{
    CMsgData, RecvFlags, RecvOptions, SendFlags, SendOptions, Socket as AxSocket, SocketAddrEx,
    SocketOps,
    options::{Configurable, GetSocketOption},
    unix::UnixSocketAddr,
};
use axtask::current;
use linux_raw_sys::{
    general::{O_PATH, timespec},
    net::{
        MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT, MSG_ERRQUEUE, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK,
        MSG_TRUNC, MSG_WAITALL, cmsghdr, mmsghdr, msghdr, sockaddr, socklen_t,
    },
};
use starry_signal::{SignalInfo, Signo};
use starry_vm::{vm_read_slice, vm_write_slice};

use super::addr::SocketAddrExt;
use crate::{
    file::{
        AfAlgSocket, File, FileDescription, FileLike, NetlinkSocket, Socket, get_file_description,
    },
    mm::{
        IoVec, IoVectorBuf, UserConstPtr, UserPtr, VmBytes, VmBytesMut, check_user_readable,
        check_user_writable,
    },
    syscall::net::{CMsg, CMsgBuilder, SCM_MAX_FD},
    task::{AsThread, send_signal_to_process},
};

const MAX_RECVMSG_IOVCNT: usize = 1024;
const MAX_MMSG_VLEN: usize = 1024;
const MSG_WAITFORONE: u32 = 0x1_0000;
// Linux bounds ancillary allocation with net.core.optmem_max. TheKernel only
// supports SCM_RIGHTS here, so 64 KiB leaves ample room for fragmented headers
// while imposing a hard parsing/work bound.
const MAX_SENDMSG_CONTROL_LEN: usize = 64 * 1024;
const SUPPORTED_RECVMSG_FLAGS: u32 =
    MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT | MSG_WAITALL | MSG_CMSG_CLOEXEC | MSG_ERRQUEUE | MSG_OOB;
const SUPPORTED_SENDMSG_FLAGS: u32 = MSG_DONTWAIT | MSG_NOSIGNAL | MSG_OOB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageSocketKind {
    AfAlg,
    Netlink,
    Network,
}

/// One stable open-file-description snapshot for a whole mmsg operation.
///
/// Linux resolves the numeric fd once at syscall entry. Keeping the OFD alive
/// here prevents a `CLONE_FILES` sibling from redirecting later batch elements
/// (or a deferred error) by closing and reusing the same number.
struct StableMessageSocket {
    description: Arc<FileDescription>,
    kind: MessageSocketKind,
}

impl StableMessageSocket {
    fn from_fd(fd: i32) -> AxResult<Self> {
        Self::from_description(get_file_description(fd)?)
    }

    fn from_description(description: Arc<FileDescription>) -> AxResult<Self> {
        if description.status_flags() & O_PATH != 0 {
            return Err(AxError::BadFileDescriptor);
        }
        let kind = if description.inner.downcast_ref::<AfAlgSocket>().is_some() {
            MessageSocketKind::AfAlg
        } else if description.inner.downcast_ref::<NetlinkSocket>().is_some() {
            MessageSocketKind::Netlink
        } else if description.inner.downcast_ref::<Socket>().is_some() {
            MessageSocketKind::Network
        } else {
            if description
                .inner
                .downcast_ref::<File>()
                .is_some_and(|file| file.inner().is_path())
            {
                return Err(AxError::BadFileDescriptor);
            }
            return Err(AxError::NotASocket);
        };
        Ok(Self { description, kind })
    }

    fn af_alg(&self) -> AxResult<&AfAlgSocket> {
        self.description
            .inner
            .downcast_ref::<AfAlgSocket>()
            .ok_or(AxError::BadState)
    }

    fn netlink(&self) -> AxResult<&NetlinkSocket> {
        self.description
            .inner
            .downcast_ref::<NetlinkSocket>()
            .ok_or(AxError::BadState)
    }

    fn network(&self) -> AxResult<&Socket> {
        self.description
            .inner
            .downcast_ref::<Socket>()
            .ok_or(AxError::BadState)
    }

    fn remember_error(&self, error: AxError) {
        if self.kind == MessageSocketKind::Network
            && let Ok(socket) = self.network()
        {
            socket.set_pending_error(LinuxError::from(error));
        }
    }

    fn take_pending_error(&self) -> AxResult {
        if self.kind != MessageSocketKind::Network {
            return Ok(());
        }
        let mut error = 0;
        self.network()?
            .get_option(GetSocketOption::Error(&mut error))?;
        if error == 0 {
            return Ok(());
        }
        Err(LinuxError::try_from(error)
            .map_err(|_| AxError::BadState)?
            .into())
    }
}

fn admitted_sendmmsg_vlen(vlen: u32) -> Option<usize> {
    if vlen == 0 {
        None
    } else {
        Some((vlen as usize).min(MAX_MMSG_VLEN))
    }
}

fn admitted_recvmmsg_vlen(vlen: u32) -> Option<usize> {
    if vlen == 0 {
        None
    } else {
        // Linux do_recvmmsg does not apply sendmmsg's UIO_MAXIOV batch cap.
        // Each contained msghdr still has its own bounded iov admission.
        Some(vlen as usize)
    }
}

const fn recvmmsg_consumes_pending_error(flags: u32) -> bool {
    flags & MSG_ERRQUEUE == 0
}

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

fn write_user_field<T: Copy>(base: usize, offset: usize, value: T) -> AxResult {
    let address = base.checked_add(offset).ok_or(AxError::BadAddress)?;
    write_user_copy(UserPtr::<T>::from(address), value)
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
    if flags & MSG_WAITALL != 0 {
        return Err(AxError::OperationNotSupported);
    }

    let mut recv_flags = RecvFlags::empty();
    if flags & MSG_PEEK != 0 {
        recv_flags |= RecvFlags::PEEK;
    }
    if flags & MSG_TRUNC != 0 {
        recv_flags |= RecvFlags::TRUNCATE;
    }
    if flags & MSG_DONTWAIT != 0 {
        recv_flags |= RecvFlags::DONT_WAIT;
    }
    Ok(recv_flags)
}

fn validate_sendmsg_flags(flags: u32) -> AxResult<SendFlags> {
    if flags & !SUPPORTED_SENDMSG_FLAGS != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if flags & MSG_OOB != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let mut send_flags = SendFlags::empty();
    if flags & MSG_DONTWAIT != 0 {
        send_flags |= SendFlags::DONT_WAIT;
    }
    Ok(send_flags)
}

fn validate_recvmsg_iovlen(iovlen: usize) -> AxResult {
    if iovlen > MAX_RECVMSG_IOVCNT {
        Err(LinuxError::EMSGSIZE.into())
    } else {
        Ok(())
    }
}

fn raise_sigpipe() {
    let process = current().as_thread().proc_data.proc.pid();
    let _ = send_signal_to_process(process, Some(SignalInfo::new_kernel(Signo::SIGPIPE)));
}

fn cmsg_align(len: usize) -> Option<usize> {
    len.checked_add(size_of::<usize>() - 1)
        .map(|len| len & !(size_of::<usize>() - 1))
}

fn parse_send_control(msg: &msghdr) -> AxResult<Vec<CMsgData>> {
    if msg.msg_controllen == 0 {
        return Ok(Vec::new());
    }
    if msg.msg_control.is_null() {
        return Err(AxError::BadAddress);
    }
    if msg.msg_controllen > MAX_SENDMSG_CONTROL_LEN {
        return Err(AxError::from(LinuxError::ENOBUFS));
    }

    let base = msg.msg_control as usize;
    base.checked_add(msg.msg_controllen)
        .ok_or(AxError::BadAddress)?;

    let mut rights = Vec::new();
    rights
        .try_reserve_exact(SCM_MAX_FD)
        .map_err(|_| AxError::NoMemory)?;

    let mut offset = 0usize;
    while msg.msg_controllen - offset >= size_of::<cmsghdr>() {
        let hdr_addr = base.checked_add(offset).ok_or(AxError::BadAddress)?;
        let hdr = read_user_copy(UserConstPtr::<cmsghdr>::from(hdr_addr))?;
        let remaining = msg.msg_controllen - offset;
        if hdr.cmsg_len < size_of::<cmsghdr>() || hdr.cmsg_len > remaining {
            return Err(AxError::InvalidInput);
        }
        CMsg::append_rights(hdr_addr, &hdr, &mut rights)?;

        let aligned = cmsg_align(hdr.cmsg_len).ok_or(AxError::InvalidInput)?;
        if aligned > remaining {
            // A final cmsg needs CMSG_LEN bytes; trailing CMSG_SPACE padding is
            // optional when there is no following header.
            break;
        }
        offset = offset.checked_add(aligned).ok_or(AxError::InvalidInput)?;
    }

    let mut cmsg = Vec::new();
    if let Some(rights) = CMsg::from_rights(rights)? {
        cmsg.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        cmsg.push(rights);
    }
    Ok(cmsg)
}

fn mmsg_address(base: usize, index: usize) -> AxResult<usize> {
    let offset = index
        .checked_mul(size_of::<mmsghdr>())
        .ok_or(AxError::BadAddress)?;
    base.checked_add(offset).ok_or(AxError::BadAddress)
}

fn recvmmsg_defers_error(error: AxError) -> bool {
    // Linux leaves EAGAIN as the ordinary short-batch terminator. Deferring it
    // through SO_ERROR would make a successful partial receive spuriously fail
    // the next recv call. This also covers SO_RCVTIMEO expiry, which axnet-ng
    // deliberately maps to WouldBlock.
    error != AxError::WouldBlock
}

fn remember_recvmmsg_error(socket: &StableMessageSocket, error: AxError) {
    if !recvmmsg_defers_error(error) {
        return;
    }
    socket.remember_error(error);
}

const fn rights_push_was_truncated(expected: usize, result: super::cmsg::RightsPushResult) -> bool {
    result.installed < expected || !result.published
}

fn should_raise_sigpipe(error: AxError, flags: u32) -> bool {
    error == AxError::BrokenPipe && flags & MSG_NOSIGNAL == 0
}

fn send_impl(
    socket: &StableMessageSocket,
    fd: i32,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    cmsg: Vec<CMsgData>,
) -> AxResult<isize> {
    let send_flags = validate_sendmsg_flags(flags)?;
    if socket.kind == MessageSocketKind::Netlink {
        let socket = socket.netlink()?;
        debug!("sys_send <= fd: {fd}, flags: {flags}, netlink");
        if !cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        return socket.write(&mut src).map(|sent| sent as isize);
    }

    let addr = if addr.is_null() || addrlen == 0 {
        None
    } else {
        Some(SocketAddrEx::read_from_user(addr, addrlen)?)
    };

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {addr:?}");

    if socket.kind != MessageSocketKind::Network {
        return Err(AxError::NotASocket);
    }
    let socket = socket.network()?;
    let pathname = match (&socket.inner, &addr) {
        (AxSocket::Unix(_), Some(SocketAddrEx::Unix(UnixSocketAddr::Path(path)))) => {
            Some(path.clone())
        }
        _ => None,
    };
    let options = SendOptions {
        to: addr,
        flags: send_flags,
        cmsg,
    };
    let sent = if let (AxSocket::Unix(unix), Some(path)) = (&socket.inner, pathname) {
        let curr = current();
        let credentials = curr.as_thread().proc_data.fs_dac_credentials();
        let target = crate::file::unix_socket::resolve_peer(path, &credentials)?;
        unix.send_to_resolved(&mut src, options, target)
    } else {
        socket.send(&mut src, options)
    };
    let sent = match sent {
        Err(error) => {
            if should_raise_sigpipe(error, flags) {
                raise_sigpipe();
            }
            return Err(error);
        }
        Ok(sent) => sent,
    };

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
    let socket = StableMessageSocket::from_fd(fd)?;
    send_impl(
        &socket,
        fd,
        VmBytes::new(buf, len),
        flags,
        addr,
        addrlen,
        Vec::new(),
    )
}

fn sendmsg_with_socket(
    socket: &StableMessageSocket,
    fd: i32,
    msg: UserConstPtr<msghdr>,
    flags: u32,
) -> AxResult<isize> {
    let msg = read_user_copy(msg)?;
    if socket.kind == MessageSocketKind::AfAlg {
        let socket = socket.af_alg()?;
        debug!("sys_sendmsg <= fd: {fd}, flags: {flags}, af_alg");
        if flags != 0 {
            return Err(AxError::OperationNotSupported);
        }
        return socket.sendmsg(&msg).map(|sent| sent as isize);
    }

    let send_iov = IoVectorBuf::new(msg.msg_iov.cast::<IoVec>(), msg.msg_iovlen)?;
    send_iov.check_readable()?;
    let cmsg = parse_send_control(&msg)?;

    send_impl(
        socket,
        fd,
        send_iov.into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        cmsg,
    )
}

pub fn sys_sendmsg(fd: i32, msg: UserConstPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let socket = StableMessageSocket::from_fd(fd)?;
    sendmsg_with_socket(&socket, fd, msg, flags)
}

fn recv_impl(
    socket: &StableMessageSocket,
    fd: i32,
    mut dst: impl Write + IoBufMut,
    recv_flags: RecvFlags,
    addr: UserPtr<sockaddr>,
    addrlen: Option<&mut socklen_t>,
    cmsg_builder: Option<CMsgBuilder>,
    cloexec_rights: bool,
) -> AxResult<(isize, bool)> {
    debug!("sys_recv <= fd: {fd}, flags: {recv_flags:?}");

    if socket.kind == MessageSocketKind::Netlink {
        let socket = socket.netlink()?;
        let recv = socket.recv_from(&mut dst, recv_flags, addr, addrlen)?;
        debug!("sys_recv => fd: {fd}, netlink recv: {recv}");
        return Ok((recv as isize, false));
    }

    if socket.kind != MessageSocketKind::Network {
        return Err(AxError::NotASocket);
    }
    let socket = socket.network()?;
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

    let mut control_truncated = cmsg_builder.is_none() && !cmsg.is_empty();
    if let Some(mut builder) = cmsg_builder {
        for cmsg in cmsg {
            let Ok(cmsg) = cmsg.downcast::<CMsg>() else {
                warn!("received unexpected cmsg");
                control_truncated = true;
                continue;
            };

            match &*cmsg {
                CMsg::Rights { fds, .. } => {
                    let expected = fds.len();
                    let result = builder.push_rights(fds, cloexec_rights);
                    if rights_push_was_truncated(expected, result) {
                        control_truncated = true;
                    }
                }
            }
        }
    }

    // Ancillary fd numbers are a Linux publication point. Process the cmsg
    // first so an invalid msg_name still consumes the message and preserves
    // already exposed descriptors.
    if let (Some(remote_addr), Some(addrlen)) = (remote_addr, addrlen) {
        remote_addr.write_to_user(addr, addrlen)?;
    }

    debug!("sys_recv => fd: {fd}, recv: {recv}");
    Ok((recv as isize, control_truncated))
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
    let socket = StableMessageSocket::from_fd(fd)?;
    let (recv, _control_truncated) = recv_impl(
        &socket,
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

fn recvmsg_with_socket(
    socket: &StableMessageSocket,
    fd: i32,
    msg: UserPtr<msghdr>,
    flags: u32,
    recv_flags: RecvFlags,
) -> AxResult<isize> {
    let mut msg_hdr = read_user_copy(UserConstPtr::<msghdr>::from(msg.address().as_usize()))?;
    if (msg_hdr.msg_namelen as i32) < 0 || (msg_hdr.msg_controllen as isize) < 0 {
        return Err(AxError::InvalidInput);
    }
    validate_recvmsg_iovlen(msg_hdr.msg_iovlen)?;

    let recv_iov = IoVectorBuf::new(msg_hdr.msg_iov.cast::<IoVec>(), msg_hdr.msg_iovlen)?;
    recv_iov.check_writable()?;

    let mut name_len = msg_hdr.msg_namelen as socklen_t;
    msg_hdr.msg_flags = 0;
    let control = if msg_hdr.msg_control.is_null() {
        msg_hdr.msg_controllen = 0;
        None
    } else {
        Some(CMsgBuilder::new(
            UserPtr::from(msg_hdr.msg_control.cast::<cmsghdr>()),
            &mut msg_hdr.msg_controllen,
        ))
    };
    let (recv, control_truncated) = recv_impl(
        socket,
        fd,
        recv_iov.into_io(),
        recv_flags,
        UserPtr::from(msg_hdr.msg_name as usize),
        (!msg_hdr.msg_name.is_null()).then_some(&mut name_len),
        control,
        flags & MSG_CMSG_CLOEXEC != 0,
    )?;
    if control_truncated {
        msg_hdr.msg_flags |= MSG_CTRUNC;
    }
    let msg_addr = msg.address().as_usize();
    // Match Linux's ordered field copyout instead of rewriting the input half
    // of msghdr after the message has already been consumed.
    if !msg_hdr.msg_name.is_null() {
        write_user_field(
            msg_addr,
            core::mem::offset_of!(msghdr, msg_namelen),
            name_len as socklen_t,
        )?;
    }
    write_user_field(
        msg_addr,
        core::mem::offset_of!(msghdr, msg_flags),
        msg_hdr.msg_flags,
    )?;
    write_user_field(
        msg_addr,
        core::mem::offset_of!(msghdr, msg_controllen),
        msg_hdr.msg_controllen,
    )?;
    Ok(recv)
}

pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let recv_flags = validate_recvmsg_flags(flags)?;
    let socket = StableMessageSocket::from_fd(fd)?;
    recvmsg_with_socket(&socket, fd, msg, flags, recv_flags)
}

pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> AxResult<isize> {
    // Linux validates and pins the socket even when there are no elements; a
    // zero vlen only suppresses access to msgvec itself.
    let socket = StableMessageSocket::from_fd(fd)?;
    let Some(vlen) = admitted_sendmmsg_vlen(vlen) else {
        return Ok(0);
    };
    let mut sent = 0usize;
    let base = msgvec.address().as_usize();
    for idx in 0..vlen {
        let ptr = match mmsg_address(base, idx) {
            Ok(ptr) => ptr,
            Err(_err) if sent != 0 => return Ok(sent as isize),
            Err(err) => return Err(err),
        };
        let msg = UserConstPtr::<mmsghdr>::from(ptr).cast::<msghdr>();
        match sendmsg_with_socket(&socket, fd, msg, flags) {
            Ok(len) => {
                if let Err(err) =
                    write_user_field(ptr, core::mem::offset_of!(mmsghdr, msg_len), len as u32)
                {
                    if sent != 0 {
                        return Ok(sent as isize);
                    }
                    return Err(err);
                }
                sent += 1;
            }
            Err(_err) if sent != 0 => return Ok(sent as isize),
            Err(err) => return Err(err),
        }
    }
    Ok(sent as isize)
}

fn recvmmsg_has_timeout(timeout: UserConstPtr<timespec>) -> AxResult<bool> {
    if timeout.is_null() {
        return Ok(false);
    }
    let timeout = read_user_copy(timeout)?;
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(AxError::InvalidInput);
    }
    Ok(true)
}

pub fn sys_recvmmsg(
    fd: i32,
    msgvec: UserPtr<mmsghdr>,
    vlen: u32,
    flags: u32,
    timeout: UserConstPtr<timespec>,
) -> AxResult<isize> {
    // The timeout object is imported before Linux enters do_recvmmsg(), even
    // for vlen zero. Pin the endpoint next, then skip only msgvec processing.
    let has_timeout = recvmmsg_has_timeout(timeout)?;
    let socket = StableMessageSocket::from_fd(fd)?;
    // do_recvmmsg() consumes sk_err before checking vlen, so a zero-length
    // batch still reports (and clears) a deferred error from an earlier batch.
    // MSG_ERRQUEUE is the exception: it reads the error queue rather than
    // consuming the ordinary one-shot socket error.
    if recvmmsg_consumes_pending_error(flags) {
        socket.take_pending_error()?;
    }
    let Some(vlen) = admitted_recvmmsg_vlen(vlen) else {
        return Ok(0);
    };
    // A per-call recvmmsg deadline cannot be represented by the socket's
    // shared SO_RCVTIMEO without racing other users of the OFD. Reject it until
    // the receive poller accepts an explicit deadline instead of silently
    // ignoring a valid timeout.
    if has_timeout {
        return Err(AxError::OperationNotSupported);
    }
    let wait_for_one = flags & MSG_WAITFORONE != 0;
    let mut active_flags = flags & !MSG_WAITFORONE;
    let mut recv_flags = validate_recvmsg_flags(active_flags)?;
    let mut received = 0usize;
    let base = msgvec.address().as_usize();
    for idx in 0..vlen {
        let ptr = match mmsg_address(base, idx) {
            Ok(ptr) => ptr,
            Err(err) if received != 0 => {
                remember_recvmmsg_error(&socket, err);
                return Ok(received as isize);
            }
            Err(err) => return Err(err),
        };
        let msg = UserPtr::<mmsghdr>::from(ptr).cast::<msghdr>();
        match recvmsg_with_socket(&socket, fd, msg, active_flags, recv_flags) {
            Ok(len) => {
                if let Err(err) =
                    write_user_field(ptr, core::mem::offset_of!(mmsghdr, msg_len), len as u32)
                {
                    if received != 0 {
                        remember_recvmmsg_error(&socket, err);
                        return Ok(received as isize);
                    }
                    return Err(err);
                }
                received += 1;
                if wait_for_one && received == 1 {
                    active_flags |= MSG_DONTWAIT;
                    recv_flags |= RecvFlags::DONT_WAIT;
                }
            }
            Err(err) if received != 0 => {
                remember_recvmmsg_error(&socket, err);
                return Ok(received as isize);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(received as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmsg_vlen_matches_linux_send_cap_and_unbounded_recv_batch() {
        assert_eq!(admitted_sendmmsg_vlen(0), None);
        assert_eq!(admitted_sendmmsg_vlen(1), Some(1));
        assert_eq!(admitted_sendmmsg_vlen(u32::MAX), Some(MAX_MMSG_VLEN));
        assert_eq!(admitted_recvmmsg_vlen(0), None);
        assert_eq!(admitted_recvmmsg_vlen(1), Some(1));
        assert_eq!(admitted_recvmmsg_vlen(u32::MAX), Some(u32::MAX as usize));
    }

    #[test]
    fn recvmsg_keeps_a_per_message_iovec_cap() {
        assert!(validate_recvmsg_iovlen(MAX_RECVMSG_IOVCNT).is_ok());
        let error = validate_recvmsg_iovlen(MAX_RECVMSG_IOVCNT + 1).unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn recvmmsg_errqueue_does_not_consume_the_ordinary_pending_error() {
        assert!(recvmmsg_consumes_pending_error(0));
        assert!(!recvmmsg_consumes_pending_error(MSG_ERRQUEUE));
        assert!(!recvmmsg_consumes_pending_error(
            MSG_ERRQUEUE | MSG_DONTWAIT
        ));
    }

    #[test]
    fn partial_recvmmsg_does_not_defer_eagain() {
        assert!(!recvmmsg_defers_error(AxError::WouldBlock));
        assert!(recvmmsg_defers_error(AxError::TimedOut));
        assert!(recvmmsg_defers_error(AxError::ConnectionReset));
    }

    #[test]
    fn installed_rights_without_a_visible_header_still_report_ctrunc() {
        use super::super::cmsg::RightsPushResult;

        assert!(rights_push_was_truncated(
            2,
            RightsPushResult {
                installed: 2,
                published: false,
            }
        ));
        assert!(rights_push_was_truncated(
            2,
            RightsPushResult {
                installed: 1,
                published: true,
            }
        ));
        assert!(!rights_push_was_truncated(
            2,
            RightsPushResult {
                installed: 2,
                published: true,
            }
        ));
    }

    #[test]
    fn datagram_peer_refusal_never_raises_sigpipe() {
        assert!(!should_raise_sigpipe(AxError::ConnectionRefused, 0));
        assert!(should_raise_sigpipe(AxError::BrokenPipe, 0));
        assert!(!should_raise_sigpipe(AxError::BrokenPipe, MSG_NOSIGNAL));
    }
}
