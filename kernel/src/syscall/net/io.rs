use alloc::vec::Vec;
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
use linux_raw_sys::{
    general::timespec,
    net::{
        AF_NETLINK, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT, MSG_ERRQUEUE, MSG_NOSIGNAL,
        MSG_OOB, MSG_PEEK, MSG_TRUNC, MSG_WAITALL, cmsghdr, mmsghdr, msghdr, sockaddr, socklen_t,
    },
};
use starry_signal::{SignalInfo, Signo};
use starry_vm::{vm_read_slice, vm_write_slice};
use thekernel_linux_packet::ReceiveFlags as PacketReceiveFlags;

use super::{
    SocketSyscallSnapshot,
    addr::SocketAddrExt,
    packet::{decode_send_address, snapshot_address, write_received_address},
};
use crate::{
    file::{
        FileLike, PacketSocket, PinnedSocketDescription, PreparedSocketMessage, SocketBackendKind,
        af_alg::AfAlgSendRequest, netlink::SockaddrNl, permission::VfsSecurityContext,
    },
    mm::{
        IoVec, IoVectorBuf, UserConstPtr, UserPtr, VmBytes, VmBytesMut, check_user_readable,
        check_user_writable,
    },
    syscall::net::{CMsg, CMsgBuilder, SCM_MAX_FD},
    task::{
        security::{SocketSecurityContext, dispatch_socket},
        send_signal_to_process,
    },
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

fn remember_socket_error(socket: &PinnedSocketDescription, error: AxError) {
    if socket.backend() == Ok(SocketBackendKind::Network)
        && let Ok(socket) = socket.network()
    {
        socket.set_pending_error(LinuxError::from(error));
    }
}

fn take_pending_socket_error(socket: &PinnedSocketDescription) -> AxResult {
    if socket.backend()? != SocketBackendKind::Network {
        return Ok(());
    }
    let mut error = 0;
    socket
        .network()?
        .get_option(GetSocketOption::Error(&mut error))?;
    if error == 0 {
        return Ok(());
    }
    Err(LinuxError::try_from(error)
        .map_err(|_| AxError::BadState)?
        .into())
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

fn snapshot_user_bytes(ptr: *const u8, len: usize, limit: usize) -> AxResult<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(AxError::BadAddress);
    }
    if len > limit {
        return Err(AxError::from(LinuxError::ENOBUFS));
    }

    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(len)
        .map_err(|_| AxError::NoMemory)?;
    snapshot.resize(len, 0);
    vm_read_slice(ptr, unsafe {
        core::slice::from_raw_parts_mut(
            snapshot.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            snapshot.len(),
        )
    })?;
    Ok(snapshot)
}

