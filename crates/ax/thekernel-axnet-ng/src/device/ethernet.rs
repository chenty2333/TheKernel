use alloc::{string::String, vec};
use core::task::Waker;

use axdriver::prelude::*;
use axerrno::{AxError, AxResult};
use axpoll::{PollRegistrationError, RegisterError, UpdateError};
use axsync::spin::SpinNoIrq;
use axtask::future::{
    IrqWakerRegisterError, IrqWakerToken, IrqWakerUpdateError, cancel_irq_waker,
    register_irq_waker, update_irq_waker,
};
use hashbrown::HashMap;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata as SmolPacketMetadata},
    time::{Duration, Instant},
    wire::{
        ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
        EthernetRepr, IpAddress, Ipv4Cidr,
    },
};

use crate::{
    consts::{ETHERNET_MAX_PENDING_PACKETS, STANDARD_MTU},
    device::{
        Device, DeviceStats, IngressPacketBuffer, InterfaceKind, PacketSendProgress, RxStep,
        RxWakeSource, classify_ethernet_ingress_protocol,
    },
    packet::{
        LinkHardwareType, LinkPacketType, PacketAncillaryCapabilities, PacketAncillaryMetadata,
        PacketDeviceCapabilities, PacketDeviceContext, PacketMetadata, PacketSendRequest,
    },
};

const EMPTY_MAC: EthernetAddress = EthernetAddress([0; 6]);
const ETHERNET_HEADER_LEN: usize = 14;
const VLAN_HEADER_LEN: usize = 4;
const ETHERTYPE_8021Q: u16 = 0x8100;
const ETHERTYPE_8021AD: u16 = 0x88a8;

/// The bounded ingress view used by the packet tap and the ordinary Ethernet
/// protocol path.  Linux removes one inline VLAN header before running packet
/// taps, retaining its TCI/TPID in skb metadata.  A second inline tag remains
/// in `payload` and is therefore the (bounded) inner protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedIngressFrame<'a> {
    dst_addr: EthernetAddress,
    src_addr: EthernetAddress,
    /// The protocol field that remains in the canonical, untagged header.
    wire_protocol: u16,
    /// Linux's normalized host-order protocol selector.
    protocol: u16,
    payload: &'a [u8],
    vlan: Option<(u16, u16)>, // (TCI, TPID), outermost inline tag
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IngressFrameError {
    TruncatedEthernet,
    TruncatedVlan,
}

#[inline]
fn read_be16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline]
const fn is_inline_vlan(protocol: u16) -> bool {
    matches!(protocol, ETHERTYPE_8021Q | ETHERTYPE_8021AD)
}

/// Parses one Ethernet ingress frame without allocating or rewriting the
/// driver's receive buffer.
///
/// This intentionally mirrors Linux's single `skb_vlan_untag()` pass: a
/// complete outer 802.1Q/802.1AD header is removed from the packet-tap view,
/// while an inner VLAN header is left in the canonical payload.  A VLAN
/// marker without its complete four-byte tag is malformed and is dropped by
/// the caller, as Linux drops a failed `skb_vlan_untag()` pull.
fn parse_ingress_frame(frame: &[u8]) -> Result<ParsedIngressFrame<'_>, IngressFrameError> {
    if frame.len() < ETHERNET_HEADER_LEN {
        return Err(IngressFrameError::TruncatedEthernet);
    }

    let dst_addr = EthernetAddress::from_bytes(&frame[..6]);
    let src_addr = EthernetAddress::from_bytes(&frame[6..12]);
    let outer_protocol = read_be16(frame, 12);
    if !is_inline_vlan(outer_protocol) {
        let payload = &frame[ETHERNET_HEADER_LEN..];
        return Ok(ParsedIngressFrame {
            dst_addr,
            src_addr,
            wire_protocol: outer_protocol,
            protocol: classify_ethernet_ingress_protocol(outer_protocol, payload),
            payload,
            vlan: None,
        });
    }

    let vlan_end = ETHERNET_HEADER_LEN
        .checked_add(VLAN_HEADER_LEN)
        .expect("Ethernet VLAN header length is bounded");
    if frame.len() < vlan_end {
        return Err(IngressFrameError::TruncatedVlan);
    }
    let tci = read_be16(frame, ETHERNET_HEADER_LEN);
    let inner_protocol = read_be16(frame, ETHERNET_HEADER_LEN + 2);
    let payload = &frame[vlan_end..];
    Ok(ParsedIngressFrame {
        dst_addr,
        src_addr,
        wire_protocol: inner_protocol,
        protocol: classify_ethernet_ingress_protocol(inner_protocol, payload),
        payload,
        vlan: Some((tci, outer_protocol)),
    })
}

