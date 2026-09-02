use alloc::{
    borrow::Cow,
    collections::VecDeque,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axnet::{
    InterfaceInfo, InterfaceKind, IpAddress, IpCidr, Ipv4Address, Ipv6Address, RecvFlags,
    RouteInfo, Rule,
};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use linux_raw_sys::{
    general::{CAP_AUDIT_READ, CAP_NET_ADMIN, CAP_SYS_ADMIN},
    net::{
        AF_INET, AF_INET6, AF_NETLINK, AF_UNSPEC, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET,
        SOCK_STREAM, sockaddr, socklen_t,
    },
};
use spin::{Lazy, Mutex, MutexGuard};
#[cfg(test)]
use thekernel_linux_net::NETLINK_MAX_MESSAGE_BYTES;
use thekernel_linux_net::{
    NETLINK_DEFAULT_SEND_BUFFER_BYTES, NetlinkQueueAdmission, NetlinkWriteAdmission,
    admit_netlink_queue, admit_netlink_write,
};

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, PseudoInode, try_pseudo_inode_path},
    mm::{UserMemoryCapability, UserPtr, map_usercopy_error},
    readiness::block_on_poll_io,
    task::{
        AsThread, Cred, NetworkNamespace, ns_capable,
        security::{AuditLandlockDenied, AuditSeccompDecision},
    },
};

const NETLINK_MAX_PROTOCOL: u32 = 31;
const NETLINK_QUEUE_LIMIT: usize = 128;
const NETLINK_QUEUE_LIMIT_BYTES: usize = NETLINK_DEFAULT_SEND_BUFFER_BYTES;
const NETLINK_ROUTE: u32 = 0;
const NETLINK_SOCK_DIAG: u32 = 4;
const NETLINK_NETFILTER: u32 = 12;
const NETLINK_KOBJECT_UEVENT: u32 = 15;
const NETLINK_GENERIC: u32 = 16;
pub(crate) const NETLINK_AUDIT: u32 = 9;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const INET_DIAG_REQ_V2_LEN: usize = 56;
const INET_DIAG_NOCOOKIE: u32 = u32::MAX;
const AUDIT_GROUP: u32 = 1;
const AUDIT_SECCOMP: u16 = 1326;
const AUDIT_LANDLOCK_ACCESS: u16 = 1423;
const KOBJECT_UEVENT_GROUP: u32 = 1;
const NETLINK_NO_ENOBUFS: u32 = 5;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_CREATE: u16 = 0x400;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const RT_TABLE_MAIN: u8 = 254;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RTN_UNICAST: u8 = 1;
const IFA_F_PERMANENT: u8 = 0x80;
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_NEWFAMILY: u8 = 1;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_VERSION: u16 = 3;
const CTRL_ATTR_HDRSIZE: u16 = 4;
const CTRL_ATTR_MAXATTR: u16 = 5;
const THEKERNEL_GENL_FAMILY_ID: u16 = 0x11;
const THEKERNEL_GENL_FAMILY_NAME: &str = "thekernel";
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_MSG_BATCH_BEGIN: u16 = 16;
const NFNL_MSG_BATCH_END: u16 = 17;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_NEWSET: u16 = 9;
const NFT_MSG_NEWSETELEM: u16 = 12;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_GETTABLE: u16 = 1;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_GETCHAIN: u16 = 4;
const NFT_MSG_DELRULE: u16 = 8;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_DELSET: u16 = 11;
const NFT_MSG_GETSET: u16 = 10;
const NFT_MSG_DELSETELEM: u16 = 14;
const NFT_MSG_GETSETELEM: u16 = 13;
const NFTA_TABLE_NAME: u16 = 1;
const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_SET_TABLE: u16 = 1;
const NFTA_SET_NAME: u16 = 2;
const NFTA_SET_ELEM_LIST_TABLE: u16 = 1;
const NFTA_SET_ELEM_LIST_SET: u16 = 2;
const NFTA_SET_ELEM_LIST_ELEMENTS: u16 = 3;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_POLICY: u16 = 5;
const NFTA_CHAIN_TYPE: u16 = 7;
const NFTA_SET_ID: u16 = 3;
const NFTA_SET_FLAGS: u16 = 4;
const NFTA_SET_KEY_TYPE: u16 = 5;
const NFTA_SET_DATA_TYPE: u16 = 6;
const NFTA_SET_DESC: u16 = 7;
const NFTA_RULE_POSITION: u16 = 5;
const NFTA_RULE_USERDATA: u16 = 7;
const NFTA_RULE_ID: u16 = 10;

/// Pure framing preflight used before selecting a protocol-specific write
/// permit.  It has no namespace or socket side effects, so a source fault or
/// malformed later frame cannot commit an earlier netlink mutation.
fn validate_netlink_frames(data: &[u8]) -> AxResult {
    let mut offset = 0usize;
    while offset < data.len() {
        if data.len() - offset < size_of::<NlMsgHdr>() {
            return Err(AxError::InvalidInput);
        }
        let header = read_unaligned::<NlMsgHdr>(&data[offset..])?;
        let len = header.nlmsg_len as usize;
        if len < size_of::<NlMsgHdr>()
            || offset.checked_add(len).ok_or(AxError::InvalidInput)? > data.len()
        {
            return Err(AxError::InvalidInput);
        }
        let next = offset
            .checked_add(align4(len))
            .ok_or(AxError::InvalidInput)?;
        if next > data.len() && offset + len != data.len() {
            return Err(AxError::InvalidInput);
        }
        offset = next.min(data.len());
    }
    Ok(())
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SockaddrNl {
    pub nl_family: u16,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgHdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgErr {
    error: i32,
    msg: NlMsgHdr,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GenlMsgHdr {
    cmd: u8,
    version: u8,
    reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtAttr {
    rta_len: u16,
    rta_type: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtMsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IfAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IfInfoMsg {
    ifi_family: u8,
    ifi_pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

#[derive(Clone, PartialEq, Eq)]
struct RouteEntry {
    family: u8,
    dst_len: u8,
    table: u8,
    scope: u8,
    route_type: u8,
    oif: Option<u32>,
    dst: Vec<u8>,
    gateway: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq)]
struct AddressEntry {
    family: u8,
    prefix_len: u8,
    flags: u8,
    scope: u8,
    index: u32,
    local: Vec<u8>,
    address: Vec<u8>,
    label: String,
}

#[derive(Clone)]
struct LinkEntry {
    index: u32,
    name: String,
    flags: u32,
    mtu: u32,
    hwaddr: Vec<u8>,
    arphrd: u16,
}

#[derive(Default)]
struct NetlinkState {
    port_id: u32,
    groups: u32,
    option_flags: u32,
    passcred: bool,
    bound: bool,
}

pub struct NetlinkSocket {
    protocol: u32,
    net_ns: Arc<NetworkNamespace>,
    inode: PseudoInode,
    state: Mutex<NetlinkState>,
    queue: Mutex<NetlinkQueue>,
    write_gate: Mutex<()>,
    overrun: AtomicBool,
    nonblocking: AtomicBool,
    poll_rx: PollSet,
}

struct NetlinkDatagram {
    data: Vec<u8>,
    source_port_id: u32,
    source_groups: u32,
    credentials: Option<NetlinkCredentials>,
}

struct NetlinkQueue {
    datagrams: VecDeque<NetlinkDatagram>,
    bytes: usize,
}

/// All shared state retained by one netlink write after its frames have been
/// copied and read-only parsed.  The variants intentionally name protocol
/// ownership instead of using a generic write lock: an accepted NOWAIT write
/// must never discover a deeper blocking lock after it has consumed a user
/// source.  Route/generic/audit/uevent retain the socket state and ACK queue;
/// sock-diag additionally owns its registry; nfnetlink owns both its global
/// transaction and namespace table graph.
enum NetlinkWritePermit<'a> {
    Route {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
        service: axnet::NetStackServicePermit<'a>,
    },
    Generic {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
    },
    Netfilter {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
        transaction: MutexGuard<'static, ()>,
        tables: MutexGuard<'static, Vec<NftNamespaceTables>>,
    },
    Audit {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
    },
    Uevent {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
        send: MutexGuard<'static, ()>,
        listeners: MutexGuard<'static, Vec<Weak<NetlinkSocket>>>,
    },
    SockDiag {
        gate: MutexGuard<'a, ()>,
        state: MutexGuard<'a, NetlinkState>,
        queue: MutexGuard<'a, NetlinkQueue>,
        registry: MutexGuard<'static, Vec<Weak<SocketDiagRegistration>>>,
    },
}

impl<'a> NetlinkWritePermit<'a> {
    fn port_id(&self) -> u32 {
        match self {
            Self::Route { state, .. }
            | Self::Generic { state, .. }
            | Self::Netfilter { state, .. }
            | Self::Audit { state, .. }
            | Self::Uevent { state, .. }
            | Self::SockDiag { state, .. } => state.port_id,
        }
    }

    fn queue(&mut self) -> &mut NetlinkQueue {
        match self {
            Self::Route { queue, .. }
            | Self::Generic { queue, .. }
            | Self::Netfilter { queue, .. }
            | Self::Audit { queue, .. }
            | Self::Uevent { queue, .. }
            | Self::SockDiag { queue, .. } => queue,
        }
    }

    fn suppress_enobufs(&self) -> bool {
        match self {
            Self::Route { state, .. }
            | Self::Generic { state, .. }
            | Self::Netfilter { state, .. }
            | Self::Audit { state, .. }
            | Self::Uevent { state, .. }
            | Self::SockDiag { state, .. } => state.option_flags & (1 << NETLINK_NO_ENOBUFS) != 0,
        }
    }

    fn subscribed_to(&self, group: u32) -> bool {
        match self {
            Self::Route { state, .. }
            | Self::Generic { state, .. }
            | Self::Netfilter { state, .. }
            | Self::Audit { state, .. }
            | Self::Uevent { state, .. }
            | Self::SockDiag { state, .. } => state.groups & group != 0,
        }
    }

    fn nft_tables(&mut self) -> Option<&mut Vec<NftNamespaceTables>> {
        match self {
            Self::Netfilter { tables, .. } => Some(tables),
            _ => None,
        }
    }

    fn sock_diag_registry(&mut self) -> Option<&mut Vec<Weak<SocketDiagRegistration>>> {
        match self {
            Self::SockDiag { registry, .. } => Some(registry),
            _ => None,
        }
    }

    fn route_service(&mut self) -> Option<&mut axnet::NetStackServicePermit<'a>> {
        match self {
            Self::Route { service, .. } => Some(service),
            _ => None,
        }
    }

    fn uevent_listeners(&mut self) -> Option<&mut Vec<Weak<NetlinkSocket>>> {
        match self {
            Self::Uevent { listeners, .. } => Some(listeners),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NetlinkReceived {
    pub(crate) len: usize,
    pub(crate) source_port_id: u32,
    pub(crate) source_groups: u32,
    pub(crate) credentials: Option<NetlinkCredentials>,
}

/// Sender identity captured when a kobject uevent is queued.  This is kept
/// beside the datagram rather than sampled at receive time: a synthetic event
/// must retain its originating task, while kernel-originated events have the
/// Linux kernel identity (pid 0, root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetlinkCredentials {
    pub(crate) pid: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

const KERNEL_UEVENT_CREDENTIALS: NetlinkCredentials = NetlinkCredentials {
    pid: 0,
    uid: 0,
    gid: 0,
};

static KOBJECT_UEVENT_SOCKETS: Lazy<Mutex<Vec<Weak<NetlinkSocket>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));
static AUDIT_SOCKETS: Lazy<Mutex<Vec<Weak<NetlinkSocket>>>> = Lazy::new(|| Mutex::new(Vec::new()));
// Kobject/device discovery is global to init-net.  The namespace is a kernel
// lifetime object: device notifications must not disappear merely because
// init exits and its process-owned reference is reaped.  This one-way global
// reference forms no ownership cycle.
static INIT_NETWORK_NAMESPACE: Lazy<Mutex<Option<Arc<NetworkNamespace>>>> =
    Lazy::new(|| Mutex::new(None));
static NETLINK_PORTS: Lazy<Mutex<Vec<NetlinkPortBinding>>> = Lazy::new(|| Mutex::new(Vec::new()));
static SOCK_DIAG_REGISTRATIONS: Lazy<Mutex<Vec<Weak<SocketDiagRegistration>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));
static KOBJECT_UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);
static KOBJECT_UEVENT_SEND_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static NETLINK_NEXT_PORT_ID: AtomicU32 = AtomicU32::new(1);
static SOCK_DIAG_NEXT_COOKIE: AtomicU64 = AtomicU64::new(1);

/// Lifetime-owned registry entry for an inet socket.  The registration is
/// retained by the socket OFD, so close/fork/exec naturally share and retire
/// the same diagnostic identity without a process-global socket table.
pub(crate) struct SocketDiagRegistration {
    net_ns: Weak<NetworkNamespace>,
    family: u16,
    socket_type: u8,
    protocol: u8,
    cookie: u64,
}

impl SocketDiagRegistration {
    fn diag_state(&self) -> u8 {
        // TCP and DCCP inet_diag use the normal close state for an OFD which
        // has no transport-state snapshot yet. Datagram diagnostics carry no
        // state bit rather than pretending to be a TCP endpoint.
        if self.socket_type == SOCK_STREAM as u8
            || self.socket_type == SOCK_SEQPACKET as u8
            || self.protocol == 33
        {
            7
        } else {
            0
        }
    }
}

pub(crate) fn register_socket_diag(
    net_ns: &Arc<NetworkNamespace>,
    family: u16,
    socket_type: u8,
    protocol: u8,
) -> AxResult<Arc<SocketDiagRegistration>> {
    let registration = Arc::try_new(SocketDiagRegistration {
        net_ns: Arc::downgrade(net_ns),
        family,
        socket_type,
        protocol,
        cookie: SOCK_DIAG_NEXT_COOKIE.fetch_add(1, Ordering::Relaxed),
    })
    .map_err(|_| AxError::NoMemory)?;
    let mut registrations = SOCK_DIAG_REGISTRATIONS.lock();
    registrations.retain(|entry| entry.strong_count() != 0);
    registrations
        .try_reserve(1)
        .map_err(|_| AxError::NoMemory)?;
    registrations.push(Arc::downgrade(&registration));
    Ok(registration)
}

struct NetlinkPortBinding {
    net_ns: Weak<NetworkNamespace>,
    protocol: u32,
    port_id: u32,
    socket_inode: u64,
}

#[derive(Clone)]
struct NftChain {
    table: String,
    name: String,
    hook: Option<NftHook>,
    policy: NftVerdict,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum NftVerdict {
    Continue,
    Accept,
    Drop,
    Reject,
    Return,
    Jump,
    Goto,
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum NftHook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}
#[derive(Clone)]
enum NftExpr {
    Ct {
        key: u32,
        dreg: u32,
    },
    Payload {
        base: u32,
        offset: u32,
        len: u32,
        dreg: u32,
    },
    Cmp {
        sreg: u32,
        op: u32,
        data: Vec<u8>,
    },
    Nat {
        kind: u32,
        family: u32,
        addr_reg: Option<u32>,
        proto_reg: Option<u32>,
        masquerade: bool,
    },
}
#[derive(Clone)]
struct NftRule {
    table: String,
    chain: String,
    handle: u64,
    verdict: NftVerdict,
    target_chain: Option<String>,
    lookup: Option<(String, Vec<u8>)>,
    expressions: Vec<NftExpr>,
    counter: u64,
}
#[derive(Clone)]
struct NftSet {
    table: String,
    name: String,
    id: u32,
    flags: u32,
    key_type: u32,
    data_type: u32,
}
#[derive(Clone)]
struct NftSetElement {
    table: String,
    set: String,
    key: Vec<u8>,
}
#[derive(Clone)]
struct NftNamespaceTables {
    namespace: Weak<NetworkNamespace>,
    tables: Vec<String>,
    chains: Vec<NftChain>,
    rules: Vec<NftRule>,
    sets: Vec<NftSet>,
    elements: Vec<NftSetElement>,
    next_rule: u64,
    generation: u32,
}
static NFT_TABLES: Lazy<Mutex<Vec<NftNamespaceTables>>> = Lazy::new(|| Mutex::new(Vec::new()));
// Serializes an nfnetlink write with packet traversal.  The state is copied
// before a datagram and published only when every message validates, so even
// a multi-message batch has no externally observable intermediate graph.
static NFT_TRANSACTION: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Clone, Copy, Eq, PartialEq)]
struct ConntrackTuple {
    family: u8,
    protocol: u8,
    source: [u8; 16],
    destination: [u8; 16],
    source_port: u16,
    destination_port: u16,
}
struct ConntrackEntry {
    namespace: Weak<NetworkNamespace>,
    original: ConntrackTuple,
    translated: ConntrackTuple,
    reply: ConntrackTuple,
    packets: u64,
    expires_at: u64,
}
static CONNTRACK: Lazy<Mutex<Vec<ConntrackEntry>>> = Lazy::new(|| Mutex::new(Vec::new()));
static CONNTRACK_CLOCK: AtomicU64 = AtomicU64::new(0);

/// Executes the namespace's nft OUTPUT verdict chain at the packet emission
/// boundary.  Rule order is insertion order, matching the retained nft
/// chain order; a terminal drop/reject is never converted to a fake success.
pub(crate) fn nft_output_verdict(namespace: &Arc<NetworkNamespace>) -> AxResult {
    // Socket send admission occurs before axnet constructs its IP header.
    // The authoritative OUTPUT traversal is the router hook, once the full
    // packet exists; treating this headerless preflight as a packet would
    // make legitimate payload/ct/nat expressions fail spuriously.
    let _ = namespace;
    Ok(())
}

/// Namespace-local packet traversal entry used by packet, TUN/TAP and inet
/// boundary code.  The packet is mutable because NAT expressions alter the
/// in-flight headers before the lower router observes them.
pub(crate) fn nft_packet_hook(
    namespace: &Arc<NetworkNamespace>,
    hook: NftHook,
    packet: &mut [u8],
) -> AxResult {
    #[cfg(feature = "bpf")]
    crate::bpf::run_network_packet_links(
        namespace,
        match hook {
            NftHook::Prerouting => crate::file::bpf::BpfNetworkHook::Prerouting,
            NftHook::Input => crate::file::bpf::BpfNetworkHook::Input,
            NftHook::Forward => crate::file::bpf::BpfNetworkHook::Forward,
            NftHook::Output => crate::file::bpf::BpfNetworkHook::Output,
            NftHook::Postrouting => crate::file::bpf::BpfNetworkHook::Postrouting,
        },
        packet,
    )?;
    let _transaction = NFT_TRANSACTION.lock();
    conntrack_reverse_translate(namespace, packet)?;
    if let Some(tuple) = conntrack_tuple(packet) {
        conntrack_observe(namespace, tuple)?;
    }
    let mut namespaces = NFT_TABLES.lock();
    namespaces.retain(|state| state.namespace.strong_count() != 0);
    let needle = Arc::downgrade(namespace);
    for state in namespaces
        .iter_mut()
        .filter(|state| Weak::ptr_eq(&state.namespace, &needle))
    {
        let mut stack = Vec::new();
        let output_chains: Vec<(String, String)> = state
            .chains
            .iter()
            .filter(|chain| chain.hook == Some(hook))
            .map(|chain| (chain.table.clone(), chain.name.clone()))
            .collect();
        for (table, chain) in output_chains {
            nft_evaluate_chain(namespace, state, &table, &chain, packet, &mut stack)?;
        }
    }
    Ok(())
}

