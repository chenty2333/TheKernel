//! Pure Linux socket ABI decoding and admission planning.
//!
//! This crate never owns sockets, file descriptors, namespaces, or transport.
#![no_std]
#![forbid(unsafe_code)]

use core::mem::{align_of, size_of};

mod msg;

pub use msg::{
    BatchDeadline, MSG_BATCH, MSG_CMSG_CLOEXEC, MSG_CMSG_COMPAT, MSG_CONFIRM, MSG_CTRUNC,
    MSG_DONTROUTE, MSG_DONTWAIT, MSG_EOR, MSG_ERRQUEUE, MSG_FASTOPEN, MSG_INTERNAL_SENDMSG_FLAGS,
    MSG_MORE, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_PROBE, MSG_SPLICE_PAGES, MSG_TRUNC,
    MSG_WAITALL, MSG_WAITFORONE, MSG_ZEROCOPY, MessageDirection, MsgOobTransport,
    NO_URGENT_DATA_ERRNO, NSEC_PER_SEC, OOB_NOT_SUPPORTED_ERRNO, ReceiveStep, Timespec64,
    absolute_deadline, batch_deadline, compat_flag_errno, msg_oob_errno,
    packet_recvmsg_flag_errno, receive_wait_target, remaining_timeout, sock_rcvlowat,
    strip_internal_sendmsg_flags, waitall_continues,
};

pub const AF_UNSPEC: u16 = 0;
pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const AF_NETLINK: u16 = 16;
pub const AF_PACKET: u16 = 17;
pub const AF_ALG: u16 = 38;
pub const AF_XDP: u16 = 44;
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

/// x86_64 Linux `SOCK_*` creation types (`include/linux/net.h`).
pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_RAW: u32 = 3;
pub const SOCK_RDM: u32 = 4;
pub const SOCK_SEQPACKET: u32 = 5;
pub const SOCK_DCCP: u32 = 6;
/// Obsolete packet-level type. `__sock_create` rewrites `(PF_INET, SOCK_PACKET)`
/// to `PF_PACKET` before the family is looked up.
pub const SOCK_PACKET: u32 = 10;
/// `SOCK_MAX` (`include/linux/net.h`): `__sock_create` reports `EINVAL` for any
/// masked type at or above this value and never consults a family.
pub const SOCK_MAX: u32 = SOCK_PACKET + 1;
/// `SOCK_TYPE_MASK` (`include/linux/net.h`).
pub const SOCK_TYPE_MASK: u32 = 0xf;

/// Admission of one already-masked socket type by `__sock_create`
/// (`net/socket.c`). Linux only range-checks here; whether a type is *usable*
/// is decided by the selected family's `create` hook, which reports
/// `ESOCKTNOSUPPORT` for the types it does not implement.
pub const fn socket_type_in_range(ty: u32) -> bool {
    ty < SOCK_MAX
}

/// Linux's obsolete-AF_INET compatibility rewrite, applied by `__sock_create`
/// after the type range check and before `security_socket_create`.
pub const fn socket_creation_family(family: u16, ty: u32) -> u16 {
    if family == AF_INET && ty == SOCK_PACKET {
        AF_PACKET
    } else {
        family
    }
}

/// `sizeof(struct sockaddr_storage)`. `move_addr_to_kernel` rejects every
/// imported address longer than this before a family-specific rule runs.
pub const SOCKADDR_STORAGE_LEN: usize = 128;

/// `net/socket.c:move_addr_to_kernel()` bounds every socket address argument
/// before the protocol is reached:
///
/// ```c
/// 	if (ulen < 0 || ulen > sizeof(struct sockaddr_storage))
/// 		return -EINVAL;
/// ```
///
/// `__sys_bind` calls it at `net/socket.c:1947` after the descriptor lookup,
/// `__sys_connect` at `:2003` and `__sys_sendto` at `:2242`; the upper bound is
/// therefore generic socket-layer state and not a family rule. `addrlen` is a
/// signed `int` on those paths, so `(socklen_t)-1` is the negative length that
/// this rejects rather than a 4 GiB buffer.
pub const fn address_import_admitted(addrlen: i32) -> bool {
    addrlen >= 0 && (addrlen as usize) <= SOCKADDR_STORAGE_LEN
}

/// `net/socket.c:do_sock_setsockopt()` / `do_sock_getsockopt()` reject a
/// negative option length before any protocol runs:
///
/// ```c
/// 	if (optlen < 0)
/// 		return -EINVAL;
/// ```
///
/// `setsockopt` reaches it at `:2342-2343` and `getsockopt` at `:2366-2367`;
/// the descriptor lookup that precedes both reports EBADF first.  Because
/// `socklen_t` is unsigned in the ABI, a caller can pass `(socklen_t)-1`, which
/// every lower-bound test in the option table would otherwise read as a 4 GiB
/// buffer.
pub const fn option_length_admitted(optlen: u32) -> bool {
    (optlen as i32) >= 0
}

/// `min_t(u32, val, READ_ONCE(sysctl_wmem_max))` (`net/core/sock.c:1342`) and
/// `min_t(u32, val, READ_ONCE(sysctl_rmem_max))` (`:1374`).
///
/// The comparison is unsigned, so a negative request does not stay negative: it
/// compares above any positive limit and is clamped to the sysctl maximum.  The
/// `FORCE` variants skip this step and instead floor the request at zero
/// (`:1356-1358`, `:1382-1384`).
pub const fn clamp_buffer_request(val: i32, sysctl_max: i32) -> i32 {
    if (val as u32) < (sysctl_max as u32) {
        val
    } else {
        sysctl_max
    }
}

