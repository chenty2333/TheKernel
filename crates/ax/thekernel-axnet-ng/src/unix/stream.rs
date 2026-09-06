use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    mem::ManuallyDrop,
    ptr,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axio::{IoBuf, Read, Write};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::{Mutex, spin::SpinNoIrq};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::{
    RecvFlags, RecvOptions, SendOptions, Shutdown,
    consts::{LISTEN_QUEUE_SIZE, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketCredentials, SocketFault},
    socket::SocketFilter,
    unix::{
        Transport, TransportOps, UnixSocketAddr,
        queue::{
            PermitSendError, Receiver, RecvReservation, SendPermit, Sender, TryRecvError,
            try_bounded,
        },
    },
};

// Match the default socket send buffer so large splice/socketpair transfers do
// not deadlock behind an unrealistically tiny in-kernel Unix stream queue.
const BUF_SIZE: usize = TCP_TX_BUF_LEN;
const STREAM_ANCILLARY_SEGMENTS: usize = 256;

/// A control-bearing stream byte interval. It is queued under the same
/// channel lock as the producer ring, so its start offset is never visible to
/// the receiver before the corresponding bytes have been committed.
struct AncillarySegment {
    start: usize,
    cmsg: Vec<crate::CMsgData>,
}

fn new_uni_channel() -> AxResult<(HeapProd<u8>, HeapCons<u8>)> {
    let rb = HeapRb::try_new(BUF_SIZE).map_err(|_| AxError::NoMemory)?;
    Ok(rb.split())
}

fn finish_stream_send(
    total: usize,
    requested: usize,
    effective_nonblocking: bool,
) -> AxResult<usize> {
    if total == requested || (effective_nonblocking && total != 0) {
        Ok(total)
    } else {
        Err(AxError::WouldBlock)
    }
}

fn new_channels(
    left_credentials: SocketCredentials,
    right_credentials: SocketCredentials,
) -> AxResult<(Channel, Channel)> {
    let (client_tx, server_rx) = new_uni_channel()?;
    let (server_tx, client_rx) = new_uni_channel()?;
    let (client_segments_tx, server_segments_rx) = try_bounded(STREAM_ANCILLARY_SEGMENTS)?;
    let (server_segments_tx, client_segments_rx) = try_bounded(STREAM_ANCILLARY_SEGMENTS)?;
    let poll_update = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
    let left_close = Arc::try_new(AtomicU8::new(0)).map_err(|_| AxError::NoMemory)?;
    let right_close = Arc::try_new(AtomicU8::new(0)).map_err(|_| AxError::NoMemory)?;
    Ok((
        Channel {
            tx: Some(client_tx),
            rx: Some(client_rx),
            segments_tx: Some(client_segments_tx),
            segments_rx: Some(client_segments_rx),
            tx_offset: 0,
            rx_offset: 0,
            poll_update: poll_update.clone(),
            peer_credentials: right_credentials,
            local_close: left_close.clone(),
            peer_close: right_close.clone(),
        },
        Channel {
            tx: Some(server_tx),
            rx: Some(server_rx),
            segments_tx: Some(server_segments_tx),
            segments_rx: Some(server_segments_rx),
            tx_offset: 0,
            rx_offset: 0,
            poll_update,
            peer_credentials: left_credentials,
            local_close: right_close,
            peer_close: left_close,
        },
    ))
}

const STREAM_READ_CLOSED: u8 = 1 << 0;
const STREAM_WRITE_CLOSED: u8 = 1 << 1;
const STREAM_BOTH_CLOSED: u8 = STREAM_READ_CLOSED | STREAM_WRITE_CLOSED;
const STREAM_RESET_OCCURRED: u8 = 1 << 2;
const STREAM_RESET_OBSERVED: u8 = 1 << 3;

struct Channel {
    tx: Option<HeapProd<u8>>,
    rx: Option<HeapCons<u8>>,
    segments_tx: Option<Sender<AncillarySegment>>,
    segments_rx: Option<Receiver<AncillarySegment>>,
    tx_offset: usize,
    rx_offset: usize,
    // TODO: granularity
    poll_update: Arc<PollSet>,
    peer_credentials: SocketCredentials,
    local_close: Arc<AtomicU8>,
    peer_close: Arc<AtomicU8>,
}

impl Channel {
    fn publish_read_close(&self) {
        self.local_close
            .fetch_or(STREAM_READ_CLOSED, Ordering::AcqRel);
    }

    fn publish_write_close(&self) {
        self.local_close
            .fetch_or(STREAM_WRITE_CLOSED, Ordering::AcqRel);
    }

    fn publish_orderly_close(&self) {
        self.local_close
            .fetch_or(STREAM_BOTH_CLOSED, Ordering::AcqRel);
    }

    fn publish_reset(&self) {
        // Keep occurrence separate from observation. Drop publishes this once
        // without waking, then the task-context finalizer repeats publication
        // before waking. An SO_ERROR/recv consumer between those two points
        // must not see ECONNRESET a second time.
        self.local_close
            .fetch_or(STREAM_BOTH_CLOSED | STREAM_RESET_OCCURRED, Ordering::AcqRel);
    }

    fn peer_read_closed(&self) -> bool {
        self.peer_close.load(Ordering::Acquire) & STREAM_READ_CLOSED != 0
            || self.tx.as_ref().is_some_and(|tx| !tx.read_is_held())
    }

    fn peer_write_closed(&self) -> bool {
        self.peer_close.load(Ordering::Acquire) & STREAM_WRITE_CLOSED != 0
            || self.rx.as_ref().is_some_and(|rx| !rx.write_is_held())
    }

    fn peer_reset_pending(&self) -> bool {
        let state = self.peer_close.load(Ordering::Acquire);
        state & STREAM_RESET_OCCURRED != 0 && state & STREAM_RESET_OBSERVED == 0
    }

    fn take_peer_reset(&self) -> bool {
        self.peer_close
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & STREAM_RESET_OCCURRED != 0 && state & STREAM_RESET_OBSERVED == 0)
                    .then_some(state | STREAM_RESET_OBSERVED)
            })
            .is_ok()
    }

    fn close_orderly_and_wake(self) {
        self.publish_orderly_close();
        let poll_update = self.poll_update.clone();
        drop(self);
        poll_update.wake();
    }

    fn reset_and_wake(self) {
        self.publish_reset();
        let poll_update = self.poll_update.clone();
        drop(self);
        poll_update.wake();
    }

    fn retire_ancillary(
        &mut self,
        read: bool,
        write: bool,
    ) -> (
        Option<Receiver<AncillarySegment>>,
        Option<Sender<AncillarySegment>>,
    ) {
        let rx = read.then(|| self.segments_rx.take()).flatten();
        let tx = write.then(|| self.segments_tx.take()).flatten();
        (rx, tx)
    }
}

