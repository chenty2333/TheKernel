//! Pure Linux socket ABI decoding and admission planning.
//!
//! This crate never owns sockets, file descriptors, namespaces, or transport.
#![no_std]
#![forbid(unsafe_code)]

use core::mem::{align_of, size_of};

pub const AF_UNIX: u16 = 1;
pub const AF_NETLINK: u16 = 16;
pub const SOL_SOCKET: i32 = 1;
pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;
pub const NETLINK_ALIGNTO: usize = 4;
pub const CMSG_ALIGNTO: usize = align_of::<usize>();
pub const NLMSG_HDRLEN: usize = 16;
pub const CMSG_HDRLEN: usize = size_of::<usize>() + 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetError {
    Truncated,
    InvalidFamily,
    InvalidLength,
    InvalidAlignment,
    UnknownOption,
    UnsupportedOption,
    InvalidValue,
    PermissionDenied,
}

/// Linux-visible failures selected by the socket ABI adapter after a
/// transport-neutral AX socket fact has been established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketFailure {
    AddressFamilyUnsupported,
    ProtocolOptionUnsupported,
    MessageTooLarge,
    NotConnected,
    AddressUnavailable,
    NetworkUnreachable,
    PeerTypeMismatch,
    ConnectionRefused,
    ConnectionReset,
    TimedOut,
    Io,
}

/// Returns the x86_64 Linux errno for an ABI-owned socket failure.
pub const fn socket_failure_errno(failure: SocketFailure) -> i32 {
    match failure {
        SocketFailure::AddressFamilyUnsupported => 97,
        SocketFailure::ProtocolOptionUnsupported => 92,
        SocketFailure::MessageTooLarge => 90,
        SocketFailure::NotConnected => 107,
        SocketFailure::AddressUnavailable => 99,
        SocketFailure::NetworkUnreachable => 101,
        SocketFailure::PeerTypeMismatch => 91,
        SocketFailure::ConnectionRefused => 111,
        SocketFailure::ConnectionReset => 104,
        SocketFailure::TimedOut => 110,
        SocketFailure::Io => 5,
    }
}

/// x86_64 Linux interface-name field width.
pub const IFNAMSIZ: usize = 16;
/// x86_64 Linux `struct ifreq` width.
pub const IFREQ_SIZE: usize = 40;
/// x86_64 Linux `struct ifconf` width.
pub const IFCONF_SIZE: usize = 16;
const IFREQ_UNION_OFFSET: usize = IFNAMSIZ;
const IFCONF_POINTER_OFFSET: usize = 8;

/// Linux interface ioctl request understood by the interface-query boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IfreqRequest {
    /// Return all IPv4 interface addresses.
    GetConfiguration,
    /// Return the interface index selected by name.
    GetIndex,
    /// Return the interface name selected by index.
    GetName,
    /// Return interface flags.
    GetFlags,
    /// Return interface MTU.
    GetMtu,
    /// Set interface flags.
    SetFlags,
    /// Set interface MTU.
    SetMtu,
}

impl IfreqRequest {
    /// Decodes an x86_64 Linux ioctl number without importing kernel state.
    pub const fn decode(raw: u32) -> Option<Self> {
        match raw {
            0x8910 => Some(Self::GetName),
            0x8912 => Some(Self::GetConfiguration),
            0x8913 => Some(Self::GetFlags),
            0x8914 => Some(Self::SetFlags),
            0x8921 => Some(Self::GetMtu),
            0x8922 => Some(Self::SetMtu),
            0x8933 => Some(Self::GetIndex),
            _ => None,
        }
    }
}

/// Complete x86_64 Linux `struct ifreq` input preserved byte-for-byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IfreqWire([u8; IFREQ_SIZE]);

