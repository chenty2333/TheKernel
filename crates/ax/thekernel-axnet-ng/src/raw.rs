//! Namespace-local IPv4/IPv6 raw-IP sockets.
//!
//! The backing `smoltcp::socket::raw::Socket` is deliberately retained in the
//! normal namespace socket set.  That makes raw ingress, egress, readiness,
//! forked descriptors, and final close use the same ownership and poll pass as
//! TCP and UDP instead of opening an out-of-band packet path.

use alloc::sync::Arc;
use core::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axio::prelude::*;
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::{Mutex, MutexGuard};
use smoltcp::{
    iface::SocketHandle,
    socket::raw as smol,
    storage::PacketMetadata,
    wire::{IpAddress, IpProtocol, IpVersion},
};

use crate::{
    RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    buffer::{
        normalized_socket_buffer_size, try_filled_buffer, try_zeroed_socket_buffer,
        udp_packet_slots,
    },
    general::GeneralOptions,
    net_stack::NetStack,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

const RAW_BUFFER_BYTES: usize = 64 * 1024;

/// One raw socket incarnation's asynchronous egress completion sink. Route
/// plans retain only a `Weak` so a socket cannot be kept alive by a queued
/// datagram. The unique allocation is also the anti-ABA identity: a later
/// socket never shares this target with a reclaimed descriptor.
pub(crate) struct RawRouteCompletion {
    fault: AtomicU8,
    wake: Arc<PollSet>,
    handles: Mutex<alloc::vec::Vec<u32>>,
}

impl RawRouteCompletion {
    fn new(wake: Arc<PollSet>) -> Self {
        Self {
            fault: AtomicU8::new(0),
            wake,
            handles: Mutex::new(alloc::vec::Vec::new()),
        }
    }
    pub(crate) fn fail(&self, error: crate::options::SocketFault) {
        self.fault.store(error as u8, Ordering::Release);
        self.wake.wake();
    }
    fn take_fault(&self) -> Option<crate::options::SocketFault> {
        crate::options::SocketFault::from_raw(self.fault.swap(0, Ordering::AcqRel))
    }
    pub(crate) fn release_handle(&self, handle: u32) {
        let mut handles = self.handles.lock();
        if let Some(index) = handles.iter().position(|candidate| *candidate == handle) {
            handles.swap_remove(index);
        }
    }
    pub(crate) fn owns_handle(&self, handle: u32) -> bool {
        self.handles
            .lock()
            .iter()
            .any(|candidate| *candidate == handle)
    }
    fn drain_handles(&self) -> alloc::vec::Vec<u32> {
        core::mem::take(&mut *self.handles.lock())
    }
}

#[derive(Clone, Copy)]
struct RawEndpoints {
    local: Option<IpAddress>,
    peer: Option<IpAddress>,
    generation: u64,
}

impl RawEndpoints {
    const fn new() -> Self {
        Self {
            local: None,
            peer: None,
            generation: 0,
        }
    }

    fn publish(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("raw endpoint generation exhausted");
    }
}

#[derive(Clone, Copy)]
struct RawSendSnapshot {
    destination: IpAddress,
    bound_source: Option<IpAddress>,
    endpoint_generation: u64,
    header_included: bool,
}

fn packet_buffer() -> AxResult<smol::PacketBuffer<'static>> {
    let bytes = normalized_socket_buffer_size(RAW_BUFFER_BYTES);
    Ok(smol::PacketBuffer::new(
        try_filled_buffer(udp_packet_slots(bytes), PacketMetadata::EMPTY)?,
        try_zeroed_socket_buffer(bytes)?,
    ))
}

fn ip_version(family: RawSocketFamily) -> IpVersion {
    match family {
        RawSocketFamily::Ipv4 => IpVersion::Ipv4,
        RawSocketFamily::Ipv6 => IpVersion::Ipv6,
    }
}

fn socket_address(ip: IpAddress) -> SocketAddrEx {
    let ip = match ip {
        IpAddress::Ipv4(v4) => IpAddr::V4(core::net::Ipv4Addr::from(v4.octets())),
        IpAddress::Ipv6(v6) => IpAddr::V6(core::net::Ipv6Addr::from(v6.octets())),
    };
    SocketAddrEx::Ip(SocketAddr::new(ip, 0))
}