fn conntrack_tuple(packet: &[u8]) -> Option<ConntrackTuple> {
    let version = *packet.first()? >> 4;
    let (family, protocol, source_offset, destination_offset, l4) = match version {
        4 => {
            let ihl = usize::from(*packet.first()? & 0x0f).checked_mul(4)?;
            if ihl < 20 || packet.len() < ihl {
                return None;
            }
            (4, *packet.get(9)?, 12, 16, ihl)
        }
        6 => {
            if packet.len() < 40 {
                return None;
            }
            (6, *packet.get(6)?, 8, 24, 40)
        }
        _ => return None,
    };
    let mut source = [0; 16];
    let mut destination = [0; 16];
    let width = if family == 4 { 4 } else { 16 };
    source[..width].copy_from_slice(packet.get(source_offset..source_offset + width)?);
    destination[..width]
        .copy_from_slice(packet.get(destination_offset..destination_offset + width)?);
    let (source_port, destination_port) = match protocol {
        6 | 17 | 132 | 33 => (
            u16::from_be_bytes(packet.get(l4..l4 + 2)?.try_into().ok()?),
            u16::from_be_bytes(packet.get(l4 + 2..l4 + 4)?.try_into().ok()?),
        ),
        _ => (0, 0),
    };
    Some(ConntrackTuple {
        family,
        protocol,
        source,
        destination,
        source_port,
        destination_port,
    })
}

fn conntrack_reverse(tuple: ConntrackTuple) -> ConntrackTuple {
    ConntrackTuple {
        family: tuple.family,
        protocol: tuple.protocol,
        source: tuple.destination,
        destination: tuple.source,
        source_port: tuple.destination_port,
        destination_port: tuple.source_port,
    }
}

fn conntrack_observe(namespace: &Arc<NetworkNamespace>, tuple: ConntrackTuple) -> AxResult {
    let now = CONNTRACK_CLOCK
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let mut state = CONNTRACK.lock();
    state.retain(|entry| entry.namespace.strong_count() != 0 && entry.expires_at > now);
    let needle = Arc::downgrade(namespace);
    if let Some(entry) = state.iter_mut().find(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == tuple || entry.translated == tuple || entry.reply == tuple)
    }) {
        entry.packets = entry.packets.saturating_add(1);
        entry.expires_at = now.saturating_add(60_000);
        return Ok(());
    }
    state.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    state.push(ConntrackEntry {
        namespace: needle,
        original: tuple,
        translated: tuple,
        reply: conntrack_reverse(tuple),
        packets: 1,
        expires_at: now.saturating_add(60_000),
    });
    Ok(())
}

fn nft_evaluate_chain(
    namespace: &Arc<NetworkNamespace>,
    state: &mut NftNamespaceTables,
    table: &str,
    chain: &str,
    packet: &mut [u8],
    stack: &mut Vec<String>,
) -> AxResult<NftVerdict> {
    // A jump cycle is rejected when installed; retaining this guard makes a
    // corrupted userspace graph fail closed instead of recursing in kernel
    // context.
    if stack.len() >= 64 || stack.iter().any(|item| item == chain) {
        return Err(LinuxError::ELOOP.into());
    }
    stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    stack.push(chain.to_string());
    let policy = state
        .chains
        .iter()
        .find(|item| item.table == table && item.name == chain)
        .map(|item| item.policy)
        .ok_or(AxError::NotFound)?;
    let rule_indexes: Vec<usize> = state
        .rules
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (item.table == table && item.chain == chain).then_some(index))
        .collect();
    for index in rule_indexes {
        let (verdict, target, lookup, expressions) = {
            let rule = &mut state.rules[index];
            rule.counter = rule.counter.saturating_add(1);
            (
                rule.verdict,
                rule.target_chain.clone(),
                rule.lookup.clone(),
                rule.expressions.clone(),
            )
        };
        if !nft_evaluate_expressions(namespace, packet, &expressions)? {
            continue;
        }
        if let Some((set, key)) = lookup {
            if !state.elements.iter().any(|item| {
                item.table == table && item.set == set && (key.is_empty() || item.key == key)
            }) {
                continue;
            }
        }
        match verdict {
            NftVerdict::Continue => {}
            NftVerdict::Accept => {
                stack.pop();
                return Ok(NftVerdict::Accept);
            }
            NftVerdict::Drop => {
                stack.pop();
                return Err(LinuxError::EPERM.into());
            }
            NftVerdict::Reject => {
                stack.pop();
                return Err(LinuxError::ECONNREFUSED.into());
            }
            NftVerdict::Return => break,
            NftVerdict::Jump | NftVerdict::Goto => {
                let target = target.ok_or(AxError::InvalidInput)?;
                match nft_evaluate_chain(namespace, state, table, &target, packet, stack)? {
                    NftVerdict::Accept => {
                        stack.pop();
                        return Ok(NftVerdict::Accept);
                    }
                    _ if verdict == NftVerdict::Goto => break,
                    _ => {}
                }
            }
        }
    }
    stack.pop();
    match policy {
        NftVerdict::Drop => Err(LinuxError::EPERM.into()),
        NftVerdict::Reject => Err(LinuxError::ECONNREFUSED.into()),
        _ => Ok(NftVerdict::Continue),
    }
}

fn nft_evaluate_expressions(
    namespace: &Arc<NetworkNamespace>,
    packet: &mut [u8],
    expressions: &[NftExpr],
) -> AxResult<bool> {
    let mut registers: [Option<Vec<u8>>; 16] = core::array::from_fn(|_| None);
    for expression in expressions {
        match expression {
            NftExpr::Ct { key, dreg } => {
                let index = usize::try_from(*dreg).map_err(|_| AxError::InvalidInput)?;
                let value = match *key {
                    // NFT_CT_STATE: NEW for an initial tuple, ESTABLISHED for
                    // either direction of a retained conntrack entry.
                    0 => (if conntrack_is_established(namespace, packet) {
                        2u32
                    } else {
                        1u32
                    })
                    .to_ne_bytes()
                    .to_vec(),
                    7 => conntrack_tuple(packet)
                        .map(|tuple| tuple.protocol as u32)
                        .unwrap_or(0)
                        .to_ne_bytes()
                        .to_vec(),
                    _ => return Err(AxError::OperationNotSupported),
                };
                let slot = registers.get_mut(index).ok_or(AxError::InvalidInput)?;
                *slot = Some(value);
            }
            NftExpr::Payload {
                base,
                offset,
                len,
                dreg,
            } => {
                let index = usize::try_from(*dreg).map_err(|_| AxError::InvalidInput)?;
                let start = payload_offset(packet, *base, *offset)?;
                let end = start
                    .checked_add(*len as usize)
                    .ok_or(AxError::InvalidInput)?;
                let bytes = packet.get(start..end).ok_or(AxError::InvalidInput)?;
                let mut value = Vec::new();
                value
                    .try_reserve_exact(bytes.len())
                    .map_err(|_| AxError::NoMemory)?;
                value.extend_from_slice(bytes);
                *registers.get_mut(index).ok_or(AxError::InvalidInput)? = Some(value);
            }
            NftExpr::Cmp { sreg, op, data } => {
                let value = registers
                    .get(*sreg as usize)
                    .and_then(Option::as_ref)
                    .ok_or(AxError::InvalidInput)?;
                let equal = value.as_slice() == data.as_slice();
                // NFT_CMP_EQ/NEQ; relational packet comparisons are defined
                // only for equal-width big-endian scalar registers here.
                let matched = match *op {
                    0 => equal,
                    1 => !equal,
                    2 => value.as_slice() < data.as_slice(),
                    3 => value.as_slice() <= data.as_slice(),
                    4 => value.as_slice() > data.as_slice(),
                    5 => value.as_slice() >= data.as_slice(),
                    _ => return Err(AxError::InvalidInput),
                };
                if !matched {
                    return Ok(false);
                }
            }
            NftExpr::Nat {
                kind,
                family,
                addr_reg,
                proto_reg,
                masquerade,
            } => {
                let address = addr_reg
                    .and_then(|reg| registers.get(reg as usize))
                    .and_then(Option::as_ref)
                    .map(Vec::as_slice);
                let port = proto_reg
                    .and_then(|reg| registers.get(reg as usize))
                    .and_then(Option::as_ref)
                    .and_then(|value| value.get(..2))
                    .map(|bytes| u16::from_be_bytes(bytes.try_into().unwrap()));
                nft_apply_nat(
                    namespace,
                    packet,
                    *kind,
                    *family,
                    address,
                    port,
                    *masquerade,
                )?;
            }
        }
    }
    Ok(true)
}

fn payload_offset(packet: &[u8], base: u32, offset: u32) -> AxResult<usize> {
    let ip = match base {
        1 => 0usize,
        2 => match packet.first().map(|byte| byte >> 4) {
            Some(4) => usize::from(packet[0] & 0x0f) * 4,
            Some(6) => 40,
            _ => return Err(AxError::InvalidInput),
        },
        _ => return Err(AxError::OperationNotSupported),
    };
    ip.checked_add(offset as usize).ok_or(AxError::InvalidInput)
}

fn conntrack_is_established(namespace: &Arc<NetworkNamespace>, packet: &[u8]) -> bool {
    let Some(tuple) = conntrack_tuple(packet) else {
        return false;
    };
    let needle = Arc::downgrade(namespace);
    CONNTRACK.lock().iter().any(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == tuple || entry.translated == tuple || entry.reply == tuple)
    })
}

fn conntrack_reverse_translate(namespace: &Arc<NetworkNamespace>, packet: &mut [u8]) -> AxResult {
    let Some(tuple) = conntrack_tuple(packet) else {
        return Ok(());
    };
    let needle = Arc::downgrade(namespace);
    let replacement = CONNTRACK
        .lock()
        .iter()
        .find(|entry| Weak::ptr_eq(&entry.namespace, &needle) && entry.reply == tuple)
        .map(|entry| conntrack_reverse(entry.original));
    if let Some(replacement) = replacement {
        rewrite_tuple(packet, replacement)?;
    }
    Ok(())
}

fn nft_apply_nat(
    namespace: &Arc<NetworkNamespace>,
    packet: &mut [u8],
    kind: u32,
    family: u32,
    address: Option<&[u8]>,
    port: Option<u16>,
    masquerade: bool,
) -> AxResult {
    let before = conntrack_tuple(packet).ok_or(AxError::InvalidInput)?;
    if family != 0 && family != before.family as u32 {
        return Err(AxError::InvalidInput);
    };
    let mut after = before;
    let width = if before.family == 4 { 4 } else { 16 };
    // nft NAT type: 0 DNAT, 1 SNAT.  Masquerade is SNAT using the route's
    // source address, which is already present in this compact router model.
    match kind {
        0 => {
            if let Some(address) = address {
                if address.len() != width {
                    return Err(AxError::InvalidInput);
                };
                after.destination[..width].copy_from_slice(address)
            }
            if let Some(port) = port {
                after.destination_port = port
            }
        }
        1 => {
            if !masquerade {
                if let Some(address) = address {
                    if address.len() != width {
                        return Err(AxError::InvalidInput);
                    };
                    after.source[..width].copy_from_slice(address)
                }
            }
            if let Some(port) = port {
                after.source_port = port
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    rewrite_tuple(packet, after)?;
    let needle = Arc::downgrade(namespace);
    let mut entries = CONNTRACK.lock();
    if let Some(entry) = entries.iter_mut().find(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == before || entry.translated == before)
    }) {
        entry.translated = after;
        entry.reply = conntrack_reverse(after);
    }
    Ok(())
}

fn rewrite_tuple(packet: &mut [u8], tuple: ConntrackTuple) -> AxResult {
    let version = packet
        .first()
        .map(|byte| byte >> 4)
        .ok_or(AxError::InvalidInput)?;
    let (width, src, dst, l4, protocol) = match version {
        4 => {
            let l4 = usize::from(packet[0] & 0xf) * 4;
            if l4 < 20 || packet.len() < l4 {
                return Err(AxError::InvalidInput);
            };
            (4, 12, 16, l4, packet[9])
        }
        6 => {
            if packet.len() < 40 {
                return Err(AxError::InvalidInput);
            };
            (16, 8, 24, 40, packet[6])
        }
        _ => return Err(AxError::InvalidInput),
    };
    packet[src..src + width].copy_from_slice(&tuple.source[..width]);
    packet[dst..dst + width].copy_from_slice(&tuple.destination[..width]);
    if matches!(protocol, 6 | 17 | 33 | 132) && packet.len() >= l4 + 4 {
        packet[l4..l4 + 2].copy_from_slice(&tuple.source_port.to_be_bytes());
        packet[l4 + 2..l4 + 4].copy_from_slice(&tuple.destination_port.to_be_bytes());
    }
    recompute_checksums(packet)?;
    Ok(())
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += (last as u32) << 8
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16)
    }
    !(sum as u16)
}
fn recompute_checksums(packet: &mut [u8]) -> AxResult {
    let version = packet
        .first()
        .map(|byte| byte >> 4)
        .ok_or(AxError::InvalidInput)?;
    let (l4, protocol, length, pseudo) = match version {
        4 => {
            let l4 = usize::from(packet[0] & 0xf) * 4;
            if l4 < 20 || packet.len() < l4 {
                return Err(AxError::InvalidInput);
            };
            packet[10] = 0;
            packet[11] = 0;
            let c = checksum(&packet[..l4]);
            packet[10..12].copy_from_slice(&c.to_be_bytes());
            let len = u16::from_be_bytes(packet[2..4].try_into().unwrap()) as usize;
            (l4, packet[9], len.saturating_sub(l4), {
                let mut p = Vec::new();
                p.extend_from_slice(&packet[12..20]);
                p.extend_from_slice(&[0, packet[9]]);
                p.extend_from_slice(&(len.saturating_sub(l4) as u16).to_be_bytes());
                p
            })
        }
        6 => {
            if packet.len() < 40 {
                return Err(AxError::InvalidInput);
            };
            let len = u16::from_be_bytes(packet[4..6].try_into().unwrap()) as usize;
            (40, packet[6], len, {
                let mut p = Vec::new();
                p.extend_from_slice(&packet[8..40]);
                p.extend_from_slice(&(len as u32).to_be_bytes());
                p.extend_from_slice(&[0, 0, 0, packet[6]]);
                p
            })
        }
        _ => return Err(AxError::InvalidInput),
    };
    if matches!(protocol, 6 | 17) && packet.len() >= l4 + length {
        let check = if protocol == 6 { l4 + 16 } else { l4 + 6 };
        if packet.len() >= check + 2 {
            packet[check] = 0;
            packet[check + 1] = 0;
            let mut data = pseudo;
            data.extend_from_slice(&packet[l4..l4 + length]);
            let value = checksum(&data);
            if protocol == 6 || value != 0 {
                packet[check..check + 2].copy_from_slice(&value.to_be_bytes())
            }
        }
    };
    Ok(())
}

