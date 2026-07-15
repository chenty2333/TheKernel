use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    mem::{ManuallyDrop, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::{Mutex, spin::SpinNoIrq};
use spin::RwLock;

use crate::{
    CMsgData, RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx,
    buffer::SocketBufferLimits,
    consts::{TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
    socket::SocketFilter,
    unix::{
        BindSlot, Transport, TransportOps, UnixSocketAddr,
        queue::{
            PermitSendError, Receiver, ReserveError, SendPermit, Sender, TryRecvError, try_bounded,
        },
    },
};

struct Packet {
    data: Vec<u8>,
    cmsg: Vec<CMsgData>,
    sender: UnixSocketAddr,
    charge: usize,
}

// Byte accounting is the primary socket-buffer limit. This independent slot
// ceiling also bounds queue-node metadata if callers send very small
// datagrams, and lets the underlying channel enforce the invariant directly.
const UNIX_DGRAM_QUEUE_SLOTS: usize = 1024;
const UNIX_DGRAM_CLEANUP_SLOTS: usize = 16_384;
const DEFERRED_CLEANUP_NODE_BUDGET: usize = 16;
const DEFERRED_CLEANUP_PACKET_BUDGET: usize = 32;
const MIN_PACKET_CHARGE: usize = 1 + size_of::<Packet>();

type ReceiveQueue = (Receiver<Packet>, Arc<AtomicUsize>, Arc<AtomicBool>);

struct DeferredReceiveCleanup {
    next: *mut Self,
    queue: Option<ReceiveQueue>,
    channel: Option<Channel>,
    poll_state: Option<PollSet>,
    _admission: DeferredCleanupAdmission,
}

// `next` is touched only while the node is uniquely owned or while the global
// intrusive-list lock is held. Shared references cannot mutate or dereference
// it, and all optional payloads are themselves Send + Sync.
unsafe impl Send for DeferredReceiveCleanup {}
unsafe impl Sync for DeferredReceiveCleanup {}

impl DeferredReceiveCleanup {
    fn try_new() -> AxResult<Box<Self>> {
        let admission = DeferredCleanupAdmission::try_acquire()?;
        Box::try_new(Self {
            next: ptr::null_mut(),
            queue: None,
            channel: None,
            poll_state: None,
            _admission: admission,
        })
        .map_err(|_| AxError::NoMemory)
    }
}

static DEFERRED_CLEANUP_ADMISSIONS: AtomicUsize = AtomicUsize::new(0);

struct DeferredCleanupAdmission;

impl DeferredCleanupAdmission {
    fn try_acquire() -> AxResult<Self> {
        DEFERRED_CLEANUP_ADMISSIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < UNIX_DGRAM_CLEANUP_SLOTS).then_some(current + 1)
            })
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self)
    }
}

impl Drop for DeferredCleanupAdmission {
    fn drop(&mut self) {
        DEFERRED_CLEANUP_ADMISSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct DeferredCleanupList {
    head: *mut DeferredReceiveCleanup,
    tail: *mut DeferredReceiveCleanup,
}

// Every raw node is owned either by one DgramTransport, by this locked list,
// or by the single draining task after pop(). No borrowed pointer crosses an
// unlock boundary.
unsafe impl Send for DeferredCleanupList {}

impl DeferredCleanupList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    fn push(&mut self, work: Box<DeferredReceiveCleanup>) {
        let raw = Box::into_raw(work);
        unsafe {
            (*raw).next = ptr::null_mut();
            if self.tail.is_null() {
                self.head = raw;
            } else {
                (*self.tail).next = raw;
            }
        }
        self.tail = raw;
    }

    fn pop(&mut self) -> Option<Box<DeferredReceiveCleanup>> {
        let raw = self.head;
        if raw.is_null() {
            return None;
        }
        unsafe {
            self.head = (*raw).next;
            (*raw).next = ptr::null_mut();
            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }
            Some(Box::from_raw(raw))
        }
    }
}

static DEFERRED_CLEANUP_LIST: SpinNoIrq<DeferredCleanupList> =
    SpinNoIrq::new(DeferredCleanupList::new());
static DEFERRED_CLEANUP_PENDING: AtomicBool = AtomicBool::new(false);

fn publish_deferred_receive_cleanup(
    queue: Option<ReceiveQueue>,
    channel: Option<Channel>,
    poll_state: Option<PollSet>,
    mut work: Box<DeferredReceiveCleanup>,
) {
    // This is the complete close publication performed by Drop/shutdown:
    // reject new sends immediately. Channel close, waiter wake, packet
    // destruction, SCM_RIGHTS release, and even the idle cleanup Box's
    // deallocation stay in the task-context worker.
    if let Some((_, _, peer_closed)) = queue.as_ref() {
        peer_closed.store(true, Ordering::Release);
    }

    work.queue = queue;
    work.channel = channel;
    work.poll_state = poll_state;
    let mut list = DEFERRED_CLEANUP_LIST.lock();
    list.push(work);
    DEFERRED_CLEANUP_PENDING.store(true, Ordering::Release);
}

fn pop_deferred_receive_cleanup() -> Option<Box<DeferredReceiveCleanup>> {
    let mut list = DEFERRED_CLEANUP_LIST.lock();
    let work = list.pop();
    if list.head.is_null() {
        DEFERRED_CLEANUP_PENDING.store(false, Ordering::Release);
    }
    work
}

fn requeue_deferred_receive_cleanup(work: Box<DeferredReceiveCleanup>) {
    let mut list = DEFERRED_CLEANUP_LIST.lock();
    list.push(work);
    DEFERRED_CLEANUP_PENDING.store(true, Ordering::Release);
}

/// Returns whether a task-context Unix datagram finalizer has been published.
pub fn has_deferred_receive_cleanup_work() -> bool {
    DEFERRED_CLEANUP_PENDING.load(Ordering::Acquire) || super::stream::has_deferred_cleanup_work()
}

