//! In-kernel TUN data-plane endpoint.
//!
//! `TunHandle` is retained by the Linux character-device file while
//! `TunDevice` is owned by the namespace router.  Their queues are shared but
//! directional, so closing the control fd never hands a stale packet buffer to
//! a recycled interface index.

use alloc::{string::String, sync::Arc, vec};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::Mutex;
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
    wire::IpAddress,
};

use crate::{
    consts::{PACKET_QUEUE_LEN, STANDARD_MTU},
    device::{Device, DevicePollBridge, DeviceStats, IngressPacketBuffer, InterfaceKind, RxStep},
    packet::PacketDeviceContext,
};

fn queue() -> PacketBuffer<'static, ()> {
    PacketBuffer::new(
        vec![PacketMetadata::EMPTY; PACKET_QUEUE_LEN],
        vec![0; STANDARD_MTU * PACKET_QUEUE_LEN],
    )
}

struct TunQueue {
    ingress: Mutex<PacketBuffer<'static, ()>>,
    egress: Mutex<PacketBuffer<'static, ()>>,
    ingress_ready: PollSet,
    egress_ready: PollSet,
}
struct TunQueues {
    queues: Mutex<alloc::vec::Vec<Arc<TunQueue>>>,
    ingress_ready: PollSet,
    rx_next: AtomicUsize,
    tx_next: AtomicUsize,
}

/// File-facing half of a TUN interface. Packets written here enter the
/// namespace IP router; packets read here were emitted by that router.
pub struct TunHandle {
    queues: Arc<TunQueues>,
    queue: Arc<TunQueue>,
}

impl TunHandle {
    pub fn try_write_packet(&self, packet: &[u8]) -> AxResult {
        if packet.is_empty() || packet.len() > STANDARD_MTU {
            return Err(AxError::InvalidInput);
        }
        let mut ingress = self.queue.ingress.lock();
        let dst = ingress
            .enqueue(packet.len(), ())
            .map_err(|_| AxError::WouldBlock)?;
        dst.copy_from_slice(packet);
        drop(ingress);
        self.queue.ingress_ready.wake();
        self.queues.ingress_ready.wake();
        Ok(())
    }

    pub fn try_read_packet(&self, dst: &mut [u8]) -> AxResult<usize> {
        let mut egress = self.queue.egress.lock();
        let (_, packet) = egress.dequeue().map_err(|_| AxError::WouldBlock)?;
        if dst.len() < packet.len() {
            return Err(AxError::OutOfRange);
        }
        dst[..packet.len()].copy_from_slice(packet);
        Ok(packet.len())
    }