fn source_from_packet(packet: &[u8]) -> Option<IpAddress> {
    match packet.first()? >> 4 {
        4 if packet.len() >= 20 && packet[0] & 0x0f >= 5 => Some(IpAddress::v4(
            packet[12], packet[13], packet[14], packet[15],
        )),
        6 if packet.len() >= 40 => {
            let bytes: [u8; 16] = packet[8..24].try_into().ok()?;
            Some(IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from_octets(
                bytes,
            )))
        }
        _ => None,
    }
}

fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for word in header.chunks_exact(2) {
        sum = sum.wrapping_add(u16::from_be_bytes([word[0], word[1]]) as u32);
    }
    if let Some(&last) = header.chunks_exact(2).remainder().first() {
        sum = sum.wrapping_add((last as u32) << 8);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
    }
    !(sum as u16)
}

/// Fixed address family for a raw socket; it cannot change through dup/fork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawSocketFamily {
    Ipv4,
    Ipv6,
}

impl RawSocketFamily {
    fn accepts(self, address: IpAddress) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, IpAddress::Ipv4(_)) | (Self::Ipv6, IpAddress::Ipv6(_))
        )
    }
    fn unspecified(self) -> IpAddress {
        match self {
            Self::Ipv4 => IpAddress::v4(0, 0, 0, 0),
            Self::Ipv6 => IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 0),
        }
    }
}

/// A raw socket receives and transmits complete IP packets.  It either accepts
/// a caller-owned IP header (`IP_HDRINCL`) or creates only the outer IP
/// envelope; it never fabricates an L4 header or turns raw bytes into UDP.
pub struct RawSocket {
    stack: Arc<NetStack>,
    handle: SocketHandle,
    family: RawSocketFamily,
    protocol: u8,
    endpoints: Mutex<RawEndpoints>,
    rx_shutdown: AtomicBool,
    tx_shutdown: AtomicBool,
    header_included: AtomicBool,
    ipv4_ident: AtomicU16,
    poll_state: Arc<PollSet>,
    route_completion: Arc<RawRouteCompletion>,
    general: GeneralOptions,
}

impl RawSocket {
    pub fn new(stack: Arc<NetStack>, family: RawSocketFamily, protocol: u8) -> AxResult<Self> {
        let poll_state = Arc::new(PollSet::new());
        let route_completion = Arc::new(RawRouteCompletion::new(Arc::clone(&poll_state)));
        let mut socket = smol::Socket::new(
            Some(ip_version(family)),
            Some(IpProtocol::from(protocol)),
            packet_buffer()?,
            packet_buffer()?,
        );
        let weak_completion = Arc::downgrade(&route_completion);
        socket.set_transmit_discard_handler(Some(Arc::new(move |handle| {
            if let Some(completion) = weak_completion.upgrade() {
                completion.release_handle(handle);
                completion.fail(crate::options::SocketFault::Other);
            }
        })));
        let handle = stack.socket_set.add(socket)?;
        Ok(Self {
            stack,
            handle,
            family,
            protocol,
            endpoints: Mutex::new(RawEndpoints::new()),
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
            header_included: AtomicBool::new(false),
            ipv4_ident: AtomicU16::new(0),
            poll_state,
            route_completion,
            general: GeneralOptions::new(),
        })
    }