/// Releases a fixed amount of detached Unix datagram state in task context.
///
/// The caller should invoke this again after yielding while work remains.
pub fn drain_deferred_receive_cleanup_work() {
    let mut nodes = 0usize;
    let mut packets = 0usize;

    while nodes < DEFERRED_CLEANUP_NODE_BUDGET && packets < DEFERRED_CLEANUP_PACKET_BUDGET {
        let Some(mut work) = pop_deferred_receive_cleanup() else {
            break;
        };
        nodes += 1;

        let queue_drained = if let Some((rx, queued_bytes, _)) = work.queue.as_ref() {
            rx.close();
            let mut drained = false;
            while packets < DEFERRED_CLEANUP_PACKET_BUDGET {
                match rx.try_recv_deferred_wake() {
                    Ok((packet, completion)) => {
                        queued_bytes.fetch_sub(packet.charge, Ordering::AcqRel);
                        completion.complete();
                        packets += 1;
                        drop(packet);
                    }
                    Err(TryRecvError::Closed) => {
                        drained = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            drained
        } else {
            true
        };

        if !queue_drained {
            requeue_deferred_receive_cleanup(work);
            continue;
        }

        // Drop publishes a separate node for the connected sender and local
        // PollSet. It may have no receive queue at all; never bypass its wake
        // and task-context destruction through the queue-drain fast path.
        if let Some(poll_state) = work.poll_state.take() {
            poll_state.wake();
            drop(poll_state);
        }
        let channel = work.channel.take();
        drop(channel);
    }
    super::stream::drain_deferred_cleanup_work();
}

enum ReceiveState {
    Idle(Box<DeferredReceiveCleanup>),
    Attached {
        queue: ReceiveQueue,
        cleanup: Box<DeferredReceiveCleanup>,
    },
    Detached,
}

impl ReceiveState {
    fn try_new(queue: Option<ReceiveQueue>) -> AxResult<Self> {
        let cleanup = DeferredReceiveCleanup::try_new()?;
        Ok(match queue {
            Some(queue) => Self::Attached { queue, cleanup },
            None => Self::Idle(cleanup),
        })
    }

    fn queue(&self) -> Option<&ReceiveQueue> {
        match self {
            Self::Attached { queue, .. } => Some(queue),
            Self::Idle(_) | Self::Detached => None,
        }
    }

    fn queue_mut(&mut self) -> Option<&mut ReceiveQueue> {
        match self {
            Self::Attached { queue, .. } => Some(queue),
            Self::Idle(_) | Self::Detached => None,
        }
    }

    fn detach(&mut self) -> Option<(ReceiveQueue, Box<DeferredReceiveCleanup>)> {
        match core::mem::replace(self, Self::Detached) {
            Self::Attached { queue, cleanup } => Some((queue, cleanup)),
            state => {
                *self = state;
                None
            }
        }
    }

    fn rollback_bind(&mut self) -> Option<ReceiveQueue> {
        match core::mem::replace(self, Self::Detached) {
            Self::Attached { queue, cleanup } => {
                *self = Self::Idle(cleanup);
                Some(queue)
            }
            state => {
                *self = state;
                None
            }
        }
    }

    fn take_cleanup(&mut self) -> Option<(Option<ReceiveQueue>, Box<DeferredReceiveCleanup>)> {
        match core::mem::replace(self, Self::Detached) {
            Self::Idle(cleanup) => Some((None, cleanup)),
            Self::Attached { queue, cleanup } => Some((Some(queue), cleanup)),
            Self::Detached => None,
        }
    }

    fn install(&mut self, queue: ReceiveQueue) -> bool {
        let cleanup = match core::mem::replace(self, Self::Detached) {
            Self::Idle(cleanup) => cleanup,
            Self::Detached => return false,
            state @ Self::Attached { .. } => {
                *self = state;
                return false;
            }
        };
        *self = Self::Attached { queue, cleanup };
        true
    }

    fn is_attached(&self) -> bool {
        matches!(self, Self::Attached { .. })
    }

    fn receiver(&self) -> Option<&Receiver<Packet>> {
        self.queue().map(|(rx, ..)| rx)
    }

    fn receiver_parts_mut(&mut self) -> Option<(&mut Receiver<Packet>, &mut Arc<AtomicUsize>)> {
        self.queue_mut()
            .map(|(rx, queued_bytes, _)| (rx, queued_bytes))
    }
}

#[derive(Clone)]
struct Channel {
    data_tx: Sender<Packet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    peer_closed: Arc<AtomicBool>,
    peer_buffers: Arc<SocketBufferLimits>,
    peer_credentials: UnixCredentials,
}

impl Channel {
    fn identity(&self) -> usize {
        Arc::as_ptr(&self.peer_closed).cast::<()>() as usize
    }

    fn capacity(&self, local_buffers: &SocketBufferLimits) -> usize {
        local_buffers.send().min(self.peer_buffers.recv()).max(1)
    }

    fn writable(&self, local_buffers: &SocketBufferLimits) -> bool {
        self.peer_closed.load(Ordering::Acquire)
            || self.data_tx.is_closed()
            || (!self.data_tx.is_full()
                && self
                    .queued_bytes
                    .load(Ordering::Acquire)
                    .checked_add(MIN_PACKET_CHARGE)
                    .is_some_and(|used| used <= self.capacity(local_buffers)))
    }

    fn writable_for(&self, local_buffers: &SocketBufferLimits, charge: usize) -> bool {
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            // Let the send attempt run and report ECONNREFUSED instead of
            // sleeping on a peer that can never wake us again.
            return true;
        }
        !self.data_tx.is_full()
            && self
                .queued_bytes
                .load(Ordering::Acquire)
                .checked_add(charge)
                .is_some_and(|queued| queued <= self.capacity(local_buffers))
    }

    fn try_admit(
        &self,
        local_buffers: &SocketBufferLimits,
        charge: usize,
    ) -> AxResult<PendingSendAdmission> {
        // Deferred receiver teardown leaves the bounded channel object alive
        // until task context drains it. Report the published peer-close state
        // before consulting stale queue accounting so close can never turn a
        // full queue into an endless WouldBlock/retry loop.
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            return Err(AxError::ConnectionRefused);
        }
        let capacity = self.capacity(local_buffers);
        if charge > capacity {
            return Err(LinuxError::EMSGSIZE.into());
        }

        // Reserve queue metadata before any payload allocation/usercopy. The
        // permit is part of the pending-send admission, so arbitrarily many
        // blocked callers cannot each retain an unaccounted queue node.
        let permit = match self.data_tx.try_reserve(UNIX_DGRAM_QUEUE_SLOTS) {
            Ok(permit) => permit,
            Err(ReserveError::Full) => return Err(AxError::WouldBlock),
            Err(ReserveError::Closed) => return Err(AxError::ConnectionRefused),
        };

        loop {
            let queued = self.queued_bytes.load(Ordering::Acquire);
            if queued
                .checked_add(charge)
                .is_none_or(|queued| queued > capacity)
            {
                drop(permit);
                return Err(AxError::WouldBlock);
            }
            if self
                .queued_bytes
                .compare_exchange(queued, queued + charge, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        let admission = PendingSendAdmission {
            permit: Some(permit),
            admitted_bytes: self.queued_bytes.clone(),
            charge,
            transferred: false,
        };
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            drop(admission);
            return Err(AxError::ConnectionRefused);
        }
        Ok(admission)
    }
}

pub(super) struct DgramSendReservation<'a> {
    transport: &'a DgramTransport,
    effective_nonblocking: bool,
    cmsg: Vec<CMsgData>,
    channel: Channel,
}

impl DgramSendReservation<'_> {
    pub(super) fn peer_identity(&self) -> usize {
        self.channel.identity()
    }

    pub(super) fn commit(self, src: impl Read + IoBuf) -> AxResult<usize> {
        self.transport
            .send_via_channel(src, self.effective_nonblocking, self.cmsg, self.channel)
    }
}