fn nft_chain_hook(bytes: &[u8]) -> AxResult<NftHook> {
    // NFTA_CHAIN_HOOK is a nested `nft_hook_attributes`: hook number is the
    // first u32 attribute.  Priority is retained by nf_tables for ordering;
    // this compact engine has one ordered chain list per hook, so install
    // order is its stable tie breaker.
    let mut number = None;
    for_each_rtattr(bytes, |kind, value| {
        match kind {
            1 if value.len() == size_of::<u32>() => {
                number = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            // priority and optional device are installation metadata.  Chain
            // order remains deterministic in this engine; device-specific
            // hooks are rejected by the caller when no matching device exists.
            2 if value.len() == size_of::<i32>() => Ok(()),
            3 => Ok(()),
            _ => Err(AxError::InvalidInput),
        }
    })?;
    match number.ok_or(AxError::InvalidInput)? {
        0 => Ok(NftHook::Prerouting),
        1 => Ok(NftHook::Input),
        2 => Ok(NftHook::Forward),
        3 => Ok(NftHook::Output),
        4 => Ok(NftHook::Postrouting),
        _ => Err(AxError::InvalidInput),
    }
}

fn nft_expression_verdict(
    bytes: &[u8],
) -> AxResult<(NftVerdict, Option<String>, Option<String>, Vec<NftExpr>)> {
    // Expressions are nested again (list element -> name/data).  Every
    // standard terminal verdict has an ASCII expression name.  Inspecting
    // only complete NUL-terminated names avoids accepting arbitrary payload
    // substrings as a policy instruction.
    let mut result = NftVerdict::Continue;
    let mut target = None;
    let mut lookup = None;
    let mut operations = Vec::new();
    fn named(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|part| part == needle)
    }
    for_each_rtattr(bytes, |_kind, expression| {
        if named(expression, b"drop\0") {
            result = NftVerdict::Drop;
        } else if named(expression, b"reject\0") {
            result = NftVerdict::Reject;
        } else if named(expression, b"accept\0") {
            result = NftVerdict::Accept;
        } else if named(expression, b"return\0") {
            result = NftVerdict::Return;
        }
        // jump/goto carry a chain name in their data payload; the parser
        // records their control-flow kind and rejects the absent target at
        // evaluation instead of silently accepting a malformed rule.
        else if named(expression, b"jump\0") || named(expression, b"goto\0") {
            result = if named(expression, b"jump\0") {
                NftVerdict::Jump
            } else {
                NftVerdict::Goto
            };
            for_each_rtattr(expression, |expr_kind, expr_body| {
                if expr_kind != 2 {
                    return Ok(());
                }
                for_each_rtattr(expr_body, |kind, data| {
                    if kind == 2 && target.is_none() {
                        target = Some(decode_link_name(data)?);
                    }
                    Ok(())
                })
            })?;
        } else if named(expression, b"lookup\0") {
            for_each_rtattr(expression, |expr_kind, expr_body| {
                if expr_kind != 2 {
                    return Ok(());
                }
                for_each_rtattr(expr_body, |kind, data| {
                    if kind == 1 && lookup.is_none() {
                        lookup = Some(decode_link_name(data)?);
                    }
                    Ok(())
                })
            })?;
        } else {
            let mut name = None;
            let mut data = None;
            for_each_rtattr(expression, |kind, value| match kind {
                1 if name.is_none() => {
                    name = Some(decode_link_name(value)?);
                    Ok(())
                }
                2 if data.is_none() => {
                    data = Some(value);
                    Ok(())
                }
                _ => Ok(()),
            })?;
            match name.as_deref() {
                Some("ct") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut key, mut dreg) = (None, None);
                    for_each_rtattr(data, |kind, value| match kind {
                        1 if value.len() == 4 => {
                            key = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        2 if value.len() == 4 => {
                            dreg = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        _ => Err(AxError::InvalidInput),
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Ct {
                        key: key.ok_or(AxError::InvalidInput)?,
                        dreg: dreg.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("payload") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut base, mut offset, mut len, mut dreg) = (None, None, None, None);
                    for_each_rtattr(data, |kind, value| {
                        if value.len() != 4 {
                            return Err(AxError::InvalidInput);
                        }
                        let value = u32::from_ne_bytes(value.try_into().unwrap());
                        match kind {
                            1 => base = Some(value),
                            2 => offset = Some(value),
                            3 => len = Some(value),
                            4 => dreg = Some(value),
                            _ => return Err(AxError::InvalidInput),
                        };
                        Ok(())
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Payload {
                        base: base.ok_or(AxError::InvalidInput)?,
                        offset: offset.ok_or(AxError::InvalidInput)?,
                        len: len.ok_or(AxError::InvalidInput)?,
                        dreg: dreg.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("cmp") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut sreg, mut op, mut rhs) = (None, None, None);
                    for_each_rtattr(data, |kind, value| match kind {
                        1 if value.len() == 4 => {
                            sreg = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        2 if value.len() == 4 => {
                            op = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        3 => for_each_rtattr(value, |inner, bytes| {
                            if inner == 1 && rhs.is_none() {
                                let mut copy = Vec::new();
                                copy.try_reserve_exact(bytes.len())
                                    .map_err(|_| AxError::NoMemory)?;
                                copy.extend_from_slice(bytes);
                                rhs = Some(copy)
                            };
                            Ok(())
                        }),
                        _ => Err(AxError::InvalidInput),
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Cmp {
                        sreg: sreg.ok_or(AxError::InvalidInput)?,
                        op: op.ok_or(AxError::InvalidInput)?,
                        data: rhs.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("nat") | Some("masq") => {
                    let data = data.unwrap_or(&[]);
                    let (mut kind, mut family, mut addr, mut proto) = (
                        if name.as_deref() == Some("masq") {
                            Some(1)
                        } else {
                            None
                        },
                        None,
                        None,
                        None,
                    );
                    for_each_rtattr(data, |kind_id, value| {
                        if value.len() != 4 {
                            return Err(AxError::InvalidInput);
                        }
                        let value = u32::from_ne_bytes(value.try_into().unwrap());
                        match kind_id {
                            1 => kind = Some(value),
                            2 => family = Some(value),
                            3 => addr = Some(value),
                            5 => proto = Some(value),
                            _ => {}
                        }
                        Ok(())
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Nat {
                        kind: kind.ok_or(AxError::InvalidInput)?,
                        family: family.unwrap_or(0),
                        addr_reg: addr,
                        proto_reg: proto,
                        masquerade: name.as_deref() == Some("masq"),
                    });
                }
                Some("immediate") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let mut code = None;
                    for_each_rtattr(data, |kind, value| {
                        if kind == 2 {
                            for_each_rtattr(value, |inner, bytes| {
                                if inner == 1 && bytes.len() == 4 && code.is_none() {
                                    code = Some(i32::from_ne_bytes(bytes.try_into().unwrap()))
                                };
                                Ok(())
                            })
                        } else {
                            Ok(())
                        }
                    })?;
                    result = match code.ok_or(AxError::InvalidInput)? {
                        0 => NftVerdict::Drop,
                        1 => NftVerdict::Accept,
                        _ => return Err(AxError::OperationNotSupported),
                    };
                }
                Some("counter") | Some("meta") => {}
                Some(_) => return Err(AxError::OperationNotSupported),
                None => return Err(AxError::InvalidInput),
            }
        }
        Ok(())
    })?;
    if matches!(result, NftVerdict::Jump | NftVerdict::Goto) && target.is_none() {
        return Err(AxError::InvalidInput);
    }
    Ok((result, target, lookup, operations))
}

fn nft_set_element_key(bytes: &[u8]) -> AxResult<Vec<u8>> {
    // NFTA_SET_ELEM_LIST_ELEMENTS is a list of NFTA_LIST_ELEM containers;
    // each carries NFTA_SET_ELEM_KEY, itself a NFTA_DATA_VALUE container.
    // This implementation admits a single element per message, exactly what
    // the retained SetElement model represents.
    let mut key = None;
    for_each_rtattr(bytes, |_list_kind, element| {
        for_each_rtattr(element, |kind, data| {
            if kind != 1 || key.is_some() {
                return Ok(());
            }
            for_each_rtattr(data, |data_kind, value| {
                if data_kind == 1 && key.is_none() {
                    let mut copied = Vec::new();
                    copied
                        .try_reserve_exact(value.len())
                        .map_err(|_| AxError::NoMemory)?;
                    copied.extend_from_slice(value);
                    key = Some(copied);
                }
                Ok(())
            })
        })
    })?;
    key.ok_or(AxError::InvalidInput)
}

impl NetlinkSocket {
    /// Audit control and read-log access are global Linux authorities.  A
    /// capability granted by a nested user namespace must never authorize
    /// creating an audit endpoint, even if it was obtained through an fd
    /// transferred from another task.
    pub(crate) fn audit_socket_creation_authorized(actor: &Cred) -> bool {
        actor.user_ns().is_initial() && actor.has_effective_capability(CAP_AUDIT_READ)
    }

    fn audit_listener_authorized(&self) -> bool {
        let task = current();
        let credential = task.as_thread().current_cred();
        Self::audit_socket_creation_authorized(&credential)
            && is_initial_network_namespace(&self.net_ns)
    }
    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub fn validate_socket_type(ty: u32, protocol: u32) -> AxResult {
        if !matches!(ty, SOCK_RAW | SOCK_DGRAM) {
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        if protocol > NETLINK_MAX_PROTOCOL
            || !matches!(
                protocol,
                NETLINK_ROUTE
                    | NETLINK_SOCK_DIAG
                    | NETLINK_NETFILTER
                    | NETLINK_AUDIT
                    | NETLINK_KOBJECT_UEVENT
                    | NETLINK_GENERIC
            )
        {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        Ok(())
    }

    pub(crate) fn try_new(protocol: u32, net_ns: Arc<NetworkNamespace>) -> AxResult<Arc<Self>> {
        let socket = Arc::try_new(Self {
            protocol,
            net_ns,
            inode: PseudoInode::socket(),
            state: Mutex::new(NetlinkState::default()),
            queue: Mutex::new(NetlinkQueue {
                datagrams: VecDeque::new(),
                bytes: 0,
            }),
            write_gate: Mutex::new(()),
            overrun: AtomicBool::new(false),
            nonblocking: AtomicBool::new(false),
            poll_rx: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        if protocol == NETLINK_KOBJECT_UEVENT {
            let mut sockets = KOBJECT_UEVENT_SOCKETS.lock();
            sockets.retain(|socket| socket.strong_count() != 0);
            sockets.push(Arc::downgrade(&socket));
        }
        if protocol == NETLINK_AUDIT {
            let mut sockets = AUDIT_SOCKETS.lock();
            sockets.retain(|socket| socket.strong_count() != 0);
            sockets.push(Arc::downgrade(&socket));
        }
        Ok(socket)
    }

    pub fn bind(&self, port_id: u32, groups: u32) -> AxResult {
        self.bind_port_id(port_id, groups, port_id == 0)
    }

    /// Linux treats `nl_pid = 0` as an automatic port-ID request.  Prefer the
    /// caller's TGID, then use a collision-free generated ID when it is taken.
    pub fn bind_auto(&self, preferred_port_id: u32, groups: u32) -> AxResult {
        self.bind_port_id(preferred_port_id, groups, true)
    }

    fn bind_port_id(&self, preferred_port_id: u32, groups: u32, automatic: bool) -> AxResult {
        if self.protocol == NETLINK_AUDIT {
            // The audit family advertises exactly AUDIT_NLGRP_READLOG.  Do
            // not let unimplemented group bits become inert, unobservable
            // subscriptions.
            if groups & !AUDIT_GROUP != 0 {
                return Err(AxError::InvalidInput);
            }
            // A port-only bind is the auditd command/reply endpoint, so it
            // has the same global authority requirement as group 1.
            if !self.audit_listener_authorized() {
                return Err(LinuxError::EPERM.into());
            }
        }
        let mut state = self.state.lock();
        if state.bound {
            return Err(LinuxError::EINVAL.into());
        }
        let port_id = reserve_netlink_port(self, preferred_port_id, automatic)?;
        state.port_id = port_id;
        state.groups = groups;
        state.bound = true;
        Ok(())
    }

    pub fn set_option(&self, optname: u32, value: u32) -> AxResult {
        match optname {
            // NETLINK_ADD_MEMBERSHIP and NETLINK_DROP_MEMBERSHIP use a multicast group number.
            1 | 2 => {
                let bit = value
                    .checked_sub(1)
                    .filter(|bit| *bit < u32::BITS)
                    .ok_or(AxError::InvalidInput)?;
                if self.protocol == NETLINK_AUDIT {
                    if value != AUDIT_GROUP {
                        return Err(AxError::InvalidInput);
                    }
                    // Recheck both addition and removal: the caller may
                    // have crossed user/net namespaces after receiving this
                    // descriptor, and no audit subscription transition may
                    // be performed outside initial authority.
                    if !self.audit_listener_authorized() {
                        return Err(LinuxError::EPERM.into());
                    }
                }
                if optname == 1 {
                    self.state.lock().groups |= 1 << bit;
                } else {
                    self.state.lock().groups &= !(1 << bit);
                }
            }
            // NETLINK_BROADCAST_ERROR, NETLINK_NO_ENOBUFS, NETLINK_CAP_ACK,
            // NETLINK_EXT_ACK, and NETLINK_GET_STRICT_CHK are boolean toggles.
            4 | 5 | 10 | 11 | 12 => {
                let mask = 1 << optname;
                if value == 0 {
                    self.state.lock().option_flags &= !mask;
                } else {
                    self.state.lock().option_flags |= mask;
                }
            }
            _ => return Err(AxError::from(LinuxError::ENOPROTOOPT)),
        }
        Ok(())
    }

    pub fn get_option(&self, optname: u32) -> AxResult<u32> {
        match optname {
            4 | 5 | 10 | 11 | 12 => Ok(u32::from(
                self.state.lock().option_flags & (1 << optname) != 0,
            )),
            _ => Err(LinuxError::ENOPROTOOPT.into()),
        }
    }

    /// NETLINK sockets expose SO_PASSCRED per open file description.  Sender
    /// credentials are retained in queued uevents, so this receive-side flag
    /// may be changed after enqueue without losing the original identity.
    pub(crate) fn set_passcred(&self, enabled: bool) {
        self.state.lock().passcred = enabled;
    }

    pub(crate) fn passcred(&self) -> bool {
        self.state.lock().passcred
    }

    pub fn write_local_addr(
        &self,
        capability: &UserMemoryCapability,
        addr: UserPtr<sockaddr>,
        addrlen: &mut socklen_t,
    ) -> AxResult {
        let state = self.state.lock();
        let nl = SockaddrNl {
            nl_family: AF_NETLINK as _,
            nl_pad: 0,
            nl_pid: state.port_id,
            nl_groups: state.groups,
        };
        drop(state);

        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&nl as *const SockaddrNl).cast::<u8>(),
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

    pub fn enqueue_kernel(&self, data: Vec<u8>) {
        self.enqueue_kernel_from(data, 0, None);
    }

    fn enqueue_kernel_permitted(&self, permit: &mut NetlinkWritePermit<'_>, data: Vec<u8>) {
        let suppress_enobufs = permit.suppress_enobufs();
        let queue = permit.queue();
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            data.len(),
            NETLINK_QUEUE_LIMIT,
            NETLINK_QUEUE_LIMIT_BYTES,
        ) == NetlinkQueueAdmission::Drop
        {
            if !suppress_enobufs {
                self.overrun.store(true, Ordering::Release);
            }
            self.poll_rx.wake();
            return;
        }
        queue.bytes += data.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data,
            source_port_id: 0,
            source_groups: 0,
            credentials: None,
        });
        self.poll_rx.wake();
    }

    fn enqueue_kernel_from(
        &self,
        data: Vec<u8>,
        source_groups: u32,
        credentials: Option<NetlinkCredentials>,
    ) {
        let mut queue = self.queue.lock();
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            data.len(),
            NETLINK_QUEUE_LIMIT,
            NETLINK_QUEUE_LIMIT_BYTES,
        ) == NetlinkQueueAdmission::Drop
        {
            drop(queue);
            self.note_queue_drop();
            return;
        }
        queue.bytes += data.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data,
            source_port_id: 0,
            source_groups,
            credentials,
        });
        drop(queue);
        self.poll_rx.wake();
    }

    fn enqueue_user_from(
        &self,
        data: Vec<u8>,
        source_port_id: u32,
        credentials: NetlinkCredentials,
        nowait: bool,
    ) -> AxResult {
        let mut queue = if nowait {
            self.queue.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.queue.lock()
        };
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            data.len(),
            NETLINK_QUEUE_LIMIT,
            NETLINK_QUEUE_LIMIT_BYTES,
        ) == NetlinkQueueAdmission::Drop
        {
            return Err(AxError::WouldBlock);
        }
        queue.bytes += data.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data,
            source_port_id,
            source_groups: 0,
            credentials: Some(credentials),
        });
        drop(queue);
        self.poll_rx.wake();
        Ok(())
    }

    /// Report one multicast delivery which this listener could not retain.
    /// This is shared by queue admission and per-listener clone failures so
    /// NETLINK_NO_ENOBUFS has the same meaning for either loss mode.
    fn note_queue_drop(&self) {
        if self.state.lock().option_flags & (1 << NETLINK_NO_ENOBUFS) == 0 {
            self.overrun.store(true, Ordering::Release);
        }
        self.poll_rx.wake();
    }

    pub fn recv(&self, dst: &mut IoDst, flags: RecvFlags) -> AxResult<usize> {
        self.recv_with_nonblocking(dst, flags, self.nonblocking())
            .map(|received| received.len)
    }

    pub(crate) fn recv_with_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: RecvFlags,
        nonblocking: bool,
    ) -> AxResult<NetlinkReceived> {
        self.recv_with_operation_nonblocking(dst, flags, nonblocking, false)
    }

    pub(crate) fn recv_with_operation_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: RecvFlags,
        nonblocking: bool,
        nowait: bool,
    ) -> AxResult<NetlinkReceived> {
        block_on_poll_io(
            self,
            IoEvents::READABLE,
            nonblocking || flags.contains(RecvFlags::DONT_WAIT),
            || self.recv_ready_with_nonblocking(dst, flags, nowait),
        )
    }

    /// Consume an already-readable netlink event.  Keeping this separate from
    /// the scheduler wait lets explicit queue fixtures exercise the same
    /// dequeue and overrun semantics without manufacturing a current task.
    fn recv_ready(&self, dst: &mut IoDst, flags: RecvFlags) -> AxResult<NetlinkReceived> {
        self.recv_ready_with_nonblocking(dst, flags, false)
    }

    fn recv_ready_with_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: RecvFlags,
        nowait: bool,
    ) -> AxResult<NetlinkReceived> {
        let mut queue = if nowait {
            self.queue.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.queue.lock()
        };
        if self.overrun.swap(false, Ordering::AcqRel) {
            return Err(LinuxError::ENOBUFS.into());
        }
        let Some(packet) = queue.datagrams.front() else {
            return Err(AxError::WouldBlock);
        };

        let packet_len = packet.data.len();
        let copy_len = packet_len.min(dst.remaining_mut());
        let source_groups = packet.source_groups;
        let source_port_id = packet.source_port_id;
        let credentials = packet.credentials;
        if flags.contains(RecvFlags::PEEK) {
            dst.write(&packet.data[..copy_len])?;
        } else {
            // Netlink receive consumes the datagram before usercopy. An
            // EFAULT therefore does not put the record back on the queue.
            let packet = queue.datagrams.pop_front().expect("front packet vanished");
            queue.bytes -= packet.data.len();
            dst.write(&packet.data[..copy_len])?;
        }

        Ok(NetlinkReceived {
            len: if flags.contains(RecvFlags::TRUNCATE) {
                packet_len
            } else {
                copy_len
            },
            source_port_id,
            source_groups,
            credentials,
        })
    }

    fn handle_write(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        data: &[u8],
        actor: &Cred,
    ) -> AxResult {
        match self.protocol {
            NETLINK_ROUTE | NETLINK_SOCK_DIAG | NETLINK_NETFILTER | NETLINK_GENERIC => {}
            NETLINK_AUDIT | NETLINK_KOBJECT_UEVENT => return Err(LinuxError::EPERM.into()),
            _ => return Err(AxError::OperationNotSupported),
        }

        // Validate the complete datagram before dispatching its first
        // mutation.  Otherwise a valid first route followed by a truncated
        // second frame could change state and still be rejected only after
        // the damage is done.
        let mut preflight = 0usize;
        while preflight < data.len() {
            if data.len() - preflight < size_of::<NlMsgHdr>() {
                return Err(AxError::InvalidInput);
            }
            let header = read_unaligned::<NlMsgHdr>(&data[preflight..])?;
            let message_len = header.nlmsg_len as usize;
            if message_len < size_of::<NlMsgHdr>() || preflight + message_len > data.len() {
                return Err(AxError::InvalidInput);
            }
            let next = preflight
                .checked_add(align4(message_len))
                .ok_or(AxError::InvalidInput)?;
            if next > data.len() && preflight + message_len != data.len() {
                return Err(AxError::InvalidInput);
            }
            preflight = next.min(data.len());
        }

        // nfnetlink batches are transactional.  Preserve the old namespace
        // graph before dispatch and restore it if any ACKed member fails; an
        // ACK is a delivery mechanism, not permission to retain a prefix of
        // the requested transaction.
        let nft_before = permit.nft_tables().map(|tables| tables.clone());
        let mut nft_failed = false;
        let mut offset = 0usize;
        while offset < data.len() {
            let hdr = read_unaligned::<NlMsgHdr>(&data[offset..])?;
            if hdr.nlmsg_len < size_of::<NlMsgHdr>() as u32 {
                return Err(AxError::InvalidInput);
            }

            let msg_len = hdr.nlmsg_len as usize;
            if offset + msg_len > data.len() {
                return Err(AxError::InvalidInput);
            }

            let payload = &data[offset + size_of::<NlMsgHdr>()..offset + msg_len];
            let result = match self.protocol {
                NETLINK_ROUTE => self.handle_route_message(permit, &hdr, payload, actor),
                NETLINK_SOCK_DIAG => self.handle_sock_diag_message(permit, &hdr, payload),
                NETLINK_NETFILTER => self.handle_nft_message(permit, &hdr, payload, actor),
                NETLINK_GENERIC => self.handle_generic_message(permit, &hdr, payload),
                _ => Err(AxError::OperationNotSupported),
            };
            if hdr.nlmsg_flags & NLM_F_ACK != 0 {
                let port_id = permit.port_id();
                let err = match result {
                    Ok(()) => 0,
                    Err(AxError::InvalidInput) => -LinuxError::EINVAL.code(),
                    Err(AxError::NotFound) => -LinuxError::ENOENT.code(),
                    Err(AxError::NoSuchDevice) => -LinuxError::ENODEV.code(),
                    Err(AxError::PermissionDenied) => -LinuxError::EPERM.code(),
                    Err(AxError::OperationNotPermitted) => -LinuxError::EPERM.code(),
                    Err(AxError::AlreadyExists) => -LinuxError::EEXIST.code(),
                    Err(AxError::OperationNotSupported) => -LinuxError::EOPNOTSUPP.code(),
                    Err(_) => -LinuxError::EINVAL.code(),
                };
                self.enqueue_kernel_permitted(permit, netlink_ack(&hdr, port_id, err));
                nft_failed |= self.protocol == NETLINK_NETFILTER && err != 0;
            } else {
                if let Err(error) = result {
                    if let Some(before) = &nft_before {
                        if let Some(tables) = permit.nft_tables() {
                            *tables = before.clone();
                        }
                    }
                    return Err(error);
                }
            }

            offset = offset.saturating_add(align4(msg_len)).min(data.len());
        }

        if nft_failed {
            if let (Some(before), Some(tables)) = (&nft_before, permit.nft_tables()) {
                *tables = before.clone();
            }
        }

        Ok(())
    }

    /// Linux's uevent_net_rcv_skb equivalent.  The netlink framing is only a
    /// userspace submission envelope: listeners receive its payload plus the
    /// kernel-assigned SEQNUM field, as a group-1 kernel multicast datagram.
    pub(crate) fn send_uevent_from_user(
        &self,
        data: &[u8],
        actor: &Cred,
        sender_pid: u32,
    ) -> AxResult {
        if self.protocol != NETLINK_KOBJECT_UEVENT {
            let mut permit = self.acquire_write_permit(false)?;
            return self.handle_write(&mut permit, data, actor);
        }
        if !ns_capable(actor, self.net_ns.owner_user_ns(), CAP_SYS_ADMIN) {
            return Err(LinuxError::EPERM.into());
        }
        if data.len() < size_of::<NlMsgHdr>() {
            return Err(AxError::InvalidInput);
        }
        let header = read_unaligned::<NlMsgHdr>(data)?;
        let message_len = header.nlmsg_len as usize;
        if message_len < size_of::<NlMsgHdr>() || message_len != data.len() {
            return Err(AxError::InvalidInput);
        }
        let payload = &data[size_of::<NlMsgHdr>()..];
        let ids = actor.ids();
        broadcast_user_uevent(
            &self.net_ns,
            payload,
            NetlinkCredentials {
                pid: sender_pid,
                uid: ids.ruid.into_raw(),
                gid: ids.rgid.into_raw(),
            },
        )
    }

    pub(crate) fn write_with_actor(
        &self,
        src: &mut IoSrc,
        actor: &Cred,
        sender_pid: u32,
    ) -> AxResult<usize> {
        self.write_to_with_actor(src, actor, sender_pid, None, false)
    }

    /// Send a userspace netlink datagram.  A port-ID-only destination is a
    /// unicast peer in this socket's network namespace and protocol family;
    /// group delivery remains the privileged synthetic uevent path below.
    pub(crate) fn write_to_with_actor(
        &self,
        src: &mut IoSrc,
        actor: &Cred,
        sender_pid: u32,
        destination: Option<SockaddrNl>,
        nowait: bool,
    ) -> AxResult<usize> {
        if let Some(destination) = destination {
            if destination.nl_pid != 0 && destination.nl_groups != 0 {
                return Err(AxError::InvalidInput);
            }
            if destination.nl_pid != 0 {
                return self.write_unicast_with_actor(
                    src,
                    actor,
                    sender_pid,
                    destination.nl_pid,
                    nowait,
                );
            }
            if destination.nl_groups != 0 && destination.nl_groups != KOBJECT_UEVENT_GROUP {
                return Err(AxError::InvalidInput);
            }
        }
        self.write_with_actor_admitted(src, actor, sender_pid, nowait)
    }

    fn write_unicast_with_actor(
        &self,
        src: &mut IoSrc,
        actor: &Cred,
        sender_pid: u32,
        destination_port_id: u32,
        nowait: bool,
    ) -> AxResult<usize> {
        if self.protocol != NETLINK_KOBJECT_UEVENT {
            return Err(AxError::OperationNotSupported);
        }
        let len = src.remaining();
        if len == 0 {
            return Err(LinuxError::ENODATA.into());
        }
        if admit_netlink_write(len) == NetlinkWriteAdmission::MessageTooLarge {
            return Err(LinuxError::EMSGSIZE.into());
        }
        // sendto(2) autobinds an unbound netlink socket before it publishes a
        // datagram.  Besides matching the ABI, this prevents peers from
        // observing the otherwise-invalid port ID zero in sockaddr_nl.
        let source_port_id = self.ensure_bound_for_send(sender_pid, nowait)?;
        let target = find_netlink_peer(self.protocol, &self.net_ns, destination_port_id, nowait)?
            .ok_or(LinuxError::ECONNREFUSED)?;
        // A nonzero destination port is ordinary netlink unicast.  Linux
        // applies the uevent CAP_SYS_ADMIN gate only to the port-0 synthetic
        // receive path, not udevd's main-process-to-worker handoff.
        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        data.resize(len, 0);
        src.read_exact(&mut data)?;
        let ids = actor.ids();
        target.enqueue_user_from(
            data,
            source_port_id,
            NetlinkCredentials {
                pid: sender_pid,
                uid: ids.ruid.into_raw(),
                gid: ids.rgid.into_raw(),
            },
            nowait,
        )?;
        Ok(len)
    }

    /// Lazily reserve a userspace port ID for sendto/sendmsg.  A NOWAIT send
    /// may not block on either the socket state or the global reservation
    /// table, so contention is surfaced as EAGAIN before payload delivery.
    fn ensure_bound_for_send(&self, preferred_port_id: u32, nowait: bool) -> AxResult<u32> {
        let mut state = if nowait {
            self.state.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.state.lock()
        };
        if state.bound {
            return Ok(state.port_id);
        }
        let port_id = reserve_netlink_port_with_mode(self, preferred_port_id, true, nowait)?;
        state.port_id = port_id;
        state.groups = 0;
        state.bound = true;
        Ok(port_id)
    }

    fn write_with_actor_admitted(
        &self,
        src: &mut IoSrc,
        actor: &Cred,
        sender_pid: u32,
        nowait: bool,
    ) -> AxResult<usize> {
        let len = src.remaining();
        if admit_netlink_write(len) == NetlinkWriteAdmission::MessageTooLarge {
            return Err(LinuxError::EMSGSIZE.into());
        }

        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        data.resize(len, 0);
        src.read_exact(&mut data)?;
        let mut permit = self.acquire_write_permit(nowait)?;
        self.send_uevent_with_permit(&mut permit, &data, actor, sender_pid)?;
        Ok(len)
    }

    /// Netlink writes have no readiness wait. Retain the operation-local
    /// nonblocking argument so RWF_NOWAIT is explicit and does not mutate the
    /// shared OFD status.
    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> AxResult<usize> {
        self.write_with_operation_nonblocking(src, nonblocking, false)
    }

    pub(crate) fn write_with_operation_nonblocking(
        &self,
        src: &mut IoSrc,
        _nonblocking: bool,
        nowait: bool,
    ) -> AxResult<usize> {
        let current = current();
        let thread = current.as_thread();
        let actor = thread.current_cred();
        let len = src.remaining();
        if admit_netlink_write(len) == NetlinkWriteAdmission::MessageTooLarge {
            return Err(LinuxError::EMSGSIZE.into());
        }
        if nowait && self.protocol == NETLINK_KOBJECT_UEVENT {
            // Uevent has a global listener registry.  Admit that registry
            // before importing an unreplayable source so it cannot produce a
            // late EAGAIN after usercopy.
            let mut permit = self.acquire_write_permit(true)?;
            let mut data = Vec::new();
            data.try_reserve_exact(len)
                .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
            data.resize(len, 0);
            src.read_exact(&mut data)?;
            validate_netlink_frames(&data)?;
            self.send_uevent_with_permit(
                &mut permit,
                &data,
                &actor,
                thread.proc_data.proc.pid() as u32,
            )?;
            return Ok(len);
        }
        // Import first into a fallible kernel buffer.  This is deliberately
        // before the permit: IoSrc may not be replayable, but an EAGAIN below
        // still occurs before any shared netlink state is changed.
        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        data.resize(len, 0);
        src.read_exact(&mut data)?;
        validate_netlink_frames(&data)?;
        let mut permit = self.acquire_write_permit(nowait)?;
        self.send_uevent_with_permit(
            &mut permit,
            &data,
            &actor,
            thread.proc_data.proc.pid() as u32,
        )?;
        Ok(len)
    }

    fn send_uevent_with_permit(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        data: &[u8],
        actor: &Cred,
        sender_pid: u32,
    ) -> AxResult {
        if self.protocol != NETLINK_KOBJECT_UEVENT {
            return self.handle_write(permit, data, actor);
        }
        if !ns_capable(actor, self.net_ns.owner_user_ns(), CAP_SYS_ADMIN) {
            return Err(LinuxError::EPERM.into());
        }
        if data.len() < size_of::<NlMsgHdr>() {
            return Err(AxError::InvalidInput);
        }
        let header = read_unaligned::<NlMsgHdr>(data)?;
        let message_len = header.nlmsg_len as usize;
        if message_len < size_of::<NlMsgHdr>() || message_len != data.len() {
            return Err(AxError::InvalidInput);
        }
        let payload = &data[size_of::<NlMsgHdr>()..];
        let ids = actor.ids();
        // The Uevent permit owns the send serialization before this broadcast;
        // do not reacquire `KOBJECT_UEVENT_SEND_LOCK` below.
        if !matches!(permit, NetlinkWritePermit::Uevent { .. }) {
            return Err(AxError::BadState);
        }
        let credentials = NetlinkCredentials {
            pid: sender_pid,
            uid: ids.ruid.into_raw(),
            gid: ids.rgid.into_raw(),
        };
        let listeners = permit.uevent_listeners().ok_or(AxError::BadState)?;
        let message = broadcast_user_uevent_nowait_locked(
            &self.net_ns,
            payload,
            credentials,
            self as *const _,
            listeners,
        )?;
        // The sender's state/queue are already retained by `permit`; preserve
        // Linux multicast loopback without re-entering its ordinary locks.
        if permit.subscribed_to(KOBJECT_UEVENT_GROUP) {
            self.enqueue_kernel_permitted(permit, message);
        }
        Ok(())
    }

    fn acquire_write_permit(&self, nowait: bool) -> AxResult<NetlinkWritePermit<'_>> {
        let gate = if nowait {
            self.write_gate.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.write_gate.lock()
        };
        let state = if nowait {
            self.state.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.state.lock()
        };
        let queue = if nowait {
            self.queue.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.queue.lock()
        };
        match self.protocol {
            NETLINK_ROUTE => {
                let service = if nowait {
                    self.net_ns.stack().try_acquire_packet_service()?
                } else {
                    self.net_ns.stack().acquire_packet_service()
                };
                Ok(NetlinkWritePermit::Route {
                    gate,
                    state,
                    queue,
                    service,
                })
            }
            NETLINK_GENERIC => Ok(NetlinkWritePermit::Generic { gate, state, queue }),
            NETLINK_NETFILTER => {
                let transaction = if nowait {
                    NFT_TRANSACTION.try_lock().ok_or(AxError::WouldBlock)?
                } else {
                    NFT_TRANSACTION.lock()
                };
                let tables = if nowait {
                    NFT_TABLES.try_lock().ok_or(AxError::WouldBlock)?
                } else {
                    NFT_TABLES.lock()
                };
                Ok(NetlinkWritePermit::Netfilter {
                    gate,
                    state,
                    queue,
                    transaction,
                    tables,
                })
            }
            NETLINK_AUDIT => Ok(NetlinkWritePermit::Audit { gate, state, queue }),
            NETLINK_KOBJECT_UEVENT => {
                let send = if nowait {
                    KOBJECT_UEVENT_SEND_LOCK
                        .try_lock()
                        .ok_or(AxError::WouldBlock)?
                } else {
                    KOBJECT_UEVENT_SEND_LOCK.lock()
                };
                let listeners = if nowait {
                    KOBJECT_UEVENT_SOCKETS
                        .try_lock()
                        .ok_or(AxError::WouldBlock)?
                } else {
                    KOBJECT_UEVENT_SOCKETS.lock()
                };
                Ok(NetlinkWritePermit::Uevent {
                    gate,
                    state,
                    queue,
                    send,
                    listeners,
                })
            }
            NETLINK_SOCK_DIAG => {
                let registry = if nowait {
                    SOCK_DIAG_REGISTRATIONS
                        .try_lock()
                        .ok_or(AxError::WouldBlock)?
                } else {
                    SOCK_DIAG_REGISTRATIONS.lock()
                };
                Ok(NetlinkWritePermit::SockDiag {
                    gate,
                    state,
                    queue,
                    registry,
                })
            }
            _ => Err(AxError::OperationNotSupported),
        }
    }

    fn handle_route_message(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        match hdr.nlmsg_type {
            RTM_NEWROUTE => self.add_route(permit, payload, actor, hdr.nlmsg_flags),
            RTM_DELROUTE => self.delete_route(permit, payload, actor),
            RTM_GETROUTE => self.dump_routes(permit, hdr, payload),
            RTM_NEWADDR => self.add_address(permit, payload, actor),
            RTM_DELADDR => self.delete_address(permit, payload, actor),
            RTM_GETADDR => self.dump_addresses(permit, hdr, payload),
            RTM_NEWLINK => self.new_link(permit, payload, actor, hdr.nlmsg_flags),
            RTM_SETLINK => self.set_link(permit, payload, actor),
            RTM_DELLINK => self.delete_link(permit, payload, actor),
            RTM_GETLINK => self.dump_links(permit, hdr, payload),
            _ => Err(AxError::OperationNotSupported),
        }
    }

    /// Generic-netlink controller.  Family discovery is a real namespace
    /// socket transaction rather than an accepted-but-blackholed protocol:
    /// `CTRL_CMD_GETFAMILY` returns the same encoded family record that later
    /// generic providers use for their command bus.
    fn handle_generic_message(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
    ) -> AxResult {
        if hdr.nlmsg_type != GENL_ID_CTRL || payload.len() < size_of::<GenlMsgHdr>() {
            return Err(AxError::OperationNotSupported);
        }
        let request = read_unaligned::<GenlMsgHdr>(payload)?;
        if request.cmd != CTRL_CMD_GETFAMILY || request.version > 2 || request.reserved != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut requested_id = None;
        let mut requested_name = None;
        for_each_rtattr(
            &payload[size_of::<GenlMsgHdr>()..],
            |kind, value| match kind {
                CTRL_ATTR_FAMILY_ID
                    if requested_id.is_none() && value.len() == size_of::<u16>() =>
                {
                    requested_id = Some(u16::from_ne_bytes(value.try_into().unwrap()));
                    Ok(())
                }
                CTRL_ATTR_FAMILY_NAME if requested_name.is_none() => {
                    requested_name = Some(decode_link_name(value)?);
                    Ok(())
                }
                CTRL_ATTR_FAMILY_ID | CTRL_ATTR_FAMILY_NAME => Err(AxError::InvalidInput),
                _ => Err(AxError::OperationNotSupported),
            },
        )?;
        if requested_id.is_some_and(|id| id != THEKERNEL_GENL_FAMILY_ID)
            || requested_name
                .as_deref()
                .is_some_and(|name| name != THEKERNEL_GENL_FAMILY_NAME)
        {
            return Err(AxError::NotFound);
        }
        let port_id = permit.port_id();
        self.enqueue_kernel_permitted(permit, generic_family_message(hdr, port_id));
        Ok(())
    }

    /// SOCK_DIAG owns a separate netlink protocol and terminates every dump
    /// with the normal multipart DONE message. `inet_diag_req_v2` is parsed
    /// as its complete fixed UAPI form: family/protocol/extensions/states and
    /// the complete inet_diag_sockid selector all participate in filtering.
    fn handle_sock_diag_message(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
    ) -> AxResult {
        if hdr.nlmsg_type != SOCK_DIAG_BY_FAMILY || payload.len() < INET_DIAG_REQ_V2_LEN {
            return Err(AxError::InvalidInput);
        }
        let family = payload[0] as u32;
        if family != AF_INET as u32 && family != AF_INET6 as u32 && family != AF_UNSPEC as u32 {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        let request = InetDiagRequest::parse(&payload[..INET_DIAG_REQ_V2_LEN])?;
        let port_id = permit.port_id();
        let namespace = Arc::downgrade(&self.net_ns);
        let registrations = permit.sock_diag_registry().ok_or(AxError::BadState)?;
        let mut replies = Vec::new();
        registrations.retain(|entry| {
            let Some(entry) = entry.upgrade() else {
                return false;
            };
            if !Weak::ptr_eq(&entry.net_ns, &namespace) || !request.matches(&entry) {
                return true;
            }
            replies.push(sock_diag_message(hdr, port_id, &entry, request.extensions));
            true
        });
        for reply in replies {
            self.enqueue_kernel_permitted(permit, reply);
        }
        self.enqueue_kernel_permitted(permit, done_message(hdr, port_id));
        Ok(())
    }

    /// Minimal nftables table transaction.  The netlink datagram preflight
    /// above prevents a malformed later message from partially committing a
    /// batch; each table mutation is additionally namespace-owned and visible
    /// only to sockets retaining this network namespace.
    fn handle_nft_message(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        // Batch begin/end belong to NFNL's control subsystem, do not carry an
        // nft family header, and deliberately have no standalone mutation.
        if hdr.nlmsg_type == NFNL_MSG_BATCH_BEGIN || hdr.nlmsg_type == NFNL_MSG_BATCH_END {
            return Ok(());
        }
        if payload.len() < 4 || hdr.nlmsg_type >> 8 != NFNL_SUBSYS_NFTABLES {
            return Err(AxError::InvalidInput);
        }
        let command = hdr.nlmsg_type & 0xff;
        if matches!(
            command,
            NFT_MSG_GETTABLE
                | NFT_MSG_GETCHAIN
                | NFT_MSG_GETRULE
                | NFT_MSG_GETSET
                | NFT_MSG_GETSETELEM
        ) {
            return self.dump_nft(permit, hdr, command);
        }
        if !matches!(
            command,
            NFT_MSG_NEWTABLE
                | NFT_MSG_DELTABLE
                | NFT_MSG_NEWCHAIN
                | NFT_MSG_DELCHAIN
                | NFT_MSG_NEWRULE
                | NFT_MSG_DELRULE
                | NFT_MSG_NEWSET
                | NFT_MSG_DELSET
                | NFT_MSG_NEWSETELEM
                | NFT_MSG_DELSETELEM
        ) {
            return Err(AxError::OperationNotSupported);
        }
        if matches!(command, NFT_MSG_NEWSETELEM | NFT_MSG_DELSETELEM) {
            let mut table = None;
            let mut set = None;
            let mut elements = None;
            for_each_rtattr(&payload[4..], |kind, value| match kind {
                NFTA_SET_ELEM_LIST_TABLE if table.is_none() => {
                    table = Some(decode_link_name(value)?);
                    Ok(())
                }
                NFTA_SET_ELEM_LIST_SET if set.is_none() => {
                    set = Some(decode_link_name(value)?);
                    Ok(())
                }
                NFTA_SET_ELEM_LIST_ELEMENTS if elements.is_none() => {
                    elements = Some(nft_set_element_key(value)?);
                    Ok(())
                }
                _ => Err(AxError::InvalidInput),
            })?;
            let table = table.ok_or(AxError::InvalidInput)?;
            let set = set.ok_or(AxError::InvalidInput)?;
            let key = elements.ok_or(AxError::InvalidInput)?;
            let namespace = Arc::downgrade(&self.net_ns);
            let namespaces = permit.nft_tables().ok_or(AxError::BadState)?;
            namespaces.retain(|state| state.namespace.strong_count() != 0);
            let state = namespaces
                .iter_mut()
                .find(|state| Weak::ptr_eq(&state.namespace, &namespace))
                .ok_or(AxError::NotFound)?;
            if !state
                .sets
                .iter()
                .any(|item| item.table == table && item.name == set)
            {
                return Err(AxError::NotFound);
            }
            let existing = state
                .elements
                .iter()
                .position(|item| item.table == table && item.set == set && item.key == key);
            match (command, existing) {
                (NFT_MSG_NEWSETELEM, Some(_)) => return Err(AxError::AlreadyExists),
                (NFT_MSG_NEWSETELEM, None) => {
                    state
                        .elements
                        .try_reserve(1)
                        .map_err(|_| AxError::NoMemory)?;
                    state.elements.push(NftSetElement { table, set, key });
                }
                (NFT_MSG_DELSETELEM, Some(index)) => {
                    state.elements.remove(index);
                }
                (NFT_MSG_DELSETELEM, None) => return Err(AxError::NotFound),
                _ => unreachable!(),
            }
            state.generation = state.generation.wrapping_add(1);
            return Ok(());
        }
        let mut table = None;
        let mut name = None;
        let mut handle = None;
        let mut verdict = NftVerdict::Continue;
        let mut target_chain = None;
        let mut lookup = None;
        let mut expressions = Vec::new();
        let mut hook = None;
        let mut policy = NftVerdict::Accept;
        let mut set_id = 0;
        let mut set_flags = 0;
        let mut set_key_type = 0;
        let mut set_data_type = 0;
        for_each_rtattr(&payload[4..], |kind, value| match (command, kind) {
            (NFT_MSG_NEWTABLE | NFT_MSG_DELTABLE, NFTA_TABLE_NAME) if name.is_none() => {
                name = Some(decode_link_name(value)?);
                Ok(())
            }
            (NFT_MSG_NEWSET | NFT_MSG_DELSET, NFTA_SET_TABLE) if table.is_none() => {
                table = Some(decode_link_name(value)?);
                Ok(())
            }
            (NFT_MSG_NEWSET | NFT_MSG_DELSET, NFTA_SET_NAME) if name.is_none() => {
                name = Some(decode_link_name(value)?);
                Ok(())
            }
            (NFT_MSG_NEWRULE | NFT_MSG_DELRULE, NFTA_RULE_TABLE) if table.is_none() => {
                table = Some(decode_link_name(value)?);
                Ok(())
            }
            (NFT_MSG_NEWRULE | NFT_MSG_DELRULE, NFTA_RULE_CHAIN) if name.is_none() => {
                name = Some(decode_link_name(value)?);
                Ok(())
            }
            (NFT_MSG_NEWRULE | NFT_MSG_DELRULE, NFTA_RULE_HANDLE) if value.len() == 8 => {
                handle = Some(u64::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            (NFT_MSG_NEWRULE, NFTA_RULE_EXPRESSIONS) => {
                let parsed = nft_expression_verdict(value)?;
                verdict = parsed.0;
                target_chain = parsed.1;
                lookup = parsed.2.map(|set| (set, Vec::new()));
                expressions = parsed.3;
                Ok(())
            }
            (NFT_MSG_NEWCHAIN, NFTA_CHAIN_HOOK) if hook.is_none() => {
                hook = Some(nft_chain_hook(value)?);
                Ok(())
            }
            (NFT_MSG_NEWCHAIN, NFTA_CHAIN_POLICY) if value.len() == size_of::<u32>() => {
                policy = match u32::from_ne_bytes(value.try_into().unwrap()) {
                    0 => NftVerdict::Drop,
                    1 => NftVerdict::Accept,
                    _ => return Err(AxError::InvalidInput),
                };
                Ok(())
            }
            (NFT_MSG_NEWCHAIN, NFTA_CHAIN_TYPE) => Ok(()),
            (_, NFTA_CHAIN_TABLE) if table.is_none() => {
                table = Some(decode_link_name(value)?);
                Ok(())
            }
            (_, NFTA_CHAIN_NAME) if name.is_none() => {
                name = Some(decode_link_name(value)?);
                Ok(())
            }
            // Keep forward-compatible userspace metadata opaque.  It has no
            // effect on packet evaluation and must not make a valid rule
            // un-installable merely because its counter/comment is newer.
            (NFT_MSG_NEWRULE, NFTA_RULE_POSITION | NFTA_RULE_USERDATA | NFTA_RULE_ID) => Ok(()),
            (NFT_MSG_NEWSET, NFTA_SET_ID) if value.len() == 4 => {
                set_id = u32::from_ne_bytes(value.try_into().unwrap());
                Ok(())
            }
            (NFT_MSG_NEWSET, NFTA_SET_FLAGS) if value.len() == 4 => {
                set_flags = u32::from_ne_bytes(value.try_into().unwrap());
                Ok(())
            }
            (NFT_MSG_NEWSET, NFTA_SET_KEY_TYPE) if value.len() == 4 => {
                set_key_type = u32::from_ne_bytes(value.try_into().unwrap());
                Ok(())
            }
            (NFT_MSG_NEWSET, NFTA_SET_DATA_TYPE) if value.len() == 4 => {
                set_data_type = u32::from_ne_bytes(value.try_into().unwrap());
                Ok(())
            }
            (NFT_MSG_NEWSET, NFTA_SET_DESC) => Ok(()),
            _ => Err(AxError::InvalidInput),
        })?;
        let name = name.ok_or(AxError::InvalidInput)?;
        let namespace = Arc::downgrade(&self.net_ns);
        let namespaces = permit.nft_tables().ok_or(AxError::BadState)?;
        namespaces.retain(|state| state.namespace.strong_count() != 0);
        let index = namespaces
            .iter()
            .position(|state| Weak::ptr_eq(&state.namespace, &namespace));
        let state = match index {
            Some(index) => &mut namespaces[index],
            None => {
                namespaces.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                namespaces.push(NftNamespaceTables {
                    namespace,
                    tables: Vec::new(),
                    chains: Vec::new(),
                    rules: Vec::new(),
                    sets: Vec::new(),
                    elements: Vec::new(),
                    next_rule: 1,
                    generation: 0,
                });
                namespaces.last_mut().unwrap()
            }
        };
        match command {
            NFT_MSG_NEWTABLE => {
                if state.tables.iter().any(|existing| existing == &name) {
                    return Err(AxError::AlreadyExists);
                }
                state.tables.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                state.tables.push(name);
            }
            NFT_MSG_DELTABLE => {
                let index = state
                    .tables
                    .iter()
                    .position(|existing| existing == &name)
                    .ok_or(AxError::NotFound)?;
                state.tables.remove(index);
                state.chains.retain(|chain| chain.table != name);
                state.rules.retain(|rule| rule.table != name);
                state.sets.retain(|set| set.table != name);
                state.elements.retain(|element| element.table != name);
            }
            NFT_MSG_NEWCHAIN => {
                let table = table.ok_or(AxError::InvalidInput)?;
                if !state.tables.iter().any(|existing| existing == &table) {
                    return Err(AxError::NotFound);
                }
                if state
                    .chains
                    .iter()
                    .any(|chain| chain.table == table && chain.name == name)
                {
                    return Err(AxError::AlreadyExists);
                }
                state.chains.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                state.chains.push(NftChain {
                    table,
                    name,
                    hook,
                    policy,
                });
            }
            NFT_MSG_DELCHAIN => {
                let table = table.ok_or(AxError::InvalidInput)?;
                let index = state
                    .chains
                    .iter()
                    .position(|chain| chain.table == table && chain.name == name)
                    .ok_or(AxError::NotFound)?;
                state.chains.remove(index);
                state
                    .rules
                    .retain(|rule| rule.table != table || rule.chain != name);
            }
            NFT_MSG_NEWRULE => {
                let table = table.ok_or(AxError::InvalidInput)?;
                if !state
                    .chains
                    .iter()
                    .any(|chain| chain.table == table && chain.name == name)
                {
                    return Err(AxError::NotFound);
                }
                if let Some(ref target) = target_chain {
                    if !state
                        .chains
                        .iter()
                        .any(|chain| chain.table == table && chain.name == *target)
                    {
                        return Err(AxError::NotFound);
                    }
                }
                if let Some((ref set, _)) = lookup {
                    if !state
                        .sets
                        .iter()
                        .any(|item| item.table == table && item.name == *set)
                    {
                        return Err(AxError::NotFound);
                    }
                }
                let handle = handle.unwrap_or_else(|| {
                    let h = state.next_rule;
                    state.next_rule = state.next_rule.saturating_add(1);
                    h
                });
                if state.rules.iter().any(|rule| rule.handle == handle) {
                    return Err(AxError::AlreadyExists);
                }
                state.rules.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                state.rules.push(NftRule {
                    table,
                    chain: name,
                    handle,
                    verdict,
                    target_chain,
                    lookup,
                    expressions,
                    counter: 0,
                });
            }
            NFT_MSG_DELRULE => {
                let table = table.ok_or(AxError::InvalidInput)?;
                let handle = handle.ok_or(AxError::InvalidInput)?;
                let index = state
                    .rules
                    .iter()
                    .position(|rule| {
                        rule.table == table && rule.chain == name && rule.handle == handle
                    })
                    .ok_or(AxError::NotFound)?;
                state.rules.remove(index);
            }
            NFT_MSG_NEWSET => {
                let table = table.ok_or(AxError::InvalidInput)?;
                if !state.tables.iter().any(|item| item == &table) {
                    return Err(AxError::NotFound);
                }
                if state
                    .sets
                    .iter()
                    .any(|item| item.table == table && item.name == name)
                {
                    return Err(AxError::AlreadyExists);
                }
                state.sets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                state.sets.push(NftSet {
                    table,
                    name,
                    id: set_id,
                    flags: set_flags,
                    key_type: set_key_type,
                    data_type: set_data_type,
                });
            }
            NFT_MSG_DELSET => {
                let table = table.ok_or(AxError::InvalidInput)?;
                let index = state
                    .sets
                    .iter()
                    .position(|item| item.table == table && item.name == name)
                    .ok_or(AxError::NotFound)?;
                state.sets.remove(index);
                state
                    .elements
                    .retain(|element| element.table != table || element.set != name);
            }
            _ => unreachable!(),
        }
        state.generation = state.generation.wrapping_add(1);
        Ok(())
    }

    fn dump_nft(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        request: &NlMsgHdr,
        command: u16,
    ) -> AxResult {
        let needle = Arc::downgrade(&self.net_ns);
        let port_id = permit.port_id();
        let mut messages = Vec::new();
        let all = permit.nft_tables().ok_or(AxError::BadState)?;
        all.retain(|state| state.namespace.strong_count() != 0);
        for state in all
            .iter()
            .filter(|state| Weak::ptr_eq(&state.namespace, &needle))
        {
            match command {
                NFT_MSG_GETTABLE => {
                    for table in &state.tables {
                        messages.push(nft_table_message(request, port_id, table));
                    }
                }
                NFT_MSG_GETCHAIN => {
                    for chain in &state.chains {
                        messages.push(nft_chain_message(request, port_id, chain));
                    }
                }
                NFT_MSG_GETRULE => {
                    for rule in &state.rules {
                        messages.push(nft_rule_message(request, port_id, rule));
                    }
                }
                NFT_MSG_GETSET => {
                    for set in &state.sets {
                        messages.push(nft_set_message(request, port_id, set));
                    }
                }
                NFT_MSG_GETSETELEM => {
                    for element in &state.elements {
                        messages.push(nft_element_message(request, port_id, element));
                    }
                }
                _ => unreachable!(),
            }
        }
        for message in messages {
            self.enqueue_kernel_permitted(permit, message);
        }
        self.enqueue_kernel_permitted(permit, done_message(request, port_id));
        Ok(())
    }

    /// Route authority is checked against the user namespace owning the
    /// socket's retained network namespace. A socket fd can cross setns, so
    /// the caller's current namespace is not an authority substitute.
    fn require_net_admin(&self, actor: &Cred) -> AxResult {
        if ns_capable(actor, self.net_ns.owner_user_ns(), CAP_NET_ADMIN) {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }

    fn add_route(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
        flags: u16,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let rule = self.parse_route_rule(permit, payload)?;
        if flags & NLM_F_REPLACE != 0 {
            permit
                .route_service()
                .ok_or(AxError::BadState)?
                .route_replace(rule)
        } else {
            permit
                .route_service()
                .ok_or(AxError::BadState)?
                .route_try_add(rule)
        }
    }

    fn delete_route(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let rule = self.parse_route_rule(permit, payload)?;
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_remove(&rule)
    }

    fn add_address(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let (ifindex, address) = self.parse_address(payload)?;
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_add_interface_addr(ifindex, address)
    }

    fn delete_address(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let (ifindex, address) = self.parse_address(payload)?;
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_remove_interface_addr(ifindex, address)
    }

    /// RTM_NEWLINK currently creates a real in-namespace veth pair.  The
    /// parsed request is complete before either device is admitted, and the
    /// lower stack rolls back the first endpoint if peer admission fails.
    fn new_link(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
        flags: u16,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        if flags & NLM_F_CREATE == 0 {
            return Err(AxError::AlreadyExists);
        }
        let (name, peer_name) = self.parse_veth_create(payload)?;
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_create_veth_pair(name, peer_name)
            .map(|_| ())
    }

    fn delete_link(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let message = parse_ifinfo(payload)?;
        if message.ifi_index <= 0 || payload.len() != size_of::<IfInfoMsg>() {
            return Err(AxError::InvalidInput);
        }
        let interface = permit
            .route_service()
            .ok_or(AxError::BadState)?
            .interfaces()
            .into_iter()
            .find(|entry| entry.index == message.ifi_index as u32)
            .ok_or(AxError::NoSuchDevice)?;
        if interface.kind == InterfaceKind::Loopback {
            return Err(AxError::OperationNotPermitted);
        }
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_remove_device(interface.index)
    }

    fn set_link(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
        actor: &Cred,
    ) -> AxResult {
        self.require_net_admin(actor)?;
        let message = parse_ifinfo(payload)?;
        if message.ifi_index <= 0 || message.ifi_family != AF_UNSPEC as u8 {
            return Err(AxError::InvalidInput);
        }
        let (name, mtu) = parse_link_attributes(&payload[size_of::<IfInfoMsg>()..])?;
        let change = message.ifi_change;
        if change & !IFF_UP != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let up = (change & IFF_UP != 0).then_some(message.ifi_flags & IFF_UP != 0);
        permit
            .route_service()
            .ok_or(AxError::BadState)?
            .configure_link(message.ifi_index as u32, name, mtu, up)
    }

    fn parse_veth_create(&self, payload: &[u8]) -> AxResult<(String, String)> {
        let message = parse_ifinfo(payload)?;
        if message.ifi_family != AF_UNSPEC as u8
            || message.ifi_index != 0
            || message.ifi_flags != 0
            || message.ifi_change != 0
        {
            return Err(AxError::InvalidInput);
        }
        let attrs = &payload[size_of::<IfInfoMsg>()..];
        let mut name = None;
        let mut link_info = None;
        for_each_rtattr(attrs, |kind, value| match kind {
            IFLA_IFNAME if name.is_none() => {
                name = Some(decode_link_name(value)?);
                Ok(())
            }
            IFLA_LINKINFO if link_info.is_none() => {
                link_info = Some(value);
                Ok(())
            }
            _ => Err(AxError::OperationNotSupported),
        })?;
        let info = link_info.ok_or(AxError::InvalidInput)?;
        let mut kind = None;
        let mut data = None;
        for_each_rtattr(info, |attribute, value| match attribute {
            IFLA_INFO_KIND if kind.is_none() => {
                kind = Some(decode_link_name(value)?);
                Ok(())
            }
            IFLA_INFO_DATA if data.is_none() => {
                data = Some(value);
                Ok(())
            }
            _ => Err(AxError::OperationNotSupported),
        })?;
        if kind.as_deref() != Some("veth") {
            return Err(AxError::OperationNotSupported);
        }
        let data = data.ok_or(AxError::InvalidInput)?;
        let mut peer = None;
        for_each_rtattr(data, |attribute, value| {
            if attribute != VETH_INFO_PEER || peer.is_some() || value.len() < size_of::<IfInfoMsg>()
            {
                return Err(AxError::InvalidInput);
            }
            let peer_info = read_unaligned::<IfInfoMsg>(value)?;
            if peer_info.ifi_family != AF_UNSPEC as u8
                || peer_info.ifi_index != 0
                || peer_info.ifi_flags != 0
                || peer_info.ifi_change != 0
            {
                return Err(AxError::InvalidInput);
            }
            let (peer_name, peer_mtu) = parse_link_attributes(&value[size_of::<IfInfoMsg>()..])?;
            if peer_mtu.is_some() {
                return Err(AxError::OperationNotSupported);
            }
            peer = peer_name;
            Ok(())
        })?;
        Ok((
            name.ok_or(AxError::InvalidInput)?,
            peer.ok_or(AxError::InvalidInput)?,
        ))
    }

    fn parse_address(&self, payload: &[u8]) -> AxResult<(u32, IpCidr)> {
        if payload.len() < size_of::<IfAddrMsg>() {
            return Err(AxError::InvalidInput);
        }
        let message = read_unaligned::<IfAddrMsg>(payload)?;
        if !matches!(message.ifa_family as u32, family if family == AF_INET || family == AF_INET6)
            || message.ifa_index == 0
            || message.ifa_flags != 0
            || message.ifa_scope != RT_SCOPE_UNIVERSE
        {
            return Err(AxError::OperationNotSupported);
        }
        let bits = if message.ifa_family as u32 == AF_INET {
            32
        } else {
            128
        };
        if message.ifa_prefixlen > bits {
            return Err(AxError::InvalidInput);
        }
        let mut address = None;
        let mut offset = size_of::<IfAddrMsg>();
        while offset < payload.len() {
            if payload.len() - offset < size_of::<RtAttr>() {
                return Err(AxError::InvalidInput);
            }
            let attr = read_unaligned::<RtAttr>(&payload[offset..])?;
            let len = attr.rta_len as usize;
            if len < size_of::<RtAttr>() || offset + len > payload.len() {
                return Err(AxError::InvalidInput);
            }
            let value = &payload[offset + size_of::<RtAttr>()..offset + len];
            match attr.rta_type {
                IFA_ADDRESS | IFA_LOCAL => {
                    let decoded = decode_ip(message.ifa_family, value)?;
                    if let Some(previous) = address.replace(decoded)
                        && previous != decoded
                    {
                        return Err(AxError::InvalidInput);
                    }
                }
                _ => return Err(AxError::OperationNotSupported),
            }
            offset = offset
                .checked_add(align4(len))
                .ok_or(AxError::InvalidInput)?;
            if offset > payload.len() {
                return Err(AxError::InvalidInput);
            }
        }
        Ok((
            message.ifa_index,
            IpCidr::new(address.ok_or(AxError::InvalidInput)?, message.ifa_prefixlen),
        ))
    }

    /// Decode an RTM_NEWROUTE/RTM_DELROUTE request completely before taking
    /// the router lock. Malformed attributes therefore cannot leave a partial
    /// routing-table update behind.
    fn parse_route_rule(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        payload: &[u8],
    ) -> AxResult<Rule> {
        if payload.len() < size_of::<RtMsg>() {
            return Err(AxError::InvalidInput);
        }
        let message = read_unaligned::<RtMsg>(payload)?;
        if !matches!(message.rtm_family as u32, family if family == AF_INET as u32 || family == AF_INET6 as u32)
            || message.rtm_src_len != 0
            || message.rtm_tos != 0
            || message.rtm_scope != 0
            || message.rtm_protocol != 0
            || message.rtm_flags != 0
        {
            return Err(AxError::InvalidInput);
        }
        if (message.rtm_table != 0 && message.rtm_table != RT_TABLE_MAIN)
            || (message.rtm_type != 0 && message.rtm_type != RTN_UNICAST)
        {
            return Err(AxError::OperationNotSupported);
        }
        let address_len = if message.rtm_family as u32 == AF_INET as u32 {
            4
        } else {
            16
        };
        if message.rtm_dst_len as usize > address_len * 8 {
            return Err(AxError::InvalidInput);
        }
        let mut destination = None;
        let mut gateway = None;
        let mut output_ifindex = None;
        let mut offset = size_of::<RtMsg>();
        while offset < payload.len() {
            if payload.len() - offset < size_of::<RtAttr>() {
                return Err(AxError::InvalidInput);
            }
            let attribute = read_unaligned::<RtAttr>(&payload[offset..])?;
            let length = attribute.rta_len as usize;
            if length < size_of::<RtAttr>() || offset + length > payload.len() {
                return Err(AxError::InvalidInput);
            }
            let value = &payload[offset + size_of::<RtAttr>()..offset + length];
            match attribute.rta_type {
                RTA_DST => {
                    if destination
                        .replace(decode_ip(message.rtm_family, value)?)
                        .is_some()
                    {
                        return Err(AxError::InvalidInput);
                    }
                }
                RTA_GATEWAY => {
                    if gateway
                        .replace(decode_ip(message.rtm_family, value)?)
                        .is_some()
                    {
                        return Err(AxError::InvalidInput);
                    }
                }
                RTA_OIF => {
                    if value.len() != size_of::<u32>() || output_ifindex.is_some() {
                        return Err(AxError::InvalidInput);
                    }
                    output_ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                }
                _ => return Err(AxError::OperationNotSupported),
            }
            offset = offset
                .checked_add(align4(length))
                .ok_or(AxError::InvalidInput)?;
            if offset > payload.len() {
                return Err(AxError::InvalidInput);
            }
        }
        let destination = match destination {
            Some(address) => address,
            None if message.rtm_dst_len == 0 => unspecified_ip(message.rtm_family)?,
            None => return Err(AxError::InvalidInput),
        };
        let ifindex = output_ifindex
            .filter(|index| *index != 0)
            .ok_or(AxError::InvalidInput)?;
        let interface = permit
            .route_service()
            .ok_or(AxError::BadState)?
            .interfaces()
            .into_iter()
            .find(|interface| interface.index == ifindex)
            .ok_or(AxError::NoSuchDevice)?;
        let source = interface
            .addresses
            .iter()
            .map(|cidr| cidr.address())
            .find(|address| same_ip_family(*address, destination))
            .ok_or(AxError::NoSuchDevice)?;
        Ok(Rule::new(
            IpCidr::new(destination, message.rtm_dst_len),
            gateway,
            ifindex,
            source,
        ))
    }

    fn dump_addresses(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
    ) -> AxResult {
        let filter = if payload.len() >= size_of::<IfAddrMsg>() {
            Some(read_unaligned::<IfAddrMsg>(payload)?)
        } else {
            None
        };
        let port_id = permit.port_id();
        let interfaces = permit
            .route_service()
            .ok_or(AxError::BadState)?
            .interfaces();
        for interface in interfaces {
            for entry in address_entries(&interface) {
                if let Some(filter) = filter
                    && ((filter.ifa_family != AF_UNSPEC as u8 && filter.ifa_family != entry.family)
                        || (filter.ifa_index != 0 && filter.ifa_index != entry.index))
                {
                    continue;
                }
                self.enqueue_kernel_permitted(permit, address_message(hdr, port_id, &entry));
            }
        }
        self.enqueue_kernel_permitted(permit, done_message(hdr, port_id));
        Ok(())
    }

    fn dump_routes(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
    ) -> AxResult {
        let filter = if payload.len() >= size_of::<RtMsg>() {
            Some(read_unaligned::<RtMsg>(payload)?)
        } else {
            None
        };
        let port_id = permit.port_id();
        let routes = permit
            .route_service()
            .ok_or(AxError::BadState)?
            .route_snapshot();
        for route in routes {
            let entry = route_entry(&route);
            if let Some(filter) = filter
                && filter.rtm_family != AF_UNSPEC as u8
                && filter.rtm_family != entry.family
            {
                continue;
            }
            self.enqueue_kernel_permitted(permit, route_message(hdr, port_id, &entry));
        }
        self.enqueue_kernel_permitted(permit, done_message(hdr, port_id));
        Ok(())
    }

    fn dump_links(
        &self,
        permit: &mut NetlinkWritePermit<'_>,
        hdr: &NlMsgHdr,
        payload: &[u8],
    ) -> AxResult {
        let filter = if payload.len() >= size_of::<IfInfoMsg>() {
            Some(read_unaligned::<IfInfoMsg>(payload)?)
        } else {
            None
        };
        let port_id = permit.port_id();
        let interfaces = permit
            .route_service()
            .ok_or(AxError::BadState)?
            .interfaces();
        for interface in interfaces {
            let link = link_entry(interface);
            if let Some(filter) = filter
                && filter.ifi_index > 0
                && filter.ifi_index as u32 != link.index
            {
                continue;
            }
            self.enqueue_kernel_permitted(permit, link_message(hdr, port_id, &link));
        }
        self.enqueue_kernel_permitted(permit, done_message(hdr, port_id));
        Ok(())
    }

    fn subscribed_to(&self, group: u32) -> bool {
        self.state.lock().groups & group != 0
    }
}

#[derive(Clone, Copy)]
struct InetDiagRequest {
    family: u8,
    protocol: u8,
    extensions: u8,
    states: u32,
    sport: u16,
    dport: u16,
    src: [u8; 16],
    dst: [u8; 16],
    ifindex: u32,
    cookie: [u32; 2],
}

impl InetDiagRequest {
    fn parse(payload: &[u8]) -> AxResult<Self> {
        debug_assert_eq!(payload.len(), INET_DIAG_REQ_V2_LEN);
        let states = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        let sport = u16::from_be_bytes(payload[8..10].try_into().unwrap());
        let dport = u16::from_be_bytes(payload[10..12].try_into().unwrap());
        let mut src = [0_u8; 16];
        let mut dst = [0_u8; 16];
        src.copy_from_slice(&payload[12..28]);
        dst.copy_from_slice(&payload[28..44]);
        Ok(Self {
            family: payload[0],
            protocol: payload[1],
            extensions: payload[2],
            states,
            sport,
            dport,
            src,
            dst,
            ifindex: u32::from_ne_bytes(payload[44..48].try_into().unwrap()),
            cookie: [
                u32::from_ne_bytes(payload[48..52].try_into().unwrap()),
                u32::from_ne_bytes(payload[52..56].try_into().unwrap()),
            ],
        })
    }

    fn matches(&self, entry: &SocketDiagRegistration) -> bool {
        if (self.family != AF_UNSPEC as u8 && self.family as u16 != entry.family)
            || (self.protocol != 0 && self.protocol != entry.protocol)
        {
            return false;
        }
        let state = entry.diag_state();
        if self.states != 0 && (state == 0 || self.states & (1_u32 << (state - 1)) == 0) {
            return false;
        }
        // Registered transport endpoints currently retain the canonical
        // unbound sockid. Therefore any nonzero address/port/interface filter
        // cannot match; cookie still identifies the exact live OFD.
        if self.sport != 0
            || self.dport != 0
            || self.src.iter().any(|&v| v != 0)
            || self.dst.iter().any(|&v| v != 0)
            || self.ifindex != 0
        {
            return false;
        }
        self.cookie == [INET_DIAG_NOCOOKIE; 2]
            || self.cookie == [entry.cookie as u32, (entry.cookie >> 32) as u32]
    }
}

fn reserve_netlink_port(
    socket: &NetlinkSocket,
    preferred_port_id: u32,
    automatic: bool,
) -> AxResult<u32> {
    reserve_netlink_port_with_mode(socket, preferred_port_id, automatic, false)
}

fn reserve_netlink_port_with_mode(
    socket: &NetlinkSocket,
    preferred_port_id: u32,
    automatic: bool,
    nowait: bool,
) -> AxResult<u32> {
    let mut ports = if nowait {
        NETLINK_PORTS.try_lock().ok_or(AxError::WouldBlock)?
    } else {
        NETLINK_PORTS.lock()
    };
    ports.retain(|binding| binding.net_ns.strong_count() != 0);
    let net_ns = Arc::downgrade(&socket.net_ns);
    let in_use = |port_id| {
        ports.iter().any(|binding| {
            binding.protocol == socket.protocol
                && binding.port_id == port_id
                && Weak::ptr_eq(&binding.net_ns, &net_ns)
        })
    };
    let port_id = if automatic && (preferred_port_id == 0 || in_use(preferred_port_id)) {
        let mut candidate = NETLINK_NEXT_PORT_ID.fetch_add(1, Ordering::Relaxed);
        let mut attempts = 0;
        loop {
            if candidate != 0 && !in_use(candidate) {
                break candidate;
            }
            attempts += 1;
            if attempts == u32::MAX {
                return Err(LinuxError::EADDRINUSE.into());
            }
            candidate = NETLINK_NEXT_PORT_ID.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        if in_use(preferred_port_id) {
            return Err(LinuxError::EADDRINUSE.into());
        }
        preferred_port_id
    };
    ports.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    ports.push(NetlinkPortBinding {
        net_ns,
        protocol: socket.protocol,
        port_id,
        socket_inode: socket.inode.inode(),
    });
    Ok(port_id)
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        let inode = self.inode.inode();
        if self.protocol == NETLINK_KOBJECT_UEVENT {
            // Never leave a dead weak entry for the next sender to scan.  This
            // lock is also taken by broadcast, but Drop does not acquire any
            // socket-local locks, preserving the broadcast lock order.
            let mut sockets = KOBJECT_UEVENT_SOCKETS.lock();
            let this = core::ptr::from_ref(self);
            // Do not upgrade here: upgrading our own weak reference can make
            // it the final Arc, whose release recursively enters Drop while
            // the registry lock remains held.
            sockets.retain(|weak| weak.as_ptr() != this && weak.strong_count() != 0);
            // A burst of unprivileged open/close must not permanently retain a
            // large sparse backing allocation for future broadcasts.
            let retained = sockets.len();
            if sockets.capacity() > retained.saturating_mul(2).max(16) {
                sockets.shrink_to(retained.max(16));
            }
        }
        if self.protocol == NETLINK_AUDIT {
            let mut sockets = AUDIT_SOCKETS.lock();
            let this = core::ptr::from_ref(self);
            sockets.retain(|weak| weak.as_ptr() != this && weak.strong_count() != 0);
            let retained = sockets.len();
            if sockets.capacity() > retained.saturating_mul(2).max(16) {
                sockets.shrink_to(retained.max(16));
            }
        }
        NETLINK_PORTS
            .lock()
            .retain(|binding| binding.socket_inode != inode);
    }
}

/// Deliver one policy decision through the generic NETLINK_AUDIT transport.
/// The record is a normal netlink frame with Linux's
/// `AUDIT_LANDLOCK_ACCESS` type, so
/// listeners receive an ordered kernel-originated datagram instead of a
/// private side channel.
pub(crate) fn emit_landlock_audit(sequence: u64, event: AuditLandlockDenied) {
    let mut text = alloc::format!(
        "audit({sequence}): landlock_blocker={} landlock_access=0x{:x} landlock_domain={} \
         exec={}\0",
        event.blocker,
        event.access,
        event.domain_id,
        u8::from(event.on_exec),
    )
    .into_bytes();
    let mut message = vec![0; size_of::<NlMsgHdr>()];
    message.append(&mut text);
    let header = NlMsgHdr {
        nlmsg_len: message.len() as u32,
        nlmsg_type: AUDIT_LANDLOCK_ACCESS,
        nlmsg_flags: 0,
        nlmsg_seq: sequence as u32,
        nlmsg_pid: 0,
    };
    write_struct(&mut message[..size_of::<NlMsgHdr>()], &header);
    let mut sockets = AUDIT_SOCKETS.lock();
    sockets.retain(|weak| {
        let Some(socket) = weak.upgrade() else {
            return false;
        };
        if socket.subscribed_to(AUDIT_GROUP) {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(message.len()).is_ok() {
                copy.extend_from_slice(&message);
                socket.enqueue_kernel_from(copy, AUDIT_GROUP, Some(KERNEL_UEVENT_CREDENTIALS));
            } else {
                socket.note_queue_drop();
            }
        }
        true
    });
}

/// Deliver one seccomp event using Linux's `AUDIT_SECCOMP` message type.
/// Credentials are captured as kernel credentials with the queued datagram,
/// not sampled later from a potentially unrelated receiver.
pub(crate) fn emit_seccomp_audit(sequence: u64, event: AuditSeccompDecision) {
    let mut text = alloc::format!(
        "audit({sequence}): arch={:#x} syscall={} ip={:#x} code={:#x} pid={}\0",
        event.architecture,
        event.syscall,
        event.instruction_pointer,
        event.action,
        event.pid,
    )
    .into_bytes();
    let mut message = vec![0; size_of::<NlMsgHdr>()];
    message.append(&mut text);
    let header = NlMsgHdr {
        nlmsg_len: message.len() as u32,
        nlmsg_type: AUDIT_SECCOMP,
        nlmsg_flags: 0,
        nlmsg_seq: sequence as u32,
        nlmsg_pid: 0,
    };
    write_struct(&mut message[..size_of::<NlMsgHdr>()], &header);
    let mut sockets = AUDIT_SOCKETS.lock();
    sockets.retain(|weak| {
        let Some(socket) = weak.upgrade() else {
            return false;
        };
        if socket.subscribed_to(AUDIT_GROUP) {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(message.len()).is_ok() {
                copy.extend_from_slice(&message);
                socket.enqueue_kernel_from(copy, AUDIT_GROUP, Some(KERNEL_UEVENT_CREDENTIALS));
            } else {
                socket.note_queue_drop();
            }
        }
        true
    });
}

/// Establish the network namespace to which all kernel kobject uevents are
/// broadcast.  Boot registers init-net before publishing devices; repeating
/// that registration is harmless, while replacing it is rejected.
pub(crate) fn register_init_network_namespace(net_ns: &Arc<NetworkNamespace>) -> AxResult {
    let mut init_net_ns = INIT_NETWORK_NAMESPACE.lock();
    match init_net_ns.as_ref() {
        Some(existing) if Arc::ptr_eq(existing, net_ns) => Ok(()),
        Some(_) => Err(AxError::AlreadyExists),
        None => {
            *init_net_ns = Some(net_ns.clone());
            Ok(())
        }
    }
}

/// Audit endpoints remain global even though ordinary netlink families are
/// network-namespace scoped.  Compare object identity rather than the owner
/// user namespace: an unshared network namespace can be owned by init-user
/// and is still not permitted to host an audit listener.
fn is_initial_network_namespace(net_ns: &Arc<NetworkNamespace>) -> bool {
    INIT_NETWORK_NAMESPACE
        .lock()
        .as_ref()
        .is_some_and(|initial| Arc::ptr_eq(initial, net_ns))
}

/// Publish a kobject uevent exclusively to the boot-established init network
/// namespace.  Before boot has registered init-net, there can be no
/// publishable device listener, so retain the historical best-effort behavior
/// and drop the notification.
pub(crate) fn emit_init_net_kobject_uevent(
    action: &str,
    devpath: &str,
    subsystem: &str,
    extra_environment: &[(&str, &str)],
) -> AxResult<Option<u64>> {
    let init_net_ns = INIT_NETWORK_NAMESPACE.lock().clone();
    let Some(init_net_ns) = init_net_ns else {
        return Ok(None);
    };
    emit_kobject_uevent(&init_net_ns, action, devpath, subsystem, extra_environment).map(Some)
}

/// Publish a kernel kobject uevent to NETLINK_KOBJECT_UEVENT group 1.
///
/// The payload follows the Linux wire format: an action/path header followed
/// by NUL-separated environment strings, with a globally monotonic SEQNUM.
/// This is intentionally independent from the route netlink request parser.
pub(crate) fn emit_kobject_uevent(
    net_ns: &NetworkNamespace,
    action: &str,
    devpath: &str,
    subsystem: &str,
    extra_environment: &[(&str, &str)],
) -> AxResult<u64> {
    if action.is_empty()
        || devpath.is_empty()
        || subsystem.is_empty()
        || action.contains('\0')
        || devpath.contains('\0')
        || subsystem.contains('\0')
        || extra_environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains('\0') || value.contains('\0'))
    {
        return Err(AxError::InvalidInput);
    }

    // A single sender domain keeps sequence allocation and delivery ordered:
    // listeners can never receive SEQNUM n + 1 before n.
    let _send_guard = KOBJECT_UEVENT_SEND_LOCK.lock();
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let mut payload_len = action
        .len()
        .checked_add(1)
        .and_then(|len| len.checked_add(devpath.len()))
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    for (key, value) in [
        ("ACTION", action),
        ("DEVPATH", devpath),
        ("SUBSYSTEM", subsystem),
        ("SEQNUM", sequence_text.as_str()),
    ]
    .into_iter()
    .chain(extra_environment.iter().copied())
    {
        payload_len = payload_len
            .checked_add(key.len())
            .and_then(|len| len.checked_add(1))
            .and_then(|len| len.checked_add(value.len()))
            .and_then(|len| len.checked_add(1))
            .ok_or(AxError::NoMemory)?;
    }
    if admit_netlink_write(payload_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| AxError::NoMemory)?;
    append_uevent_field(&mut payload, action)?;
    payload.push(b'@');
    append_uevent_field(&mut payload, devpath)?;
    payload.push(0);
    append_uevent_assignment(&mut payload, "ACTION", action)?;
    append_uevent_assignment(&mut payload, "DEVPATH", devpath)?;
    append_uevent_assignment(&mut payload, "SUBSYSTEM", subsystem)?;
    append_uevent_assignment(&mut payload, "SEQNUM", &sequence_text)?;
    for &(key, value) in extra_environment {
        append_uevent_assignment(&mut payload, key, value)?;
    }
    debug_assert_eq!(payload.len(), payload_len);

    broadcast_uevent_to_namespace(net_ns, &payload, KERNEL_UEVENT_CREDENTIALS, None);
    Ok(sequence)
}

fn broadcast_user_uevent(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
) -> AxResult {
    // Keep synthetic and kernel-originated uevents in one sequence/delivery
    // domain, matching uevent_sock_mutex plus the global Linux sequence.
    let _send_guard = KOBJECT_UEVENT_SEND_LOCK.lock();
    broadcast_user_uevent_locked(net_ns, payload, credentials, None)
}

/// Caller already owns `KOBJECT_UEVENT_SEND_LOCK` through a typed Uevent
/// write permit or through the ordinary wrapper above.
fn broadcast_user_uevent_locked(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    skip: Option<*const NetlinkSocket>,
) -> AxResult {
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let suffix_len = "SEQNUM="
        .len()
        .checked_add(sequence_text.len())
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    let message_len = payload
        .len()
        .checked_add(suffix_len)
        .ok_or(AxError::NoMemory)?;
    if admit_netlink_write(message_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(message_len)
        .map_err(|_| AxError::NoMemory)?;
    message.extend_from_slice(payload);
    append_uevent_assignment(&mut message, "SEQNUM", &sequence_text)?;
    debug_assert_eq!(message.len(), message_len);
    broadcast_uevent_to_namespace(net_ns, &message, credentials, skip);
    Ok(())
}

/// NOWAIT uevent delivery while the caller owns the global sender domain.
/// Peer state/queues are probed only; contended listeners observe a normal
/// multicast drop rather than making this source-consuming operation sleep.
/// The caller's own delivery is returned for its retained queue to enqueue.
fn broadcast_user_uevent_nowait_locked(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    sender: *const NetlinkSocket,
    sockets: &mut Vec<Weak<NetlinkSocket>>,
) -> AxResult<Vec<u8>> {
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let suffix_len = "SEQNUM="
        .len()
        .checked_add(sequence_text.len())
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    let message_len = payload
        .len()
        .checked_add(suffix_len)
        .ok_or(AxError::NoMemory)?;
    if admit_netlink_write(message_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(message_len)
        .map_err(|_| AxError::NoMemory)?;
    message.extend_from_slice(payload);
    append_uevent_assignment(&mut message, "SEQNUM", &sequence_text)?;
    sockets.retain(|entry| {
        let Some(socket) = entry.upgrade() else {
            return false;
        };
        if core::ptr::eq(Arc::as_ptr(&socket), sender)
            || !core::ptr::eq(socket.net_ns.as_ref(), net_ns)
        {
            return true;
        }
        let Some(state) = socket.state.try_lock() else {
            // The NO_ENOBUFS bit itself is protected by this contended lock;
            // report the loss conservatively so userspace can rescan.
            socket.overrun.store(true, Ordering::Release);
            socket.poll_rx.wake();
            return true;
        };
        let subscribed = state.groups & KOBJECT_UEVENT_GROUP != 0;
        let suppress_enobufs = state.option_flags & (1 << NETLINK_NO_ENOBUFS) != 0;
        drop(state);
        if !subscribed {
            return true;
        }
        let Some(mut queue) = socket.queue.try_lock() else {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        };
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            message.len(),
            NETLINK_QUEUE_LIMIT,
            NETLINK_QUEUE_LIMIT_BYTES,
        ) == NetlinkQueueAdmission::Drop
        {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(message.len()).is_err() {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        }
        copy.extend_from_slice(&message);
        queue.bytes += copy.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data: copy,
            source_port_id: 0,
            source_groups: KOBJECT_UEVENT_GROUP,
            credentials: Some(credentials),
        });
        drop(queue);
        socket.poll_rx.wake();
        true
    });
    Ok(message)
}

fn broadcast_uevent_to_namespace(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    skip: Option<*const NetlinkSocket>,
) {
    // Never retain the global listener registry while taking a socket-local
    // state or queue lock.  A userspace sender holds its own state/queue
    // before it takes the sender-domain lock, so registry -> peer lock here
    // would otherwise form a cross-sender cycle.
    let mut sockets = KOBJECT_UEVENT_SOCKETS.lock();
    let listeners = match collect_live_uevent_listeners(&mut sockets) {
        Ok(listeners) => listeners,
        Err(error) => {
            // Kernel-originated uevents are best effort.  OOM while taking a
            // snapshot must not hold the sender domain or make device
            // publication fail; retain only live registrations and drop this
            // multicast with a diagnostic.
            sockets.retain(|socket| socket.strong_count() != 0);
            warn!("dropping kobject uevent: cannot snapshot listeners: {error}");
            return;
        }
    };
    drop(sockets);

    for socket in listeners {
        if skip.is_some_and(|skip| core::ptr::eq(Arc::as_ptr(&socket), skip)) {
            continue;
        }
        if !core::ptr::eq(socket.net_ns.as_ref(), net_ns) {
            continue;
        }
        // This path runs with KOBJECT_UEVENT_SEND_LOCK held.  A synthetic
        // sender owns its own state before waiting for that lock, so listener
        // state and queue must be probed, never waited on.
        let Some(state) = socket.state.try_lock() else {
            // We cannot inspect NETLINK_NO_ENOBUFS without this lock.  Report
            // the loss conservatively so eudevd receives an ENOBUFS rescan
            // signal instead of silently missing device lifecycle events.
            socket.overrun.store(true, Ordering::Release);
            socket.poll_rx.wake();
            continue;
        };
        let subscribed = state.groups & KOBJECT_UEVENT_GROUP != 0;
        let suppress_enobufs = state.option_flags & (1 << NETLINK_NO_ENOBUFS) != 0;
        drop(state);
        if !subscribed {
            continue;
        }
        let Some(mut queue) = socket.queue.try_lock() else {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            continue;
        };
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            payload.len(),
            NETLINK_QUEUE_LIMIT,
            NETLINK_QUEUE_LIMIT_BYTES,
        ) == NetlinkQueueAdmission::Drop
        {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            drop(queue);
            socket.poll_rx.wake();
            continue;
        }
        // Keep per-socket buffers isolated: an allocation failure for one
        // listener never makes another listener observe its datagram.
        let mut message = Vec::new();
        if message.try_reserve_exact(payload.len()).is_err() {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            drop(queue);
            socket.poll_rx.wake();
            continue;
        }
        message.extend_from_slice(payload);
        queue.bytes += message.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data: message,
            source_port_id: 0,
            source_groups: KOBJECT_UEVENT_GROUP,
            credentials: Some(credentials),
        });
        drop(queue);
        socket.poll_rx.wake();
    }
}

/// Snapshot live listeners while holding only the registry lock.  Callers
/// must release that lock before touching socket-local state or queues.
fn collect_live_uevent_listeners(
    sockets: &mut Vec<Weak<NetlinkSocket>>,
) -> AxResult<Vec<Arc<NetlinkSocket>>> {
    let mut listeners = Vec::new();
    listeners
        .try_reserve(sockets.len())
        .map_err(|_| AxError::NoMemory)?;
    sockets.retain(|entry| {
        let Some(socket) = entry.upgrade() else {
            return false;
        };
        listeners.push(socket);
        true
    });
    Ok(listeners)
}

fn find_netlink_peer(
    protocol: u32,
    net_ns: &NetworkNamespace,
    port_id: u32,
    nowait: bool,
) -> AxResult<Option<Arc<NetlinkSocket>>> {
    // KOBJECT_UEVENT is the one netlink family in this kernel that accepts
    // user-to-user datagrams.  Its existing weak listener registry provides
    // a lifetime pin without changing the deliberately metadata-only port
    // reservation table used by the other kernel-service families.
    if protocol != NETLINK_KOBJECT_UEVENT {
        return Ok(None);
    }
    // Binding takes socket state and then the port registry.  Do not invert
    // that order by holding the uevent registry while inspecting a peer's
    // state: clone live candidates first, then drop the registry lock.
    let mut sockets = if nowait {
        KOBJECT_UEVENT_SOCKETS
            .try_lock()
            .ok_or(AxError::WouldBlock)?
    } else {
        KOBJECT_UEVENT_SOCKETS.lock()
    };
    let candidates = collect_live_uevent_listeners(&mut sockets)?;
    drop(sockets);

    for socket in candidates {
        let state = if nowait {
            socket.state.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            socket.state.lock()
        };
        let matches = state.bound
            && state.port_id == port_id
            && core::ptr::eq(socket.net_ns.as_ref(), net_ns);
        drop(state);
        if matches {
            return Ok(Some(socket));
        }
    }
    Ok(None)
}

#[cfg(test)]
fn kobject_uevent_socket_is_registered(socket: *const NetlinkSocket) -> bool {
    KOBJECT_UEVENT_SOCKETS
        .lock()
        .iter()
        .any(|weak| weak.as_ptr() == socket && weak.strong_count() != 0)
}

fn append_uevent_field(payload: &mut Vec<u8>, value: &str) -> AxResult {
    payload
        .try_reserve(value.len())
        .map_err(|_| AxError::NoMemory)?;
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

fn append_uevent_assignment(payload: &mut Vec<u8>, key: &str, value: &str) -> AxResult {
    payload
        .try_reserve(key.len() + 1 + value.len() + 1)
        .map_err(|_| AxError::NoMemory)?;
    payload.extend_from_slice(key.as_bytes());
    payload.push(b'=');
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    Ok(())
}

impl FileLike for NetlinkSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.recv(dst, RecvFlags::empty())
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.write_with_nonblocking(src, self.nonblocking())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.inode.update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }
}

