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
use axnet::{InterfaceInfo, InterfaceKind, IpAddress, RecvFlags, RouteInfo};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use linux_raw_sys::{
    general::CAP_SYS_ADMIN,
    net::{AF_INET, AF_INET6, AF_NETLINK, AF_UNSPEC, SOCK_DGRAM, SOCK_RAW, sockaddr, socklen_t},
};
use spin::{Lazy, Mutex};

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, PseudoInode, try_pseudo_inode_path},
    mm::{UserMemoryCapability, UserPtr, map_usercopy_error},
    readiness::block_on_poll_io,
    task::{AsThread, Cred, NetworkNamespace, ns_capable},
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
const NETLINK_QUEUE_LIMIT_BYTES: usize = NETLINK_DEFAULT_SEND_BUFFER_BYTES;
const NETLINK_ROUTE: u32 = 0;
const NETLINK_KOBJECT_UEVENT: u32 = 15;
const KOBJECT_UEVENT_GROUP: u32 = 1;
const NETLINK_NO_ENOBUFS: u32 = 5;
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
    passcred: bool,
    bound: bool,
}

pub struct NetlinkSocket {
    protocol: u32,
    net_ns: Arc<NetworkNamespace>,
    inode: PseudoInode,
    state: Mutex<NetlinkState>,
    queue: Mutex<NetlinkQueue>,
    overrun: AtomicBool,
    nonblocking: AtomicBool,
    poll_rx: PollSet,
}

struct NetlinkDatagram {
    data: Vec<u8>,
    source_groups: u32,
    credentials: Option<NetlinkCredentials>,
}

struct NetlinkQueue {
    datagrams: VecDeque<NetlinkDatagram>,
    bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NetlinkReceived {
    pub(crate) len: usize,
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
// Kobject/device discovery is global to init-net.  The namespace is a kernel
// lifetime object: device notifications must not disappear merely because
// init exits and its process-owned reference is reaped.  This one-way global
// reference forms no ownership cycle.
static INIT_NETWORK_NAMESPACE: Lazy<Mutex<Option<Arc<NetworkNamespace>>>> =
    Lazy::new(|| Mutex::new(None));
static NETLINK_PORTS: Lazy<Mutex<Vec<NetlinkPortBinding>>> = Lazy::new(|| Mutex::new(Vec::new()));
static KOBJECT_UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);
static KOBJECT_UEVENT_SEND_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static NETLINK_NEXT_PORT_ID: AtomicU32 = AtomicU32::new(1);

struct NetlinkPortBinding {
    net_ns: Weak<NetworkNamespace>,
    protocol: u32,
    port_id: u32,
    socket_inode: u64,
}

impl NetlinkSocket {
    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub fn validate_socket_type(ty: u32, protocol: u32) -> AxResult {
        if !matches!(ty, SOCK_RAW | SOCK_DGRAM) {
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        if protocol > NETLINK_MAX_PROTOCOL
            || !matches!(protocol, NETLINK_ROUTE | NETLINK_KOBJECT_UEVENT)
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

    fn enqueue_kernel_from(
        &self,
        data: Vec<u8>,
        source_groups: u32,
        credentials: Option<NetlinkCredentials>,
    ) {
        let mut queue = self.queue.lock();
        if queue.datagrams.len() >= NETLINK_QUEUE_LIMIT
            || data.len() > NETLINK_QUEUE_LIMIT_BYTES.saturating_sub(queue.bytes)
        {
            drop(queue);
            self.note_queue_drop();
            return;
        }
        queue.bytes += data.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data,
            source_groups,
            credentials,
        });
        drop(queue);
        self.poll_rx.wake();
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
        block_on_poll_io(
            self,
            IoEvents::READABLE,
            nonblocking || flags.contains(RecvFlags::DONT_WAIT),
            || {
                if self.overrun.swap(false, Ordering::AcqRel) {
                    return Err(LinuxError::ENOBUFS.into());
                }
                let mut queue = self.queue.lock();
                let Some(packet) = queue.datagrams.front() else {
                    return Err(AxError::WouldBlock);
                };

                let packet_len = packet.data.len();
                let copy_len = packet_len.min(dst.remaining_mut());
                dst.write(&packet.data[..copy_len])?;
                let source_groups = packet.source_groups;
                let credentials = packet.credentials;
                if !flags.contains(RecvFlags::PEEK) {
                    let packet = queue.datagrams.pop_front().expect("front packet vanished");
                    queue.bytes -= packet.data.len();
                }

                Ok(NetlinkReceived {
                    len: if flags.contains(RecvFlags::TRUNCATE) {
                        packet_len
                    } else {
                        copy_len
                    },
                    source_groups,
                    credentials,
                })
            },
        )
    }

