use alloc::vec::Vec;
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axnet::options::{Configurable, GetSocketOption, SetSocketOption};
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    general::CAP_NET_ADMIN,
    if_packet::tpacket_stats,
    net::{
        AF_INET, AF_PACKET, IPV6_ADDRFORM, SO_ATTACH_BPF, SO_ATTACH_FILTER,
        SO_ATTACH_REUSEPORT_CBPF, SO_ATTACH_REUSEPORT_EBPF, SO_DETACH_BPF, SO_DETACH_REUSEPORT_BPF,
        SO_DOMAIN, SO_ERROR, SO_LOCK_FILTER, SO_PROTOCOL, SO_RCVBUF, SO_RCVBUFFORCE, SO_SNDBUF,
        SO_SNDBUFFORCE, SO_TYPE, SOCK_DGRAM, SOCK_RAW, SOL_IPV6, SOL_NETLINK, SOL_PACKET,
        SOL_SOCKET, socklen_t,
    },
};
use thekernel_linux_packet::{
    GetPacketOption, PacketError, PacketOption, PacketOptionOperation, PacketOptionValue,
    PacketSocketType, SetPacketOption,
};

use super::{SocketSyscallSnapshot, import_socket_output_after_policy};
use crate::{
    file::{
        FileLike, PinnedSocketDescription, SocketBackendKind, af_alg, packet_socket::packet_error,
    },
    mm::{UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
    task::{
        ns_capable,
        security::{SocketOption, SocketSecurityContext, dispatch_socket},
    },
};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;
const IPT_SO_SET_REPLACE: u32 = 64;
const NF_INET_NUMHOOKS: usize = 5;
const XT_EXTENSION_MAXNAMELEN: usize = 29;
const XT_ENTRY_HEADER_SIZE: usize = 2 + XT_EXTENSION_MAXNAMELEN + 1;
const XT_ALIGNMENT: usize = 8;
const IPT_REPLACE_MAX_BYTES: usize = 1 << 20;
const TPACKET_STATS_LEN: usize = size_of::<tpacket_stats>();

const _: [(); 8] = [(); TPACKET_STATS_LEN];
const _: [(); 0] = [(); core::mem::offset_of!(tpacket_stats, tp_packets)];
const _: [(); 4] = [(); core::mem::offset_of!(tpacket_stats, tp_drops)];

fn packet_sol_socket_value(socket_type: PacketSocketType, optname: u32) -> AxResult<i32> {
    match optname {
        SO_TYPE => Ok(match socket_type {
            PacketSocketType::Raw => SOCK_RAW as i32,
            PacketSocketType::Datagram => SOCK_DGRAM as i32,
        }),
        SO_ERROR => Ok(0),
        SO_DOMAIN => Ok(AF_PACKET as i32),
        // Linux reports the generic `sk_protocol` field here. AF_PACKET's
        // EtherType selector is separate bind/send state and must not leak
        // into SO_PROTOCOL.
        SO_PROTOCOL => Ok(0),
        // Buffer accounting/tuning is not backed by the ordinary bounded
        // packet broker yet. Do not report axnet defaults which this OFD does
        // not consume.
        _ => Err(LinuxError::ENOPROTOOPT.into()),
    }
}

fn packet_sol_socket_set_error(optname: u32) -> AxError {
    match optname {
        SO_SNDBUF
        | SO_RCVBUF
        | SO_SNDBUFFORCE
        | SO_RCVBUFFORCE
        | SO_ATTACH_FILTER
        | SO_DETACH_BPF
        | SO_ATTACH_BPF
        | SO_LOCK_FILTER
        | SO_ATTACH_REUSEPORT_CBPF
        | SO_ATTACH_REUSEPORT_EBPF
        | SO_DETACH_REUSEPORT_BPF => LinuxError::EOPNOTSUPP.into(),
        // Read-only introspection and unknown SOL_SOCKET names have no setter
        // in this packet baseline.
        _ => LinuxError::ENOPROTOOPT.into(),
    }
}

fn packet_option_error(error: PacketError) -> AxError {
    match error {
        PacketError::UnknownPacketOption => LinuxError::ENOPROTOOPT.into(),
        PacketError::UnsupportedPacketOption { option, operation } => {
            let has_no_linux_getter = matches!(
                option,
                PacketOption::AddMembership
                    | PacketOption::DropMembership
                    | PacketOption::ReceiveOutput
                    | PacketOption::ReceiveRing
                    | PacketOption::TransmitRing
            );
            let has_no_linux_setter = matches!(
                option,
                PacketOption::Statistics
                    | PacketOption::HeaderLength
                    | PacketOption::RolloverStatistics
            );
            if (operation == PacketOptionOperation::Get && has_no_linux_getter)
                || (operation == PacketOptionOperation::Set && has_no_linux_setter)
            {
                LinuxError::ENOPROTOOPT.into()
            } else {
                LinuxError::EOPNOTSUPP.into()
            }
        }
        other => packet_error(other),
    }
}

fn write_packet_option_bytes(
    capability: &UserMemoryCapability,
    output: UserPtr<u8>,
    length: &mut socklen_t,
    bytes: &[u8],
) -> AxResult<()> {
    let copied = packet_option_copy_len(*length, bytes.len());
    if copied != 0 {
        capability
            .write_bytes(output.address().as_usize(), &bytes[..copied])
            .map_err(map_usercopy_error)?;
    }
    *length = copied as socklen_t;
    Ok(())
}

fn packet_option_copy_len(requested: socklen_t, available: usize) -> usize {
    (requested as usize).min(available)
}

fn option_copy_len(requested: socklen_t, available: usize) -> usize {
    (requested as usize).min(available)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IptReplaceHeader {
    name: [u8; 32],
    valid_hooks: u32,
    num_entries: u32,
    size: u32,
    hook_entry: [u32; NF_INET_NUMHOOKS],
    underflow: [u32; NF_INET_NUMHOOKS],
    num_counters: u32,
    counters: usize,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct IptIpHeader {
    src: u32,
    dst: u32,
    smsk: u32,
    dmsk: u32,
    iniface: [u8; 16],
    outiface: [u8; 16],
    iniface_mask: [u8; 16],
    outiface_mask: [u8; 16],
    proto: u16,
    flags: u8,
    invflags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct XtCounters {
    pcnt: u64,
    bcnt: u64,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct IptEntryHeader {
    ip: IptIpHeader,
    nfcache: u32,
    target_offset: u16,
    next_offset: u16,
    comefrom: u32,
    counters: XtCounters,
}

mod conv {
    use axerrno::{AxError, AxResult};
    use axnet::options::UnixCredentials;
    use linux_raw_sys::{general::timeval, net::ucred};

    use crate::time::TimeValueLike;

    pub struct Int<T>(T);

    impl<T: TryFrom<i32> + TryInto<i32>> Int<T> {
        pub fn sys_to_rust(val: i32) -> AxResult<T> {
            T::try_from(val).map_err(|_| AxError::InvalidInput)
        }

        pub fn rust_to_sys(val: T) -> AxResult<i32> {
            val.try_into().map_err(|_| AxError::InvalidInput)
        }
    }

    pub struct IntBool;

    impl IntBool {
        pub fn sys_to_rust(val: i32) -> AxResult<bool> {
            Ok(val != 0)
        }

        pub fn rust_to_sys(val: bool) -> AxResult<i32> {
            Ok(val as _)
        }
    }

    pub struct Duration;

    impl Duration {
        pub fn sys_to_rust(val: timeval) -> AxResult<core::time::Duration> {
            val.try_into_time_value()
        }

        pub fn rust_to_sys(val: core::time::Duration) -> AxResult<timeval> {
            Ok(timeval::from_time_value(val))
        }
    }

    pub struct Ucred;

    impl Ucred {
        pub fn sys_to_rust(val: ucred) -> AxResult<UnixCredentials> {
            Ok(UnixCredentials {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }

        pub fn rust_to_sys(val: UnixCredentials) -> AxResult<ucred> {
            Ok(ucred {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }
    }
}

macro_rules! call_dispatch {
    ($dispatch:ident, $pat:expr) => {{
        use conv::*;
        use linux_raw_sys::net::*;

        call_dispatch! {
            $dispatch, $pat,
            (SOL_SOCKET, SO_REUSEADDR) => ReuseAddress as IntBool,
            (SOL_SOCKET, SO_ERROR) => Error,
            (SOL_SOCKET, SO_DONTROUTE) => DontRoute as IntBool,
            (SOL_SOCKET, SO_SNDBUF) => SendBuffer as Int<usize>,
            (SOL_SOCKET, SO_RCVBUF) => ReceiveBuffer as Int<usize>,
            (SOL_SOCKET, SO_KEEPALIVE) => KeepAlive as IntBool,
            (SOL_SOCKET, SO_RCVTIMEO) => ReceiveTimeout as Duration,
            (SOL_SOCKET, SO_SNDTIMEO) => SendTimeout as Duration,
            (SOL_SOCKET, SO_PASSCRED) => PassCredentials as IntBool,
            (SOL_SOCKET, SO_PEERCRED) => PeerCredentials as Ucred,

            (PROTO_TCP, TCP_NODELAY) => NoDelay as IntBool,
            (PROTO_TCP, TCP_MAXSEG) => MaxSegment as Int<usize>,

            (PROTO_IP, IP_TTL) => Ttl as Int<u8>,
            (SOL_IPV6, IPV6_V6ONLY) => Ipv6Only as IntBool,
        }
    }};
    ($dispatch:ident, $in:expr, $($pat:pat => $which:ident $(as $conv:ty)?),* $(,)?) => {
        match $in {
            $(
                $pat => {
                    dispatch!($which $(as $conv)?);
                }
            )*
            _ => return Err(AxError::from(LinuxError::ENOPROTOOPT)),
        }
    }
}

fn xt_align(size: usize) -> AxResult<usize> {
    size.checked_add(XT_ALIGNMENT - 1)
        .map(|value| value & !(XT_ALIGNMENT - 1))
        .ok_or(AxError::InvalidInput)
}

fn validate_xt_entry_size(size: usize, min_size: usize) -> AxResult<()> {
    if size < min_size || !size.is_multiple_of(XT_ALIGNMENT) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn xt_entry_size(table: &[u8], offset: usize) -> AxResult<usize> {
    let raw = table
        .get(offset..offset.checked_add(2).ok_or(AxError::InvalidInput)?)
        .ok_or(AxError::InvalidInput)?;
    Ok(u16::from_ne_bytes([raw[0], raw[1]]) as usize)
}

fn xt_entry_name(table: &[u8], offset: usize) -> AxResult<&[u8]> {
    let name_start = offset.checked_add(2).ok_or(AxError::InvalidInput)?;
    let name_end = name_start
        .checked_add(XT_EXTENSION_MAXNAMELEN)
        .ok_or(AxError::InvalidInput)?;
    table.get(name_start..name_end).ok_or(AxError::InvalidInput)
}

fn xt_name_eq(raw: &[u8], name: &[u8]) -> bool {
    raw.starts_with(name) && raw.get(name.len()).is_none_or(|byte| *byte == 0)
}

fn validate_xt_reject_target(
    table: &[u8],
    target_offset: usize,
    target_size: usize,
) -> AxResult<()> {
    let name = xt_entry_name(table, target_offset)?;
    if xt_name_eq(name, b"REJECT") {
        let reject_min_size = xt_align(XT_ENTRY_HEADER_SIZE + size_of::<u32>())?;
        if target_size < reject_min_size {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(())
}

fn validate_ipt_replace_table(table: &[u8], num_entries: u32) -> AxResult<()> {
    let mut offset = 0usize;
    let mut entries = 0u32;
    let entry_header_len = size_of::<IptEntryHeader>();

    while offset < table.len() {
        let entry_end = offset
            .checked_add(entry_header_len)
            .ok_or(AxError::InvalidInput)?;
        let entry_bytes = table.get(offset..entry_end).ok_or(AxError::InvalidInput)?;
        let entry: IptEntryHeader = bytemuck::pod_read_unaligned(entry_bytes);
        let target_offset = entry.target_offset as usize;
        let next_offset = entry.next_offset as usize;

        if target_offset < entry_header_len
            || target_offset > next_offset
            || !target_offset.is_multiple_of(XT_ALIGNMENT)
            || !next_offset.is_multiple_of(XT_ALIGNMENT)
        {
            return Err(AxError::InvalidInput);
        }

        let absolute_target = offset
            .checked_add(target_offset)
            .ok_or(AxError::InvalidInput)?;
        let absolute_next = offset
            .checked_add(next_offset)
            .ok_or(AxError::InvalidInput)?;
        if absolute_next > table.len() {
            return Err(AxError::InvalidInput);
        }

        let mut match_offset = entry_end;
        while match_offset < absolute_target {
            let match_size = xt_entry_size(table, match_offset)?;
            validate_xt_entry_size(match_size, XT_ENTRY_HEADER_SIZE)?;
            match_offset = match_offset
                .checked_add(match_size)
                .ok_or(AxError::InvalidInput)?;
            if match_offset > absolute_target {
                return Err(AxError::InvalidInput);
            }
        }
        if match_offset != absolute_target {
            return Err(AxError::InvalidInput);
        }

        let target_size = xt_entry_size(table, absolute_target)?;
        validate_xt_entry_size(target_size, XT_ENTRY_HEADER_SIZE)?;
        validate_xt_reject_target(table, absolute_target, target_size)?;
        if absolute_target
            .checked_add(target_size)
            .ok_or(AxError::InvalidInput)?
            != absolute_next
        {
            return Err(AxError::InvalidInput);
        }

        entries = entries.checked_add(1).ok_or(AxError::InvalidInput)?;
        offset = absolute_next;
    }

    if entries != num_entries {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn read_option<T: Copy>(
    capability: &UserMemoryCapability,
    val: UserConstPtr<u8>,
    len: socklen_t,
) -> AxResult<T> {
    if len as usize != size_of::<T>() {
        return Err(AxError::InvalidInput);
    }
    capability
        .read_value_uninit(val.address().as_usize() as *const T)
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })
}

fn write_option<T: Copy>(
    capability: &UserMemoryCapability,
    val: UserPtr<u8>,
    len: &mut socklen_t,
    value: T,
) -> AxResult<()> {
    let copied = option_copy_len(*len, size_of::<T>());
    if copied != 0 {
        capability
            .write_bytes(val.address().as_usize(), unsafe {
                core::slice::from_raw_parts((&value as *const T).cast::<u8>(), copied)
            })
            .map_err(map_usercopy_error)?;
    }
    *len = copied as socklen_t;
    Ok(())
}

fn handle_ipt_set_replace(
    capability: &UserMemoryCapability,
    optval: UserConstPtr<u8>,
) -> AxResult<isize> {
    let header =
        read_option::<IptReplaceHeader>(capability, optval, size_of::<IptReplaceHeader>() as _)?;
    if header.num_counters == 0 {
        return Err(AxError::InvalidInput);
    }

    let header_len = size_of::<IptReplaceHeader>();
    let table_len = header.size as usize;
    let total_len = header_len
        .checked_add(table_len)
        .ok_or(AxError::InvalidInput)?;
    if total_len > IPT_REPLACE_MAX_BYTES {
        return Err(AxError::InvalidInput);
    }

    let mut replace = Vec::new();
    replace
        .try_reserve_exact(total_len)
        .map_err(|_| AxError::NoMemory)?;
    replace.resize(total_len, 0);
    capability
        .read_slice(optval.address().as_usize() as *const u8, unsafe {
            core::slice::from_raw_parts_mut(
                replace.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                total_len,
            )
        })
        .map_err(map_usercopy_error)?;
    validate_ipt_replace_table(&replace[header_len..], header.num_entries)?;

    Err(AxError::from(LinuxError::ENOPROTOOPT))
}

pub fn sys_getsockopt(
    capability: UserMemoryCapability,
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserPtr<u8>,
    optlen_ptr: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    let mut optlen = import_socket_output_after_policy(
        || {
            dispatch_socket(&SocketSecurityContext::get_option(
                snapshot.actor(),
                &socket_ref,
                SocketOption::new(level as i32, optname as i32),
            ))
        },
        || {
            capability
                .read_value(optlen_ptr.address().as_usize() as *const socklen_t)
                .map_err(map_usercopy_error)
        },
    )?;
    debug!(
        "sys_getsockopt <= fd: {}, level: {}, optname: {}, optval: {:?}, optlen: {}",
        fd,
        level,
        optname,
        optval.address(),
        optlen,
    );

    if optlen > i32::MAX as socklen_t {
        return Err(AxError::InvalidInput);
    }
    if pinned.backend()? == SocketBackendKind::Netlink {
        if level != SOL_NETLINK {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        let value = pinned.netlink()?.get_option(optname)?;
        write_option(&capability, optval, &mut optlen, value)?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }

    if pinned.backend()? == SocketBackendKind::Packet {
        if level == SOL_SOCKET {
            let value = packet_sol_socket_value(pinned.packet()?.socket_type(), optname)?;
            write_packet_option_bytes(&capability, optval, &mut optlen, &value.to_ne_bytes())?;
            capability
                .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
                .map_err(map_usercopy_error)?;
            return Ok(0);
        }
        if level != SOL_PACKET {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        let option = GetPacketOption::decode(optname as i32).map_err(packet_option_error)?;
        // For PACKET_STATISTICS this call is the sole destructive reset and
        // deliberately precedes optval copyout. A later EFAULT still consumes
        // the snapshot, matching Linux.
        let value = pinned.packet()?.get_packet_option(option);
        match value {
            PacketOptionValue::IgnoreOutgoing(enabled) => {
                write_packet_option_bytes(
                    &capability,
                    optval,
                    &mut optlen,
                    &i32::from(enabled).to_ne_bytes(),
                )?;
            }
            PacketOptionValue::Statistics(statistics) => {
                // Linux's native counters are u32 and wrap at that UAPI
                // boundary; do not invent saturation or drop reasons. Build
                // the asserted native layout bytewise so no Rust padding can
                // ever become userspace-visible.
                let mut native = [0_u8; TPACKET_STATS_LEN];
                native[..4].copy_from_slice(&(statistics.packets() as u32).to_ne_bytes());
                native[4..].copy_from_slice(&(statistics.drops() as u32).to_ne_bytes());
                write_packet_option_bytes(&capability, optval, &mut optlen, &native)?;
            }
        }
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }

    let socket = pinned.network()?;
    macro_rules! dispatch {
        ($which:ident) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::$which(&mut val))?;
            write_option(&capability, optval, &mut optlen, val)?;
        };
        ($which:ident as $conv:ty) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::$which(&mut val))?;
            write_option(&capability, optval, &mut optlen, <$conv>::rust_to_sys(val)?)?;
        };
    }
    match level {
        SOL_SOCKET | PROTO_TCP | PROTO_IP | SOL_IPV6 => {
            call_dispatch!(dispatch, (level, optname))
        }
        _ => return Err(AxError::from(LinuxError::EOPNOTSUPP)),
    }

    capability
        .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
        .map_err(map_usercopy_error)?;
    Ok(0)
}

pub fn sys_setsockopt(
    capability: UserMemoryCapability,
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> AxResult<isize> {
    let snapshot = SocketSyscallSnapshot::capture();
    debug!(
        "sys_setsockopt <= fd: {}, level: {}, optname: {}, optval: {:?}, optlen: {}",
        fd,
        level,
        optname,
        optval.address(),
        optlen
    );

    let pinned = PinnedSocketDescription::from_fd(fd)?;
    let socket_ref = pinned.security_ref()?;
    dispatch_socket(&SocketSecurityContext::set_option(
        snapshot.actor(),
        &socket_ref,
        SocketOption::new(level as i32, optname as i32),
    ))?;
    if pinned.backend()? == SocketBackendKind::AfAlg {
        if level != af_alg::SOL_ALG {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        if optname != af_alg::ALG_SET_KEY {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        if optlen as usize > IPT_REPLACE_MAX_BYTES {
            return Err(AxError::InvalidInput);
        }
        let mut key = Vec::new();
        key.try_reserve_exact(optlen as usize)
            .map_err(|_| AxError::NoMemory)?;
        key.resize(optlen as usize, 0);
        capability
            .read_slice(optval.address().as_usize() as *const u8, unsafe {
                core::slice::from_raw_parts_mut(
                    key.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                    key.len(),
                )
            })
            .map_err(map_usercopy_error)?;
        pinned.af_alg()?.set_alg_key(&key)?;
        return Ok(0);
    }

    if pinned.backend()? == SocketBackendKind::Netlink {
        if level != SOL_NETLINK {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        pinned
            .netlink()?
            .set_option(optname, read_option::<u32>(&capability, optval, optlen)?)?;
        return Ok(0);
    }

    if pinned.backend()? == SocketBackendKind::Packet {
        if level == SOL_SOCKET {
            return Err(packet_sol_socket_set_error(optname));
        }
        if level != SOL_PACKET {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        let option = PacketOption::from_raw(optname as i32).map_err(packet_option_error)?;
        if option != PacketOption::IgnoreOutgoing {
            return Err(packet_option_error(PacketError::UnsupportedPacketOption {
                option,
                operation: PacketOptionOperation::Set,
            }));
        }
        let value = read_option::<i32>(&capability, optval, optlen)?;
        let option = SetPacketOption::decode(optname as i32, value).map_err(packet_option_error)?;
        pinned.packet()?.set_packet_option(option)?;
        return Ok(0);
    }

    let socket = pinned.network()?;
    if level == SOL_IPV6 && optname == IPV6_ADDRFORM {
        if read_option::<i32>(&capability, optval, optlen)? as u32 != AF_INET {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        socket.set_ipv6_addrform_to_ipv4()?;
        return Ok(0);
    }
    if level == PROTO_IP && optname == IPT_SO_SET_REPLACE {
        return handle_ipt_set_replace(&capability, optval);
    }
    if level == SOL_SOCKET {
        match optname {
            SO_SNDBUFFORCE => {
                if !ns_capable(
                    snapshot.actor(),
                    socket.net_namespace().owner_user_ns(),
                    CAP_NET_ADMIN,
                ) {
                    return Err(LinuxError::EPERM.into());
                }
                let size = (read_option::<u32>(&capability, optval, optlen)? as usize)
                    .min(i32::MAX as usize);
                socket.set_option(SetSocketOption::SendBufferForce(&size))?;
                return Ok(0);
            }
            SO_RCVBUFFORCE => {
                if !ns_capable(
                    snapshot.actor(),
                    socket.net_namespace().owner_user_ns(),
                    CAP_NET_ADMIN,
                ) {
                    return Err(LinuxError::EPERM.into());
                }
                let size = (read_option::<u32>(&capability, optval, optlen)? as usize)
                    .min(i32::MAX as usize);
                socket.set_option(SetSocketOption::ReceiveBufferForce(&size))?;
                return Ok(0);
            }
            SO_ATTACH_BPF => {
                let prog_fd = read_option::<i32>(&capability, optval, optlen)?;
                let prog_fd = crate::file::bpf::BpfProgFd::from_fd(prog_fd)?;
                if prog_fd.prog.prog_type != crate::bpf::defs::BPF_PROG_TYPE_SOCKET_FILTER {
                    return Err(AxError::InvalidInput);
                }
                socket.set_bpf_filter(Some(prog_fd.prog.clone()))?;
                return Ok(0);
            }
            SO_DETACH_BPF => {
                socket.set_bpf_filter(None)?;
                return Ok(0);
            }
            _ => {}
        }
    }
    macro_rules! dispatch {
        ($which:ident) => {
            let val = read_option(&capability, optval, optlen)?;
            socket.set_option(SetSocketOption::$which(&val))?;
        };
        ($which:ident as $conv:ty) => {
            let raw = read_option(&capability, optval, optlen)?;
            let mut val = <$conv>::sys_to_rust(raw)?;
            socket.set_option(SetSocketOption::$which(&mut val))?;
        };
    }
    match level {
        SOL_SOCKET | PROTO_TCP | PROTO_IP | SOL_IPV6 => {
            call_dispatch!(dispatch, (level, optname))
        }
        _ => return Err(AxError::from(LinuxError::ENOPROTOOPT)),
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errno(error: AxError) -> LinuxError {
        LinuxError::from(error)
    }

    #[test]
    fn packet_sol_socket_introspection_is_typed_and_does_not_leak_ethertype() {
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Raw, SO_TYPE),
            Ok(SOCK_RAW as i32)
        );
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Datagram, SO_TYPE),
            Ok(SOCK_DGRAM as i32)
        );
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Raw, SO_ERROR),
            Ok(0)
        );
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Raw, SO_DOMAIN),
            Ok(AF_PACKET as i32)
        );
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Raw, SO_PROTOCOL),
            Ok(0)
        );
        assert_eq!(
            packet_sol_socket_value(PacketSocketType::Raw, SO_RCVBUF).map_err(errno),
            Err(LinuxError::ENOPROTOOPT)
        );
    }

    #[test]
    fn packet_sol_socket_mutation_is_an_explicit_nonclaim() {
        assert_eq!(
            errno(packet_sol_socket_set_error(SO_RCVBUF)),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            errno(packet_sol_socket_set_error(SO_ATTACH_BPF)),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            errno(packet_sol_socket_set_error(SO_TYPE)),
            LinuxError::ENOPROTOOPT
        );
        assert_eq!(
            errno(packet_sol_socket_set_error(u32::MAX)),
            LinuxError::ENOPROTOOPT
        );
    }

    #[test]
    fn packet_integer_options_accept_and_report_short_copy_lengths() {
        assert_eq!(packet_option_copy_len(0, size_of::<i32>()), 0);
        assert_eq!(packet_option_copy_len(1, size_of::<i32>()), 1);
        assert_eq!(packet_option_copy_len(2, size_of::<i32>()), 2);
        assert_eq!(packet_option_copy_len(3, size_of::<i32>()), 3);
        assert_eq!(packet_option_copy_len(4, size_of::<i32>()), 4);
        assert_eq!(packet_option_copy_len(99, size_of::<i32>()), 4);
    }

    #[test]
    fn generic_integer_options_accept_zero_and_short_optlen() {
        assert_eq!(option_copy_len(0, size_of::<i32>()), 0);
        assert_eq!(option_copy_len(1, size_of::<i32>()), 1);
        assert_eq!(option_copy_len(2, size_of::<i32>()), 2);
        assert_eq!(option_copy_len(3, size_of::<i32>()), 3);
        assert_eq!(option_copy_len(4, size_of::<i32>()), 4);
        assert_eq!(option_copy_len(99, size_of::<i32>()), 4);
    }

    #[test]
    fn generic_option_copyout_reports_the_copied_short_or_zero_length() {
        use alloc::sync::Arc;

        use axhal::paging::{MappingFlags, PageSize};
        use axsync::Mutex;
        use memory_addr::{PAGE_SIZE_4K, VirtAddr};

        let mut address_space =
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap();
        address_space
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        let capability = UserMemoryCapability::new(Arc::new(Mutex::new(address_space)));

        let value = 0x0102_0304_i32;
        let mut short = 1;
        write_option(&capability, UserPtr::from(0x1000), &mut short, value).unwrap();
        assert_eq!(short, 1);
        let mut copied = [core::mem::MaybeUninit::<u8>::uninit(); 1];
        capability.read_bytes(0x1000, &mut copied).unwrap();
        assert_eq!(unsafe { copied[0].assume_init() }, value.to_ne_bytes()[0]);

        let mut zero = 0;
        write_option(&capability, UserPtr::from(usize::MAX), &mut zero, value).unwrap();
        assert_eq!(zero, 0);
    }

    #[test]
    fn packet_option_classifier_distinguishes_unknown_absent_and_deferred_surfaces() {
        assert_eq!(
            errno(packet_option_error(PacketError::UnknownPacketOption)),
            LinuxError::ENOPROTOOPT
        );
        assert_eq!(
            errno(packet_option_error(PacketError::UnsupportedPacketOption {
                option: PacketOption::ReceiveRing,
                operation: PacketOptionOperation::Get,
            })),
            LinuxError::ENOPROTOOPT
        );
        assert_eq!(
            errno(packet_option_error(PacketError::UnsupportedPacketOption {
                option: PacketOption::Statistics,
                operation: PacketOptionOperation::Set,
            })),
            LinuxError::ENOPROTOOPT
        );
        assert_eq!(
            errno(packet_option_error(PacketError::UnsupportedPacketOption {
                option: PacketOption::Fanout,
                operation: PacketOptionOperation::Set,
            })),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            errno(packet_option_error(PacketError::InvalidPacketOptionValue)),
            LinuxError::EINVAL
        );
    }
}
