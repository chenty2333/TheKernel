//! Ethernet TAP endpoint with one bounded L2 queue pair per file description.

use crate::{
    consts::{PACKET_QUEUE_LEN, STANDARD_MTU},
    device::{Device, DevicePollBridge, DeviceStats, IngressPacketBuffer, InterfaceKind, RxStep},
    packet::PacketDeviceContext,
};
use alloc::{string::String, sync::Arc, vec};
use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::Mutex;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};
use smoltcp::{
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
    wire::IpAddress,
};

const ETH: usize = 14;
const IPV4: u16 = 0x0800;
const IPV6: u16 = 0x86dd;
fn queue() -> PacketBuffer<'static, ()> {
    PacketBuffer::new(
        vec![PacketMetadata::EMPTY; PACKET_QUEUE_LEN],
        vec![0; (STANDARD_MTU + ETH) * PACKET_QUEUE_LEN],
    )
}

struct TapQueue {
    ingress: Mutex<PacketBuffer<'static, ()>>,
    egress: Mutex<PacketBuffer<'static, ()>>,
    ingress_ready: PollSet,
    egress_ready: PollSet,
}
struct TapQueues {
    queues: Mutex<alloc::vec::Vec<Arc<TapQueue>>>,
    ingress_ready: PollSet,
    rx_next: AtomicUsize,
    tx_next: AtomicUsize,
}

/// One file-description queue attachment of an Ethernet TAP interface.
pub struct TapHandle {
    queues: Arc<TapQueues>,
    queue: Arc<TapQueue>,
}
impl TapHandle {
    pub fn try_write_frame(&self, frame: &[u8]) -> AxResult {
        if frame.len() < ETH || frame.len() > STANDARD_MTU + ETH {
            return Err(AxError::InvalidInput);
        }
        let mut ingress = self.queue.ingress.lock();
        let dst = ingress
            .enqueue(frame.len(), ())
            .map_err(|_| AxError::WouldBlock)?;
        dst.copy_from_slice(frame);
        drop(ingress);
        self.queue.ingress_ready.wake();
        self.queues.ingress_ready.wake();
        Ok(())
    }
    pub fn try_read_frame(&self, dst: &mut [u8]) -> AxResult<usize> {
        let mut egress = self.queue.egress.lock();
        let (_, frame) = egress.dequeue().map_err(|_| AxError::WouldBlock)?;
        if dst.len() < frame.len() {
            return Err(AxError::OutOfRange);
        }
        dst[..frame.len()].copy_from_slice(frame);
        Ok(frame.len())
    }
    pub fn ingress_ready(&self) -> &PollSet {
        &self.queue.ingress_ready
    }
    pub fn egress_ready(&self) -> &PollSet {
        &self.queue.egress_ready
    }
    pub fn has_egress_frame(&self) -> bool {
        !self.queue.egress.lock().is_empty()
    }
    /// Prepares all allocations before publishing the new queue under one lock.
    pub fn attach_queue(&self) -> AxResult<Arc<Self>> {
        let queue = Arc::try_new(TapQueue {
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
    /// Removes exactly this OFD attachment. A second detach fails.
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

/// Router-side Ethernet TAP.
pub struct TapDevice {
    name: String,
    mac: [u8; 6],
    queues: Arc<TapQueues>,
    bridge: DevicePollBridge,
    stats: DeviceStats,
}
impl TapDevice {
    pub fn new(name: String) -> AxResult<(Self, Arc<TapHandle>)> {
        if name.is_empty() || name.len() > 15 || name.as_bytes().contains(&0) {
            return Err(AxError::InvalidInput);
        }
        let first = Arc::try_new(TapQueue {
            ingress: Mutex::new(queue()),
            egress: Mutex::new(queue()),
            ingress_ready: PollSet::new(),
            egress_ready: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let queues = Arc::try_new(TapQueues {
            queues: Mutex::new(alloc::vec![first.clone()]),
            ingress_ready: PollSet::new(),
            rx_next: AtomicUsize::new(0),
            tx_next: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)?;
        let handle = Arc::try_new(TapHandle {
            queues: queues.clone(),
            queue: first,
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok((
            Self {
                name,
                mac: [0x02, 0, 0, 0, 0, 1],
                queues,
                bridge: DevicePollBridge::new(),
                stats: DeviceStats::default(),
            },
            handle,
        ))
    }
}
impl Device for TapDevice {
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
        Some(self.mac)
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
        _: Instant,
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
        let Ok((_, frame)) = ingress.dequeue() else {
            return RxStep::Idle;
        };
        if frame.len() < ETH {
            self.stats.record_rx_drop();
            return RxStep::Consumed;
        }
        let proto = u16::from_be_bytes([frame[12], frame[13]]);
        if !matches!(proto, IPV4 | IPV6) {
            self.stats.record_rx(frame.len());
            return RxStep::Consumed;
        }
        let packet = &frame[ETH..];
        let Ok(dst) = buffer.enqueue(packet.len(), context.interface_index()) else {
            self.stats.record_rx_drop();
            return RxStep::Consumed;
        };
        dst.copy_from_slice(packet);
        self.stats.record_rx(frame.len());
        RxStep::Delivered
    }
    fn send(
        &mut self,
        _: PacketDeviceContext<'_>,
        _: IpAddress,
        packet: &[u8],
        _: Instant,
    ) -> bool {
        let proto = match packet.first().map(|byte| byte >> 4) {
            Some(4) => IPV4,
            Some(6) => IPV6,
            _ => {
                self.stats.record_tx_drop();
                return false;
            }
        };
        let queues = self.queues.queues.lock();
        if queues.is_empty() {
            self.stats.record_tx_drop();
            return false;
        }
        let queue = &queues[self.queues.tx_next.fetch_add(1, Ordering::Relaxed) % queues.len()];
        let mut egress = queue.egress.lock();
        let Ok(dst) = egress.enqueue(packet.len() + ETH, ()) else {
            self.stats.record_tx_drop();
            return false;
        };
        dst[..6].fill(0xff);
        dst[6..12].copy_from_slice(&self.mac);
        dst[12..14].copy_from_slice(&proto.to_be_bytes());
        dst[ETH..].copy_from_slice(packet);
        self.stats.record_tx(packet.len() + ETH);
        drop(egress);
        queue.egress_ready.wake();
        false
    }
    fn register_waker(&self, waker: &Waker) -> Result<(), axpoll::PollRegistrationError> {
        self.bridge.refresh(&self.queues.ingress_ready, waker)
    }
    fn stop_rx_waker(&self) {
        self.bridge.cancel(&self.queues.ingress_ready);
    }
}
impl Drop for TapDevice {
    fn drop(&mut self) {
        self.bridge.cancel(&self.queues.ingress_ready);
    }
}