struct Neighbor {
    hardware_address: EthernetAddress,
    expires_at: Instant,
}

pub struct EthernetDevice {
    name: String,
    inner: AxNetDevice,
    irq: Option<usize>,
    neighbors: HashMap<IpAddress, Option<Neighbor>>,
    ip: Ipv4Cidr,
    stats: DeviceStats,
    irq_registration: SpinNoIrq<Option<IrqWakerToken>>,
    rx_irq_registration: SpinNoIrq<Option<IrqWakerToken>>,
    /// A terminal completion/ownership failure fences this interface.  The
    /// raw driver retains every DMA owner in this state; keeping the marker at
    /// the link-device boundary prevents router and worker polling from
    /// repeatedly probing a fenced used ring.
    quarantined: bool,

    pending_packets: PacketBuffer<'static, IpAddress>,
}
impl EthernetDevice {
    const NEIGHBOR_TTL: Duration = Duration::from_secs(60);

    pub fn new(name: String, inner: AxNetDevice, ip: Ipv4Cidr) -> Self {
        let irq = inner.irq_num();
        if let Some(irq) = irq {
            // `EthernetDevice` owns the probed NIC and therefore owns the
            // platform interrupt capability. Waker registration is a generic
            // dispatch mechanism and deliberately does not enable hardware.
            axhal::irq::set_enable(irq, true);
        }
        let pending_packets = PacketBuffer::new(
            vec![SmolPacketMetadata::EMPTY; ETHERNET_MAX_PENDING_PACKETS],
            vec![
                0u8;
                (STANDARD_MTU + EthernetFrame::<&[u8]>::header_len())
                    * ETHERNET_MAX_PENDING_PACKETS
            ],
        );
        Self {
            name,
            inner,
            irq,
            neighbors: HashMap::new(),
            ip,
            stats: DeviceStats::default(),
            irq_registration: SpinNoIrq::new(None),
            rx_irq_registration: SpinNoIrq::new(None),
            quarantined: false,

            pending_packets,
        }
    }

    #[inline]
    fn hardware_address(&self) -> EthernetAddress {
        EthernetAddress(self.inner.mac_address().0)
    }

    const fn packet_capabilities_value() -> PacketDeviceCapabilities {
        PacketDeviceCapabilities {
            hardware_type: LinkHardwareType::Ethernet,
            raw_receive: true,
            raw_send: true,
            cooked_receive: true,
            cooked_send: true,
            link_header_len: EthernetFrame::<&[u8]>::header_len() as u16,
            address_len: 6,
            checksum: crate::packet::PacketChecksumContext::SOFTWARE,
            ancillary: PacketAncillaryCapabilities::CANONICAL,
        }
    }

    fn packet_type(dst: EthernetAddress, local: EthernetAddress) -> LinkPacketType {
        if dst.is_broadcast() {
            LinkPacketType::Broadcast
        } else if dst.is_multicast() {
            LinkPacketType::Multicast
        } else if dst == EMPTY_MAC {
            // The ordinary receive path accepts this legacy placeholder, but
            // it is not a local unicast address for capture metadata.
            LinkPacketType::OtherHost
        } else if dst == local {
            LinkPacketType::Host
        } else {
            LinkPacketType::OtherHost
        }
    }

    fn stage_frame(
        context: &PacketDeviceContext<'_>,
        frame: &EthernetFrame<&[u8]>,
        repr: &EthernetRepr,
        protocol: u16,
        packet_type: LinkPacketType,
        address: EthernetAddress,
    ) {
        // This helper is used for locally generated traffic. Linux's
        // dev_queue_xmit_nit() tap observes an outgoing raw frame before the
        // receive-side VLAN untag pass, so preserve those bytes and the
        // caller-supplied protocol. Incoming frames use
        // `stage_ingress_frame` below instead.
        let header_len = repr.buffer_len().min(frame.as_ref().len());
        let (header, payload) = frame.as_ref().split_at(header_len);
        let mut link_address = [0u8; 8];
        link_address[..address.0.len()].copy_from_slice(&address.0);
        let metadata = PacketMetadata {
            interface_index: context.interface_index(),
            protocol,
            hardware_type: LinkHardwareType::Ethernet,
            packet_type,
            link_header_len: header_len as u16,
            address: link_address,
            address_len: address.0.len() as u8,
        };

        // Packet observation is deliberately best effort. Capture pressure is
        // accounted by the broker and must never block or fail the ordinary
        // network datapath.
        let _ = context.stage_with_ancillary(
            metadata,
            PacketAncillaryMetadata::canonical(),
            header,
            payload,
        );
    }