/// `SO_SNDBUFFORCE`/`SO_RCVBUFFORCE`'s `if (val < 0) val = 0;`
/// (`net/core/sock.c:1356-1358`, `:1382-1384`).
pub const fn force_buffer_request(val: i32) -> i32 {
    if val < 0 { 0 } else { val }
}
/// `sizeof(struct sockaddr_in)`.
pub const SOCKADDR_IN_LEN: usize = 16;
/// `SIN6_LEN_RFC2133` (`include/net/ipv6.h`). IPv6 `bind`/`connect` accept a
/// 24-byte address even though `struct sockaddr_in6` is 28 bytes.
pub const SIN6_LEN_RFC2133: usize = 24;
/// `sizeof(struct sockaddr_in6)`.
pub const SOCKADDR_IN6_LEN: usize = 28;
/// `sizeof(struct sockaddr_un)`.
pub const SOCKADDR_UN_LEN: usize = 110;
/// `offsetof(struct sockaddr_un, sun_path)`, which is also the length of the
/// family-only address that makes `unix_bind()` autobind.
pub const SOCKADDR_UN_PATH_OFFSET: usize = 2;
/// Length of the abstract name `unix_autobind()` generates: the NUL marker plus
/// the five lowercase hex digits written by
/// `sprintf(addr->name->sun_path + 1, "%05x", ordernum)`
/// (`net/unix/af_unix.c:1315-1320`).  Linux stores it as
/// `addr->len = offsetof(struct sockaddr_un, sun_path) + 6`, i.e. 8, so
/// `getsockname()` reports 8 for an autobound socket.
pub const UNIX_AUTOBIND_PATH_LEN: usize = 6;
/// `ordernum & 0xFFFFF`: `unix_autobind()` retries inside a 20-bit space and
/// reports `-ENOSPC` once it wraps back to the value it started from
/// (`net/unix/af_unix.c:1311-1334`).
pub const UNIX_AUTOBIND_ORDERNUM_MASK: u32 = 0x000F_FFFF;

/// The six abstract-name bytes `unix_autobind()` stores for one order number:
/// the abstract marker followed by `%05x`.  The marker is part of Linux's
/// stored `sun_path`; a caller whose abstract-name representation excludes it
/// — this crate's [`UnixName::Abstract`], which [`UnixSockAddr::decode`] splits
/// the same way — takes `&name[1..]`.
pub fn unix_autobind_name(ordernum: u32) -> [u8; UNIX_AUTOBIND_PATH_LEN] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let ordernum = ordernum & UNIX_AUTOBIND_ORDERNUM_MASK;
    let mut name = [0_u8; UNIX_AUTOBIND_PATH_LEN];
    for (index, slot) in name[1..].iter_mut().enumerate() {
        let shift = 4 * (UNIX_AUTOBIND_PATH_LEN - 2 - index);
        *slot = HEX[((ordernum >> shift) & 0xf) as usize];
    }
    name
}

/// Whether `addrlen` is the family-only AF_UNIX address that `unix_bind()`
/// turns into an autobind request:
///
/// ```c
/// 	if (addr_len == offsetof(struct sockaddr_un, sun_path) &&
/// 	    sunaddr->sun_family == AF_UNIX)
/// 		return unix_autobind(sk);
/// ```
///
/// (`net/unix/af_unix.c:1463-1470`).  The family comparison belongs to the
/// caller, which has already read the two-byte record; a shorter or longer
/// address never autobinds, and a two-byte record of any other family falls
/// through to `unix_validate_addr()`, which reports `-EINVAL`.
pub const fn unix_autobind_request(addrlen: usize, family: u16) -> bool {
    addrlen == SOCKADDR_UN_PATH_OFFSET && family as u32 == AF_UNIX as u32
}
/// `sizeof(struct sockaddr_nl)`.
pub const SOCKADDR_NL_LEN: usize = 12;
/// `sizeof(sa_family_t)`.
pub const SA_FAMILY_LEN: usize = 2;

/// Address family whose `bind`/`connect` length rule is being applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SockaddrFamily {
    Ipv4,
    Ipv6,
    Unix,
    Netlink,
}

impl SockaddrFamily {
    /// Smallest `addrlen` the family accepts once `move_addr_to_kernel` has
    /// bounded the import.
    ///
    /// - IPv4: `inet_bind_sk` and `__inet_stream_connect` require
    ///   `sizeof(struct sockaddr_in)`.
    /// - IPv6: `inet6_bind_sk`, `tcp_v6_connect` and `ip6_dgram_connect`
    ///   compare against `SIN6_LEN_RFC2133` (24), *not*
    ///   `sizeof(struct sockaddr_in6)` (28).
    /// - AF_UNIX: `unix_validate_addr` and `unix_mkname` require strictly more
    ///   than the two-byte family.
    /// - AF_NETLINK: `netlink_bind` and `netlink_connect` require
    ///   `sizeof(struct sockaddr_nl)`.
    pub const fn minimum(self) -> usize {
        match self {
            Self::Ipv4 => SOCKADDR_IN_LEN,
            Self::Ipv6 => SIN6_LEN_RFC2133,
            Self::Unix => SOCKADDR_UN_PATH_OFFSET + 1,
            Self::Netlink => SOCKADDR_NL_LEN,
        }
    }