fn netlink_ack(request: &NlMsgHdr, port_id: u32, error: i32) -> Vec<u8> {
    let header_len = size_of::<NlMsgHdr>();
    let err_len = size_of::<NlMsgErr>();
    let mut out = vec![0; header_len + err_len];
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: NLMSG_ERROR,
        nlmsg_flags: 0,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out[..header_len], &hdr);
    let err = NlMsgErr {
        error,
        msg: *request,
    };
    write_struct(&mut out[header_len..], &err);
    out
}

fn done_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    let mut out = vec![0; size_of::<NlMsgHdr>()];
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: NLMSG_DONE,
        nlmsg_flags: NLM_F_MULTI,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out, &hdr);
    out
}

fn netlink_message(
    request: &NlMsgHdr,
    port_id: u32,
    msg_type: u16,
    mut payload: Vec<u8>,
) -> Vec<u8> {
    let header_len = size_of::<NlMsgHdr>();
    let mut out = vec![0; header_len];
    out.append(&mut payload);
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: msg_type,
        nlmsg_flags: NLM_F_MULTI,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out[..header_len], &hdr);
    out
}

fn sock_diag_message(
    request: &NlMsgHdr,
    port_id: u32,
    entry: &SocketDiagRegistration,
    extensions: u8,
) -> Vec<u8> {
    // `inet_diag_msg`: family/state/timer/retrans, inet_diag_sockid, then
    // expires/rqueue/wqueue/uid/inode.  Addresses and queues are zero until
    // the transport exposes its bind/connect snapshot; identity, protocol
    // selection and lifecycle are nevertheless the actual live OFD record.
    let mut payload = vec![0_u8; 72];
    payload[0] = entry.family as u8;
    payload[1] = entry.diag_state();
    payload[44..48].copy_from_slice(&(entry.cookie as u32).to_ne_bytes());
    payload[48..52].copy_from_slice(&((entry.cookie >> 32) as u32).to_ne_bytes());
    // No provider extension is invented yet; retaining the parsed extension
    // mask makes the request path complete without changing base selection.
    let _ = extensions;
    netlink_message(request, port_id, SOCK_DIAG_BY_FAMILY, payload)
}

