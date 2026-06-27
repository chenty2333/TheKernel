use axerrno::{AxError, AxResult, LinuxError};
use axnet::options::{Configurable, GetSocketOption, SetSocketOption};
use bytemuck::AnyBitPattern;
use linux_raw_sys::net::{
    AF_INET, IPV6_ADDRFORM, SO_ATTACH_BPF, SO_DETACH_BPF, SO_NO_CHECK, SO_OOBINLINE,
    SO_SNDBUFFORCE, SOL_DCCP, SOL_IPV6, SOL_NETLINK, SOL_SOCKET, SOL_TLS, TCP_INFO, TCP_ULP,
    socklen_t, tcp_info,
};

use crate::{
    file::{
        AfAlgSocket, FileLike, NetlinkSocket, PacketSocket, Socket, af_alg, get_file_like,
        packet::{
            PACKET_FANOUT, PACKET_RESERVE, PACKET_RX_RING, PACKET_VERSION, PACKET_VNET_HDR,
            SOL_PACKET, TpacketReq, TpacketReq3,
        },
    },
    mm::{UserConstPtr, UserPtr},
};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;
const SO_RXQ_OVFL_COMPAT: u32 = 40;
const TCP_ESTABLISHED: u8 = 1;
const DEFAULT_TCP_MSS: u32 = 1460;
const DEFAULT_TCP_CWND: u32 = 10;
const DEFAULT_TCP_RTO_US: u32 = 200_000;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;
const TLS_TX: u32 = 1;
const DCCP_SOCKOPT_SERVICE: u32 = 2;
const IPT_SO_SET_REPLACE: u32 = 64;
const MCAST_JOIN_GROUP: u32 = 42;
const MCAST_LEAVE_GROUP: u32 = 45;
const NF_INET_NUMHOOKS: usize = 5;
const XT_EXTENSION_MAXNAMELEN: usize = 29;
const XT_ENTRY_HEADER_SIZE: usize = 2 + XT_EXTENSION_MAXNAMELEN + 1;
const XT_ALIGNMENT: usize = 8;
const IPT_REPLACE_MAX_BYTES: usize = 1 << 20;

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

fn socket_fd_error(fd: i32, err: AxError) -> AxError {
    if err != AxError::BadFileDescriptor || get_file_like(fd).is_err() {
        err
    } else {
        AxError::NotASocket
    }
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
            (PROTO_TCP, TCP_INFO) => TcpInfo,

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

fn xt_entry_name<'a>(table: &'a [u8], offset: usize) -> AxResult<&'a [u8]> {
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

fn handle_ipt_set_replace(optval: UserConstPtr<u8>) -> AxResult<isize> {
    let header = optval.cast::<IptReplaceHeader>().get_as_ref()?;
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

    let replace = optval.get_as_slice(total_len)?;
    validate_ipt_replace_table(&replace[header_len..], header.num_entries)?;

    Err(AxError::from(LinuxError::ENOPROTOOPT))
}

fn get_tcp_info(fd: i32, optval: UserPtr<u8>, optlen: &mut socklen_t) -> AxResult<isize> {
    let socket = Socket::from_fd(fd).map_err(|err| socket_fd_error(fd, err))?;
    if !socket.is_tcp() {
        return Err(AxError::from(LinuxError::ENOPROTOOPT));
    }

    let user_len = *optlen as usize;
    if user_len == 0 {
        return Err(AxError::InvalidInput);
    }

    let mut info = tcp_info {
        tcpi_state: TCP_ESTABLISHED,
        tcpi_rto: DEFAULT_TCP_RTO_US,
        tcpi_snd_mss: DEFAULT_TCP_MSS,
        tcpi_rcv_mss: DEFAULT_TCP_MSS,
        tcpi_pmtu: DEFAULT_TCP_MSS,
        tcpi_rcv_ssthresh: u32::MAX,
        tcpi_snd_ssthresh: u32::MAX,
        tcpi_snd_cwnd: DEFAULT_TCP_CWND,
        tcpi_advmss: DEFAULT_TCP_MSS,
        tcpi_reordering: 3,
        tcpi_rcv_space: 64 * 1024,
        ..unsafe { core::mem::zeroed() }
    };

    let copy_len = user_len.min(size_of::<tcp_info>());
    let bytes = unsafe {
        core::slice::from_raw_parts((&mut info as *mut tcp_info).cast::<u8>(), copy_len)
    };
    optval.get_as_mut_slice(copy_len)?.copy_from_slice(bytes);
    *optlen = copy_len as socklen_t;
    Ok(0)
}