    /// Largest `addrlen` the family accepts. AF_UNIX is the only family whose
    /// own structure is narrower than `sockaddr_storage`.
    pub const fn maximum(self) -> usize {
        match self {
            Self::Unix => SOCKADDR_UN_LEN,
            Self::Ipv4 | Self::Ipv6 | Self::Netlink => SOCKADDR_STORAGE_LEN,
        }
    }

    /// Whether `addrlen` selects a kernel address this family will decode.
    /// `move_addr_to_kernel`'s `sockaddr_storage` bound is part of every
    /// family rule because it runs first on the syscall path.
    pub const fn admits(self, addrlen: usize) -> bool {
        addrlen >= self.minimum() && addrlen <= self.maximum()
    }
}

/// Linux `SOCK_*` protocol values that AF_UNIX's `unix_create` admits.
/// `protocol` is either zero or the AF_UNIX family number itself.
pub const fn unix_protocol_admitted(protocol: u32) -> bool {
    protocol == 0 || protocol == AF_UNIX as u32
}

/// Linux's AF_UNIX creation type rewrite. `SOCK_RAW` is the BSD compatibility
/// spelling of `SOCK_DGRAM`; every other type is admitted unchanged.
pub const fn unix_creation_type(ty: u32) -> Option<u32> {
    match ty {
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET => Some(ty),
        SOCK_RAW => Some(SOCK_DGRAM),
        _ => None,
    }
}

/// Linux's AF_PACKET creation types (`packet_create`).
pub const fn packet_creation_type_admitted(ty: u32) -> bool {
    matches!(ty, SOCK_DGRAM | SOCK_RAW | SOCK_PACKET)
}

/// Linux `NETLINK_*` protocol numbers whose kernel endpoints TheKernel models.
/// Every other value fails `netlink_create`'s `nl_table[protocol].registered`
/// test with `EPROTONOSUPPORT`.
pub const NETLINK_ROUTE: u32 = 0;
pub const NETLINK_USERSOCK: u32 = 2;
pub const NETLINK_SOCK_DIAG: u32 = 4;
pub const NETLINK_AUDIT: u32 = 9;
pub const NETLINK_NETFILTER: u32 = 12;
pub const NETLINK_KOBJECT_UEVENT: u32 = 15;
pub const NETLINK_GENERIC: u32 = 16;
/// `MAX_LINKS` (`include/uapi/linux/netlink.h`).
pub const NETLINK_MAX_LINKS: u32 = 32;

/// Whether Linux would find a registered `nl_table` entry for `protocol`.
pub const fn netlink_protocol_registered(protocol: u32) -> bool {
    matches!(
        protocol,
        NETLINK_ROUTE
            | NETLINK_USERSOCK
            | NETLINK_SOCK_DIAG
            | NETLINK_AUDIT
            | NETLINK_NETFILTER
            | NETLINK_KOBJECT_UEVENT
            | NETLINK_GENERIC
    )
}

/// `nl_table[protocol].groups`, the multicast group capacity
/// `netlink_realloc_groups` copies into `nlk->ngroups`.
///
/// `__netlink_kernel_create` raises every registration below 32 groups to 32;
/// NETLINK_ROUTE is the one endpoint that asks for more, `RTNLGRP_MAX`.
pub const fn netlink_protocol_group_capacity(protocol: u32) -> u32 {
    if protocol == NETLINK_ROUTE {
        RTNLGRP_MAX
    } else {
        32
    }
}

/// `RTNLGRP_MAX` (`include/uapi/linux/rtnetlink.h`): `__RTNLGRP_MAX - 1` where
/// the enum holds `RTNLGRP_NONE` plus 39 real groups.
pub const RTNLGRP_MAX: u32 = 39;

/// `RTNLGRP_IPV4_MROUTE_R`, one of the two reverse-path multicast groups
/// `rtnetlink_bind` reserves for `CAP_NET_ADMIN`.
pub const RTNLGRP_IPV4_MROUTE_R: u32 = 30;

/// `RTNLGRP_IPV6_MROUTE_R`, the other privileged rtnetlink group.
pub const RTNLGRP_IPV6_MROUTE_R: u32 = 31;

/// `nl_table[protocol].bind`'s answer for one multicast group.
///
/// Linux calls the hook once per set bit from `netlink_bind` and once for the
/// requested group from `NETLINK_ADD_MEMBERSHIP`.  `rtnetlink_bind` is the only
/// hook that gates a group of an endpoint TheKernel models:
///
/// ```c
///     case RTNLGRP_IPV4_MROUTE_R:
///     case RTNLGRP_IPV6_MROUTE_R:
///         if (!ns_capable(net->user_ns, CAP_NET_ADMIN))
///             return -EPERM;
/// ```
///
/// The remaining families either register no hook at all (usersock,
/// kobject-uevent, sock_diag, nfnetlink) or are covered by the caller:
/// `audit_multicast_bind` is the audit authority check.  Generic netlink's
/// `genl_bind` gates only the groups of *registered* families, so with no
/// generic-netlink family present every group id has no owner and the hook
/// answers zero, exactly as `idr_for_each_entry` falling through does.
pub const fn netlink_group_bind_permitted(protocol: u32, group: u32, net_admin: bool) -> bool {
    match protocol {
        NETLINK_ROUTE => {
            (group != RTNLGRP_IPV4_MROUTE_R && group != RTNLGRP_IPV6_MROUTE_R) || net_admin
        }
        _ => true,
    }
}

/// Byte length `NETLINK_LIST_MEMBERSHIPS` reports for `ngroups` groups:
/// `ALIGN(BITS_TO_BYTES(ngroups), sizeof(u32))`.
pub const fn netlink_membership_bitmap_len(ngroups: u32) -> usize {
    (ngroups as usize).div_ceil(8).next_multiple_of(4)
}

