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
    options::{Configurable, GetSocketOption, SocketFault},
    sctp::{SctpRecvMetadata, SctpSendMetadata},
    unix::UnixSocketAddr,
};
use linux_raw_sys::{
    general::timespec,
    net::{
        AF_NETLINK, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT, MSG_EOR, MSG_ERRQUEUE,
        MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_TRUNC, MSG_WAITALL, cmsghdr, mmsghdr, msghdr,
        sockaddr, socklen_t,
    },
};
use memory_addr::PAGE_SIZE_4K;
use thekernel_linux_net::{PendingErrorPolicy, SocketWaitKind, plan_pending_error};
use thekernel_linux_packet::ReceiveFlags as PacketReceiveFlags;
use thekernel_linux_signal::{SignalInfo, Signo};

use super::{
    SocketSyscallSnapshot,
    addr::SocketAddrExt,
    packet::{decode_send_address, snapshot_address, write_received_address},
    socket::{map_socket_send_error, socket_failure, validate_network_address},
};
use crate::{
    file::{
        PacketSocket, PinnedSocketDescription, PreparedSocketMessage, SocketBackendKind, WriteBuf,
        af_alg::AfAlgSendRequest, netlink::SockaddrNl, permission::VfsSecurityContext,
    },
    mm::{
        IoVec, IoVectorBuf, IoVectorBufIo, UserConstPtr, UserMemoryCapability, UserPtr,
        map_usercopy_error,
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
const SOL_SCTP: u32 = 132;
const SOL_DCCP: u32 = 269;
const DCCP_SCM_PRIORITY: u32 = 1;
const SCTP_SNDRCV: u32 = 1;
const SCTP_SNDINFO: u32 = 2;
const SCTP_RCVINFO: u32 = 3;
const SCTP_NXTINFO: u32 = 4;
const SCTP_PRINFO: u32 = 5;
const SUPPORTED_RECVMSG_FLAGS: u32 =
    MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT | MSG_WAITALL | MSG_CMSG_CLOEXEC | MSG_ERRQUEUE | MSG_OOB;
const SUPPORTED_SENDMSG_FLAGS: u32 = MSG_DONTWAIT | MSG_NOSIGNAL | MSG_OOB;

fn remember_socket_error(socket: &PinnedSocketDescription, error: AxError) {
    if socket.backend() == Ok(SocketBackendKind::Network)
        && let Ok(socket) = socket.network()
    {
        socket.set_pending_error(SocketFault::from_ax_error(error));
    }
}

fn take_pending_socket_error(socket: &PinnedSocketDescription) -> AxResult {
    if socket.backend()? != SocketBackendKind::Network {
        return Ok(());
    }
    let mut error = None;
    socket
        .network()?
        .get_option(GetSocketOption::Error(&mut error))?;
    let Some(error) = error else {
        return Ok(());
    };
    let failure = match error {
        SocketFault::ConnectionRefused => thekernel_linux_net::SocketFailure::ConnectionRefused,
        SocketFault::ConnectionReset => thekernel_linux_net::SocketFailure::ConnectionReset,
        SocketFault::TimedOut => thekernel_linux_net::SocketFailure::TimedOut,
        SocketFault::Other => thekernel_linux_net::SocketFailure::Io,
    };
    Err(socket_failure(failure))
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
    matches!(
        plan_pending_error(SocketWaitKind::Receive),
        PendingErrorPolicy::ConsumeBeforeAttempt
    ) && flags & MSG_ERRQUEUE == 0
}

fn read_user_copy<T: Copy>(capability: &UserMemoryCapability, ptr: UserConstPtr<T>) -> AxResult<T> {
    capability
        .read_value_uninit(ptr.address().as_usize() as *const T)
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })
}

fn snapshot_user_bytes(
    capability: &UserMemoryCapability,
    ptr: *const u8,
    len: usize,
    limit: usize,
) -> AxResult<Vec<u8>> {
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
    capability
        .read_slice(ptr, unsafe {
            core::slice::from_raw_parts_mut(
                snapshot.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                snapshot.len(),
            )
        })
        .map_err(map_usercopy_error)?;
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

fn write_user_copy<T: Copy>(
    capability: &UserMemoryCapability,
    ptr: UserPtr<T>,
    value: T,
) -> AxResult {
    capability
        .write_bytes(ptr.address().as_usize(), unsafe {
            core::slice::from_raw_parts((&value as *const T).cast::<u8>(), size_of::<T>())
        })
        .map_err(map_usercopy_error)
}

fn write_user_field<T: Copy>(
    capability: &UserMemoryCapability,
    base: usize,
    offset: usize,
    value: T,
) -> AxResult {
    let address = base.checked_add(offset).ok_or(AxError::BadAddress)?;
    write_user_copy(capability, UserPtr::<T>::from(address), value)
}

/// A userspace byte source that reports a successfully copied page prefix as
/// ordinary I/O progress. The generic VM buffer performs one all-or-nothing
/// range access; that is correct for snapshots but would make a TCP send lose
/// bytes that preceded a later unmapped page.
struct ProgressiveVmBytes {
    capability: UserMemoryCapability,
    ptr: usize,
    len: usize,
}

impl ProgressiveVmBytes {
    fn new(capability: UserMemoryCapability, ptr: *const u8, len: usize) -> Self {
        Self {
            capability,
            ptr: ptr as usize,
            len,
        }
    }

    fn advance(&mut self, count: usize) {
        self.ptr = self.ptr.wrapping_add(count);
        self.len -= count;
    }
}

impl Read for ProgressiveVmBytes {
    fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
        let target = self.len.min(output.len());
        let mut copied = 0;
        while copied < target {
            let address = match self.ptr.checked_add(copied) {
                Some(address) => address,
                None => {
                    self.advance(copied);
                    return if copied != 0 {
                        Ok(copied)
                    } else {
                        Err(AxError::BadAddress)
                    };
                }
            };
            let page_offset = address & (PAGE_SIZE_4K - 1);
            let chunk = (target - copied).min(PAGE_SIZE_4K - page_offset);
            let destination = unsafe {
                core::slice::from_raw_parts_mut(
                    output[copied..copied + chunk]
                        .as_mut_ptr()
                        .cast::<MaybeUninit<u8>>(),
                    chunk,
                )
            };
            if let Err(error) = self
                .capability
                .read_slice(address as *const u8, destination)
                .map_err(map_usercopy_error)
            {
                self.advance(copied);
                return if copied != 0 { Ok(copied) } else { Err(error) };
            }
            copied += chunk;
        }
        self.advance(copied);
        Ok(copied)
    }
}

impl IoBuf for ProgressiveVmBytes {
    fn remaining(&self) -> usize {
        self.len
    }
}

/// A userspace destination with page-granular progress. Stream receives use
/// the short-count behavior, while datagram callers set `strict_fault` so a
/// later copyout fault remains an error after the datagram has been consumed.
struct ProgressiveVmBytesMut {
    capability: UserMemoryCapability,
    ptr: usize,
    len: usize,
    strict_fault: bool,
}