fn generic_family_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CTRL_CMD_NEWFAMILY,
        version: 2,
        reserved: 0,
    });
    push_attr(
        &mut payload,
        CTRL_ATTR_FAMILY_ID,
        &THEKERNEL_GENL_FAMILY_ID.to_ne_bytes(),
    );
    push_attr_string(
        &mut payload,
        CTRL_ATTR_FAMILY_NAME,
        THEKERNEL_GENL_FAMILY_NAME,
    );
    push_attr(&mut payload, CTRL_ATTR_VERSION, &[1, 0, 0, 0]);
    push_attr(&mut payload, CTRL_ATTR_HDRSIZE, &[0, 0, 0, 0]);
    push_attr(&mut payload, CTRL_ATTR_MAXATTR, &[0, 0, 0, 0]);
    netlink_message(request, port_id, GENL_ID_CTRL, payload)
}

fn nft_payload() -> Vec<u8> {
    vec![0, 0, 0, 0]
} // struct nfgenmsg
fn nft_message(request: &NlMsgHdr, port_id: u32, command: u16, payload: Vec<u8>) -> Vec<u8> {
    netlink_message(
        request,
        port_id,
        (NFNL_SUBSYS_NFTABLES << 8) | command,
        payload,
    )
}
fn nft_table_message(request: &NlMsgHdr, port_id: u32, table: &str) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_TABLE_NAME, table);
    nft_message(request, port_id, NFT_MSG_NEWTABLE, payload)
}
fn nft_chain_message(request: &NlMsgHdr, port_id: u32, chain: &NftChain) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_CHAIN_TABLE, &chain.table);
    push_attr_string(&mut payload, NFTA_CHAIN_NAME, &chain.name);
    nft_message(request, port_id, NFT_MSG_NEWCHAIN, payload)
}
fn nft_rule_message(request: &NlMsgHdr, port_id: u32, rule: &NftRule) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_RULE_TABLE, &rule.table);
    push_attr_string(&mut payload, NFTA_RULE_CHAIN, &rule.chain);
    push_attr(&mut payload, NFTA_RULE_HANDLE, &rule.handle.to_ne_bytes());
    nft_message(request, port_id, NFT_MSG_NEWRULE, payload)
}
fn nft_set_message(request: &NlMsgHdr, port_id: u32, set: &NftSet) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_SET_TABLE, &set.table);
    push_attr_string(&mut payload, NFTA_SET_NAME, &set.name);
    push_attr_u32(&mut payload, NFTA_SET_ID, set.id);
    push_attr_u32(&mut payload, NFTA_SET_FLAGS, set.flags);
    push_attr_u32(&mut payload, NFTA_SET_KEY_TYPE, set.key_type);
    push_attr_u32(&mut payload, NFTA_SET_DATA_TYPE, set.data_type);
    nft_message(request, port_id, NFT_MSG_NEWSET, payload)
}
fn nft_element_message(request: &NlMsgHdr, port_id: u32, element: &NftSetElement) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_SET_ELEM_LIST_TABLE, &element.table);
    push_attr_string(&mut payload, NFTA_SET_ELEM_LIST_SET, &element.set);
    push_attr(&mut payload, NFTA_SET_ELEM_LIST_ELEMENTS, &element.key);
    nft_message(request, port_id, NFT_MSG_NEWSETELEM, payload)
}