    pub const fn family(&self) -> RawSocketFamily {
        self.family
    }
    pub const fn protocol(&self) -> u8 {
        self.protocol
    }
    /// Arms the raw socket's device readiness directly into an aggregate
    /// registration.  Protocol dispatchers use this to wake their own users
    /// when shared raw ingress becomes readable, rather than depending on a
    /// later unrelated SCTP poll call to drain it.
    pub(crate) fn arm_dispatcher_readiness<'a>(
        &'a self,
        prepared: &mut PreparedPollRegistration<'a>,
        context: &Context<'_>,
    ) -> Result<(), PollRegistrationError> {
        prepared.arm(&self.poll_state, context.waker())?;
        self.stack
            .arm_readiness(prepared, self.general.device_mask(), context.waker())
    }
    fn with_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        self.stack
            .socket_set
            .with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }
    fn try_with_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> AxResult<R> {
        self.stack
            .socket_set
            .try_with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }
    fn validate(&self, addr: &SocketAddrEx) -> AxResult<IpAddress> {
        let addr = addr.clone().into_ip().map_err(|_| AxError::InvalidInput)?;
        let ip: IpAddress = addr.ip().into();
        self.family
            .accepts(ip)
            .then_some(ip)
            .ok_or(AxError::InvalidInput)
    }

    pub fn recv_pending_len(&self) -> AxResult<usize> {
        self.with_socket(|socket| {
            Ok(if socket.can_recv() {
                socket.peek().map(|packet| packet.len()).unwrap_or(0)
            } else {
                0
            })
        })
    }

    pub fn set_pending_error(&self, error: crate::options::SocketFault) {
        self.general.set_pending_error(error);
        self.poll_state.wake();
    }

    fn consume_route_failure(&self) {
        if let Some(error) = self.route_completion.take_fault() {
            self.general.set_pending_error(error);
        }
    }

    fn claim_route_handle_slot(
        &self,
        nowait: bool,
    ) -> AxResult<MutexGuard<'_, alloc::vec::Vec<u32>>> {
        let mut handles = if nowait {
            self.route_completion
                .handles
                .try_lock()
                .ok_or(AxError::WouldBlock)?
        } else {
            self.route_completion.handles.lock()
        };
        handles.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        Ok(handles)
    }

    pub fn disconnect(&self) {
        let mut endpoints = self.endpoints.lock();
        endpoints.peer = None;
        endpoints.publish();
        self.poll_state.wake();
    }

    /// Linux IP_HDRINCL is an OFD property and therefore remains shared by
    /// dup/fork holders of this socket.
    pub fn header_included(&self) -> bool {
        self.header_included.load(Ordering::Acquire)
    }
    pub fn set_header_included(&self, enabled: bool) {
        self.header_included.store(enabled, Ordering::Release);
    }
    /// A protocol dispatcher owns shared raw ingress and must remain armed on
    /// every device even before an individual endpoint has selected a route.
    pub(crate) fn set_dispatcher_device_mask(&self, mask: u64) {
        self.general.set_device_mask(mask);
    }
    /// Dispatcher-only full-IP output.  Route admission and enqueue happen in
    /// one raw-socket operation so protocol dispatchers do not preflight a
    /// policy and then send through an unrelated default raw socket policy.
    pub(crate) fn send_header_included_with_dont_route(
        &self,
        packet: &[u8],
        destination: SocketAddr,
        dont_route: bool,
    ) -> AxResult {
        self.stack.poll_interfaces();
        self.try_send_header_included_with_dont_route(packet, destination, dont_route)
    }

    /// Submit a dispatcher-owned full-IP packet without driving ingress or
    /// waiting for writable readiness.  Transport timer callbacks use this
    /// while serializing association state: polling here could synchronously
    /// dispatch a packet back into that association, while waiting would hold
    /// its state transition hostage to TX capacity.
    pub(crate) fn try_send_header_included_with_dont_route(
        &self,
        packet: &[u8],
        destination: SocketAddr,
        dont_route: bool,
    ) -> AxResult {
        let source = source_from_packet(packet).ok_or(AxError::InvalidInput)?;
        let destination: IpAddress = destination.ip().into();
        let source_std = match source {
            IpAddress::Ipv4(address) => IpAddr::V4(core::net::Ipv4Addr::from(address.octets())),
            IpAddress::Ipv6(address) => IpAddr::V6(core::net::Ipv6Addr::from(address.octets())),
        };
        let destination_std = match destination {
            IpAddress::Ipv4(address) => IpAddr::V4(core::net::Ipv4Addr::from(address.octets())),
            IpAddress::Ipv6(address) => IpAddr::V6(core::net::Ipv6Addr::from(address.octets())),
        };
        let mut permit = self.stack.acquire_packet_service();
        let mut handle_slot = self.claim_route_handle_slot(false)?;
        let (_, _, _, route_handle) = permit.reserve_raw_route(
            destination_std,
            Some(source_std),
            dont_route,
            true,
            Arc::downgrade(&self.route_completion),
        )?;
        let result = self.with_socket(|socket| {
            if !socket.can_send() {
                return Err(AxError::WouldBlock);
            }
            socket
                .send_slice_with_metadata(packet, route_handle)
                .map_err(|_| AxError::WouldBlock)
        });
        if result.is_err() {
            permit.discard_raw_route(route_handle);
        } else {
            handle_slot.push(route_handle);
        }
        // Router completion/discard paths take this lock while servicing the
        // newly published queue entry. Never carry it into service release or
        // deferred polling, which may synchronously reach those paths.
        drop(handle_slot);
        drop(permit);
        if result.is_ok() {
            self.stack.schedule_deferred_packet_poll();
        }
        result
    }

    fn snapshot_send(
        &self,
        requested: Option<IpAddress>,
        nowait: bool,
    ) -> AxResult<RawSendSnapshot> {
        let endpoints = if nowait {
            *self.endpoints.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            *self.endpoints.lock()
        };
        Ok(RawSendSnapshot {
            destination: requested.or(endpoints.peer).ok_or(AxError::NotConnected)?,
            bound_source: endpoints.local.filter(|source| !source.is_unspecified()),
            endpoint_generation: endpoints.generation,
            header_included: self.header_included(),
        })
    }

    fn admit_raw_outbound(
        &self,
        snapshot: RawSendSnapshot,
        nowait: bool,
    ) -> AxResult<(IpAddr, u64, u64)> {
        let destination: IpAddr = match snapshot.destination {
            IpAddress::Ipv4(address) => IpAddr::V4(core::net::Ipv4Addr::from(address.octets())),
            IpAddress::Ipv6(address) => IpAddr::V6(core::net::Ipv6Addr::from(address.octets())),
        };
        let bound_source = snapshot.bound_source.map(|source| match source {
            IpAddress::Ipv4(address) => IpAddr::V4(core::net::Ipv4Addr::from(address.octets())),
            IpAddress::Ipv6(address) => IpAddr::V6(core::net::Ipv6Addr::from(address.octets())),
        });
        if nowait {
            self.stack
                .try_acquire_packet_service()?
                .resolve_raw_outbound(destination, bound_source, self.general.dont_route())
        } else {
            self.stack.acquire_packet_service().resolve_raw_outbound(
                destination,
                bound_source,
                self.general.dont_route(),
            )
        }
    }

    fn packet_for_send(
        &self,
        payload: &[u8],
        snapshot: RawSendSnapshot,
        route_source: IpAddr,
    ) -> AxResult<alloc::vec::Vec<u8>> {
        // A complete IP packet is the native smoltcp raw representation.  It
        // is also what Linux sends with IP_HDRINCL.  For ordinary raw sends,
        // build the exact minimal IP envelope here from the connected/sendto
        // destination, preserving raw L4 bytes without routing them through
        // a UDP/TCP helper.
        let expected_version = match self.family {
            RawSocketFamily::Ipv4 => 4,
            RawSocketFamily::Ipv6 => 6,
        };
        if snapshot.header_included {
            if !payload
                .first()
                .is_some_and(|first| first >> 4 == expected_version)
            {
                return Err(AxError::InvalidInput);
            }
            let mut packet = payload.to_vec();
            match (self.family, snapshot.destination, route_source) {
                (RawSocketFamily::Ipv4, IpAddress::Ipv4(_destination), IpAddr::V4(source)) => {
                    if packet.len() < 20 {
                        return Err(AxError::InvalidInput);
                    }
                    let header_len = (packet[0] as usize & 0x0f) * 4;
                    if header_len < 20 || header_len > packet.len() {
                        return Err(AxError::InvalidInput);
                    }
                    if packet[9] != self.protocol {
                        return Err(AxError::InvalidInput);
                    }
                    if packet.len() > u16::MAX as usize {
                        return Err(AxError::OutOfRange);
                    }
                    // raw_send_hdrinc owns these two fields: Linux replaces
                    // a caller-supplied total/checksum rather than accepting
                    // an inconsistent value as an alternate packet length.
                    let packet_len = packet.len() as u16;
                    packet[2..4].copy_from_slice(&packet_len.to_be_bytes());
                    if packet[4..6] == [0, 0] {
                        let ident = self
                            .ipv4_ident
                            .fetch_add(1, Ordering::AcqRel)
                            .wrapping_add(1);
                        packet[4..6].copy_from_slice(&ident.to_be_bytes());
                    }
                    if packet[12..16] == [0, 0, 0, 0] {
                        packet[12..16].copy_from_slice(&source.octets());
                    }
                    packet[10..12].fill(0);
                    let checksum = ip_checksum(&packet[..header_len]);
                    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
                }
                (RawSocketFamily::Ipv6, IpAddress::Ipv6(_destination), IpAddr::V6(source)) => {
                    if packet.len() < 40 {
                        return Err(AxError::InvalidInput);
                    }
                    if packet[6] != self.protocol {
                        return Err(AxError::InvalidInput);
                    }
                    if u16::from_be_bytes([packet[4], packet[5]]) as usize != packet.len() - 40 {
                        return Err(AxError::InvalidInput);
                    }
                    if packet[8..24].iter().all(|byte| *byte == 0) {
                        packet[8..24].copy_from_slice(&source.octets());
                    }
                }
                _ => return Err(AxError::InvalidInput),
            }
            return Ok(packet);
        }
        let destination = snapshot.destination;
        let source = match route_source {
            IpAddr::V4(source) => IpAddress::Ipv4(source.into()),
            IpAddr::V6(source) => IpAddress::Ipv6(source.into()),
        };
        let header_len: usize = if self.family == RawSocketFamily::Ipv4 {
            20
        } else {
            40
        };
        let total = header_len
            .checked_add(payload.len())
            .ok_or(AxError::OutOfRange)?;
        if (self.family == RawSocketFamily::Ipv4 && total > u16::MAX as usize)
            || (self.family == RawSocketFamily::Ipv6 && payload.len() > u16::MAX as usize)
        {
            return Err(AxError::OutOfRange);
        }
        let mut packet = alloc::vec::Vec::new();
        packet
            .try_reserve_exact(total)
            .map_err(|_| AxError::NoMemory)?;
        packet.resize(total, 0);
        match (self.family, source, destination) {
            (RawSocketFamily::Ipv4, IpAddress::Ipv4(src), IpAddress::Ipv4(dst)) => {
                packet[0] = 0x45;
                packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
                packet[8] = 64;
                packet[9] = self.protocol;
                packet[12..16].copy_from_slice(&src.octets());
                packet[16..20].copy_from_slice(&dst.octets());
                let checksum = ip_checksum(&packet[..20]);
                packet[10..12].copy_from_slice(&checksum.to_be_bytes());
            }
            (RawSocketFamily::Ipv6, IpAddress::Ipv6(src), IpAddress::Ipv6(dst)) => {
                packet[0] = 0x60;
                packet[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
                packet[6] = self.protocol;
                packet[7] = 64;
                packet[8..24].copy_from_slice(&src.octets());
                packet[24..40].copy_from_slice(&dst.octets());
            }
            _ => return Err(AxError::InvalidInput),
        }
        packet[header_len..].copy_from_slice(payload);
        Ok(packet)
    }
}