impl IfreqWire {
    /// Decodes exactly one x86_64 Linux `struct ifreq`.
    pub fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        let bytes: [u8; IFREQ_SIZE] = bytes.try_into().map_err(|_| NetError::InvalidLength)?;
        Ok(Self(bytes))
    }

    /// Returns the complete preserved wire image.
    pub const fn bytes(self) -> [u8; IFREQ_SIZE] {
        self.0
    }

    /// Returns the NUL-terminated interface-name field.
    pub fn name(&self) -> &[u8; IFNAMSIZ] {
        self.0[..IFNAMSIZ]
            .try_into()
            .expect("fixed ifreq name width")
    }

    /// Reads the active integer member of the interface union.
    pub fn ivalue(&self) -> i32 {
        i32::from_ne_bytes(
            self.0[IFREQ_UNION_OFFSET..IFREQ_UNION_OFFSET + 4]
                .try_into()
                .expect("fixed ifreq integer width"),
        )
    }

    /// Replaces the name while preserving the interface union.
    pub fn with_name(mut self, name: &[u8]) -> Self {
        self.0[..IFNAMSIZ].copy_from_slice(&encode_ifreq_name(name));
        self
    }

    /// Replaces only the command-selected output member.
    pub fn with_output(mut self, output: IfreqOutput) -> Self {
        match output {
            IfreqOutput::Integer(value) | IfreqOutput::Mtu(value) | IfreqOutput::Index(value) => {
                self.0[IFREQ_UNION_OFFSET..IFREQ_UNION_OFFSET + 4]
                    .copy_from_slice(&value.to_ne_bytes());
            }
            IfreqOutput::Flags(value) => {
                self.0[IFREQ_UNION_OFFSET..IFREQ_UNION_OFFSET + 2]
                    .copy_from_slice(&value.to_ne_bytes());
            }
        }
        self
    }
}

/// Command-selected interface-query result encoded into an [`IfreqWire`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IfreqOutput {
    /// Integer union member.
    Integer(i32),
    /// Short flags union member.
    Flags(i16),
    /// MTU union member.
    Mtu(i32),
    /// Interface-index union member.
    Index(i32),
}

/// Decoded x86_64 Linux `struct ifconf` input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IfconfWire {
    /// Requested byte capacity, clamped by Linux's non-negative convention.
    pub requested_len: usize,
    /// Userspace destination pointer; zero requests the required size only.
    pub buffer: usize,
}

impl IfconfWire {
    /// Decodes exactly one x86_64 Linux `struct ifconf`.
    pub fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() != IFCONF_SIZE {
            return Err(NetError::InvalidLength);
        }
        let len = i32::from_ne_bytes(bytes[..4].try_into().expect("fixed ifconf length width"));
        let buffer = usize::from_ne_bytes(
            bytes[IFCONF_POINTER_OFFSET..]
                .try_into()
                .expect("fixed ifconf pointer width"),
        );
        Ok(Self {
            requested_len: len.max(0) as usize,
            buffer,
        })
    }
}

#[cfg(test)]
mod ifreq_tests {
    use super::*;