/// Owns both one queue slot and the complete conservative byte/ancillary
/// charge before the payload buffer is allocated. Success transfers the byte
/// charge to the queued Packet; every other exit rolls both reservations back.
struct PendingSendAdmission {
    permit: Option<SendPermit<Packet>>,
    admitted_bytes: Arc<AtomicUsize>,
    charge: usize,
    transferred: bool,
}

impl PendingSendAdmission {
    fn publish(mut self, mut packet: Packet) -> AxResult<()> {
        packet.charge = self.charge;
        let permit = self.permit.take().ok_or(AxError::BadState)?;
        match permit.send(packet) {
            Ok(()) => {
                self.transferred = true;
                Ok(())
            }
            Err(PermitSendError::Closed(packet)) => {
                drop(packet);
                Err(AxError::ConnectionRefused)
            }
        }
    }
}

impl Drop for PendingSendAdmission {
    fn drop(&mut self) {
        if !self.transferred {
            self.admitted_bytes.fetch_sub(self.charge, Ordering::AcqRel);
        }
        // Drop releases an unpublished slot and wakes registered writers.
        let permit = self.permit.take();
        drop(permit);
    }
}

struct DgramSendPoll<'a> {
    channel: &'a Channel,
    local_buffers: &'a SocketBufferLimits,
    local_poll: &'a PollSet,
    charge: usize,
}

impl Pollable for DgramSendPoll<'_> {
    fn poll(&self) -> IoEvents {
        if self.channel.writable_for(self.local_buffers, self.charge) {
            IoEvents::WRITABLE
        } else {
            IoEvents::empty()
        }
    }

    fn register<'b>(
        &'b self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'b>, PollRegistrationError> {
        let writable = events.contains(IoEvents::WRITABLE);
        let mut prepared = PreparedPollRegistration::try_new(if writable { 2 } else { 0 })?;
        if writable {
            prepared.arm_owned(self.channel.data_tx.write_poll_source(), context.waker())?;
            prepared.arm(self.local_poll, context.waker())?;
        }
        prepared.commit()
    }
}

#[derive(Clone)]
pub struct Bind {
    data_tx: Sender<Packet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    peer_closed: Arc<AtomicBool>,
    buffers: Arc<SocketBufferLimits>,
}
impl Bind {
    fn connect(&self) -> AxResult<Channel> {
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            return Err(AxError::ConnectionRefused);
        }
        let tx = self.data_tx.clone();
        Ok(Channel {
            data_tx: tx,
            filter: self.filter.clone(),
            queued_bytes: self.queued_bytes.clone(),
            peer_closed: self.peer_closed.clone(),
            peer_buffers: self.buffers.clone(),
            // Linux does not expose credentials through SO_PEERCRED for a
            // pathname-connected datagram socket. SCM_CREDENTIALS is a
            // separate, per-message facility.
            peer_credentials: UnixCredentials::UNKNOWN,
        })
    }
}