impl Configurable for RawSocket {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }
    fn get_option_inner(&self, option: &mut GetSocketOption) -> AxResult<bool> {
        self.general.get_option_inner(option)
    }
    fn set_option_inner(&self, option: SetSocketOption) -> AxResult<bool> {
        self.general.set_option_inner(option)
    }
}

impl SocketOps for RawSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult {
        let ip = self.validate(&local_addr)?;
        let mut endpoints = self.endpoints.lock();
        if endpoints.local.is_some() {
            return Err(AxError::InvalidInput);
        }
        endpoints.local = Some(ip);
        endpoints.publish();
        self.poll_state.wake();
        Ok(())
    }
    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let ip = self.validate(&remote_addr)?;
        // Raw sockets still need the normal routing decision.  In particular,
        // transports built directly on raw IP (SCTP and DCCP) must never emit
        // an unspecified source address merely because they own their L4
        // ports outside smoltcp's TCP/UDP tables.
        let bound = self.endpoints.lock().local;
        let route = self
            .stack
            .get_service()
            .resolve_outbound_with_dont_route(&ip, bound, self.general.dont_route())
            .map_err(crate::service::RouteReject::as_ax_error)?;
        let mut endpoints = self.endpoints.lock();
        if endpoints.local.is_none() {
            endpoints.local = Some(route.src_addr);
        }
        endpoints.peer = Some(ip);
        endpoints.publish();
        self.general.set_device_mask(route.device_mask);
        self.poll_state.wake();
        Ok(())
    }
    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        self.consume_route_failure();
        if self.tx_shutdown.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        let requested = options
            .to
            .as_ref()
            .map(|to| self.validate(to))
            .transpose()?;
        let len = src.remaining();
        let nowait = options.effective_nonblocking(self.nonblocking());
        let snapshot = self.snapshot_send(requested, nowait)?;
        // Route errors intentionally precede copy faults, matching raw_sendmsg:
        // policy is admitted from the operation snapshot before user memory is
        // touched.  This permit is dropped before copyin.
        let _ = self.admit_raw_outbound(snapshot, nowait)?;
        let mut payload = alloc::vec::Vec::new();
        payload
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        payload.resize(len, 0);
        src.read_exact(&mut payload)?;
        let (initial_source, initial_device_mask, initial_generation) =
            self.admit_raw_outbound(snapshot, nowait)?;
        // Validate the complete caller-owned header before entering the final
        // service domain.  The final pass below rebuilds from `payload` if a
        // route changed, so a zero Linux-owned field never leaks between two
        // admissions.
        let initial_packet = self.packet_for_send(&payload, snapshot, initial_source)?;
        self.general.send_poller_with_effective_nonblocking(
            self,
            options.effective_nonblocking(self.nonblocking()),
            || {
                if !nowait {
                    self.stack.poll_interfaces();
                }
                let endpoints = if nowait {
                    self.endpoints.try_lock().ok_or(AxError::WouldBlock)?
                } else {
                    self.endpoints.lock()
                };
                if endpoints.generation != snapshot.endpoint_generation {
                    return Err(AxError::WouldBlock);
                }
                let destination: IpAddr = match snapshot.destination {
                    IpAddress::Ipv4(address) => {
                        IpAddr::V4(core::net::Ipv4Addr::from(address.octets()))
                    }
                    IpAddress::Ipv6(address) => {
                        IpAddr::V6(core::net::Ipv6Addr::from(address.octets()))
                    }
                };
                let bound_source = snapshot.bound_source.map(|source| match source {
                    IpAddress::Ipv4(address) => {
                        IpAddr::V4(core::net::Ipv4Addr::from(address.octets()))
                    }
                    IpAddress::Ipv6(address) => {
                        IpAddr::V6(core::net::Ipv6Addr::from(address.octets()))
                    }
                });
                let mut permit = if nowait {
                    self.stack.try_acquire_packet_service()?
                } else {
                    self.stack.acquire_packet_service()
                };
                let mut handle_slot = self.claim_route_handle_slot(nowait)?;
                let (source, device_mask, final_generation, route_handle) = permit
                    .reserve_raw_route(
                        destination,
                        bound_source,
                        self.general.dont_route(),
                        snapshot.header_included,
                        Arc::downgrade(&self.route_completion),
                    )?;
                // A changed route is not reused from the pre-copy admission: the
                // held permit makes this freshly resolved generation and the
                // queue publication one transaction.  Rebuild below, including
                // Linux-owned header fields, from the original copied bytes.
                let rebuilt_packet;
                let packet = if final_generation == initial_generation
                    && source == initial_source
                    && device_mask == initial_device_mask
                {
                    &initial_packet[..]
                } else {
                    rebuilt_packet = self.packet_for_send(&payload, snapshot, source)?;
                    &rebuilt_packet[..]
                };
                let submit = |socket: &mut smol::Socket| {
                    if !socket.can_send() {
                        return Err(AxError::WouldBlock);
                    }
                    socket
                        .send_slice_with_metadata(packet, route_handle)
                        .map_err(|_| AxError::WouldBlock)?;
                    Ok(len)
                };
                let result = if nowait {
                    self.try_with_socket(submit)?
                } else {
                    self.with_socket(submit)
                };
                if result.is_err() {
                    // `send_slice_with_metadata` either publishes the complete
                    // entry or leaves the socket queue untouched.  Roll back the
                    // matching plan on the latter path.
                    permit.discard_raw_route(route_handle);
                } else {
                    handle_slot.push(route_handle);
                }
                // The packet is now published. Release the completion registry
                // before releasing the service permit or driving/defering egress:
                // either action can consume this handle synchronously.
                drop(handle_slot);
                let quarantine_edge = if result.is_ok() && !nowait {
                    // The route permit remains live through the first egress
                    // pass, so a route/topology writer cannot turn this accepted
                    // packet into a later router-side source/device drop.
                    permit.publish_raw_egress()
                } else {
                    false
                };
                drop(permit);
                drop(endpoints);
                if result.is_ok() {
                    if quarantine_edge {
                        self.stack.publish_quarantine_edge();
                    }
                    self.general.set_device_mask(device_mask);
                    // Mirror UDP's post-publication path.  It runs only after
                    // dropping both endpoint/service ownership, so loopback
                    // delivery and router hooks cannot recurse on this send.
                    if !nowait {
                        let status = self.stack.poll_interfaces();
                        if status.is_continuation() {
                            self.stack.poll_interfaces();
                        }
                    } else {
                        self.stack.schedule_deferred_packet_poll();
                    }
                    self.poll_state.wake();
                }
                result
            },
        )
    }
    fn recv(
        &self,
        mut dst: impl Write + IoBufMut,
        mut options: RecvOptions<'_>,
    ) -> AxResult<usize> {
        self.consume_route_failure();
        if self.rx_shutdown.load(Ordering::Acquire) {
            return Ok(0);
        }
        let flags = options.flags;
        self.general.recv_poller_with_effective_nonblocking(
            self,
            options.effective_nonblocking(self.nonblocking()),
            || {
                self.stack.poll_interfaces();
                self.with_socket(|socket| {
                    if !socket.can_recv() {
                        return Err(AxError::WouldBlock);
                    }
                    let packet = if flags.contains(RecvFlags::PEEK) {
                        socket.peek().map_err(|_| AxError::WouldBlock)?
                    } else {
                        socket.recv().map_err(|_| AxError::WouldBlock)?
                    };
                    if let Some(from) = options.from.as_deref_mut() {
                        *from = socket_address(
                            source_from_packet(packet)
                                .or(self.endpoints.lock().peer)
                                .unwrap_or_else(|| self.family.unspecified()),
                        );
                    }
                    let copied = dst.write(packet)?;
                    self.poll_state.wake();
                    Ok(if flags.contains(RecvFlags::TRUNCATE) {
                        packet.len()
                    } else {
                        copied
                    })
                })
            },
        )
    }
    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(socket_address(
            self.endpoints
                .lock()
                .local
                .unwrap_or_else(|| self.family.unspecified()),
        ))
    }
    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        self.endpoints
            .lock()
            .peer
            .map(socket_address)
            .ok_or(AxError::NotConnected)
    }
    fn shutdown(&self, how: Shutdown) -> AxResult {
        if how.has_read() {
            self.rx_shutdown.store(true, Ordering::Release);
        }
        if how.has_write() {
            self.tx_shutdown.store(true, Ordering::Release);
        }
        self.poll_state.wake();
        Ok(())
    }
}