struct Listener {
    conn_tx: Sender<ConnRequest>,
    credentials: Mutex<SocketCredentials>,
    backlog: AtomicUsize,
}

#[derive(Clone)]
pub struct Bind(Arc<Listener>);

impl Bind {
    fn try_new(conn_tx: Sender<ConnRequest>) -> AxResult<Self> {
        Arc::try_new(Listener {
            conn_tx,
            credentials: Mutex::new(SocketCredentials::UNKNOWN),
            backlog: AtomicUsize::new(0),
        })
        .map(Self)
        .map_err(|_| AxError::NoMemory)
    }

    fn reserve(&self) -> AxResult<SendPermit<ConnRequest>> {
        self.0
            .conn_tx
            .try_reserve(self.0.backlog.load(Ordering::Acquire))
            .map_err(|_| AxError::ConnectionRefused)
    }

    pub(super) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<()>() as usize
    }

    fn start_listening(&self, backlog: usize, credentials: SocketCredentials) -> AxResult<()> {
        if self.0.conn_tx.is_closed() {
            return Err(AxError::InvalidInput);
        }
        // Publish the listen(2)-time identity before making any queue slot
        // admissible.  A socket may have been created or bound by a different
        // task (for example across fork), so creation-time credentials are not
        // Linux SO_PEERCRED semantics.
        *self.0.credentials.lock() = credentials;
        self.0
            .backlog
            .store(backlog.clamp(1, LISTEN_QUEUE_SIZE), Ordering::Release);
        Ok(())
    }
}

struct ConnRequest {
    channel: Channel,
    addr: UnixSocketAddr,
}

impl ConnRequest {
    fn identity(&self) -> usize {
        Arc::as_ptr(&self.channel.poll_update).cast::<()>() as usize
    }
}

pub(super) struct StreamConnectReservation<'a> {
    transport: &'a StreamTransport,
    listener: Bind,
    permit: Option<SendPermit<ConnRequest>>,
    client_channel: Option<Channel>,
    request: Option<ConnRequest>,
    committed: bool,
}

impl StreamConnectReservation<'_> {
    pub(super) fn listener_identity(&self) -> usize {
        self.listener.identity()
    }

    pub(super) fn accepted_identity(&self) -> usize {
        self.request
            .as_ref()
            .expect("active stream-connect reservation")
            .identity()
    }

    pub(super) fn commit(mut self) -> AxResult<()> {
        let permit = self
            .permit
            .take()
            .expect("active stream-connect queue permit");
        let request = self.request.take().expect("active stream-connect request");
        if let Err(PermitSendError::Closed(request)) = permit.send(request) {
            drop(request);
            drop(self.client_channel.take());
            return Err(AxError::ConnectionRefused);
        }

        *self.transport.channel.lock() = self.client_channel.take();
        self.transport
            .connect_state
            .store(CONNECT_CONNECTED, Ordering::Release);
        self.transport.poll_state.wake();
        self.committed = true;
        Ok(())
    }
}

impl Drop for StreamConnectReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.transport
                .connect_state
                .store(CONNECT_UNCONNECTED, Ordering::Release);
        }
    }
}

pub(super) struct StreamAcceptReservation {
    request: Option<RecvReservation<ConnRequest>>,
    drop_cleanup: Option<Box<DeferredStreamCleanup>>,
}

/// Synchronously prepared listener state for one accept wait.
///
/// Acquiring the listener's sleeping endpoint lock and reserving teardown
/// storage happen when this value is constructed. Its async wait phase only
/// touches the bounded queue's non-sleeping state and poll source.
pub(super) struct PreparedStreamAccept {
    receiver: Receiver<ConnRequest>,
    drop_cleanup: Option<Box<DeferredStreamCleanup>>,
}

impl PreparedStreamAccept {
    pub(super) async fn wait(&mut self) -> AxResult<StreamAcceptReservation> {
        if self.drop_cleanup.is_none() {
            return Err(AxError::BadState);
        }
        let request = self.receiver.reserve().await?;
        let drop_cleanup = self.drop_cleanup.take().ok_or(AxError::BadState)?;
        Ok(StreamAcceptReservation {
            request: Some(request),
            drop_cleanup: Some(drop_cleanup),
        })
    }
}

impl StreamAcceptReservation {
    pub(super) fn accepted_identity(&self) -> usize {
        self.request
            .as_ref()
            .expect("active stream-accept reservation")
            .item()
            .identity()
    }

    pub(super) fn commit(mut self) -> AxResult<(Transport, UnixSocketAddr)> {
        let reservation = self
            .request
            .take()
            .expect("active stream-accept reservation");
        let ConnRequest {
            channel,
            addr: peer_addr,
        } = match reservation.commit() {
            Ok(request) => request,
            Err(request) => {
                request.channel.reset_and_wake();
                return Err(AxError::ConnectionReset);
            }
        };
        let cleanup = self
            .drop_cleanup
            .take()
            .expect("prepared stream-accept cleanup");
        Ok((
            Transport::Stream(StreamTransport::new_channel_with_cleanup(
                Some(channel),
                cleanup,
            )),
            peer_addr,
        ))
    }
}

impl Drop for StreamAcceptReservation {
    fn drop(&mut self) {
        let Some(reservation) = self.request.take() else {
            return;
        };
        if let Some(request) = reservation.cancel() {
            request.channel.reset_and_wake();
        }
    }
}

const UNIX_STREAM_CLEANUP_SLOTS: usize = 16_384;
const STREAM_CLEANUP_NODE_BUDGET: usize = 16;
const STREAM_CLEANUP_REQUEST_BUDGET: usize = 32;

struct DeferredStreamCleanup {
    next: *mut Self,
    channel: Option<Channel>,
    receiver: Option<Receiver<ConnRequest>>,
    poll_state: Option<PollSet>,
    _admission: StreamCleanupAdmission,
}