/// Datagram transport for Unix domain sockets.
///
/// [`Drop`] never acquires a blocking mutex and never invokes a registered
/// waker. Attached/idle receive state, the connected sender, and the socket's
/// poll registry are moved into preallocated bounded work and finalized by the
/// task-context policy worker. Remaining automatic fields contain only fixed
/// atomic state and admitted ownership without poll callbacks.
pub struct DgramTransport {
    data_rx: Mutex<ReceiveState>,
    drop_cleanup: ManuallyDrop<Box<DeferredReceiveCleanup>>,
    connected: RwLock<Option<Channel>>,
    // Linux snapshots SO_PEERCRED when a datagram socketpair is created.
    // Reconnecting or disconnecting either endpoint must not replace that
    // creation-time identity with the credentials of a later pathname peer.
    sticky_peer_credentials: Option<UnixCredentials>,
    local_addr: RwLock<UnixSocketAddr>,
    poll_state: PollSet,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    general: Arc<GeneralOptions>,
    buffers: Arc<SocketBufferLimits>,
    rx_shutdown: AtomicBool,
    tx_shutdown: AtomicBool,
}
impl DgramTransport {
    pub(super) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.general
            .transfer_poller(self, direction, effective_nonblocking, attempt)
    }

    /// Create a new unconnected datagram transport.
    pub fn new() -> AxResult<Self> {
        let data_rx = ReceiveState::try_new(None)?;
        let drop_cleanup = DeferredReceiveCleanup::try_new()?;
        Ok(DgramTransport {
            data_rx: Mutex::new(data_rx),
            drop_cleanup: ManuallyDrop::new(drop_cleanup),
            connected: RwLock::new(None),
            sticky_peer_credentials: None,
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: PollSet::new(),
            filter: Arc::try_new(RwLock::new(None)).map_err(|_| AxError::NoMemory)?,
            general: Arc::try_new(GeneralOptions::default()).map_err(|_| AxError::NoMemory)?,
            buffers: Arc::try_new(SocketBufferLimits::new(TCP_TX_BUF_LEN, TCP_RX_BUF_LEN))
                .map_err(|_| AxError::NoMemory)?,
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
        })
    }

    fn new_connected(
        data_rx: ReceiveQueue,
        connected: Channel,
        filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
        general: Arc<GeneralOptions>,
        buffers: Arc<SocketBufferLimits>,
        sticky_peer_credentials: Option<UnixCredentials>,
    ) -> AxResult<Self> {
        let data_rx = ReceiveState::try_new(Some(data_rx))?;
        let drop_cleanup = DeferredReceiveCleanup::try_new()?;
        Ok(DgramTransport {
            data_rx: Mutex::new(data_rx),
            drop_cleanup: ManuallyDrop::new(drop_cleanup),
            connected: RwLock::new(Some(connected)),
            sticky_peer_credentials,
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: PollSet::new(),
            filter,
            general,
            buffers,
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
        })
    }

    /// Create a connected pair of datagram transports.
    pub fn new_pair(credentials: UnixCredentials) -> AxResult<(Self, Self)> {
        let (tx1, rx1) = try_bounded(UNIX_DGRAM_QUEUE_SLOTS)?;
        let (tx2, rx2) = try_bounded(UNIX_DGRAM_QUEUE_SLOTS)?;
        let queued1 = Arc::try_new(AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let queued2 = Arc::try_new(AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let closed1 = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let closed2 = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let filter1 = Arc::try_new(RwLock::new(None)).map_err(|_| AxError::NoMemory)?;
        let filter2 = Arc::try_new(RwLock::new(None)).map_err(|_| AxError::NoMemory)?;
        let general1 = Arc::try_new(GeneralOptions::default()).map_err(|_| AxError::NoMemory)?;
        let general2 = Arc::try_new(GeneralOptions::default()).map_err(|_| AxError::NoMemory)?;
        let buffers1 = Arc::try_new(SocketBufferLimits::new(TCP_TX_BUF_LEN, TCP_RX_BUF_LEN))
            .map_err(|_| AxError::NoMemory)?;
        let buffers2 = Arc::try_new(SocketBufferLimits::new(TCP_TX_BUF_LEN, TCP_RX_BUF_LEN))
            .map_err(|_| AxError::NoMemory)?;
        let transport1 = DgramTransport::new_connected(
            (rx1, queued1.clone(), closed1.clone()),
            Channel {
                data_tx: tx2,
                filter: filter2.clone(),
                queued_bytes: queued2.clone(),
                peer_closed: closed2.clone(),
                peer_buffers: buffers2.clone(),
                peer_credentials: credentials,
            },
            filter1.clone(),
            general1.clone(),
            buffers1.clone(),
            Some(credentials),
        )?;
        let transport2 = DgramTransport::new_connected(
            (rx2, queued2, closed2),
            Channel {
                data_tx: tx1,
                filter: filter1.clone(),
                queued_bytes: queued1,
                peer_closed: closed1,
                peer_buffers: buffers1,
                peer_credentials: credentials,
            },
            filter2.clone(),
            general2,
            buffers2,
            Some(credentials),
        )?;
        Ok((transport1, transport2))
    }

    fn channel_from_slot(slot: &BindSlot) -> AxResult<Channel> {
        let bind = slot.dgram.lock().as_ref().cloned();
        if let Some(bind) = bind {
            return bind.connect();
        }
        if slot.stream.lock().is_some() {
            Err(LinuxError::EPROTOTYPE.into())
        } else {
            Err(AxError::ConnectionRefused)
        }
    }

    fn prepare_with_channel(
        &self,
        options: SendOptions,
        channel: Channel,
    ) -> DgramSendReservation<'_> {
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        DgramSendReservation {
            transport: self,
            effective_nonblocking,
            cmsg: options.cmsg,
            channel,
        }
    }

    pub(super) fn prepare_send_to_slot(
        &self,
        options: SendOptions,
        slot: &BindSlot,
    ) -> AxResult<DgramSendReservation<'_>> {
        if !matches!(
            options.to,
            Some(SocketAddrEx::Unix(UnixSocketAddr::Path(_)))
                | Some(SocketAddrEx::Unix(UnixSocketAddr::Abstract(_)))
        ) {
            return Err(AxError::InvalidInput);
        }
        Ok(self.prepare_with_channel(options, Self::channel_from_slot(slot)?))
    }

    pub(super) fn prepare_send(&self, options: SendOptions) -> AxResult<DgramSendReservation<'_>> {
        if let Some(addr) = options.to.as_ref() {
            match addr {
                SocketAddrEx::Unix(UnixSocketAddr::Unnamed) => {
                    return Err(AxError::InvalidInput);
                }
                SocketAddrEx::Unix(UnixSocketAddr::Path(_) | UnixSocketAddr::Abstract(_)) => {
                    return Err(AxError::OperationNotSupported);
                }
                _ => return Err(AxError::InvalidInput),
            }
        }
        let channel = self
            .connected
            .read()
            .as_ref()
            .cloned()
            .ok_or(AxError::NotConnected)?;
        Ok(self.prepare_with_channel(options, channel))
    }

    fn send_via_channel(
        &self,
        mut src: impl Read + IoBuf,
        effective_nonblocking: bool,
        cmsg: Vec<CMsgData>,
        channel: Channel,
    ) -> AxResult<usize> {
        if self.tx_shutdown.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }

        let len = src.remaining();
        let ancillary_charge = cmsg.iter().try_fold(0usize, |total, cmsg| {
            total.checked_add(cmsg.charge()).ok_or(AxError::NoMemory)
        })?;
        let charge = len
            .max(1)
            .checked_add(ancillary_charge)
            .and_then(|charge| charge.checked_add(size_of::<Packet>()))
            .ok_or(AxError::NoMemory)?;
        if charge > channel.capacity(&self.buffers) {
            return Err(AxError::from(LinuxError::EMSGSIZE));
        }

        let pollable = DgramSendPoll {
            channel: &channel,
            local_buffers: &self.buffers,
            local_poll: &self.poll_state,
            charge,
        };
        let admission = self.general.send_poller_with_effective_nonblocking(
            &pollable,
            effective_nonblocking,
            || {
                if self.tx_shutdown.load(Ordering::Acquire) {
                    Err(AxError::BrokenPipe)
                } else {
                    channel.try_admit(&self.buffers, charge)
                }
            },
        )?;

        let mut message = Vec::new();
        message
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        message.resize(len, 0);
        src.read_exact(&mut message)?;
        let mut packet = Packet {
            data: message,
            cmsg,
            sender: self.local_addr.read().clone(),
            charge: 0,
        };

        let filter = channel.filter.read().clone();
        if let Some(filter) = filter {
            let keep = filter.filter(packet.data.as_mut_slice())?;
            if keep == 0 {
                return Ok(len);
            }
            packet.data.truncate(keep.min(packet.data.len()));
        }
        admission.publish(packet)?;
        Ok(len)
    }

    pub fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        let retired = core::mem::replace(&mut *self.filter.write(), filter);
        drop(retired);
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.read().is_some()
    }

    pub(super) fn disconnect(&self) {
        let retired = self.connected.write().take();
        drop(retired);
        self.poll_state.wake();
    }

    pub(super) fn rollback_bind(&self, slot: &super::BindSlot) {
        let retired_bind = slot.dgram.lock().take();
        // This callback is only for a reservation that was never published.
        // Recover the preallocated cleanup admission so the same socket can
        // retry bind, and destroy the private empty queue in task context.
        let retired_queue = self.data_rx.lock().rollback_bind();
        let retired_address = core::mem::take(&mut *self.local_addr.write());
        drop(retired_bind);
        if let Some((receiver, queued_bytes, _peer_closed)) = retired_queue {
            receiver.close();
            for _ in 0..UNIX_DGRAM_QUEUE_SLOTS {
                match receiver.try_recv_deferred_wake() {
                    Ok((packet, completion)) => {
                        queued_bytes.fetch_sub(packet.charge, Ordering::AcqRel);
                        completion.complete();
                        drop(packet);
                    }
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            }
            drop(receiver);
        }
        drop(retired_address);
        self.poll_state.wake();
    }
}

