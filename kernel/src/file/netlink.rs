use alloc::{borrow::Cow, collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axnet::{InterfaceInfo, InterfaceKind, IpAddress, RecvFlags, RouteInfo};
use axpoll::{IoEvents, PollSet, Pollable};
use linux_raw_sys::net::{
    AF_INET, AF_INET6, AF_NETLINK, AF_UNSPEC, SOCK_DGRAM, SOCK_RAW, sockaddr, socklen_t,
};
use spin::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, PseudoInode, try_pseudo_inode_path},
    mm::{UserMemoryCapability, UserPtr, map_usercopy_error},
    readiness::block_on_poll_io,
    task::NetworkNamespace,
};

const NETLINK_MAX_PROTOCOL: u32 = 31;
const NETLINK_QUEUE_LIMIT: usize = 128;
// Linux limits one netlink datagram to sk_sndbuf minus 32 bytes. TheKernel
// does not yet expose a configurable netlink SO_SNDBUF, so use Linux's
// 208-KiB default send buffer as the explicit per-message admission budget.
const NETLINK_DEFAULT_SEND_BUFFER_BYTES: usize = 208 * 1024;
const NETLINK_SEND_BUFFER_OVERHEAD: usize = 32;
const NETLINK_MAX_MESSAGE_BYTES: usize =
    NETLINK_DEFAULT_SEND_BUFFER_BYTES - NETLINK_SEND_BUFFER_OVERHEAD;
const NETLINK_ROUTE: u32 = 0;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
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
}

pub struct NetlinkSocket {
    protocol: u32,
    net_ns: Arc<NetworkNamespace>,
    inode: PseudoInode,
    state: Mutex<NetlinkState>,
    queue: Mutex<VecDeque<Vec<u8>>>,
    nonblocking: AtomicBool,
    poll_rx: PollSet,
}

impl NetlinkSocket {
    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub fn validate_socket_type(ty: u32, protocol: u32) -> AxResult {
        if !matches!(ty, SOCK_RAW | SOCK_DGRAM) {
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        if protocol > NETLINK_MAX_PROTOCOL || protocol != NETLINK_ROUTE {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        Ok(())
    }

    pub(crate) fn try_new(protocol: u32, net_ns: Arc<NetworkNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            protocol,
            net_ns,
            inode: PseudoInode::socket(),
            state: Mutex::new(NetlinkState::default()),
            queue: Mutex::new(VecDeque::new()),
            nonblocking: AtomicBool::new(false),
            poll_rx: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub fn bind(&self, port_id: u32, groups: u32) -> AxResult {
        let mut state = self.state.lock();
        state.port_id = port_id;
        state.groups = groups;
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
        let mut queue = self.queue.lock();
        if queue.len() >= NETLINK_QUEUE_LIMIT {
            queue.pop_front();
        }
        queue.push_back(data);
        drop(queue);
        self.poll_rx.wake();
    }

    pub fn recv(&self, dst: &mut IoDst, flags: RecvFlags) -> AxResult<usize> {
        self.recv_with_nonblocking(dst, flags, self.nonblocking())
    }

    pub(crate) fn recv_with_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: RecvFlags,
        nonblocking: bool,
    ) -> AxResult<usize> {
        block_on_poll_io(
            self,
            IoEvents::READABLE,
            nonblocking || flags.contains(RecvFlags::DONT_WAIT),
            || {
                let mut queue = self.queue.lock();
                let Some(packet) = queue.front() else {
                    return Err(AxError::WouldBlock);
                };

                let packet_len = packet.len();
                let copy_len = packet_len.min(dst.remaining_mut());
                dst.write(&packet[..copy_len])?;
                if !flags.contains(RecvFlags::PEEK) {
                    queue.pop_front();
                }

                Ok(if flags.contains(RecvFlags::TRUNCATE) {
                    packet_len
                } else {
                    copy_len
                })
            },
        )
    }

    fn handle_write(&self, data: &[u8]) -> AxResult {
        if self.protocol != NETLINK_ROUTE {
            return Err(AxError::OperationNotSupported);
        }

        let mut offset = 0usize;
        while offset + size_of::<NlMsgHdr>() <= data.len() {
            let hdr = read_unaligned::<NlMsgHdr>(&data[offset..])?;
            if hdr.nlmsg_len < size_of::<NlMsgHdr>() as u32 {
                return Err(AxError::InvalidInput);
            }

            let msg_len = hdr.nlmsg_len as usize;
            if offset + msg_len > data.len() {
                return Err(AxError::InvalidInput);
            }

            let payload = &data[offset + size_of::<NlMsgHdr>()..offset + msg_len];
            let result = self.handle_route_message(&hdr, payload);
            if hdr.nlmsg_flags & NLM_F_ACK != 0 {
                let port_id = self.state.lock().port_id;
                let err = match result {
                    Ok(()) => 0,
                    Err(AxError::InvalidInput) => -LinuxError::EINVAL.code(),
                    Err(AxError::NotFound) => -LinuxError::ENOENT.code(),
                    Err(AxError::OperationNotSupported) => -LinuxError::EOPNOTSUPP.code(),
                    Err(_) => -LinuxError::EINVAL.code(),
                };
                self.enqueue_kernel(netlink_ack(&hdr, port_id, err));
            } else {
                result?;
            }

            offset += align4(msg_len);
        }

        Ok(())
    }

    fn handle_route_message(&self, hdr: &NlMsgHdr, payload: &[u8]) -> AxResult {
        match hdr.nlmsg_type {
            RTM_NEWROUTE | RTM_DELROUTE => Err(AxError::OperationNotSupported),
            RTM_GETROUTE => self.dump_routes(hdr, payload),
            RTM_NEWADDR | RTM_DELADDR => Err(AxError::OperationNotSupported),
            RTM_GETADDR => self.dump_addresses(hdr, payload),
            RTM_NEWLINK | RTM_SETLINK | RTM_DELLINK => Err(AxError::OperationNotSupported),
            RTM_GETLINK => self.dump_links(hdr, payload),
            _ => Err(AxError::OperationNotSupported),
        }
    }

    fn dump_addresses(&self, hdr: &NlMsgHdr, payload: &[u8]) -> AxResult {
        let filter = if payload.len() >= size_of::<IfAddrMsg>() {
            Some(read_unaligned::<IfAddrMsg>(payload)?)
        } else {
            None
        };
        let port_id = self.state.lock().port_id;
        for interface in self.net_ns.stack().interfaces() {
            for entry in address_entries(&interface) {
                if let Some(filter) = filter
                    && ((filter.ifa_family != AF_UNSPEC as u8 && filter.ifa_family != entry.family)
                        || (filter.ifa_index != 0 && filter.ifa_index != entry.index))
                {
                    continue;
                }
                self.enqueue_kernel(address_message(hdr, port_id, &entry));
            }
        }
        self.enqueue_kernel(done_message(hdr, port_id));
        Ok(())
    }

    fn dump_routes(&self, hdr: &NlMsgHdr, payload: &[u8]) -> AxResult {
        let filter = if payload.len() >= size_of::<RtMsg>() {
            Some(read_unaligned::<RtMsg>(payload)?)
        } else {
            None
        };
        let port_id = self.state.lock().port_id;
        for route in self.net_ns.stack().routes() {
            let entry = route_entry(&route);
            if let Some(filter) = filter
                && filter.rtm_family != AF_UNSPEC as u8
                && filter.rtm_family != entry.family
            {
                continue;
            }
            self.enqueue_kernel(route_message(hdr, port_id, &entry));
        }
        self.enqueue_kernel(done_message(hdr, port_id));
        Ok(())
    }

    fn dump_links(&self, hdr: &NlMsgHdr, payload: &[u8]) -> AxResult {
        let filter = if payload.len() >= size_of::<IfInfoMsg>() {
            Some(read_unaligned::<IfInfoMsg>(payload)?)
        } else {
            None
        };
        let port_id = self.state.lock().port_id;
        for interface in self.net_ns.stack().interfaces() {
            let link = link_entry(interface);
            if let Some(filter) = filter
                && filter.ifi_index > 0
                && filter.ifi_index as u32 != link.index
            {
                continue;
            }
            self.enqueue_kernel(link_message(hdr, port_id, &link));
        }
        self.enqueue_kernel(done_message(hdr, port_id));
        Ok(())
    }
}

impl FileLike for NetlinkSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.recv(dst, RecvFlags::empty())
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let len = src.remaining();
        if len > NETLINK_MAX_MESSAGE_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }

        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        data.resize(len, 0);
        src.read_exact(&mut data)?;
        self.handle_write(&data)?;
        Ok(len)
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<core::time::Duration>,
        mtime: Option<core::time::Duration>,
        ctime: core::time::Duration,
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