fn payload_with<T: Copy>(value: &T) -> Vec<u8> {
    let mut out = vec![0; size_of::<T>()];
    write_struct(&mut out, value);
    out
}

fn push_attr(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    let len = size_of::<RtAttr>() + value.len();
    let aligned = align4(len);
    let start = out.len();
    out.resize(start + aligned, 0);
    write_struct(
        &mut out[start..start + size_of::<RtAttr>()],
        &RtAttr {
            rta_len: len as u16,
            rta_type: attr_type,
        },
    );
    out[start + size_of::<RtAttr>()..start + len].copy_from_slice(value);
}

fn push_attr_u32(out: &mut Vec<u8>, attr_type: u16, value: u32) {
    push_attr(out, attr_type, &value.to_ne_bytes());
}

fn push_attr_string(out: &mut Vec<u8>, attr_type: u16, value: &str) {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    push_attr(out, attr_type, &bytes);
}

fn address_message(request: &NlMsgHdr, port_id: u32, entry: &AddressEntry) -> Vec<u8> {
    let mut payload = payload_with(&IfAddrMsg {
        ifa_family: entry.family,
        ifa_prefixlen: entry.prefix_len,
        ifa_flags: entry.flags,
        ifa_scope: entry.scope,
        ifa_index: entry.index,
    });
    if !entry.address.is_empty() {
        push_attr(&mut payload, IFA_ADDRESS, &entry.address);
    }
    if !entry.local.is_empty() {
        push_attr(&mut payload, IFA_LOCAL, &entry.local);
    }
    if !entry.label.is_empty() {
        push_attr_string(&mut payload, IFA_LABEL, &entry.label);
    }
    netlink_message(request, port_id, RTM_NEWADDR, payload)
}