    /// Stages the Linux packet-tap view of one parsed ingress frame.
    ///
    /// The ordinary frame path remains allocation-free for an untagged
    /// packet: the existing driver's header and payload slices are passed
    /// directly to the bounded broker.  VLAN normalization only needs a
    /// fourteen-byte stack header because the inline four-byte tag is not
    /// contiguous with the canonical EtherType field.
    fn stage_ingress_frame(
        context: &PacketDeviceContext<'_>,
        frame: &[u8],
        parsed: ParsedIngressFrame<'_>,
        packet_type: LinkPacketType,
    ) {
        let mut link_address = [0u8; 8];
        link_address[..parsed.src_addr.0.len()].copy_from_slice(&parsed.src_addr.0);
        let metadata = PacketMetadata {
            interface_index: context.interface_index(),
            protocol: parsed.protocol,
            hardware_type: LinkHardwareType::Ethernet,
            packet_type,
            link_header_len: ETHERNET_HEADER_LEN as u16,
            address: link_address,
            address_len: parsed.src_addr.0.len() as u8,
        };

        let Some((tci, tpid)) = parsed.vlan else {
            let (header, payload) = frame.split_at(ETHERNET_HEADER_LEN);
            let _ = context.stage_with_ancillary(
                metadata,
                PacketAncillaryMetadata::canonical(),
                header,
                payload,
            );
            return;
        };

        // The first twelve bytes are unchanged; the canonical header exposes
        // the inner EtherType at the normal Ethernet offset.  The original
        // receive buffer is never mutated, and the broker copies this bounded
        // stack header before this function returns.
        let mut header = [0u8; ETHERNET_HEADER_LEN];
        header[..12].copy_from_slice(&frame[..12]);
        header[12..14].copy_from_slice(&parsed.wire_protocol.to_be_bytes());
        let _ = context.stage_with_ancillary(
            metadata,
            PacketAncillaryMetadata::canonical().with_vlan(tci, true, tpid),
            &header,
            parsed.payload,
        );
    }

    fn map_dev_error(error: DevError) -> AxError {
        match error {
            DevError::AlreadyExists => AxError::AlreadyExists,
            DevError::Again => AxError::WouldBlock,
            DevError::BadState => AxError::BadState,
            DevError::InvalidParam => AxError::InvalidInput,
            DevError::Io => AxError::Io,
            DevError::NoMemory => AxError::NoMemory,
            DevError::ResourceBusy => AxError::ResourceBusy,
            DevError::Unsupported => AxError::Unsupported,
        }
    }

