use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use async_channel::TryRecvError;
use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::{Mutex, spin::SpinNoIrq};
use spin::RwLock;

use crate::{
    CMsgData, RecvFlags, RecvOptions, SendFlags, SendOptions, Shutdown, SocketAddrEx,
    buffer::SocketBufferLimits,
    consts::{TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
    socket::SocketFilter,
    unix::{Transport, TransportOps, UnixSocketAddr, with_slot},
};

struct Packet {
    data: Vec<u8>,
    cmsg: Vec<CMsgData>,
    sender: UnixSocketAddr,
    charge: usize,
}

impl Packet {
    fn accounting_charge(&self) -> AxResult<usize> {
        let ancillary_charge = self.cmsg.iter().try_fold(0usize, |total, cmsg| {
            total.checked_add(cmsg.charge()).ok_or(AxError::NoMemory)
        })?;
        self.data
            .len()
            .max(1)
            .checked_add(ancillary_charge)
            .and_then(|charge| charge.checked_add(size_of::<Self>()))
            .ok_or(AxError::NoMemory)
    }
}

// Byte accounting is the primary socket-buffer limit. This independent slot
// ceiling also bounds queue-node metadata if callers send very small
// datagrams, and lets the underlying channel enforce the invariant directly.
const UNIX_DGRAM_QUEUE_SLOTS: usize = 1024;
const UNIX_DGRAM_CLEANUP_SLOTS: usize = 16_384;
const DEFERRED_CLEANUP_NODE_BUDGET: usize = 16;
const DEFERRED_CLEANUP_PACKET_BUDGET: usize = 32;

type ReceiveQueue = (
    async_channel::Receiver<Packet>,
    Arc<PollSet>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
);

struct DeferredReceiveCleanup {
    next: *mut Self,
    queue: Option<ReceiveQueue>,
    _admission: DeferredCleanupAdmission,
}

// `next` is touched only while the node is uniquely owned or while the global
// intrusive-list lock is held. The queued receiver and accounting objects are
// themselves Send.
unsafe impl Send for DeferredReceiveCleanup {}

impl DeferredReceiveCleanup {
    fn try_new() -> AxResult<Box<Self>> {
        let admission = DeferredCleanupAdmission::try_acquire()?;
        Box::try_new(Self {
            next: ptr::null_mut(),
            queue: None,
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

fn publish_deferred_receive_cleanup(queue: ReceiveQueue, mut work: Box<DeferredReceiveCleanup>) {
    let (rx, poll_update, queued_bytes, peer_closed) = queue;

    // This is the complete close publication performed by Drop/shutdown:
    // reject new sends immediately and wake bounded poll state. Channel close,
    // packet destruction, and SCM_RIGHTS release stay in task context.
    peer_closed.store(true, Ordering::Release);
    poll_update.wake();

    work.queue = Some((rx, poll_update, queued_bytes, peer_closed));
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
    DEFERRED_CLEANUP_PENDING.load(Ordering::Acquire)
}

/// Releases a fixed amount of detached Unix datagram state in task context.
///
/// The caller should invoke this again after yielding while work remains.
pub fn drain_deferred_receive_cleanup_work() {
    let mut nodes = 0usize;
    let mut packets = 0usize;

    while nodes < DEFERRED_CLEANUP_NODE_BUDGET && packets < DEFERRED_CLEANUP_PACKET_BUDGET {
        let Some(work) = pop_deferred_receive_cleanup() else {
            break;
        };
        nodes += 1;

        let queue_drained = {
            let Some((rx, _, queued_bytes, _)) = work.queue.as_ref() else {
                continue;
            };
            rx.close();
            let mut drained = false;
            while packets < DEFERRED_CLEANUP_PACKET_BUDGET {
                match rx.try_recv() {
                    Ok(packet) => {
                        queued_bytes.fetch_sub(packet.charge, Ordering::AcqRel);
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
        };

        if !queue_drained {
            requeue_deferred_receive_cleanup(work);
        }
    }
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

    fn poll_update(&self) -> Option<&Arc<PollSet>> {
        self.queue().map(|(_, poll_update, ..)| poll_update)
    }

    fn receiver(&self) -> Option<&async_channel::Receiver<Packet>> {
        self.queue().map(|(rx, ..)| rx)
    }

    fn receiver_parts_mut(
        &mut self,
    ) -> Option<(
        &mut async_channel::Receiver<Packet>,
        &mut Arc<PollSet>,
        &mut Arc<AtomicUsize>,
    )> {
        self.queue_mut()
            .map(|(rx, poll_update, queued_bytes, _)| (rx, poll_update, queued_bytes))
    }

    fn register_read_waker(&self, context: &mut Context<'_>) {
        if let Some(poll) = self.poll_update() {
            poll.register(context.waker());
        }
    }
}

#[derive(Clone)]
struct Channel {
    data_tx: async_channel::Sender<Packet>,
    poll_update: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    peer_closed: Arc<AtomicBool>,
    peer_buffers: Arc<SocketBufferLimits>,
    peer_credentials: UnixCredentials,
}

impl Channel {
    fn capacity(&self, local_buffers: &SocketBufferLimits) -> usize {
        local_buffers.send().min(self.peer_buffers.recv()).max(1)
    }

    fn writable(&self, local_buffers: &SocketBufferLimits) -> bool {
        self.peer_closed.load(Ordering::Acquire)
            || self.data_tx.is_closed()
            || (!self.data_tx.is_full()
                && self.queued_bytes.load(Ordering::Acquire) < self.capacity(local_buffers))
    }

    fn writable_for(&self, local_buffers: &SocketBufferLimits, charge: usize) -> bool {
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            // Let the send attempt run and report EPIPE instead of sleeping on
            // a peer that can never wake us again.
            return true;
        }
        !self.data_tx.is_full()
            && self
                .queued_bytes
                .load(Ordering::Acquire)
                .checked_add(charge)
                .is_some_and(|queued| queued <= self.capacity(local_buffers))
    }

    fn try_send(
        &self,
        local_buffers: &SocketBufferLimits,
        mut packet: Packet,
    ) -> Result<(), SendFailure> {
        // Deferred receiver teardown leaves the bounded channel object alive
        // until task context drains it. Report the published peer-close state
        // before consulting stale queue accounting so close can never turn a
        // full queue into an endless WouldBlock/retry loop.
        if self.peer_closed.load(Ordering::Acquire) || self.data_tx.is_closed() {
            return Err(SendFailure {
                error: AxError::BrokenPipe,
                packet,
            });
        }
        let capacity = self.capacity(local_buffers);
        let charge = match packet.accounting_charge() {
            Ok(charge) => charge,
            Err(error) => return Err(SendFailure { error, packet }),
        };
        if charge > capacity {
            return Err(SendFailure {
                error: LinuxError::EMSGSIZE.into(),
                packet,
            });
        }

        loop {
            let queued = self.queued_bytes.load(Ordering::Acquire);
            if queued
                .checked_add(charge)
                .is_none_or(|queued| queued > capacity)
            {
                return Err(SendFailure {
                    error: AxError::WouldBlock,
                    packet,
                });
            }
            if self
                .queued_bytes
                .compare_exchange(queued, queued + charge, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        packet.charge = charge;
        if let Err(err) = self.data_tx.try_send(packet) {
            self.queued_bytes.fetch_sub(charge, Ordering::AcqRel);
            return Err(match err {
                async_channel::TrySendError::Full(packet) => SendFailure {
                    error: AxError::WouldBlock,
                    packet,
                },
                async_channel::TrySendError::Closed(packet) => SendFailure {
                    error: AxError::BrokenPipe,
                    packet,
                },
            });
        }

        self.poll_update.wake();
        Ok(())
    }
}

struct SendFailure {
    error: AxError,
    packet: Packet,
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
            IoEvents::OUT
        } else {
            IoEvents::empty()
        }
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::OUT) {
            self.channel.poll_update.register(context.waker());
            self.local_poll.register(context.waker());
        }
    }
}

pub struct Bind {
    data_tx: async_channel::Sender<Packet>,
    poll_update: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    peer_closed: Arc<AtomicBool>,
    buffers: Arc<SocketBufferLimits>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.data_tx.clone();
        Channel {
            data_tx: tx,
            poll_update: self.poll_update.clone(),
            filter: self.filter.clone(),
            queued_bytes: self.queued_bytes.clone(),
            peer_closed: self.peer_closed.clone(),
            peer_buffers: self.buffers.clone(),
            // Linux does not expose credentials through SO_PEERCRED for a
            // pathname-connected datagram socket. SCM_CREDENTIALS is a
            // separate, per-message facility.
            peer_credentials: UnixCredentials::UNKNOWN,
        }
    }
}

/// Datagram transport for Unix domain sockets.
pub struct DgramTransport {
    data_rx: Mutex<ReceiveState>,
    connected: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixSocketAddr>,
    poll_state: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    general: Arc<GeneralOptions>,
    buffers: Arc<SocketBufferLimits>,
    rx_shutdown: AtomicBool,
    tx_shutdown: AtomicBool,
}
impl DgramTransport {
    /// Create a new unconnected datagram transport.
    pub fn new() -> AxResult<Self> {
        let data_rx = ReceiveState::try_new(None)?;
        Ok(DgramTransport {
            data_rx: Mutex::new(data_rx),
            connected: RwLock::new(None),
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
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
    ) -> AxResult<Self> {
        let data_rx = ReceiveState::try_new(Some(data_rx))?;
        Ok(DgramTransport {
            data_rx: Mutex::new(data_rx),
            connected: RwLock::new(Some(connected)),
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
            filter,
            general,
            buffers,
            rx_shutdown: AtomicBool::new(false),
            tx_shutdown: AtomicBool::new(false),
        })
    }

    /// Create a connected pair of datagram transports.
    pub fn new_pair(credentials: UnixCredentials) -> AxResult<(Self, Self)> {
        let (tx1, rx1) = async_channel::bounded(UNIX_DGRAM_QUEUE_SLOTS);
        let (tx2, rx2) = async_channel::bounded(UNIX_DGRAM_QUEUE_SLOTS);
        let poll1 = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let poll2 = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
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
            (rx1, poll1.clone(), queued1.clone(), closed1.clone()),
            Channel {
                data_tx: tx2,
                poll_update: poll2.clone(),
                filter: filter2.clone(),
                queued_bytes: queued2.clone(),
                peer_closed: closed2.clone(),
                peer_buffers: buffers2.clone(),
                peer_credentials: credentials,
            },
            filter1.clone(),
            general1.clone(),
            buffers1.clone(),
        )?;
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone(), queued2, closed2),
            Channel {
                data_tx: tx1,
                poll_update: poll1.clone(),
                filter: filter1.clone(),
                queued_bytes: queued1,
                peer_closed: closed1,
                peer_buffers: buffers1,
                peer_credentials: credentials,
            },
            filter2.clone(),
            general2,
            buffers2,
        )?;
        Ok((transport1, transport2))
    }

    pub fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        *self.filter.write() = filter;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.read().is_some()
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
                **cred = self
                    .connected
                    .read()
                    .as_ref()
                    .map_or(UnixCredentials::UNKNOWN, |channel| channel.peer_credentials);
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
                if let Some(poll_update) = self.data_rx.lock().poll_update() {
                    poll_update.wake();
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
        // Prepare the receive side before taking either publication lock. The
        // async-channel constructor is an inherited infallible dependency API,
        // but all state added by this layer is admitted fallibly and no
        // allocator runs while the slot/transport locks are held.
        let (tx, rx) = async_channel::bounded(UNIX_DGRAM_QUEUE_SLOTS);
        let poll_update = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let queued_bytes = Arc::try_new(AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let peer_closed = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let prepared_bind = Bind {
            data_tx: tx,
            poll_update: poll_update.clone(),
            filter: self.filter.clone(),
            queued_bytes: queued_bytes.clone(),
            peer_closed: peer_closed.clone(),
            buffers: self.buffers.clone(),
        };
        let prepared_queue = (rx, poll_update, queued_bytes, peer_closed);

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
            *slot = None;
            return Err(AxError::InvalidInput);
        }
        self.local_addr.write().clone_from(local_addr);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(&self, slot: &super::BindSlot, _local_addr: &UnixSocketAddr) -> AxResult {
        let mut guard = self.connected.write();
        if guard.is_some() {
            return Err(AxError::AlreadyConnected);
        }
        *guard = Some(
            slot.dgram
                .lock()
                .as_ref()
                .ok_or(AxError::NotConnected)?
                .connect(),
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> AxResult<(Transport, UnixSocketAddr)> {
        Err(AxError::InvalidInput)
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if self.tx_shutdown.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        let SendOptions { to, flags, cmsg } = options;
        let channel = if let Some(addr) = to {
            let addr = addr.into_unix()?;
            with_slot(&addr, |slot| {
                slot.dgram
                    .lock()
                    .as_ref()
                    .map(Bind::connect)
                    .ok_or(AxError::NotConnected)
            })?
        } else {
            self.connected
                .read()
                .as_ref()
                .cloned()
                .ok_or(AxError::NotConnected)?
        };

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

        if let Some(filter) = channel.filter.read().as_ref() {
            let keep = filter.filter(&mut packet.data)?;
            if keep == 0 {
                return Ok(len);
            }
            packet.data.truncate(keep.min(packet.data.len()));
        }
        let charge = packet.accounting_charge()?;
        let pollable = DgramSendPoll {
            channel: &channel,
            local_buffers: &self.buffers,
            local_poll: &self.poll_state,
            charge,
        };
        let mut pending = Some(packet);
        self.general.send_poller_with_nonblocking(
            &pollable,
            flags.contains(SendFlags::DONT_WAIT),
            || {
                let packet = pending.take().ok_or(AxError::BadState)?;
                match channel.try_send(&self.buffers, packet) {
                    Ok(()) => Ok(()),
                    Err(failure) => {
                        pending = Some(failure.packet);
                        Err(failure.error)
                    }
                }
            },
        )?;
        Ok(len)
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
        let per_call_nonblocking = options.flags.contains(RecvFlags::DONT_WAIT);
        self.general
            .recv_poller_with_nonblocking(self, per_call_nonblocking, move || {
                let (packet, poll_update, queued_bytes) = {
                    let mut guard = self.data_rx.lock();
                    let Some((rx, poll_update, queued_bytes)) = guard.receiver_parts_mut() else {
                        return Err(AxError::NotConnected);
                    };
                    let packet = match rx.try_recv() {
                        Ok(packet) => packet,
                        Err(TryRecvError::Empty) => {
                            return Err(AxError::WouldBlock);
                        }
                        Err(TryRecvError::Closed) => {
                            return Ok(0);
                        }
                    };
                    (packet, Arc::clone(poll_update), Arc::clone(queued_bytes))
                };
                let Packet {
                    data,
                    cmsg,
                    sender,
                    charge,
                } = packet;
                queued_bytes.fetch_sub(charge, Ordering::AcqRel);
                poll_update.wake();

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
            })
    }

    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        if how.has_read() && !self.rx_shutdown.swap(true, Ordering::AcqRel) {
            let detached = self.data_rx.lock().detach();
            if let Some((queue, cleanup)) = detached {
                publish_deferred_receive_cleanup(queue, cleanup);
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
            IoEvents::IN,
            rx_shutdown
                || self
                    .data_rx
                    .lock()
                    .receiver()
                    .is_some_and(|rx| !rx.is_empty()),
        );
        events.set(
            IoEvents::OUT,
            !tx_shutdown
                && self
                    .connected
                    .read()
                    .as_ref()
                    .is_none_or(|chan| chan.writable(&self.buffers)),
        );
        events.set(IoEvents::RDHUP, rx_shutdown);
        self.general.add_pending_error_event(events)
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.data_rx.lock().register_read_waker(context);
        }
        if let Some(chan) = self.connected.read().as_ref()
            && events.contains(IoEvents::OUT)
        {
            chan.poll_update.register(context.waker());
        }
        self.poll_state.register(context.waker());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DEFERRED_CLEANUP_TEST_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

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
    }

    #[test]
    fn pathname_datagram_channel_does_not_fake_peer_credentials() {
        let (tx, _rx) = async_channel::bounded(UNIX_DGRAM_QUEUE_SLOTS);
        let bind = Bind {
            data_tx: tx,
            poll_update: Arc::new(PollSet::new()),
            filter: Arc::new(RwLock::new(None)),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            peer_closed: Arc::new(AtomicBool::new(false)),
            buffers: Arc::new(SocketBufferLimits::new(TCP_TX_BUF_LEN, TCP_RX_BUF_LEN)),
        };

        assert_eq!(bind.connect().peer_credentials, UnixCredentials::UNKNOWN);
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
        assert!(!left.poll().contains(IoEvents::OUT));

        right.shutdown(Shutdown::Read).unwrap();
        assert!(right.rx_shutdown.load(Ordering::Acquire));
        assert!(right.poll().contains(IoEvents::RDHUP));
    }

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn read_shutdown_closes_peer_send_and_releases_queued_ancillary_data() {
        let _guard = DEFERRED_CLEANUP_TEST_LOCK.lock();
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
        assert!(left.poll().contains(IoEvents::OUT));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::BrokenPipe
        );
        drop(left);
        drop(right);
        drain_all_deferred_cleanup();
    }

    #[test]
    fn drop_publishes_bounded_task_context_cleanup() {
        let _guard = DEFERRED_CLEANUP_TEST_LOCK.lock();
        drain_all_deferred_cleanup();
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = DgramTransport::new_pair(credentials).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
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

        drop(right);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(has_deferred_receive_cleanup_work());
        assert!(left.poll().contains(IoEvents::OUT));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::BrokenPipe
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
        drop(left);
        drain_all_deferred_cleanup();
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
        assert!(left.poll().contains(IoEvents::OUT));
        assert_eq!(
            left.send(&byte[..], SendOptions::default()).unwrap_err(),
            AxError::BrokenPipe
        );
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        let detached = self.data_rx.lock().detach();
        if let Some((queue, cleanup)) = detached {
            publish_deferred_receive_cleanup(queue, cleanup);
        }
        if let Some(chan) = self.connected.write().take() {
            chan.poll_update.wake();
        }
    }
}
