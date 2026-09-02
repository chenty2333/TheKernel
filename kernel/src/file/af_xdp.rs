//! AF_XDP socket state: long-term UMEM pins, shared control rings and leases.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::PageSize;
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex;

use super::{
    FileLike, FileMmapProtection, FileMmapRequest, FixedSharedMmapRegion, IoDst, IoSrc,
    IoctlContext, Kstat, PreparedFileMmap, PseudoInode,
};
use crate::{
    mm::{
        PinnedUserSegmentsMut, SharedPages, UserMemoryCapability,
        try_pin_user_segments_to_user_longterm_with,
    },
    task::NetworkNamespace,
};

pub(crate) const AF_XDP: u32 = 44;
pub(crate) const SOL_XDP: u32 = 283;
pub(crate) const XDP_RX_RING: u32 = 2;
pub(crate) const XDP_TX_RING: u32 = 3;
pub(crate) const XDP_UMEM_REG: u32 = 4;
pub(crate) const XDP_UMEM_FILL_RING: u32 = 5;
pub(crate) const XDP_UMEM_COMPLETION_RING: u32 = 6;
pub(crate) const XDP_SHARED_UMEM: u16 = 1;
pub(crate) const XDP_USE_NEED_WAKEUP: u16 = 1 << 3;

const MIN_CHUNK: u32 = 2048;
const MAX_CHUNK: u32 = 4096;
const MAX_RING: u32 = 1 << 20;
pub(crate) const XDP_PGOFF_RX_RING: u64 = 0;
pub(crate) const XDP_PGOFF_TX_RING: u64 = 0x8000_0000;
pub(crate) const XDP_UMEM_PGOFF_FILL_RING: u64 = 0x1_0000_0000;
pub(crate) const XDP_UMEM_PGOFF_COMPLETION_RING: u64 = 0x1_8000_0000;
const RING_HEADER: usize = 64;
const DESC_SIZE: usize = 16;
const fn mmap_offset(pgoff: u64) -> u64 {
    pgoff << 12
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct XdpUmemLayout {
    pub address: u64,
    pub length: u64,
    pub chunk_size: u32,
    pub headroom: u32,
    pub flags: u32,
    pub tx_metadata_len: u32,
}
impl XdpUmemLayout {
    pub(crate) fn validate(self) -> AxResult<()> {
        let unaligned = self.flags & 1 != 0;
        if self.address == 0
            || self.length == 0
            || self.flags & !0x7 != 0
            || !self.chunk_size.is_power_of_two()
            || !(MIN_CHUNK..=MAX_CHUNK).contains(&self.chunk_size)
            || self.headroom >= self.chunk_size
            || self.length % u64::from(self.chunk_size) != 0
            || (!unaligned && self.address % u64::from(self.chunk_size) != 0)
            || self.tx_metadata_len > self.chunk_size.saturating_sub(self.headroom)
        {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct XdpRingLayout {
    pub entries: u32,
}
impl XdpRingLayout {
    pub(crate) fn validate(self) -> AxResult<()> {
        (self.entries != 0 && self.entries.is_power_of_two() && self.entries <= MAX_RING)
            .then_some(())
            .ok_or(AxError::InvalidInput)
    }
}

/// Ring control pages are kernel-owned fixed shared pages. UMEM itself stays
/// user-owned but is retained by a long-term MM pin below.
struct XdpRing {
    entries: u32,
    pages: Arc<SharedPages>,
    region: FixedSharedMmapRegion,
    producer: AtomicU32,
    consumer: AtomicU32,
}
impl XdpRing {
    fn new(layout: XdpRingLayout, offset: u64) -> AxResult<Arc<Self>> {
        layout.validate()?;
        let bytes = RING_HEADER
            .checked_add(
                (layout.entries as usize)
                    .checked_mul(DESC_SIZE)
                    .ok_or(AxError::NoMemory)?,
            )
            .ok_or(AxError::NoMemory)?
            .next_multiple_of(PageSize::Size4K as usize);
        let pages = Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        let region = FixedSharedMmapRegion::try_new(
            offset,
            pages.clone(),
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        Arc::try_new(Self {
            entries: layout.entries,
            pages,
            region,
            producer: AtomicU32::new(0),
            consumer: AtomicU32::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn mapped_index(&self, offset: usize) -> AxResult<u32> {
        let mut raw = [0; 4];
        self.pages.read_bytes(offset, &mut raw)?;
        Ok(u32::from_ne_bytes(raw))
    }
    // Producer is owned by userspace for TX/fill and by the kernel for
    // RX/completion. Conversely consumer is owned by the kernel for TX/fill
    // and by userspace for RX/completion. The mapped control words are the
    // cross-domain source of truth; atomics only remember this side's cursor.
    fn free(&self) -> AxResult<u32> {
        Ok(self.entries.saturating_sub(
            self.producer
                .load(Ordering::Acquire)
                .wrapping_sub(self.mapped_index(4)?),
        ))
    }
    fn ready(&self) -> AxResult<u32> {
        Ok(self
            .mapped_index(0)?
            .wrapping_sub(self.consumer.load(Ordering::Acquire)))
    }
    /// User-visible free space on a TX ring: userspace owns producer while
    /// the kernel owns consumer, the inverse of `free()` above.
    fn user_tx_free(&self) -> AxResult<u32> {
        Ok(self.entries.saturating_sub(
            self.mapped_index(0)?
                .wrapping_sub(self.consumer.load(Ordering::Acquire)),
        ))
    }
    /// User-visible completed records on an RX/completion ring: the kernel
    /// producer is local and userspace owns the mapped consumer cursor.
    fn user_ready(&self) -> AxResult<u32> {
        Ok(self
            .producer
            .load(Ordering::Acquire)
            .wrapping_sub(self.mapped_index(4)?))
    }
    fn publish(&self, descriptor: &[u8; DESC_SIZE]) -> AxResult<bool> {
        if self.free()? == 0 {
            return Ok(false);
        }
        let producer = self.producer.load(Ordering::Relaxed);
        let offset = RING_HEADER + (producer & (self.entries - 1)) as usize * DESC_SIZE;
        self.pages.write_bytes(offset, descriptor)?;
        core::sync::atomic::fence(Ordering::Release);
        self.producer
            .store(producer.wrapping_add(1), Ordering::Release);
        self.pages
            .write_bytes(0, &producer.wrapping_add(1).to_ne_bytes())?;
        Ok(true)
    }
    fn consume(&self) -> AxResult<Option<[u8; DESC_SIZE]>> {
        if self.ready()? == 0 {
            return Ok(None);
        }
        let consumer = self.consumer.load(Ordering::Relaxed);
        let mut descriptor = [0; DESC_SIZE];
        self.pages.read_bytes(
            RING_HEADER + (consumer & (self.entries - 1)) as usize * DESC_SIZE,
            &mut descriptor,
        )?;
        core::sync::atomic::fence(Ordering::Acquire);
        self.consumer
            .store(consumer.wrapping_add(1), Ordering::Release);
        self.pages
            .write_bytes(4, &consumer.wrapping_add(1).to_ne_bytes())?;
        Ok(Some(descriptor))
    }
}

struct XdpUmem {
    layout: XdpUmemLayout,
    capability: UserMemoryCapability,
    _pins: PinnedUserSegmentsMut,
}
struct State {
    umem: Option<XdpUmem>,
    ifindex: Option<u32>,
    queue_id: Option<u32>,
    flags: u16,
    rx: Option<Arc<XdpRing>>,
    tx: Option<Arc<XdpRing>>,
    fill: Option<Arc<XdpRing>>,
    completion: Option<Arc<XdpRing>>,
    pending_completions: Vec<[u8; DESC_SIZE]>,
}
struct QueueLease {
    namespace: Weak<NetworkNamespace>,
    ifindex: u32,
    queue_id: u32,
    endpoint: Weak<XdpEndpoint>,
}
static QUEUE_LEASES: Mutex<alloc::vec::Vec<QueueLease>> = Mutex::new(alloc::vec::Vec::new());

pub(crate) fn redirect_ingress(
    namespace: &Arc<NetworkNamespace>,
    ifindex: u32,
    packet: &[u8],
) -> AxResult<bool> {
    // Never call into an endpoint while holding the global lease registry:
    // bind/close publish leases before they take endpoint state, whereas
    // ingress needs endpoint state to consume a fill descriptor.
    let mut endpoints = Vec::new();
    {
        let mut leases = QUEUE_LEASES.lock();
        leases.retain(|lease| {
            lease.namespace.upgrade().is_some() && lease.endpoint.upgrade().is_some()
        });
        endpoints
            .try_reserve(leases.len())
            .map_err(|_| AxError::NoMemory)?;
        for lease in leases.iter() {
            if lease.ifindex == ifindex
                && lease
                    .namespace
                    .upgrade()
                    .is_some_and(|candidate| Arc::ptr_eq(&candidate, namespace))
                && let Some(endpoint) = lease.endpoint.upgrade()
            {
                endpoints.push(endpoint);
            }
        }
    }
    for endpoint in endpoints {
        match endpoint.redirect_packet(packet, 0) {
            Ok(true) => return Ok(true),
            Ok(false) | Err(AxError::BadState) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// This exact object is retained by XSKMAP entries. It is never reconstructed
/// from an FD, and final close retracts the queue lease before releasing UMEM.
pub struct XdpEndpoint {
    net_ns: Arc<NetworkNamespace>,
    state: Mutex<State>,
    closed: AtomicBool,
    generation: AtomicU32,
    waiters: PollSet,
}
impl XdpEndpoint {
    fn new(net_ns: Arc<NetworkNamespace>) -> Arc<Self> {
        Arc::new(Self {
            net_ns,
            state: Mutex::new(State {
                umem: None,
                ifindex: None,
                queue_id: None,
                flags: 0,
                rx: None,
                tx: None,
                fill: None,
                completion: None,
                pending_completions: Vec::new(),
            }),
            closed: AtomicBool::new(false),
            generation: AtomicU32::new(1),
            waiters: PollSet::new(),
        })
    }
    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }
    pub(crate) fn accepts_xdp_redirect(
        &self,
        namespace: &Arc<NetworkNamespace>,
        ifindex: u32,
    ) -> bool {
        !self.closed.load(Ordering::Acquire)
            && Arc::ptr_eq(&self.net_ns, namespace)
            && self.state.lock().ifindex == Some(ifindex)
    }
    pub(crate) fn is_bound_live(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.state.lock().ifindex.is_some()
    }
    pub(crate) fn options(&self) -> u32 {
        0
    }
    pub(crate) fn register_umem(
        &self,
        cap: &UserMemoryCapability,
        layout: XdpUmemLayout,
    ) -> AxResult<()> {
        layout.validate()?;
        let length = usize::try_from(layout.length).map_err(|_| AxError::InvalidInput)?;
        // The pin also registers a LongTerm range reservation with the MM;
        // unmap/remap/exit cannot recycle the frames before this owner drops.
        let pins =
            try_pin_user_segments_to_user_longterm_with(cap, layout.address as *mut u8, length)
                .ok_or(AxError::BadAddress)?;
        let mut state = self.state.lock();
        if self.closed.load(Ordering::Acquire) || state.umem.is_some() {
            return Err(AxError::BadState);
        }
        state.umem = Some(XdpUmem {
            layout,
            capability: cap.clone(),
            _pins: pins,
        });
        Ok(())
    }
    pub(crate) fn configure_ring(&self, kind: u32, layout: XdpRingLayout) -> AxResult<()> {
        let offset = match kind {
            XDP_RX_RING => mmap_offset(XDP_PGOFF_RX_RING),
            XDP_TX_RING => mmap_offset(XDP_PGOFF_TX_RING),
            XDP_UMEM_FILL_RING => mmap_offset(XDP_UMEM_PGOFF_FILL_RING),
            XDP_UMEM_COMPLETION_RING => mmap_offset(XDP_UMEM_PGOFF_COMPLETION_RING),
            _ => return Err(AxError::InvalidInput),
        };
        let ring = XdpRing::new(layout, offset)?;
        let mut state = self.state.lock();
        if self.closed.load(Ordering::Acquire) || state.umem.is_none() || state.ifindex.is_some() {
            return Err(AxError::BadState);
        }
        let slot = match kind {
            XDP_RX_RING => &mut state.rx,
            XDP_TX_RING => &mut state.tx,
            XDP_UMEM_FILL_RING => &mut state.fill,
            XDP_UMEM_COMPLETION_RING => &mut state.completion,
            _ => unreachable!(),
        };
        if slot.is_some() {
            return Err(AxError::ResourceBusy);
        }
        *slot = Some(ring);
        Ok(())
    }
    pub(crate) fn bind(self: &Arc<Self>, ifindex: u32, queue_id: u32, flags: u16) -> AxResult<()> {
        // axnet's current device receive ABI exposes one ingress queue.  Do
        // not pretend a nonzero hardware queue was bound and then redirect
        // it according to lease iteration order; reject it until the typed
        // device queue identity is propagated through the router hook.
        if ifindex == 0 || queue_id != 0 || flags & !(XDP_SHARED_UMEM | XDP_USE_NEED_WAKEUP) != 0 {
            return Err(AxError::InvalidInput);
        }
        if !self
            .net_ns
            .stack()
            .interfaces()
            .iter()
            .any(|i| i.index == ifindex)
        {
            return Err(AxError::NoSuchDevice);
        }
        // The lease registry is the outer lock everywhere (including close),
        // preventing a bind/ingress or bind/close ABBA cycle.
        let mut leases = QUEUE_LEASES.lock();
        leases.retain(|lease| {
            lease.namespace.upgrade().is_some() && lease.endpoint.upgrade().is_some()
        });
        if leases.iter().any(|lease| {
            lease
                .namespace
                .upgrade()
                .is_some_and(|ns| Arc::ptr_eq(&ns, &self.net_ns))
                && lease.ifindex == ifindex
                && lease.queue_id == queue_id
        }) {
            return Err(AxError::ResourceBusy);
        }
        leases.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let mut state = self.state.lock();
        if self.closed.load(Ordering::Acquire) || state.umem.is_none() || state.ifindex.is_some() {
            return Err(AxError::BadState);
        }
        // A configured TX ring must have its return path before any lease is
        // published.  Once the consumer advances, completion is mandatory.
        if state.tx.is_some() && state.completion.is_none() {
            return Err(AxError::BadState);
        }
        if let Some(tx_entries) = state.tx.as_ref().map(|tx| tx.entries as usize) {
            state
                .pending_completions
                .try_reserve(tx_entries)
                .map_err(|_| AxError::NoMemory)?;
        }
        leases.push(QueueLease {
            namespace: Arc::downgrade(&self.net_ns),
            ifindex,
            queue_id,
            endpoint: Arc::downgrade(self),
        });
        state.ifindex = Some(ifindex);
        state.queue_id = Some(queue_id);
        state.flags = flags;
        Ok(())
    }
    pub(crate) fn prepare_mmap(
        &self,
        request: FileMmapRequest,
    ) -> AxResult<Option<PreparedFileMmap>> {
        let state = self.state.lock();
        let ring = match request.offset() {
            value if value == mmap_offset(XDP_PGOFF_RX_RING) => state.rx.as_ref(),
            value if value == mmap_offset(XDP_PGOFF_TX_RING) => state.tx.as_ref(),
            value if value == mmap_offset(XDP_UMEM_PGOFF_FILL_RING) => state.fill.as_ref(),
            value if value == mmap_offset(XDP_UMEM_PGOFF_COMPLETION_RING) => {
                state.completion.as_ref()
            }
            _ => return Err(AxError::InvalidInput),
        };
        ring.ok_or(AxError::BadState)?.region.prepare(request)
    }
    pub(crate) fn redirect_rx(&self, address: u64, length: u32, options: u32) -> AxResult<bool> {
        let state = self.state.lock();
        let umem = state.umem.as_ref().ok_or(AxError::BadState)?;
        if self.closed.load(Ordering::Acquire)
            || state.ifindex.is_none()
            || address
                .checked_add(u64::from(length))
                .is_none_or(|end| end > umem.layout.length)
        {
            return Err(AxError::BadState);
        }
        let mut descriptor = [0; DESC_SIZE];
        descriptor[..8].copy_from_slice(&address.to_ne_bytes());
        descriptor[8..12].copy_from_slice(&length.to_ne_bytes());
        descriptor[12..].copy_from_slice(&options.to_ne_bytes());
        let result = state
            .rx
            .as_ref()
            .ok_or(AxError::BadState)?
            .publish(&descriptor)?;
        if result {
            self.waiters.wake();
        }
        Ok(result)
    }
    /// Redirects one router-owned packet into a userspace-provided fill frame.
    /// The long-term pin keeps the UMEM stable while the retained capability
    /// performs the checked copy; neither raw user pointers nor an arbitrary
    /// XSKMAP fd are trusted here.
    pub(crate) fn redirect_packet(&self, packet: &[u8], options: u32) -> AxResult<bool> {
        let state = self.state.lock();
        if self.closed.load(Ordering::Acquire) || state.ifindex.is_none() {
            return Err(AxError::BadState);
        }
        // A TX-only AF_XDP binding legitimately has no ingress rings.  It
        // must not claim or drop unrelated device ingress.
        let (rx_ring, fill_ring) = match (state.rx.as_ref(), state.fill.as_ref()) {
            (Some(rx), Some(fill)) => (rx, fill),
            _ => return Ok(false),
        };
        let umem = state.umem.as_ref().ok_or(AxError::BadState)?;
        // Check the destination before consuming a userspace-owned fill
        // frame.  A full RX ring leaves the fill descriptor in userspace.
        if rx_ring.free()? == 0 {
            return Ok(false);
        }
        let descriptor = fill_ring.consume()?.ok_or(AxError::WouldBlock)?;
        let frame = u64::from_ne_bytes(descriptor[..8].try_into().unwrap());
        let chunk = u64::from(umem.layout.chunk_size);
        if frame >= umem.layout.length
            || (umem.layout.flags & 1 == 0 && frame % chunk != 0)
            || packet.len() > umem.layout.chunk_size.saturating_sub(umem.layout.headroom) as usize
        {
            return Err(AxError::InvalidInput);
        }
        let offset = frame
            .checked_add(u64::from(umem.layout.headroom))
            .ok_or(AxError::InvalidInput)?;
        let end = offset
            .checked_add(packet.len() as u64)
            .ok_or(AxError::InvalidInput)?;
        if end > umem.layout.length {
            return Err(AxError::InvalidInput);
        }
        let user = umem
            .layout
            .address
            .checked_add(offset)
            .ok_or(AxError::BadAddress)? as usize;
        umem.capability
            .write_bytes(user, packet)
            .map_err(crate::mm::map_usercopy_error)?;
        let mut rx = [0; DESC_SIZE];
        rx[..8].copy_from_slice(&offset.to_ne_bytes());
        rx[8..12].copy_from_slice(&(packet.len() as u32).to_ne_bytes());
        rx[12..].copy_from_slice(&options.to_ne_bytes());
        let published = rx_ring.publish(&rx)?;
        if published {
            self.waiters.wake();
        }
        Ok(published)
    }
    /// Flushes completed descriptors before accepting new TX work.  A full
    /// userspace-owned completion ring must retain kernel ownership rather
    /// than silently losing a descriptor after the device has consumed it.
    fn flush_completions_locked(&self, state: &mut State) -> AxResult<bool> {
        let completion = state.completion.as_ref().ok_or(AxError::BadState)?.clone();
        let mut published = false;
        while !state.pending_completions.is_empty() {
            if !completion.publish(&state.pending_completions[0])? {
                break;
            }
            state.pending_completions.remove(0);
            published = true;
        }
        Ok(published)
    }

    /// Returns a consumed TX descriptor to userspace, retaining it locally if
    /// the completion ring is temporarily full.  This is called after every
    /// device result, including a rejected or failed send: XDP ownership has
    /// already moved from the TX ring to the kernel at that point.
    fn complete_consumed_tx(&self, descriptor: [u8; DESC_SIZE], generation: u32) -> AxResult<()> {
        let mut state = self.state.lock();
        if self.closed.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != generation
        {
            return Ok(());
        }
        let published = self.flush_completions_locked(&mut state)?;
        let completion = state.completion.as_ref().ok_or(AxError::BadState)?.clone();
        if completion.publish(&descriptor)? {
            self.waiters.wake();
            return Ok(());
        }
        debug_assert!(
            state.pending_completions.len() < state.tx.as_ref().map_or(0, |tx| tx.entries as usize)
        );
        state.pending_completions.push(descriptor);
        if published {
            self.waiters.wake();
        }
        Ok(())
    }

    /// Doorbells userspace-owned XDP TX descriptors into the selected link
    /// device.  Descriptor bytes are copied out of the long-term pin before
    /// leaving the endpoint lock, so close/teardown can quiesce the lease
    /// without a raw user pointer or a live MM escaping into axnet.
    pub(crate) fn kick_tx(&self) -> AxResult<usize> {
        let mut sent = 0usize;
        loop {
            let (descriptor, generation, work) = {
                let mut state = self.state.lock();
                if self.closed.load(Ordering::Acquire) {
                    return Err(AxError::BadState);
                }
                if self.flush_completions_locked(&mut state)? {
                    self.waiters.wake();
                }
                // Do not consume another user descriptor while a prior one
                // still awaits a completion slot; ordering is observable.
                if !state.pending_completions.is_empty() {
                    break;
                }
                let descriptor = match state.tx.as_ref().ok_or(AxError::BadState)?.consume()? {
                    Some(value) => value,
                    None => break,
                };
                // The consumer update made this TX slot writable again.
                self.waiters.wake();
                let generation = self.generation.load(Ordering::Acquire);
                let work = (|| -> AxResult<(Vec<u8>, u32)> {
                    let umem = state.umem.as_ref().ok_or(AxError::BadState)?;
                    let address = u64::from_ne_bytes(descriptor[..8].try_into().unwrap());
                    let length = u32::from_ne_bytes(descriptor[8..12].try_into().unwrap()) as usize;
                    let options = u32::from_ne_bytes(descriptor[12..].try_into().unwrap());
                    // Multi-buffer TX is not implemented yet; do not transmit
                    // a partial frame while nevertheless returning ownership.
                    if options != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    let end = address
                        .checked_add(length as u64)
                        .ok_or(AxError::InvalidInput)?;
                    if address >= umem.layout.length || end > umem.layout.length {
                        return Err(AxError::InvalidInput);
                    }
                    if umem.layout.flags & 1 == 0 {
                        let base = address
                            .checked_sub(u64::from(umem.layout.headroom))
                            .ok_or(AxError::InvalidInput)?;
                        if base % u64::from(umem.layout.chunk_size) != 0
                            || length
                                > umem.layout.chunk_size.saturating_sub(umem.layout.headroom)
                                    as usize
                        {
                            return Err(AxError::InvalidInput);
                        }
                    }
                    let mut frame = Vec::new();
                    frame
                        .try_reserve_exact(length)
                        .map_err(|_| AxError::NoMemory)?;
                    frame.resize(length, core::mem::MaybeUninit::uninit());
                    let user = umem
                        .layout
                        .address
                        .checked_add(address)
                        .ok_or(AxError::BadAddress)? as usize;
                    umem.capability
                        .read_bytes(user, &mut frame)
                        .map_err(crate::mm::map_usercopy_error)?;
                    // The checked usercopy initialized every byte. `u8` and
                    // `MaybeUninit<u8>` have identical allocation layouts,
                    // so retain the already-reserved frame without a second
                    // allocation before handing it to the network stack.
                    let length = frame.len();
                    let capacity = frame.capacity();
                    let ptr = frame.as_mut_ptr().cast::<u8>();
                    core::mem::forget(frame);
                    let frame = unsafe { Vec::from_raw_parts(ptr, length, capacity) };
                    Ok((frame, state.ifindex.ok_or(AxError::BadState)?))
                })();
                (descriptor, generation, work)
            };
            let (frame, ifindex) = match work {
                Ok(work) => work,
                Err(error) => {
                    self.complete_consumed_tx(descriptor, generation)?;
                    return Err(error);
                }
            };
            let protocol = if frame.len() >= 14 {
                u16::from_be_bytes([frame[12], frame[13]])
            } else {
                0
            };
            // Ring TX is still local output and must not bypass the
            // namespace's nftables/iptables policy just because it did not
            // originate from an AF_PACKET endpoint.
            let result = (|| -> AxResult<()> {
                crate::syscall::iptables_output_verdict(&self.net_ns)?;
                crate::file::netlink::nft_output_verdict(&self.net_ns)?;
                self.net_ns.stack().send_packet_unattributed(
                    ifindex,
                    axnet::packet::PacketSendRequest::Raw {
                        protocol,
                        frame: &frame,
                    },
                )
            })();
            // A device rejection still consumes this descriptor.  Complete it
            // first, then surface the transmit error to the doorbell caller.
            self.complete_consumed_tx(descriptor, generation)?;
            sent = sent.saturating_add(1);
            result?;
        }
        Ok(sent)
    }
    pub(crate) fn binding(&self) -> Option<(u16, u32, u32)> {
        let state = self.state.lock();
        Some((state.flags, state.ifindex?, state.queue_id?))
    }
    fn poll_events(&self) -> IoEvents {
        let state = self.state.lock();
        if self.closed.load(Ordering::Acquire) {
            return IoEvents::HANGUP | IoEvents::ERROR;
        }
        let mut events = IoEvents::empty();
        events.set(
            IoEvents::READABLE,
            state
                .rx
                .as_ref()
                .is_some_and(|r| r.user_ready().unwrap_or(0) != 0)
                || state
                    .completion
                    .as_ref()
                    .is_some_and(|r| r.user_ready().unwrap_or(0) != 0),
        );
        events.set(
            IoEvents::WRITABLE,
            state
                .tx
                .as_ref()
                .is_some_and(|r| r.user_tx_free().unwrap_or(0) != 0),
        );
        events
    }
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        QUEUE_LEASES.lock().retain(|lease| {
            !lease
                .endpoint
                .upgrade()
                .is_some_and(|target| core::ptr::eq(Arc::as_ptr(&target), self))
        });
        *self.state.lock() = State {
            umem: None,
            ifindex: None,
            queue_id: None,
            flags: 0,
            rx: None,
            tx: None,
            fill: None,
            completion: None,
            pending_completions: Vec::new(),
        };
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.waiters.close();
    }
}

pub(crate) struct XdpSocket {
    endpoint: Arc<XdpEndpoint>,
    nonblocking: AtomicBool,
    inode: PseudoInode,
}
impl XdpSocket {
    pub(crate) fn try_new(net_ns: Arc<NetworkNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            endpoint: XdpEndpoint::new(net_ns),
            nonblocking: AtomicBool::new(false),
            inode: PseudoInode::socket(),
        })
        .map_err(|_| AxError::NoMemory)
    }
    pub(crate) fn endpoint(&self) -> Arc<XdpEndpoint> {
        self.endpoint.clone()
    }
    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        self.endpoint.net_namespace()
    }
}
impl FileLike for XdpSocket {
    fn read(&self, _: &mut IoDst) -> AxResult<usize> {
        Err(LinuxError::EOPNOTSUPP.into())
    }
    fn write(&self, _: &mut IoSrc) -> AxResult<usize> {
        Err(LinuxError::EOPNOTSUPP.into())
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }
    fn update_timestamps(
        &self,
        _: Option<axfs_ng_vfs::Timestamp>,
        _: Option<axfs_ng_vfs::Timestamp>,
        _: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        Ok(())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult<()> {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn ioctl(&self, _: &IoctlContext, _: u32, _: usize) -> AxResult<usize> {
        Err(LinuxError::ENOTTY.into())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"socket:[xdp]")))
    }
    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        self.endpoint.prepare_mmap(request)
    }
}
impl Pollable for XdpSocket {
    fn poll(&self) -> IoEvents {
        self.endpoint.poll_events()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let mut registration = axpoll::PreparedPollRegistration::try_new(1)?;
        registration.arm(&self.endpoint.waiters, context.waker())?;
        registration.commit()
    }
}
impl Drop for XdpSocket {
    fn drop(&mut self) {
        self.endpoint.close();
    }
}