impl Pollable for RawSocket {
    fn poll(&self) -> IoEvents {
        self.consume_route_failure();
        self.stack.poll_interfaces();
        let mut events = IoEvents::empty();
        self.with_socket(|socket| {
            events.set(
                IoEvents::READABLE,
                self.rx_shutdown.load(Ordering::Acquire) || socket.can_recv(),
            );
            events.set(
                IoEvents::WRITABLE,
                !self.tx_shutdown.load(Ordering::Acquire) && socket.can_send(),
            );
            events.set(
                IoEvents::READ_HANGUP,
                self.rx_shutdown.load(Ordering::Acquire),
            );
        });
        self.general
            .add_pending_error_event(self.stack.add_terminal_events(events))
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let mut prepared = PreparedPollRegistration::try_new(2)?;
        prepared.arm(&self.poll_state, context.waker())?;
        self.stack
            .arm_readiness(&mut prepared, self.general.device_mask(), context.waker())?;
        prepared.commit()
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        // Final OFD close owns raw queue destruction. Enumerate its exact
        // metadata first, releasing every corresponding route lease before
        // removing the smoltcp socket. Completion targets are weak, so no
        // callback can resurrect this socket during teardown.
        let mut permit = self.stack.acquire_packet_service();
        self.with_socket(|socket| {
            socket.discard_queued_transmit_metadata(|handle| permit.discard_raw_route(handle));
        });
        // The queue may already have handed an entry to the router TX queue.
        // Drop all handles ever published by this socket as well; router-side
        // removal is idempotent and an already detached entry becomes an
        // explicit stale-token discard instead of a route-plan lease leak.
        for handle in self.route_completion.drain_handles() {
            permit.discard_raw_route(handle);
        }
        self.stack.socket_set.remove(self.handle);
    }
}