/// Linux `SOL_NETLINK` option numbers (`include/uapi/linux/netlink.h`).
pub const NETLINK_ADD_MEMBERSHIP: u32 = 1;
pub const NETLINK_DROP_MEMBERSHIP: u32 = 2;
pub const NETLINK_PKTINFO: u32 = 3;
pub const NETLINK_BROADCAST_ERROR: u32 = 4;
pub const NETLINK_NO_ENOBUFS: u32 = 5;
pub const NETLINK_LISTEN_ALL_NSID: u32 = 8;
pub const NETLINK_LIST_MEMBERSHIPS: u32 = 9;
pub const NETLINK_CAP_ACK: u32 = 10;
pub const NETLINK_EXT_ACK: u32 = 11;
pub const NETLINK_GET_STRICT_CHK: u32 = 12;
/// `NL_CFG_F_NONROOT_SEND` / `NL_CFG_F_NONROOT_RECV`.
pub const NL_CFG_F_NONROOT_SEND: u32 = 1;
pub const NL_CFG_F_NONROOT_RECV: u32 = 2;

/// How Linux's `netlink_setsockopt` treats a `SOL_NETLINK` option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetlinkSetOption {
    /// `NETLINK_ADD_MEMBERSHIP` / `NETLINK_DROP_MEMBERSHIP`; the value is a
    /// one-based group number validated against `nlk->ngroups`.
    Membership { add: bool },
    /// A boolean `nlk->flags` toggle read back by `get_option`.
    Boolean,
    /// `NETLINK_NO_ENOBUFS` additionally clears the congestion mark.
    SuppressEnoBufs,
    /// Recognised by `netlink_setsockopt` but not settable.
    Rejected,
}

/// Decodes a `SOL_NETLINK` set request without touching socket state.
pub const fn netlink_set_option(optname: u32) -> Option<NetlinkSetOption> {
    match optname {
        NETLINK_ADD_MEMBERSHIP => Some(NetlinkSetOption::Membership { add: true }),
        NETLINK_DROP_MEMBERSHIP => Some(NetlinkSetOption::Membership { add: false }),
        NETLINK_PKTINFO
        | NETLINK_BROADCAST_ERROR
        | NETLINK_LISTEN_ALL_NSID
        | NETLINK_CAP_ACK
        | NETLINK_EXT_ACK
        | NETLINK_GET_STRICT_CHK => Some(NetlinkSetOption::Boolean),
        NETLINK_NO_ENOBUFS => Some(NetlinkSetOption::SuppressEnoBufs),
        // NETLINK_LIST_MEMBERSHIPS is a getter; Linux's `netlink_setsockopt`
        // falls through to `-ENOPROTOOPT` for it.
        _ => None,
    }
}

/// `netlink_allowed` (`net/netlink/af_netlink.c`): a privileged multicast
/// operation is permitted when the protocol advertises the capability flag or
/// the caller holds `CAP_NET_ADMIN` over the socket's network namespace.
pub const fn netlink_allowed(protocol: u32, flag: u32, net_admin: bool) -> bool {
    netlink_protocol_flags(protocol) & flag != 0 || net_admin
}

/// `nl_table[protocol].flags` for the endpoints TheKernel models.
pub const fn netlink_protocol_flags(protocol: u32) -> u32 {
    match protocol {
        // rtnetlink_net_init, genl_net_init, diag_net_init, audit_net_init and
        // uevent_net_init all register `.flags = NL_CFG_F_NONROOT_RECV`;
        // nfnetlink_net_init registers no flags at all.
        NETLINK_ROUTE | NETLINK_GENERIC | NETLINK_SOCK_DIAG | NETLINK_AUDIT
        | NETLINK_KOBJECT_UEVENT => NL_CFG_F_NONROOT_RECV,
        // netlink_add_usersock_entry: `.flags = NL_CFG_F_NONROOT_SEND`.
        NETLINK_USERSOCK => NL_CFG_F_NONROOT_SEND,
        _ => 0,
    }
}

/// `SOL_SOCKET` names Linux's generic `sk_setsockopt`/`sk_getsockopt` serve
/// for every socket, including endpoints whose own `proto_ops` omit the
/// operation (netlink, AF_ALG, AF_XDP).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenericSocketOption {
    Debug,
    ReuseAddress,
    Type,
    Error,
    DontRoute,
    Broadcast,
    SendBuffer,
    ReceiveBuffer,
    KeepAlive,
    OutOfBandInline,
    NoCheck,
    Priority,
    Linger,
    ReusePort,
    PassCredentials,
    PeerCredentials,
    ReceiveLowWater,
    SendLowWater,
    AcceptConn,
    Mark,
    Protocol,
    Domain,
}

pub const SO_DEBUG: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_DONTROUTE: i32 = 5;
pub const SO_BROADCAST: i32 = 6;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_OOBINLINE: i32 = 10;
pub const SO_NO_CHECK: i32 = 11;
pub const SO_PRIORITY: i32 = 12;
pub const SO_LINGER: i32 = 13;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PASSCRED: i32 = 16;
pub const SO_PEERCRED: i32 = 17;
pub const SO_RCVLOWAT: i32 = 18;
pub const SO_SNDLOWAT: i32 = 19;
pub const SO_MARK: i32 = 36;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_PROTOCOL: i32 = 38;
pub const SO_DOMAIN: i32 = 39;
/// `sizeof(struct linger)`.
pub const LINGER_LEN: usize = 8;
/// `sizeof(struct ucred)`.
pub const UCRED_LEN: usize = 12;