// SAFETY: `next` is only mutated under unique ownership or the intrusive-list lock.
// Shared references cannot dereference it, and every payload is Send + Sync.
unsafe impl Send for DeferredStreamCleanup {}
// SAFETY: Shared access cannot mutate or dereference the private link;
// the remaining fields support shared access through their own synchronization.
unsafe impl Sync for DeferredStreamCleanup {}

impl DeferredStreamCleanup {
    fn try_new() -> AxResult<Box<Self>> {
        let admission = StreamCleanupAdmission::try_acquire()?;
        Box::try_new(Self {
            next: ptr::null_mut(),
            channel: None,
            receiver: None,
            poll_state: None,
            _admission: admission,
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn run(mut self: Box<Self>) -> Option<Box<Self>> {
        if let Some(receiver) = self.receiver.take() {
            receiver.close();
            let mut drained = 0;
            while drained < STREAM_CLEANUP_REQUEST_BUDGET {
                match receiver.try_recv_deferred_wake() {
                    Ok((request, completion)) => {
                        request.channel.reset_and_wake();
                        completion.complete();
                        drained += 1;
                    }
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            }
            if !receiver.is_empty() {
                self.receiver = Some(receiver);
            } else {
                drop(receiver);
            }
        }
        if let Some(channel) = self.channel.take() {
            channel.close_orderly_and_wake();
        }
        if let Some(poll_state) = self.poll_state.take() {
            poll_state.wake();
            drop(poll_state);
        }
        self.receiver.is_some().then_some(self)
    }
}

static STREAM_CLEANUP_ADMISSIONS: AtomicUsize = AtomicUsize::new(0);

struct StreamCleanupAdmission;

impl StreamCleanupAdmission {
    fn try_acquire() -> AxResult<Self> {
        STREAM_CLEANUP_ADMISSIONS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < UNIX_STREAM_CLEANUP_SLOTS).then_some(current + 1)
            })
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self)
    }
}

