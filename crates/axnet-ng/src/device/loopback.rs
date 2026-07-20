use alloc::{vec, vec::Vec};
use core::task::Waker;

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata as SmolPacketMetadata},
    time::Instant,
    wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Packet, Ipv6Address, Ipv6Packet},
};

use crate::{
    consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
    device::{
        Device, DevicePollBridge, DeviceStats, InterfaceKind, classify_ethernet_ingress_protocol,
    },
    packet::{
        LinkHardwareType, LinkPacketType, PacketDeviceCapabilities, PacketDeviceContext,
        PacketEndpointId, PacketMetadata, PacketSendRequest,
    },
};

const LOOPBACK_HEADER_LEN: usize = 14;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;

pub struct LoopbackDevice {
    buffer: PacketBuffer<'static, LoopbackPacketMetadata>,
    poll: PollSet,
    poll_bridge: DevicePollBridge,
    stats: DeviceStats,
}

#[derive(Clone, Copy)]
struct LoopbackPacketMetadata {
    origin: Option<PacketEndpointId>,
    header: [u8; LOOPBACK_HEADER_LEN],
}

impl LoopbackDevice {
    pub fn new() -> Self {
        Self::try_new().expect("failed to allocate loopback packet queue")
    }

    pub fn try_new() -> AxResult<Self> {
        let mut metadata = Vec::new();
        metadata
            .try_reserve_exact(PACKET_QUEUE_LEN)
            .map_err(|_| AxError::NoMemory)?;
        metadata.resize(PACKET_QUEUE_LEN, SmolPacketMetadata::EMPTY);
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(LOOPBACK_MTU * PACKET_QUEUE_LEN)
            .map_err(|_| AxError::NoMemory)?;
        storage.resize(LOOPBACK_MTU * PACKET_QUEUE_LEN, 0);
        let buffer = PacketBuffer::new(metadata, storage);
        Ok(Self {
            buffer,
            poll: PollSet::new(),
            poll_bridge: DevicePollBridge::new(),
            stats: DeviceStats::default(),
        })
    }

    const fn packet_capabilities_value() -> PacketDeviceCapabilities {
        PacketDeviceCapabilities {
            hardware_type: LinkHardwareType::Loopback,
            raw_receive: true,
            raw_send: true,
            cooked_receive: true,
            cooked_send: true,
            link_header_len: LOOPBACK_HEADER_LEN as u16,
            address_len: 6,
        }
    }

    fn ip_protocol(payload: &[u8]) -> Option<u16> {
        match payload.first().map(|byte| byte >> 4) {
            Some(4) => Ipv4Packet::new_checked(payload)
                .is_ok_and(|packet| packet.version() == 4 && packet.header_len() >= 20)
                .then_some(ETH_P_IP),
            Some(6) => Ipv6Packet::new_checked(payload)
                .is_ok_and(|packet| packet.version() == 6)
                .then_some(ETH_P_IPV6),
            _ => None,
        }
    }

    fn packet_type(header: &[u8; LOOPBACK_HEADER_LEN]) -> LinkPacketType {
        let destination = &header[..6];
        if destination.iter().all(|byte| *byte == 0xff) {
            LinkPacketType::Broadcast
        } else if destination[0] & 1 != 0 {
            LinkPacketType::Multicast
        } else if destination.iter().all(|byte| *byte == 0) {
            LinkPacketType::Host
        } else {
            LinkPacketType::OtherHost
        }
    }

    fn make_header(protocol: u16, destination: &[u8; 6]) -> [u8; LOOPBACK_HEADER_LEN] {
        let mut header = [0u8; LOOPBACK_HEADER_LEN];
        header[..6].copy_from_slice(destination);
        header[12..].copy_from_slice(&protocol.to_be_bytes());
        header
    }