/// Linux `sk_getsockopt` admission for one generic `SOL_SOCKET` name.
/// Names outside this table fall through to `-ENOPROTOOPT`.
pub const fn generic_socket_get_option(optname: i32) -> Option<GenericSocketOption> {
    match optname {
        SO_DEBUG => Some(GenericSocketOption::Debug),
        SO_REUSEADDR => Some(GenericSocketOption::ReuseAddress),
        SO_TYPE => Some(GenericSocketOption::Type),
        SO_ERROR => Some(GenericSocketOption::Error),
        SO_DONTROUTE => Some(GenericSocketOption::DontRoute),
        SO_BROADCAST => Some(GenericSocketOption::Broadcast),
        SO_SNDBUF => Some(GenericSocketOption::SendBuffer),
        SO_RCVBUF => Some(GenericSocketOption::ReceiveBuffer),
        SO_KEEPALIVE => Some(GenericSocketOption::KeepAlive),
        SO_OOBINLINE => Some(GenericSocketOption::OutOfBandInline),
        SO_NO_CHECK => Some(GenericSocketOption::NoCheck),
        SO_PRIORITY => Some(GenericSocketOption::Priority),
        SO_LINGER => Some(GenericSocketOption::Linger),
        SO_REUSEPORT => Some(GenericSocketOption::ReusePort),
        SO_PASSCRED => Some(GenericSocketOption::PassCredentials),
        SO_PEERCRED => Some(GenericSocketOption::PeerCredentials),
        SO_RCVLOWAT => Some(GenericSocketOption::ReceiveLowWater),
        SO_SNDLOWAT => Some(GenericSocketOption::SendLowWater),
        SO_ACCEPTCONN => Some(GenericSocketOption::AcceptConn),
        SO_MARK => Some(GenericSocketOption::Mark),
        SO_PROTOCOL => Some(GenericSocketOption::Protocol),
        SO_DOMAIN => Some(GenericSocketOption::Domain),
        _ => None,
    }
}

/// Linux `sk_setsockopt` admission for one generic `SOL_SOCKET` name.
///
/// `SO_TYPE`, `SO_PROTOCOL`, `SO_DOMAIN` and `SO_ERROR` are read-only and
/// return `-ENOPROTOOPT` from an explicit case, which is the same errno as an
/// unknown name. `SO_ACCEPTCONN` and `SO_PEERCRED` have no setter at all, and
/// neither has `SO_SNDLOWAT`: `sk_setsockopt` implements `SO_RCVLOWAT`
/// (`net/core/sock.c:1450-1464`) but has no case for the send side, so a set
/// request reaches `default: ret = -ENOPROTOOPT;` (`:1676-1678`) even though
/// `sk_getsockopt` reports the name (`:1873`).
pub const fn generic_socket_set_option(optname: i32) -> Option<GenericSocketOption> {
    match optname {
        SO_TYPE | SO_PROTOCOL | SO_DOMAIN | SO_ERROR | SO_ACCEPTCONN | SO_PEERCRED
        | SO_SNDLOWAT => None,
        _ => generic_socket_get_option(optname),
    }
}

/// Linux `SOCK_MIN_SNDBUF` / `SOCK_MIN_RCVBUF` (`include/net/sock.h`):
/// `TCP_SKB_MIN_TRUESIZE` is `2048` plus `SKB_DATA_ALIGN(sizeof(struct
/// sk_buff))`, which is 2304/4608 for the pinned Linux 7.2.3 x86_64 build
/// (`sizeof(struct sk_buff)` 232, `SMP_CACHE_BYTES` 64).
pub const TCP_SKB_MIN_TRUESIZE: i32 = 2304;
pub const SOCK_MIN_RCVBUF: i32 = TCP_SKB_MIN_TRUESIZE;
pub const SOCK_MIN_SNDBUF: i32 = TCP_SKB_MIN_TRUESIZE * 2;
/// `sysctl_wmem_max` / `sysctl_rmem_max` initial values (`net/core/sock.c`).
pub const SYSCTL_WMEM_MAX: i32 = 4 << 20;
pub const SYSCTL_RMEM_MAX: i32 = 4 << 20;
/// `sysctl_wmem_default` / `sysctl_rmem_default` initial values:
/// `SK_WMEM_DEFAULT == SK_RMEM_DEFAULT == SKB_TRUESIZE(256) * 256`, which is
/// 212992 for the pinned Linux 7.2.3 x86_64 build.  `sock_init_data_uid` seeds
/// both socket buffers from them without applying a minimum.
pub const SYSCTL_WMEM_DEFAULT: i32 = 212_992;
pub const SYSCTL_RMEM_DEFAULT: i32 = 212_992;

/// Applies `__sock_set_rcvbuf`'s arithmetic: clamp to `INT_MAX / 2`, double,
/// and floor at `SOCK_MIN_RCVBUF`.
pub const fn decode_receive_buffer(val: i32) -> i32 {
    let val = if val > i32::MAX / 2 {
        i32::MAX / 2
    } else {
        val
    };
    let doubled = val * 2;
    if doubled < SOCK_MIN_RCVBUF {
        SOCK_MIN_RCVBUF
    } else {
        doubled
    }
}

