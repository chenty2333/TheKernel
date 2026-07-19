use alloc::{string::String, sync::Arc, vec};
use core::task::Waker;

use axpoll::PollSet;
use axsync::Mutex;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
    wire::IpAddress,
};

use crate::{
    consts::{PACKET_QUEUE_LEN, STANDARD_MTU},
    device::{Device, DevicePollBridge, DeviceStats, InterfaceKind},
    packet::PacketDeviceContext,
};

/// One end of a virtual ethernet pair.
///
/// Packets sent from one end appear as received on the other end,
/// enabling communication between two separate [`NetStack`](crate::NetStack)
/// instances (network namespaces).
pub struct VethEnd {
    name: String,
    /// Incoming packets for this end (peer writes here, we read).
    rx_buffer: Arc<Mutex<PacketBuffer<'static, ()>>>,
    /// Incoming packets for the peer (we write here on send).
    peer_rx_buffer: Arc<Mutex<PacketBuffer<'static, ()>>>,
    /// Waker for this end — notified when peer sends us data.
    waker: Arc<PollSet>,
    waker_bridge: DevicePollBridge,
    /// Waker for the peer — we notify it when we send data.
    peer_waker: Arc<PollSet>,
    stats: DeviceStats,
}

fn new_packet_buffer() -> PacketBuffer<'static, ()> {
    PacketBuffer::new(
        vec![PacketMetadata::EMPTY; PACKET_QUEUE_LEN],
        vec![0u8; STANDARD_MTU * PACKET_QUEUE_LEN],
    )
}

impl VethEnd {
    /// Create a paired veth device. Returns `(end_a, end_b)`.
    pub fn new_pair(name_a: String, name_b: String) -> (Self, Self) {
        let buf_a = Arc::new(Mutex::new(new_packet_buffer()));
        let buf_b = Arc::new(Mutex::new(new_packet_buffer()));
        let waker_a = Arc::new(PollSet::new());
        let waker_b = Arc::new(PollSet::new());

        let end_a = Self {
            name: name_a,
            rx_buffer: buf_a.clone(),
            peer_rx_buffer: buf_b.clone(),
            waker: waker_a.clone(),
            waker_bridge: DevicePollBridge::new(),
            peer_waker: waker_b.clone(),
            stats: DeviceStats::default(),
        };
        let end_b = Self {
            name: name_b,
            rx_buffer: buf_b,
            peer_rx_buffer: buf_a,
            waker: waker_b,
            waker_bridge: DevicePollBridge::new(),
            peer_waker: waker_a,
            stats: DeviceStats::default(),
        };
        (end_a, end_b)
    }
}

impl Device for VethEnd {
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

    fn recv(
        &mut self,
        _context: PacketDeviceContext<'_>,
        buffer: &mut PacketBuffer<()>,
        _timestamp: Instant,
    ) -> bool {
        let mut rx_buffer = self.rx_buffer.lock();
        let Ok((_, rx_buf)) = rx_buffer.dequeue() else {
            return false;
        };
        let len = rx_buf.len();
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
        _context: PacketDeviceContext<'_>,
        next_hop: IpAddress,
        packet: &[u8],
        _timestamp: Instant,
    ) -> bool {
        match self.peer_rx_buffer.lock().enqueue(packet.len(), ()) {
            Ok(tx_buf) => {
                tx_buf.copy_from_slice(packet);
                self.stats.record_tx(packet.len());
                // Wake the peer so it polls and picks up the packet.
                self.peer_waker.wake();
                false // recv readiness is on the OTHER stack, not ours
            }
            Err(_) => {
                self.stats.record_tx_drop();
                warn!(
                    "veth {}: peer buffer full, dropping packet to {}",
                    self.name, next_hop
                );
                false
            }
        }
    }

    fn register_waker(&self, waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
        self.waker_bridge.refresh(&self.waker, waker)
    }
}

impl Drop for VethEnd {
    fn drop(&mut self) {
        self.waker_bridge.cancel(&self.waker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::LinkHardwareType;

    #[test]
    fn ethernet_interface_kind_does_not_imply_packet_capabilities() {
        let (device, _peer) = VethEnd::new_pair("veth0".into(), "veth1".into());
        assert_eq!(device.interface_kind(), InterfaceKind::Ethernet);

        let capabilities = device.packet_capabilities();
        assert_eq!(capabilities.hardware_type, LinkHardwareType::Ethernet);
        assert!(!capabilities.raw_receive);
        assert!(!capabilities.raw_send);
        assert!(!capabilities.cooked_receive);
        assert!(!capabilities.cooked_send);
        assert_eq!(capabilities.link_header_len, 0);
        assert_eq!(capabilities.address_len, 0);
    }
}
