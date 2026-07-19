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
    device::{Device, DeviceStats, InterfaceKind},
    packet::{
        LinkHardwareType, LinkPacketType, PacketDeviceCapabilities, PacketDeviceContext,
        PacketMetadata, PacketSendRequest,
    },
};

const EMPTY_MAC: EthernetAddress = EthernetAddress([0; 6]);

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
        }
    }

    fn packet_type(dst: EthernetAddress, local: EthernetAddress) -> LinkPacketType {
        if dst.is_broadcast() {
            LinkPacketType::Broadcast
        } else if dst.is_multicast() {
            LinkPacketType::Multicast
        } else if dst == EMPTY_MAC || dst == local {
            LinkPacketType::Host
        } else {
            LinkPacketType::OtherHost
        }
    }

    fn stage_frame(
        context: &PacketDeviceContext<'_>,
        frame: &EthernetFrame<&[u8]>,
        repr: &EthernetRepr,
        packet_type: LinkPacketType,
        address: EthernetAddress,
    ) {
        let header_len = repr.buffer_len().min(frame.as_ref().len());
        let (header, payload) = frame.as_ref().split_at(header_len);
        let mut link_address = [0u8; 8];
        link_address[..address.0.len()].copy_from_slice(&address.0);
        let metadata = PacketMetadata {
            interface_index: context.interface_index(),
            protocol: u16::from(repr.ethertype),
            hardware_type: LinkHardwareType::Ethernet,
            packet_type,
            link_header_len: header_len as u16,
            address: link_address,
            address_len: address.0.len() as u8,
        };

        // Packet observation is deliberately best effort. Capture pressure is
        // accounted by the broker and must never block or fail the ordinary
        // network datapath.
        let _ = context.stage(metadata, header, payload);
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

    fn transmit_frame<F>(
        inner: &mut AxNetDevice,
        stats: &mut DeviceStats,
        context: &PacketDeviceContext<'_>,
        frame_len: usize,
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
        Self::transmit_frame(inner, stats, context, repr.buffer_len() + size, |buffer| {
            let mut frame = EthernetFrame::new_unchecked(buffer);
            repr.emit(&mut frame);
            f(frame.payload_mut());
            repr
        })
    }

    fn handle_frame(
        &mut self,
        context: &PacketDeviceContext<'_>,
        frame: &[u8],
        buffer: &mut PacketBuffer<()>,
        timestamp: Instant,
    ) -> bool {
        let frame = EthernetFrame::new_unchecked(frame);
        let Ok(repr) = EthernetRepr::parse(&frame) else {
            self.stats.record_rx_error();
            self.stats.record_rx_drop();
            warn!("Dropping malformed Ethernet frame");
            return false;
        };

        Self::stage_frame(
            context,
            &frame,
            &repr,
            Self::packet_type(repr.dst_addr, self.hardware_address()),
            repr.src_addr,
        );

        if !repr.dst_addr.is_broadcast()
            && repr.dst_addr != EMPTY_MAC
            && repr.dst_addr != self.hardware_address()
        {
            self.stats.record_rx_drop();
            return false;
        }

        match repr.ethertype {
            EthernetProtocol::Ipv4 => {
                let Ok(dst) = buffer.enqueue(frame.payload().len(), ()) else {
                    self.stats.record_rx_drop();
                    return false;
                };
                dst.copy_from_slice(frame.payload());
                true
            }
            EthernetProtocol::Arp => {
                self.process_arp(context, frame.payload(), timestamp);
                false
            }
            _ => {
                self.stats.record_rx_drop();
                false
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

        let _ = Self::send_to(
            &mut self.inner,
            &mut self.stats,
            context,
            EthernetAddress::BROADCAST,
            arp_repr.buffer_len(),
            |buf| arp_repr.emit(&mut ArpPacket::new_unchecked(buf)),
            EthernetProtocol::Arp,
        );

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

                let _ = Self::send_to(
                    &mut self.inner,
                    &mut self.stats,
                    context,
                    source_hardware_addr,
                    response.buffer_len(),
                    |buf| response.emit(&mut ArpPacket::new_unchecked(buf)),
                    EthernetProtocol::Arp,
                );
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

                    let _ = Self::send_to(
                        &mut self.inner,
                        &mut self.stats,
                        context,
                        neighbor.hardware_address,
                        buf.len(),
                        |b| b.copy_from_slice(buf),
                        EthernetProtocol::Ipv4,
                    );
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
        buffer: &mut PacketBuffer<()>,
        timestamp: Instant,
    ) -> bool {
        loop {
            let rx_buf = match self.inner.receive() {
                Ok(buf) => buf,
                Err(err) => {
                    if !matches!(err, DevError::Again) {
                        self.stats.record_rx_error();
                        warn!("receive failed: {err:?}");
                    }
                    return false;
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
                return false;
            }
            if result {
                return true;
            }
        }
    }

    fn send(
        &mut self,
        context: PacketDeviceContext<'_>,
        next_hop: IpAddress,
        packet: &[u8],
        timestamp: Instant,
    ) -> bool {
        if next_hop.is_broadcast() || self.ip.broadcast().map(IpAddress::Ipv4) == Some(next_hop) {
            let _ = Self::send_to(
                &mut self.inner,
                &mut self.stats,
                &context,
                EthernetAddress::BROADCAST,
                packet.len(),
                |buf| buf.copy_from_slice(packet),
                EthernetProtocol::Ipv4,
            );
            return false;
        }

        let need_request = match self.neighbors.get(&next_hop) {
            Some(Some(neighbor)) => {
                if neighbor.expires_at > timestamp {
                    let _ = Self::send_to(
                        &mut self.inner,
                        &mut self.stats,
                        &context,
                        neighbor.hardware_address,
                        packet.len(),
                        |buf| buf.copy_from_slice(packet),
                        EthernetProtocol::Ipv4,
                    );
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
    ) -> AxResult<()> {
        match request {
            PacketSendRequest::Raw { frame } => {
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
        }
    }

    fn register_waker(&self, waker: &Waker) -> Result<(), PollRegistrationError> {
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
        if let Some(token) = self.irq_registration.get_mut().take() {
            cancel_irq_waker(token);
        }
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, false);
        }
    }
}