    fn quarantine(&mut self) {
        if self.quarantined {
            return;
        }
        self.quarantined = true;
        // A terminal queue protocol error is not recoverable by another
        // readiness arm.  Mask the device IRQ before removing both one-shot
        // registrations; the raw driver retains the descriptor owners.
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, false);
        }
        if let Some(token) = self.rx_irq_registration.lock().take() {
            cancel_irq_waker(token);
        }
        if let Some(token) = self.irq_registration.lock().take() {
            cancel_irq_waker(token);
        }
    }

    #[inline]
    fn note_terminal_error(&mut self, error: AxError) {
        if matches!(error, AxError::BadState | AxError::Io) {
            self.quarantine();
        }
    }

    fn transmit_frame<F>(
        inner: &mut AxNetDevice,
        stats: &mut DeviceStats,
        context: &PacketDeviceContext<'_>,
        frame_len: usize,
        capture_protocol: u16,
        build: F,
    ) -> AxResult<()>
    where
        F: FnOnce(&mut [u8]) -> EthernetRepr,
    {
        if let Err(error) = inner.recycle_tx_buffers() {
            stats.record_tx_error();
            warn!("recycle_tx_buffers failed: {error:?}");
            return Err(Self::map_dev_error(error));
        }

        let mut tx_buf = match inner.alloc_tx_buffer(frame_len) {
            Ok(buffer) => buffer,
            Err(error) => {
                stats.record_tx_drop();
                warn!("alloc_tx_buffer failed: {error:?}");
                return Err(Self::map_dev_error(error));
            }
        };
        let repr = build(tx_buf.packet_mut());
        let frame = EthernetFrame::new_unchecked(tx_buf.packet());
        Self::stage_frame(
            context,
            &frame,
            &repr,
            capture_protocol,
            LinkPacketType::Outgoing,
            repr.src_addr,
        );
        trace!(
            "SEND {} bytes: {:02X?}",
            tx_buf.packet_len(),
            tx_buf.packet()
        );
        let transmitted_len = tx_buf.packet_len();
        match inner.transmit(tx_buf) {
            Ok(()) => {
                stats.record_tx(transmitted_len);
                Ok(())
            }
            Err(error) => {
                stats.record_tx_error();
                stats.record_tx_drop();
                warn!("transmit failed: {error:?}");
                Err(Self::map_dev_error(error))
            }
        }
    }

    fn send_to<F>(
        inner: &mut AxNetDevice,
        stats: &mut DeviceStats,
        context: &PacketDeviceContext<'_>,
        dst: EthernetAddress,
        size: usize,
        f: F,
        proto: EthernetProtocol,
    ) -> AxResult<()>
    where
        F: FnOnce(&mut [u8]),
    {
        let repr = EthernetRepr {
            src_addr: EthernetAddress(inner.mac_address().0),
            dst_addr: dst,
            ethertype: proto,
        };
        Self::transmit_frame(
            inner,
            stats,
            context,
            repr.buffer_len() + size,
            u16::from(proto),
            |buffer| {
                let mut frame = EthernetFrame::new_unchecked(buffer);
                repr.emit(&mut frame);
                f(frame.payload_mut());
                repr
            },
        )
    }

    fn handle_frame(
        &mut self,
        context: &PacketDeviceContext<'_>,
        frame: &[u8],
        buffer: &mut IngressPacketBuffer,
        timestamp: Instant,
    ) -> RxStep {
        let Ok(parsed) = parse_ingress_frame(frame) else {
            self.stats.record_rx_error();
            self.stats.record_rx_drop();
            warn!("Dropping malformed Ethernet frame");
            return RxStep::Consumed;
        };

        Self::stage_ingress_frame(
            context,
            frame,
            parsed,
            Self::packet_type(parsed.dst_addr, self.hardware_address()),
        );

        if !parsed.dst_addr.is_broadcast()
            && parsed.dst_addr != EMPTY_MAC
            && parsed.dst_addr != self.hardware_address()
        {
            self.stats.record_rx_drop();
            return RxStep::Consumed;
        }

        match EthernetProtocol::from(parsed.wire_protocol) {
            EthernetProtocol::Ipv4 => {
                let Ok(dst) = buffer.enqueue(parsed.payload.len(), context.interface_index())
                else {
                    self.stats.record_rx_drop();
                    return RxStep::Consumed;
                };
                dst.copy_from_slice(parsed.payload);
                RxStep::Delivered
            }
            EthernetProtocol::Arp => {
                self.process_arp(context, parsed.payload, timestamp);
                RxStep::Consumed
            }
            _ => {
                self.stats.record_rx_drop();
                RxStep::Consumed
            }
        }
    }

    fn request_arp(&mut self, context: &PacketDeviceContext<'_>, target_ip: IpAddress) {
        let IpAddress::Ipv4(target_ipv4) = target_ip else {
            warn!("IPv6 address ARP is not supported: {target_ip}");
            return;
        };
        debug!("Requesting ARP for {target_ipv4}");

        let arp_repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: self.hardware_address(),
            source_protocol_addr: self.ip.address(),
            target_hardware_addr: EthernetAddress::BROADCAST,
            target_protocol_addr: target_ipv4,
        };

        let result = Self::send_to(
            &mut self.inner,
            &mut self.stats,
            context,
            EthernetAddress::BROADCAST,
            arp_repr.buffer_len(),
            |buf| arp_repr.emit(&mut ArpPacket::new_unchecked(buf)),
            EthernetProtocol::Arp,
        );
        if let Err(error) = result {
            self.note_terminal_error(error);
        }
        if self.quarantined {
            return;
        }

        self.neighbors.insert(target_ip, None);
    }

    fn process_arp(&mut self, context: &PacketDeviceContext<'_>, payload: &[u8], now: Instant) {
        let Ok(repr) = ArpPacket::new_checked(payload).and_then(|packet| ArpRepr::parse(&packet))
        else {
            warn!("Dropping malformed ARP packet");
            return;
        };

        if let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } = repr
        {
            let is_unicast_mac =
                target_hardware_addr != EMPTY_MAC && !target_hardware_addr.is_broadcast();
            if is_unicast_mac && self.hardware_address() != target_hardware_addr {
                // Only process packet that are for us
                return;
            }

            if let ArpOperation::Unknown(_) = operation {
                return;
            }

            if !source_hardware_addr.is_unicast()
                || source_protocol_addr.is_broadcast()
                || source_protocol_addr.is_multicast()
                || source_protocol_addr.is_unspecified()
            {
                return;
            }
            if self.ip.address() != target_protocol_addr {
                return;
            }

            debug!("ARP: {source_protocol_addr} -> {source_hardware_addr}");
            self.neighbors.insert(
                IpAddress::Ipv4(source_protocol_addr),
                Some(Neighbor {
                    hardware_address: source_hardware_addr,
                    expires_at: now + Self::NEIGHBOR_TTL,
                }),
            );

            if let ArpOperation::Request = operation {
                let response = ArpRepr::EthernetIpv4 {
                    operation: ArpOperation::Reply,
                    source_hardware_addr: self.hardware_address(),
                    source_protocol_addr: self.ip.address(),
                    target_hardware_addr: source_hardware_addr,
                    target_protocol_addr: source_protocol_addr,
                };

                let result = Self::send_to(
                    &mut self.inner,
                    &mut self.stats,
                    context,
                    source_hardware_addr,
                    response.buffer_len(),
                    |buf| response.emit(&mut ArpPacket::new_unchecked(buf)),
                    EthernetProtocol::Arp,
                );
                if let Err(error) = result {
                    self.note_terminal_error(error);
                    if self.quarantined {
                        return;
                    }
                }
            }

            if self
                .pending_packets
                .peek()
                .is_ok_and(|it| it.0 == &IpAddress::Ipv4(source_protocol_addr))
            {
                while let Ok((&next_hop, buf)) = self.pending_packets.peek() {
                    // TODO: optimize logic such that one long-pending ARP
                    // request does not block all other packets

                    let Some(Some(neighbor)) = self.neighbors.get(&next_hop) else {
                        break;
                    };
                    if neighbor.expires_at <= now {
                        // Neighbor is expired, we need to request ARP again
                        self.request_arp(context, next_hop);
                        break;
                    }

                    let result = Self::send_to(
                        &mut self.inner,
                        &mut self.stats,
                        context,
                        neighbor.hardware_address,
                        buf.len(),
                        |b| b.copy_from_slice(buf),
                        EthernetProtocol::Ipv4,
                    );
                    if let Err(error) = result {
                        self.note_terminal_error(error);
                        break;
                    }
                    let _ = self.pending_packets.dequeue();
                }
            }
        }
    }
}

