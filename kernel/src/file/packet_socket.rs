//! Linux AF_PACKET adapter over the generic bounded packet broker.
//!
//! This file owns only Layer 3 glue: namespace retention, lower-endpoint
//! publication, errno conversion, ordinary queue copies, and file readiness.
//! Linux value/state rules remain in `thekernel-linux-packet`; packet capture,
//! injection, queue budgets, and wake registration remain in `axnet-ng`.
//! TPACKET rings, fanout, mmap, and statistics are deliberately not represented
//! by this baseline instead of being exposed as placeholder success.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axnet::{
    InterfaceKind,
    packet::{
        LinkHardwareType, LinkPacketType, MAX_PACKET_FRAME_BYTES, PacketDeviceCapabilities,
        PacketEndpoint, PacketMetadata, PacketProtocol, PacketSelector, PacketSendRequest,
        PacketView as EndpointPacketView,
    },
};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use thekernel_linux_packet::{
    FrameLayout, InterfaceIndex, LinkLayerAddress, LinkLayerInfo, PacketBindRequest, PacketBinding,
    PacketError, PacketSocketState, PacketSocketType, PacketType, ProtocolSelector, ReceiveFlags,
    SockAddrLl,
};

use super::{
    FileLike, IoDst, IoSrc, Kstat, PseudoInode, packet::socket_ifreq_ioctl, try_pseudo_inode_path,
};
use crate::{readiness::block_on_poll_io, task::NetworkNamespace};

#[cfg(test)]
extern crate std;

const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

/// Completed ordinary receive together with the address and truncation facts
/// needed by a later `recvfrom`/`recvmsg` userspace adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PacketReceiveResult {
    copied_len: usize,
    returned_len: usize,
    message_truncated: bool,
    address: SockAddrLl,
}

impl PacketReceiveResult {
    pub(crate) const fn copied_len(self) -> usize {
        self.copied_len
    }

    pub(crate) const fn returned_len(self) -> usize {
        self.returned_len
    }

    pub(crate) const fn message_truncated(self) -> bool {
        self.message_truncated
    }

    pub(crate) const fn address(self) -> SockAddrLl {
        self.address
    }
}

struct PacketSendPlan {
    interface_index: u32,
    socket_type: PacketSocketType,
    protocol: u16,
    destination: [u8; 8],
    destination_len: usize,
}

/// One AF_PACKET open-file backend.
///
/// `state` is the authoritative Linux bind/option state. `endpoint` owns the
/// bounded lower queue and readiness source. Ordinary receive claims its queue
/// record before usercopy, while `MSG_PEEK` takes a retained clone; this matches
/// Linux's distinct EFAULT consumption behavior without an OFD-wide recv lock.
pub(crate) struct PacketSocket {
    net_ns: Arc<NetworkNamespace>,
    endpoint: Arc<PacketEndpoint>,
    state: Mutex<PacketSocketState>,
    nonblocking: AtomicBool,
    inode: PseudoInode,
}