    fn handle_write(&self, data: &[u8]) -> AxResult {
        match self.protocol {
            NETLINK_ROUTE => {}
            NETLINK_KOBJECT_UEVENT => return Err(LinuxError::EPERM.into()),
            _ => return Err(AxError::OperationNotSupported),
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
            return self.handle_write(data);
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
                uid: ids.euid.into_raw(),
                gid: ids.egid.into_raw(),
            },
        )
    }

    pub(crate) fn write_with_actor(
        &self,
        src: &mut IoSrc,
        actor: &Cred,
        sender_pid: u32,
    ) -> AxResult<usize> {
        let len = src.remaining();
        if len > NETLINK_MAX_MESSAGE_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }

        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        data.resize(len, 0);
        src.read_exact(&mut data)?;
        self.send_uevent_from_user(&data, actor, sender_pid)?;
        Ok(len)
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

    fn subscribed_to(&self, group: u32) -> bool {
        self.state.lock().groups & group != 0
    }
}

fn reserve_netlink_port(
    socket: &NetlinkSocket,
    preferred_port_id: u32,
    automatic: bool,
) -> AxResult<u32> {
    let mut ports = NETLINK_PORTS.lock();
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
        NETLINK_PORTS
            .lock()
            .retain(|binding| binding.socket_inode != inode);
    }
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
    if payload_len > NETLINK_MAX_MESSAGE_BYTES {
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

    broadcast_uevent_to_namespace(net_ns, &payload, KERNEL_UEVENT_CREDENTIALS);
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
    if message_len > NETLINK_MAX_MESSAGE_BYTES {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(message_len)
        .map_err(|_| AxError::NoMemory)?;
    message.extend_from_slice(payload);
    append_uevent_assignment(&mut message, "SEQNUM", &sequence_text)?;
    debug_assert_eq!(message.len(), message_len);
    broadcast_uevent_to_namespace(net_ns, &message, credentials);
    Ok(())
}

fn broadcast_uevent_to_namespace(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
) {
    let mut sockets = KOBJECT_UEVENT_SOCKETS.lock();
    sockets.retain(|socket| {
        let Some(socket) = socket.upgrade() else {
            return false;
        };
        if core::ptr::eq(socket.net_ns.as_ref(), net_ns)
            && socket.subscribed_to(KOBJECT_UEVENT_GROUP)
        {
            // Keep per-socket buffers isolated: an allocation failure for one
            // listener never makes another listener observe its datagram.
            let mut message = Vec::new();
            if message.try_reserve_exact(payload.len()).is_ok() {
                message.extend_from_slice(&payload);
                socket.enqueue_kernel_from(message, KOBJECT_UEVENT_GROUP, Some(credentials));
            } else {
                socket.note_queue_drop();
            }
        }
        true
    });
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
        // write(2) has no syscall socket snapshot.  Capture the current
        // credential at this FileLike boundary and use the same helper as the
        // send/sendmsg paths below.
        let current = current();
        let thread = current.as_thread();
        let actor = thread.current_cred();
        self.write_with_actor(src, &actor, thread.proc_data.proc.pid() as u32)
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
        file::FileLike,
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
            socket.recv_with_nonblocking(&mut dst, RecvFlags::empty(), true),
            Err(LinuxError::ENOBUFS.into())
        );
        assert_eq!(
            socket
                .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
                .unwrap()
                .len,
            1
        );

        let suppressed = route_socket();
        suppressed.set_option(NETLINK_NO_ENOBUFS, 1).unwrap();
        for _ in 0..=super::NETLINK_QUEUE_LIMIT {
            suppressed.enqueue_kernel(alloc::vec![1]);
        }
        assert_eq!(
            suppressed
                .recv_with_nonblocking(&mut dst, RecvFlags::empty(), true)
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
