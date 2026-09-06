use alloc::{sync::Arc, vec::Vec};
use core::mem::ManuallyDrop;

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_net::{EthernetAddress, NetBuf, NetBufBox, NetBufPool, NetBufPtr, NetDriverOps};
use virtio_drivers::{
    Hal,
    device::net::{ResetOutcome, VirtIONetRaw as InnerDev},
    transport::Transport,
};

use crate::as_dev_err;

const NET_BUF_LEN: usize = 1526;

/// The VirtIO network device driver.
///
/// `QS` is the VirtIO queue size.
pub struct VirtIoNetDev<H: Hal, T: Transport, const QS: usize> {
    // All packet owners are manually dropped so a failed bounded reset can
    // quarantine them together with the raw device instead of freeing DMA
    // memory while the device may still access it.
    rx_buffers: ManuallyDrop<[Option<NetBufBox>; QS]>,
    tx_buffers: ManuallyDrop<[Option<NetBufBox>; QS]>,
    free_tx_bufs: ManuallyDrop<Vec<NetBufBox>>,
    /// Owners retained after an internal token/slot invariant failure.  The
    /// device is quarantined in that case, so these owners must outlive the
    /// queue rather than being returned to the pool while a descriptor may
    /// still reference them.
    // `Some(token)` retains a queue-owned receive descriptor. `None` retains
    // a completed buffer handed to the caller whose recycle raced a device
    // quarantine; NetBufPtr has no Drop, so that owner must be reconstructed
    // and kept here explicitly as well.
    quarantine_rx: ManuallyDrop<Vec<(Option<u16>, NetBufBox)>>,
    quarantine_tx: ManuallyDrop<Vec<(u16, NetBufBox)>>,
    buf_pool: ManuallyDrop<Arc<NetBufPool>>,
    inner: ManuallyDrop<InnerDev<H, T, QS>>,
    irq: Option<usize>,
    quarantined: bool,
}

unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetDev<H, T, QS> {}
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetDev<H, T, QS> {}

impl<H: Hal, T: Transport, const QS: usize> VirtIoNetDev<H, T, QS> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T, irq: Option<usize>) -> DevResult<Self> {
        // 0. Create a new driver instance.
        const NONE_BUF: Option<NetBufBox> = None;
        let inner = InnerDev::new(transport).map_err(as_dev_err)?;
        let rx_buffers = [NONE_BUF; QS];
        let tx_buffers = [NONE_BUF; QS];
        let buf_pool = NetBufPool::new(2 * QS, NET_BUF_LEN)?;
        let free_tx_bufs = Vec::with_capacity(QS);

        let mut dev = Self {
            rx_buffers: ManuallyDrop::new(rx_buffers),
            inner: ManuallyDrop::new(inner),
            tx_buffers: ManuallyDrop::new(tx_buffers),
            free_tx_bufs: ManuallyDrop::new(free_tx_bufs),
            quarantine_rx: ManuallyDrop::new(Vec::with_capacity(1)),
            quarantine_tx: ManuallyDrop::new(Vec::with_capacity(1)),
            buf_pool: ManuallyDrop::new(buf_pool),
            irq,
            quarantined: false,
        };

        // 1. Fill all rx buffers.
        for (i, rx_buf_place) in dev.rx_buffers.iter_mut().enumerate() {
            let mut rx_buf = dev.buf_pool.alloc_boxed().ok_or(DevError::NoMemory)?;
            // Safe because the buffer lives as long as the queue.
            let token = unsafe {
                dev.inner
                    .receive_begin(rx_buf.raw_buf_mut())
                    .map_err(as_dev_err)?
            };
            assert_eq!(token, i as u16);
            *rx_buf_place = Some(rx_buf);
        }

        // 2. Allocate all tx buffers.
        for _ in 0..QS {
            let mut tx_buf = dev.buf_pool.alloc_boxed().ok_or(DevError::NoMemory)?;
            // Fill header
            let hdr_len = dev
                .inner
                .fill_buffer_header(tx_buf.raw_buf_mut())
                .or(Err(DevError::InvalidParam))?;
            tx_buf.set_header_len(hdr_len);
            dev.free_tx_bufs.push(tx_buf);
        }

        // 3. Return the driver instance.
        Ok(dev)
    }
}