    pub fn ingress_ready(&self) -> &PollSet {
        &self.queue.ingress_ready
    }
    pub fn egress_ready(&self) -> &PollSet {
        &self.queue.egress_ready
    }
    pub fn has_egress_packet(&self) -> bool {
        !self.queue.egress.lock().is_empty()
    }
    pub fn attach_queue(&self) -> AxResult<Arc<Self>> {
        let queue = Arc::try_new(TunQueue {
            ingress: Mutex::new(queue()),
            egress: Mutex::new(queue()),
            ingress_ready: PollSet::new(),
            egress_ready: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let handle = Arc::try_new(Self {
            queues: self.queues.clone(),
            queue: queue.clone(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let mut queues = self.queues.queues.lock();
        queues.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        queues.push(queue);
        Ok(handle)
    }
    pub fn detach_queue(&self) -> AxResult {
        let mut queues = self.queues.queues.lock();
        let before = queues.len();
        queues.retain(|queue| !Arc::ptr_eq(queue, &self.queue));
        if queues.len() == before {
            Err(AxError::NoSuchDevice)
        } else {
            Ok(())
        }
    }
}

/// Router-owned Layer-3 TUN interface.
pub struct TunDevice {
    name: String,
    queues: Arc<TunQueues>,
    bridge: DevicePollBridge,
    stats: DeviceStats,
}

impl TunDevice {
    pub fn new(name: String) -> AxResult<(Self, Arc<TunHandle>)> {
        if name.is_empty() || name.len() > 15 || name.as_bytes().contains(&0) {
            return Err(AxError::InvalidInput);
        }
        let first = Arc::try_new(TunQueue {
            ingress: Mutex::new(queue()),
            egress: Mutex::new(queue()),
            ingress_ready: PollSet::new(),
            egress_ready: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let queues = Arc::try_new(TunQueues {
            queues: Mutex::new(alloc::vec![first.clone()]),
            ingress_ready: PollSet::new(),
            rx_next: AtomicUsize::new(0),
            tx_next: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)?;
        let handle = Arc::try_new(TunHandle {
            queues: queues.clone(),
            queue: first,
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok((
            Self {
                name,
                queues,
                bridge: DevicePollBridge::new(),
                stats: DeviceStats::default(),
            },
            handle,
        ))
    }
}

impl Device for TunDevice {
    fn name(&self) -> &str {
        &self.name
    }
    fn stats(&self) -> DeviceStats {
        self.stats
    }
    // The smoltcp namespace interface is IP-medium.  Keep the existing
    // Ethernet-shaped control-plane type only for link reporting; this device
    // explicitly advertises no AF_PACKET link capability.
    fn interface_kind(&self) -> InterfaceKind {
        InterfaceKind::Ethernet
    }
    fn mtu(&self) -> usize {
        STANDARD_MTU
    }
    fn has_rx_backlog(&self) -> bool {
        self.queues
            .queues
            .lock()
            .iter()
            .any(|queue| !queue.ingress.lock().is_empty())
    }
    fn recv(
        &mut self,
        context: PacketDeviceContext<'_>,
        buffer: &mut IngressPacketBuffer,
        _timestamp: Instant,
    ) -> RxStep {
        let queues = self.queues.queues.lock();
        if queues.is_empty() {
            return RxStep::Idle;
        }
        let start = self.queues.rx_next.fetch_add(1, Ordering::Relaxed) % queues.len();
        let Some(index) = (0..queues.len())
            .map(|offset| (start + offset) % queues.len())
            .find(|index| !queues[*index].ingress.lock().is_empty())
        else {
            return RxStep::Idle;
        };
        let mut ingress = queues[index].ingress.lock();
        let Ok((_, packet)) = ingress.dequeue() else {
            return RxStep::Idle;
        };
        let len = packet.len();
        let Ok(dst) = buffer.enqueue(len, context.interface_index()) else {
            self.stats.record_rx_drop();
            return RxStep::Consumed;
        };
        dst.copy_from_slice(packet);
        self.stats.record_rx(len);
        RxStep::Delivered
    }
    fn send(
        &mut self,
        _context: PacketDeviceContext<'_>,
        _next_hop: IpAddress,
        packet: &[u8],
        _timestamp: Instant,
    ) -> bool {
        let queues = self.queues.queues.lock();
        if queues.is_empty() {
            self.stats.record_tx_drop();
            return false;
        }
        let queue = &queues[self.queues.tx_next.fetch_add(1, Ordering::Relaxed) % queues.len()];
        let mut egress = queue.egress.lock();
        match egress.enqueue(packet.len(), ()) {
            Ok(dst) => {
                dst.copy_from_slice(packet);
                self.stats.record_tx(packet.len());
                drop(egress);
                queue.egress_ready.wake();
                false
            }
            Err(_) => {
                self.stats.record_tx_drop();
                false
            }
        }
    }
    fn register_waker(&self, waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
        self.bridge.refresh(&self.queues.ingress_ready, waker)
    }
    fn stop_rx_waker(&self) {
        self.bridge.cancel(&self.queues.ingress_ready);
    }
}

impl Drop for TunDevice {
    fn drop(&mut self) {
        self.bridge.cancel(&self.queues.ingress_ready);
    }
}