pub fn sys_getsockopt(
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserPtr<u8>,
    optlen: UserPtr<socklen_t>,
) -> AxResult<isize> {
    let optlen = optlen.get_as_mut()?;
    debug!(
        "sys_getsockopt <= fd: {}, level: {}, optname: {}, optval: {:?}, optlen: {}",
        fd,
        level,
        optname,
        optval.address(),
        optlen,
    );

    if *optlen > i32::MAX as socklen_t {
        return Err(AxError::InvalidInput);
    }
    if level == PROTO_TCP && optname == TCP_INFO {
        return get_tcp_info(fd, optval, optlen);
    }

    fn get<'a, T: 'static>(val: UserPtr<u8>, len: &mut socklen_t) -> AxResult<&'a mut T> {
        if (*len as usize) < size_of::<T>() {
            return Err(AxError::InvalidInput);
        }
        *len = size_of::<T>() as socklen_t;
        val.cast().get_as_mut()
    }

    if let Ok(socket) = PacketSocket::from_fd(fd) {
        if level != SOL_PACKET {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }

        match optname {
            PACKET_VERSION => {
                *get::<i32>(optval, optlen)? = socket.packet_version();
            }
            PACKET_RESERVE => {
                *get::<u32>(optval, optlen)? = socket.packet_reserve();
            }
            _ => return Err(AxError::from(LinuxError::ENOPROTOOPT)),
        }
        return Ok(0);
    }

    if let Ok(socket) = NetlinkSocket::from_fd(fd) {
        if level != SOL_NETLINK {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        let value = *get::<u32>(optval, optlen)?;
        socket.set_option(optname, value)?;
        return Ok(0);
    }

    let socket = Socket::from_fd(fd).map_err(|err| socket_fd_error(fd, err))?;
    if level == SOL_SOCKET && optname == SO_OOBINLINE {
        *get::<i32>(optval, optlen)? = 0;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_RXQ_OVFL_COMPAT {
        *get::<i32>(optval, optlen)? = 0;
        return Ok(0);
    }
    macro_rules! dispatch {
        ($which:ident) => {
            socket.get_option(GetSocketOption::$which(get(optval, optlen)?))?;
        };
        ($which:ident as $conv:ty) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::$which(&mut val))?;
            *get(optval, optlen)? = <$conv>::rust_to_sys(val)?;
        };
    }
    match level {
        SOL_SOCKET | PROTO_TCP | PROTO_IP | SOL_IPV6 => {
            call_dispatch!(dispatch, (level, optname))
        }
        _ => return Err(AxError::from(LinuxError::EOPNOTSUPP)),
    }

    Ok(0)
}