    fn stage_packet(
        context: PacketDeviceContext<'_>,
        protocol: u16,
        packet_type: LinkPacketType,
        header: &[u8; LOOPBACK_HEADER_LEN],
        payload: &[u8],
    ) {
        let mut address = [0u8; 8];
        address[..6].copy_from_slice(&header[6..12]);
        let metadata = PacketMetadata {
            interface_index: context.interface_index(),
            protocol,
            hardware_type: LinkHardwareType::Loopback,
            packet_type,
            link_header_len: LOOPBACK_HEADER_LEN as u16,
            address,
            address_len: 6,
        };

        // Packet observation is best effort and can neither block nor reject
        // the ordinary loopback path.
        let _ = context.stage(metadata, header, payload);
    }

    fn enqueue_packet(
        &mut self,
        context: PacketDeviceContext<'_>,
        protocol: u16,
        header: [u8; LOOPBACK_HEADER_LEN],
        payload: &[u8],
    ) -> AxResult<()> {
        let tx_buf = self
            .buffer
            .enqueue(
                payload.len(),
                LoopbackPacketMetadata {
                    origin: context.origin(),
                    header,
                },
            )
            .map_err(|_| AxError::WouldBlock)?;
        tx_buf.copy_from_slice(payload);
        Self::stage_packet(
            context,
            protocol,
            LinkPacketType::Outgoing,
            &header,
            payload,
        );
        self.stats.record_tx(payload.len());
        self.poll.wake();
        Ok(())
    }
}

impl Device for LoopbackDevice {
    fn name(&self) -> &str {
        "lo"
    }

    fn stats(&self) -> DeviceStats {
        self.stats
    }

    fn interface_kind(&self) -> InterfaceKind {
        InterfaceKind::Loopback
    }

    fn mtu(&self) -> usize {
        LOOPBACK_MTU
    }

    fn hardware_address(&self) -> Option<[u8; 6]> {
        Some([0; 6])
    }

    fn addresses(&self) -> alloc::vec::Vec<IpCidr> {
        vec![
            IpCidr::new(Ipv4Address::LOCALHOST.into(), 8),
            IpCidr::new(Ipv6Address::LOCALHOST.into(), 128),
        ]
    }

    fn packet_capabilities(&self) -> PacketDeviceCapabilities {
        Self::packet_capabilities_value()
    }

    fn recv(
        &mut self,
        context: PacketDeviceContext<'_>,
        buffer: &mut PacketBuffer<()>,
        _timestamp: Instant,
    ) -> bool {
        let Ok((metadata, rx_buf)) = self.buffer.dequeue() else {
            return false;
        };
        let len = rx_buf.len();
        let ingress_context = if metadata.origin.is_some() {
            // Linux suppresses the injecting packet endpoint only for the
            // outgoing tap. The looped-back ingress copy is independently
            // visible to that endpoint.
            context.with_origin(None)
        } else {
            context
        };
        Self::stage_packet(
            ingress_context,
            classify_ethernet_ingress_protocol(
                u16::from_be_bytes([metadata.header[12], metadata.header[13]]),
                rx_buf,
            ),
            Self::packet_type(&metadata.header),
            &metadata.header,
            rx_buf,
        );
        let Ok(dst) = buffer.enqueue(len, ()) else {
            self.stats.record_rx_drop();
            return false;
        };
        dst.copy_from_slice(rx_buf);
        self.stats.record_rx(len);
        true
    }

    fn send(
        &mut self,
        context: PacketDeviceContext<'_>,
        next_hop: IpAddress,
        packet: &[u8],
        _timestamp: Instant,
    ) -> bool {
        let Some(protocol) = Self::ip_protocol(packet) else {
            self.stats.record_tx_drop();
            warn!("Loopback device received a malformed IP packet for {next_hop}");
            return false;
        };
        let header = Self::make_header(protocol, &[0; 6]);
        match self.enqueue_packet(context, protocol, header, packet) {
            Ok(()) => true,
            Err(_) => {
                self.stats.record_tx_drop();
                warn!("Loopback device buffer is full, dropping packet to {next_hop}");
                false
            }
        }
    }