/// Applies `sk_setsockopt`'s `set_sndbuf` arithmetic after the caller applied
/// the `SO_SNDBUF` (`sysctl_wmem_max`) or `SO_SNDBUFFORCE` (uncapped) clamp.
pub const fn decode_send_buffer(val: i32) -> i32 {
    let val = if val > i32::MAX / 2 {
        i32::MAX / 2
    } else {
        val
    };
    let doubled = val * 2;
    if doubled < SOCK_MIN_SNDBUF {
        SOCK_MIN_SNDBUF
    } else {
        doubled
    }
}

/// `SO_RCVLOWAT`'s value rule: a negative request becomes `INT_MAX` and an
/// exact zero becomes one.
pub const fn decode_receive_low_water(val: i32) -> i32 {
    if val < 0 {
        i32::MAX
    } else if val == 0 {
        1
    } else {
        val
    }
}

/// `sk_getsockopt`'s `SO_MARK` capability rule: `CAP_NET_RAW` or
/// `CAP_NET_ADMIN` over the socket's network namespace.
pub const fn mark_set_permitted(net_raw: bool, net_admin: bool) -> bool {
    net_raw || net_admin
}

/// `sk_set_prio_allowed`: the traffic-class range every caller may select, or
/// a network capability over the socket's namespace for anything else.
pub const fn priority_set_permitted(val: i32, net_raw: bool, net_admin: bool) -> bool {
    (val >= TC_PRIO_BESTEFFORT && val <= TC_PRIO_INTERACTIVE) || net_raw || net_admin
}