pub fn sys_setsockopt(
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> AxResult<isize> {
    debug!(
        "sys_setsockopt <= fd: {}, level: {}, optname: {}, optval: {:?}, optlen: {}",
        fd,
        level,
        optname,
        optval.address(),
        optlen
    );

    fn get<'a, T: 'static>(val: UserConstPtr<u8>, len: socklen_t) -> AxResult<&'a T> {
        if len as usize != size_of::<T>() {
            return Err(AxError::InvalidInput);
        }
        val.cast().get_as_ref()
    }

    if let Ok(socket) = AfAlgSocket::from_fd(fd) {
        if level != af_alg::SOL_ALG {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        if optname != af_alg::ALG_SET_KEY {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        let key = optval.get_as_slice(optlen as usize)?;
        socket.set_alg_key(key)?;
        return Ok(0);
    }

    if let Ok(socket) = PacketSocket::from_fd(fd) {
        if level != SOL_PACKET {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }

        match optname {
            PACKET_VERSION => {
                let version = *get::<i32>(optval, optlen)?;
                socket.set_packet_version(version)?;
            }
            PACKET_RX_RING => {
                let req = if socket.packet_version() == 2 {
                    *get::<TpacketReq3>(optval, optlen)?
                } else {
                    if (optlen as usize) < size_of::<TpacketReq>() {
                        return Err(AxError::InvalidInput);
                    }
                    TpacketReq3::from(*optval.cast::<TpacketReq>().get_as_ref()?)
                };
                socket.set_rx_ring(req)?;
            }
            PACKET_RESERVE => {
                let reserve = *get::<u32>(optval, optlen)?;
                socket.set_packet_reserve(reserve)?;
            }
            PACKET_VNET_HDR => {
                let enabled = *get::<i32>(optval, optlen)? != 0;
                socket.set_vnet_hdr(enabled);
            }
            PACKET_FANOUT => {
                let value = *get::<u32>(optval, optlen)?;
                socket.set_fanout(value);
            }
            _ => return Err(AxError::from(LinuxError::ENOPROTOOPT)),
        }
        return Ok(0);
    }

    let socket = Socket::from_fd(fd).map_err(|err| socket_fd_error(fd, err))?;
    if level == SOL_SOCKET && optname == SO_OOBINLINE {
        let _ = get::<i32>(optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_NO_CHECK {
        let _ = get::<i32>(optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_RXQ_OVFL_COMPAT {
        let _ = get::<i32>(optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_DCCP && optname == DCCP_SOCKOPT_SERVICE {
        let _ = get::<i32>(optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_IPV6 && optname == IPV6_ADDRFORM {
        if *get::<i32>(optval, optlen)? as u32 != AF_INET {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        socket.set_ipv6_addrform_to_ipv4()?;
        return Ok(0);
    }
    if level == PROTO_IP {
        match optname {
            IPT_SO_SET_REPLACE => return handle_ipt_set_replace(optval),
            MCAST_JOIN_GROUP => return Ok(0),
            MCAST_LEAVE_GROUP => return Err(AxError::from(LinuxError::EADDRNOTAVAIL)),
            _ => {}
        }
    }
    if level == PROTO_TCP && optname == TCP_ULP {
        if !socket.is_tcp() {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        if optlen == 0 {
            return Err(AxError::InvalidInput);
        }
        let name = optval.get_as_slice(optlen as usize)?;
        let name = name.split(|byte| *byte == 0).next().unwrap_or(name);
        if name == b"tls" {
            socket.set_tcp_tls_ulp();
            return Ok(0);
        }
        return Err(AxError::from(LinuxError::ENOENT));
    }
    if level == SOL_TLS && optname == TLS_TX {
        if !socket.is_tcp() {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        if !socket.has_tcp_tls_ulp() {
            return Err(AxError::from(LinuxError::ENOPROTOOPT));
        }
        let _ = optval.get_as_slice(optlen as usize)?;
        return Ok(0);
    }
    if level == SOL_SOCKET {
        match optname {
            SO_SNDBUFFORCE => {
                let size = (*get::<u32>(optval, optlen)? as usize).min(i32::MAX as usize);
                socket.set_option(SetSocketOption::SendBufferForce(&size))?;
                return Ok(0);
            }
            SO_ATTACH_BPF => {
                let prog_fd = *get::<i32>(optval, optlen)?;
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
            socket.set_option(SetSocketOption::$which(get(optval, optlen)?))?;
        };
        ($which:ident as $conv:ty) => {
            let mut val = <$conv>::sys_to_rust(*get(optval, optlen)?)?;
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