impl Device for EthernetDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn stats(&self) -> DeviceStats {
        self.stats
    }

    fn interface_kind(&self) -> InterfaceKind {
        InterfaceKind::Ethernet
    }

    fn mtu(&self) -> usize {
        STANDARD_MTU
    }

    fn has_rx_backlog(&self) -> bool {
        !self.quarantined && self.inner.can_receive()
    }

    fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    fn rx_wake_capable(&self) -> bool {
        !self.quarantined && self.irq.is_some()
    }

    fn rx_wake_required(&self) -> bool {
        true
    }

    fn hardware_address(&self) -> Option<[u8; 6]> {
        Some(self.hardware_address().0)
    }

    fn addresses(&self) -> alloc::vec::Vec<smoltcp::wire::IpCidr> {
        vec![self.ip.into()]
    }

    fn packet_capabilities(&self) -> PacketDeviceCapabilities {
        Self::packet_capabilities_value()
    }

    fn recv(
        &mut self,
        context: PacketDeviceContext<'_>,
        buffer: &mut IngressPacketBuffer,
        timestamp: Instant,
    ) -> RxStep {
        if self.quarantined {
            return RxStep::Idle;
        }
        let rx_buf = match self.inner.receive() {
            Ok(buf) => buf,
            Err(err) => {
                if !matches!(err, DevError::Again) {
                    self.stats.record_rx_error();
                    warn!("receive failed: {err:?}");
                    self.quarantine();
                }
                return RxStep::Idle;
            }
        };
        trace!(
            "RECV {} bytes: {:02X?}",
            rx_buf.packet_len(),
            rx_buf.packet()
        );
        self.stats.record_rx(rx_buf.packet_len());

        let result = self.handle_frame(&context, rx_buf.packet(), buffer, timestamp);
        if let Err(err) = self.inner.recycle_rx_buffer(rx_buf) {
            self.stats.record_rx_error();
            warn!("recycle_rx_buffer failed: {err:?}");
            self.quarantine();
        }
        result
    }

    fn send(
        &mut self,
        context: PacketDeviceContext<'_>,
        next_hop: IpAddress,
        packet: &[u8],
        timestamp: Instant,
    ) -> bool {
        if self.quarantined {
            return false;
        }
        if next_hop.is_broadcast() || self.ip.broadcast().map(IpAddress::Ipv4) == Some(next_hop) {
            let result = Self::send_to(
                &mut self.inner,
                &mut self.stats,
                &context,
                EthernetAddress::BROADCAST,
                packet.len(),
                |buf| buf.copy_from_slice(packet),
                EthernetProtocol::Ipv4,
            );
            if let Err(error) = result {
                self.note_terminal_error(error);
            }
            return false;
        }

        let need_request = match self.neighbors.get(&next_hop) {
            Some(Some(neighbor)) => {
                if neighbor.expires_at > timestamp {
                    let result = Self::send_to(
                        &mut self.inner,
                        &mut self.stats,
                        &context,
                        neighbor.hardware_address,
                        packet.len(),
                        |buf| buf.copy_from_slice(packet),
                        EthernetProtocol::Ipv4,
                    );
                    if let Err(error) = result {
                        self.note_terminal_error(error);
                    }
                    return false;
                } else {
                    true
                }
            }
            // Request already sent
            Some(None) => false,
            None => true,
        };
        // Only send ARP request if we haven't already requested it
        if need_request {
            self.request_arp(&context, next_hop);
        }
        if self.pending_packets.is_full() {
            self.stats.record_tx_drop();
            warn!("Pending packets buffer is full, dropping packet");
            return false;
        }
        let Ok(dst_buffer) = self.pending_packets.enqueue(packet.len(), next_hop) else {
            self.stats.record_tx_drop();
            warn!("Failed to enqueue packet in pending packets buffer");
            return false;
        };
        dst_buffer.copy_from_slice(packet);
        false
    }

    fn send_packet(
        &mut self,
        context: PacketDeviceContext<'_>,
        request: PacketSendRequest<'_>,
        _timestamp: Instant,
    ) -> AxResult<PacketSendProgress> {
        if self.quarantined {
            return Err(AxError::BadState);
        }
        let result = match request {
            PacketSendRequest::Raw { protocol, frame } => {
                let header_len = EthernetFrame::<&[u8]>::header_len();
                if frame.len() < header_len || frame.len() > STANDARD_MTU + header_len {
                    return Err(AxError::InvalidInput);
                }
                let checked =
                    EthernetFrame::new_checked(frame).map_err(|_| AxError::InvalidInput)?;
                let repr = EthernetRepr::parse(&checked).map_err(|_| AxError::InvalidInput)?;

                Self::transmit_frame(
                    &mut self.inner,
                    &mut self.stats,
                    &context,
                    frame.len(),
                    protocol,
                    |buffer| {
                        buffer.copy_from_slice(frame);
                        repr
                    },
                )
            }
            PacketSendRequest::Cooked {
                protocol,
                destination,
                payload,
            } => {
                if destination.len() != 6 || payload.len() > STANDARD_MTU {
                    return Err(AxError::InvalidInput);
                }

                Self::send_to(
                    &mut self.inner,
                    &mut self.stats,
                    &context,
                    EthernetAddress::from_bytes(destination),
                    payload.len(),
                    |buffer| buffer.copy_from_slice(payload),
                    EthernetProtocol::from(protocol),
                )
            }
        };
        if let Err(error) = result {
            self.note_terminal_error(error);
        }
        result.map(|()| PacketSendProgress::NoImmediateIngress)
    }

    fn register_waker(&self, waker: &Waker) -> Result<(), PollRegistrationError> {
        if self.quarantined {
            return Ok(());
        }
        let Some(irq) = self.irq else {
            return Ok(());
        };

        let mut registration = self.irq_registration.lock();
        if let Some(token) = *registration {
            match update_irq_waker(token, waker) {
                Ok(()) => return Ok(()),
                Err(IrqWakerUpdateError::Registration(UpdateError::InvalidToken)) => {
                    *registration = None;
                }
                Err(IrqWakerUpdateError::Registration(UpdateError::Closed)) => {
                    return Err(PollRegistrationError::Source {
                        index: 0,
                        error: RegisterError::Closed,
                    });
                }
                Err(IrqWakerUpdateError::InvalidSource) => {
                    return Err(PollRegistrationError::InvalidState);
                }
            }
        }

        *registration = Some(register_irq_waker(irq, waker).map_err(map_irq_register_error)?);
        Ok(())
    }

    fn register_rx_waker(&self, waker: &Waker) -> Result<RxWakeSource, PollRegistrationError> {
        if self.quarantined {
            return Ok(RxWakeSource::Unavailable);
        }
        let Some(irq) = self.irq else {
            return Ok(RxWakeSource::Unavailable);
        };

        let mut registration = self.rx_irq_registration.lock();
        if let Some(token) = *registration {
            match update_irq_waker(token, waker) {
                Ok(()) => return Ok(RxWakeSource::Armed),
                Err(IrqWakerUpdateError::Registration(UpdateError::InvalidToken)) => {
                    *registration = None;
                }
                Err(IrqWakerUpdateError::Registration(UpdateError::Closed)) => {
                    return Err(PollRegistrationError::Source {
                        index: 0,
                        error: RegisterError::Closed,
                    });
                }
                Err(IrqWakerUpdateError::InvalidSource) => {
                    return Err(PollRegistrationError::InvalidState);
                }
            }
        }

        *registration = Some(register_irq_waker(irq, waker).map_err(map_irq_register_error)?);
        Ok(RxWakeSource::Armed)
    }

    fn stop_rx_waker(&self) {
        if let Some(irq) = self.irq {
            // Teardown or source quarantine masks before cancelling every
            // one-shot registration, so no final interrupt can race removal.
            axhal::irq::set_enable(irq, false);
        }
        if let Some(token) = self.rx_irq_registration.lock().take() {
            cancel_irq_waker(token);
        }
        if let Some(token) = self.irq_registration.lock().take() {
            cancel_irq_waker(token);
        }
    }
}