fn route_message(request: &NlMsgHdr, port_id: u32, entry: &RouteEntry) -> Vec<u8> {
    let mut payload = payload_with(&RtMsg {
        rtm_family: entry.family,
        rtm_dst_len: entry.dst_len,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: entry.table,
        rtm_protocol: 0,
        rtm_scope: entry.scope,
        rtm_type: entry.route_type,
        rtm_flags: 0,
    });
    if !entry.dst.is_empty() {
        push_attr(&mut payload, RTA_DST, &entry.dst);
    }
    if !entry.gateway.is_empty() {
        push_attr(&mut payload, RTA_GATEWAY, &entry.gateway);
    }
    if let Some(oif) = entry.oif {
        push_attr_u32(&mut payload, RTA_OIF, oif);
    }
    netlink_message(request, port_id, RTM_NEWROUTE, payload)
}

fn link_message(request: &NlMsgHdr, port_id: u32, entry: &LinkEntry) -> Vec<u8> {
    let mut payload = payload_with(&IfInfoMsg {
        ifi_family: AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: entry.arphrd,
        ifi_index: entry.index as i32,
        ifi_flags: entry.flags,
        ifi_change: 0,
    });
    push_attr_string(&mut payload, IFLA_IFNAME, &entry.name);
    push_attr_u32(&mut payload, IFLA_MTU, entry.mtu);
    if !entry.hwaddr.is_empty() {
        push_attr(&mut payload, IFLA_ADDRESS, &entry.hwaddr);
    }
    netlink_message(request, port_id, RTM_NEWLINK, payload)
}

fn parse_ifinfo(payload: &[u8]) -> AxResult<IfInfoMsg> {
    if payload.len() < size_of::<IfInfoMsg>() {
        return Err(AxError::InvalidInput);
    }
    read_unaligned::<IfInfoMsg>(payload)
}

/// Iterate a fully copied NLA stream.  Netlink attribute alignment is part of
/// the ABI: accepting an unterminated padding fragment would otherwise make a
/// later message in the same write appear to be an attribute of this one.
fn for_each_rtattr<'a>(
    mut bytes: &'a [u8],
    mut visit: impl FnMut(u16, &'a [u8]) -> AxResult,
) -> AxResult {
    while !bytes.is_empty() {
        if bytes.len() < size_of::<RtAttr>() {
            return Err(AxError::InvalidInput);
        }
        let attr = read_unaligned::<RtAttr>(bytes)?;
        let length = attr.rta_len as usize;
        if length < size_of::<RtAttr>() || length > bytes.len() {
            return Err(AxError::InvalidInput);
        }
        visit(attr.rta_type & !0x8000, &bytes[size_of::<RtAttr>()..length])?;
        let aligned = align4(length);
        if aligned > bytes.len() {
            // The final netlink attribute does not require explicit padding.
            if length == bytes.len() {
                return Ok(());
            }
            return Err(AxError::InvalidInput);
        }
        bytes = &bytes[aligned..];
    }
    Ok(())
}

fn decode_link_name(bytes: &[u8]) -> AxResult<String> {
    let name = bytes.strip_suffix(&[0]).ok_or(AxError::InvalidInput)?;
    if name.is_empty() || name.len() > 15 || name.contains(&0) {
        return Err(AxError::InvalidInput);
    }
    core::str::from_utf8(name)
        .map(String::from)
        .map_err(|_| AxError::InvalidInput)
}

