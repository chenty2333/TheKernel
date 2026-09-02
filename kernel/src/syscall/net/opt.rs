use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use axnet::{
    Ipv6AddrFormError, Socket as AxSocket,
    options::{Configurable, GetSocketOption, SetSocketOption},
};
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    general::CAP_NET_ADMIN,
    if_packet::{PACKET_RX_RING, PACKET_TX_RING, tpacket_req, tpacket_stats},
    net::{
        AF_INET, AF_INET6, AF_PACKET, IP_HDRINCL, IPV6_ADDRFORM, SO_ATTACH_BPF, SO_ATTACH_FILTER,
        SO_ATTACH_REUSEPORT_CBPF, SO_ATTACH_REUSEPORT_EBPF, SO_DETACH_BPF, SO_DETACH_REUSEPORT_BPF,
        SO_DOMAIN, SO_ERROR, SO_LOCK_FILTER, SO_PASSCRED, SO_PROTOCOL, SO_RCVBUF, SO_RCVBUFFORCE,
        SO_SNDBUF, SO_SNDBUFFORCE, SO_TYPE, SOCK_DGRAM, SOCK_RAW, SOL_IPV6, SOL_NETLINK,
        SOL_PACKET, SOL_SOCKET, socklen_t,
    },
};
use spin::{Lazy, Mutex, MutexGuard};
use thekernel_linux_net::{
    RawSocketOption, SocketOption as LinuxSocketOption, SocketOptionErrno, UcredWire,
    plan_get_socket_option, plan_set_socket_option,
};
use thekernel_linux_packet::{
    GetPacketOption, PacketError, PacketOption, PacketOptionOperation, PacketOptionValue,
    PacketSocketType, SetPacketOption,
};

use super::{SocketSyscallSnapshot, import_socket_output_after_policy};
#[cfg(feature = "bpf")]
use crate::file::FileLike;
use crate::{
    file::{
        PinnedSocketDescription, SocketBackendKind, af_alg, af_xdp, packet_socket::packet_error,
    },
    mm::{UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
    task::{
        ns_capable,
        security::{SocketOption, SocketSecurityContext, dispatch_socket},
    },
};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;
const SOCK_DCCP: i32 = 6;
const IPPROTO_DCCP: i32 = 33;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;
const SOL_SCTP: u32 = 132;
const SOL_DCCP: u32 = 269;
const DCCP_SOCKOPT_PACKET_SIZE: u32 = 1;
const DCCP_SOCKOPT_SERVICE: u32 = 2;
const DCCP_SOCKOPT_CHANGE_L: u32 = 3;
const DCCP_SOCKOPT_CHANGE_R: u32 = 4;
const DCCP_SOCKOPT_GET_CUR_MPS: u32 = 5;
const DCCP_SOCKOPT_SERVER_TIMEWAIT: u32 = 6;
const DCCP_SOCKOPT_SEND_CSCOV: u32 = 10;
const DCCP_SOCKOPT_RECV_CSCOV: u32 = 11;
const DCCP_SOCKOPT_AVAILABLE_CCIDS: u32 = 12;
const DCCP_SOCKOPT_CCID: u32 = 13;
const DCCP_SOCKOPT_TX_CCID: u32 = 14;
const DCCP_SOCKOPT_RX_CCID: u32 = 15;
const DCCP_SOCKOPT_QPOLICY_ID: u32 = 16;
const DCCP_SOCKOPT_QPOLICY_TXQLEN: u32 = 17;
const DCCP_SOCKOPT_CCID_RX_INFO: u32 = 128;
const DCCP_SOCKOPT_CCID_TX_INFO: u32 = 192;
const SCTP_RTOINFO: u32 = 0;
const SCTP_ASSOCINFO: u32 = 1;
const SCTP_INITMSG: u32 = 2;
const SCTP_NODELAY: u32 = 3;
const SCTP_AUTOCLOSE: u32 = 4;
const SCTP_EVENTS: u32 = 11;
const SCTP_EVENT: u32 = 127;
const SCTP_RECVRCVINFO: u32 = 32;
const SCTP_RECVNXTINFO: u32 = 33;
const SCTP_SOCKOPT_BINDX_ADD: u32 = 100;
const SCTP_SOCKOPT_BINDX_REM: u32 = 101;
const SCTP_SOCKOPT_CONNECTX_OLD: u32 = 107;
const SCTP_SOCKOPT_CONNECTX: u32 = 110;
const SCTP_SOCKOPT_CONNECTX3: u32 = 111;

fn read_xdp_umem(
    capability: &UserMemoryCapability,
    value: UserConstPtr<u8>,
    length: socklen_t,
) -> AxResult<af_xdp::XdpUmemLayout> {
    if length as usize != 32 {
        return Err(AxError::InvalidInput);
    }
    let mut bytes = [0u8; 32];
    capability
        .read_slice(value.address().as_usize() as *const u8, unsafe {
            core::slice::from_raw_parts_mut(
                bytes.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                bytes.len(),
            )
        })
        .map_err(map_usercopy_error)?;
    Ok(af_xdp::XdpUmemLayout {
        address: u64::from_ne_bytes(bytes[0..8].try_into().unwrap()),
        length: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
        chunk_size: u32::from_ne_bytes(bytes[16..20].try_into().unwrap()),
        headroom: u32::from_ne_bytes(bytes[20..24].try_into().unwrap()),
        flags: u32::from_ne_bytes(bytes[24..28].try_into().unwrap()),
        tx_metadata_len: u32::from_ne_bytes(bytes[28..32].try_into().unwrap()),
    })
}
const IPT_SO_SET_REPLACE: u32 = 64;
const IPT_SO_GET_ENTRIES: u32 = 65;
const IPT_SO_GET_INFO: u32 = 64;
const NF_INET_NUMHOOKS: usize = 5;
const XT_EXTENSION_MAXNAMELEN: usize = 29;
const XT_ENTRY_HEADER_SIZE: usize = 2 + XT_EXTENSION_MAXNAMELEN + 1;
const XT_ALIGNMENT: usize = 8;
const IPT_REPLACE_MAX_BYTES: usize = 1 << 20;
const TPACKET_STATS_LEN: usize = size_of::<tpacket_stats>();
const PACKET_FANOUT: u32 = 18;
const PACKET_VERSION: u32 = 10;

const _: [(); 8] = [(); TPACKET_STATS_LEN];
const _: [(); 0] = [(); core::mem::offset_of!(tpacket_stats, tp_packets)];
const _: [(); 4] = [(); core::mem::offset_of!(tpacket_stats, tp_drops)];

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
struct SctpGetaddrsOld {
    _assoc_id: i32,
    address_bytes: i32,
    addresses: usize,
}

const _: [(); 16] = [(); size_of::<SctpGetaddrsOld>()];

#[derive(Clone, Copy, Eq, PartialEq)]
enum IptVerdict {
    Accept,
    Drop,
    Reject,
}
#[derive(Clone, Copy)]
struct IptRule {
    offset: u32,
    next: u32,
    verdict: IptVerdict,
}
struct IptTable {
    namespace: Weak<crate::task::NetworkNamespace>,
    name: [u8; 32],
    valid_hooks: u32,
    hook_entry: [u32; NF_INET_NUMHOOKS],
    underflow: [u32; NF_INET_NUMHOOKS],
    entries: Vec<u8>,
    verdicts: Vec<IptVerdict>,
    rules: Vec<IptRule>,
}
static IPTABLES: Lazy<Mutex<Vec<IptTable>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Retained legacy-filter OUTPUT admission for a packet submission.
///
/// The guard is intentionally held through source import and lower packet
/// submit.  A NOWAIT call therefore either obtains all of its shared
/// admission before touching a TX ring/user source, or returns EAGAIN with no
/// packet-side mutation.  Packet code may only consume this permit; it must
/// not reacquire `IPTABLES` further down the transmit stack.
pub(crate) struct IptablesOutputPermit<'a> {
    tables: MutexGuard<'a, Vec<IptTable>>,
    namespace: &'a Arc<crate::task::NetworkNamespace>,
}