    #[test]
    fn x86_64_ifreq_and_ifconf_wire_layouts_are_byte_exact() {
        assert_eq!(IFNAMSIZ, 16);
        assert_eq!(IFREQ_SIZE, 40);
        assert_eq!(IFCONF_SIZE, 16);
        assert_eq!(ifconf_entry_offset(2), Some(80));

        let mut input = [0xa5; IFREQ_SIZE];
        input[..3].copy_from_slice(b"lo\0");
        let output = IfreqWire::decode(&input)
            .unwrap()
            .with_output(IfreqOutput::Integer(7))
            .bytes();
        assert_eq!(&output[16..20], &7_i32.to_ne_bytes());
        assert!(output[20..].iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn ifconf_ipv4_entry_has_linux_ifreq_stride_and_sockaddr_offsets() {
        let entry = encode_ifconf_ipv4(b"eth0", [192, 0, 2, 1]);
        assert_eq!(&entry[..5], b"eth0\0");
        assert_eq!(&entry[16..18], &2_u16.to_ne_bytes());
        assert_eq!(&entry[20..24], &[192, 0, 2, 1]);
    }
}

/// Encodes one IPv4 `ifconf` entry using the x86_64 Linux `ifreq` stride.
pub fn encode_ifconf_ipv4(name: &[u8], ipv4: [u8; 4]) -> [u8; IFREQ_SIZE] {
    let mut bytes = [0; IFREQ_SIZE];
    let name_len = name.len().min(IFNAMSIZ);
    bytes[..name_len].copy_from_slice(&name[..name_len]);
    bytes[IFREQ_UNION_OFFSET..IFREQ_UNION_OFFSET + 2].copy_from_slice(&2u16.to_ne_bytes());
    bytes[IFREQ_UNION_OFFSET + 4..IFREQ_UNION_OFFSET + 8].copy_from_slice(&ipv4);
    bytes
}

/// Returns a checked byte offset for an `ifconf` entry.
pub const fn ifconf_entry_offset(index: usize) -> Option<usize> {
    index.checked_mul(IFREQ_SIZE)
}

/// Returns whether the wire name equals an interface name exactly.
pub fn ifreq_name_eq(raw_name: &[u8; IFNAMSIZ], name: &[u8]) -> bool {
    let len = raw_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    len == name.len() && raw_name[..len] == *name
}

/// Encodes a Rust interface name into Linux's fixed NUL-terminated field.
pub fn encode_ifreq_name(name: &[u8]) -> [u8; IFNAMSIZ] {
    let mut raw_name = [0; IFNAMSIZ];
    let copied = name.len().min(IFNAMSIZ - 1);
    raw_name[..copied].copy_from_slice(&name[..copied]);
    raw_name
}

/// Linux's default netlink send-buffer size used when the socket has not
/// configured SO_SNDBUF.
pub const NETLINK_DEFAULT_SEND_BUFFER_BYTES: usize = 208 * 1024;
/// Linux reserves skb bookkeeping space from a netlink sender's buffer.
pub const NETLINK_SEND_BUFFER_OVERHEAD: usize = 32;
pub const NETLINK_MAX_MESSAGE_BYTES: usize =
    NETLINK_DEFAULT_SEND_BUFFER_BYTES - NETLINK_SEND_BUFFER_OVERHEAD;

/// Pure admission result for one netlink datagram submitted by userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetlinkWriteAdmission {
    Admit,
    MessageTooLarge,
}

/// Decides whether a datagram fits the Linux default netlink send budget.
/// It does not copy payload bytes or allocate an skb.
pub const fn admit_netlink_write(len: usize) -> NetlinkWriteAdmission {
    if len > NETLINK_MAX_MESSAGE_BYTES {
        NetlinkWriteAdmission::MessageTooLarge
    } else {
        NetlinkWriteAdmission::Admit
    }
}

/// Pure admission result for a receiver queue with independently owned
/// storage and wakeup mechanisms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetlinkQueueAdmission {
    Enqueue,
    Drop,
}

/// Decides whether another datagram can be retained by a bounded netlink
/// receiver queue. `queued_bytes` may be corrupt or stale; saturating
/// subtraction conservatively rejects rather than wrapping.
pub const fn admit_netlink_queue(
    queued_messages: usize,
    queued_bytes: usize,
    message_len: usize,
    message_limit: usize,
    byte_limit: usize,
) -> NetlinkQueueAdmission {
    if queued_messages >= message_limit || message_len > byte_limit.saturating_sub(queued_bytes) {
        NetlinkQueueAdmission::Drop
    } else {
        NetlinkQueueAdmission::Enqueue
    }
}

/// Socket operation whose wait policy is being selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketWaitKind {
    Connect,
    Send,
    Receive,
}

/// Linux-visible result class when a socket wait budget expires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketWaitOutcome {
    WouldBlock,
    InProgress,
}