fn map_irq_register_error(error: IrqWakerRegisterError) -> PollRegistrationError {
    match error {
        IrqWakerRegisterError::Waiter(error) => PollRegistrationError::Source { index: 0, error },
        IrqWakerRegisterError::SourceCapacityExhausted
        | IrqWakerRegisterError::HookInstallationInProgress => PollRegistrationError::Quota,
        IrqWakerRegisterError::HookUnavailable => PollRegistrationError::InvalidState,
    }
}

impl Drop for EthernetDevice {
    fn drop(&mut self) {
        if let Some(irq) = self.irq {
            // Mask before removing registrations so no new interrupt can
            // race the final token cancellation.
            axhal::irq::set_enable(irq, false);
        }
        if let Some(token) = self.rx_irq_registration.get_mut().take() {
            cancel_irq_waker(token);
        }
        if let Some(token) = self.irq_registration.get_mut().take() {
            cancel_irq_waker(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        vec::Vec,
    };

    use super::*;
    use crate::packet::{
        PacketBroker, PacketFilter, PacketFilterContext, PacketProtocol, PacketSelector, PacketView,
    };

    fn vlan_frame(tpid: u16, tci: u16, inner_protocol: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + VLAN_HEADER_LEN + payload.len());
        frame.extend_from_slice(&[2, 1, 2, 3, 4, 5]);
        frame.extend_from_slice(&[0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6]);
        frame.extend_from_slice(&tpid.to_be_bytes());
        frame.extend_from_slice(&tci.to_be_bytes());
        frame.extend_from_slice(&inner_protocol.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn ingress_vlan_view_strips_one_tag_and_preserves_inner_protocol() {
        let frame = vlan_frame(ETHERTYPE_8021Q, 0x0064, 0x0800, &[0x45, 0, 1, 2]);
        let parsed = parse_ingress_frame(&frame).unwrap();
        assert_eq!(parsed.wire_protocol, 0x0800);
        assert_eq!(parsed.protocol, 0x0800);
        assert_eq!(parsed.payload, &[0x45, 0, 1, 2]);
        assert_eq!(parsed.vlan, Some((0x0064, ETHERTYPE_8021Q)));

        let broker = PacketBroker::try_new().unwrap();
        let raw = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::Exact(0x0800),
                Some(1),
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let filter_seen = Arc::new(AtomicBool::new(false));
        raw.set_filter(Some(Arc::new(VlanFilter(Arc::clone(&filter_seen)))))
            .unwrap();
        let cooked = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::Exact(0x0800),
                Some(1),
                PacketView::Cooked,
                true,
            ))
            .unwrap();
        let context = PacketDeviceContext::new(1, &broker, None);
        stage_ingress_for_test(&context, &frame, parsed);
        broker.drain_staged();

        let raw_record = raw.try_receive(false).unwrap();
        assert_eq!(
            raw_record.data(),
            &[
                2, 1, 2, 3, 4, 5, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0x08, 0x00, 0x45, 0, 1, 2
            ]
        );
        assert_eq!(raw_record.wire_len(), 18);
        assert_eq!(raw_record.metadata().protocol, 0x0800);
        assert_eq!(raw_record.metadata().link_header_len, 14);
        assert_eq!(cooked.try_receive(false).unwrap().data(), &[0x45, 0, 1, 2]);
        assert!(filter_seen.load(Ordering::Acquire));
    }

    struct VlanFilter(Arc<AtomicBool>);

    impl PacketFilter for VlanFilter {
        fn filter(&self, packet: &[u8], context: PacketFilterContext<'_>) -> AxResult<usize> {
            if packet.get(12..14) == Some(&[0x08, 0x00])
                && context.metadata().protocol == 0x0800
                && context.metadata().link_header_len == ETHERNET_HEADER_LEN as u16
                && context.ancillary().vlan() == (0x0064, true, ETHERTYPE_8021Q)
            {
                self.0.store(true, Ordering::Release);
            }
            Ok(packet.len())
        }
    }

    // Keep the test on the same staging helper used by `handle_frame` while
    // avoiding an Ethernet driver allocation in this parser-focused unit
    // test.
    fn stage_ingress_for_test(
        context: &PacketDeviceContext<'_>,
        frame: &[u8],
        parsed: ParsedIngressFrame<'_>,
    ) {
        EthernetDevice::stage_ingress_frame(context, frame, parsed, LinkPacketType::Host);
    }

    #[test]
    fn ingress_vlan_parser_bounds_truncation_and_double_tags() {
        let mut truncated = vec![0u8; ETHERNET_HEADER_LEN + 1];
        truncated[12..14].copy_from_slice(&ETHERTYPE_8021Q.to_be_bytes());
        assert_eq!(
            parse_ingress_frame(&truncated),
            Err(IngressFrameError::TruncatedVlan)
        );

        let double = vlan_frame(
            ETHERTYPE_8021AD,
            0x0123,
            ETHERTYPE_8021Q,
            &[0x00, 0x2a, 0x08, 0x00],
        );
        let parsed = parse_ingress_frame(&double).unwrap();
        assert_eq!(parsed.wire_protocol, ETHERTYPE_8021Q);
        assert_eq!(parsed.protocol, ETHERTYPE_8021Q);
        assert_eq!(parsed.payload, &[0x00, 0x2a, 0x08, 0x00]);
        assert_eq!(parsed.vlan, Some((0x0123, ETHERTYPE_8021AD)));
    }

    #[test]
    fn zero_destination_is_not_classified_as_local_host_metadata() {
        let local = EthernetAddress([2, 1, 2, 3, 4, 5]);
        assert_eq!(
            EthernetDevice::packet_type(EMPTY_MAC, local),
            LinkPacketType::OtherHost
        );
        assert_eq!(
            EthernetDevice::packet_type(local, local),
            LinkPacketType::Host
        );
    }

    #[test]
    fn raw_outgoing_capture_preserves_frame_and_uses_request_protocol() {
        let broker = PacketBroker::try_new().unwrap();
        let selector = PacketSelector::new(PacketProtocol::All, Some(1), PacketView::Raw, true);
        let source = broker.subscribe(selector).unwrap();
        let observer = broker.subscribe(selector).unwrap();
        let origin = broker.origin_id(source.as_ref()).unwrap();
        let context = PacketDeviceContext::new(1, &broker, Some(origin));
        let bytes = [
            2, 1, 2, 3, 4, 5, // destination
            0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, // source
            0x08, 0x00, // frame protocol
            0x45, 0, // payload prefix
        ];
        let frame = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
        let repr = EthernetRepr::parse(&frame).unwrap();

        EthernetDevice::stage_frame(
            &context,
            &frame,
            &repr,
            0x88b5,
            LinkPacketType::Outgoing,
            repr.src_addr,
        );
        broker.drain_staged();

        assert!(matches!(
            source.try_receive(false),
            Err(AxError::WouldBlock)
        ));
        let outgoing = observer.try_receive(false).unwrap();
        assert_eq!(outgoing.data(), bytes);
        assert_eq!(outgoing.metadata().protocol, 0x88b5);
        assert_eq!(outgoing.metadata().packet_type, LinkPacketType::Outgoing);
    }
}