impl Drop for StreamCleanupAdmission {
    fn drop(&mut self) {
        STREAM_CLEANUP_ADMISSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct DeferredStreamCleanupList {
    head: *mut DeferredStreamCleanup,
    tail: *mut DeferredStreamCleanup,
}

// SAFETY: The list exclusively owns its Box-derived nodes. Access is through
// &mut self under the global list lock, and pop transfers ownership to one task.
unsafe impl Send for DeferredStreamCleanupList {}

impl DeferredStreamCleanupList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    fn push(&mut self, work: Box<DeferredStreamCleanup>) {
        let raw = Box::into_raw(work);
        // SAFETY: raw is the newly transferred live Box; a non-null tail is
        // a live list-owned node. Exclusive list access protects both links.
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

    fn pop(&mut self) -> Option<Box<DeferredStreamCleanup>> {
        let raw = self.head;
        if raw.is_null() {
            return None;
        }
        // SAFETY: The checked non-null head is a live Box-owned node. Unlink
        // it under exclusive access before reconstructing its unique Box.
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

static STREAM_CLEANUP_LIST: SpinNoIrq<DeferredStreamCleanupList> =
    SpinNoIrq::new(DeferredStreamCleanupList::new());
static STREAM_CLEANUP_PENDING: AtomicBool = AtomicBool::new(false);

fn publish_stream_cleanup(
    channel: Option<Channel>,
    receiver: Option<Receiver<ConnRequest>>,
    poll_state: PollSet,
    mut work: Box<DeferredStreamCleanup>,
) {
    work.channel = channel;
    work.receiver = receiver;
    work.poll_state = Some(poll_state);
    let mut list = STREAM_CLEANUP_LIST.lock();
    list.push(work);
    STREAM_CLEANUP_PENDING.store(true, Ordering::Release);
}

fn requeue_stream_cleanup(work: Box<DeferredStreamCleanup>) {
    let mut list = STREAM_CLEANUP_LIST.lock();
    list.push(work);
    STREAM_CLEANUP_PENDING.store(true, Ordering::Release);
}

pub(super) fn has_deferred_cleanup_work() -> bool {
    STREAM_CLEANUP_PENDING.load(Ordering::Acquire)
}

pub(super) fn drain_deferred_cleanup_work() {
    for _ in 0..STREAM_CLEANUP_NODE_BUDGET {
        let work = {
            let mut list = STREAM_CLEANUP_LIST.lock();
            let work = list.pop();
            if list.head.is_null() {
                STREAM_CLEANUP_PENDING.store(false, Ordering::Release);
            }
            work
        };
        let Some(work) = work else {
            break;
        };
        if let Some(work) = work.run() {
            requeue_stream_cleanup(work);
        }
    }
}

/// Stream transport for Unix domain sockets.
pub struct StreamTransport {
    channel: Mutex<Option<Channel>>,
    conn_rx: Mutex<Option<Receiver<ConnRequest>>>,
    drop_cleanup: ManuallyDrop<Box<DeferredStreamCleanup>>,
    poll_state: PollSet,
    general: GeneralOptions,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    connect_state: AtomicU8,
}

const CONNECT_UNCONNECTED: u8 = 0;
const CONNECT_RESERVED: u8 = 1;
const CONNECT_CONNECTED: u8 = 2;
impl StreamTransport {
    /// Create a new unconnected stream transport.
    pub fn new() -> AxResult<Self> {
        StreamTransport::new_channel(None)
    }

    pub(super) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.general
            .transfer_poller(self, direction, effective_nonblocking, attempt)
    }

    fn new_channel(channel: Option<Channel>) -> AxResult<Self> {
        let drop_cleanup = DeferredStreamCleanup::try_new()?;
        Ok(Self::new_channel_with_cleanup(channel, drop_cleanup))
    }

    fn new_channel_with_cleanup(
        channel: Option<Channel>,
        drop_cleanup: Box<DeferredStreamCleanup>,
    ) -> Self {
        let connect_state = if channel.is_some() {
            CONNECT_CONNECTED
        } else {
            CONNECT_UNCONNECTED
        };
        StreamTransport {
            channel: Mutex::new(channel),
            conn_rx: Mutex::new(None),
            drop_cleanup: ManuallyDrop::new(drop_cleanup),
            poll_state: PollSet::new(),
            general: GeneralOptions::default(),
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
            connect_state: AtomicU8::new(connect_state),
        }
    }

    pub fn set_filter(&self, _filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        Err(AxError::Unsupported)
    }

    pub fn is_connected(&self) -> bool {
        self.connect_state.load(Ordering::Acquire) == CONNECT_CONNECTED
    }

    /// Returns the number of stream bytes currently available to receive.
    ///
    /// This is a non-consuming snapshot of the receive ring. The channel lock
    /// is released before the caller can perform any userspace copyout.
    pub(super) fn recv_pending_len(&self) -> AxResult<usize> {
        let channel = self.channel.lock();
        Ok(channel
            .as_ref()
            .and_then(|channel| channel.rx.as_ref())
            .map_or(0, |rx| rx.occupied_len()))
    }

    #[cfg(test)]
    pub(super) fn pending_connections(&self) -> usize {
        self.conn_rx.lock().as_ref().map_or(0, Receiver::len)
    }

    /// Create a connected pair of stream transports.
    pub fn new_pair(credentials: SocketCredentials) -> AxResult<(Self, Self)> {
        let (chan1, chan2) = new_channels(credentials, credentials)?;
        let transport1 = StreamTransport::new_channel(Some(chan1))?;
        let transport2 = StreamTransport::new_channel(Some(chan2))?;
        Ok((transport1, transport2))
    }

    pub(super) fn rollback_bind(&self, slot: &super::BindSlot) {
        let retired_bind = slot.stream.lock().take();
        let retired_receiver = self.conn_rx.lock().take();
        if let Some(receiver) = retired_receiver.as_ref() {
            receiver.close();
            // A cloned target may have been used before namespace commit.
            // Drain every bounded request and explicitly wake its client so a
            // rolled-back private listener cannot strand connected sleepers.
            for _ in 0..LISTEN_QUEUE_SIZE {
                match receiver.try_recv_deferred_wake() {
                    Ok((request, completion)) => {
                        request.channel.reset_and_wake();
                        completion.complete();
                    }
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            }
        }
        drop(retired_bind);
        drop(retired_receiver);
        self.poll_state.wake();
    }

    pub(super) fn prepare_connect<'a>(
        &'a self,
        slot: &super::BindSlot,
        local_addr: &UnixSocketAddr,
        credentials: SocketCredentials,
    ) -> AxResult<StreamConnectReservation<'a>> {
        if self
            .connect_state
            .compare_exchange(
                CONNECT_UNCONNECTED,
                CONNECT_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(AxError::AlreadyConnected);
        }

        let result = (|| {
            let bind = slot.stream.lock().as_ref().cloned();
            let bind = if let Some(bind) = bind {
                bind
            } else if slot.dgram.lock().is_some() || slot.seqpacket.lock().is_some() {
                return Err(AxError::OperationNotSupported);
            } else {
                return Err(AxError::ConnectionRefused);
            };
            let permit = bind.reserve()?;
            let listener_credentials = *bind.0.credentials.lock();
            let (client_channel, server_channel) = new_channels(credentials, listener_credentials)?;
            Ok(StreamConnectReservation {
                transport: self,
                listener: bind,
                permit: Some(permit),
                client_channel: Some(client_channel),
                request: Some(ConnRequest {
                    channel: server_channel,
                    addr: local_addr.clone(),
                }),
                committed: false,
            })
        })();

        if result.is_err() {
            self.connect_state
                .store(CONNECT_UNCONNECTED, Ordering::Release);
        }
        result
    }

    pub(super) fn prepare_accept(&self) -> AxResult<PreparedStreamAccept> {
        let drop_cleanup = DeferredStreamCleanup::try_new()?;
        let Some(rx) = self.conn_rx.lock().clone() else {
            return Err(AxError::NotConnected);
        };
        Ok(PreparedStreamAccept {
            receiver: rx,
            drop_cleanup: Some(drop_cleanup),
        })
    }
}

impl Configurable for StreamTransport {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(opt)? {
            if let O::Error(error) = opt
                && error.is_none()
                && self
                    .channel
                    .lock()
                    .as_ref()
                    .is_some_and(Channel::take_peer_reset)
            {
                **error = Some(SocketFault::ConnectionReset);
            }
            return Ok(true);
        }

        match opt {
            O::SendBuffer(size) | O::ReceiveBuffer(size) => {
                **size = BUF_SIZE;
            }
            O::PeerCredentials(cred) => {
                **cred = self
                    .channel
                    .lock()
                    .as_ref()
                    .map_or(SocketCredentials::UNKNOWN, |chan| chan.peer_credentials);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool> {
        if self.general.set_option_inner(opt)? {
            return Ok(true);
        }

        let _ = opt;
        Ok(false)
    }
}
impl TransportOps for StreamTransport {
    fn set_pending_error(&self, error: SocketFault) {
        self.general.set_pending_error(error);
    }

    fn bind(&self, slot: &super::BindSlot, _local_addr: &UnixSocketAddr) -> AxResult<()> {
        // Admission and queue storage are completely prepared before either
        // endpoint lock is acquired. Installation below only moves ownership.
        let (tx, rx) = try_bounded(LISTEN_QUEUE_SIZE)?;
        let prepared_bind = Bind::try_new(tx)?;
        let mut slot = slot.stream.lock();
        if slot.is_some() {
            return Err(AxError::AddrInUse);
        }
        let mut guard = self.conn_rx.lock();
        if guard.is_some() {
            return Err(AxError::InvalidInput);
        }
        *slot = Some(prepared_bind);
        *guard = Some(rx);
        drop(guard);
        drop(slot);
        self.poll_state.wake();
        Ok(())
    }

    fn listen(
        &self,
        slot: &super::BindSlot,
        backlog: usize,
        credentials: SocketCredentials,
    ) -> AxResult<()> {
        if self.conn_rx.lock().is_none() {
            return Err(AxError::InvalidInput);
        }
        let bind = slot
            .stream
            .lock()
            .as_ref()
            .cloned()
            .ok_or(AxError::InvalidInput)?;
        bind.start_listening(backlog, credentials)?;
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindSlot,
        local_addr: &UnixSocketAddr,
        credentials: SocketCredentials,
    ) -> AxResult<()> {
        self.prepare_connect(slot, local_addr, credentials)?
            .commit()
    }

    fn send(&self, mut src: impl Read + IoBuf, mut options: SendOptions) -> AxResult<usize> {
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        if options.to.is_some() {
            return Err(AxError::InvalidInput);
        }
        let size = src.remaining();
        let mut total = 0;
        let result = self.general.send_poller_with_effective_nonblocking(
            self,
            effective_nonblocking,
            || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(AxError::NotConnected);
                };
                if chan.peer_read_closed() {
                    return Err(AxError::BrokenPipe);
                }
                let segment = if options.cmsg.is_empty() {
                    None
                } else {
                    Some(
                        chan.segments_tx
                            .as_ref()
                            .ok_or(AxError::BrokenPipe)?
                            .try_reserve(STREAM_ANCILLARY_SEGMENTS)
                            .map_err(|_| AxError::WouldBlock)?,
                    )
                };
                let start = chan.tx_offset;
                let Some(tx) = chan.tx.as_mut() else {
                    return Err(AxError::BrokenPipe);
                };

                let count = {
                    let (left, right) = tx.vacant_slices_mut();
                    // Read is a safe trait: implementations may inspect every
                    // byte of the supplied buffer, so initialize vacant storage.
                    for byte in left.iter_mut().chain(right.iter_mut()) {
                        byte.write(0);
                    }
                    // SAFETY: Every byte was initialized above, the producer
                    // exclusively owns these vacant slots, and u8 has alignment 1.
                    let left = unsafe {
                        core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len())
                    };
                    // SAFETY: The second initialized slice is disjoint from the
                    // first (ringbuf's vacant-slice contract) and remains writable.
                    let right = unsafe {
                        core::slice::from_raw_parts_mut(
                            right.as_mut_ptr().cast::<u8>(),
                            right.len(),
                        )
                    };
                    let mut count = src.read(left)?;
                    if count > left.len() {
                        return Err(AxError::InvalidInput);
                    }
                    if count == left.len() {
                        let second = src.read(right)?;
                        if second > right.len() {
                            return Err(AxError::InvalidInput);
                        }
                        count += second;
                    }
                    // SAFETY: Validated counts cover only initialized vacant
                    // slots; no other producer can advance this channel's index.
                    unsafe { tx.advance_write_index(count) };
                    count
                };
                if count != 0 {
                    chan.tx_offset = chan.tx_offset.wrapping_add(count);
                    if let Some(segment) = segment {
                        segment
                            .send(AncillarySegment {
                                start,
                                cmsg: core::mem::take(&mut options.cmsg),
                            })
                            .map_err(|_| AxError::BrokenPipe)?;
                    }
                }
                total += count;
                let poll_update = (count > 0).then(|| chan.poll_update.clone());
                let result = finish_stream_send(total, size, effective_nonblocking);
                drop(guard);
                if let Some(poll_update) = poll_update {
                    poll_update.wake();
                }
                result
            },
        );
        // Once stream bytes have been queued, Linux reports that positive
        // progress instead of a later interruption, timeout, peer close, or
        // user-copy failure. Returning the error would invite user space to
        // retry bytes that the peer can already observe.
        match result {
            Err(_) if total != 0 => Ok(total),
            other => other,
        }
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> AxResult<usize> {
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        self.general
            .recv_poller_with_effective_nonblocking(self, effective_nonblocking, || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(AxError::NotConnected);
                };
                if chan.take_peer_reset() {
                    return Err(AxError::ConnectionReset);
                }
                let peer_write_closed = chan.peer_write_closed();
                let Some(rx) = chan.rx.as_mut() else {
                    return Ok(0);
                };

                let count = {
                    let (left, right) = rx.as_slices();
                    let mut count = dst.write(left)?;
                    if count > left.len() {
                        return Err(AxError::InvalidInput);
                    }
                    if count == left.len() {
                        let second = dst.write(right)?;
                        if second > right.len() {
                            return Err(AxError::InvalidInput);
                        }
                        count += second;
                    }
                    if !options.flags.contains(RecvFlags::PEEK) {
                        // SAFETY: Both returned counts were bounded by occupied
                        // slices, and this locked channel owns the only consumer.
                        unsafe { rx.advance_read_index(count) };
                    }
                    count
                };
                let cmsg_start = chan.rx_offset;
                if count != 0 && !options.flags.contains(RecvFlags::PEEK) {
                    chan.rx_offset = chan.rx_offset.wrapping_add(count);
                }
                if count != 0 {
                    if let Some(segments) = chan.segments_rx.as_ref()
                        && segments
                            .with_front(|segment| segment.start == cmsg_start)
                            .unwrap_or(false)
                    {
                        let reservation = segments
                            .try_reserve_inner()
                            .map_err(|_| AxError::BadState)?;
                        if options.flags.contains(RecvFlags::PEEK) {
                            if let Some(output) = options.cmsg.as_mut() {
                                for cmsg in &reservation.item().cmsg {
                                    output.push(cmsg.clone_for_peek()?);
                                }
                            }
                            let _ = reservation.cancel();
                        } else {
                            let segment = reservation.commit().map_err(|_| AxError::BrokenPipe)?;
                            if let Some(output) = options.cmsg.as_mut() {
                                output.extend(segment.cmsg);
                            }
                        }
                    }
                }
                let poll_update = (count > 0).then(|| chan.poll_update.clone());
                let result = if count > 0 {
                    Ok(count)
                } else if peer_write_closed {
                    Ok(0)
                } else {
                    Err(AxError::WouldBlock)
                };
                drop(guard);
                if let Some(poll_update) = poll_update {
                    poll_update.wake();
                }
                result
            })
    }

    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        let (retired_rx, retired_tx, retired_segments_rx, retired_segments_tx, poll_update) = {
            let mut channel = self.channel.lock();
            let channel = channel.as_mut().ok_or(AxError::NotConnected)?;
            let retired_rx = if how.has_read() {
                self.rx_closed.store(true, Ordering::Release);
                channel.publish_read_close();
                channel.rx.take()
            } else {
                None
            };
            let retired_tx = if how.has_write() {
                self.tx_closed.store(true, Ordering::Release);
                channel.publish_write_close();
                channel.tx.take()
            } else {
                None
            };
            let (retired_segments_rx, retired_segments_tx) =
                channel.retire_ancillary(how.has_read(), how.has_write());
            (
                retired_rx,
                retired_tx,
                retired_segments_rx,
                retired_segments_tx,
                channel.poll_update.clone(),
            )
        };
        drop(retired_rx);
        drop(retired_tx);
        if let Some(receiver) = retired_segments_rx {
            receiver.close();
            while let Ok(segment) = receiver.try_recv() {
                drop(segment);
            }
        }
        drop(retired_segments_tx);
        poll_update.wake();
        self.poll_state.wake();
        Ok(())
    }
}