impl<H: Hal, T: Transport, const QS: usize> Drop for VirtIoNetDev<H, T, QS> {
    fn drop(&mut self) {
        // The packet buffers and pool are declared before `inner`, so relying
        // on Rust's field drop order would release DMA memory before the
        // virtqueues are torn down.  Reset the device while every buffer is
        // still owned by this object; `VirtIONetRaw` makes the operation
        // idempotent for its own destructor.
        let inner = &mut *self.inner;
        if inner.reset_device() != ResetOutcome::Quiesced || self.quarantined {
            // Keep every packet owner and the raw queue/transport retained by
            // their ManuallyDrop fields. Releasing any one of them without a
            // quiescence proof would permit DMA use-after-free.
            return;
        }

        // Reclaim every descriptor still owned by either queue.  The token
        // index is the queue head and the matching array slot is the sole
        // owner of the original buffer, so this also releases H::share and
        // indirect descriptor tables exactly once.
        for (token, slot) in (&mut *self.rx_buffers).iter_mut().enumerate() {
            if let Some(mut rx_buf) = slot.take() {
                // SAFETY: reset_device proved quiescence and this slot owns
                // the exact buffer submitted for the queue token.
                unsafe { inner.discard_receive(token as u16, rx_buf.raw_buf_mut()) };
            }
        }
        for (token, slot) in (&mut *self.tx_buffers).iter_mut().enumerate() {
            if let Some(tx_buf) = slot.take() {
                // SAFETY: reset_device proved quiescence and this slot owns
                // the exact buffer submitted for the queue token.
                unsafe { inner.discard_transmit(token as u16, tx_buf.packet_with_header()) };
            }
        }

        if inner.has_dma_owners() {
            // Every queue descriptor must have a corresponding packet owner
            // before the manually-managed fields can be released.  A missing
            // slot is a bookkeeping failure; retaining the whole object is a
            // safe quarantine and keeps both H::share mappings and indirect
            // descriptor storage alive.
            return;
        }

        // Drop packet owners before the pool, and the pool before the queue.
        unsafe {
            ManuallyDrop::drop(&mut self.rx_buffers);
            ManuallyDrop::drop(&mut self.tx_buffers);
            ManuallyDrop::drop(&mut self.free_tx_bufs);
            ManuallyDrop::drop(&mut self.quarantine_rx);
            ManuallyDrop::drop(&mut self.quarantine_tx);
            ManuallyDrop::drop(&mut self.buf_pool);
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

impl<H: Hal, T: Transport, const QS: usize> VirtIoNetDev<H, T, QS> {
    fn quarantine(&mut self) {
        self.inner.quarantine();
        self.quarantined = true;
    }
}

fn complete_tx_owner<F>(slot: &mut Option<NetBufBox>, complete: F) -> DevResult<NetBufBox>
where
    F: FnOnce(&[u8]) -> DevResult<()>,
{
    let owner = slot.as_ref().ok_or(DevError::BadState)?;
    // Keep the owner in the slot until the queue has successfully popped and
    // unshared its descriptor. In particular, a WrongToken or NotReady result
    // must not drop/reuse this DMA-backed buffer.
    complete(owner.packet_with_header())?;
    slot.take().ok_or(DevError::BadState)
}

fn complete_rx_owner<F>(slot: &mut Option<NetBufBox>, complete: F) -> DevResult<NetBufPtr>
where
    F: FnOnce(&mut NetBuf) -> DevResult<(usize, usize)>,
{
    let owner = slot.as_mut().ok_or(DevError::BadState)?;
    let (header_len, packet_len) = complete(owner)?;
    owner.set_header_len(header_len);
    owner.set_packet_len(packet_len);
    slot.take()
        .ok_or(DevError::BadState)
        .map(NetBuf::into_buf_ptr)
}

fn retain_quarantine_rx_owner(
    ledger: &mut Vec<(Option<u16>, NetBufBox)>,
    token: Option<u16>,
    rx_buf: NetBufPtr,
) {
    // SAFETY: every NetBufPtr admitted here was created by
    // `NetBuf::into_buf_ptr`; quarantine is the terminal owner transfer and
    // prevents the raw pointer from being dropped or reused without its
    // pool-backed NetBuf owner.
    let rx_buf = unsafe { NetBuf::from_buf_ptr(rx_buf) };
    ledger.push((token, rx_buf));
}

/// Take a transmit owner only after the requested packet length has been
/// validated against the free buffer. Keeping the preflight borrow separate
/// from the `pop` is important: an invalid request must leave the owner in
/// the free list for the next valid request.
fn alloc_tx_owner(free_tx_bufs: &mut Vec<NetBufBox>, size: usize) -> DevResult<NetBufPtr> {
    let free_buf = free_tx_bufs.last().ok_or(DevError::NoMemory)?;
    let total_len = free_buf
        .header_len()
        .checked_add(size)
        .ok_or(DevError::InvalidParam)?;
    if total_len > free_buf.capacity() {
        return Err(DevError::InvalidParam);
    }

    let mut net_buf = free_tx_bufs
        .pop()
        .expect("free TX owner disappeared during exclusive allocation");
    net_buf.set_packet_len(size);
    Ok(net_buf.into_buf_ptr())
}

impl<H: Hal, T: Transport, const QS: usize> BaseDriverOps for VirtIoNetDev<H, T, QS> {
    fn device_name(&self) -> &str {
        "virtio-net"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn irq_num(&self) -> Option<usize> {
        self.irq
    }
}

impl<H: Hal, T: Transport, const QS: usize> NetDriverOps for VirtIoNetDev<H, T, QS> {
    #[inline]
    fn mac_address(&self) -> EthernetAddress {
        EthernetAddress(self.inner.mac_address())
    }

    #[inline]
    fn can_transmit(&self) -> bool {
        !self.quarantined && !self.free_tx_bufs.is_empty() && self.inner.can_send()
    }

    #[inline]
    fn can_receive(&self) -> bool {
        !self.quarantined && self.inner.poll_receive().is_some()
    }

    #[inline]
    fn rx_queue_size(&self) -> usize {
        QS
    }

    #[inline]
    fn tx_queue_size(&self) -> usize {
        QS
    }

    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult {
        if self.quarantined {
            // The caller may be returning a buffer completed before another
            // operation fenced this device. NetBufPtr has no Drop; retain the
            // exact owner instead of returning an error with a leaked raw
            // pointer.
            retain_quarantine_rx_owner(&mut self.quarantine_rx, None, rx_buf);
            return Err(DevError::BadState);
        }
        let mut rx_buf = unsafe { NetBuf::from_buf_ptr(rx_buf) };
        // Safe because we take the ownership of `rx_buf` back to `rx_buffers`,
        // it lives as long as the queue.
        let new_token = match unsafe { self.inner.receive_begin(rx_buf.raw_buf_mut()) } {
            Ok(token) => token,
            Err(error) => {
                if self.inner.is_quarantined() {
                    self.quarantine_rx.push((None, rx_buf));
                    self.quarantine();
                } else {
                    drop(rx_buf);
                }
                return Err(as_dev_err(error));
            }
        };
        // `rx_buffers[new_token]` is expected to be `None` since it was taken
        // away at `Self::receive()` and has not been added back. Retain the
        // exact owner if a corrupt token cannot be represented by the fixed
        // slot array.
        let Some(slot) = self.rx_buffers.get_mut(usize::from(new_token)) else {
            self.quarantine_rx.push((Some(new_token), rx_buf));
            self.quarantine();
            return Err(DevError::BadState);
        };
        if slot.is_some() {
            self.quarantine_rx.push((Some(new_token), rx_buf));
            self.quarantine();
            return Err(DevError::BadState);
        }
        *slot = Some(rx_buf);
        Ok(())
    }

    fn recycle_tx_buffers(&mut self) -> DevResult {
        if self.quarantined {
            return Err(DevError::BadState);
        }
        while let Some(token) = self.inner.poll_transmit() {
            let Some(slot) = self.tx_buffers.get_mut(usize::from(token)) else {
                self.quarantine();
                return Err(DevError::BadState);
            };
            let tx_buf = match complete_tx_owner(slot, |buffer| unsafe {
                self.inner
                    .transmit_complete(token, buffer)
                    .map(|_| ())
                    .map_err(as_dev_err)
            }) {
                Ok(tx_buf) => tx_buf,
                Err(error) => {
                    self.quarantine();
                    return Err(error);
                }
            };
            // Recycle the buffer only after the queue has released its DMA
            // mapping successfully.
            self.free_tx_bufs.push(tx_buf);
        }
        Ok(())
    }

    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        if self.quarantined {
            return Err(DevError::BadState);
        }
        // 0. prepare tx buffer.
        let tx_buf = unsafe { NetBuf::from_buf_ptr(tx_buf) };
        // 1. transmit packet.
        let token = match unsafe { self.inner.transmit_begin(tx_buf.packet_with_header()) } {
            Ok(token) => token,
            Err(error) => {
                if self.inner.is_quarantined() {
                    self.quarantine();
                }
                return Err(as_dev_err(error));
            }
        };
        let Some(slot) = self.tx_buffers.get_mut(usize::from(token)) else {
            self.quarantine_tx.push((token, tx_buf));
            self.quarantine();
            return Err(DevError::BadState);
        };
        if slot.is_some() {
            self.quarantine_tx.push((token, tx_buf));
            self.quarantine();
            return Err(DevError::BadState);
        }
        *slot = Some(tx_buf);
        Ok(())
    }

    fn receive(&mut self) -> DevResult<NetBufPtr> {
        if self.quarantined {
            return Err(DevError::BadState);
        }
        self.inner.ack_interrupt();
        if let Some(token) = self.inner.poll_receive() {
            let Some(slot) = self.rx_buffers.get_mut(usize::from(token)) else {
                self.quarantine();
                return Err(DevError::BadState);
            };
            match complete_rx_owner(slot, |rx_buf| unsafe {
                self.inner
                    .receive_complete(token, rx_buf.raw_buf_mut())
                    .map_err(as_dev_err)
            }) {
                Ok(rx_buf) => Ok(rx_buf),
                Err(error) => {
                    self.quarantine();
                    Err(error)
                }
            }
        } else {
            Err(DevError::Again)
        }
    }

    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        if self.quarantined {
            return Err(DevError::BadState);
        }
        alloc_tx_owner(&mut self.free_tx_bufs, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_used_error_retains_tx_owner() {
        let pool = NetBufPool::new(1, NET_BUF_LEN).unwrap();
        let mut slot = Some(pool.alloc_boxed().unwrap());

        // Model a raw queue pop_used/transmit_complete failure. The owner
        // must remain attached to the token slot so its DMA mapping cannot be
        // returned to the pool and reused by another request.
        let result = complete_tx_owner(&mut slot, |_| Err(DevError::Again));
        assert!(matches!(result, Err(DevError::Again)));
        assert!(slot.is_some());

        // Once completion succeeds, ownership may move to the free list.
        let owner = complete_tx_owner(&mut slot, |_| Ok(())).unwrap();
        assert!(slot.is_none());
        drop(owner);
    }

    #[test]
    fn quarantined_rx_ptr_is_reconstructed_into_owner_ledger() {
        let pool = NetBufPool::new(1, NET_BUF_LEN).unwrap();
        let rx_buf = pool.alloc_boxed().unwrap().into_buf_ptr();
        let mut ledger = Vec::with_capacity(1);

        // A caller-owned receive pointer has no RAII drop.  The quarantine
        // path must reconstruct the exact NetBuf before returning the error.
        retain_quarantine_rx_owner(&mut ledger, None, rx_buf);
        assert_eq!(ledger.len(), 1);
        assert!(pool.alloc_boxed().is_none());

        drop(ledger);
        assert!(pool.alloc_boxed().is_some());
    }

    #[test]
    fn oversized_tx_alloc_keeps_free_owner() {
        let pool = NetBufPool::new(1, NET_BUF_LEN).unwrap();
        let mut free_tx_bufs = Vec::with_capacity(1);
        let mut tx_buf = pool.alloc_boxed().unwrap();
        let header_len = core::mem::size_of::<virtio_drivers::device::net::VirtioNetHdr>();
        tx_buf.set_header_len(header_len);
        free_tx_bufs.push(tx_buf);

        // The failed request must not consume the only reusable owner.
        assert!(matches!(
            alloc_tx_owner(&mut free_tx_bufs, NET_BUF_LEN),
            Err(DevError::InvalidParam)
        ));
        assert_eq!(free_tx_bufs.len(), 1);
        assert!(pool.alloc_boxed().is_none());

        // A subsequent valid request can still use that same owner.
        let ptr = alloc_tx_owner(&mut free_tx_bufs, NET_BUF_LEN - header_len).unwrap();
        assert!(free_tx_bufs.is_empty());
        // SAFETY: `ptr` was produced by `NetBuf::into_buf_ptr` above.
        drop(unsafe { NetBuf::from_buf_ptr(ptr) });
        assert!(pool.alloc_boxed().is_some());
    }
}