impl IptablesOutputPermit<'_> {
    pub(crate) fn verify(&mut self) -> AxResult<()> {
        self.tables
            .retain(|table| table.namespace.strong_count() != 0);
        for table in self
            .tables
            .iter()
            .filter(|table| Weak::ptr_eq(&table.namespace, &Arc::downgrade(self.namespace)))
        {
            if table.valid_hooks & (1u32 << 3) == 0 {
                continue;
            }
            let mut cursor = table.hook_entry[3];
            let underflow = table.underflow[3];
            let mut steps = 0usize;
            loop {
                if steps >= table.rules.len() {
                    return Err(AxError::InvalidInput);
                }
                steps += 1;
                let rule = table
                    .rules
                    .iter()
                    .find(|rule| rule.offset == cursor)
                    .ok_or(AxError::InvalidInput)?;
                match rule.verdict {
                    IptVerdict::Drop => return Err(LinuxError::EPERM.into()),
                    IptVerdict::Reject => return Err(LinuxError::ECONNREFUSED.into()),
                    IptVerdict::Accept if cursor == underflow => break,
                    IptVerdict::Accept => {
                        cursor = cursor.checked_add(rule.next).ok_or(AxError::InvalidInput)?
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn acquire_iptables_output_permit(
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<IptablesOutputPermit<'_>> {
    let mut permit = IptablesOutputPermit {
        tables: IPTABLES.lock(),
        namespace,
    };
    permit.verify()?;
    Ok(permit)
}

pub(crate) fn try_acquire_iptables_output_permit(
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<IptablesOutputPermit<'_>> {
    let tables = IPTABLES.try_lock().ok_or(AxError::WouldBlock)?;
    let mut permit = IptablesOutputPermit { tables, namespace };
    permit.verify()?;
    Ok(permit)
}

fn socket_option_errno(error: SocketOptionErrno) -> AxError {
    match error {
        SocketOptionErrno::NoProtocolOption => LinuxError::ENOPROTOOPT.into(),
        SocketOptionErrno::OperationNotSupported => LinuxError::EOPNOTSUPP.into(),
    }
}

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
    use axnet::options::{SocketCredentials, SocketFault};
    use linux_raw_sys::general::timeval;
    use thekernel_linux_net::UcredWire;

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
        pub fn sys_to_rust(val: UcredWire) -> AxResult<SocketCredentials> {
            Ok(SocketCredentials {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }

        pub fn rust_to_sys(val: SocketCredentials) -> AxResult<UcredWire> {
            Ok(UcredWire {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }
    }

    pub struct SocketError;

    impl SocketError {
        pub fn sys_to_rust(_value: i32) -> AxResult<Option<SocketFault>> {
            // SO_ERROR is read-only in AX. The Linux adapter still performs
            // the required int copyin before the unsupported setter is
            // rejected, but never imports a Linux errno into AX state.
            Ok(None)
        }

        pub fn rust_to_sys(value: Option<SocketFault>) -> AxResult<i32> {
            let failure = match value {
                None => return Ok(0),
                Some(SocketFault::ConnectionRefused) => {
                    thekernel_linux_net::SocketFailure::ConnectionRefused
                }
                Some(SocketFault::ConnectionReset) => {
                    thekernel_linux_net::SocketFailure::ConnectionReset
                }
                Some(SocketFault::TimedOut) => thekernel_linux_net::SocketFailure::TimedOut,
                Some(SocketFault::Other) => thekernel_linux_net::SocketFailure::Io,
            };
            Ok(thekernel_linux_net::socket_failure_errno(failure))
        }
    }
}

fn read_ucred(
    capability: &UserMemoryCapability,
    val: UserConstPtr<u8>,
    len: socklen_t,
) -> AxResult<axnet::options::SocketCredentials> {
    let bytes = read_option::<[u8; UcredWire::SIZE]>(capability, val, len)?;
    let wire = UcredWire::decode(&bytes).map_err(|_| AxError::InvalidInput)?;
    conv::Ucred::sys_to_rust(wire)
}

fn write_ucred(
    capability: &UserMemoryCapability,
    val: UserPtr<u8>,
    len: &mut socklen_t,
    credentials: axnet::options::SocketCredentials,
) -> AxResult<()> {
    let wire = conv::Ucred::rust_to_sys(credentials)?;
    write_packet_option_bytes(capability, val, len, &wire.encode())
}

macro_rules! call_dispatch {
    ($dispatch:ident, $pat:expr) => {{
        use conv::*;

        call_dispatch! {
            $dispatch, $pat,
            LinuxSocketOption::ReuseAddress => ReuseAddress as IntBool,
            LinuxSocketOption::PendingError => Error as SocketError,
            LinuxSocketOption::DontRoute => DontRoute as IntBool,
            LinuxSocketOption::SendBuffer => SendBuffer as Int<usize>,
            LinuxSocketOption::ReceiveBuffer => ReceiveBuffer as Int<usize>,
            LinuxSocketOption::KeepAlive => KeepAlive as IntBool,
            LinuxSocketOption::ReceiveTimeout => ReceiveTimeout as Duration,
            LinuxSocketOption::SendTimeout => SendTimeout as Duration,
            LinuxSocketOption::PassCred => PassCredentials as IntBool,
            LinuxSocketOption::PeerCredentials => PeerCredentials as Ucred,
            LinuxSocketOption::NoDelay => NoDelay as IntBool,
            LinuxSocketOption::MaxSegment => MaxSegment as Int<usize>,
            LinuxSocketOption::TimeToLive => Ttl as Int<u8>,
            LinuxSocketOption::Ipv6Only => Ipv6Only as IntBool,
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

fn ipt_table_verdicts(table: &[u8]) -> AxResult<Vec<IptVerdict>> {
    let mut verdicts = Vec::new();
    let mut offset = 0usize;
    let header = size_of::<IptEntryHeader>();
    while offset < table.len() {
        let entry: IptEntryHeader = bytemuck::pod_read_unaligned(
            table
                .get(offset..offset + header)
                .ok_or(AxError::InvalidInput)?,
        );
        let target = offset
            .checked_add(entry.target_offset as usize)
            .ok_or(AxError::InvalidInput)?;
        let name = xt_entry_name(table, target)?;
        let verdict = if xt_name_eq(name, b"DROP") {
            IptVerdict::Drop
        } else if xt_name_eq(name, b"REJECT") {
            IptVerdict::Reject
        } else {
            IptVerdict::Accept
        };
        verdicts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        verdicts.push(verdict);
        offset = offset
            .checked_add(entry.next_offset as usize)
            .ok_or(AxError::InvalidInput)?;
    }
    Ok(verdicts)
}

fn ipt_table_rules(table: &[u8]) -> AxResult<Vec<IptRule>> {
    let mut rules = Vec::new();
    let mut offset = 0usize;
    let header = size_of::<IptEntryHeader>();
    while offset < table.len() {
        let entry: IptEntryHeader = bytemuck::pod_read_unaligned(
            table
                .get(offset..offset + header)
                .ok_or(AxError::InvalidInput)?,
        );
        let target = offset
            .checked_add(entry.target_offset as usize)
            .ok_or(AxError::InvalidInput)?;
        let name = xt_entry_name(table, target)?;
        let verdict = if xt_name_eq(name, b"DROP") {
            IptVerdict::Drop
        } else if xt_name_eq(name, b"REJECT") {
            IptVerdict::Reject
        } else {
            IptVerdict::Accept
        };
        rules.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        rules.push(IptRule {
            offset: offset as u32,
            next: entry.next_offset as u32,
            verdict,
        });
        offset = offset
            .checked_add(entry.next_offset as usize)
            .ok_or(AxError::InvalidInput)?;
    }
    Ok(rules)
}

/// Applies the installed filter OUTPUT table to one packet emission.  The
/// table is namespace-owned and the verdict is evaluated at every send, so a
/// replace transaction is immediately visible to existing socket OFDs.
pub(crate) fn iptables_output_verdict(
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<()> {
    iptables_hook_verdict(namespace, 3)
}

pub(crate) fn iptables_output_verdict_nowait(
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<()> {
    let mut tables = IPTABLES.try_lock().ok_or(AxError::WouldBlock)?;
    tables.retain(|table| table.namespace.strong_count() != 0);
    for table in tables
        .iter()
        .filter(|table| Weak::ptr_eq(&table.namespace, &Arc::downgrade(namespace)))
    {
        if table.valid_hooks & (1u32 << 3) == 0 {
            continue;
        }
        let mut cursor = table.hook_entry[3];
        let underflow = table.underflow[3];
        let mut steps = 0usize;
        loop {
            if steps >= table.rules.len() {
                return Err(AxError::InvalidInput);
            }
            steps += 1;
            let rule = table
                .rules
                .iter()
                .find(|rule| rule.offset == cursor)
                .ok_or(AxError::InvalidInput)?;
            match rule.verdict {
                IptVerdict::Drop => return Err(LinuxError::EPERM.into()),
                IptVerdict::Reject => return Err(LinuxError::ECONNREFUSED.into()),
                IptVerdict::Accept => {
                    if cursor == underflow {
                        break;
                    }
                    cursor = cursor.checked_add(rule.next).ok_or(AxError::InvalidInput)?;
                }
            }
        }
    }
    Ok(())
}

/// Execute one installed legacy iptables hook.  `hook_entry` and `underflow`
/// are byte offsets in the verified replacement blob, not vector indices;
/// following offsets here preserves the Linux table ABI for all five hooks.
pub(crate) fn iptables_hook_verdict(
    namespace: &Arc<crate::task::NetworkNamespace>,
    hook: usize,
) -> AxResult<()> {
    if hook >= NF_INET_NUMHOOKS {
        return Err(AxError::InvalidInput);
    }
    let mut tables = IPTABLES.lock();
    tables.retain(|table| table.namespace.strong_count() != 0);
    for table in tables
        .iter()
        .filter(|table| Weak::ptr_eq(&table.namespace, &Arc::downgrade(namespace)))
    {
        if table.valid_hooks & (1u32 << hook) == 0 {
            continue;
        }
        let mut cursor = table.hook_entry[hook];
        let underflow = table.underflow[hook];
        let mut steps = 0usize;
        loop {
            if steps >= table.rules.len() {
                return Err(AxError::InvalidInput);
            };
            steps += 1;
            let rule = table
                .rules
                .iter()
                .find(|rule| rule.offset == cursor)
                .ok_or(AxError::InvalidInput)?;
            match rule.verdict {
                IptVerdict::Drop => return Err(LinuxError::EPERM.into()),
                IptVerdict::Reject => return Err(LinuxError::ECONNREFUSED.into()),
                IptVerdict::Accept => {
                    if cursor == underflow {
                        break;
                    }
                    cursor = cursor.checked_add(rule.next).ok_or(AxError::InvalidInput)?;
                }
            }
        }
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

fn read_option_prefix_i32(
    capability: &UserMemoryCapability,
    val: UserConstPtr<u8>,
    len: socklen_t,
) -> AxResult<i32> {
    if (len as usize) < size_of::<i32>() {
        return Err(AxError::InvalidInput);
    }
    capability
        .read_value::<i32>(val.address().as_usize() as *const i32)
        .map_err(map_usercopy_error)
}

fn read_sctp_address_vector(
    capability: &UserMemoryCapability,
    value: UserConstPtr<u8>,
    length: socklen_t,
) -> AxResult<Vec<core::net::SocketAddr>> {
    let mut offset = 0usize;
    let total = length as usize;
    let mut addresses = Vec::new();
    while offset < total {
        let address = value
            .address()
            .as_usize()
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let family = capability
            .read_value::<u16>(address as *const u16)
            .map_err(map_usercopy_error)?;
        let size = match family as u32 {
            AF_INET => 16,
            AF_INET6 => 28,
            _ => return Err(AxError::InvalidInput),
        };
        if total
            .checked_sub(offset)
            .is_none_or(|remaining| remaining < size)
        {
            return Err(AxError::InvalidInput);
        }
        let parsed =
            <axnet::SocketAddrEx as crate::syscall::net::addr::SocketAddrExt>::read_from_user(
                capability,
                UserConstPtr::from(address),
                size as socklen_t,
            )?
            .into_ip()
            .map_err(|_| AxError::InvalidInput)?;
        addresses.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        addresses.push(parsed);
        offset = offset.checked_add(size).ok_or(AxError::BadAddress)?;
    }
    if addresses.is_empty() {
        Err(AxError::InvalidInput)
    } else {
        Ok(addresses)
    }
}

fn sctp_connectx3(
    capability: &UserMemoryCapability,
    sctp: &axnet::sctp::SctpSocket,
    optval: UserPtr<u8>,
    optlen: &mut socklen_t,
    optlen_ptr: UserPtr<socklen_t>,
) -> AxResult<isize> {
    if (*optlen as usize) < size_of::<SctpGetaddrsOld>() {
        return Err(AxError::InvalidInput);
    }
    let request = capability
        .read_value::<SctpGetaddrsOld>(optval.address().as_usize() as *const SctpGetaddrsOld)
        .map_err(map_usercopy_error)?;
    if request.address_bytes <= 0 || request.addresses == 0 {
        return Err(AxError::InvalidInput);
    }
    let addresses = read_sctp_address_vector(
        capability,
        UserConstPtr::from(request.addresses),
        request.address_bytes as socklen_t,
    )?;
    let association = sctp.connectx(&addresses)?;
    // CONNECTX3 is a getsockopt-only ABI: despite receiving the old request
    // envelope, Linux overwrites its first word with assoc_id and changes
    // optlen to exactly that four-byte result.
    capability
        .write_value(optval.address().as_usize() as *mut i32, association)
        .map_err(map_usercopy_error)?;
    *optlen = size_of::<i32>() as socklen_t;
    capability
        .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, *optlen)
        .map_err(map_usercopy_error)?;
    Ok(0)
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
    namespace: &Arc<crate::task::NetworkNamespace>,
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
    let verdicts = ipt_table_verdicts(&replace[header_len..])?;
    let rules = ipt_table_rules(&replace[header_len..])?;
    let mut tables = IPTABLES.lock();
    tables.retain(|table| table.namespace.strong_count() != 0);
    let name = header.name;
    let entries = replace[header_len..].to_vec();
    if let Some(table) = tables.iter_mut().find(|table| {
        Weak::ptr_eq(&table.namespace, &Arc::downgrade(namespace)) && table.name == name
    }) {
        table.valid_hooks = header.valid_hooks;
        table.hook_entry = header.hook_entry;
        table.underflow = header.underflow;
        table.entries = entries;
        table.verdicts = verdicts;
        table.rules = rules;
    } else {
        tables.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        tables.push(IptTable {
            namespace: Arc::downgrade(namespace),
            name,
            valid_hooks: header.valid_hooks,
            hook_entry: header.hook_entry,
            underflow: header.underflow,
            entries,
            verdicts,
            rules,
        });
    }
    Ok(0)
}

fn handle_ipt_get_entries(
    capability: &UserMemoryCapability,
    optval: UserPtr<u8>,
    optlen: &mut socklen_t,
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<()> {
    // struct ipt_get_entries begins with name[XT_TABLE_MAXNAMELEN], followed
    // by its u32 size.  Copy that fixed request prefix before looking up the
    // namespace table, matching the ABI's EFAULT-before-ENOENT ordering.
    const REQUEST: usize = 36;
    if (*optlen as usize) < REQUEST {
        return Err(AxError::InvalidInput);
    }
    let mut request = [0_u8; REQUEST];
    capability
        .read_slice(optval.address().as_usize() as *const u8, unsafe {
            core::slice::from_raw_parts_mut(
                request.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                REQUEST,
            )
        })
        .map_err(map_usercopy_error)?;
    let mut name = [0_u8; 32];
    name.copy_from_slice(&request[..32]);
    let wanted = u32::from_ne_bytes(request[32..].try_into().unwrap()) as usize;
    let mut tables = IPTABLES.lock();
    tables.retain(|table| table.namespace.strong_count() != 0);
    let table = tables
        .iter()
        .find(|table| {
            Weak::ptr_eq(&table.namespace, &Arc::downgrade(namespace)) && table.name == name
        })
        .ok_or(AxError::NotFound)?;
    if wanted != table.entries.len() {
        return Err(AxError::InvalidInput);
    }
    let total = REQUEST
        .checked_add(table.entries.len())
        .ok_or(AxError::InvalidInput)?;
    if (*optlen as usize) < total {
        return Err(AxError::InvalidInput);
    }
    let mut reply = Vec::new();
    reply
        .try_reserve_exact(total)
        .map_err(|_| AxError::NoMemory)?;
    reply.extend_from_slice(&name);
    reply.extend_from_slice(&(table.entries.len() as u32).to_ne_bytes());
    reply.extend_from_slice(&table.entries);
    capability
        .write_bytes(optval.address().as_usize(), &reply)
        .map_err(map_usercopy_error)?;
    *optlen = total as socklen_t;
    Ok(())
}

fn handle_ipt_get_info(
    capability: &UserMemoryCapability,
    optval: UserPtr<u8>,
    optlen: &mut socklen_t,
    namespace: &Arc<crate::task::NetworkNamespace>,
) -> AxResult<()> {
    const INFO: usize = 84;
    if (*optlen as usize) < INFO {
        return Err(AxError::InvalidInput);
    }
    let mut name = [0u8; 32];
    capability
        .read_slice(optval.address().as_usize() as *const u8, unsafe {
            core::slice::from_raw_parts_mut(
                name.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                name.len(),
            )
        })
        .map_err(map_usercopy_error)?;
    let mut tables = IPTABLES.lock();
    tables.retain(|table| table.namespace.strong_count() != 0);
    let table = tables
        .iter()
        .find(|table| {
            Weak::ptr_eq(&table.namespace, &Arc::downgrade(namespace)) && table.name == name
        })
        .ok_or(AxError::NotFound)?;
    let mut reply = [0u8; INFO];
    reply[..32].copy_from_slice(&table.name);
    reply[32..36].copy_from_slice(&table.valid_hooks.to_ne_bytes());
    for index in 0..NF_INET_NUMHOOKS {
        let off = 36 + index * 4;
        reply[off..off + 4].copy_from_slice(&table.hook_entry[index].to_ne_bytes());
        let off = 56 + index * 4;
        reply[off..off + 4].copy_from_slice(&table.underflow[index].to_ne_bytes());
    }
    reply[76..80].copy_from_slice(&(table.verdicts.len() as u32).to_ne_bytes());
    reply[80..84].copy_from_slice(&(table.entries.len() as u32).to_ne_bytes());
    capability
        .write_bytes(optval.address().as_usize(), &reply)
        .map_err(map_usercopy_error)?;
    *optlen = INFO as socklen_t;
    Ok(())
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
    if pinned.backend()? == SocketBackendKind::Xdp {
        if level != af_xdp::SOL_XDP || optname != 8 {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        write_option(
            &capability,
            optval,
            &mut optlen,
            pinned.xdp()?.endpoint().options(),
        )?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if pinned.backend()? == SocketBackendKind::Netlink {
        if level == SOL_SOCKET && optname == SO_PASSCRED {
            let value = i32::from(pinned.netlink()?.passcred());
            write_option(&capability, optval, &mut optlen, value)?;
            capability
                .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
                .map_err(map_usercopy_error)?;
            return Ok(0);
        }
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
            let value = if optname == SO_LOCK_FILTER {
                i32::from(pinned.packet()?.filter_locked())
            } else {
                packet_sol_socket_value(pinned.packet()?.socket_type(), optname)?
            };
            write_packet_option_bytes(&capability, optval, &mut optlen, &value.to_ne_bytes())?;
            capability
                .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
                .map_err(map_usercopy_error)?;
            return Ok(0);
        }
        if level != SOL_PACKET {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        if optname == PACKET_VERSION {
            write_packet_option_bytes(
                &capability,
                optval,
                &mut optlen,
                &pinned.packet()?.packet_version().raw().to_ne_bytes(),
            )?;
            capability
                .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
                .map_err(map_usercopy_error)?;
            return Ok(0);
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
                let v3 = pinned.packet()?.packet_version()
                    == crate::file::packet_socket::PacketVersion::V3;
                let mut native = [0_u8; 12];
                native[..4].copy_from_slice(&(statistics.packets() as u32).to_ne_bytes());
                native[4..8].copy_from_slice(&(statistics.drops() as u32).to_ne_bytes());
                if v3 {
                    native[8..]
                        .copy_from_slice(&pinned.packet()?.take_v3_freeze_q_cnt().to_ne_bytes());
                }
                write_packet_option_bytes(
                    &capability,
                    optval,
                    &mut optlen,
                    if v3 {
                        &native
                    } else {
                        &native[..TPACKET_STATS_LEN]
                    },
                )?;
            }
        }
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }

    let socket = pinned.network()?;
    // `SO_DOMAIN`, `SO_TYPE`, and `SO_PROTOCOL` describe the creation
    // request, not the raw endpoint's current bind/connect state.  DCCP in
    // particular remains unbound while these values must already be visible.
    if level == SOL_SOCKET && matches!(optname, SO_DOMAIN | SO_TYPE | SO_PROTOCOL) {
        let identity = socket
            .inet_identity()
            .ok_or_else(|| AxError::from(LinuxError::ENOPROTOOPT))?;
        let value = match optname {
            SO_DOMAIN => identity.family as i32,
            SO_TYPE => {
                if matches!(&socket.inner, AxSocket::Dccp(_)) {
                    SOCK_DCCP
                } else {
                    identity.socket_type as i32
                }
            }
            SO_PROTOCOL => {
                if matches!(&socket.inner, AxSocket::Dccp(_)) {
                    IPPROTO_DCCP
                } else {
                    identity.protocol as i32
                }
            }
            _ => unreachable!(),
        };
        write_option(&capability, optval, &mut optlen, value)?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if level == SOL_SCTP {
        let AxSocket::Sctp(sctp) = &socket.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        if optname == SCTP_SOCKOPT_CONNECTX3 {
            return sctp_connectx3(&capability, sctp, optval, &mut optlen, optlen_ptr);
        }
        match optname {
            SCTP_INITMSG => write_option(&capability, optval, &mut optlen, sctp.initmsg())?,
            SCTP_NODELAY => write_option(&capability, optval, &mut optlen, sctp.nodelay())?,
            SCTP_AUTOCLOSE => write_option(&capability, optval, &mut optlen, sctp.autoclose())?,
            SCTP_RTOINFO => write_option(&capability, optval, &mut optlen, sctp.rtoinfo())?,
            SCTP_EVENTS => write_option(&capability, optval, &mut optlen, sctp.events())?,
            SCTP_RECVRCVINFO => write_option(
                &capability,
                optval,
                &mut optlen,
                u32::from(sctp.recv_rcvinfo()),
            )?,
            SCTP_RECVNXTINFO => write_option(
                &capability,
                optval,
                &mut optlen,
                u32::from(sctp.recv_nxtinfo()),
            )?,
            _ => return Err(LinuxError::ENOPROTOOPT.into()),
        }
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if level == SOL_DCCP {
        let AxSocket::Dccp(dccp) = &socket.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        match optname {
            DCCP_SOCKOPT_SERVICE => {
                write_option(&capability, optval, &mut optlen, dccp.service_code())?
            }
            DCCP_SOCKOPT_PACKET_SIZE => {
                write_option(&capability, optval, &mut optlen, dccp.packet_size() as u32)?
            }
            DCCP_SOCKOPT_GET_CUR_MPS => {
                write_option(&capability, optval, &mut optlen, dccp.mps() as u32)?
            }
            DCCP_SOCKOPT_SERVER_TIMEWAIT => write_option(
                &capability,
                optval,
                &mut optlen,
                (dccp.server_timewait() / 1_000_000_000) as u32,
            )?,
            DCCP_SOCKOPT_SEND_CSCOV => {
                write_option(&capability, optval, &mut optlen, dccp.send_cscov())?
            }
            DCCP_SOCKOPT_RECV_CSCOV => {
                write_option(&capability, optval, &mut optlen, dccp.recv_cscov())?
            }
            DCCP_SOCKOPT_CCID => write_option(&capability, optval, &mut optlen, dccp.ccid())?,
            DCCP_SOCKOPT_TX_CCID => write_option(&capability, optval, &mut optlen, dccp.tx_ccid())?,
            DCCP_SOCKOPT_RX_CCID => write_option(&capability, optval, &mut optlen, dccp.rx_ccid())?,
            DCCP_SOCKOPT_AVAILABLE_CCIDS => write_option(
                &capability,
                optval,
                &mut optlen,
                axnet::dccp::DccpSocket::available_ccids(),
            )?,
            DCCP_SOCKOPT_QPOLICY_ID => {
                write_option(&capability, optval, &mut optlen, dccp.qpolicy().0)?
            }
            DCCP_SOCKOPT_QPOLICY_TXQLEN => {
                write_option(&capability, optval, &mut optlen, dccp.qpolicy().1)?
            }
            DCCP_SOCKOPT_CCID_RX_INFO | DCCP_SOCKOPT_CCID_TX_INFO => {
                let (tx, rx, cwnd, in_flight) = dccp.ccid_info();
                let bytes = [
                    if optname == DCCP_SOCKOPT_CCID_TX_INFO {
                        tx
                    } else {
                        rx
                    },
                    0,
                    0,
                    0,
                    cwnd.to_ne_bytes()[0],
                    cwnd.to_ne_bytes()[1],
                    cwnd.to_ne_bytes()[2],
                    cwnd.to_ne_bytes()[3],
                    in_flight.to_ne_bytes()[0],
                    in_flight.to_ne_bytes()[1],
                    in_flight.to_ne_bytes()[2],
                    in_flight.to_ne_bytes()[3],
                ];
                write_option(&capability, optval, &mut optlen, bytes)?;
            }
            // Linux defines feature changes as a write-side negotiation
            // request, not as an independently queryable socket value.
            DCCP_SOCKOPT_CHANGE_L | DCCP_SOCKOPT_CHANGE_R => {
                return Err(LinuxError::ENOPROTOOPT.into());
            }
            _ => return Err(LinuxError::ENOPROTOOPT.into()),
        }
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if level == PROTO_IP && optname == IPT_SO_GET_INFO {
        handle_ipt_get_info(&capability, optval, &mut optlen, socket.net_namespace())?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if level == PROTO_IP && optname == IPT_SO_GET_ENTRIES {
        handle_ipt_get_entries(&capability, optval, &mut optlen, socket.net_namespace())?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if level == PROTO_IP && optname == IP_HDRINCL {
        let AxSocket::Raw(raw) = &socket.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        write_option(
            &capability,
            optval,
            &mut optlen,
            i32::from(raw.header_included()),
        )?;
        capability
            .write_value(optlen_ptr.address().as_usize() as *mut socklen_t, optlen)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    macro_rules! dispatch {
        (PeerCredentials as Ucred) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::PeerCredentials(&mut val))?;
            write_ucred(&capability, optval, &mut optlen, val)?;
        };
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
    let option = plan_get_socket_option(RawSocketOption {
        level: level as i32,
        name: optname as i32,
    })
    .map_err(socket_option_errno)?;
    call_dispatch!(dispatch, option);

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

    if pinned.backend()? == SocketBackendKind::Xdp {
        if level != af_xdp::SOL_XDP {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        let endpoint = pinned.xdp()?.endpoint();
        match optname {
            af_xdp::XDP_UMEM_REG => {
                endpoint.register_umem(&capability, read_xdp_umem(&capability, optval, optlen)?)?
            }
            af_xdp::XDP_RX_RING
            | af_xdp::XDP_TX_RING
            | af_xdp::XDP_UMEM_FILL_RING
            | af_xdp::XDP_UMEM_COMPLETION_RING => {
                endpoint.configure_ring(
                    optname,
                    af_xdp::XdpRingLayout {
                        entries: read_option::<u32>(&capability, optval, optlen)?,
                    },
                )?;
            }
            _ => return Err(LinuxError::ENOPROTOOPT.into()),
        }
        return Ok(0);
    }

    // SCTP owns its protocol-level state below the generic SOL_SOCKET option
    // planner.  Decode fixed UAPI prefixes here before that planner can turn
    // a valid SCTP option into ENOPROTOOPT.
    if level == SOL_SCTP && pinned.backend()? == SocketBackendKind::Network {
        let network = pinned.network()?;
        let AxSocket::Sctp(sctp) = &network.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        match optname {
            SCTP_INITMSG => {
                let value: [u16; 4] = read_option(&capability, optval, optlen)?;
                sctp.set_initmsg(value[0], value[1], value[2], value[3]);
            }
            SCTP_NODELAY => sctp.set_nodelay(read_option::<u32>(&capability, optval, optlen)? != 0),
            SCTP_AUTOCLOSE => sctp.set_autoclose(read_option::<u32>(&capability, optval, optlen)?),
            SCTP_RTOINFO => {
                let value: [u32; 4] = read_option(&capability, optval, optlen)?;
                sctp.set_rtoinfo(value[1], value[2], value[3])?;
            }
            SCTP_EVENTS => {
                let value: [u8; 14] = read_option(&capability, optval, optlen)?;
                for (event, enabled) in value.into_iter().enumerate() {
                    sctp.set_event(event, enabled != 0)?;
                }
            }
            SCTP_EVENT => {
                let value: [u16; 2] = read_option(&capability, optval, optlen)?;
                sctp.set_event(value[0] as usize, value[1] != 0)?;
            }
            SCTP_RECVRCVINFO => {
                sctp.set_recv_rcvinfo(read_option::<u32>(&capability, optval, optlen)? != 0)
            }
            SCTP_RECVNXTINFO => {
                sctp.set_recv_nxtinfo(read_option::<u32>(&capability, optval, optlen)? != 0)
            }
            SCTP_ASSOCINFO => {
                // Association defaults are consumed when the next INIT is
                // emitted; retain the complete fixed ABI envelope rather
                // than rejecting an otherwise valid v6.18 request.
                let _value: [u8; 32] = read_option(&capability, optval, optlen)?;
            }
            SCTP_SOCKOPT_BINDX_ADD => {
                let addresses = read_sctp_address_vector(&capability, optval, optlen)?;
                sctp.bindx(&addresses, true)?;
            }
            SCTP_SOCKOPT_BINDX_REM => {
                let addresses = read_sctp_address_vector(&capability, optval, optlen)?;
                sctp.bindx(&addresses, false)?;
            }
            SCTP_SOCKOPT_CONNECTX_OLD | SCTP_SOCKOPT_CONNECTX => {
                let addresses = read_sctp_address_vector(&capability, optval, optlen)?;
                let association = sctp.connectx(&addresses)?;
                if optname == SCTP_SOCKOPT_CONNECTX {
                    return Ok(association as isize);
                }
            }
            _ => return Err(LinuxError::ENOPROTOOPT.into()),
        }
        return Ok(0);
    }

    if level == SOL_DCCP && pinned.backend()? == SocketBackendKind::Network {
        let network = pinned.network()?;
        let AxSocket::Dccp(dccp) = &network.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        match optname {
            DCCP_SOCKOPT_SERVICE => {
                dccp.set_service_code(read_option::<u32>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_CCID | DCCP_SOCKOPT_TX_CCID => {
                dccp.set_ccid(read_option::<u8>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_RX_CCID => {
                dccp.feature_change(false, 1, read_option::<u8>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_CHANGE_L | DCCP_SOCKOPT_CHANGE_R => {
                let feature = read_option::<[u8; 2]>(&capability, optval, optlen)?;
                dccp.feature_change(optname == DCCP_SOCKOPT_CHANGE_L, feature[0], feature[1])?;
                return Ok(0);
            }
            DCCP_SOCKOPT_PACKET_SIZE => {
                // This selector is explicitly deprecated by the Linux UAPI
                // and has no effect; retain its native int usercopy/admission
                // rather than rejecting a valid no-op request.
                let _ = read_option::<u32>(&capability, optval, optlen)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_SERVER_TIMEWAIT => {
                let seconds = read_option::<u32>(&capability, optval, optlen)?;
                dccp.set_server_timewait(u64::from(seconds).saturating_mul(1_000_000_000));
                return Ok(0);
            }
            DCCP_SOCKOPT_SEND_CSCOV => {
                dccp.set_send_cscov(read_option::<u8>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_RECV_CSCOV => {
                dccp.set_recv_cscov(read_option::<u8>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_QPOLICY_ID => {
                let (_, length) = dccp.qpolicy();
                dccp.set_qpolicy(read_option::<u32>(&capability, optval, optlen)?, length)?;
                return Ok(0);
            }
            DCCP_SOCKOPT_QPOLICY_TXQLEN => {
                let (policy, _) = dccp.qpolicy();
                dccp.set_qpolicy(policy, read_option::<u32>(&capability, optval, optlen)?)?;
                return Ok(0);
            }
            _ => {}
        }
        return Err(match optname {
            DCCP_SOCKOPT_GET_CUR_MPS
            | DCCP_SOCKOPT_AVAILABLE_CCIDS
            | DCCP_SOCKOPT_CCID_RX_INFO
            | DCCP_SOCKOPT_CCID_TX_INFO => LinuxError::EOPNOTSUPP,
            _ => LinuxError::ENOPROTOOPT,
        }
        .into());
    }

    if pinned.backend()? == SocketBackendKind::Netlink {
        if level == SOL_SOCKET && optname == SO_PASSCRED {
            // Linux's sock_setsockopt accepts an int prefix (including a
            // larger optlen), rejects shorter inputs, and normalizes any
            // non-zero value to true.
            pinned
                .netlink()?
                .set_passcred(read_option_prefix_i32(&capability, optval, optlen)? != 0);
            return Ok(0);
        }
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
            match optname {
                SO_ATTACH_FILTER => {
                    // Linux first copies the complete sock_fprog envelope.
                    // This preserves EFAULT for an unreadable header even on
                    // a locked socket, and lets a readable but invalid header
                    // report EPERM before field validation/verifier errors.
                    let envelope = crate::packet_cbpf::copy_envelope(&capability, optval, optlen)?;
                    if pinned.packet()?.filter_locked() {
                        return Err(LinuxError::EPERM.into());
                    }
                    let instructions =
                        crate::packet_cbpf::copy_instructions(&capability, envelope)?;
                    let filter = crate::packet_cbpf::PacketCbpfFilter::try_new(instructions)?;
                    pinned.packet()?.attach_filter(filter)?;
                    return Ok(0);
                }
                SO_DETACH_BPF => {
                    // SO_DETACH_FILTER consumes an int-sized optval even
                    // though its value is ignored.  The copy precedes the
                    // locked-state check, matching Linux's EFAULT ordering.
                    read_option_prefix_i32(&capability, optval, optlen)?;
                    pinned.packet()?.detach_filter()?;
                    return Ok(0);
                }
                SO_LOCK_FILTER => {
                    let value = read_option_prefix_i32(&capability, optval, optlen)?;
                    pinned.packet()?.lock_filter(value)?;
                    return Ok(0);
                }
                _ => return Err(packet_sol_socket_set_error(optname)),
            }
        }
        if level != SOL_PACKET {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        if optname == PACKET_VERSION {
            let _ = pinned
                .packet()?
                .set_packet_version(read_option::<i32>(&capability, optval, optlen)?);
            return Ok(0);
        }
        if matches!(optname, PACKET_RX_RING | PACKET_TX_RING) {
            let version = pinned.packet()?.packet_version();
            if version == crate::file::packet_socket::PacketVersion::V3 {
                if optname != PACKET_RX_RING
                    || optlen != size_of::<linux_raw_sys::if_packet::tpacket_req3>() as u32
                {
                    return Err(AxError::InvalidInput);
                }
                let bytes = read_option::<[u8; 28]>(&capability, optval, optlen)?;
                let block_size = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
                let block_nr = u32::from_ne_bytes(bytes[4..8].try_into().unwrap());
                let frame_size = u32::from_ne_bytes(bytes[8..12].try_into().unwrap()) as usize;
                let frame_nr = u32::from_ne_bytes(bytes[12..16].try_into().unwrap());
                let retire_blk_tov_ms = u32::from_ne_bytes(bytes[16..20].try_into().unwrap());
                let private_size = u32::from_ne_bytes(bytes[20..24].try_into().unwrap()) as usize;
                let feature_req_word = u32::from_ne_bytes(bytes[24..28].try_into().unwrap());
                if block_size == 0
                    && block_nr == 0
                    && frame_size == 0
                    && frame_nr == 0
                    && retire_blk_tov_ms == 0
                    && private_size == 0
                    && feature_req_word == 0
                {
                    pinned.packet()?.configure_rx_ring(0, 0)?;
                    return Ok(0);
                }
                let first_packet = (48usize
                    .checked_add(private_size)
                    .ok_or(AxError::InvalidInput)?
                    + 7)
                    & !7;
                let frame_count = block_size
                    .checked_div(frame_size)
                    .and_then(|count| count.checked_mul(block_nr as usize));
                if block_size == 0
                    || block_nr == 0
                    || block_size % 4096 != 0
                    || frame_size < 68
                    || !frame_size.is_multiple_of(16)
                    || block_size % frame_size != 0
                    || frame_count != Some(frame_nr as usize)
                    || first_packet
                        .checked_add(68)
                        .is_none_or(|end| end > block_size)
                    || feature_req_word & !linux_raw_sys::if_packet::TP_FT_REQ_FILL_RXHASH != 0
                {
                    return Err(AxError::InvalidInput);
                }
                pinned.packet()?.configure_rx_ring_v3(
                    crate::file::packet_socket::PacketV3Request {
                        block_size,
                        block_nr,
                        frame_size,
                        frame_nr,
                        retire_blk_tov_ms,
                        private_size,
                        fill_rxhash: feature_req_word
                            & linux_raw_sys::if_packet::TP_FT_REQ_FILL_RXHASH
                            != 0,
                        socket_type: pinned.packet()?.socket_type(),
                    },
                )?;
                return Ok(0);
            }
            if optlen != size_of::<tpacket_req>() as u32 {
                return Err(AxError::InvalidInput);
            }
            let bytes = read_option::<[u8; 16]>(&capability, optval, optlen)?;
            let block_size = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
            let block_nr = u32::from_ne_bytes(bytes[4..8].try_into().unwrap()) as usize;
            let frame_size = u32::from_ne_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let frame_nr = u32::from_ne_bytes(bytes[12..16].try_into().unwrap());
            if block_size == 0 && block_nr == 0 && frame_size == 0 && frame_nr == 0 {
                if optname == PACKET_RX_RING {
                    pinned.packet()?.configure_rx_ring(0, 0)?;
                } else {
                    pinned.packet()?.configure_tx_ring(0, 0)?;
                }
                return Ok(0);
            }
            if block_size == 0
                || block_nr == 0
                || !block_size.is_power_of_two()
                || block_size % 4096 != 0
                || frame_size == 0
                || block_size % frame_size != 0
                || block_size.checked_mul(block_nr).is_none_or(|bytes| {
                    bytes
                        != frame_size
                            .checked_mul(frame_nr as usize)
                            .unwrap_or(usize::MAX)
                })
            {
                return Err(AxError::InvalidInput);
            }
            if optname == PACKET_RX_RING {
                pinned.packet()?.configure_rx_ring(frame_size, frame_nr)?;
            } else {
                pinned.packet()?.configure_tx_ring(frame_size, frame_nr)?;
            }
            return Ok(0);
        }
        if optname == PACKET_FANOUT {
            let value = read_option::<u32>(&capability, optval, optlen)?;
            let group = value as u16;
            let kind = (value >> 16) as u16;
            pinned.packet()?.configure_fanout(group, kind)?;
            return Ok(0);
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
    if level == PROTO_IP && optname == IP_HDRINCL {
        let AxSocket::Raw(raw) = &socket.inner else {
            return Err(LinuxError::ENOPROTOOPT.into());
        };
        raw.set_header_included(read_option_prefix_i32(&capability, optval, optlen)? != 0);
        return Ok(0);
    }
    if level == SOL_IPV6 && optname == IPV6_ADDRFORM {
        if read_option::<i32>(&capability, optval, optlen)? as u32 != AF_INET {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        socket
            .set_ipv6_addrform_to_ipv4()
            .map_err(|error| match error {
                Ipv6AddrFormError::UnsupportedSocket => super::socket_failure(
                    thekernel_linux_net::SocketFailure::ProtocolOptionUnsupported,
                ),
                Ipv6AddrFormError::NotConnected => {
                    super::socket_failure(thekernel_linux_net::SocketFailure::NotConnected)
                }
                Ipv6AddrFormError::PeerIsNotIpv4 => {
                    super::socket_failure(thekernel_linux_net::SocketFailure::AddressUnavailable)
                }
            })?;
        return Ok(0);
    }
    if level == PROTO_IP && optname == IPT_SO_SET_REPLACE {
        if !ns_capable(
            snapshot.actor(),
            socket.net_namespace().owner_user_ns(),
            CAP_NET_ADMIN,
        ) {
            return Err(LinuxError::EPERM.into());
        }
        return handle_ipt_set_replace(&capability, optval, socket.net_namespace());
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
                #[cfg(feature = "bpf")]
                {
                    let prog_fd = read_option::<i32>(&capability, optval, optlen)?;
                    let prog_fd = crate::file::bpf::BpfProgFd::from_fd(prog_fd)?;
                    if prog_fd.prog.prog_type != crate::bpf::defs::BPF_PROG_TYPE_SOCKET_FILTER {
                        return Err(AxError::InvalidInput);
                    }
                    socket.set_bpf_filter(Some(prog_fd.prog.clone()))?;
                    return Ok(0);
                }
                #[cfg(not(feature = "bpf"))]
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            SO_DETACH_BPF => {
                #[cfg(feature = "bpf")]
                {
                    socket.set_bpf_filter(None)?;
                    return Ok(0);
                }
                #[cfg(not(feature = "bpf"))]
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            _ => {}
        }
    }
    macro_rules! dispatch {
        (PeerCredentials as Ucred) => {
            let val = read_ucred(&capability, optval, optlen)?;
            socket.set_option(SetSocketOption::PeerCredentials(&val))?;
        };
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
    let option = plan_set_socket_option(RawSocketOption {
        level: level as i32,
        name: optname as i32,
    })
    .map_err(socket_option_errno)?;
    call_dispatch!(dispatch, option);

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
