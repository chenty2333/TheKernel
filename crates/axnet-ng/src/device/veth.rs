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
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::Device,
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
    /// Waker for the peer — we notify it when we send data.
    peer_waker: Arc<PollSet>,
}

fn new_packet_buffer() -> PacketBuffer<'static, ()> {
    PacketBuffer::new(
        vec![PacketMetadata::EMPTY; SOCKET_BUFFER_SIZE],
        vec![0u8; STANDARD_MTU * SOCKET_BUFFER_SIZE],
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
            peer_waker: waker_b.clone(),
        };
        let end_b = Self {
            name: name_b,
            rx_buffer: buf_b,
            peer_rx_buffer: buf_a,
            waker: waker_b,
            peer_waker: waker_a,
        };
        (end_a, end_b)
    }
}

impl Device for VethEnd {
    fn name(&self) -> &str {
        &self.name
    }

    fn recv(&mut self, buffer: &mut PacketBuffer<()>, _timestamp: Instant) -> bool {
        self.rx_buffer
            .lock()
            .dequeue()
            .ok()
            .is_some_and(|(_, rx_buf)| {
                buffer
                    .enqueue(rx_buf.len(), ())
                    .unwrap()
                    .copy_from_slice(rx_buf);
                true
            })
    }

    fn send(&mut self, next_hop: IpAddress, packet: &[u8], _timestamp: Instant) -> bool {
        match self.peer_rx_buffer.lock().enqueue(packet.len(), ()) {
            Ok(tx_buf) => {
                tx_buf.copy_from_slice(packet);
                // Wake the peer so it polls and picks up the packet.
                self.peer_waker.wake();
                false // recv readiness is on the OTHER stack, not ours
            }
            Err(_) => {
                warn!(
                    "veth {}: peer buffer full, dropping packet to {}",
                    self.name, next_hop
                );
                false
            }
        }
    }

    fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }
}