fn snapshot_iov_payload(iov: IoVectorBuf) -> AxResult<Vec<u8>> {
    let len = iov.len();
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(len)
        .map_err(|_| AxError::NoMemory)?;
    payload.resize(len, 0);
    let read = iov.into_io().read(&mut payload)?;
    if read != len {
        return Err(AxError::BadState);
    }
    Ok(payload)
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

#[derive(Clone, Copy, Debug)]
struct ValidatedRecvFlags {
    raw: u32,
    generic: RecvFlags,
    defer_packet_mechanism: bool,
}

impl ValidatedRecvFlags {
    fn packet_flags(self) -> AxResult<PacketReceiveFlags> {
        debug_assert!(self.defer_packet_mechanism);
        // These are valid generic recvmsg flag bits, but AF_PACKET rejects or
        // short-circuits them in its protocol receive operation. Keep that
        // mechanism decision after security_socket_recvmsg.
        if self.raw & !SUPPORTED_RECVMSG_FLAGS != 0 {
            return Err(AxError::InvalidInput);
        }
        if self.raw & MSG_OOB != 0 {
            return Err(AxError::InvalidInput);
        }
        if self.raw & MSG_ERRQUEUE != 0 {
            return Err(LinuxError::EAGAIN.into());
        }
        if self.raw & MSG_WAITALL != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut bits = 0;
        if self.generic.contains(RecvFlags::PEEK) {
            bits |= MSG_PEEK;
        }
        if self.generic.contains(RecvFlags::TRUNCATE) {
            bits |= MSG_TRUNC;
        }
        PacketReceiveFlags::from_bits(bits).map_err(crate::file::packet_socket::packet_error)
    }

    fn insert_dont_wait(&mut self) {
        self.raw |= MSG_DONTWAIT;
        self.generic |= RecvFlags::DONT_WAIT;
    }
}

fn validate_recvmsg_flags(
    flags: u32,
    defer_packet_mechanism: bool,
) -> AxResult<ValidatedRecvFlags> {
    if !defer_packet_mechanism && flags & !SUPPORTED_RECVMSG_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if !defer_packet_mechanism && flags & MSG_OOB != 0 {
        return Err(AxError::InvalidInput);
    }
    if !defer_packet_mechanism && flags & MSG_ERRQUEUE != 0 {
        return Err(AxError::from(LinuxError::EAGAIN));
    }
    if !defer_packet_mechanism && flags & MSG_WAITALL != 0 {
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
    Ok(ValidatedRecvFlags {
        raw: flags,
        generic: recv_flags,
        defer_packet_mechanism,
    })
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

fn raise_sigpipe(process: u32) {
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

fn remember_recvmmsg_error(socket: &PinnedSocketDescription, error: AxError) {
    if !recvmmsg_defers_error(error) {
        return;
    }
    remember_socket_error(socket, error);
}

const fn rights_push_was_truncated(expected: usize, result: super::cmsg::RightsPushResult) -> bool {
    result.installed < expected || !result.published
}

fn should_raise_sigpipe(error: AxError, flags: u32) -> bool {
    error == AxError::BrokenPipe && flags & MSG_NOSIGNAL == 0
}

const fn effective_message_flags(flags: u32, nonblocking: bool) -> u32 {
    if nonblocking {
        flags | MSG_DONTWAIT
    } else {
        flags
    }
}

/// Completes the AF_PACKET mechanism phase after policy admission.
///
/// The lower-device/MTU plan is prepared before allocation or payload copy.
/// This is the ordering Linux's `packet_snd` relies on: an invalid explicit
/// interface remains `ENXIO` even when the payload mapping would later fault.
fn send_packet_after_security(
    socket: &PacketSocket,
    mut src: impl Read + IoBuf,
    destination: Option<thekernel_linux_packet::PacketSendAddress>,
    ancillary_items: usize,
) -> AxResult<usize> {
    let len = src.remaining();
    let plan = socket.prepare_send(len, destination)?;
    if ancillary_items != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(len)
        .map_err(|_| AxError::NoMemory)?;
    payload.resize(len, 0);
    src.read_exact(&mut payload)?;
    socket.send_prepared(plan, &payload)
}

fn send_impl(
    socket: &PinnedSocketDescription,
    snapshot: &SocketSyscallSnapshot,
    fd: i32,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    cmsg: Vec<CMsgData>,
    packet_control_length: usize,
    iov_count: usize,
    control_length: usize,
) -> AxResult<isize> {
    let backend = socket.backend()?;
    let send_flags = if backend == SocketBackendKind::Packet {
        None
    } else {
        Some(validate_sendmsg_flags(flags)?)
    };
    let (network_addr, packet_address) = match backend {
        SocketBackendKind::Network if !addr.is_null() && addrlen != 0 => {
            (Some(SocketAddrEx::read_from_user(addr, addrlen)?), None)
        }
        SocketBackendKind::Packet if !addr.is_null() => {
            (None, Some(snapshot_address(addr, addrlen)?))
        }
        _ => (None, None),
    };
    let mut message = PreparedSocketMessage::new(
        flags,
        iov_count,
        if network_addr.is_some() || packet_address.is_some() {
            addrlen as usize
        } else {
            0
        },
        control_length,
        cmsg.len(),
    );
    if let Some(address) = packet_address.clone() {
        message = message.with_packet_address(address);
    }
    let socket_ref = socket.security_ref()?;
    dispatch_socket(&SocketSecurityContext::send_message(
        snapshot.actor(),
        &socket_ref,
        &message,
        src.remaining(),
    ))?;

    if backend == SocketBackendKind::Netlink {
        let socket = socket.netlink()?;
        debug!("sys_send <= fd: {fd}, flags: {flags}, netlink");
        if !cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        return socket.write(&mut src).map(|sent| sent as isize);
    }

    if backend == SocketBackendKind::Packet {
        // AF_PACKET flag interpretation and ancillary support are protocol
        // mechanism decisions. The raw flags/control length were visible to
        // policy above; reject them only after that hook.
        validate_sendmsg_flags(flags)?;
        let destination = packet_address
            .as_ref()
            .map(decode_send_address)
            .transpose()?;
        debug!("sys_send <= fd: {fd}, flags: {flags}, packet: {destination:?}");
        let ancillary_items = usize::from(packet_control_length != 0 || !cmsg.is_empty());
        return send_packet_after_security(socket.packet()?, src, destination, ancillary_items)
            .map(|sent| sent as isize);
    }

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {network_addr:?}");

    if backend != SocketBackendKind::Network {
        return Err(AxError::NotASocket);
    }
    let nonblocking = socket.nonblocking();
    let socket = socket.network()?;
    let options = SendOptions {
        to: network_addr,
        flags: send_flags.ok_or(AxError::BadState)?,
        cmsg,
        nonblocking_override: Some(nonblocking),
    };
    let sent = match &socket.inner {
        AxSocket::Unix(unix) if unix.is_datagram() => {
            let reservation = match options.to.as_ref() {
                Some(SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
                    let security = VfsSecurityContext::new(snapshot.actor().clone());
                    let target = crate::file::unix_socket::resolve_peer(path.clone(), &security)?;
                    unix.prepare_send_to_resolved(options, target)?
                }
                _ => unix.prepare_may_send(options)?,
            };
            let receiving = crate::file::UnixEndpointSecurityRef::new(
                reservation.receiving_identity(),
                socket.net_namespace(),
                &reservation,
            );
            dispatch_socket(&SocketSecurityContext::unix_may_send(
                snapshot.actor(),
                &socket_ref,
                &receiving,
            ))?;
            reservation.commit(&mut src)
        }
        _ => socket.send(&mut src, options),
    };
    let sent = match sent {
        Err(error) => {
            if should_raise_sigpipe(error, flags) {
                raise_sigpipe(snapshot.pid());
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
    let snapshot = SocketSyscallSnapshot::capture();
    let payload_admission = if len == 0 {
        Ok(())
    } else {
        check_user_readable(buf as usize, len)
    };
    // Preserve the established eager-prefault errno order for every existing
    // backend while deferring only AF_PACKET payload faults until after its
    // security hook and device/MTU plan. The speculative fd classification is
    // read-only; if it fails after an eager fault, the legacy fault still wins.
    let socket = match payload_admission {
        Ok(()) => PinnedSocketDescription::from_fd(fd)?,
        Err(payload_error) => match PinnedSocketDescription::from_fd(fd) {
            Ok(socket) if socket.backend()? == SocketBackendKind::Packet => socket,
            _ => return Err(payload_error.into()),
        },
    };
    let flags = effective_message_flags(flags, socket.nonblocking());
    send_impl(
        &socket,
        &snapshot,
        fd,
        VmBytes::new(buf, len),
        flags,
        addr,
        addrlen,
        Vec::new(),
        0,
        1,
        0,
    )
}

struct ImportedAfAlgSend {
    request: AfAlgSendRequest,
    message: PreparedSocketMessage,
}

impl ImportedAfAlgSend {
    fn import(msg: &msghdr, flags: u32) -> AxResult<Self> {
        let send_iov = IoVectorBuf::new(msg.msg_iov.cast::<IoVec>(), msg.msg_iovlen)?;
        send_iov.check_readable()?;
        let iov_count = send_iov.iovcnt();
        let control = snapshot_user_bytes(
            msg.msg_control.cast::<u8>(),
            msg.msg_controllen,
            MAX_SENDMSG_CONTROL_LEN,
        )?;
        let payload = snapshot_iov_payload(send_iov)?;
        let request = AfAlgSendRequest::prepare(
            payload,
            &control,
            !msg.msg_name.is_null() || msg.msg_namelen != 0,
        )?;
        let message = PreparedSocketMessage::new(
            flags,
            iov_count,
            if msg.msg_name.is_null() {
                0
            } else {
                msg.msg_namelen as usize
            },
            msg.msg_controllen,
            request.ancillary_items(),
        );
        Ok(Self { request, message })
    }

    fn payload_len(&self) -> usize {
        self.request.payload_len()
    }
}

fn import_send_after_socket_hook<I, O>(
    import: impl FnOnce() -> AxResult<I>,
    authorize: impl FnOnce(&I) -> AxResult<()>,
    send: impl FnOnce(I) -> AxResult<O>,
) -> AxResult<O> {
    let imported = import()?;
    authorize(&imported)?;
    send(imported)
}

fn sendmsg_with_socket(
    socket: &PinnedSocketDescription,
    snapshot: &SocketSyscallSnapshot,
    fd: i32,
    msg: UserConstPtr<msghdr>,
    flags: u32,
) -> AxResult<isize> {
    let msg = read_user_copy(msg)?;
    if socket.backend()? == SocketBackendKind::AfAlg {
        let af_alg = socket.af_alg()?;
        debug!("sys_sendmsg <= fd: {fd}, flags: {flags}, af_alg");
        if flags & !MSG_DONTWAIT != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let policy_socket = socket.security_ref()?;
        return import_send_after_socket_hook(
            || ImportedAfAlgSend::import(&msg, flags),
            |imported| {
                dispatch_socket(&SocketSecurityContext::send_message(
                    snapshot.actor(),
                    &policy_socket,
                    &imported.message,
                    imported.payload_len(),
                ))
            },
            |imported| {
                af_alg
                    .send_prepared(imported.request)
                    .map(|sent| sent as isize)
            },
        );
    }

    let send_iov = IoVectorBuf::new(msg.msg_iov.cast::<IoVec>(), msg.msg_iovlen)?;
    if socket.backend()? != SocketBackendKind::Packet {
        send_iov.check_readable()?;
    }
    let (cmsg, packet_control_length) = if socket.backend()? == SocketBackendKind::Packet {
        // Copy the bounded generic control buffer once, but defer semantic
        // cmsg parsing/support to the AF_PACKET mechanism phase after policy.
        let control = snapshot_user_bytes(
            msg.msg_control.cast::<u8>(),
            msg.msg_controllen,
            MAX_SENDMSG_CONTROL_LEN,
        )?;
        (Vec::new(), control.len())
    } else {
        (parse_send_control(&msg)?, 0)
    };

    send_impl(
        socket,
        snapshot,
        fd,
        send_iov.into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        cmsg,
        packet_control_length,
        msg.msg_iovlen,
        msg.msg_controllen,
    )
}

pub fn sys_sendmsg(fd: i32, msg: UserConstPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    sendmsg_with_socket(&socket, &snapshot, fd, msg, flags)
}

enum ReceivedSocketAddress {
    Network(SocketAddrEx),
    NetlinkKernel,
    Packet(thekernel_linux_packet::SockAddrLl),
}

impl ReceivedSocketAddress {
    fn write_to_user(self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        match self {
            Self::Network(addr_value) => addr_value.write_to_user(addr, addrlen),
            Self::NetlinkKernel => {
                let addr_value = SockaddrNl {
                    nl_family: AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: 0,
                    nl_groups: 0,
                };
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&addr_value as *const SockaddrNl).cast::<u8>(),
                        size_of::<SockaddrNl>(),
                    )
                };
                let copy_len = (*addrlen as usize).min(bytes.len());
                if copy_len != 0 {
                    vm_write_slice(addr.address().as_usize() as *mut u8, &bytes[..copy_len])?;
                }
                *addrlen = bytes.len() as _;
                Ok(())
            }
            Self::Packet(address) => write_received_address(address, addr, addrlen),
        }
    }
}

struct ReceiveOutcome {
    returned_len: isize,
    message_truncated: bool,
    control_truncated: bool,
    address: Option<ReceivedSocketAddress>,
}

fn recv_impl(
    socket: &PinnedSocketDescription,
    fd: i32,
    mut dst: impl Write + IoBufMut,
    recv_flags: ValidatedRecvFlags,
    want_address: bool,
    cmsg_builder: Option<CMsgBuilder>,
    cloexec_rights: bool,
) -> AxResult<ReceiveOutcome> {
    debug!("sys_recv <= fd: {fd}, flags: {recv_flags:?}");

    if socket.backend()? == SocketBackendKind::Netlink {
        let nonblocking = socket.nonblocking();
        let netlink = socket.netlink()?;
        let recv = netlink.recv_with_nonblocking(&mut dst, recv_flags.generic, nonblocking)?;
        debug!("sys_recv => fd: {fd}, netlink recv: {recv}");
        return Ok(ReceiveOutcome {
            returned_len: recv as isize,
            message_truncated: false,
            control_truncated: false,
            address: want_address.then_some(ReceivedSocketAddress::NetlinkKernel),
        });
    }

    if socket.backend()? == SocketBackendKind::Packet {
        let packet_flags = recv_flags.packet_flags()?;
        let result = socket.packet()?.recv_with_nonblocking(
            &mut dst,
            packet_flags,
            recv_flags.generic.contains(RecvFlags::DONT_WAIT),
        )?;
        return Ok(ReceiveOutcome {
            returned_len: result.returned_len() as isize,
            message_truncated: result.message_truncated(),
            control_truncated: false,
            address: want_address.then_some(ReceivedSocketAddress::Packet(result.address())),
        });
    }

    if socket.backend()? != SocketBackendKind::Network {
        return Err(AxError::NotASocket);
    }
    let nonblocking = socket.nonblocking();
    let socket = socket.network()?;
    let mut cmsg = Vec::new();

    let mut remote_addr = want_address.then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let recv = socket.recv(
        &mut dst,
        RecvOptions {
            from: remote_addr.as_mut(),
            flags: recv_flags.generic,
            cmsg: Some(&mut cmsg),
            nonblocking_override: Some(nonblocking),
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

    debug!("sys_recv => fd: {fd}, recv: {recv}");
    Ok(ReceiveOutcome {
        returned_len: recv as isize,
        message_truncated: false,
        control_truncated,
        address: remote_addr.map(ReceivedSocketAddress::Network),
    })
}

fn dispatch_receive_message(
    socket: &PinnedSocketDescription,
    snapshot: &SocketSyscallSnapshot,
    message: &PreparedSocketMessage,
    size: usize,
    flags: u32,
) -> AxResult<()> {
    let socket_ref = socket.security_ref()?;
    dispatch_socket(&SocketSecurityContext::receive_message(
        snapshot.actor(),
        &socket_ref,
        message,
        size,
        flags as i32,
    ))
}

fn receive_after_socket_hook<I, O>(
    imported: I,
    authorize: impl FnOnce(&I) -> AxResult<()>,
    receive: impl FnOnce(I) -> AxResult<O>,
) -> AxResult<O> {
    authorize(&imported)?;
    receive(imported)
}

fn receive_socket_output_after_policy<T, O>(
    authorize: impl FnOnce() -> AxResult<()>,
    receive: impl FnOnce() -> AxResult<T>,
    export_output: impl FnOnce(T) -> AxResult<O>,
) -> AxResult<O> {
    authorize()?;
    export_output(receive()?)
}

fn recvfrom_security_message(flags: u32) -> PreparedSocketMessage {
    // Linux builds recvfrom's kernel msghdr with only msg_name initialized.
    // The output capacity is not imported until move_addr_to_user after recv.
    PreparedSocketMessage::new(flags, 1, 0, 0, 0)
}

fn recvmsg_security_message(
    flags: u32,
    iov_count: usize,
    control_length: usize,
) -> PreparedSocketMessage {
    // Linux imports the user msghdr but resets msg_namelen before the receive
    // hook. The original capacity remains syscall copyout state, not policy.
    PreparedSocketMessage::new(flags, iov_count, 0, control_length, 0)
}

pub fn sys_recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: u32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    // Linux packet receive claims an ordinary skb before payload copy.  Do
    // not pre-fault packet destinations: a later EFAULT must consume ordinary
    // receive while MSG_PEEK retains it. Other backends keep their established
    // eager-writable admission.
    if len != 0 && socket.backend()? != SocketBackendKind::Packet {
        check_user_writable(buf as usize, len)?;
    }
    let flags = effective_message_flags(flags, socket.nonblocking());
    let recv_flags = validate_recvmsg_flags(flags, socket.backend()? == SocketBackendKind::Packet)?;
    let message = recvfrom_security_message(flags);
    receive_socket_output_after_policy(
        || dispatch_receive_message(&socket, &snapshot, &message, len, flags),
        || {
            recv_impl(
                &socket,
                fd,
                VmBytesMut::new(buf, len),
                recv_flags,
                !addr.is_null(),
                None,
                flags & MSG_CMSG_CLOEXEC != 0,
            )
        },
        |outcome| {
            if let Some(remote_addr) = outcome.address {
                let mut user_addrlen = read_user_copy(UserConstPtr::<socklen_t>::from(
                    addrlen.address().as_usize(),
                ))?;
                remote_addr.write_to_user(addr, &mut user_addrlen)?;
                write_user_copy(addrlen, user_addrlen)?;
            }
            Ok(outcome.returned_len)
        },
    )
}

struct ImportedRecvMessage {
    user: UserPtr<msghdr>,
    header: msghdr,
    iov: IoVectorBuf,
}

impl ImportedRecvMessage {
    fn import(user: UserPtr<msghdr>, defer_payload_fault: bool) -> AxResult<Self> {
        let msg_hdr = read_user_copy(UserConstPtr::<msghdr>::from(user.address().as_usize()))?;
        if (msg_hdr.msg_namelen as i32) < 0 || (msg_hdr.msg_controllen as isize) < 0 {
            return Err(AxError::InvalidInput);
        }
        validate_recvmsg_iovlen(msg_hdr.msg_iovlen)?;

        let recv_iov = IoVectorBuf::new(msg_hdr.msg_iov.cast::<IoVec>(), msg_hdr.msg_iovlen)?;
        if !defer_payload_fault {
            recv_iov.check_writable()?;
        }
        Ok(Self {
            user,
            header: msg_hdr,
            iov: recv_iov,
        })
    }

    fn security_message(&self, flags: u32) -> PreparedSocketMessage {
        recvmsg_security_message(
            flags,
            self.iov.iovcnt(),
            if self.header.msg_control.is_null() {
                0
            } else {
                self.header.msg_controllen
            },
        )
    }

    fn payload_capacity(&self) -> usize {
        self.iov.len()
    }
}

fn recvmsg_imported(
    socket: &PinnedSocketDescription,
    fd: i32,
    imported: ImportedRecvMessage,
    flags: u32,
    recv_flags: ValidatedRecvFlags,
) -> AxResult<isize> {
    let ImportedRecvMessage {
        user: msg,
        header: mut msg_hdr,
        iov: recv_iov,
    } = imported;

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
    let recv = recv_impl(
        socket,
        fd,
        recv_iov.into_io(),
        recv_flags,
        !msg_hdr.msg_name.is_null(),
        control,
        flags & MSG_CMSG_CLOEXEC != 0,
    )?;
    // Ancillary fd numbers are a Linux publication point. `recv_impl` handles
    // those first, so an invalid msg_name preserves already exposed fds.
    if let Some(remote_addr) = recv.address {
        remote_addr.write_to_user(UserPtr::from(msg_hdr.msg_name as usize), &mut name_len)?;
    }
    if recv.control_truncated {
        msg_hdr.msg_flags |= MSG_CTRUNC;
    }
    if recv.message_truncated {
        msg_hdr.msg_flags |= MSG_TRUNC;
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
    Ok(recv.returned_len)
}

pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    let recv_flags = validate_recvmsg_flags(flags, socket.backend()? == SocketBackendKind::Packet)?;
    let imported =
        ImportedRecvMessage::import(msg, socket.backend()? == SocketBackendKind::Packet)?;
    receive_after_socket_hook(
        imported,
        |imported| {
            let message = imported.security_message(flags);
            dispatch_receive_message(
                &socket,
                &snapshot,
                &message,
                imported.payload_capacity(),
                flags,
            )
        },
        |imported| recvmsg_imported(&socket, fd, imported, flags, recv_flags),
    )
}

pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> AxResult<isize> {
    // Linux validates and pins the socket even when there are no elements; a
    // zero vlen only suppresses access to msgvec itself.
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
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
        match sendmsg_with_socket(&socket, &snapshot, fd, msg, flags) {
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
    let snapshot = SocketSyscallSnapshot::capture();
    // The timeout object is imported before Linux enters do_recvmmsg(), even
    // for vlen zero. Pin the endpoint next, then skip only msgvec processing.
    let has_timeout = recvmmsg_has_timeout(timeout)?;
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let Some(vlen) = admitted_recvmmsg_vlen(vlen) else {
        // A zero-length batch has no receive attempt and therefore no receive
        // security hook. Preserve the existing one-shot socket-error behavior.
        if recvmmsg_consumes_pending_error(flags) {
            take_pending_socket_error(&socket)?;
        }
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
    let mut active_flags = effective_message_flags(flags & !MSG_WAITFORONE, socket.nonblocking());
    let mut recv_flags =
        validate_recvmsg_flags(active_flags, socket.backend()? == SocketBackendKind::Packet)?;
    let mut received = 0usize;
    let defer_payload_fault = socket.backend()? == SocketBackendKind::Packet;
    let base = msgvec.address().as_usize();
    let first_ptr = mmsg_address(base, 0)?;
    let first_msg = UserPtr::<mmsghdr>::from(first_ptr).cast::<msghdr>();
    let first_imported = ImportedRecvMessage::import(first_msg, defer_payload_fault)?;
    let message = first_imported.security_message(active_flags);
    dispatch_receive_message(
        &socket,
        &snapshot,
        &message,
        first_imported.payload_capacity(),
        active_flags,
    )?;
    // The security hook observes the first concrete receive attempt before a
    // transport reports and clears its deferred error. A denial therefore
    // leaves that error available to a later admitted call.
    if recvmmsg_consumes_pending_error(flags) {
        take_pending_socket_error(&socket)?;
    }
    let mut first_imported = Some(first_imported);
    for idx in 0..vlen {
        let ptr = match mmsg_address(base, idx) {
            Ok(ptr) => ptr,
            Err(err) if received != 0 => {
                remember_recvmmsg_error(&socket, err);
                return Ok(received as isize);
            }
            Err(err) => return Err(err),
        };
        let imported = if idx == 0 {
            first_imported
                .take()
                .expect("first recvmmsg element was imported before dispatch")
        } else {
            let msg = UserPtr::<mmsghdr>::from(ptr).cast::<msghdr>();
            match ImportedRecvMessage::import(msg, defer_payload_fault) {
                Ok(imported) => imported,
                Err(err) if received != 0 => {
                    remember_recvmmsg_error(&socket, err);
                    return Ok(received as isize);
                }
                Err(err) => return Err(err),
            }
        };
        let receive = if idx == 0 {
            recvmsg_imported(&socket, fd, imported, active_flags, recv_flags)
        } else {
            receive_after_socket_hook(
                imported,
                |imported| {
                    let message = imported.security_message(active_flags);
                    dispatch_receive_message(
                        &socket,
                        &snapshot,
                        &message,
                        imported.payload_capacity(),
                        active_flags,
                    )
                },
                |imported| recvmsg_imported(&socket, fd, imported, active_flags, recv_flags),
            )
        };
        match receive {
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
                    recv_flags.insert_dont_wait();
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

    struct FaultingPayload<'a> {
        reads: &'a core::cell::Cell<usize>,
        len: usize,
    }

    impl Read for FaultingPayload<'_> {
        fn read(&mut self, _output: &mut [u8]) -> AxResult<usize> {
            self.reads.set(self.reads.get() + 1);
            Err(AxError::BadAddress)
        }
    }

    impl IoBuf for FaultingPayload<'_> {
        fn remaining(&self) -> usize {
            self.len
        }
    }

    #[test]
    fn packet_invalid_device_precedes_a_faulting_payload_copy() {
        use core::cell::Cell;

        let _context = crate::file::packet_socket::packet_test_context();
        let user_namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let net_namespace =
            crate::task::NetworkNamespace::try_new_loopback_only(user_namespace).unwrap();
        let socket = PacketSocket::try_new(
            thekernel_linux_packet::PacketSocketType::Datagram,
            thekernel_linux_packet::ProtocolSelector::Disabled,
            net_namespace,
        )
        .unwrap();
        let invalid = thekernel_linux_packet::PacketSendAddress::try_from_network_order_fields(
            0x0800_u16.to_be(),
            999,
            6,
            [0; 8],
        )
        .unwrap();
        let reads = Cell::new(0);
        assert_eq!(
            send_packet_after_security(
                &socket,
                FaultingPayload {
                    reads: &reads,
                    len: 20,
                },
                Some(invalid),
                0,
            ),
            Err(LinuxError::ENXIO.into())
        );
        assert_eq!(reads.get(), 0);

        let valid = thekernel_linux_packet::PacketSendAddress::try_from_network_order_fields(
            0x0800_u16.to_be(),
            1,
            6,
            [0; 8],
        )
        .unwrap();
        assert_eq!(
            send_packet_after_security(
                &socket,
                FaultingPayload {
                    reads: &reads,
                    len: 20,
                },
                Some(valid),
                0,
            ),
            Err(AxError::BadAddress)
        );
        assert_eq!(reads.get(), 1);
    }

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
    fn packet_receive_mechanism_flags_are_rejected_only_after_policy_stage() {
        for (flag, expected) in [
            (MSG_OOB, LinuxError::EINVAL),
            (MSG_ERRQUEUE, LinuxError::EAGAIN),
            (MSG_WAITALL, LinuxError::EINVAL),
            (1_u32 << 31, LinuxError::EINVAL),
        ] {
            assert!(validate_recvmsg_flags(flag, false).is_err());
            let deferred = validate_recvmsg_flags(flag, true).unwrap();
            assert_eq!(
                deferred.packet_flags().map_err(LinuxError::from),
                Err(expected)
            );
        }
    }

    #[test]
    fn recvmsg_keeps_a_per_message_iovec_cap() {
        assert!(validate_recvmsg_iovlen(MAX_RECVMSG_IOVCNT).is_ok());
        let error = validate_recvmsg_iovlen(MAX_RECVMSG_IOVCNT + 1).unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn nonblocking_ofd_adds_dontwait_to_the_hook_and_backend_view() {
        assert_eq!(
            effective_message_flags(MSG_PEEK, true),
            MSG_PEEK | MSG_DONTWAIT
        );
    }

    #[test]
    fn explicit_dontwait_is_preserved_without_duplication() {
        assert_eq!(
            effective_message_flags(MSG_PEEK | MSG_DONTWAIT, false),
            MSG_PEEK | MSG_DONTWAIT
        );
        assert_eq!(
            effective_message_flags(MSG_PEEK | MSG_DONTWAIT, true),
            MSG_PEEK | MSG_DONTWAIT
        );
    }

    #[test]
    fn bad_af_alg_control_copy_precedes_policy_denial() {
        use core::cell::Cell;

        let hook_calls = Cell::new(0);
        let result = import_send_after_socket_hook(
            || Err::<AfAlgSendRequest, _>(AxError::BadAddress),
            |_: &AfAlgSendRequest| {
                hook_calls.set(hook_calls.get() + 1);
                Err(AxError::PermissionDenied)
            },
            |request| Ok(request.payload_len()),
        );

        assert_eq!(result, Err(AxError::BadAddress));
        assert_eq!(hook_calls.get(), 0);
    }

    #[test]
    fn af_alg_hook_and_commit_share_one_owned_payload() {
        use core::cell::{Cell, RefCell};

        let source = RefCell::new(alloc::vec![1_u8, 2, 3, 4]);
        let hook_size = Cell::new(0);
        let result = import_send_after_socket_hook(
            || AfAlgSendRequest::prepare(source.borrow().clone(), &[], false),
            |request| {
                hook_size.set(request.payload_len());
                source.borrow_mut().fill(9);
                Ok(())
            },
            |request| {
                assert_eq!(request.payload(), &[1, 2, 3, 4]);
                Ok(request.payload_len())
            },
        );

        assert_eq!(hook_size.get(), 4);
        assert_eq!(result, Ok(4));
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
    fn batch_receive_denial_stops_before_the_denied_element_is_consumed() {
        use core::cell::Cell;

        let hook_calls = Cell::new(0);
        let consumed = Cell::new(0);
        for element in 0..3 {
            let result = receive_after_socket_hook(
                element,
                |element| {
                    hook_calls.set(hook_calls.get() + 1);
                    if *element == 1 {
                        Err(AxError::PermissionDenied)
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    consumed.set(consumed.get() + 1);
                    Ok(())
                },
            );
            if result.is_err() {
                break;
            }
        }

        assert_eq!(hook_calls.get(), 2);
        assert_eq!(consumed.get(), 1);
    }

    #[test]
    fn denied_recvfrom_policy_does_not_consume_or_import_a_bad_output_length() {
        use core::cell::Cell;

        let consumed = Cell::new(0);
        let output_imports = Cell::new(0);
        let result = receive_socket_output_after_policy(
            || Err(AxError::PermissionDenied),
            || {
                consumed.set(consumed.get() + 1);
                Ok(())
            },
            |_| {
                output_imports.set(output_imports.get() + 1);
                Err::<(), _>(AxError::BadAddress)
            },
        );

        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(consumed.get(), 0);
        assert_eq!(output_imports.get(), 0);
    }

    #[test]
    fn admitted_recvfrom_consumes_before_output_import_and_writeback() {
        use core::cell::Cell;

        let stage = Cell::new(0);
        let result = receive_socket_output_after_policy(
            || {
                assert_eq!(stage.replace(1), 0);
                Ok(())
            },
            || {
                assert_eq!(stage.replace(2), 1);
                Ok(17usize)
            },
            |received| {
                assert_eq!(stage.replace(3), 2); // Import output capacity.
                assert_eq!(stage.replace(4), 3); // Write address and true length.
                Ok(received)
            },
        );

        assert_eq!(result, Ok(17));
        assert_eq!(stage.get(), 4);
    }

    #[test]
    fn recvfrom_policy_does_not_observe_the_unimported_output_capacity() {
        let message = recvfrom_security_message(MSG_DONTWAIT);

        assert_eq!(message.flags(), MSG_DONTWAIT);
        assert_eq!(message.iov_count(), 1);
        assert_eq!(message.name_length(), 0);
    }

    #[test]
    fn recvmsg_policy_does_not_observe_the_copyout_name_capacity() {
        let message = recvmsg_security_message(MSG_PEEK, 3, 128);

        assert_eq!(message.flags(), MSG_PEEK);
        assert_eq!(message.iov_count(), 3);
        assert_eq!(message.name_length(), 0);
        assert_eq!(message.control_length(), 128);
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