impl PacketSocket {
    /// Allocates a Linux state object and its namespace-local bounded endpoint
    /// before any descriptor becomes visible.
    pub(crate) fn try_new(
        socket_type: PacketSocketType,
        protocol: ProtocolSelector,
        net_ns: Arc<NetworkNamespace>,
    ) -> AxResult<Arc<Self>> {
        let state = PacketSocketState::new(socket_type, protocol);
        let endpoint = net_ns
            .stack()
            .subscribe_packets(selector_for_state(&state))?;
        Arc::try_new(Self {
            net_ns,
            endpoint,
            state: Mutex::new(state),
            nonblocking: AtomicBool::new(false),
            inode: PseudoInode::socket(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub(crate) fn binding(&self) -> PacketBinding {
        self.state.lock().binding()
    }

    /// Publishes a bind as `validate device -> lower selector -> ABI state`.
    ///
    /// The state mutex excludes another adapter transition between prepare and
    /// publish. Therefore the final publication cannot become stale; if lower
    /// selector admission fails, live Linux state remains unchanged.
    pub(crate) fn bind(&self, request: PacketBindRequest) -> AxResult<()> {
        let mut state = self.state.lock();
        let plan = state.prepare_bind(request).map_err(packet_error)?;
        let replacement = plan.replacement();
        validate_receive_device(&self.net_ns, state.socket_type(), replacement.interface())?;

        if !plan.is_noop() {
            let selector =
                selector_for_binding(state.socket_type(), replacement, state.ignore_outgoing());
            self.endpoint.set_selector(selector)?;
        }

        state.publish_bind(plan).map_err(|error| {
            debug_assert_eq!(error, PacketError::StaleBindPlan);
            AxError::BadState
        })?;
        Ok(())
    }

    /// Returns a coherent name from the live binding and a matching interface
    /// snapshot. No userspace pointer or device reference escapes this method.
    pub(crate) fn get_name(&self) -> AxResult<SockAddrLl> {
        let state = self.state.lock();
        let interface = state.binding().interface();
        let link = if interface.is_any() {
            None
        } else {
            let raw = exact_interface(interface)?;
            let info = self
                .net_ns
                .stack()
                .interfaces()
                .into_iter()
                .find(|candidate| candidate.index == raw)
                .ok_or(AxError::NoSuchDevice)?;
            let mut bytes = [0_u8; 8];
            let address_len = match info.hardware_address {
                Some(address) => {
                    bytes[..address.len()].copy_from_slice(&address);
                    address.len() as u8
                }
                None => 0,
            };
            let address = LinkLayerAddress::new(bytes, address_len).map_err(packet_error)?;
            Some(
                LinkLayerInfo::new(interface, hardware_type_for_kind(info.kind), address)
                    .map_err(packet_error)?,
            )
        };
        state.get_name(link).map_err(packet_error)
    }

    /// Performs one ordinary queue receive. `nonblocking` is an OFD snapshot;
    /// `MSG_DONTWAIT` remains a syscall-layer override and is folded into that
    /// value by the future `recvmsg` adapter.
    pub(crate) fn recv_with_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: ReceiveFlags,
        nonblocking: bool,
    ) -> AxResult<PacketReceiveResult> {
        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let peek = flags.contains(ReceiveFlags::PEEK);
            // Linux ordinary packet receive dequeues before usercopy: EFAULT
            // consumes the record. MSG_PEEK clones the head and therefore
            // retains it across the same fault.
            let record = self.endpoint.try_receive(peek)?;
            let metadata = record.metadata();
            let socket_type = self.state.lock().socket_type();
            let header_len = usize::from(metadata.link_header_len);
            let frame_len = match socket_type {
                PacketSocketType::Raw => record.wire_len(),
                PacketSocketType::Datagram => record
                    .wire_len()
                    .checked_add(header_len)
                    .ok_or(AxError::InvalidInput)?,
            };
            let view = FrameLayout::new(frame_len, header_len)
                .and_then(|layout| layout.captured_view(socket_type, record.data().len()))
                .map_err(packet_error)?;
            let decision = view.receive_decision(dst.remaining_mut(), flags);
            // Layer 1 must claim atomically in `try_receive`; a peek followed
            // by a separate destructive call would race another OFD reader.
            // Keep that atomic choice checked against the Layer 2 contract.
            debug_assert_eq!(decision.queue_disposition().claims_before_copy(), !peek);
            let copied = dst.write(&record.data()[..decision.copy_len()])?;
            if copied != decision.copy_len() {
                return Err(AxError::BadState);
            }

            Ok(PacketReceiveResult {
                copied_len: copied,
                returned_len: decision.returned_len(),
                message_truncated: decision.message_truncated(),
                address: address_from_metadata(metadata)?,
            })
        })
    }

    /// Sends one already-copied ordinary RAW frame or cooked DGRAM payload.
    ///
    /// Layer 1 does not yet expose device completion credits or writable
    /// admission readiness. Consequently blocking and nonblocking sends share
    /// this single attempt and a racing lower `WouldBlock` is returned as-is;
    /// ring transmission, retry, and deferred completion are outside this
    /// baseline.
    pub(crate) fn send_with_nonblocking(
        &self,
        payload: &[u8],
        destination: Option<SockAddrLl>,
        _nonblocking: bool,
    ) -> AxResult<usize> {
        if payload.len() > MAX_PACKET_FRAME_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let plan = self.prepare_send(payload.len(), destination)?;
        let request = match plan.socket_type {
            PacketSocketType::Raw => PacketSendRequest::Raw { frame: payload },
            PacketSocketType::Datagram => PacketSendRequest::Cooked {
                protocol: plan.protocol,
                destination: &plan.destination[..plan.destination_len],
                payload,
            },
        };
        self.net_ns
            .stack()
            .send_packet(plan.interface_index, self.endpoint.id(), request)?;
        Ok(payload.len())
    }

    fn prepare_send(
        &self,
        payload_len: usize,
        destination: Option<SockAddrLl>,
    ) -> AxResult<PacketSendPlan> {
        let state = self.state.lock();
        let socket_type = state.socket_type();
        let binding = state.binding();
        drop(state);

        let selected_interface = destination
            .map(SockAddrLl::interface)
            .filter(|interface| !interface.is_any())
            .unwrap_or(binding.interface());
        let interface_index =
            exact_interface(selected_interface).map_err(|_| AxError::from(LinuxError::ENXIO))?;
        let info = self
            .net_ns
            .stack()
            .interfaces()
            .into_iter()
            .find(|candidate| candidate.index == interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
        let capabilities = self
            .net_ns
            .stack()
            .packet_device_capabilities(interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;

        match socket_type {
            PacketSocketType::Raw if !capabilities.raw_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            PacketSocketType::Datagram if !capabilities.cooked_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            _ => {}
        }
        let max_len = match socket_type {
            PacketSocketType::Raw => info
                .mtu
                .checked_add(usize::from(capabilities.link_header_len))
                .ok_or(AxError::InvalidInput)?,
            PacketSocketType::Datagram => info.mtu,
        };
        if payload_len > max_len {
            return Err(LinuxError::EMSGSIZE.into());
        }

        let protocol = destination
            .map(SockAddrLl::protocol)
            .filter(|protocol| *protocol != ProtocolSelector::Disabled)
            .unwrap_or(binding.protocol())
            .host_order();
        let mut address = [0_u8; 8];
        let destination_address = destination.map(SockAddrLl::address);
        let destination_len =
            if let Some(link) = destination_address.filter(|link| !link.is_empty()) {
                if link.len() != capabilities.address_len {
                    return Err(AxError::InvalidInput);
                }
                address = link.padded_bytes();
                usize::from(link.len())
            } else {
                default_destination(&mut address, info.hardware_address, capabilities)?
            };

        Ok(PacketSendPlan {
            interface_index,
            socket_type,
            protocol,
            destination: address,
            destination_len,
        })
    }
}

impl FileLike for PacketSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.recv_with_nonblocking(dst, ReceiveFlags::EMPTY, self.nonblocking())
            .map(PacketReceiveResult::returned_len)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let len = src.remaining();
        if len > MAX_PACKET_FRAME_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        payload.resize(len, 0);
        // One file write is one packet. A source that violates its advertised
        // remaining length must fail instead of publishing a truncated frame.
        src.read_exact(&mut payload)?;
        self.send_with_nonblocking(&payload, None, self.nonblocking())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        socket_ifreq_ioctl(self.net_ns.stack(), cmd, arg)
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }
}

impl Pollable for PacketSocket {
    fn poll(&self) -> IoEvents {
        // READABLE is queue-backed. WRITABLE is deliberately optimistic until
        // Layer 1 grows a device completion-credit readiness contract.
        self.endpoint.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        self.endpoint.register(context, events)
    }
}

fn selector_for_state(state: &PacketSocketState) -> PacketSelector {
    selector_for_binding(
        state.socket_type(),
        state.binding(),
        state.ignore_outgoing(),
    )
}

fn selector_for_binding(
    socket_type: PacketSocketType,
    binding: PacketBinding,
    ignore_outgoing: bool,
) -> PacketSelector {
    let protocol = match binding.protocol() {
        ProtocolSelector::Disabled => PacketProtocol::Disabled,
        ProtocolSelector::All => PacketProtocol::All,
        ProtocolSelector::Exact(protocol) => PacketProtocol::Exact(protocol.host_order()),
    };
    let interface = match binding.interface() {
        InterfaceIndex::Any => None,
        exact => Some(exact.raw() as u32),
    };
    let view = match socket_type {
        PacketSocketType::Raw => EndpointPacketView::Raw,
        PacketSocketType::Datagram => EndpointPacketView::Cooked,
    };
    PacketSelector::new(
        protocol,
        interface,
        view,
        binding.protocol() == ProtocolSelector::All && !ignore_outgoing,
    )
}

fn validate_receive_device(
    net_ns: &NetworkNamespace,
    socket_type: PacketSocketType,
    interface: InterfaceIndex,
) -> AxResult<()> {
    if interface.is_any() {
        return Ok(());
    }
    let capabilities = net_ns
        .stack()
        .packet_device_capabilities(exact_interface(interface)?)
        .ok_or(AxError::NoSuchDevice)?;
    match socket_type {
        PacketSocketType::Raw if capabilities.raw_receive => Ok(()),
        PacketSocketType::Datagram if capabilities.cooked_receive => Ok(()),
        _ => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

fn exact_interface(interface: InterfaceIndex) -> AxResult<u32> {
    u32::try_from(interface.raw())
        .ok()
        .filter(|index| *index != 0)
        .ok_or(AxError::InvalidInput)
}

fn default_destination(
    output: &mut [u8; 8],
    hardware_address: Option<[u8; 6]>,
    capabilities: PacketDeviceCapabilities,
) -> AxResult<usize> {
    let len = usize::from(capabilities.address_len);
    if len > output.len() {
        return Err(AxError::InvalidInput);
    }
    if let Some(address) = hardware_address {
        if address.len() != len {
            return Err(AxError::BadState);
        }
        output[..len].copy_from_slice(&address);
    }
    Ok(len)
}

const fn hardware_type_for_kind(kind: InterfaceKind) -> u16 {
    match kind {
        InterfaceKind::Loopback => ARPHRD_LOOPBACK,
        InterfaceKind::Ethernet => ARPHRD_ETHER,
    }
}

const fn hardware_type(metadata: PacketMetadata) -> u16 {
    match metadata.hardware_type {
        LinkHardwareType::Ethernet => ARPHRD_ETHER,
        LinkHardwareType::Loopback => ARPHRD_LOOPBACK,
    }
}

const fn packet_type(packet_type: LinkPacketType) -> PacketType {
    match packet_type {
        LinkPacketType::Host => PacketType::HOST,
        LinkPacketType::Broadcast => PacketType::BROADCAST,
        LinkPacketType::Multicast => PacketType::MULTICAST,
        LinkPacketType::OtherHost => PacketType::OTHER_HOST,
        LinkPacketType::Outgoing => PacketType::OUTGOING,
    }
}

fn address_from_metadata(metadata: PacketMetadata) -> AxResult<SockAddrLl> {
    let interface = InterfaceIndex::exact(metadata.interface_index).map_err(packet_error)?;
    let address =
        LinkLayerAddress::new(metadata.address, metadata.address_len).map_err(packet_error)?;
    Ok(SockAddrLl::new(
        interface,
        ProtocolSelector::from_host_order(metadata.protocol),
        hardware_type(metadata),
        packet_type(metadata.packet_type),
        address,
    ))
}

pub(crate) fn packet_error(error: PacketError) -> AxError {
    match error {
        PacketError::UnsupportedSocketType => LinuxError::ESOCKTNOSUPPORT.into(),
        PacketError::InvalidAddressFamily => LinuxError::EAFNOSUPPORT.into(),
        PacketError::UnsupportedReceiveFlags
        | PacketError::UnknownPacketOption
        | PacketError::UnsupportedPacketOption { .. } => LinuxError::EOPNOTSUPP.into(),
        PacketError::MissingLinkLayerInfo => AxError::NoSuchDevice,
        PacketError::StaleBindPlan | PacketError::LinkLayerInfoMismatch => AxError::BadState,
        PacketError::BindGenerationExhausted => LinuxError::EOVERFLOW.into(),
        PacketError::InvalidExactProtocol
        | PacketError::InvalidInterfaceIndex
        | PacketError::InvalidHardwareAddressLength
        | PacketError::InvalidBindingGeneration
        | PacketError::InvalidFrameLayout
        | PacketError::InvalidCapturedLength => AxError::InvalidInput,
        _ => AxError::InvalidInput,
    }
}

#[cfg(test)]
static PACKET_TEST_INIT: std::sync::Once = std::sync::Once::new();
#[cfg(test)]
static PACKET_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes host tests that share the emulated primary-CPU current-task slot.
#[cfg(test)]
pub(crate) fn packet_test_context() -> std::sync::MutexGuard<'static, ()> {
    let guard = PACKET_TEST_SERIAL
        .lock()
        .expect("packet test runtime lock poisoned");
    PACKET_TEST_INIT.call_once(|| {
        if let Err(error) = axtask::init_scheduler() {
            assert!(
                axtask::current_may_uninit().is_some(),
                "host scheduler initialization failed: {error:?}"
            );
        }
    });
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::UserNamespace;

    struct FaultDst {
        remaining: usize,
    }

    struct ShortSrc {
        bytes: &'static [u8],
        offset: usize,
        advertised: usize,
    }

    impl Write for FaultDst {
        fn write(&mut self, _buf: &[u8]) -> AxResult<usize> {
            Err(AxError::BadAddress)
        }

        fn flush(&mut self) -> AxResult<()> {
            Ok(())
        }
    }

    impl IoBufMut for FaultDst {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    impl Read for ShortSrc {
        fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
            let source = &self.bytes[self.offset..];
            let copied = source.len().min(output.len());
            output[..copied].copy_from_slice(&source[..copied]);
            self.offset += copied;
            Ok(copied)
        }
    }

    impl IoBuf for ShortSrc {
        fn remaining(&self) -> usize {
            self.advertised
        }
    }

    fn namespace() -> Arc<NetworkNamespace> {
        NetworkNamespace::try_new_loopback_only(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    fn loopback_address(protocol: ProtocolSelector) -> SockAddrLl {
        SockAddrLl::new(
            InterfaceIndex::exact(1).unwrap(),
            protocol,
            ARPHRD_LOOPBACK,
            PacketType::HOST,
            LinkLayerAddress::new([0; 8], 6).unwrap(),
        )
    }

    fn raw_ipv4_frame() -> [u8; 34] {
        let mut frame = [0_u8; 34];
        frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&20_u16.to_be_bytes());
        frame[22] = 64;
        frame
    }

    #[test]
    fn namespace_lifetime_and_exact_bind_are_owned_by_the_adapter() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let weak = Arc::downgrade(&net_ns);
        let socket = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::Disabled,
            net_ns.clone(),
        )
        .unwrap();
        drop(net_ns);
        assert!(weak.upgrade().is_some());

        let request = PacketBindRequest::new(
            InterfaceIndex::exact(1).unwrap(),
            ProtocolSelector::from_host_order(0x0800),
        );
        socket.bind(request).unwrap();
        assert_eq!(socket.binding().interface().raw(), 1);
        let name = socket.get_name().unwrap();
        assert_eq!(name.interface().raw(), 1);
        assert_eq!(name.protocol().host_order(), 0x0800);
        assert_eq!(name.hardware_type(), ARPHRD_LOOPBACK);
        assert_eq!(name.address().as_bytes(), &[0; 6]);

        drop(socket);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn linux_selector_exposes_outgoing_only_to_eth_p_all() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let all =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let exact = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::from_host_order(0x0800),
            net_ns.clone(),
        )
        .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();

        sender
            .send_with_nonblocking(
                &raw_ipv4_frame(),
                Some(loopback_address(ProtocolSelector::from_host_order(0x0800))),
                true,
            )
            .unwrap();
        net_ns.stack().poll_interfaces();

        assert_eq!(all.endpoint.queue_usage().0, 2);
        assert_eq!(exact.endpoint.queue_usage().0, 1);
        assert_eq!(sender.endpoint.queue_usage().0, 1);
    }