    fn path(&self) -> AxResult<Cow<'_, str>> {
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

fn ip_address_bytes(address: IpAddress) -> Vec<u8> {
    match address {
        IpAddress::Ipv4(address) => address.octets().to_vec(),
        IpAddress::Ipv6(address) => address.octets().to_vec(),
    }
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
    let flags = if is_loopback {
        IFF_UP | IFF_LOOPBACK | IFF_RUNNING
    } else {
        IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST
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
        events.set(IoEvents::READABLE, !self.queue.lock().is_empty());
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

    use axerrno::LinuxError;
    use axhal::paging::{MappingFlags, PageSize};
    use axio::{IoBuf, Read};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        NETLINK_MAX_MESSAGE_BYTES, NetlinkSocket, NlMsgHdr, RTM_GETLINK, SockaddrNl, write_struct,
    };
    use crate::{
        file::FileLike,
        mm::{AddrSpace, Backend, UserMemoryCapability, UserPtr},
        task::{NetworkNamespace, UserNamespace},
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
        let mut source = UnreadableLengthSource {
            remaining: usize::MAX,
        };

        let error = socket.write(&mut source).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn write_rejects_message_above_explicit_limit_without_reading_source() {
        let socket = route_socket();
        let mut source = UnreadableLengthSource {
            remaining: NETLINK_MAX_MESSAGE_BYTES + 1,
        };

        let error = socket.write(&mut source).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EMSGSIZE);
    }

    #[test]
    fn write_admits_message_at_explicit_limit() {
        let socket = route_socket();
        let mut source = ZeroedSource {
            remaining: NETLINK_MAX_MESSAGE_BYTES,
            reads: 0,
        };

        // A zeroed datagram is structurally invalid, but it must clear the
        // length admission and reach netlink parsing at the exact limit.
        let error = socket.write(&mut source).unwrap_err();

        assert_eq!(LinuxError::from(error), LinuxError::EINVAL);
        assert_eq!(source.remaining, 0);
        assert_eq!(source.reads, 1);
    }

    #[test]
    fn write_accepts_normal_route_message() {
        let _context = crate::test_support::scheduler_test_context();
        let socket = route_socket();
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

        assert_eq!(socket.write(&mut source).unwrap(), message.len());
    }
}