/// Deferred socket-error handling selected for an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingErrorPolicy {
    ConsumeBeforeAttempt,
    PreserveForSocketError,
}

/// Plans the Linux timeout result without owning clocks or wait queues.
pub const fn plan_wait_timeout(kind: SocketWaitKind) -> SocketWaitOutcome {
    match kind {
        SocketWaitKind::Connect => SocketWaitOutcome::InProgress,
        SocketWaitKind::Send | SocketWaitKind::Receive => SocketWaitOutcome::WouldBlock,
    }
}

/// Plans whether a deferred transport error is consumed by this operation.
pub const fn plan_pending_error(kind: SocketWaitKind) -> PendingErrorPolicy {
    match kind {
        SocketWaitKind::Connect => PendingErrorPolicy::PreserveForSocketError,
        SocketWaitKind::Send | SocketWaitKind::Receive => PendingErrorPolicy::ConsumeBeforeAttempt,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SockAddr {
    pub family: u16,
}
impl SockAddr {
    pub const fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() < 2 {
            return Err(NetError::Truncated);
        }
        Ok(Self {
            family: u16::from_ne_bytes([bytes[0], bytes[1]]),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixName<'a> {
    Unnamed,
    Pathname(&'a [u8]),
    Abstract(&'a [u8]),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixSockAddr<'a> {
    pub name: UnixName<'a>,
}
impl<'a> UnixSockAddr<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, NetError> {
        let head = SockAddr::decode(bytes)?;
        if head.family != AF_UNIX {
            return Err(NetError::InvalidFamily);
        }
        let body = &bytes[2..];
        if body.is_empty() {
            return Ok(Self {
                name: UnixName::Unnamed,
            });
        }
        if body[0] == 0 {
            return Ok(Self {
                name: UnixName::Abstract(&body[1..]),
            });
        }
        let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
        if end == 0 {
            return Err(NetError::InvalidValue);
        }
        Ok(Self {
            name: UnixName::Pathname(&body[..end]),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cmsg<'a> {
    pub level: i32,
    pub kind: i32,
    pub data: &'a [u8],
}
pub struct CmsgIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    failed: bool,
}
pub const fn cmsg_align(value: usize) -> Option<usize> {
    match value.checked_add(CMSG_ALIGNTO - 1) {
        Some(value) => Some(value & !(CMSG_ALIGNTO - 1)),
        None => None,
    }
}
pub const fn nlmsg_align(value: usize) -> Option<usize> {
    match value.checked_add(NETLINK_ALIGNTO - 1) {
        Some(value) => Some(value & !(NETLINK_ALIGNTO - 1)),
        None => None,
    }
}
pub fn cmsgs(bytes: &[u8]) -> CmsgIter<'_> {
    CmsgIter {
        bytes,
        offset: 0,
        failed: false,
    }
}
impl<'a> Iterator for CmsgIter<'a> {
    type Item = Result<Cmsg<'a>, NetError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset == self.bytes.len() {
            return None;
        }
        if self.bytes.len() - self.offset < CMSG_HDRLEN {
            self.failed = true;
            return Some(Err(NetError::Truncated));
        }
        let b = &self.bytes[self.offset..];
        let len = usize::from_ne_bytes(b[..size_of::<usize>()].try_into().ok()?);
        if len < CMSG_HDRLEN || len > b.len() {
            self.failed = true;
            return Some(Err(NetError::InvalidLength));
        }
        let level = i32::from_ne_bytes(
            b[size_of::<usize>()..size_of::<usize>() + 4]
                .try_into()
                .ok()?,
        );
        let kind = i32::from_ne_bytes(b[size_of::<usize>() + 4..CMSG_HDRLEN].try_into().ok()?);
        // Linux's cmsg_nxthdr stops after a valid final header when its
        // aligned successor would no longer leave room for another header.
        // The trailing alignment bytes are not another malformed cmsg.
        let next = cmsg_align(len).unwrap_or(usize::MAX);
        self.offset = if next <= b.len() && b.len() - next >= CMSG_HDRLEN {
            self.offset + next
        } else {
            self.bytes.len()
        };
        Some(Ok(Cmsg {
            level,
            kind,
            data: &b[CMSG_HDRLEN..len],
        }))
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NlMsgHdr {
    pub len: u32,
    pub kind: u16,
    pub flags: u16,
    pub seq: u32,
    pub pid: u32,
}
const _: () = {
    assert!(size_of::<NlMsgHdr>() == 16);
    assert!(align_of::<NlMsgHdr>() == 4);
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetlinkMessage<'a> {
    pub header: NlMsgHdr,
    pub payload: &'a [u8],
}
pub struct NetlinkIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    failed: bool,
}
pub fn netlink_messages(bytes: &[u8]) -> NetlinkIter<'_> {
    NetlinkIter {
        bytes,
        offset: 0,
        failed: false,
    }
}
impl<'a> Iterator for NetlinkIter<'a> {
    type Item = Result<NetlinkMessage<'a>, NetError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset == self.bytes.len() {
            return None;
        }
        let b = &self.bytes[self.offset..];
        if b.len() < NLMSG_HDRLEN {
            self.failed = true;
            return Some(Err(NetError::Truncated));
        }
        let len = u32::from_ne_bytes(b[0..4].try_into().ok()?) as usize;
        if len < NLMSG_HDRLEN || len > b.len() {
            self.failed = true;
            return Some(Err(NetError::InvalidLength));
        }
        let header = NlMsgHdr {
            len: len as u32,
            kind: u16::from_ne_bytes(b[4..6].try_into().ok()?),
            flags: u16::from_ne_bytes(b[6..8].try_into().ok()?),
            seq: u32::from_ne_bytes(b[8..12].try_into().ok()?),
            pid: u32::from_ne_bytes(b[12..16].try_into().ok()?),
        };
        // NLMSG_OK/NLMSG_NEXT style traversal accepts a complete final
        // message and leaves a short trailer uninterpreted.
        let next = nlmsg_align(len).unwrap_or(usize::MAX);
        self.offset = if next <= b.len() && b.len() - next >= NLMSG_HDRLEN {
            self.offset + next
        } else {
            self.bytes.len()
        };
        Some(Ok(NetlinkMessage {
            header,
            payload: &b[16..len],
        }))
    }
}
pub fn encode_netlink(
    header: NlMsgHdr,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, NetError> {
    let len = NLMSG_HDRLEN
        .checked_add(payload.len())
        .ok_or(NetError::InvalidLength)?;
    let used = nlmsg_align(len).ok_or(NetError::InvalidLength)?;
    if header.len as usize != len || output.len() < used {
        return Err(NetError::InvalidLength);
    }
    output[..used].fill(0);
    output[0..4].copy_from_slice(&(len as u32).to_ne_bytes());
    output[4..6].copy_from_slice(&header.kind.to_ne_bytes());
    output[6..8].copy_from_slice(&header.flags.to_ne_bytes());
    output[8..12].copy_from_slice(&header.seq.to_ne_bytes());
    output[12..16].copy_from_slice(&header.pid.to_ne_bytes());
    output[16..len].copy_from_slice(payload);
    Ok(used)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SockAddrNl {
    pub family: u16,
    pub pad: u16,
    pub pid: u32,
    pub groups: u32,
}
const _: () = {
    assert!(size_of::<SockAddrNl>() == 12);
    assert!(align_of::<SockAddrNl>() == 4);
};
impl SockAddrNl {
    pub fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() != 12 {
            return Err(NetError::InvalidLength);
        }
        let v = Self {
            family: u16::from_ne_bytes([bytes[0], bytes[1]]),
            pad: u16::from_ne_bytes([bytes[2], bytes[3]]),
            pid: u32::from_ne_bytes(bytes[4..8].try_into().map_err(|_| NetError::Truncated)?),
            groups: u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| NetError::Truncated)?),
        };
        if v.family != AF_NETLINK {
            return Err(NetError::InvalidFamily);
        }
        Ok(v)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketOption {
    ReuseAddress,
    PendingError,
    DontRoute,
    SendBuffer,
    ReceiveBuffer,
    KeepAlive,
    ReceiveTimeout,
    SendTimeout,
    PeerCredentials,
    NoDelay,
    MaxSegment,
    TimeToLive,
    Ipv6Only,
    PassCred,
    ReceiveCredentials,
}

/// Decodes the Linux socket-option namespace into a capability without
/// exposing its level/name integers to transport implementations.
/// Why a Linux socket-option selector has no normal socket capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketOptionDecodeError {
    /// The protocol level is not a normal socket-option namespace.
    UnknownLevel,
    /// The level is known but the option name is not.
    UnknownOption,
}

/// Linux errno category selected before a normal socket option reaches a
/// transport implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketOptionErrno {
    /// `ENOPROTOOPT`: the selected protocol option does not exist.
    NoProtocolOption,
    /// `EOPNOTSUPP`: get on an unsupported protocol level.
    OperationNotSupported,
}

/// Raw Linux socket-option selector retained only at the ABI boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawSocketOption {
    /// Linux socket option level.
    pub level: i32,
    /// Linux socket option name.
    pub name: i32,
}

