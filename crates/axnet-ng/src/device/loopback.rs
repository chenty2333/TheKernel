use alloc::vec;
use core::task::Waker;

use axpoll::PollSet;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
    wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address},
};

use crate::{
    consts::{LOOPBACK_MTU, PACKET_QUEUE_LEN},
    device::{Device, DeviceStats, InterfaceKind},
};

pub struct LoopbackDevice {
    buffer: PacketBuffer<'static, ()>,
    poll: PollSet,
    stats: DeviceStats,
}
impl LoopbackDevice {
    pub fn new() -> Self {
        let buffer = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; PACKET_QUEUE_LEN],
            vec![0u8; LOOPBACK_MTU * PACKET_QUEUE_LEN],
        );
        Self {
            buffer,
            poll: PollSet::new(),
            stats: DeviceStats::default(),
        }
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

    fn recv(&mut self, buffer: &mut PacketBuffer<()>, _timestamp: Instant) -> bool {
        let Ok((_, rx_buf)) = self.buffer.dequeue() else {
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

    fn send(&mut self, next_hop: IpAddress, packet: &[u8], _timestamp: Instant) -> bool {
        match self.buffer.enqueue(packet.len(), ()) {
            Ok(tx_buf) => {
                tx_buf.copy_from_slice(packet);
                self.stats.record_tx(packet.len());
                self.poll.wake();
                true
            }
            Err(_) => {
                self.stats.record_tx_drop();
                warn!(
                    "Loopback device buffer is full, dropping packet to {}",
                    next_hop
                );
                false
            }
        }
    }

    fn register_waker(&self, waker: &Waker) {
        self.poll.register(waker);
    }
}