impl ProgressiveVmBytesMut {
    fn new(capability: UserMemoryCapability, ptr: *mut u8, len: usize, strict_fault: bool) -> Self {
        Self {
            capability,
            ptr: ptr as usize,
            len,
            strict_fault,
        }
    }

    fn advance(&mut self, count: usize) {
        self.ptr = self.ptr.wrapping_add(count);
        self.len -= count;
    }
}

impl Write for ProgressiveVmBytesMut {
    fn write(&mut self, input: &[u8]) -> AxResult<usize> {
        let target = self.len.min(input.len());
        let mut copied = 0;
        while copied < target {
            let address = match self.ptr.checked_add(copied) {
                Some(address) => address,
                None => {
                    self.advance(copied);
                    return if copied != 0 && !self.strict_fault {
                        Ok(copied)
                    } else {
                        Err(AxError::BadAddress)
                    };
                }
            };
            let page_offset = address & (PAGE_SIZE_4K - 1);
            let chunk = (target - copied).min(PAGE_SIZE_4K - page_offset);
            if let Err(error) = self
                .capability
                .write_bytes(address, &input[copied..copied + chunk])
                .map_err(map_usercopy_error)
            {
                self.advance(copied);
                return if copied != 0 && !self.strict_fault {
                    Ok(copied)
                } else {
                    Err(error)
                };
            }
            copied += chunk;
        }
        self.advance(copied);
        Ok(copied)
    }

    fn flush(&mut self) -> AxResult {
        Ok(())
    }
}

impl IoBufMut for ProgressiveVmBytesMut {
    fn remaining_mut(&self) -> usize {
        self.len
    }
}

// Receive backends that consume a datagram need the destination capacity as
// well as its writable cursor.  This writer has no readable side, but its
// remaining capacity is exactly the `IoBuf` fact required by `WriteBuf`.
impl IoBuf for ProgressiveVmBytesMut {
    fn remaining(&self) -> usize {
        self.remaining_mut()
    }
}

/// Adds page-sized read/write requests around the capability-backed iovec
/// cursor. `IoVectorBufIo` already preserves progress at iovec boundaries;
/// this adapter extends the same rule to a fault in the middle of one iovec.
struct PageProgressIo {
    inner: IoVectorBufIo,
    entries: Vec<IoVec>,
    position: usize,
}

impl PageProgressIo {
    fn new(iov: IoVectorBuf) -> AxResult<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(iov.iovcnt())
            .map_err(|_| AxError::NoMemory)?;
        for index in 0..iov.iovcnt() {
            entries.push(iov.entry(index)?);
        }
        Ok(Self {
            inner: iov.into_io(),
            entries,
            position: 0,
        })
    }

    fn chunk_len(&self, requested: usize) -> usize {
        let mut position = self.position;
        for entry in &self.entries {
            let len = entry.iov_len as usize;
            if position >= len {
                position -= len;
                continue;
            }
            let address = (entry.iov_base as usize).wrapping_add(position);
            let page_remaining = PAGE_SIZE_4K - (address & (PAGE_SIZE_4K - 1));
            return requested.min(len - position).min(page_remaining);
        }
        0
    }

    fn advance(&mut self, count: usize) {
        self.position = self.position.saturating_add(count);
    }
}

impl Read for PageProgressIo {
    fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
        let target = self.inner.remaining().min(output.len());
        let mut copied = 0;
        while copied < target {
            let chunk = self.chunk_len(target - copied);
            if chunk == 0 {
                break;
            }
            match self.inner.read(&mut output[copied..copied + chunk]) {
                Ok(count) => {
                    self.advance(count);
                    copied += count;
                    if count < chunk {
                        break;
                    }
                }
                Err(error) => {
                    return if copied != 0 { Ok(copied) } else { Err(error) };
                }
            }
        }
        Ok(copied)
    }
}

impl Write for PageProgressIo {
    fn write(&mut self, input: &[u8]) -> AxResult<usize> {
        let target = self.inner.remaining_mut().min(input.len());
        let mut copied = 0;
        while copied < target {
            let chunk = self.chunk_len(target - copied);
            if chunk == 0 {
                break;
            }
            match self.inner.write(&input[copied..copied + chunk]) {
                Ok(count) => {
                    self.advance(count);
                    copied += count;
                    if count < chunk {
                        break;
                    }
                }
                Err(error) => {
                    return if copied != 0 { Ok(copied) } else { Err(error) };
                }
            }
        }
        Ok(copied)
    }

    fn flush(&mut self) -> AxResult {
        self.inner.flush()
    }
}

impl IoBuf for PageProgressIo {
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }
}

impl IoBufMut for PageProgressIo {
    fn remaining_mut(&self) -> usize {
        self.inner.remaining_mut()
    }
}

/// `IoVectorBufIo` reports a prefix when a later iovec faults. Datagram
/// transports must turn that prefix back into EFAULT (after consuming the
/// datagram); a short destination caused solely by capacity remains valid.
struct StrictDatagramWrite<T> {
    inner: T,
}

impl<T> StrictDatagramWrite<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: Write + IoBufMut> Write for StrictDatagramWrite<T> {
    fn write(&mut self, input: &[u8]) -> AxResult<usize> {
        let copied = self.inner.write(input)?;
        if copied < input.len() && self.inner.remaining_mut() != 0 {
            return Err(AxError::BadAddress);
        }
        Ok(copied)
    }

    fn flush(&mut self) -> AxResult {
        self.inner.flush()
    }
}

impl<T: Write + IoBufMut> IoBufMut for StrictDatagramWrite<T> {
    fn remaining_mut(&self) -> usize {
        self.inner.remaining_mut()
    }
}

impl<T: Write + IoBufMut> IoBuf for StrictDatagramWrite<T> {
    fn remaining(&self) -> usize {
        self.remaining_mut()
    }
}

fn recv_copyout_requires_error(socket: &PinnedSocketDescription) -> AxResult<bool> {
    // axnet's UDP receive dequeues before invoking `dst.write`, while MSG_PEEK
    // takes the non-consuming peek path. Unix datagrams follow the same
    // consume-before-copy contract. Keep a usercopy fault an error here so a
    // consumed datagram is not reported as a successful short message, while
    // the lower peek path still retains its record.
    match socket.backend()? {
        SocketBackendKind::Packet | SocketBackendKind::Netlink => Ok(true),
        SocketBackendKind::Network => {
            let socket = socket.network()?;
            Ok(match &socket.inner {
                AxSocket::Raw(_) | AxSocket::Udp(_) | AxSocket::Sctp(_) => true,
                AxSocket::Unix(unix) => unix.is_record_oriented(),
                _ => false,
            })
        }
        _ => Ok(false),
    }
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

fn native_u16(bytes: &[u8], offset: usize) -> AxResult<u16> {
    Ok(u16::from_ne_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(AxError::InvalidInput)?
            .try_into()
            .unwrap(),
    ))
}