/// Plans normal getsockopt selector admission and Linux errno category.
pub const fn plan_get_socket_option(
    raw: RawSocketOption,
) -> Result<SocketOption, SocketOptionErrno> {
    match decode_socket_option(raw.level, raw.name) {
        Ok(option) => Ok(option),
        Err(SocketOptionDecodeError::UnknownLevel) => Err(SocketOptionErrno::OperationNotSupported),
        Err(SocketOptionDecodeError::UnknownOption) => Err(SocketOptionErrno::NoProtocolOption),
    }
}

/// Plans normal setsockopt selector admission and Linux errno category.
pub const fn plan_set_socket_option(
    raw: RawSocketOption,
) -> Result<SocketOption, SocketOptionErrno> {
    match decode_socket_option(raw.level, raw.name) {
        Ok(option) => Ok(option),
        Err(_) => Err(SocketOptionErrno::NoProtocolOption),
    }
}

pub const fn decode_socket_option(
    level: i32,
    option: i32,
) -> Result<SocketOption, SocketOptionDecodeError> {
    match (level, option) {
        (1, 2) => Ok(SocketOption::ReuseAddress),
        (1, 4) => Ok(SocketOption::PendingError),
        (1, 5) => Ok(SocketOption::DontRoute),
        (1, 7) => Ok(SocketOption::SendBuffer),
        (1, 8) => Ok(SocketOption::ReceiveBuffer),
        (1, 9) => Ok(SocketOption::KeepAlive),
        (1, 20) => Ok(SocketOption::ReceiveTimeout),
        (1, 21) => Ok(SocketOption::SendTimeout),
        (1, 16) => Ok(SocketOption::PassCred),
        (1, 17) => Ok(SocketOption::PeerCredentials),
        (6, 1) => Ok(SocketOption::NoDelay),
        (6, 2) => Ok(SocketOption::MaxSegment),
        (0, 2) => Ok(SocketOption::TimeToLive),
        (41, 26) => Ok(SocketOption::Ipv6Only),
        (1 | 6 | 0 | 41, _) => Err(SocketOptionDecodeError::UnknownOption),
        _ => Err(SocketOptionDecodeError::UnknownLevel),
    }
}