impl Configurable for DgramTransport {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(opt)? {
            return Ok(true);
        }

        match opt {
            O::SendBuffer(size) => {
                **size = self.buffers.send();
            }
            O::ReceiveBuffer(size) => {
                **size = self.buffers.recv();
            }
            O::PeerCredentials(cred) => {
                **cred = self.sticky_peer_credentials.unwrap_or_else(|| {
                    self.connected
                        .read()
                        .as_ref()
                        .map_or(UnixCredentials::UNKNOWN, |channel| channel.peer_credentials)
                });
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool> {
        use SetSocketOption as O;

        if self.general.set_option_inner(opt)? {
            return Ok(true);
        }

        match opt {
            O::SendBuffer(size) | O::SendBufferForce(size) => {
                self.buffers.set_send(*size);
                self.poll_state.wake();
            }
            O::ReceiveBuffer(size) | O::ReceiveBufferForce(size) => {
                self.buffers.set_recv(*size);
                let receiver = self.data_rx.lock().receiver().cloned();
                if let Some(receiver) = receiver {
                    receiver.wake_writers();
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
#[async_trait]
impl TransportOps for DgramTransport {
    fn set_pending_error(&self, error: LinuxError) {
        self.general.set_pending_error(error);
    }

    fn bind(&self, slot: &super::BindSlot, local_addr: &UnixSocketAddr) -> AxResult {
        // Prepare and reserve the complete bounded receive queue before taking
        // either publication lock. Installation below only moves ownership.
        let (tx, rx) = try_bounded(UNIX_DGRAM_QUEUE_SLOTS)?;
        let queued_bytes = Arc::try_new(AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let peer_closed = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let prepared_bind = Bind {
            data_tx: tx,
            filter: self.filter.clone(),
            queued_bytes: queued_bytes.clone(),
            peer_closed: peer_closed.clone(),
            buffers: self.buffers.clone(),
        };
        let prepared_queue = (rx, queued_bytes, peer_closed);
        let prepared_address = local_addr.clone();

        let mut slot = slot.dgram.lock();
        if slot.is_some() {
            return Err(AxError::AddrInUse);
        }
        let mut guard = self.data_rx.lock();
        if guard.is_attached() {
            return Err(AxError::InvalidInput);
        }
        *slot = Some(prepared_bind);
        if !guard.install(prepared_queue) {
            let retired_bind = slot.take();
            drop(guard);
            drop(slot);
            drop(retired_bind);
            return Err(AxError::InvalidInput);
        }
        let retired_address = core::mem::replace(&mut *self.local_addr.write(), prepared_address);
        drop(guard);
        drop(slot);
        drop(retired_address);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindSlot,
        _local_addr: &UnixSocketAddr,
        _credentials: UnixCredentials,
    ) -> AxResult {
        let channel = Self::channel_from_slot(slot)?;
        let retired = {
            let mut guard = self.connected.write();
            (*guard).replace(channel)
        };
        drop(retired);
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> AxResult<(Transport, UnixSocketAddr)> {
        Err(AxError::InvalidInput)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        self.prepare_send(options)?.commit(src)
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> AxResult<usize> {
        if self.rx_shutdown.load(Ordering::Acquire) {
            return Ok(0);
        }
        // A correct SCM_RIGHTS peek must duplicate the received fds while
        // retaining the original queued references. The current channel has
        // no non-consuming/fallible clone operation, so reject this mode
        // rather than silently consuming the datagram.
        if options.flags.contains(RecvFlags::PEEK) {
            return Err(AxError::OperationNotSupported);
        }
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        self.general.recv_poller_with_effective_nonblocking(
            self,
            effective_nonblocking,
            move || {
                let (packet, queued_bytes, completion) = {
                    let mut guard = self.data_rx.lock();
                    let Some((rx, queued_bytes)) = guard.receiver_parts_mut() else {
                        return Err(AxError::NotConnected);
                    };
                    let (packet, completion) = match rx.try_recv_deferred_wake() {
                        Ok(received) => received,
                        Err(TryRecvError::Empty) => {
                            return Err(AxError::WouldBlock);
                        }
                        Err(TryRecvError::Closed) => {
                            return Ok(0);
                        }
                    };
                    (packet, Arc::clone(queued_bytes), completion)
                };
                let Packet {
                    data,
                    cmsg,
                    sender,
                    charge,
                } = packet;
                queued_bytes.fetch_sub(charge, Ordering::AcqRel);
                // The queue slot and byte charge jointly define writability.
                // Publish both removals before waking a sender; otherwise it
                // can re-register while the stale byte charge still says full
                // and miss the only completion edge.
                completion.complete();

                let count = dst.write(&data)?;
                if count < data.len() {
                    warn!(
                        "Unix datagram message truncated: {} -> {} bytes",
                        data.len(),
                        count
                    );
                }

                if let Some(from) = options.from.as_mut() {
                    **from = SocketAddrEx::Unix(sender);
                }
                if let Some(dst) = options.cmsg.as_mut() {
                    if dst.is_empty() {
                        **dst = cmsg;
                    } else {
                        dst.try_reserve(cmsg.len()).map_err(|_| AxError::NoMemory)?;
                        dst.extend(cmsg);
                    }
                }

                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    data.len()
                } else {
                    count
                })
            },
        )
    }

    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        if how.has_read() && !self.rx_shutdown.swap(true, Ordering::AcqRel) {
            let detached = self.data_rx.lock().detach();
            if let Some((queue, cleanup)) = detached {
                publish_deferred_receive_cleanup(Some(queue), None, None, cleanup);
            }
        }
        if how.has_write() {
            self.tx_shutdown.store(true, Ordering::Release);
        }
        self.poll_state.wake();
        Ok(())
    }
}

impl Pollable for DgramTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let rx_shutdown = self.rx_shutdown.load(Ordering::Acquire);
        let tx_shutdown = self.tx_shutdown.load(Ordering::Acquire);
        events.set(
            IoEvents::READABLE,
            rx_shutdown
                || self
                    .data_rx
                    .lock()
                    .receiver()
                    .is_some_and(|rx| !rx.is_empty()),
        );
        let connected = self.connected.read().as_ref().cloned();
        events.set(
            IoEvents::WRITABLE,
            !tx_shutdown
                && connected
                    .as_ref()
                    .is_none_or(|chan| chan.writable(&self.buffers)),
        );
        events.set(IoEvents::READ_HANGUP, rx_shutdown);
        self.general.add_pending_error_event(events)
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let read_source = events
            .contains(IoEvents::READABLE)
            .then(|| {
                self.data_rx
                    .lock()
                    .receiver()
                    .map(Receiver::read_poll_source)
            })
            .flatten();
        let write_source = events
            .contains(IoEvents::WRITABLE)
            .then(|| {
                self.connected
                    .read()
                    .as_ref()
                    .map(|channel| channel.data_tx.write_poll_source())
            })
            .flatten();
        let maximum = 1 + usize::from(read_source.is_some()) + usize::from(write_source.is_some());
        let mut prepared = PreparedPollRegistration::try_new(maximum)?;
        if let Some(source) = read_source {
            prepared.arm_owned(source, context.waker())?;
        }
        if let Some(source) = write_source {
            prepared.arm_owned(source, context.waker())?;
        }
        prepared.arm(&self.poll_state, context.waker())?;
        prepared.commit()
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        let receive_cleanup = self.data_rx.get_mut().take_cleanup();
        if let Some((queue, cleanup)) = receive_cleanup {
            publish_deferred_receive_cleanup(queue, None, None, cleanup);
        }
        let retired_channel = self.connected.get_mut().take();
        let retired_poll_state = core::mem::replace(&mut self.poll_state, PollSet::new());
        // SAFETY: `drop_cleanup` is initialized exactly once by each
        // constructor, is inaccessible outside this type, and `Drop` runs at
        // most once. Moving it into the worker also prevents its admitted Box
        // from being deallocated in the transport's final-release context.
        let finalizer = unsafe { ManuallyDrop::take(&mut self.drop_cleanup) };
        publish_deferred_receive_cleanup(
            None,
            retired_channel,
            Some(retired_poll_state),
            finalizer,
        );
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::*;
    use crate::SendFlags;

    struct GatedThreadWake {
        thread: std::thread::Thread,
        woke: Arc<AtomicBool>,
        rechecked: Arc<std::sync::Barrier>,
    }

    impl alloc::task::Wake for GatedThreadWake {
        fn wake(self: Arc<Self>) {
            self.woke.store(true, Ordering::Release);
            self.thread.unpark();
            self.rechecked.wait();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.woke.store(true, Ordering::Release);
            self.thread.unpark();
            self.rechecked.wait();
        }
    }

    struct CountingWake(Arc<AtomicUsize>);

    impl alloc::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn drain_all_deferred_cleanup() {
        for _ in 0..UNIX_DGRAM_QUEUE_SLOTS * 64 {
            if !has_deferred_receive_cleanup_work() {
                return;
            }
            drain_deferred_receive_cleanup_work();
        }
        assert!(!has_deferred_receive_cleanup_work());
    }

    fn peer_credentials(transport: &DgramTransport) -> UnixCredentials {
        let mut credentials = UnixCredentials::default();
        transport
            .get_option(GetSocketOption::PeerCredentials(&mut credentials))
            .unwrap();
        credentials
    }

    #[test]
    fn unconnected_datagram_reports_unknown_peer_credentials() {
        let transport = DgramTransport::new().unwrap();
        assert_eq!(peer_credentials(&transport), UnixCredentials::UNKNOWN);
    }

    #[test]
    fn datagram_socketpair_snapshots_creator_credentials() {
        let credentials = UnixCredentials::new(11, 12, 13);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        assert_eq!(peer_credentials(&left), credentials);
        assert_eq!(peer_credentials(&right), credentials);

        let server = DgramTransport::new().unwrap();
        let slot = BindSlot::default();
        let pathname = UnixSocketAddr::Path(Arc::new(String::from("/tmp/peercred-reconnect")));
        server.bind(&slot, &pathname).unwrap();
        left.connect(&slot, &UnixSocketAddr::Unnamed, UnixCredentials::UNKNOWN)
            .unwrap();
        assert_eq!(peer_credentials(&left), credentials);

        left.disconnect();
        assert_eq!(peer_credentials(&left), credentials);
    }

    #[test]
    fn pathname_datagram_channel_does_not_fake_peer_credentials() {
        let (tx, _rx) = try_bounded(UNIX_DGRAM_QUEUE_SLOTS).unwrap();
        let bind = Bind {
            data_tx: tx,
            filter: Arc::new(RwLock::new(None)),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            peer_closed: Arc::new(AtomicBool::new(false)),
            buffers: Arc::new(SocketBufferLimits::new(TCP_TX_BUF_LEN, TCP_RX_BUF_LEN)),
        };

        assert_eq!(
            bind.connect().unwrap().peer_credentials,
            UnixCredentials::UNKNOWN
        );
    }

    #[test]
    fn prepared_datagram_send_cannot_be_redirected_by_peer_replacement() {
        let credentials = UnixCredentials::new(11, 12, 13);
        let (sender, original_receiver) = DgramTransport::new_pair(credentials).unwrap();
        let (alternate_sender, alternate_receiver) = DgramTransport::new_pair(credentials).unwrap();

        let reservation = sender.prepare_send(SendOptions::default()).unwrap();
        let original_identity = reservation.peer_identity();
        let replacement = alternate_sender.connected.read().as_ref().unwrap().clone();
        assert_ne!(original_identity, replacement.identity());
        *sender.connected.write() = Some(replacement);

        assert_eq!(reservation.commit(&b"x"[..]), Ok(1));
        let mut original = [0u8; 1];
        assert_eq!(
            original_receiver.recv(
                &mut original[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Ok(1)
        );
        assert_eq!(&original, b"x");

        let mut alternate = [0u8; 1];
        assert_eq!(
            alternate_receiver.recv(
                &mut alternate[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn receive_accounting_completes_before_a_concurrent_writer_wakes() {
        use core::task::Waker;
        use std::{sync::Barrier, time::Duration};

        let (tx, rx) = try_bounded(1).unwrap();
        tx.try_send(7u8).unwrap();
        let queued_bytes = Arc::new(AtomicUsize::new(1));
        let ready = Arc::new(Barrier::new(2));
        let rechecked = Arc::new(Barrier::new(2));
        let woke = Arc::new(AtomicBool::new(false));
        let observed_free = Arc::new(AtomicBool::new(false));

        let writer = {
            let tx = tx.clone();
            let queued_bytes = queued_bytes.clone();
            let ready = ready.clone();
            let rechecked = rechecked.clone();
            let woke = woke.clone();
            let observed_free = observed_free.clone();
            std::thread::spawn(move || {
                let waker = Waker::from(Arc::new(GatedThreadWake {
                    thread: std::thread::current(),
                    woke: woke.clone(),
                    rechecked: rechecked.clone(),
                }));
                let mut registration =
                    PollRegistration::single_owned(tx.write_poll_source(), &waker).unwrap();
                ready.wait();
                std::thread::park_timeout(Duration::from_secs(1));

                let was_woken = woke.load(Ordering::Acquire);
                let free = queued_bytes.load(Ordering::Acquire) == 0;
                if was_woken && !free {
                    // This is the lost-wake interleaving: a sender awakened by
                    // queue removal still sees the stale byte charge and
                    // registers again before that charge is decremented.
                    registration =
                        PollRegistration::single_owned(tx.write_poll_source(), &waker).unwrap();
                }
                observed_free.store(was_woken && free, Ordering::Release);
                if was_woken {
                    rechecked.wait();
                }
                drop(registration);
            })
        };

        ready.wait();
        let (item, completion) = rx.try_recv_deferred_wake().unwrap();
        assert_eq!(item, 7);
        queued_bytes.fetch_sub(1, Ordering::AcqRel);
        completion.complete();
        writer.join().unwrap();
        assert!(observed_free.load(Ordering::Acquire));
    }

    #[test]
    fn datagram_shutdown_is_allowed_without_a_peer_and_changes_io_state() {
        let unconnected = DgramTransport::new().unwrap();
        unconnected.shutdown(Shutdown::Both).unwrap();
        unconnected.shutdown(Shutdown::Both).unwrap();
        assert!(unconnected.rx_shutdown.load(Ordering::Acquire));
        assert!(unconnected.tx_shutdown.load(Ordering::Acquire));

        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        left.shutdown(Shutdown::Write).unwrap();
        assert!(left.tx_shutdown.load(Ordering::Acquire));
        assert!(!left.poll().contains(IoEvents::WRITABLE));
        assert_eq!(
            left.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );

        right.shutdown(Shutdown::Read).unwrap();
        assert!(right.rx_shutdown.load(Ordering::Acquire));
        assert!(right.poll().contains(IoEvents::READ_HANGUP));
    }

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn read_shutdown_closes_peer_send_and_releases_queued_ancillary_data() {
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        drain_all_deferred_cleanup();
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let byte = [0u8; 1];

        assert_eq!(
            left.send(
                &byte[..],
                SendOptions {
                    cmsg: alloc::vec![CMsgData::new(Box::new(DropProbe(drops.clone())), 1)],
                    ..SendOptions::default()
                }
            )
            .unwrap(),
            1
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        right.shutdown(Shutdown::Read).unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(has_deferred_receive_cleanup_work());
        drain_all_deferred_cleanup();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        right.shutdown(Shutdown::Read).unwrap();
        assert!(left.poll().contains(IoEvents::WRITABLE));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::ConnectionRefused
        );
        drop(left);
        drop(right);
        drain_all_deferred_cleanup();
    }

    #[test]
    fn drop_publishes_bounded_task_context_cleanup() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        drain_all_deferred_cleanup();
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        let left_registration = left.register(&mut context, IoEvents::READABLE).unwrap();
        let right_registration = right.register(&mut context, IoEvents::READABLE).unwrap();
        // A logical waiter owns cancellation and must release it before the
        // object supplying a borrowed local source is destroyed.
        drop(right_registration);
        let byte = [0u8; 1];
        let packet_count = DEFERRED_CLEANUP_PACKET_BUDGET + 1;

        for _ in 0..packet_count {
            left.send(
                &byte[..],
                SendOptions {
                    flags: SendFlags::DONT_WAIT,
                    cmsg: alloc::vec![CMsgData::new(Box::new(DropProbe(drops.clone())), 1)],
                    ..SendOptions::default()
                },
            )
            .unwrap();
        }

        let wakes_before_drop = wakes.load(Ordering::SeqCst);
        drop(right);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(wakes.load(Ordering::SeqCst), wakes_before_drop);
        assert!(has_deferred_receive_cleanup_work());
        assert!(left.poll().contains(IoEvents::WRITABLE));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::ConnectionRefused
        );

        let mut observed_progress = false;
        for _ in 0..DEFERRED_CLEANUP_NODE_BUDGET * 4 {
            let before = drops.load(Ordering::SeqCst);
            drain_deferred_receive_cleanup_work();
            let after = drops.load(Ordering::SeqCst);
            assert!(after - before <= DEFERRED_CLEANUP_PACKET_BUDGET);
            if after != 0 {
                observed_progress = true;
                break;
            }
        }
        assert!(observed_progress);
        assert!(drops.load(Ordering::SeqCst) < packet_count);

        drain_all_deferred_cleanup();
        assert_eq!(drops.load(Ordering::SeqCst), packet_count);
        assert!(wakes.load(Ordering::SeqCst) > wakes_before_drop);
        drop(left_registration);
        drop(left);
        drain_all_deferred_cleanup();
    }

    #[test]
    fn cancelled_idle_registration_is_not_woken_by_deferred_drop() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        drain_all_deferred_cleanup();
        let transport = DgramTransport::new().unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        let registration = transport
            .register(&mut context, IoEvents::READABLE | IoEvents::WRITABLE)
            .unwrap();
        drop(registration);

        drop(transport);
        assert_eq!(wakes.load(Ordering::Acquire), 0);
        drain_all_deferred_cleanup();
        assert_eq!(wakes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn peer_drop_with_queued_data_makes_blocking_send_fail_immediately() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        left.buffers.set_send(0);
        right.buffers.set_recv(0);

        let byte = [0u8; 1];
        let mut queued = 0usize;
        loop {
            let result = left.send(
                &byte[..],
                SendOptions {
                    flags: SendFlags::DONT_WAIT,
                    ..SendOptions::default()
                },
            );
            match result {
                Ok(1) => queued += 1,
                Err(AxError::WouldBlock) => break,
                other => panic!("unexpected datagram fill result: {other:?}"),
            }
            assert!(queued < UNIX_DGRAM_QUEUE_SLOTS);
        }
        assert!(queued > 0);

        drop(right);
        // Linux reports POLLOUT for a datagram peer that will make the next
        // send fail immediately rather than blocking in poll forever.
        assert!(left.poll().contains(IoEvents::WRITABLE));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::ConnectionRefused
        );
    }

    #[test]
    fn concurrent_pending_sends_are_admitted_before_payload_and_bounded() {
        use std::sync::{Arc as StdArc, Barrier};

        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, _right) = DgramTransport::new_pair(credentials).unwrap();
        let channel = left.connected.read().as_ref().unwrap().clone();
        let capacity = channel.capacity(&left.buffers);
        let charge = (capacity / 4).max(1);
        let thread_count = 32usize;
        let attempted = StdArc::new(Barrier::new(thread_count + 1));
        let release = StdArc::new(Barrier::new(thread_count + 1));
        let winners = StdArc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..thread_count)
            .map(|_| {
                let channel = channel.clone();
                let buffers = left.buffers.clone();
                let attempted = attempted.clone();
                let release = release.clone();
                let winners = winners.clone();
                std::thread::spawn(move || {
                    let admission = channel.try_admit(&buffers, charge);
                    if admission.is_ok() {
                        winners.fetch_add(1, Ordering::AcqRel);
                    } else {
                        assert!(matches!(admission, Err(AxError::WouldBlock)));
                    }
                    attempted.wait();
                    release.wait();
                    drop(admission);
                })
            })
            .collect();

        attempted.wait();
        let admitted = channel.queued_bytes.load(Ordering::Acquire);
        let winner_count = winners.load(Ordering::Acquire);
        assert!(winner_count > 0);
        assert_eq!(admitted, winner_count * charge);
        assert!(admitted <= capacity);
        assert!(winner_count <= UNIX_DGRAM_QUEUE_SLOTS);
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(channel.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn pollout_requires_room_for_the_smallest_datagram_charge() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        let channel = left.connected.read().as_ref().unwrap().clone();
        let capacity = channel.capacity(&left.buffers);
        assert!(capacity >= MIN_PACKET_CHARGE);
        let charge = capacity - (MIN_PACKET_CHARGE - 1);
        let admission = channel.try_admit(&left.buffers, charge).unwrap();
        admission
            .publish(Packet {
                data: alloc::vec![1],
                cmsg: Vec::new(),
                sender: UnixSocketAddr::Unnamed,
                charge: 0,
            })
            .unwrap();

        assert!(!left.poll().contains(IoEvents::WRITABLE));
        let mut byte = [0u8; 1];
        assert_eq!(
            right
                .recv(
                    &mut byte[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            1
        );
        assert!(left.poll().contains(IoEvents::WRITABLE));
    }
}