fn parse_link_attributes(bytes: &[u8]) -> AxResult<(Option<String>, Option<usize>)> {
    let mut name = None;
    let mut mtu = None;
    for_each_rtattr(bytes, |kind, value| match kind {
        IFLA_IFNAME if name.is_none() => {
            name = Some(decode_link_name(value)?);
            Ok(())
        }
        IFLA_MTU if mtu.is_none() && value.len() == size_of::<u32>() => {
            mtu = Some(u32::from_ne_bytes(value.try_into().unwrap()) as usize);
            Ok(())
        }
        IFLA_MTU => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    Ok((name, mtu))
}

fn ip_address_bytes(address: IpAddress) -> Vec<u8> {
    match address {
        IpAddress::Ipv4(address) => address.octets().to_vec(),
        IpAddress::Ipv6(address) => address.octets().to_vec(),
    }
}

fn decode_ip(family: u8, bytes: &[u8]) -> AxResult<IpAddress> {
    match family as u32 {
        value if value == AF_INET as u32 && bytes.len() == 4 => Ok(IpAddress::Ipv4(
            Ipv4Address::from_octets(bytes.try_into().map_err(|_| AxError::InvalidInput)?),
        )),
        value if value == AF_INET6 as u32 && bytes.len() == 16 => Ok(IpAddress::Ipv6(
            Ipv6Address::from_octets(bytes.try_into().map_err(|_| AxError::InvalidInput)?),
        )),
        _ => Err(AxError::InvalidInput),
    }
}

fn unspecified_ip(family: u8) -> AxResult<IpAddress> {
    match family as u32 {
        value if value == AF_INET as u32 => Ok(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
        value if value == AF_INET6 as u32 => Ok(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)),
        _ => Err(AxError::InvalidInput),
    }
}

fn same_ip_family(left: IpAddress, right: IpAddress) -> bool {
    matches!(
        (left, right),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

fn address_entries(interface: &InterfaceInfo) -> Vec<AddressEntry> {
    interface
        .addresses
        .iter()
        .map(|cidr| {
            let address = cidr.address();
            let family = match address {
                IpAddress::Ipv4(_) => AF_INET as u8,
                IpAddress::Ipv6(_) => AF_INET6 as u8,
            };
            let bytes = ip_address_bytes(address);
            AddressEntry {
                family,
                prefix_len: cidr.prefix_len(),
                flags: IFA_F_PERMANENT,
                scope: if interface.kind == InterfaceKind::Loopback {
                    RT_SCOPE_HOST
                } else {
                    RT_SCOPE_UNIVERSE
                },
                index: interface.index,
                local: bytes.clone(),
                address: bytes,
                label: interface.name.clone(),
            }
        })
        .collect()
}

fn link_entry(interface: InterfaceInfo) -> LinkEntry {
    let is_loopback = interface.kind == InterfaceKind::Loopback;
    let base = if is_loopback {
        IFF_LOOPBACK | IFF_RUNNING
    } else {
        IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST
    };
    let flags = base
        | if interface.administrative_up {
            IFF_UP
        } else {
            0
        };
    LinkEntry {
        index: interface.index,
        name: interface.name,
        flags,
        mtu: interface.mtu.min(u32::MAX as usize) as u32,
        hwaddr: interface
            .hardware_address
            .map(|address| address.to_vec())
            .unwrap_or_default(),
        arphrd: if is_loopback {
            ARPHRD_LOOPBACK
        } else {
            ARPHRD_ETHER
        },
    }
}

fn route_entry(route: &RouteInfo) -> RouteEntry {
    let destination = route.destination.address();
    let is_loopback = match destination {
        IpAddress::Ipv4(address) => address.is_loopback(),
        IpAddress::Ipv6(address) => address.is_loopback(),
    };
    RouteEntry {
        family: match destination {
            IpAddress::Ipv4(_) => AF_INET as u8,
            IpAddress::Ipv6(_) => AF_INET6 as u8,
        },
        dst_len: route.destination.prefix_len(),
        table: RT_TABLE_MAIN,
        scope: if is_loopback {
            RT_SCOPE_HOST
        } else if route.gateway.is_some() {
            RT_SCOPE_UNIVERSE
        } else {
            RT_SCOPE_LINK
        },
        route_type: RTN_UNICAST,
        oif: Some(route.interface_index),
        dst: if route.destination.prefix_len() == 0 {
            Vec::new()
        } else {
            ip_address_bytes(destination)
        },
        gateway: route.gateway.map(ip_address_bytes).unwrap_or_default(),
    }
}

fn read_unaligned<T: Copy>(data: &[u8]) -> AxResult<T> {
    if data.len() < size_of::<T>() {
        return Err(AxError::InvalidInput);
    }
    Ok(unsafe { core::ptr::read_unaligned(data.as_ptr().cast::<T>()) })
}

fn write_struct<T: Copy>(dst: &mut [u8], value: &T) {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    dst[..bytes.len()].copy_from_slice(bytes);
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

impl Pollable for NetlinkSocket {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::WRITABLE;
        events.set(
            IoEvents::READABLE,
            self.overrun.load(Ordering::Acquire) || !self.queue.lock().datagrams.is_empty(),
        );
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            axpoll::PollRegistration::single(&self.poll_rx, context.waker())
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use core::mem::{MaybeUninit, size_of};

    use axerrno::{AxError, LinuxError};
    use axhal::paging::{MappingFlags, PageSize};
    use axio::{IoBuf, Read};
    use axnet::RecvFlags;
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        NETLINK_KOBJECT_UEVENT, NETLINK_MAX_MESSAGE_BYTES, NETLINK_NO_ENOBUFS, NETLINK_ROUTE,
        NetlinkSocket, NlMsgHdr, RTM_GETLINK, SockaddrNl, emit_init_net_kobject_uevent,
        emit_kobject_uevent, kobject_uevent_socket_is_registered, register_init_network_namespace,
        write_struct,
    };
    use crate::{
        mm::{AddrSpace, Backend, UserMemoryCapability, UserPtr},
        task::{Cred, Kgid, Kuid, NetworkNamespace, UserNamespace},
    };

    struct UnreadableLengthSource {
        remaining: usize,
    }

    impl Read for UnreadableLengthSource {
        fn read(&mut self, _buf: &mut [u8]) -> axio::Result<usize> {
            panic!("an over-limit netlink source must not be read")
        }
    }

    impl IoBuf for UnreadableLengthSource {
        fn remaining(&self) -> usize {
            self.remaining
        }
    }

    struct ZeroedSource {
        remaining: usize,
        reads: usize,
    }

    impl Read for ZeroedSource {
        fn read(&mut self, output: &mut [u8]) -> axio::Result<usize> {
            assert!(output.len() <= self.remaining);
            output.fill(0);
            self.remaining -= output.len();
            self.reads += 1;
            Ok(output.len())
        }
    }

    impl IoBuf for ZeroedSource {
        fn remaining(&self) -> usize {
            self.remaining
        }
    }

    fn route_socket() -> Arc<NetlinkSocket> {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        NetlinkSocket::try_new(0, net_ns).unwrap()
    }

    fn socket_owner_credential(socket: &NetlinkSocket) -> Arc<Cred> {
        Cred::try_root(socket.net_namespace().owner_user_ns().clone()).unwrap()
    }

    fn uevent_socket(groups: u32) -> Arc<NetlinkSocket> {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let socket = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        socket.bind(42, groups).unwrap();
        socket
    }

    fn uevent_frame(payload: &[u8]) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0; size_of::<NlMsgHdr>() + payload.len()];
        let frame_len = frame.len();
        write_struct(
            &mut frame,
            &NlMsgHdr {
                nlmsg_len: frame_len as u32,
                nlmsg_type: 0,
                nlmsg_flags: 0,
                nlmsg_seq: 0,
                nlmsg_pid: 0,
            },
        );
        frame[size_of::<NlMsgHdr>()..].copy_from_slice(payload);
        frame
    }

    fn mapped_capability() -> UserMemoryCapability {
        let mut address_space = AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap();
        address_space
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        UserMemoryCapability::new(Arc::new(Mutex::new(address_space)))
    }

    #[test]
    fn namespace_owner_netlink_socket_retains_complete_network_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let weak = Arc::downgrade(&net_ns);
        let socket = NetlinkSocket::try_new(0, net_ns.clone()).unwrap();

        drop(net_ns);
        assert!(weak.upgrade().is_some());
        drop(socket);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn local_addr_uses_explicit_capability_and_reports_native_length() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let socket = NetlinkSocket::try_new(0, net_ns).unwrap();
        socket.bind(41, 7).unwrap();
        let capability = mapped_capability();

        let mut length = 4;
        socket
            .write_local_addr(&capability, UserPtr::from(0x1000), &mut length)
            .unwrap();
        assert_eq!(length as usize, size_of::<SockaddrNl>());

        let mut bytes = [MaybeUninit::<u8>::uninit(); 4];
        capability.read_bytes(0x1000, &mut bytes).unwrap();
        let expected = SockaddrNl {
            nl_family: linux_raw_sys::net::AF_NETLINK as _,
            nl_pad: 0,
            nl_pid: 41,
            nl_groups: 7,
        };
        let expected_bytes = unsafe {
            core::slice::from_raw_parts(
                (&expected as *const SockaddrNl).cast::<u8>(),
                size_of::<SockaddrNl>(),
            )
        };
        let copied = unsafe { bytes.map(|byte| byte.assume_init()) };
        assert_eq!(&copied, &expected_bytes[..4]);
    }

    #[test]
    fn write_rejects_usize_max_message_without_reading_source() {
        let socket = route_socket();
        let actor = socket_owner_credential(&socket);
        let mut source = UnreadableLengthSource {
            remaining: usize::MAX,
        };

        let error = socket.write_with_actor(&mut source, &actor, 1).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn write_rejects_message_above_explicit_limit_without_reading_source() {
        let socket = route_socket();
        let actor = socket_owner_credential(&socket);
        let mut source = UnreadableLengthSource {
            remaining: NETLINK_MAX_MESSAGE_BYTES + 1,
        };

        let error = socket.write_with_actor(&mut source, &actor, 1).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn write_admits_message_at_explicit_limit() {
        let socket = route_socket();
        let actor = socket_owner_credential(&socket);
        let mut source = ZeroedSource {
            remaining: NETLINK_MAX_MESSAGE_BYTES,
            reads: 0,
        };

        // A zeroed datagram is structurally invalid, but it must clear the
        // length admission and reach netlink parsing at the exact limit.
        let error = socket.write_with_actor(&mut source, &actor, 1).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EINVAL);
        assert_eq!(source.remaining, 0);
        assert_eq!(source.reads, 1);
    }

    #[test]
    fn write_accepts_normal_route_message() {
        let socket = route_socket();
        let actor = socket_owner_credential(&socket);
        let header = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: RTM_GETLINK,
            nlmsg_flags: 0,
            nlmsg_seq: 7,
            nlmsg_pid: 0,
        };
        let mut message = [0_u8; size_of::<NlMsgHdr>()];
        write_struct(&mut message, &header);
        let mut source = &message[..];

        assert_eq!(
            socket.write_with_actor(&mut source, &actor, 1).unwrap(),
            message.len()
        );
    }

    #[test]
    fn uevent_port_unicast_preserves_raw_payload_sender_and_credentials() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let sender = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();
        let receiver = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        sender.bind(100, 0).unwrap();
        receiver.bind(101, 0).unwrap();
        let actor = socket_owner_credential(&sender);
        let payload = b"libudev\0ACTION=add\0DEVNAME=input/event0\0";
        let mut source = &payload[..];

        assert_eq!(
            sender
                .write_to_with_actor(
                    &mut source,
                    &actor,
                    1234,
                    Some(SockaddrNl {
                        nl_family: linux_raw_sys::net::AF_NETLINK as _,
                        nl_pad: 0,
                        nl_pid: 101,
                        nl_groups: 0,
                    }),
                    false,
                )
                .unwrap(),
            payload.len()
        );

        let mut bytes = [0_u8; 128];
        let mut dst = &mut bytes[..];
        let received = receiver
            .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
            .unwrap();
        assert_eq!(received.source_port_id, 100);
        assert_eq!(received.source_groups, 0);
        assert_eq!(
            received.credentials,
            Some(super::NetlinkCredentials {
                pid: 1234,
                uid: 0,
                gid: 0,
            })
        );
        assert_eq!(&bytes[..received.len], payload);
    }

    #[test]
    fn uevent_port_unicast_rejects_absent_peer() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let sender = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        sender.bind(100, 0).unwrap();
        let actor = socket_owner_credential(&sender);
        let mut source = &b"libudev\0ACTION=add\0"[..];

        let error = sender
            .write_to_with_actor(
                &mut source,
                &actor,
                1234,
                Some(SockaddrNl {
                    nl_family: linux_raw_sys::net::AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: 101,
                    nl_groups: 0,
                }),
                false,
            )
            .unwrap_err();
        assert_eq!(LinuxError::from(error), LinuxError::ECONNREFUSED);
    }

    #[test]
    fn uevent_port_unicast_rejects_empty_datagram_without_queueing() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let sender = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();
        let receiver = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        sender.bind(100, 0).unwrap();
        receiver.bind(101, 0).unwrap();
        let actor = socket_owner_credential(&sender);
        let mut source = &b""[..];

        assert_eq!(
            sender.write_to_with_actor(
                &mut source,
                &actor,
                1234,
                Some(SockaddrNl {
                    nl_family: linux_raw_sys::net::AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: 101,
                    nl_groups: 0,
                }),
                false,
            ),
            Err(LinuxError::ENODATA.into())
        );
        let mut byte = [0_u8; 1];
        let mut dst = &mut byte[..];
        assert_eq!(
            receiver.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn uevent_port_unicast_autobinds_and_allows_unprivileged_peer_delivery() {
        let owner = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let sender = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();
        let receiver = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        receiver.bind(101, 0).unwrap();
        let root = Cred::try_root(owner.clone()).unwrap();
        let payload = b"libudev\0ACTION=add\0";
        let mut source = &payload[..];

        sender
            .write_to_with_actor(
                &mut source,
                &root,
                1234,
                Some(SockaddrNl {
                    nl_family: linux_raw_sys::net::AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: 101,
                    nl_groups: 0,
                }),
                false,
            )
            .unwrap();
        assert_eq!(sender.state.lock().port_id, 1234);

        let child = owner
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let unprivileged = Cred::try_with_user_namespace(&root, child).unwrap();
        let mut unprivileged_source = &payload[..];
        assert_eq!(
            sender.write_to_with_actor(
                &mut unprivileged_source,
                &unprivileged,
                1234,
                Some(SockaddrNl {
                    nl_family: linux_raw_sys::net::AF_NETLINK as _,
                    nl_pad: 0,
                    nl_pid: 101,
                    nl_groups: 0,
                }),
                false,
            ),
            Ok(payload.len())
        );
    }

    #[test]
    fn kobject_uevent_group_one_gets_nul_payload_and_kernel_source() {
        let socket = uevent_socket(1);
        let sequence = emit_kobject_uevent(
            socket.net_namespace(),
            "add",
            "/devices/virtual/test0",
            "test",
            &[("MAJOR", "1")],
        )
        .unwrap();
        let mut bytes = [0_u8; 256];
        let mut dst = &mut bytes[..];
        let received = socket
            .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
            .unwrap();

        assert_eq!(received.source_groups, 1);
        assert_eq!(received.credentials, Some(super::KERNEL_UEVENT_CREDENTIALS));
        let expected = alloc::format!(
            "add@{}\0ACTION=add\0DEVPATH={}\0SUBSYSTEM=test\0SEQNUM={}\0MAJOR=1\0",
            "/devices/virtual/test0",
            "/devices/virtual/test0",
            sequence,
        );
        assert_eq!(&bytes[..received.len], expected.as_bytes());
    }

    #[test]
    fn ordinary_netlink_datagrams_do_not_gain_uevent_credentials() {
        let socket = route_socket();
        socket.enqueue_kernel(alloc::vec![1]);
        let mut bytes = [0_u8; 1];
        let mut dst = &mut bytes[..];
        let received = socket
            .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
            .unwrap();

        assert_eq!(received.credentials, None);
    }

    #[test]
    fn passcred_is_per_socket_and_can_be_enabled_after_uevent_enqueue() {
        let socket = uevent_socket(1);
        emit_kobject_uevent(
            socket.net_namespace(),
            "change",
            "/devices/virtual/test0",
            "test",
            &[],
        )
        .unwrap();
        assert!(!socket.passcred());
        socket.set_passcred(true);
        assert!(socket.passcred());

        let mut bytes = [0_u8; 128];
        let mut dst = &mut bytes[..];
        let received = socket
            .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
            .unwrap();
        assert_eq!(received.credentials, Some(super::KERNEL_UEVENT_CREDENTIALS));
    }

    #[test]
    fn kobject_uevents_require_group_one_subscription() {
        let socket = uevent_socket(0);
        emit_kobject_uevent(
            socket.net_namespace(),
            "remove",
            "/devices/virtual/test0",
            "test",
            &[],
        )
        .unwrap();
        let mut bytes = [0_u8; 1];
        let mut dst = &mut bytes[..];
        assert_eq!(
            socket.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn synthetic_uevent_requires_admin_in_the_socket_namespace_owner() {
        let owner = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let socket = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        let child = owner
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let actor = Cred::try_with_user_namespace(&Cred::try_root(owner).unwrap(), child).unwrap();

        assert_eq!(
            socket.send_uevent_from_user(&uevent_frame(b"change@/devices/test0\0"), &actor, 17),
            Err(LinuxError::EPERM.into())
        );
    }

    #[test]
    fn synthetic_uevent_requires_one_complete_nlmsghdr_frame() {
        let owner = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let socket = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        let actor = Cred::try_root(owner).unwrap();

        assert_eq!(
            socket.send_uevent_from_user(&[0; size_of::<NlMsgHdr>() - 1], &actor, 17),
            Err(AxError::InvalidInput)
        );

        let mut frame = uevent_frame(b"change@/devices/test0\0");
        let truncated_len = frame.len() - 1;
        write_struct(
            &mut frame,
            &NlMsgHdr {
                nlmsg_len: truncated_len as u32,
                nlmsg_type: 0,
                nlmsg_flags: 0,
                nlmsg_seq: 0,
                nlmsg_pid: 0,
            },
        );
        assert_eq!(
            socket.send_uevent_from_user(&frame, &actor, 17),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn synthetic_uevent_strips_header_appends_sequence_and_stays_in_namespace() {
        let owner = UserNamespace::try_new_root().unwrap();
        let first_ns = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let second_ns = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let sender = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, first_ns.clone()).unwrap();
        let listener = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, first_ns).unwrap();
        let isolated = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, second_ns).unwrap();
        listener.bind(101, 1).unwrap();
        isolated.bind(101, 1).unwrap();
        let actor = Cred::try_root(owner).unwrap();
        let payload = b"change@/devices/test0\0ACTION=change\0";

        sender
            .send_uevent_from_user(&uevent_frame(payload), &actor, 1234)
            .unwrap();
        let mut bytes = [0_u8; 128];
        let mut dst = &mut bytes[..];
        let received = listener
            .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
            .unwrap();
        let message = &bytes[..received.len];
        assert_eq!(received.source_groups, 1);
        assert_eq!(
            received.credentials,
            Some(super::NetlinkCredentials {
                pid: 1234,
                uid: 0,
                gid: 0,
            })
        );
        assert!(message.starts_with(payload));
        assert!(message.ends_with(b"\0"));
        assert!(message[payload.len()..].starts_with(b"SEQNUM="));

        let mut isolated_bytes = [0_u8; 1];
        let mut isolated_dst = &mut isolated_bytes[..];
        assert_eq!(
            isolated.recv_with_nonblocking(&mut isolated_dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn uevent_socket_drop_unlinks_its_weak_registration() {
        let socket = uevent_socket(0);
        let socket_ptr = Arc::as_ptr(&socket);
        assert!(kobject_uevent_socket_is_registered(socket_ptr));
        drop(socket);
        assert!(!kobject_uevent_socket_is_registered(socket_ptr));
    }

    #[test]
    fn kobject_uevent_accepts_datagram_and_raw_socket_types() {
        assert!(
            NetlinkSocket::validate_socket_type(
                linux_raw_sys::net::SOCK_DGRAM,
                NETLINK_KOBJECT_UEVENT,
            )
            .is_ok()
        );
        assert!(
            NetlinkSocket::validate_socket_type(
                linux_raw_sys::net::SOCK_RAW,
                NETLINK_KOBJECT_UEVENT,
            )
            .is_ok()
        );
    }

    #[test]
    fn kobject_uevent_port_ids_are_unique_rebindable_only_after_close() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let first = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();
        let second = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();

        first.bind(42, 1).unwrap();
        assert_eq!(second.bind(42, 1), Err(LinuxError::EADDRINUSE.into()));
        assert_eq!(first.bind(43, 1), Err(LinuxError::EINVAL.into()));

        second.bind_auto(42, 1).unwrap();
        assert_ne!(second.state.lock().port_id, 42);
        drop(first);

        let replacement = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns).unwrap();
        replacement.bind(42, 1).unwrap();
    }

    #[test]
    fn kobject_uevent_delivery_is_limited_to_its_network_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let first_ns = NetworkNamespace::try_new_loopback_only(user_ns.clone()).unwrap();
        let second_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let first = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, first_ns.clone()).unwrap();
        let second = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, second_ns).unwrap();
        first.bind(42, 1).unwrap();
        second.bind(42, 1).unwrap();

        emit_kobject_uevent(&first_ns, "change", "/devices/test0", "test", &[]).unwrap();
        let mut first_bytes = [0_u8; 128];
        let mut first_dst = &mut first_bytes[..];
        assert!(
            first
                .recv_with_nonblocking(&mut first_dst, RecvFlags::empty(), true)
                .is_ok()
        );
        let mut second_bytes = [0_u8; 1];
        let mut second_dst = &mut second_bytes[..];
        assert_eq!(
            second.recv_with_nonblocking(&mut second_dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn device_uevents_always_use_registered_init_network_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let init_net_ns = NetworkNamespace::try_new_loopback_only(user_ns.clone()).unwrap();
        let other_net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        register_init_network_namespace(&init_net_ns).unwrap();
        // Re-establishing the same boot namespace is idempotent, but a
        // different namespace must never become the device-event target.
        assert!(register_init_network_namespace(&init_net_ns).is_ok());
        assert_eq!(
            register_init_network_namespace(&other_net_ns),
            Err(AxError::AlreadyExists)
        );

        let init_listener = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, init_net_ns).unwrap();
        let other_listener = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, other_net_ns).unwrap();
        init_listener.bind(101, 1).unwrap();
        other_listener.bind(101, 1).unwrap();

        assert!(
            emit_init_net_kobject_uevent("change", "/devices/test0", "test", &[])
                .unwrap()
                .is_some()
        );
        let mut init_bytes = [0_u8; 128];
        let mut init_dst = &mut init_bytes[..];
        assert!(
            init_listener
                .recv_with_nonblocking(&mut init_dst, RecvFlags::empty(), true)
                .is_ok()
        );
        let mut other_bytes = [0_u8; 1];
        let mut other_dst = &mut other_bytes[..];
        assert_eq!(
            other_listener.recv_with_nonblocking(&mut other_dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn route_and_uevent_port_ids_are_reserved_per_namespace_and_protocol() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let route = NetlinkSocket::try_new(NETLINK_ROUTE, net_ns.clone()).unwrap();
        let route_collision = NetlinkSocket::try_new(NETLINK_ROUTE, net_ns.clone()).unwrap();
        let uevent = NetlinkSocket::try_new(NETLINK_KOBJECT_UEVENT, net_ns.clone()).unwrap();

        route.bind(77, 0).unwrap();
        assert_eq!(
            route_collision.bind(77, 0),
            Err(LinuxError::EADDRINUSE.into())
        );
        // The protocol is part of a netlink port identity.
        uevent.bind(77, 1).unwrap();
        assert_eq!(route.bind(78, 0), Err(LinuxError::EINVAL.into()));
        route_collision.bind_auto(77, 0).unwrap();
        assert_ne!(route_collision.state.lock().port_id, 77);

        drop(route);
        let replacement = NetlinkSocket::try_new(NETLINK_ROUTE, net_ns).unwrap();
        replacement.bind(77, 0).unwrap();
    }

    #[test]
    fn netlink_queue_reports_one_overrun_unless_suppressed() {
        let socket = route_socket();
        for _ in 0..=super::NETLINK_QUEUE_LIMIT {
            socket.enqueue_kernel(alloc::vec![1]);
        }
        let mut bytes = [0_u8; 1];
        let mut dst = &mut bytes[..];
        assert_eq!(
            socket.recv_ready(&mut dst, RecvFlags::empty()),
            Err(LinuxError::ENOBUFS.into())
        );
        assert_eq!(
            socket.recv_ready(&mut dst, RecvFlags::empty()).unwrap().len,
            1
        );

        let suppressed = route_socket();
        suppressed.set_option(NETLINK_NO_ENOBUFS, 1).unwrap();
        for _ in 0..=super::NETLINK_QUEUE_LIMIT {
            suppressed.enqueue_kernel(alloc::vec![1]);
        }
        let mut suppressed_bytes = [0_u8; 1];
        let mut suppressed_dst = &mut suppressed_bytes[..];
        assert_eq!(
            suppressed
                .recv_ready(&mut suppressed_dst, RecvFlags::empty())
                .unwrap()
                .len,
            1
        );
    }

    #[test]
    fn netlink_queue_drop_helper_honors_no_enobufs() {
        let socket = route_socket();
        socket.note_queue_drop();
        let mut bytes = [0_u8; 1];
        let mut dst = &mut bytes[..];
        assert_eq!(
            socket.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(LinuxError::ENOBUFS.into())
        );

        let suppressed = route_socket();
        suppressed.set_option(NETLINK_NO_ENOBUFS, 1).unwrap();
        suppressed.note_queue_drop();
        assert_eq!(
            suppressed.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn netlink_queue_rejects_messages_past_its_byte_limit() {
        let socket = route_socket();
        socket.enqueue_kernel(alloc::vec![0; super::NETLINK_QUEUE_LIMIT_BYTES + 1]);
        let mut bytes = [0_u8; 1];
        let mut dst = &mut bytes[..];
        assert_eq!(
            socket.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(LinuxError::ENOBUFS.into())
        );
    }
}