    fn send_packet(
        &mut self,
        context: PacketDeviceContext<'_>,
        request: PacketSendRequest<'_>,
        _timestamp: Instant,
    ) -> AxResult<()> {
        let (protocol, header, payload) = match request {
            PacketSendRequest::Raw { protocol, frame } => {
                if frame.len() < LOOPBACK_HEADER_LEN {
                    return Err(AxError::InvalidInput);
                }
                let header = frame[..LOOPBACK_HEADER_LEN]
                    .try_into()
                    .map_err(|_| AxError::InvalidInput)?;
                (protocol, header, &frame[LOOPBACK_HEADER_LEN..])
            }
            PacketSendRequest::Cooked {
                protocol,
                destination,
                payload,
            } => {
                let destination: &[u8; 6] =
                    destination.try_into().map_err(|_| AxError::InvalidInput)?;
                (protocol, Self::make_header(protocol, destination), payload)
            }
        };

        if payload.len() > LOOPBACK_MTU {
            return Err(AxError::InvalidInput);
        }
        self.enqueue_packet(context, protocol, header, payload)
            .inspect_err(|_| self.stats.record_tx_drop())
    }

    fn register_waker(&self, waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
        self.poll_bridge.refresh(&self.poll, waker)
    }
}

impl Drop for LoopbackDevice {
    fn drop(&mut self) {
        self.poll_bridge.cancel(&self.poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{PacketBroker, PacketProtocol, PacketSelector, PacketView};

    fn minimal_ipv4_packet() -> [u8; 20] {
        let mut packet = [0u8; 20];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&20u16.to_be_bytes());
        packet[8] = 64;
        packet
    }

    #[test]
    fn ordinary_loopback_send_publishes_outgoing_then_host_views() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                Some(1),
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let mut device = LoopbackDevice::try_new().unwrap();
        let context = PacketDeviceContext::new(1, &broker, None);
        let packet = minimal_ipv4_packet();

        assert!(device.send(
            context,
            IpAddress::Ipv4(Ipv4Address::LOCALHOST),
            &packet,
            Instant::ZERO,
        ));
        broker.drain_staged();

        let outgoing = endpoint.try_receive(false).unwrap();
        assert_eq!(outgoing.metadata().packet_type, LinkPacketType::Outgoing);
        assert_eq!(outgoing.metadata().protocol, ETH_P_IP);
        assert_eq!(&outgoing.data()[..12], &[0; 12]);
        assert_eq!(&outgoing.data()[12..14], &ETH_P_IP.to_be_bytes());
        assert_eq!(&outgoing.data()[14..], &packet);

        let mut ingress =
            PacketBuffer::new(vec![SmolPacketMetadata::EMPTY; 1], vec![0u8; packet.len()]);
        assert!(device.recv(context, &mut ingress, Instant::ZERO));
        broker.drain_staged();

        let host = endpoint.try_receive(false).unwrap();
        assert_eq!(host.metadata().packet_type, LinkPacketType::Host);
        assert_eq!(host.metadata().protocol, ETH_P_IP);
        assert_eq!(host.data(), outgoing.data());
        let (_, received) = ingress.dequeue().unwrap();
        assert_eq!(received, packet);
    }

    #[test]
    fn loopback_packet_capabilities_match_its_pseudo_link_contract() {
        let device = LoopbackDevice::try_new().unwrap();
        let capabilities = device.packet_capabilities();
        assert_eq!(capabilities.hardware_type, LinkHardwareType::Loopback);
        assert!(capabilities.raw_receive);
        assert!(capabilities.raw_send);
        assert!(capabilities.cooked_receive);
        assert!(capabilities.cooked_send);
        assert_eq!(capabilities.link_header_len, LOOPBACK_HEADER_LEN as u16);
        assert_eq!(capabilities.address_len, 6);
    }

