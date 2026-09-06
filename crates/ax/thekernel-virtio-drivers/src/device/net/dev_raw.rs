use core::{hint::spin_loop, mem::ManuallyDrop};

use log::{debug, info, warn};
use zerocopy::AsBytes;

use super::{
    Config, EthernetAddress, Features, VirtioNetHdr, MIN_BUFFER_LEN, NET_HDR_SIZE, QUEUE_RECEIVE,
    QUEUE_TRANSMIT, SUPPORTED_FEATURES,
};
use crate::{hal::Hal, queue::VirtQueue, transport::Transport, volatile::volread, Error, Result};

const RESET_POLL_BUDGET: usize = 1024;

/// Result of the bounded network-device reset protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetOutcome {
    /// Device status was observed reset and queue owners may be reclaimed.
    Quiesced,
    /// The device did not acknowledge reset within the proof budget. DMA
    /// owners remain retained by the device object and are deliberately leaked
    /// by its destructor.
    Quarantined,
}

/// Raw driver for a VirtIO network device.
///
/// This is a raw version of the VirtIONet driver. It provides non-blocking
/// methods for transmitting and receiving raw slices, without the buffer
/// management. For more higher-level functions such as receive buffer backing,
/// see [`VirtIONet`].
///
/// [`VirtIONet`]: super::VirtIONet
pub struct VirtIONetRaw<H: Hal, T: Transport, const QUEUE_SIZE: usize> {
    // These fields are manually dropped so a failed bounded reset can retain
    // every queue/DMA owner instead of freeing memory the device may still
    // access.
    transport: ManuallyDrop<T>,
    mac: EthernetAddress,
    recv_queue: ManuallyDrop<VirtQueue<H, QUEUE_SIZE>>,
    send_queue: ManuallyDrop<VirtQueue<H, QUEUE_SIZE>>,
    /// Whether the transport has already been reset by the owning wrapper.
    ///
    /// The raw device is also used as a field of higher-level drivers.  Those
    /// drivers must quiesce this object before their DMA-backed buffer fields
    /// are dropped; keeping the state here makes that protocol explicit and
    /// makes the raw destructor idempotent instead of relying on field order.
    quiesced: bool,
    quarantined: bool,
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> VirtIONetRaw<H, T, QUEUE_SIZE> {
    #[inline]
    fn operational(&self) -> bool {
        !self.quiesced && !self.quarantined
    }

    /// Fence the raw queues after a completion protocol violation.  The used
    /// ring is intentionally left untouched: its owner mappings must remain
    /// available for teardown, while normal polling and notification are
    /// stopped permanently.
    pub fn quarantine(&mut self) {
        if self.quiesced {
            return;
        }
        self.send_queue.set_dev_notify(false);
        self.recv_queue.set_dev_notify(false);
        self.quarantined = true;
    }

    /// Whether a completion protocol violation has fenced this raw device.
    pub const fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    /// Create a new VirtIO-Net driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);
        info!("negotiated_features {:?}", negotiated_features);
        // read configuration space
        let config = transport.config_space::<Config>()?;
        let mac;
        // Safe because config points to a valid MMIO region for the config space.
        unsafe {
            mac = volread!(config, mac);
            debug!(
                "Got MAC={:02x?}, status={:?}",
                mac,
                volread!(config, status)
            );
        }
        let send_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;
        let recv_queue = VirtQueue::new(
            &mut transport,
            QUEUE_RECEIVE,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;

        transport.finish_init();

        Ok(VirtIONetRaw {
            transport: ManuallyDrop::new(transport),
            mac,
            recv_queue: ManuallyDrop::new(recv_queue),
            send_queue: ManuallyDrop::new(send_queue),
            quiesced: false,
            quarantined: false,
        })
    }

    /// Stop device DMA before any queue or packet buffer owner is released.
    ///
    /// VirtIO PCI cannot unset an individual queue, so clearing device status
    /// is the transport-level reset which proves that the device no longer
    /// owns descriptors.  The operation is intentionally idempotent: a wrapper can
    /// call it while its packet buffers are still alive and the raw destructor
    /// will not repeat the reset after the wrapper has returned.  Queue
    /// registers are deliberately left to the status-reset protocol: the
    /// generic transport trait's queue-unset operation is not timeout-bounded
    /// on all transports.
    pub fn reset_device(&mut self) -> ResetOutcome {
        if self.quiesced {
            return ResetOutcome::Quiesced;
        }
        if self.quarantined {
            return ResetOutcome::Quarantined;
        }

        self.disable_interrupts();
        self.transport
            .set_status(crate::transport::DeviceStatus::empty());
        for _ in 0..RESET_POLL_BUDGET {
            if self.transport.get_status().is_empty() {
                // A status reset is the transport-level DMA quiescence proof.
                // Do not call `Transport::queue_unset` here: the MMIO
                // implementation waits for a device read-back with no
                // timeout, which would turn this otherwise bounded teardown
                // into an unbounded spin on a wedged transport.  Once status
                // is zero, the VirtIO device is no longer permitted to fetch
                // either queue, and the queue owner can safely recycle its
                // exact descriptor mappings before dropping the transport.
                self.transport.mark_reset_complete();
                self.quiesced = true;
                return ResetOutcome::Quiesced;
            }
            spin_loop();
        }
        self.quarantine();
        ResetOutcome::Quarantined
    }

    /// Reports whether all queue-owned descriptor mappings have been
    /// explicitly reclaimed by their buffer owner after a successful reset.
    /// The raw API cannot reconstruct caller-owned slices, so it must retain
    /// the complete object if this is false rather than letting queue drop
    /// release an `H::share` mapping behind the caller's back.
    pub fn has_dma_owners(&self) -> bool {
        !self.recv_queue.is_empty()
            || !self.send_queue.is_empty()
            || self.recv_queue.has_live_indirect_lists()
            || self.send_queue.has_live_indirect_lists()
    }

    /// Reclaim one outstanding receive chain after reset proved quiescence.
    /// This releases both `H::share` mappings and any indirect table owned by
    /// the queue.
    ///
    /// # Safety
    ///
    /// `reset_device` must have returned [`ResetOutcome::Quiesced`], and
    /// `rx_buf` must be the exact buffer submitted for `token`.
    pub unsafe fn discard_receive(&mut self, token: u16, rx_buf: &mut [u8]) {
        let mut outputs = [rx_buf];
        // SAFETY: the caller supplies the exact buffer and proved quiescence.
        unsafe {
            self.recv_queue.discard_quiesced(token, &[], &mut outputs);
        }
    }

    /// Reclaim one outstanding transmit chain after reset proved quiescence.
    ///
    /// # Safety
    ///
    /// `reset_device` must have returned [`ResetOutcome::Quiesced`], and
    /// `tx_buf` must be the exact buffer submitted for `token`.
    pub unsafe fn discard_transmit(&mut self, token: u16, tx_buf: &[u8]) {
        let inputs = [tx_buf];
        let mut outputs: [&mut [u8]; 0] = [];
        // SAFETY: the caller supplies the exact buffer and proved quiescence.
        unsafe {
            self.send_queue
                .discard_quiesced(token, &inputs, &mut outputs);
        }
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        if !self.operational() {
            return false;
        }
        self.transport.ack_interrupt()
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        if !self.operational() {
            return;
        }
        self.send_queue.set_dev_notify(false);
        self.recv_queue.set_dev_notify(false);
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        if !self.operational() {
            return;
        }
        self.send_queue.set_dev_notify(true);
        self.recv_queue.set_dev_notify(true);
    }

    /// Get MAC address.
    pub fn mac_address(&self) -> EthernetAddress {
        self.mac
    }

    /// Whether can send packet.
    pub fn can_send(&self) -> bool {
        self.operational() && self.send_queue.available_desc() >= 2
    }

    /// Whether the length of the receive buffer is valid.
    fn check_rx_buf_len(rx_buf: &[u8]) -> Result<()> {
        if rx_buf.len() < MIN_BUFFER_LEN {
            warn!("Receive buffer len {} is too small", rx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Whether the length of the transmit buffer is valid.
    fn check_tx_buf_len(tx_buf: &[u8]) -> Result<()> {
        if tx_buf.len() < NET_HDR_SIZE {
            warn!("Transmit buffer len {} is too small", tx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Fill the header of the `buffer` with [`VirtioNetHdr`].
    ///
    /// If the `buffer` is not large enough, it returns [`Error::InvalidParam`].
    pub fn fill_buffer_header(&self, buffer: &mut [u8]) -> Result<usize> {
        if buffer.len() < NET_HDR_SIZE {
            return Err(Error::InvalidParam);
        }
        let header = VirtioNetHdr::default();
        buffer[..NET_HDR_SIZE].copy_from_slice(header.as_bytes());
        Ok(NET_HDR_SIZE)
    }

    /// Submits a request to transmit a buffer immediately without waiting for
    /// the transmission to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller needs to fill the `tx_buf` with a header by calling
    /// [`fill_buffer_header`] before transmission. Then it calls [`poll_transmit`]
    /// with the returned token to check whether the device has finished handling
    /// the request. Once it has, the caller must call [`transmit_complete`] with
    /// the same buffer before reading the result (transmitted length).
    ///
    /// # Safety
    ///
    /// `tx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`fill_buffer_header`]: Self::fill_buffer_header
    /// [`poll_transmit`]: Self::poll_transmit
    /// [`transmit_complete`]: Self::transmit_complete
    pub unsafe fn transmit_begin(&mut self, tx_buf: &[u8]) -> Result<u16> {
        if !self.operational() {
            return Err(Error::NotReady);
        }
        Self::check_tx_buf_len(tx_buf)?;
        let token = self.send_queue.add(&[tx_buf], &mut [])?;
        if self.send_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        Ok(token)
    }

    /// Fetches the token of the next completed transmission request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_transmit(&mut self) -> Option<u16> {
        if !self.operational() {
            return None;
        }
        self.send_queue.peek_used()
    }

    /// Completes a transmission operation which was started by [`transmit_begin`].
    /// Returns number of bytes transmitted.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`transmit_begin`] when it returned the token.
    ///
    /// [`transmit_begin`]: Self::transmit_begin
    pub unsafe fn transmit_complete(&mut self, token: u16, tx_buf: &[u8]) -> Result<usize> {
        if !self.operational() {
            return Err(Error::NotReady);
        }
        match self.send_queue.pop_used(token, &[tx_buf], &mut []) {
            Ok(len) => Ok(len as usize),
            Err(_) => {
                self.quarantine();
                Err(Error::Quarantined)
            }
        }
    }

    /// Submits a request to receive a buffer immediately without waiting for
    /// the reception to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller can then call [`poll_receive`] with the returned token to
    /// check whether the device has finished handling the request. Once it has,
    /// the caller must call [`receive_complete`] with the same buffer before
    /// reading the response.
    ///
    /// # Safety
    ///
    /// `rx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`poll_receive`]: Self::poll_receive
    /// [`receive_complete`]: Self::receive_complete
    pub unsafe fn receive_begin(&mut self, rx_buf: &mut [u8]) -> Result<u16> {
        if !self.operational() {
            return Err(Error::NotReady);
        }
        Self::check_rx_buf_len(rx_buf)?;
        let token = self.recv_queue.add(&[], &mut [rx_buf])?;
        if self.recv_queue.should_notify() {
            self.transport.notify(QUEUE_RECEIVE);
        }
        Ok(token)
    }

    /// Fetches the token of the next completed reception request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_receive(&self) -> Option<u16> {
        if !self.operational() {
            return None;
        }
        self.recv_queue.peek_used()
    }

    /// Completes a transmission operation which was started by [`receive_begin`].
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`receive_begin`] when it returned the token.
    ///
    /// [`receive_begin`]: Self::receive_begin
    pub unsafe fn receive_complete(
        &mut self,
        token: u16,
        rx_buf: &mut [u8],
    ) -> Result<(usize, usize)> {
        if !self.operational() {
            return Err(Error::NotReady);
        }
        let len = match self.recv_queue.pop_used(token, &[], &mut [rx_buf]) {
            Ok(len) => len as usize,
            Err(_) => {
                self.quarantine();
                return Err(Error::Quarantined);
            }
        };
        let Some(packet_len) = len.checked_sub(NET_HDR_SIZE) else {
            self.quarantine();
            return Err(Error::Quarantined);
        };
        Ok((NET_HDR_SIZE, packet_len))
    }

    /// Sends a packet to the network, and blocks until the request completed.
    pub fn send(&mut self, tx_buf: &[u8]) -> Result {
        if !self.operational() {
            return Err(Error::NotReady);
        }
        let header = VirtioNetHdr::default();
        if tx_buf.is_empty() {
            // Special case sending an empty packet, to avoid adding an empty buffer to the
            // virtqueue.
            self.send_queue.add_notify_wait_pop(
                &[header.as_bytes()],
                &mut [],
                &mut *self.transport,
            )?;
        } else {
            self.send_queue.add_notify_wait_pop(
                &[header.as_bytes(), tx_buf],
                &mut [],
                &mut *self.transport,
            )?;
        }
        Ok(())
    }

    /// Blocks and waits for a packet to be received.
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    pub fn receive_wait(&mut self, rx_buf: &mut [u8]) -> Result<(usize, usize)> {
        let token = unsafe { self.receive_begin(rx_buf)? };
        while self.poll_receive().is_none() {
            core::hint::spin_loop();
        }
        unsafe { self.receive_complete(token, rx_buf) }
    }
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> Drop for VirtIONetRaw<H, T, QUEUE_SIZE> {
    fn drop(&mut self) {
        if self.reset_device() != ResetOutcome::Quiesced {
            // A quarantined transport and its queues are intentionally leaked;
            // dropping them would free DMA memory without a quiescence proof.
            return;
        }
        if self.has_dma_owners() {
            // Reset stopped the device, but the raw API has no ownership
            // record from which it can safely supply the original slices to
            // `H::unshare`.  Keep the queue/transport quarantined instead of
            // freeing a live mapping or an indirect descriptor table.
            return;
        }
        // The high-level driver reclaims outstanding chains first. Release
        // queue DMA and transport only after those mappings are gone.
        unsafe {
            ManuallyDrop::drop(&mut self.recv_queue);
            ManuallyDrop::drop(&mut self.send_queue);
            ManuallyDrop::drop(&mut self.transport);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};
    use core::ptr::NonNull;
    use std::sync::Mutex;

    use super::*;
    use crate::{
        device::net::Status,
        hal::fake::FakeHal,
        transport::{
            fake::{FakeTransport, QueueStatus, State},
            DeviceType,
        },
        volatile::ReadOnly,
    };

    #[test]
    fn reset_reclaims_in_flight_receive_and_transmit_owners() {
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..Default::default()
        }));
        let mut config = Config {
            mac: ReadOnly::new([0x02, 0, 0, 0, 0, 1]),
            status: ReadOnly::new(Status::LINK_UP),
            max_virtqueue_pairs: ReadOnly::new(1),
            mtu: ReadOnly::new(1500),
        };
        let transport = FakeTransport {
            device_type: DeviceType::Network,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config),
            state: state.clone(),
        };
        let mut dev = VirtIONetRaw::<FakeHal, _, 4>::new(transport).unwrap();
        let mut buffer = vec![0u8; MIN_BUFFER_LEN];
        // Keep a receive descriptor in flight while reset runs.  The backing
        // buffer is intentionally dropped only after reset quiesces DMA and
        // the original owner reclaims its descriptor chain.
        let rx_token = unsafe { dev.receive_begin(&mut buffer).unwrap() };
        let tx = vec![0u8; MIN_BUFFER_LEN];
        let tx_token = unsafe { dev.transmit_begin(&tx).unwrap() };
        assert!(state.lock().unwrap().queues[QUEUE_RECEIVE as usize].descriptors != 0);
        assert!(state.lock().unwrap().queues[QUEUE_TRANSMIT as usize].descriptors != 0);

        assert_eq!(dev.reset_device(), ResetOutcome::Quiesced);
        assert!(dev.has_dma_owners());
        // Reclaiming after reset must release the exact queue-owned mappings
        // and any indirect table before the backing buffers are dropped.
        unsafe {
            dev.discard_receive(rx_token, &mut buffer);
            dev.discard_transmit(tx_token, &tx);
        }
        assert!(!dev.has_dma_owners());
        let state = state.lock().unwrap();
        assert!(state.status.is_empty());
        // Reset quiesces device access; it does not dismantle queue storage.
        // Exact-owner discard above, rather than zero fake register addresses,
        // proves it is safe to release the backing buffers.
        assert_eq!(
            unsafe { dev.receive_begin(&mut buffer) },
            Err(Error::NotReady)
        );
        assert_eq!(unsafe { dev.transmit_begin(&tx) }, Err(Error::NotReady));
        drop(state);
        drop(buffer);
    }

    #[test]
    fn malformed_used_id_quarantines_without_repoll_or_owner_drop() {
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..Default::default()
        }));
        let mut config = Config {
            mac: ReadOnly::new([0x02, 0, 0, 0, 0, 2]),
            status: ReadOnly::new(Status::LINK_UP),
            max_virtqueue_pairs: ReadOnly::new(1),
            mtu: ReadOnly::new(1500),
        };
        let transport = FakeTransport {
            device_type: DeviceType::Network,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config),
            state,
        };
        let mut dev = VirtIONetRaw::<FakeHal, _, 4>::new(transport).unwrap();
        let mut buffer = vec![0u8; MIN_BUFFER_LEN];
        let token = unsafe { dev.receive_begin(&mut buffer).unwrap() };

        // Leave the descriptor pending but publish a used-ring id which is
        // outside the queue. The first completion attempt must fence the
        // device; retry polling must not keep observing the corrupt entry.
        dev.recv_queue.set_used_for_test(0, u32::MAX, 0, 1);
        assert_eq!(
            unsafe { dev.receive_complete(token, &mut buffer) },
            Err(Error::Quarantined)
        );
        assert_eq!(dev.poll_receive(), None);
        assert!(dev.has_dma_owners());
        assert_eq!(
            unsafe { dev.receive_complete(token, &mut buffer) },
            Err(Error::NotReady)
        );
        assert!(dev.has_dma_owners());
    }
}