    #[test]
    fn recv_truncation_claims_before_copy_and_returns_wire_length() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();
        sender
            .send_with_nonblocking(
                &raw_ipv4_frame(),
                Some(loopback_address(ProtocolSelector::from_host_order(0x0800))),
                true,
            )
            .unwrap();

        let mut bytes = [0_u8; 8];
        let mut dst = &mut bytes[..];
        let result = receiver
            .recv_with_nonblocking(&mut dst, ReceiveFlags::TRUNC, true)
            .unwrap();
        assert_eq!(result.copied_len(), bytes.len());
        assert_eq!(result.returned_len(), raw_ipv4_frame().len());
        assert!(result.message_truncated());
        assert_eq!(result.address().packet_type(), PacketType::OUTGOING);
        assert_eq!(receiver.endpoint.queue_usage().0, 0);
    }

    #[test]
    fn ordinary_copy_fault_consumes_while_peek_fault_retains() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();

        sender
            .send_with_nonblocking(
                &raw_ipv4_frame(),
                Some(loopback_address(ProtocolSelector::from_host_order(0x0800))),
                true,
            )
            .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
        let mut ordinary = FaultDst {
            remaining: raw_ipv4_frame().len(),
        };
        assert_eq!(
            receiver.recv_with_nonblocking(&mut ordinary, ReceiveFlags::EMPTY, true),
            Err(AxError::BadAddress)
        );
        assert_eq!(receiver.endpoint.queue_usage().0, 0);

        sender
            .send_with_nonblocking(
                &raw_ipv4_frame(),
                Some(loopback_address(ProtocolSelector::from_host_order(0x0800))),
                true,
            )
            .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
        let mut peek = FaultDst {
            remaining: raw_ipv4_frame().len(),
        };
        assert_eq!(
            receiver.recv_with_nonblocking(&mut peek, ReceiveFlags::PEEK, true),
            Err(AxError::BadAddress)
        );
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
    }

    #[test]
    fn file_write_rejects_a_source_shorter_than_its_packet_length() {
        let _context = packet_test_context();
        let socket = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::Disabled,
            namespace(),
        )
        .unwrap();
        let mut source = ShortSrc {
            bytes: &[1, 2, 3],
            offset: 0,
            advertised: 4,
        };
        assert_eq!(socket.write(&mut source), Err(AxError::UnexpectedEof));
    }
}