    #[test]
    fn raw_injection_preserves_header_and_suppresses_only_outgoing_origin() {
        let broker = PacketBroker::try_new().unwrap();
        let selector = PacketSelector::new(PacketProtocol::All, Some(1), PacketView::Raw, true);
        let source = broker.subscribe(selector).unwrap();
        let observer = broker.subscribe(selector).unwrap();
        let mut device = LoopbackDevice::try_new().unwrap();
        let origin = broker.origin_id(source.as_ref()).unwrap();
        let inject = PacketDeviceContext::new(1, &broker, Some(origin));
        let ingress = PacketDeviceContext::new(1, &broker, None);
        let frame = [
            2, 1, 2, 3, 4, 5, // non-local unicast destination
            0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, // source
            0x08, 0x00, // protocol
            b'n', b'o', b't', b'-', b'i', b'p', // deliberately malformed L3
        ];

        device
            .send_packet(
                inject,
                PacketSendRequest::Raw {
                    protocol: 0x88b5,
                    frame: &frame,
                },
                Instant::ZERO,
            )
            .unwrap();
        broker.drain_staged();

        assert!(matches!(
            source.try_receive(false),
            Err(AxError::WouldBlock)
        ));
        let outgoing = observer.try_receive(false).unwrap();
        assert_eq!(outgoing.data(), frame);
        assert_eq!(outgoing.metadata().packet_type, LinkPacketType::Outgoing);
        assert_eq!(outgoing.metadata().protocol, 0x88b5);
        assert_eq!(&outgoing.metadata().address[..6], &frame[6..12]);

        let mut ip_ingress = PacketBuffer::new(
            vec![SmolPacketMetadata::EMPTY; 1],
            vec![0u8; frame.len() - LOOPBACK_HEADER_LEN],
        );
        assert!(device.recv(ingress, &mut ip_ingress, Instant::ZERO));
        broker.drain_staged();

        let source_host = source.try_receive(false).unwrap();
        let observer_host = observer.try_receive(false).unwrap();
        for host in [&source_host, &observer_host] {
            assert_eq!(host.data(), frame);
            assert_eq!(host.metadata().packet_type, LinkPacketType::OtherHost);
            assert_eq!(host.metadata().protocol, ETH_P_IP);
            assert_eq!(&host.metadata().address[..6], &frame[6..12]);
        }
        let (_, payload) = ip_ingress.dequeue().unwrap();
        assert_eq!(payload, &frame[LOOPBACK_HEADER_LEN..]);
    }

    #[test]
    fn cooked_zero_protocol_is_reclassified_only_on_loopback_ingress() {
        let broker = PacketBroker::try_new().unwrap();
        let endpoint = broker
            .subscribe(PacketSelector::new(
                PacketProtocol::All,
                Some(1),
                PacketView::Raw,
                true,
            ))
            .unwrap();
        let mut device = LoopbackDevice::try_new().unwrap();
        let context = PacketDeviceContext::new(1, &broker, None);
        let destination = [2, 8, 9, 10, 11, 12];
        let payload = b"non-IP payload";
        let protocol = 0;

        device
            .send_packet(
                context,
                PacketSendRequest::Cooked {
                    protocol,
                    destination: &destination,
                    payload,
                },
                Instant::ZERO,
            )
            .unwrap();
        broker.drain_staged();

        let outgoing = endpoint.try_receive(false).unwrap();
        assert_eq!(&outgoing.data()[..6], &destination);
        assert_eq!(&outgoing.data()[6..12], &[0; 6]);
        assert_eq!(&outgoing.data()[12..14], &protocol.to_be_bytes());
        assert_eq!(&outgoing.data()[14..], payload);
        assert_eq!(outgoing.metadata().protocol, protocol);
        assert_eq!(outgoing.metadata().packet_type, LinkPacketType::Outgoing);
        assert_eq!(&outgoing.metadata().address[..6], &[0; 6]);

        let mut ingress =
            PacketBuffer::new(vec![SmolPacketMetadata::EMPTY; 1], vec![0u8; payload.len()]);
        assert!(device.recv(context, &mut ingress, Instant::ZERO));
        broker.drain_staged();

        let host = endpoint.try_receive(false).unwrap();
        assert_eq!(host.data(), outgoing.data());
        assert_eq!(host.metadata().packet_type, LinkPacketType::OtherHost);
        assert_eq!(host.metadata().protocol, 0x0004);
    }
}