/// x86_64 Linux `struct ucred` wire image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UcredWire {
    /// Sender process identifier.
    pub pid: u32,
    /// Sender user identifier.
    pub uid: u32,
    /// Sender group identifier.
    pub gid: u32,
}

impl UcredWire {
    /// Linux x86_64 `struct ucred` width.
    pub const SIZE: usize = 12;

    /// Decodes the exact Linux wire representation.
    pub fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() != Self::SIZE {
            return Err(NetError::InvalidLength);
        }
        Ok(Self {
            pid: u32::from_ne_bytes(bytes[..4].try_into().expect("fixed ucred pid width")),
            uid: u32::from_ne_bytes(bytes[4..8].try_into().expect("fixed ucred uid width")),
            gid: u32::from_ne_bytes(bytes[8..].try_into().expect("fixed ucred gid width")),
        })
    }

    /// Encodes the exact Linux wire representation without Rust layout.
    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[..4].copy_from_slice(&self.pid.to_ne_bytes());
        bytes[4..8].copy_from_slice(&self.uid.to_ne_bytes());
        bytes[8..].copy_from_slice(&self.gid.to_ne_bytes());
        bytes
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketPlan {
    SetPassCred(bool),
    ReceiveCredentials,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketFeatures {
    pub passcred: bool,
    pub receive_credentials: bool,
}
pub fn plan_option(
    features: SocketFeatures,
    level: i32,
    option: i32,
    value: i32,
) -> Result<SocketPlan, NetError> {
    if level != SOL_SOCKET {
        return Err(NetError::UnknownOption);
    }
    match option {
        16 if features.passcred => match value {
            0 => Ok(SocketPlan::SetPassCred(false)),
            1 => Ok(SocketPlan::SetPassCred(true)),
            _ => Err(NetError::InvalidValue),
        },
        17 if features.receive_credentials && value == 0 => Ok(SocketPlan::ReceiveCredentials),
        16 | 17 => Err(NetError::UnsupportedOption),
        _ => Err(NetError::UnknownOption),
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionSnapshot {
    pub can_bind: bool,
    pub can_send: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionPlan {
    Bind,
    Send,
}
pub const fn plan_admission(
    snapshot: AdmissionSnapshot,
    bind: bool,
) -> Result<AdmissionPlan, NetError> {
    if bind {
        if snapshot.can_bind {
            Ok(AdmissionPlan::Bind)
        } else {
            Err(NetError::PermissionDenied)
        }
    } else if snapshot.can_send {
        Ok(AdmissionPlan::Send)
    } else {
        Err(NetError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_failure_errno_matches_x86_64_linux_uapi() {
        assert_eq!(
            socket_failure_errno(SocketFailure::AddressFamilyUnsupported),
            97
        );
        assert_eq!(
            socket_failure_errno(SocketFailure::ProtocolOptionUnsupported),
            92
        );
        assert_eq!(socket_failure_errno(SocketFailure::MessageTooLarge), 90);
        assert_eq!(socket_failure_errno(SocketFailure::NotConnected), 107);
        assert_eq!(socket_failure_errno(SocketFailure::AddressUnavailable), 99);
        assert_eq!(socket_failure_errno(SocketFailure::NetworkUnreachable), 101);
        assert_eq!(socket_failure_errno(SocketFailure::PeerTypeMismatch), 91);
        assert_eq!(socket_failure_errno(SocketFailure::Io), 5);
    }

    #[test]
    fn netlink_admission_preserves_limit_and_overflow_boundaries() {
        assert_eq!(
            admit_netlink_write(NETLINK_MAX_MESSAGE_BYTES),
            NetlinkWriteAdmission::Admit
        );
        assert_eq!(
            admit_netlink_write(NETLINK_MAX_MESSAGE_BYTES.saturating_add(1)),
            NetlinkWriteAdmission::MessageTooLarge
        );
        assert_eq!(
            admit_netlink_queue(128, 0, 1, 128, 16),
            NetlinkQueueAdmission::Drop
        );
        assert_eq!(
            admit_netlink_queue(0, usize::MAX, 1, 128, 16),
            NetlinkQueueAdmission::Drop
        );
    }

    #[test]
    fn layouts_and_wire_roundtrip() {
        let h = NlMsgHdr {
            len: 19,
            kind: 7,
            flags: 2,
            seq: 3,
            pid: 4,
        };
        let mut out = [0xaa; 24];
        assert_eq!(encode_netlink(h, b"abc", &mut out), Ok(20));
        let msg = netlink_messages(&out[..20]).next().unwrap().unwrap();
        assert_eq!(msg.header, h);
        assert_eq!(msg.payload, b"abc");
    }
    #[test]
    fn validation_order() {
        assert_eq!(SockAddrNl::decode(&[0; 3]), Err(NetError::InvalidLength));
        assert_eq!(UnixSockAddr::decode(&[16, 0]), Err(NetError::InvalidFamily));
    }

    #[test]
    fn valid_final_control_and_netlink_messages_ignore_short_trailers() {
        let mut cmsg = [0_u8; 18];
        cmsg[..8].copy_from_slice(&17_usize.to_ne_bytes());
        cmsg[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        cmsg[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        cmsg[16] = 9;
        let mut cmsgs = cmsgs(&cmsg);
        assert!(cmsgs.next().unwrap().is_ok());
        assert!(cmsgs.next().is_none());

        let mut nlmsg = [0_u8; 19];
        nlmsg[..4].copy_from_slice(&16_u32.to_ne_bytes());
        let mut messages = netlink_messages(&nlmsg);
        assert!(messages.next().unwrap().is_ok());
        assert!(messages.next().is_none());
    }

    #[test]
    fn netlink_pad_is_not_a_bind_validation_field() {
        let mut wire = [0_u8; 12];
        wire[..2].copy_from_slice(&AF_NETLINK.to_ne_bytes());
        wire[2..4].copy_from_slice(&7_u16.to_ne_bytes());
        assert_eq!(SockAddrNl::decode(&wire).unwrap().pad, 7);
    }

    #[test]
    fn socket_option_and_ucred_wire_are_linux_owned() {
        assert_eq!(decode_socket_option(1, 16), Ok(SocketOption::PassCred));
        assert_eq!(decode_socket_option(6, 1), Ok(SocketOption::NoDelay));
        assert_eq!(
            decode_socket_option(99, 1),
            Err(SocketOptionDecodeError::UnknownLevel)
        );
        let credentials = UcredWire {
            pid: u32::MAX,
            uid: 2,
            gid: 3,
        };
        assert_eq!(UcredWire::decode(&credentials.encode()), Ok(credentials));
    }

    #[test]
    fn socket_option_admission_preserves_get_and_set_errno_split() {
        assert_eq!(
            plan_get_socket_option(RawSocketOption { level: 99, name: 1 }),
            Err(SocketOptionErrno::OperationNotSupported)
        );
        assert_eq!(
            plan_get_socket_option(RawSocketOption { level: 1, name: 99 }),
            Err(SocketOptionErrno::NoProtocolOption)
        );
        assert_eq!(
            plan_set_socket_option(RawSocketOption { level: 99, name: 1 }),
            Err(SocketOptionErrno::NoProtocolOption)
        );
    }
    #[test]
    fn socket_wait_policy_keeps_connect_and_data_distinct() {
        assert_eq!(
            plan_wait_timeout(SocketWaitKind::Connect),
            SocketWaitOutcome::InProgress
        );
        assert_eq!(
            plan_wait_timeout(SocketWaitKind::Send),
            SocketWaitOutcome::WouldBlock
        );
        assert_eq!(
            plan_pending_error(SocketWaitKind::Receive),
            PendingErrorPolicy::ConsumeBeforeAttempt
        );
        assert_eq!(
            plan_pending_error(SocketWaitKind::Connect),
            PendingErrorPolicy::PreserveForSocketError
        );
    }
}