/// `TC_PRIO_BESTEFFORT` / `TC_PRIO_INTERACTIVE` (`pkt_sched.h`).
pub const TC_PRIO_BESTEFFORT: i32 = 0;
pub const TC_PRIO_INTERACTIVE: i32 = 6;

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
    /// Decodes the fixed `struct sockaddr_nl` prefix of one imported address.
    ///
    /// Linux's `netlink_bind` and `netlink_connect` compare `addr_len` against
    /// `sizeof(struct sockaddr_nl)` with `<`, so a longer buffer is admitted
    /// and only these twelve bytes are read; `move_addr_to_kernel` has already
    /// bounded the import by `sockaddr_storage`.
    pub fn decode(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() < SOCKADDR_NL_LEN || bytes.len() > SOCKADDR_STORAGE_LEN {
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
    ReceiveErrors4,
    ReceiveErrors6,
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
        (0, 11) => Ok(SocketOption::ReceiveErrors4),
        (41, 25) => Ok(SocketOption::ReceiveErrors6),
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
    fn socket_type_admission_matches_sock_max() {
        for ty in 0..SOCK_MAX {
            assert!(socket_type_in_range(ty), "type {ty}");
        }
        for ty in SOCK_MAX..=SOCK_TYPE_MASK {
            assert!(!socket_type_in_range(ty), "type {ty}");
        }
        // `__sock_create` admits SOCK_RDM and SOCK_PACKET; rejecting them here
        // would report EINVAL where Linux reports the family's errno.
        assert!(socket_type_in_range(SOCK_RDM));
        assert!(socket_type_in_range(SOCK_PACKET));
    }

    #[test]
    fn only_af_inet_sock_packet_is_rewritten_to_af_packet() {
        assert_eq!(socket_creation_family(AF_INET, SOCK_PACKET), AF_PACKET);
        assert_eq!(socket_creation_family(AF_INET, SOCK_RAW), AF_INET);
        assert_eq!(socket_creation_family(AF_PACKET, SOCK_PACKET), AF_PACKET);
        assert_eq!(socket_creation_family(AF_INET6, SOCK_PACKET), AF_INET6);
    }

    #[test]
    fn address_length_rules_follow_each_family_syscall_check() {
        // move_addr_to_kernel bounds every import at sockaddr_storage.
        assert!(!SockaddrFamily::Ipv4.admits(129));
        assert!(!SockaddrFamily::Ipv6.admits(129));
        assert!(!SockaddrFamily::Netlink.admits(129));

        assert!(!SockaddrFamily::Ipv4.admits(0));
        assert!(!SockaddrFamily::Ipv4.admits(15));
        assert!(SockaddrFamily::Ipv4.admits(16));
        assert!(SockaddrFamily::Ipv4.admits(128));

        // IPv6 compares against SIN6_LEN_RFC2133, not sizeof(sockaddr_in6).
        assert_eq!(SockaddrFamily::Ipv6.minimum(), 24);
        assert!(!SockaddrFamily::Ipv6.admits(23));
        assert!(SockaddrFamily::Ipv6.admits(24));
        assert!(SockaddrFamily::Ipv6.admits(SOCKADDR_IN6_LEN));
        assert!(SockaddrFamily::Ipv6.admits(128));

        // AF_UNIX rejects the family-only length and stops at sockaddr_un.
        assert!(!SockaddrFamily::Unix.admits(0));
        assert!(!SockaddrFamily::Unix.admits(2));
        assert!(SockaddrFamily::Unix.admits(3));
        assert!(SockaddrFamily::Unix.admits(SOCKADDR_UN_LEN));
        assert!(!SockaddrFamily::Unix.admits(SOCKADDR_UN_LEN + 1));

        assert!(!SockaddrFamily::Netlink.admits(11));
        assert!(SockaddrFamily::Netlink.admits(12));
        assert!(SockaddrFamily::Netlink.admits(128));
    }

    #[test]
    fn netlink_sockaddr_decoding_admits_overlong_buffers() {
        let mut wire = [0_u8; SOCKADDR_STORAGE_LEN];
        wire[..2].copy_from_slice(&AF_NETLINK.to_ne_bytes());
        wire[4..8].copy_from_slice(&7_u32.to_ne_bytes());
        wire[8..12].copy_from_slice(&6_u32.to_ne_bytes());
        assert_eq!(SockAddrNl::decode(&wire).unwrap().pid, 7);
        assert_eq!(SockAddrNl::decode(&wire).unwrap().groups, 6);
        assert_eq!(
            SockAddrNl::decode(&wire[..11]),
            Err(NetError::InvalidLength)
        );
        assert_eq!(SockAddrNl::decode(&wire[..12]).unwrap().pid, 7);
    }

    #[test]
    fn unix_creation_admits_only_the_bsd_protocol_spellings() {
        assert!(unix_protocol_admitted(0));
        assert!(unix_protocol_admitted(AF_UNIX as u32));
        assert!(!unix_protocol_admitted(AF_INET as u32));
        assert!(!unix_protocol_admitted(AF_INET6 as u32));

        assert_eq!(unix_creation_type(SOCK_STREAM), Some(SOCK_STREAM));
        assert_eq!(unix_creation_type(SOCK_SEQPACKET), Some(SOCK_SEQPACKET));
        // SOCK_RAW is rewritten so SO_TYPE reports SOCK_DGRAM afterwards.
        assert_eq!(unix_creation_type(SOCK_RAW), Some(SOCK_DGRAM));
        assert_eq!(unix_creation_type(SOCK_RDM), None);
        assert_eq!(unix_creation_type(SOCK_PACKET), None);
    }

    #[test]
    fn netlink_protocol_admission_models_nl_table_registration() {
        assert!(netlink_protocol_registered(NETLINK_USERSOCK));
        assert!(netlink_protocol_registered(NETLINK_ROUTE));
        assert!(!netlink_protocol_registered(NETLINK_MAX_LINKS));
        assert!(!netlink_protocol_registered(1));
        assert!(!netlink_protocol_registered(3));
        assert_eq!(netlink_protocol_group_capacity(NETLINK_ROUTE), RTNLGRP_MAX);
        // Every other registration asks for fewer than 32 groups (uevent and
        // audit for one, SOCK_DIAG for `SKNLGRP_MAX`, nfnetlink for
        // `NFNLGRP_MAX`) or asks for none at all (generic netlink), so
        // `__netlink_kernel_create`'s floor is what they all end up with.
        for protocol in [
            NETLINK_USERSOCK,
            NETLINK_GENERIC,
            NETLINK_SOCK_DIAG,
            NETLINK_NETFILTER,
            NETLINK_AUDIT,
            NETLINK_KOBJECT_UEVENT,
        ] {
            assert_eq!(netlink_protocol_group_capacity(protocol), 32);
        }
    }

    #[test]
    fn rtnetlink_reserves_only_its_reverse_path_groups() {
        // `rtnetlink_bind` names exactly two groups, and the neighbouring
        // RTNLGRP_* values stay open to an unprivileged subscriber.
        assert!(netlink_group_bind_permitted(NETLINK_ROUTE, 29, false));
        assert!(!netlink_group_bind_permitted(
            NETLINK_ROUTE,
            RTNLGRP_IPV4_MROUTE_R,
            false
        ));
        assert!(!netlink_group_bind_permitted(
            NETLINK_ROUTE,
            RTNLGRP_IPV6_MROUTE_R,
            false
        ));
        assert!(netlink_group_bind_permitted(
            NETLINK_ROUTE,
            RTNLGRP_IPV4_MROUTE_R,
            true
        ));
        assert_eq!(RTNLGRP_IPV4_MROUTE_R + 1, RTNLGRP_IPV6_MROUTE_R);
        // Families that register no hook, and generic netlink's per-family
        // gate with no family registered, all answer zero.
        for protocol in [NETLINK_USERSOCK, NETLINK_GENERIC, NETLINK_SOCK_DIAG] {
            assert!(netlink_group_bind_permitted(protocol, 30, false));
        }
    }

    #[test]
    fn netlink_membership_bitmap_matches_align_of_bits_to_bytes() {
        assert_eq!(netlink_membership_bitmap_len(0), 0);
        assert_eq!(netlink_membership_bitmap_len(1), 4);
        assert_eq!(netlink_membership_bitmap_len(32), 4);
        assert_eq!(netlink_membership_bitmap_len(33), 8);
        assert_eq!(netlink_membership_bitmap_len(39), 8);
        assert_eq!(netlink_membership_bitmap_len(64), 8);
        assert_eq!(netlink_membership_bitmap_len(65), 12);
    }

    #[test]
    fn netlink_sol_netlink_option_table_is_exhaustive_for_the_uapi() {
        assert_eq!(
            netlink_set_option(NETLINK_ADD_MEMBERSHIP),
            Some(NetlinkSetOption::Membership { add: true })
        );
        assert_eq!(
            netlink_set_option(NETLINK_DROP_MEMBERSHIP),
            Some(NetlinkSetOption::Membership { add: false })
        );
        assert_eq!(
            netlink_set_option(NETLINK_PKTINFO),
            Some(NetlinkSetOption::Boolean)
        );
        assert_eq!(
            netlink_set_option(NETLINK_LISTEN_ALL_NSID),
            Some(NetlinkSetOption::Boolean)
        );
        assert_eq!(
            netlink_set_option(NETLINK_NO_ENOBUFS),
            Some(NetlinkSetOption::SuppressEnoBufs)
        );
        // Linux has no setter for LIST_MEMBERSHIPS.
        assert_eq!(netlink_set_option(NETLINK_LIST_MEMBERSHIPS), None);
        assert_eq!(netlink_set_option(13), None);
        assert_eq!(netlink_set_option(0), None);
    }

    #[test]
    fn netlink_allowed_accepts_protocol_flags_or_net_admin() {
        assert!(netlink_allowed(NETLINK_ROUTE, NL_CFG_F_NONROOT_RECV, false));
        assert!(!netlink_allowed(
            NETLINK_USERSOCK,
            NL_CFG_F_NONROOT_RECV,
            false
        ));
        assert!(netlink_allowed(
            NETLINK_USERSOCK,
            NL_CFG_F_NONROOT_SEND,
            false
        ));
        assert!(netlink_allowed(
            NETLINK_USERSOCK,
            NL_CFG_F_NONROOT_RECV,
            true
        ));
        // `netlink_connect` gates a nonzero peer on NL_CFG_F_NONROOT_SEND, and
        // only usersock registers that flag: an unprivileged rtnetlink peer is
        // EPERM while the same peer on a usersock endpoint is legal.
        assert!(!netlink_allowed(
            NETLINK_ROUTE,
            NL_CFG_F_NONROOT_SEND,
            false
        ));
        assert!(netlink_allowed(
            NETLINK_ROUTE,
            NL_CFG_F_NONROOT_SEND,
            true
        ));
        // `uevent_net_init` sets `.flags = NL_CFG_F_NONROOT_RECV` (its
        // `.groups = 1` still becomes 32, because `__netlink_kernel_create`
        // raises every request below 32), so an unprivileged process may
        // listen for kernel uevents while a nfnetlink subscriber may not.
        assert!(netlink_allowed(
            NETLINK_KOBJECT_UEVENT,
            NL_CFG_F_NONROOT_RECV,
            false
        ));
        assert!(!netlink_allowed(
            NETLINK_KOBJECT_UEVENT,
            NL_CFG_F_NONROOT_SEND,
            false
        ));
        assert!(!netlink_allowed(
            NETLINK_NETFILTER,
            NL_CFG_F_NONROOT_RECV,
            false
        ));
    }

    #[test]
    fn netlink_sol_socket_names_are_split_by_read_only_cases() {
        assert_eq!(
            generic_socket_get_option(SO_TYPE),
            Some(GenericSocketOption::Type)
        );
        assert_eq!(
            generic_socket_get_option(SO_DOMAIN),
            Some(GenericSocketOption::Domain)
        );
        assert_eq!(
            generic_socket_get_option(SO_SNDBUF),
            Some(GenericSocketOption::SendBuffer)
        );
        assert_eq!(
            generic_socket_get_option(SO_LINGER),
            Some(GenericSocketOption::Linger)
        );
        assert_eq!(
            generic_socket_get_option(SO_SNDLOWAT),
            Some(GenericSocketOption::SendLowWater)
        );
        // Read-only names have an explicit `-ENOPROTOOPT` case in
        // `sk_setsockopt`, so they are absent from the setter table.  The same
        // holds for `SO_SNDLOWAT`, which the setter's `default:` answers
        // (`net/core/sock.c:1676-1678`) while the getter still reports it.
        for optname in [
            SO_TYPE,
            SO_PROTOCOL,
            SO_DOMAIN,
            SO_ERROR,
            SO_ACCEPTCONN,
            SO_PEERCRED,
            SO_SNDLOWAT,
        ] {
            assert!(generic_socket_get_option(optname).is_some(), "{optname}");
            assert_eq!(generic_socket_set_option(optname), None, "{optname}");
        }
        assert_eq!(
            generic_socket_set_option(SO_REUSEADDR),
            Some(GenericSocketOption::ReuseAddress)
        );
        assert_eq!(generic_socket_get_option(49), None);
        assert_eq!(generic_socket_set_option(9999), None);
    }

    #[test]
    fn buffer_arithmetic_matches_the_linux_clamps() {
        assert_eq!(SOCK_MIN_RCVBUF, 2304);
        assert_eq!(SOCK_MIN_SNDBUF, 4608);
        assert_eq!(SYSCTL_WMEM_MAX, 4 << 20);
        assert_eq!(SYSCTL_WMEM_DEFAULT, 212_992);
        assert_eq!(SYSCTL_RMEM_DEFAULT, 212_992);
        assert_eq!(decode_receive_buffer(1000), 2304);
        assert_eq!(decode_send_buffer(1000), 4608);
        assert_eq!(decode_receive_buffer(1 << 20), 2 << 20);
        assert_eq!(decode_send_buffer(1 << 20), 2 << 20);
        assert_eq!(decode_receive_buffer(i32::MAX), (i32::MAX / 2) * 2);
        assert_eq!(decode_receive_low_water(-1), i32::MAX);
        assert_eq!(decode_receive_low_water(0), 1);
        assert_eq!(decode_receive_low_water(5), 5);
    }

    #[test]
    fn privileged_sol_socket_rules_keep_their_capability_alternatives() {
        assert!(mark_set_permitted(false, true));
        assert!(mark_set_permitted(true, false));
        assert!(!mark_set_permitted(false, false));
        assert!(priority_set_permitted(TC_PRIO_BESTEFFORT, false, false));
        assert!(priority_set_permitted(TC_PRIO_INTERACTIVE, false, false));
        assert!(!priority_set_permitted(
            TC_PRIO_INTERACTIVE + 1,
            false,
            false
        ));
        assert!(!priority_set_permitted(-1, false, false));
        assert!(priority_set_permitted(7, false, true));
    }

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