fn native_u32(bytes: &[u8], offset: usize) -> AxResult<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(AxError::InvalidInput)?
            .try_into()
            .unwrap(),
    ))
}

fn read_cmsg_body(
    capability: &UserMemoryCapability,
    header_address: usize,
    header: &cmsghdr,
    exact_len: usize,
) -> AxResult<Vec<u8>> {
    let body_len = header
        .cmsg_len
        .checked_sub(size_of::<cmsghdr>())
        .ok_or(AxError::InvalidInput)?;
    if body_len != exact_len {
        return Err(AxError::InvalidInput);
    }
    let data_address = header_address
        .checked_add(size_of::<cmsghdr>())
        .ok_or(AxError::BadAddress)?;
    snapshot_user_bytes(capability, data_address as *const u8, body_len, exact_len)
}

/// Decodes Linux's three send-side SCTP cmsghdr formats.  The `sndrcv` and
/// `sndinfo` alternatives are mutually exclusive; PRINFO may refine either.
fn parse_sctp_send_cmsg(
    capability: &UserMemoryCapability,
    header_address: usize,
    header: &cmsghdr,
    metadata: &mut Option<SctpSendMetadata>,
    send_info_seen: &mut bool,
    pr_info_seen: &mut bool,
) -> AxResult<()> {
    let replace = |mut value: SctpSendMetadata,
                   metadata: &mut Option<SctpSendMetadata>,
                   send_info_seen: &mut bool|
     -> AxResult<()> {
        if *send_info_seen {
            Err(AxError::InvalidInput)
        } else {
            if let Some(pr) = metadata.take() {
                value.pr_policy = pr.pr_policy;
                value.pr_value = pr.pr_value;
            }
            *metadata = Some(value);
            *send_info_seen = true;
            Ok(())
        }
    };
    match header.cmsg_type as u32 {
        SCTP_SNDRCV => {
            let body = read_cmsg_body(capability, header_address, header, 32)?;
            let flags = native_u16(&body, 4)?;
            replace(
                SctpSendMetadata {
                    stream: native_u16(&body, 0)?,
                    flags,
                    ppid: native_u32(&body, 8)?,
                    context: native_u32(&body, 12)?,
                    pr_policy: flags & 0x0030,
                    pr_value: native_u32(&body, 16)?,
                    ..Default::default()
                },
                metadata,
                send_info_seen,
            )
        }
        SCTP_SNDINFO => {
            let body = read_cmsg_body(capability, header_address, header, 16)?;
            replace(
                SctpSendMetadata {
                    stream: native_u16(&body, 0)?,
                    flags: native_u16(&body, 2)?,
                    ppid: native_u32(&body, 4)?,
                    context: native_u32(&body, 8)?,
                    ..Default::default()
                },
                metadata,
                send_info_seen,
            )
        }
        SCTP_PRINFO => {
            if *pr_info_seen {
                return Err(AxError::InvalidInput);
            }
            let body = read_cmsg_body(capability, header_address, header, 8)?;
            let value = metadata.get_or_insert_with(Default::default);
            value.pr_policy = native_u16(&body, 0)?;
            value.pr_value = native_u32(&body, 4)?;
            *pr_info_seen = true;
            Ok(())
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn parse_send_control(
    capability: &UserMemoryCapability,
    msg: &msghdr,
    sctp: bool,
    dccp: bool,
) -> AxResult<Vec<CMsgData>> {
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
    let mut sctp_metadata = None;
    let mut dccp_priority = None;
    let mut sctp_send_info_seen = false;
    let mut sctp_pr_info_seen = false;
    rights
        .try_reserve_exact(SCM_MAX_FD)
        .map_err(|_| AxError::NoMemory)?;

    let mut offset = 0usize;
    while msg.msg_controllen - offset >= size_of::<cmsghdr>() {
        let hdr_addr = base.checked_add(offset).ok_or(AxError::BadAddress)?;
        let hdr = read_user_copy(capability, UserConstPtr::<cmsghdr>::from(hdr_addr))?;
        let remaining = msg.msg_controllen - offset;
        if hdr.cmsg_len < size_of::<cmsghdr>() || hdr.cmsg_len > remaining {
            return Err(AxError::InvalidInput);
        }
        if (hdr.cmsg_level as u32) == SOL_SCTP {
            if !sctp {
                return Err(AxError::InvalidInput);
            }
            parse_sctp_send_cmsg(
                capability,
                hdr_addr,
                &hdr,
                &mut sctp_metadata,
                &mut sctp_send_info_seen,
                &mut sctp_pr_info_seen,
            )?;
        } else if (hdr.cmsg_level as u32) == SOL_DCCP {
            if !dccp || (hdr.cmsg_type as u32) != DCCP_SCM_PRIORITY {
                return Err(AxError::InvalidInput);
            }
            if dccp_priority.is_some() {
                return Err(AxError::InvalidInput);
            }
            let body = read_cmsg_body(capability, hdr_addr, &hdr, size_of::<u32>())?;
            dccp_priority = Some(u32::from_ne_bytes(
                body.try_into().expect("exact cmsg body"),
            ));
        } else {
            CMsg::append_rights(capability, hdr_addr, &hdr, &mut rights)?;
        }

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
    if let Some(metadata) = sctp_metadata {
        if !cmsg.is_empty() || dccp_priority.is_some() {
            // SCTP ancillary data is protocol metadata, not Unix descriptor
            // passing; reject a mixed control buffer before payload transfer.
            return Err(AxError::InvalidInput);
        }
        cmsg.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        cmsg.push(CMsg::sctp_send(
            metadata.stream,
            metadata.flags,
            metadata.ppid,
            metadata.context,
            metadata.pr_policy,
            metadata.pr_value,
        )?);
    }
    if let Some(priority) = dccp_priority {
        if !cmsg.is_empty() {
            return Err(AxError::InvalidInput);
        }
        cmsg.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        cmsg.push(CMsg::dccp_priority(priority)?);
    }
    Ok(cmsg)
}

fn mmsg_address(base: usize, index: usize) -> AxResult<usize> {
    let offset = index
        .checked_mul(size_of::<mmsghdr>())
        .ok_or(AxError::BadAddress)?;
    base.checked_add(offset).ok_or(AxError::BadAddress)
}

const fn recvmmsg_transport_fault(error: AxError) -> bool {
    // A partial recvmmsg return suppresses the immediate error, but only a
    // fault reported by the transport belongs to the socket's SO_ERROR state.
    // Importing a later mmsghdr or publishing its result can fail locally
    // (EFAULT, EINVAL, address arithmetic, etc.); turning those into
    // SocketFault::Other would leak a spurious EIO into the next operation.
    matches!(error, AxError::ConnectionRefused | AxError::ConnectionReset)
}

fn remember_recvmmsg_error(socket: &PinnedSocketDescription, error: AxError) {
    if !recvmmsg_transport_fault(error) {
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
    // A TPACKET_TX_RING is producer-owned by userspace and has no separate
    // submission syscall.  Linux observes SEND_REQUEST frames at the next
    // packet send boundary, before admitting this ordinary sendmsg payload.
    // Keep that ordering so an invalid explicit destination still leaves a
    // subsequently submitted ring frame visible to the packet device.
    socket.flush_tx_ring()?;
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

fn take_sctp_send_metadata(cmsg: Vec<CMsgData>) -> AxResult<SctpSendMetadata> {
    let mut metadata = SctpSendMetadata::default();
    let mut found = false;
    for cmsg in cmsg {
        let cmsg = cmsg.downcast::<CMsg>().map_err(|_| AxError::InvalidInput)?;
        match *cmsg {
            CMsg::SctpSend {
                stream,
                flags,
                ppid,
                context,
                pr_policy,
                pr_value,
            } if !found => {
                metadata = SctpSendMetadata {
                    stream,
                    flags,
                    ppid,
                    context,
                    pr_policy,
                    pr_value,
                };
                found = true;
            }
            _ => return Err(AxError::InvalidInput),
        }
    }
    Ok(metadata)
}

/// Consumes the one DCCP per-record priority ancillary value.  Parsing has
/// already rejected mixed/duplicate control messages; keep this second check
/// so internal callers cannot bypass the ABI boundary and silently lose one.
fn take_dccp_send_priority(cmsg: Vec<CMsgData>) -> AxResult<Option<u32>> {
    let mut priority = None;
    for cmsg in cmsg {
        let cmsg = cmsg.downcast::<CMsg>().map_err(|_| AxError::InvalidInput)?;
        let CMsg::DccpPriority(value) = *cmsg else {
            return Err(AxError::InvalidInput);
        };
        if priority.replace(value).is_some() {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(priority)
}

fn sctp_rcvinfo_bytes(metadata: SctpRecvMetadata) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    bytes[0..2].copy_from_slice(&metadata.stream.to_ne_bytes());
    bytes[2..4].copy_from_slice(&metadata.ssn.to_ne_bytes());
    bytes[4..6].copy_from_slice(&metadata.flags.to_ne_bytes());
    bytes[8..12].copy_from_slice(&metadata.ppid.to_ne_bytes());
    bytes[12..16].copy_from_slice(&metadata.tsn.to_ne_bytes());
    bytes[16..20].copy_from_slice(&metadata.cumtsn.to_ne_bytes());
    bytes[20..24].copy_from_slice(&metadata.context.to_ne_bytes());
    bytes[24..28].copy_from_slice(&metadata.assoc_id.to_ne_bytes());
    bytes
}

fn sctp_nxtinfo_bytes(metadata: axnet::sctp::SctpNextMetadata) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[0..2].copy_from_slice(&metadata.stream.to_ne_bytes());
    bytes[2..4].copy_from_slice(&metadata.flags.to_ne_bytes());
    bytes[4..8].copy_from_slice(&metadata.ppid.to_ne_bytes());
    bytes[8..12].copy_from_slice(&metadata.length.to_ne_bytes());
    bytes[12..16].copy_from_slice(&metadata.assoc_id.to_ne_bytes());
    bytes
}

fn send_impl(
    capability: &UserMemoryCapability,
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
    let owner_description = socket.description().clone();
    let backend = socket.backend()?;
    let send_flags = if backend == SocketBackendKind::Packet {
        None
    } else {
        Some(validate_sendmsg_flags(flags)?)
    };
    let (network_addr, packet_address, netlink_address) = match backend {
        SocketBackendKind::Network if !addr.is_null() && addrlen != 0 => (
            Some(SocketAddrEx::read_from_user(capability, addr, addrlen)?),
            None,
            None,
        ),
        SocketBackendKind::Packet if !addr.is_null() => (
            None,
            Some(snapshot_address(capability, addr, addrlen)?),
            None,
        ),
        SocketBackendKind::Netlink if !addr.is_null() || addrlen != 0 => (
            None,
            None,
            Some(read_netlink_send_address(capability, addr, addrlen)?),
        ),
        _ => (None, None, None),
    };
    let mut message = PreparedSocketMessage::new(
        flags,
        iov_count,
        if network_addr.is_some() || packet_address.is_some() || netlink_address.is_some() {
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
        return socket
            .write_to_with_actor(
                &mut src,
                snapshot.actor(),
                snapshot.pid(),
                netlink_address,
                flags & MSG_DONTWAIT != 0,
            )
            .map(|sent| sent as isize);
    }

    if backend == SocketBackendKind::Packet {
        // Linux packet_snd consumes MSG_DONTWAIT for allocation policy and
        // otherwise leaves ordinary send flags uninterpreted. This bounded
        // adapter does not wait for transmit capacity, so the raw flags need
        // only remain visible to policy above; do not apply stream/datagram
        // protocol whitelists to AF_PACKET.
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

    if backend == SocketBackendKind::Xdp {
        // AF_XDP transmit data lives exclusively in the UMEM TX ring.  A
        // sendmsg/sendto is its doorbell, not a byte-stream write; consuming
        // iovecs here would create a second, incompatible data path.
        socket.xdp()?.endpoint().kick_tx()?;
        return Ok(0);
    }
    if backend != SocketBackendKind::Network {
        return Err(AxError::NotASocket);
    }
    let nonblocking = socket.nonblocking();
    let socket = socket.network()?;
    if let Some(address) = network_addr.as_ref() {
        validate_network_address(&socket.inner, address)?;
    }
    if matches!(&socket.inner, AxSocket::Udp(_)) && src.remaining() > axnet::udp::MAX_UDP_SEND_LEN {
        return Err(socket_failure(
            thekernel_linux_net::SocketFailure::MessageTooLarge,
        ));
    }
    // Socket transports hand payload to axnet after this boundary, where the
    // IP header is constructed.  Run the namespace OUTPUT policy here so
    // TCP/UDP/raw traffic cannot bypass the same nft/iptables graph that
    // AF_PACKET uses; raw and TUN packet paths additionally provide headers
    // to the conntrack/NAT traversal entry.
    crate::syscall::iptables_output_verdict(socket.net_namespace())?;
    crate::file::netlink::nft_output_verdict(socket.net_namespace())?;
    let mut options = SendOptions {
        to: network_addr,
        flags: send_flags.ok_or(AxError::BadState)?,
        cmsg,
        credentials: Some(snapshot.automatic_unix_credentials()),
        nonblocking_override: Some(nonblocking),
    };
    if matches!(&socket.inner, AxSocket::Unix(_)) {
        super::cmsg::set_scm_rights_owner(&mut options.cmsg, &owner_description);
    }
    let sent = match &socket.inner {
        AxSocket::Unix(unix) if unix.is_datagram() => {
            let mut reservation = match options.to.as_ref() {
                Some(SocketAddrEx::Unix(UnixSocketAddr::Path(path))) => {
                    let security = VfsSecurityContext::new(snapshot.actor().clone());
                    let target = crate::file::unix_socket::resolve_peer(path.clone(), &security)?;
                    unix.prepare_send_to_resolved(options, target)
                        .map_err(|error| map_socket_send_error(&socket.inner, error))?
                }
                _ => unix
                    .prepare_may_send(options)
                    .map_err(|error| map_socket_send_error(&socket.inner, error))?,
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
            let receiving_endpoint = reservation.receiving_identity().raw();
            super::cmsg::set_scm_rights_endpoint_owner(reservation.cmsg_mut(), receiving_endpoint);
            reservation.commit(&mut src)
        }
        AxSocket::Sctp(sctp) => {
            let metadata = take_sctp_send_metadata(core::mem::take(&mut options.cmsg))?;
            // SCTP consumes its cmsg as record metadata.  Pass a fresh empty
            // list to the transport so kernel-only ancillary state can never
            // leak through the generic axnet interface.
            sctp.send_with_metadata(&mut src, options, metadata)
        }
        AxSocket::Dccp(dccp) => {
            let priority = take_dccp_send_priority(core::mem::take(&mut options.cmsg))?;
            dccp.send_with_priority(&mut src, options, priority)
        }
        _ => socket.send(&mut src, options),
    };
    let sent = match sent {
        Err(error) => {
            if should_raise_sigpipe(error, flags) {
                raise_sigpipe(snapshot.pid());
            }
            return Err(map_socket_send_error(&socket.inner, error));
        }
        Ok(sent) => sent,
    };

    Ok(sent as isize)
}

/// `sendto`/`sendmsg` use the same fixed-size AF_NETLINK address layout as
/// bind.  In particular, do not silently discard `sockaddr_nl`: udevd uses a
/// port-ID-only destination to hand an event from its main process to a worker.
fn read_netlink_send_address(
    capability: &UserMemoryCapability,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
) -> AxResult<SockaddrNl> {
    // sockaddr APIs accept a larger caller buffer and consume only the
    // address's defined prefix.  Reject truncation, not harmless extension.
    if addr.is_null() || (addrlen as usize) < size_of::<SockaddrNl>() {
        return Err(AxError::InvalidInput);
    }
    let address = unsafe {
        capability
            .read_value_uninit(addr.address().as_usize() as *const SockaddrNl)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    if address.nl_family as u32 != AF_NETLINK {
        return Err(LinuxError::EINVAL.into());
    }
    Ok(address)
}

pub fn sys_sendto(
    capability: UserMemoryCapability,
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    // Payload access is deliberately deferred to the transport's actual
    // read. Stream transports can then report bytes copied before a later page
    // fault instead of turning the whole send into an eager EFAULT.
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    send_impl(
        &capability,
        &socket,
        &snapshot,
        fd,
        ProgressiveVmBytes::new(capability.clone(), buf, len),
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
    fn import(capability: &UserMemoryCapability, msg: &msghdr, flags: u32) -> AxResult<Self> {
        let send_iov = IoVectorBuf::new(
            capability.clone(),
            msg.msg_iov.cast::<IoVec>(),
            msg.msg_iovlen,
        )?;
        send_iov.check_readable()?;
        let iov_count = send_iov.iovcnt();
        let control = snapshot_user_bytes(
            capability,
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
    capability: &UserMemoryCapability,
    socket: &PinnedSocketDescription,
    snapshot: &SocketSyscallSnapshot,
    fd: i32,
    msg: UserConstPtr<msghdr>,
    flags: u32,
) -> AxResult<isize> {
    let msg = read_user_copy(capability, msg)?;
    if socket.backend()? == SocketBackendKind::AfAlg {
        let af_alg = socket.af_alg()?;
        debug!("sys_sendmsg <= fd: {fd}, flags: {flags}, af_alg");
        if flags & !MSG_DONTWAIT != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let policy_socket = socket.security_ref()?;
        return import_send_after_socket_hook(
            || ImportedAfAlgSend::import(capability, &msg, flags),
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

    let send_iov = IoVectorBuf::new(
        capability.clone(),
        msg.msg_iov.cast::<IoVec>(),
        msg.msg_iovlen,
    )?;
    let (cmsg, packet_control_length) = if socket.backend()? == SocketBackendKind::Packet {
        // Copy the bounded generic control buffer once, but defer semantic
        // cmsg parsing/support to the AF_PACKET mechanism phase after policy.
        let control = snapshot_user_bytes(
            capability,
            msg.msg_control.cast::<u8>(),
            msg.msg_controllen,
            MAX_SENDMSG_CONTROL_LEN,
        )?;
        (Vec::new(), control.len())
    } else {
        let is_sctp = socket.backend()? == SocketBackendKind::Network
            && matches!(&socket.network()?.inner, AxSocket::Sctp(_));
        let is_dccp = socket.backend()? == SocketBackendKind::Network
            && matches!(&socket.network()?.inner, AxSocket::Dccp(_));
        let cmsg = parse_send_control(capability, &msg, is_sctp, is_dccp)?;
        (cmsg, 0)
    };

    send_impl(
        capability,
        socket,
        snapshot,
        fd,
        PageProgressIo::new(send_iov)?,
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        cmsg,
        packet_control_length,
        msg.msg_iovlen,
        msg.msg_controllen,
    )
}

pub fn sys_sendmsg(
    capability: UserMemoryCapability,
    fd: i32,
    msg: UserConstPtr<msghdr>,
    flags: u32,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    sendmsg_with_socket(&capability, &socket, &snapshot, fd, msg, flags)
}

enum ReceivedSocketAddress {
    Network(SocketAddrEx),
    Netlink { pid: u32, groups: u32 },
    Packet(thekernel_linux_packet::SockAddrLl),
}

impl ReceivedSocketAddress {
    fn write_to_user(
        self,
        capability: &UserMemoryCapability,
        addr: UserPtr<sockaddr>,
        addrlen: &mut socklen_t,
    ) -> AxResult<()> {
        match self {
            Self::Network(addr_value) => addr_value.write_to_user(capability, addr, addrlen),
            Self::Netlink { pid, groups } => {
                let addr_value = SockaddrNl {
                    nl_family: AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: pid,
                    nl_groups: groups,
                };
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&addr_value as *const SockaddrNl).cast::<u8>(),
                        size_of::<SockaddrNl>(),
                    )
                };
                let copy_len = (*addrlen as usize).min(bytes.len());
                if copy_len != 0 {
                    capability
                        .write_bytes(addr.address().as_usize(), &bytes[..copy_len])
                        .map_err(map_usercopy_error)?;
                }
                *addrlen = bytes.len() as _;
                Ok(())
            }
            Self::Packet(address) => write_received_address(capability, address, addr, addrlen),
        }
    }
}

struct ReceiveOutcome {
    returned_len: isize,
    message_truncated: bool,
    message_eor: bool,
    control_truncated: bool,
    address: Option<ReceivedSocketAddress>,
}

/// Retained io_uring socket-receive result.  It deliberately carries record
/// termination/truncation separately from CQE buffer selection so the ring
/// executor never has to reinterpret a generic file read as a socket event.
pub(crate) struct IoUringSocketReceive {
    pub(crate) bytes: i32,
    pub(crate) eof: bool,
    pub(crate) message_flags: u32,
}

impl IoUringSocketReceive {
    /// io_uring exposes post-receive queue state through the standard
    /// `IORING_CQE_F_SOCK_NONEMPTY` bit. `MSG_TRUNC`, `MSG_EOR`, and
    /// `MSG_CTRUNC` have no invented CQE bit: truncation has shaped `bytes`,
    /// while record/control state remains retained for completion policy.
    pub(crate) const fn cqe_flags(self, socket_nonempty: bool) -> u32 {
        const IORING_CQE_F_SOCK_NONEMPTY: u32 = 1 << 2;
        if socket_nonempty && !self.eof {
            IORING_CQE_F_SOCK_NONEMPTY
        } else {
            0
        }
    }

    pub(crate) const fn record_boundary(self) -> bool {
        self.message_flags & MSG_EOR != 0
    }
    pub(crate) const fn truncated(self) -> bool {
        self.message_flags & (MSG_TRUNC | MSG_CTRUNC) != 0
    }
}

/// Performs one nonblocking receive against the already retained socket OFD.
/// No numeric fd lookup, current task file status, or generic pread path is
/// involved.  io_uring has no recvmsg control/name storage, therefore only
/// payload flags admitted by its SQE parser reach this primitive.
pub(crate) fn io_uring_recv_pinned(
    capability: &UserMemoryCapability,
    socket: &PinnedSocketDescription,
    buffer: *mut u8,
    length: usize,
    flags: u32,
) -> AxResult<IoUringSocketReceive> {
    let strict_copyout = recv_copyout_requires_error(socket)?;
    let mut flags = validate_recvmsg_flags(
        effective_message_flags(flags | MSG_DONTWAIT, socket.nonblocking()),
        socket.backend()? == SocketBackendKind::Packet,
    )?;
    flags.insert_dont_wait();
    let outcome = recv_impl(
        capability,
        socket,
        -1,
        ProgressiveVmBytesMut::new(capability.clone(), buffer, length, strict_copyout),
        flags,
        false,
        None,
        false,
    )?;
    let bytes = i32::try_from(outcome.returned_len).map_err(|_| AxError::OutOfRange)?;
    let mut message_flags = 0;
    if outcome.message_truncated {
        message_flags |= MSG_TRUNC;
    }
    if outcome.message_eor {
        message_flags |= MSG_EOR;
    }
    if outcome.control_truncated {
        message_flags |= MSG_CTRUNC;
    }
    Ok(IoUringSocketReceive {
        bytes,
        eof: bytes == 0,
        message_flags,
    })
}

fn recv_impl(
    _capability: &UserMemoryCapability,
    socket: &PinnedSocketDescription,
    fd: i32,
    mut dst: impl WriteBuf,
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
        debug!("sys_recv => fd: {fd}, netlink recv: {recv:?}");
        let mut control_truncated = false;
        if netlink.passcred()
            && let Some(credentials) = recv.credentials
        {
            control_truncated = match cmsg_builder {
                Some(mut builder) => {
                    !builder.push_credentials(credentials.pid, credentials.uid, credentials.gid)
                }
                None => true,
            };
        }
        return Ok(ReceiveOutcome {
            returned_len: recv.len as isize,
            message_truncated: false,
            message_eor: false,
            control_truncated,
            address: want_address.then_some(ReceivedSocketAddress::Netlink {
                pid: recv.source_port_id,
                groups: recv.source_groups,
            }),
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
            message_eor: false,
            control_truncated: false,
            address: want_address.then_some(ReceivedSocketAddress::Packet(result.address())),
        });
    }

    if socket.backend()? == SocketBackendKind::Xdp {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if socket.backend()? != SocketBackendKind::Network {
        return Err(AxError::NotASocket);
    }
    let nonblocking = socket.nonblocking();
    let socket = socket.network()?;
    let sctp = matches!(&socket.inner, axnet::Socket::Sctp(_));
    // DCCP retains application datagram boundaries even though Linux assigns
    // it its own SOCK_DCCP type rather than SOCK_SEQPACKET.  Expose the
    // record/truncation/EOR rules through recvmsg just as SCTP does.
    let dccp = matches!(&socket.inner, axnet::Socket::Dccp(_));
    let seqpacket =
        matches!(&socket.inner, axnet::Socket::Unix(unix) if unix.is_seqpacket()) || sctp || dccp;
    let record_len = if seqpacket {
        socket.inner.recv_pending_len()?
    } else {
        0
    };
    let record_capacity = dst.remaining_mut();
    let mut cmsg = Vec::new();

    let mut remote_addr = want_address.then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let options = RecvOptions {
        from: remote_addr.as_mut(),
        flags: recv_flags.generic,
        cmsg: Some(&mut cmsg),
        nonblocking_override: Some(nonblocking),
    };
    let mut sctp_metadata = None;
    let recv = match &socket.inner {
        AxSocket::Sctp(sctp_socket) => {
            sctp_socket.recv_with_metadata(&mut dst, options, &mut sctp_metadata)?
        }
        _ => socket.recv(&mut dst, options)?,
    };

    let sctp_requested_control = matches!(&socket.inner, AxSocket::Sctp(sctp_socket)
        if sctp_metadata.is_some()
            && (sctp_socket.events()[0] != 0 || sctp_socket.recv_rcvinfo()
                || (sctp_socket.recv_nxtinfo() && sctp_metadata.and_then(|metadata| metadata.next).is_some())));
    let mut control_truncated =
        cmsg_builder.is_none() && (!cmsg.is_empty() || sctp_requested_control);
    if let Some(mut builder) = cmsg_builder {
        if let (AxSocket::Sctp(sctp_socket), Some(metadata)) = (&socket.inner, sctp_metadata) {
            let legacy_data_io = sctp_socket.events()[0] != 0;
            if legacy_data_io || sctp_socket.recv_rcvinfo() {
                if !builder.push_fixed(SOL_SCTP, SCTP_RCVINFO, &sctp_rcvinfo_bytes(metadata)) {
                    control_truncated = true;
                }
            }
            if sctp_socket.recv_nxtinfo() {
                if let Some(next) = metadata.next {
                    if !builder.push_fixed(SOL_SCTP, SCTP_NXTINFO, &sctp_nxtinfo_bytes(next)) {
                        control_truncated = true;
                    }
                }
            }
        }
        for cmsg in cmsg {
            let cmsg = match cmsg.downcast::<CMsg>() {
                Ok(cmsg) => cmsg,
                Err(cmsg) => match cmsg.downcast::<axnet::options::SocketCredentials>() {
                    Ok(credentials) => {
                        if !builder.push_credentials(
                            credentials.pid,
                            credentials.uid,
                            credentials.gid,
                        ) {
                            control_truncated = true;
                        }
                        continue;
                    }
                    Err(_) => {
                        warn!("received unexpected cmsg");
                        control_truncated = true;
                        continue;
                    }
                },
            };

            match &*cmsg {
                CMsg::Rights { graph, .. } => {
                    let fds = graph.fds.lock();
                    let expected = fds.len();
                    let result = builder.push_rights(&fds, cloexec_rights);
                    if rights_push_was_truncated(expected, result) {
                        control_truncated = true;
                    }
                }
                CMsg::Credentials { pid, uid, gid } => {
                    if !builder.push_credentials(*pid, *uid, *gid) {
                        control_truncated = true;
                    }
                }
                CMsg::SctpSend { .. } => {
                    control_truncated = true;
                }
                CMsg::DccpPriority(_) => {
                    control_truncated = true;
                }
            }
        }
    }

    debug!("sys_recv => fd: {fd}, recv: {recv}");
    Ok(ReceiveOutcome {
        returned_len: recv as isize,
        message_truncated: seqpacket && record_len > record_capacity,
        message_eor: seqpacket && recv != 0,
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
    capability: UserMemoryCapability,
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: u32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    // Do not pre-fault destinations. Stream transports return a prefix when a
    // later page faults; datagram transports use the strict adapter below so
    // the consumed datagram still reports EFAULT.
    let strict_copyout = recv_copyout_requires_error(&socket)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    let recv_flags = validate_recvmsg_flags(flags, socket.backend()? == SocketBackendKind::Packet)?;
    let message = recvfrom_security_message(flags);
    receive_socket_output_after_policy(
        || dispatch_receive_message(&socket, &snapshot, &message, len, flags),
        || {
            recv_impl(
                &capability,
                &socket,
                fd,
                ProgressiveVmBytesMut::new(capability.clone(), buf, len, strict_copyout),
                recv_flags,
                !addr.is_null(),
                None,
                flags & MSG_CMSG_CLOEXEC != 0,
            )
        },
        |outcome| {
            if let Some(remote_addr) = outcome.address {
                let mut user_addrlen = read_user_copy(
                    &capability,
                    UserConstPtr::<socklen_t>::from(addrlen.address().as_usize()),
                )?;
                remote_addr.write_to_user(&capability, addr, &mut user_addrlen)?;
                write_user_copy(&capability, addrlen, user_addrlen)?;
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
    fn import(
        capability: &UserMemoryCapability,
        user: UserPtr<msghdr>,
        _defer_payload_fault: bool,
    ) -> AxResult<Self> {
        let msg_hdr = read_user_copy(
            capability,
            UserConstPtr::<msghdr>::from(user.address().as_usize()),
        )?;
        if (msg_hdr.msg_namelen as i32) < 0 || (msg_hdr.msg_controllen as isize) < 0 {
            return Err(AxError::InvalidInput);
        }
        validate_recvmsg_iovlen(msg_hdr.msg_iovlen)?;

        let recv_iov = IoVectorBuf::new(
            capability.clone(),
            msg_hdr.msg_iov.cast::<IoVec>(),
            msg_hdr.msg_iovlen,
        )?;
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
    capability: &UserMemoryCapability,
    snapshot: &SocketSyscallSnapshot,
    socket: &PinnedSocketDescription,
    fd: i32,
    imported: ImportedRecvMessage,
    flags: u32,
    recv_flags: ValidatedRecvFlags,
    strict_copyout: bool,
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
            capability.clone(),
            UserPtr::from(msg_hdr.msg_control.cast::<cmsghdr>()),
            &mut msg_hdr.msg_controllen,
            snapshot.pid_namespace().clone(),
        ))
    };
    let recv_iov = PageProgressIo::new(recv_iov)?;
    let recv = if strict_copyout {
        recv_impl(
            capability,
            socket,
            fd,
            StrictDatagramWrite::new(recv_iov),
            recv_flags,
            !msg_hdr.msg_name.is_null(),
            control,
            flags & MSG_CMSG_CLOEXEC != 0,
        )?
    } else {
        recv_impl(
            capability,
            socket,
            fd,
            recv_iov,
            recv_flags,
            !msg_hdr.msg_name.is_null(),
            control,
            flags & MSG_CMSG_CLOEXEC != 0,
        )?
    };
    // Ancillary fd numbers are a Linux publication point. `recv_impl` handles
    // those first, so an invalid msg_name preserves already exposed fds.
    if let Some(remote_addr) = recv.address {
        remote_addr.write_to_user(
            capability,
            UserPtr::from(msg_hdr.msg_name as usize),
            &mut name_len,
        )?;
    }
    if recv.control_truncated {
        msg_hdr.msg_flags |= MSG_CTRUNC;
    }
    if recv.message_truncated {
        msg_hdr.msg_flags |= MSG_TRUNC;
    }
    if recv.message_eor {
        msg_hdr.msg_flags |= MSG_EOR;
    }
    let msg_addr = msg.address().as_usize();
    // Match Linux's ordered field copyout instead of rewriting the input half
    // of msghdr after the message has already been consumed.
    if !msg_hdr.msg_name.is_null() {
        write_user_field(
            capability,
            msg_addr,
            core::mem::offset_of!(msghdr, msg_namelen),
            name_len as socklen_t,
        )?;
    }
    write_user_field(
        capability,
        msg_addr,
        core::mem::offset_of!(msghdr, msg_flags),
        msg_hdr.msg_flags,
    )?;
    write_user_field(
        capability,
        msg_addr,
        core::mem::offset_of!(msghdr, msg_controllen),
        msg_hdr.msg_controllen,
    )?;
    Ok(recv.returned_len)
}

pub fn sys_recvmsg(
    capability: UserMemoryCapability,
    fd: i32,
    msg: UserPtr<msghdr>,
    flags: u32,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let socket = PinnedSocketDescription::from_fd(fd)?;
    let flags = effective_message_flags(flags, socket.nonblocking());
    let recv_flags = validate_recvmsg_flags(flags, socket.backend()? == SocketBackendKind::Packet)?;
    let strict_copyout = recv_copyout_requires_error(&socket)?;
    let imported = ImportedRecvMessage::import(
        &capability,
        msg,
        socket.backend()? == SocketBackendKind::Packet,
    )?;
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
        |imported| {
            recvmsg_imported(
                &capability,
                &snapshot,
                &socket,
                fd,
                imported,
                flags,
                recv_flags,
                strict_copyout,
            )
        },
    )
}

pub fn sys_sendmmsg(
    capability: UserMemoryCapability,
    fd: i32,
    msgvec: UserPtr<mmsghdr>,
    vlen: u32,
    flags: u32,
) -> AxResult<isize> {
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
        match sendmsg_with_socket(&capability, &socket, &snapshot, fd, msg, flags) {
            Ok(len) => {
                if let Err(err) = write_user_field(
                    &capability,
                    ptr,
                    core::mem::offset_of!(mmsghdr, msg_len),
                    len as u32,
                ) {
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

fn recvmmsg_has_timeout(
    capability: &UserMemoryCapability,
    timeout: UserConstPtr<timespec>,
) -> AxResult<bool> {
    if timeout.is_null() {
        return Ok(false);
    }
    let timeout = read_user_copy(capability, timeout)?;
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(AxError::InvalidInput);
    }
    Ok(true)
}

pub fn sys_recvmmsg(
    capability: UserMemoryCapability,
    fd: i32,
    msgvec: UserPtr<mmsghdr>,
    vlen: u32,
    flags: u32,
    timeout: UserConstPtr<timespec>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    // The timeout object is imported before Linux enters do_recvmmsg(), even
    // for vlen zero. Pin the endpoint next, then skip only msgvec processing.
    let has_timeout = recvmmsg_has_timeout(&capability, timeout)?;
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
    let strict_copyout = recv_copyout_requires_error(&socket)?;
    let mut received = 0usize;
    let defer_payload_fault = socket.backend()? == SocketBackendKind::Packet;
    let base = msgvec.address().as_usize();
    let first_ptr = mmsg_address(base, 0)?;
    let first_msg = UserPtr::<mmsghdr>::from(first_ptr).cast::<msghdr>();
    let first_imported = ImportedRecvMessage::import(&capability, first_msg, defer_payload_fault)?;
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
            Err(_err) if received != 0 => {
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
            match ImportedRecvMessage::import(&capability, msg, defer_payload_fault) {
                Ok(imported) => imported,
                Err(_err) if received != 0 => {
                    return Ok(received as isize);
                }
                Err(err) => return Err(err),
            }
        };
        let receive = if idx == 0 {
            recvmsg_imported(
                &capability,
                &snapshot,
                &socket,
                fd,
                imported,
                active_flags,
                recv_flags,
                strict_copyout,
            )
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
                |imported| {
                    recvmsg_imported(
                        &capability,
                        &snapshot,
                        &socket,
                        fd,
                        imported,
                        active_flags,
                        recv_flags,
                        strict_copyout,
                    )
                },
            )
        };
        match receive {
            Ok(len) => {
                if let Err(err) = write_user_field(
                    &capability,
                    ptr,
                    core::mem::offset_of!(mmsghdr, msg_len),
                    len as u32,
                ) {
                    if received != 0 {
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

    fn mapped_io_capability() -> UserMemoryCapability {
        use alloc::sync::Arc;

        use axhal::paging::{MappingFlags, PageSize};
        use axsync::Mutex;
        use memory_addr::{PAGE_SIZE_4K, VirtAddr};

        let mut address_space =
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 4).unwrap();
        for base in [0x1000, 0x3000] {
            address_space
                .map(
                    VirtAddr::from(base),
                    PAGE_SIZE_4K,
                    MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                    false,
                    crate::mm::Backend::new_alloc(VirtAddr::from(base), PageSize::Size4K),
                )
                .unwrap();
        }
        UserMemoryCapability::new(Arc::new(Mutex::new(address_space)))
    }

    fn map_io_page(capability: &UserMemoryCapability, base: usize) {
        use axhal::paging::{MappingFlags, PageSize};
        use memory_addr::{PAGE_SIZE_4K, VirtAddr};

        capability
            .address_space()
            .lock()
            .map(
                VirtAddr::from(base),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(VirtAddr::from(base), PageSize::Size4K),
            )
            .unwrap();
    }

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
    fn progressive_send_copy_returns_a_prefix_before_a_later_page_fault() {
        let capability = mapped_io_capability();
        let first_page = [0x5a_u8; 16];
        capability.write_bytes(0x1ff0, &first_page).unwrap();
        let mut source = ProgressiveVmBytes::new(capability.clone(), 0x1ff0 as *const u8, 32);
        let mut output = [0_u8; 32];

        assert_eq!(source.read(&mut output), Ok(16));
        assert_eq!(&output[..16], &first_page);
        assert_eq!(source.remaining(), 16);
        assert_eq!(source.read(&mut output[..16]), Err(AxError::BadAddress));
        assert_eq!(source.remaining(), 16);

        map_io_page(&capability, 0x2000);
        capability.write_bytes(0x2000, &[0x6b; 16]).unwrap();
        assert_eq!(source.read(&mut output[..16]), Ok(16));
        assert_eq!(&output[..16], &[0x6b; 16]);
        assert_eq!(source.remaining(), 0);
    }

    #[test]
    fn progressive_iovec_copy_returns_a_prefix_before_a_middle_page_fault() {
        let capability = mapped_io_capability();
        let descriptor = IoVec {
            iov_base: 0x1ff0,
            iov_len: 32,
        };
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut IoVec, descriptor)
                .unwrap();
        }
        let iov = IoVectorBuf::new(capability.clone(), 0x1000 as *const IoVec, 1).unwrap();
        let mut source = PageProgressIo::new(iov).unwrap();
        let mut output = [0_u8; 32];
        capability.write_bytes(0x1ff0, &[0x4d; 16]).unwrap();

        assert_eq!(source.read(&mut output), Ok(16));
        assert_eq!(source.remaining(), 16);
        assert_eq!(source.read(&mut output[..16]), Err(AxError::BadAddress));
        assert_eq!(source.remaining(), 16);

        map_io_page(&capability, 0x2000);
        capability.write_bytes(0x2000, &[0x4e; 16]).unwrap();
        assert_eq!(source.read(&mut output[..16]), Ok(16));
        assert_eq!(&output[..16], &[0x4e; 16]);
        assert_eq!(source.remaining(), 0);
    }

    #[test]
    fn stream_copyout_returns_a_prefix_but_datagram_copyout_keeps_efault() {
        let capability = mapped_io_capability();
        let input = [0x7c_u8; 32];

        let mut stream =
            ProgressiveVmBytesMut::new(capability.clone(), 0x1ff0 as *mut u8, input.len(), false);
        assert_eq!(stream.write(&input), Ok(16));
        assert_eq!(stream.remaining_mut(), 16);

        let mut datagram =
            ProgressiveVmBytesMut::new(capability, 0x1ff0 as *mut u8, input.len(), true);
        assert_eq!(datagram.write(&input), Err(AxError::BadAddress));
        assert_eq!(datagram.remaining_mut(), 16);
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
    fn partial_recvmmsg_only_defers_transport_socket_faults() {
        assert!(!recvmmsg_transport_fault(AxError::WouldBlock));
        assert!(!recvmmsg_transport_fault(AxError::BadAddress));
        assert!(!recvmmsg_transport_fault(AxError::InvalidInput));
        assert!(!recvmmsg_transport_fault(AxError::Io));
        assert!(recvmmsg_transport_fault(AxError::ConnectionRefused));
        assert!(recvmmsg_transport_fault(AxError::ConnectionReset));
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