impl Pollable for StreamTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some(chan) = self.channel.lock().as_ref() {
            let peer_write_closed = chan.peer_write_closed();
            let peer_read_closed = chan.peer_read_closed();
            let peer_reset_pending = chan.peer_reset_pending();
            events.set(
                IoEvents::READABLE,
                self.rx_closed.load(Ordering::Acquire)
                    || peer_write_closed
                    || chan.rx.as_ref().is_some_and(|rx| rx.occupied_len() > 0),
            );
            events.set(
                IoEvents::WRITABLE,
                !self.tx_closed.load(Ordering::Acquire)
                    && chan
                        .tx
                        .as_ref()
                        .is_some_and(|tx| peer_read_closed || tx.vacant_len() > 0),
            );
            events.set(IoEvents::ERROR, peer_reset_pending);
            events.set(IoEvents::READ_HANGUP, peer_write_closed);
            events.set(IoEvents::HANGUP, peer_read_closed && peer_write_closed);
        } else if let Some(conn_rx) = self.conn_rx.lock().as_ref() {
            events.set(IoEvents::READABLE, !conn_rx.is_empty());
        }
        self.general.add_pending_error_event(events)
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let channel_poll = self
            .channel
            .lock()
            .as_ref()
            .map(|channel| channel.poll_update.clone());
        let channel_poll = channel_poll.filter(|_| {
            events.intersects(
                IoEvents::READABLE
                    | IoEvents::WRITABLE
                    | IoEvents::ERROR
                    | IoEvents::HANGUP
                    | IoEvents::READ_HANGUP,
            )
        });
        let listener_poll = if channel_poll.is_none() && events.contains(IoEvents::READABLE) {
            self.conn_rx.lock().as_ref().map(Receiver::read_poll_source)
        } else {
            None
        };
        let maximum =
            1 + usize::from(channel_poll.is_some()) + usize::from(listener_poll.is_some());
        let mut prepared = PreparedPollRegistration::try_new(maximum)?;
        if let Some(source) = channel_poll {
            prepared.arm_owned(source, context.waker())?;
        }
        if let Some(source) = listener_poll {
            prepared.arm_owned(source, context.waker())?;
        }
        prepared.arm(&self.poll_state, context.waker())?;
        prepared.commit()
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        let retired_channel = self.channel.get_mut().take();
        let retired_receiver = self.conn_rx.get_mut().take();
        if let Some(channel) = retired_channel.as_ref() {
            channel.publish_orderly_close();
        }
        if let Some(channel) = retired_channel.as_ref() {
            if let Some(receiver) = channel.segments_rx.as_ref() {
                receiver.close_without_wake_and_visit(|_| {});
            }
        }
        if let Some(receiver) = retired_receiver.as_ref() {
            receiver.close_without_wake_and_visit(|request| request.channel.publish_reset());
        }
        let retired_poll_state = core::mem::replace(&mut self.poll_state, PollSet::new());
        // SAFETY: every constructor initializes this private field exactly
        // once, and Drop runs at most once. The worker takes over the Box so
        // no channel endpoint, receiver, PollSet, or waker is destroyed here.
        let cleanup = unsafe { ManuallyDrop::take(&mut self.drop_cleanup) };
        publish_stream_cleanup(
            retired_channel,
            retired_receiver,
            retired_poll_state,
            cleanup,
        );
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;

    use super::*;
    use crate::SendFlags;

    #[test]
    fn safe_io_implementations_cannot_corrupt_stream_ring() {
        struct InspectReader;
        impl Read for InspectReader {
            fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
                assert!(buf.iter().all(|byte| *byte == 0));
                if buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = b'x';
                Ok(1)
            }
        }
        impl IoBuf for InspectReader {
            fn remaining(&self) -> usize {
                1
            }
        }
        struct OversizedIo;
        impl Read for OversizedIo {
            fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
                Ok(buf.len() + 1)
            }
        }
        impl IoBuf for OversizedIo {
            fn remaining(&self) -> usize {
                1
            }
        }
        impl Write for OversizedIo {
            fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
                Ok(buf.len() + 1)
            }
            fn flush(&mut self) -> axio::Result<()> {
                Ok(())
            }
        }
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        let (left, right) = StreamTransport::new_pair(SocketCredentials::new(1, 2, 3)).unwrap();
        assert_eq!(
            left.send(OversizedIo, SendOptions::default()),
            Err(AxError::InvalidInput)
        );
        assert_eq!(left.send(InspectReader, SendOptions::default()), Ok(1));
        assert_eq!(
            right.recv(OversizedIo, RecvOptions::default()),
            Err(AxError::InvalidInput)
        );
        let mut output = [0; 1];
        assert_eq!(right.recv(&mut output[..], RecvOptions::default()), Ok(1));
        assert_eq!(output, *b"x");
        drop((left, right));
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
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

    fn peer_credentials(transport: &StreamTransport) -> SocketCredentials {
        let mut credentials = SocketCredentials::default();
        transport
            .get_option(GetSocketOption::PeerCredentials(&mut credentials))
            .unwrap();
        credentials
    }

    fn socket_error(transport: &StreamTransport) -> Option<SocketFault> {
        let mut error = None;
        transport
            .get_option(GetSocketOption::Error(&mut error))
            .unwrap();
        error
    }

    fn closed_stream_events() -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::HANGUP | IoEvents::READ_HANGUP
    }

    #[test]
    fn unconnected_stream_reports_unknown_peer_credentials() {
        let transport = StreamTransport::new().unwrap();
        assert_eq!(peer_credentials(&transport), SocketCredentials::UNKNOWN);
    }

    #[test]
    fn stream_drop_defers_endpoint_and_waker_teardown() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        // poll(2) must wake for its unconditional ERR/HUP interests even when
        // user space did not request IN or OUT.
        let registration = left
            .register(&mut context, IoEvents::ERROR | IoEvents::HANGUP)
            .unwrap();

        drop(right);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        let events = left.poll();
        assert_eq!(events.bits(), closed_stream_events().bits());
        assert_eq!(socket_error(&left), None);
        assert_eq!(
            left.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        assert!(super::super::has_deferred_receive_cleanup_work());
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        assert!(wakes.load(Ordering::SeqCst) > 0);
        assert_eq!(left.poll().bits(), closed_stream_events().bits());
        assert_eq!(socket_error(&left), None);

        drop(registration);
        drop(left);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn listener_drop_wakes_clients_with_queued_connections() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-listener-drop-wake",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();

        let client = StreamTransport::new().unwrap();
        client
            .connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap();
        assert_eq!(listener.pending_connections(), 1);

        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        let registration = client
            .register(&mut context, IoEvents::READABLE | IoEvents::WRITABLE)
            .unwrap();

        drop(listener);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        let immediate_events = client.poll();
        assert_eq!(
            immediate_events.bits(),
            (closed_stream_events() | IoEvents::ERROR).bits()
        );
        assert_eq!(socket_error(&client), Some(SocketFault::ConnectionReset));
        assert_eq!(socket_error(&client), None);
        assert_eq!(client.poll().bits(), closed_stream_events().bits());
        assert_eq!(
            client.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        let mut byte = [0u8; 1];
        assert_eq!(
            client.recv(
                &mut byte[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Ok(0)
        );
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        assert!(wakes.load(Ordering::SeqCst) > 0);
        let events = client.poll();
        assert_eq!(events.bits(), closed_stream_events().bits());
        assert_eq!(socket_error(&client), None);

        drop(registration);
        drop(client);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn listener_reset_is_consumed_once_by_recv_but_not_send() {
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-listener-reset-recv",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();

        let client = StreamTransport::new().unwrap();
        client
            .connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap();
        drop(listener);

        assert_eq!(
            client.poll().bits(),
            (closed_stream_events() | IoEvents::ERROR).bits()
        );
        assert_eq!(
            client.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        assert!(client.poll().contains(IoEvents::ERROR));

        let mut byte = [0u8; 1];
        assert_eq!(
            client.recv(
                &mut byte[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Err(AxError::ConnectionReset)
        );
        assert_eq!(client.poll().bits(), closed_stream_events().bits());
        assert_eq!(
            client.recv(
                &mut byte[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                }
            ),
            Ok(0)
        );
        assert_eq!(socket_error(&client), None);

        drop(client);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn listener_drop_publishes_close_beyond_one_cleanup_batch() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-listener-drop-batch",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener
            .listen(&slot, LISTEN_QUEUE_SIZE, credentials)
            .unwrap();

        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        let mut clients = alloc::vec::Vec::new();
        for _ in 0..=STREAM_CLEANUP_REQUEST_BUDGET {
            let client = StreamTransport::new().unwrap();
            client
                .connect(&slot, &UnixSocketAddr::Unnamed, credentials)
                .unwrap();
            clients.push(client);
        }

        let _registrations = clients
            .iter()
            .map(|client| client.register(&mut context, IoEvents::READABLE | IoEvents::WRITABLE))
            .collect::<Result<alloc::vec::Vec<_>, _>>()
            .unwrap();

        drop(listener);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        for client in &clients {
            let events = client.poll();
            assert!(events.contains(IoEvents::READ_HANGUP));
            assert!(events.contains(IoEvents::ERROR));
            assert!(events.contains(IoEvents::HANGUP));
            assert_eq!(
                client.send(&b"x"[..], SendOptions::default()),
                Err(AxError::BrokenPipe)
            );
        }

        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        assert!(wakes.load(Ordering::SeqCst) > 0);
        drop(_registrations);
        drop(clients);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn peer_drop_preserves_queued_bytes_before_eof() {
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();
        assert_eq!(left.send(&b"data"[..], SendOptions::default()).unwrap(), 4);
        drop(left);

        let mut bytes = [0u8; 4];
        assert_eq!(
            right
                .recv(
                    &mut bytes[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            4
        );
        assert_eq!(&bytes, b"data");
        assert_eq!(
            right
                .recv(
                    &mut bytes[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    },
                )
                .unwrap(),
            0
        );

        drop(right);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn recv_pending_len_is_a_non_consuming_stream_snapshot() {
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();
        assert_eq!(right.recv_pending_len().unwrap(), 0);
        assert_eq!(
            left.send(&b"stream-data"[..], SendOptions::default()),
            Ok(11)
        );
        assert_eq!(right.recv_pending_len().unwrap(), 11);
        assert_eq!(right.recv_pending_len().unwrap(), 11);

        let mut bytes = [0u8; 11];
        assert_eq!(
            right.recv(
                &mut bytes[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ..RecvOptions::default()
                },
            ),
            Ok(11)
        );
        assert_eq!(&bytes, b"stream-data");
        assert_eq!(right.recv_pending_len().unwrap(), 0);

        drop(left);
        drop(right);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn connected_streams_snapshot_the_opposite_peer_credentials() {
        let left = SocketCredentials::new(11, 12, 13);
        let right = SocketCredentials::new(21, 22, 23);
        let (left_channel, right_channel) = new_channels(left, right).unwrap();
        let left_transport = StreamTransport::new_channel(Some(left_channel)).unwrap();
        let right_transport = StreamTransport::new_channel(Some(right_channel)).unwrap();

        assert_eq!(peer_credentials(&left_transport), right);
        assert_eq!(peer_credentials(&right_transport), left);
    }

    #[test]
    fn stream_connect_snapshots_listen_and_connect_time_credentials() {
        let listen_credentials = SocketCredentials::new(101, 102, 103);
        let connect_credentials = SocketCredentials::new(201, 202, 203);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-operation-credentials",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, listen_credentials).unwrap();

        let client = StreamTransport::new().unwrap();
        client
            .connect(&slot, &UnixSocketAddr::Unnamed, connect_credentials)
            .unwrap();
        assert_eq!(peer_credentials(&client), listen_credentials);

        let request = listener
            .conn_rx
            .lock()
            .as_ref()
            .unwrap()
            .try_recv()
            .unwrap();
        let accepted = StreamTransport::new_channel(Some(request.channel)).unwrap();
        assert_eq!(peer_credentials(&accepted), connect_credentials);
    }

    #[test]
    fn stream_connect_reservation_is_private_and_retryable_after_drop() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-stream-connect-reservation",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();
        let client = StreamTransport::new().unwrap();

        let reservation = client
            .prepare_connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap();
        assert_ne!(reservation.listener_identity(), 0);
        assert_ne!(reservation.accepted_identity(), 0);
        assert!(!client.is_connected());
        assert_eq!(listener.pending_connections(), 0);
        drop(reservation);
        assert!(!client.is_connected());
        assert_eq!(listener.pending_connections(), 0);

        client
            .prepare_connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap()
            .commit()
            .unwrap();
        assert!(client.is_connected());
        assert_eq!(listener.pending_connections(), 1);
    }

    #[test]
    fn stream_accept_reservation_drop_restores_the_exact_request() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-stream-accept-reservation",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();
        let client = StreamTransport::new().unwrap();
        client
            .connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap();

        let receiver = listener.conn_rx.lock().as_ref().unwrap().clone();
        let request = receiver.try_reserve_inner().unwrap();
        let identity = request.item().identity();
        let reservation = StreamAcceptReservation {
            request: Some(request),
            drop_cleanup: Some(DeferredStreamCleanup::try_new().unwrap()),
        };
        assert_eq!(reservation.accepted_identity(), identity);
        assert_eq!(listener.pending_connections(), 0);
        drop(reservation);
        assert_eq!(listener.pending_connections(), 1);

        let restored = receiver.try_recv().unwrap();
        assert_eq!(restored.identity(), identity);
    }

    #[test]
    fn prepared_accept_wait_does_not_reenter_the_endpoint_mutex() {
        use core::task::Waker;

        let _cleanup_guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-prepared-accept-wait",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();

        let mut prepared = listener.prepare_accept().unwrap();
        let readers = prepared.receiver.read_poll_source();
        let endpoint_guard = listener.conn_rx.lock();
        let mut context = Context::from_waker(Waker::noop());
        {
            let mut wait = core::pin::pin!(prepared.wait());

            // The old async preparation tried to acquire conn_rx here and
            // could enter a nested synchronous block session. Preparation now
            // happened before this future existed, so its first poll reaches
            // only queue state and the bounded reader PollSet.
            assert!(wait.as_mut().poll(&mut context).is_pending());
            assert_eq!(readers.len(), 1);
        }
        assert!(readers.is_empty());
        drop(endpoint_guard);
        drop(prepared);
        drop(listener);

        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn listener_close_during_accept_reservation_resets_client() {
        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        let credentials = SocketCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(
            alloc::string::String::from(
                "/home/ava/.cache/thekernel-test-tmp/axnet-stream-accept-close",
            )
            .into_bytes(),
        ));
        listener.bind(&slot, &address).unwrap();
        listener.listen(&slot, 1, credentials).unwrap();
        let client = StreamTransport::new().unwrap();
        client
            .connect(&slot, &UnixSocketAddr::Unnamed, credentials)
            .unwrap();

        let receiver = listener.conn_rx.lock().as_ref().unwrap().clone();
        let reservation = StreamAcceptReservation {
            request: Some(receiver.try_reserve_inner().unwrap()),
            drop_cleanup: Some(DeferredStreamCleanup::try_new().unwrap()),
        };
        drop(listener);
        drop(reservation);
        assert!(client.poll().contains(IoEvents::ERROR));
        assert_eq!(socket_error(&client), Some(SocketFault::ConnectionReset));
        drop(client);
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
    }

    #[test]
    fn stream_send_progress_uses_total_and_preserves_nonblocking_short_write() {
        assert_eq!(finish_stream_send(7, 7, false), Ok(7));
        assert_eq!(finish_stream_send(3, 7, true), Ok(3));
        assert_eq!(finish_stream_send(0, 7, true), Err(AxError::WouldBlock));
        assert_eq!(finish_stream_send(3, 7, false), Err(AxError::WouldBlock));
    }

    #[test]
    fn per_call_nonblocking_stream_send_returns_the_queued_prefix() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();
        let flags = SendFlags::DONT_WAIT;

        let prefill = alloc::vec![0x11u8; BUF_SIZE - 1];
        assert_eq!(
            left.send(
                &prefill[..],
                SendOptions {
                    flags,
                    ..SendOptions::default()
                }
            )
            .unwrap(),
            prefill.len()
        );

        let tail = [0x22u8, 0x33];
        assert_eq!(
            left.send(
                &tail[..],
                SendOptions {
                    flags,
                    ..SendOptions::default()
                }
            )
            .unwrap(),
            1
        );

        let mut received = alloc::vec![0u8; BUF_SIZE];
        assert_eq!(
            right
                .recv(
                    &mut received[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..RecvOptions::default()
                    }
                )
                .unwrap(),
            BUF_SIZE
        );
        assert!(received[..BUF_SIZE - 1].iter().all(|byte| *byte == 0x11));
        assert_eq!(received[BUF_SIZE - 1], 0x22);
    }

    #[test]
    fn listener_rejects_connect_before_listen_and_enforces_backlog() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let (tx, rx) = try_bounded(LISTEN_QUEUE_SIZE).unwrap();
        let bind = Bind::try_new(tx).unwrap();

        assert_eq!(bind.reserve().err().unwrap(), AxError::ConnectionRefused);

        bind.start_listening(1, credentials).unwrap();
        let permit = bind.reserve().unwrap();
        assert_eq!(bind.reserve().err().unwrap(), AxError::ConnectionRefused);
        permit
            .send(ConnRequest {
                channel: new_channels(credentials, credentials).unwrap().1,
                addr: UnixSocketAddr::Unnamed,
            })
            .ok()
            .unwrap();
        drop(rx.try_recv().unwrap());
        assert!(bind.reserve().is_ok());
    }

    #[test]
    fn stream_write_shutdown_preserves_queued_data_and_closes_peer_writer() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();

        assert_eq!(
            left.channel
                .lock()
                .as_mut()
                .unwrap()
                .tx
                .as_mut()
                .unwrap()
                .push_slice(b"x"),
            1
        );
        left.shutdown(Shutdown::Write).unwrap();
        assert!(left.channel.lock().as_ref().unwrap().tx.is_none());
        assert!(right.poll().contains(IoEvents::READ_HANGUP));

        let mut byte = [0u8; 1];
        let mut right_channel = right.channel.lock();
        let rx = right_channel.as_mut().unwrap().rx.as_mut().unwrap();
        assert_eq!(rx.pop_slice(&mut byte), 1);
        assert_eq!(byte, [b'x']);
        assert!(!rx.write_is_held());
    }

    #[test]
    fn stream_read_shutdown_breaks_peer_writes() {
        let credentials = SocketCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();

        right.shutdown(Shutdown::Read).unwrap();
        assert!(right.channel.lock().as_ref().unwrap().rx.is_none());
        assert!(
            !left
                .channel
                .lock()
                .as_ref()
                .unwrap()
                .tx
                .as_ref()
                .unwrap()
                .read_is_held()
        );
    }
}
