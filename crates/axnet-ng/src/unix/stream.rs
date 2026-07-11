use alloc::{boxed::Box, sync::Arc};
use core::{
    mem::ManuallyDrop,
    ptr,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    task::Context,
};

use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::{Mutex, spin::SpinNoIrq};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::{
    RecvFlags, RecvOptions, SendFlags, SendOptions, Shutdown,
    consts::{LISTEN_QUEUE_SIZE, TCP_TX_BUF_LEN},
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
    socket::SocketFilter,
    unix::{
        Transport, TransportOps, UnixSocketAddr,
        queue::{PermitSendError, Receiver, SendPermit, Sender, TryRecvError, try_bounded},
    },
};

// Match the default socket send buffer so large splice/socketpair transfers do
// not deadlock behind an unrealistically tiny in-kernel Unix stream queue.
const BUF_SIZE: usize = TCP_TX_BUF_LEN;

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
    left_credentials: UnixCredentials,
    right_credentials: UnixCredentials,
) -> AxResult<(Channel, Channel)> {
    let (client_tx, server_rx) = new_uni_channel()?;
    let (server_tx, client_rx) = new_uni_channel()?;
    let poll_update = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
    let left_close = Arc::try_new(AtomicU8::new(0)).map_err(|_| AxError::NoMemory)?;
    let right_close = Arc::try_new(AtomicU8::new(0)).map_err(|_| AxError::NoMemory)?;
    Ok((
        Channel {
            tx: Some(client_tx),
            rx: Some(client_rx),
            poll_update: poll_update.clone(),
            peer_credentials: right_credentials,
            local_close: left_close.clone(),
            peer_close: right_close.clone(),
        },
        Channel {
            tx: Some(server_tx),
            rx: Some(server_rx),
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

struct Channel {
    tx: Option<HeapProd<u8>>,
    rx: Option<HeapCons<u8>>,
    // TODO: granularity
    poll_update: Arc<PollSet>,
    peer_credentials: UnixCredentials,
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

    fn publish_close(&self) {
        self.local_close
            .fetch_or(STREAM_BOTH_CLOSED, Ordering::AcqRel);
    }

    fn peer_read_closed(&self) -> bool {
        self.peer_close.load(Ordering::Acquire) & STREAM_READ_CLOSED != 0
            || self.tx.as_ref().is_some_and(|tx| !tx.read_is_held())
    }

    fn peer_write_closed(&self) -> bool {
        self.peer_close.load(Ordering::Acquire) & STREAM_WRITE_CLOSED != 0
            || self.rx.as_ref().is_some_and(|rx| !rx.write_is_held())
    }

    fn close_and_wake(self) {
        self.publish_close();
        let poll_update = self.poll_update.clone();
        drop(self);
        poll_update.wake();
    }
}

struct Listener {
    conn_tx: Sender<ConnRequest>,
    credentials: Mutex<UnixCredentials>,
    backlog: AtomicUsize,
}

#[derive(Clone)]
pub struct Bind(Arc<Listener>);

impl Bind {
    fn try_new(conn_tx: Sender<ConnRequest>) -> AxResult<Self> {
        Arc::try_new(Listener {
            conn_tx,
            credentials: Mutex::new(UnixCredentials::UNKNOWN),
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

    fn start_listening(&self, backlog: usize, credentials: UnixCredentials) -> AxResult<()> {
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

// `next` is only mutated under unique ownership or the intrusive-list lock.
// Shared references cannot dereference it, and every payload is Send + Sync.
unsafe impl Send for DeferredStreamCleanup {}
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
                        request.channel.close_and_wake();
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
            channel.close_and_wake();
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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

    #[cfg(test)]
    pub(super) fn pending_connections(&self) -> usize {
        self.conn_rx.lock().as_ref().map_or(0, Receiver::len)
    }

    /// Create a connected pair of stream transports.
    pub fn new_pair(credentials: UnixCredentials) -> AxResult<(Self, Self)> {
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
                        request.channel.close_and_wake();
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
}

impl Configurable for StreamTransport {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(opt)? {
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
                    .map_or(UnixCredentials::UNKNOWN, |chan| chan.peer_credentials);
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
#[async_trait]
impl TransportOps for StreamTransport {
    fn set_pending_error(&self, error: LinuxError) {
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
        credentials: UnixCredentials,
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
        credentials: UnixCredentials,
    ) -> AxResult<()> {
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
            // Clone only the stable listener handle under the slot lock. Queue
            // admission, ring allocation and publication all happen after it
            // is released.
            let bind = slot.stream.lock().as_ref().cloned();
            let bind = if let Some(bind) = bind {
                bind
            } else if slot.dgram.lock().is_some() {
                return Err(LinuxError::EPROTOTYPE.into());
            } else {
                return Err(AxError::ConnectionRefused);
            };
            let permit = bind.reserve()?;
            let listener_credentials = *bind.0.credentials.lock();
            let (client_channel, server_channel) = new_channels(credentials, listener_credentials)?;
            let request = ConnRequest {
                channel: server_channel,
                addr: local_addr.clone(),
            };
            if let Err(PermitSendError::Closed(request)) = permit.send(request) {
                // Channel endpoints can own wake state and ring buffers; keep
                // their destruction outside every endpoint/queue lock.
                drop(request);
                drop(client_channel);
                return Err(AxError::ConnectionRefused);
            }
            // The CAS above is the sole transition into CONNECT_RESERVED, and
            // no other operation can install a channel in that state. This is
            // therefore an infallible ownership move after listener enqueue.
            *self.channel.lock() = Some(client_channel);
            self.connect_state
                .store(CONNECT_CONNECTED, Ordering::Release);
            self.poll_state.wake();
            Ok(())
        })();

        if result.is_err() {
            self.connect_state
                .store(CONNECT_UNCONNECTED, Ordering::Release);
        }
        result
    }

    async fn accept(&self) -> AxResult<(Transport, UnixSocketAddr)> {
        // Admission must precede dequeue. Once a client-visible connection is
        // removed from the listen queue, construction is an infallible move;
        // otherwise ENOMEM here could drop the server endpoint without waking
        // the already connected client.
        let drop_cleanup = DeferredStreamCleanup::try_new()?;
        let Some(rx) = self.conn_rx.lock().clone() else {
            return Err(AxError::NotConnected);
        };
        let ConnRequest {
            channel,
            addr: peer_addr,
        } = rx.recv().await.map_err(|_| AxError::ConnectionReset)?;
        Ok((
            Transport::Stream(StreamTransport::new_channel_with_cleanup(
                Some(channel),
                drop_cleanup,
            )),
            peer_addr,
        ))
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if !options.cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        let per_call_nonblocking = options.flags.contains(SendFlags::DONT_WAIT);
        if options.to.is_some() {
            return Err(AxError::InvalidInput);
        }
        let size = src.remaining();
        let mut total = 0;
        let effective_nonblocking = self.general.nonblocking() || per_call_nonblocking;
        let result = self
            .general
            .send_poller_with_nonblocking(self, per_call_nonblocking, || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(AxError::NotConnected);
                };
                if chan.peer_read_closed() {
                    return Err(AxError::BrokenPipe);
                }
                let Some(tx) = chan.tx.as_mut() else {
                    return Err(AxError::BrokenPipe);
                };

                let count = {
                    let (left, right) = tx.vacant_slices_mut();
                    // The ring buffer guarantees these vacant slices are fully
                    // writable byte ranges.
                    let left = unsafe {
                        core::slice::from_raw_parts_mut(left.as_mut_ptr().cast::<u8>(), left.len())
                    };
                    let right = unsafe {
                        core::slice::from_raw_parts_mut(
                            right.as_mut_ptr().cast::<u8>(),
                            right.len(),
                        )
                    };
                    let mut count = src.read(left)?;
                    if count >= left.len() {
                        count += src.read(right)?;
                    }
                    unsafe { tx.advance_write_index(count) };
                    count
                };
                total += count;
                let poll_update = (count > 0).then(|| chan.poll_update.clone());
                let result = finish_stream_send(total, size, effective_nonblocking);
                drop(guard);
                if let Some(poll_update) = poll_update {
                    poll_update.wake();
                }
                result
            });
        // Once stream bytes have been queued, Linux reports that positive
        // progress instead of a later interruption, timeout, peer close, or
        // user-copy failure. Returning the error would invite user space to
        // retry bytes that the peer can already observe.
        match result {
            Err(_) if total != 0 => Ok(total),
            other => other,
        }
    }

    fn recv(&self, mut dst: impl Write, options: RecvOptions) -> AxResult<usize> {
        let per_call_nonblocking = options.flags.contains(RecvFlags::DONT_WAIT);
        self.general
            .recv_poller_with_nonblocking(self, per_call_nonblocking, || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(AxError::NotConnected);
                };
                let peer_write_closed = chan.peer_write_closed();
                let Some(rx) = chan.rx.as_mut() else {
                    return Ok(0);
                };

                let count = {
                    let (left, right) = rx.as_slices();
                    let mut count = dst.write(left)?;
                    if count >= left.len() {
                        count += dst.write(right)?;
                    }
                    if !options.flags.contains(RecvFlags::PEEK) {
                        unsafe { rx.advance_read_index(count) };
                    }
                    count
                };
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
        let (retired_rx, retired_tx, poll_update) = {
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
            (retired_rx, retired_tx, channel.poll_update.clone())
        };
        drop(retired_rx);
        drop(retired_tx);
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
            events.set(
                IoEvents::IN,
                self.rx_closed.load(Ordering::Acquire)
                    || peer_write_closed
                    || chan.rx.as_ref().is_some_and(|rx| rx.occupied_len() > 0),
            );
            events.set(
                IoEvents::OUT,
                !self.tx_closed.load(Ordering::Acquire)
                    && chan
                        .tx
                        .as_ref()
                        .is_some_and(|tx| peer_read_closed || tx.vacant_len() > 0),
            );
            events.set(IoEvents::ERR, peer_read_closed);
            events.set(IoEvents::RDHUP, peer_write_closed);
            events.set(IoEvents::HUP, peer_read_closed && peer_write_closed);
        } else if let Some(conn_rx) = self.conn_rx.lock().as_ref() {
            events.set(IoEvents::IN, !conn_rx.is_empty());
        }
        self.general.add_pending_error_event(events)
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        let channel_poll = self
            .channel
            .lock()
            .as_ref()
            .map(|channel| channel.poll_update.clone());
        if let Some(channel_poll) = channel_poll {
            if events.intersects(IoEvents::IN | IoEvents::OUT) {
                channel_poll.register(context.waker());
            }
        } else if events.contains(IoEvents::IN) {
            let receiver = self.conn_rx.lock().clone();
            if let Some(receiver) = receiver {
                receiver.register_read(context.waker());
            }
        }
        self.poll_state.register(context.waker());
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        let retired_channel = self.channel.get_mut().take();
        let retired_receiver = self.conn_rx.get_mut().take();
        if let Some(channel) = retired_channel.as_ref() {
            channel.publish_close();
        }
        if let Some(receiver) = retired_receiver.as_ref() {
            receiver.close_without_wake_and_visit(|request| request.channel.publish_close());
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
    use super::*;

    struct CountingWake(Arc<AtomicUsize>);

    impl alloc::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn peer_credentials(transport: &StreamTransport) -> UnixCredentials {
        let mut credentials = UnixCredentials::default();
        transport
            .get_option(GetSocketOption::PeerCredentials(&mut credentials))
            .unwrap();
        credentials
    }

    #[test]
    fn unconnected_stream_reports_unknown_peer_credentials() {
        let transport = StreamTransport::new().unwrap();
        assert_eq!(peer_credentials(&transport), UnixCredentials::UNKNOWN);
    }

    #[test]
    fn stream_drop_defers_endpoint_and_waker_teardown() {
        use core::task::Waker;

        let _guard = super::super::UNIX_CLEANUP_TEST_LOCK.lock();
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        left.register(&mut context, IoEvents::IN | IoEvents::OUT);
        right.register(&mut context, IoEvents::IN | IoEvents::OUT);

        drop(right);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        let events = left.poll();
        assert!(events.contains(IoEvents::RDHUP));
        assert!(events.contains(IoEvents::ERR));
        assert!(events.contains(IoEvents::HUP));
        assert_eq!(
            left.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        assert!(super::super::has_deferred_receive_cleanup_work());
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        assert!(wakes.load(Ordering::SeqCst) > 0);

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

        let credentials = UnixCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(alloc::string::String::from(
            "/tmp/axnet-listener-drop-wake",
        )));
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
        client.register(&mut context, IoEvents::IN | IoEvents::OUT);

        drop(listener);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        let immediate_events = client.poll();
        assert!(immediate_events.contains(IoEvents::RDHUP));
        assert!(immediate_events.contains(IoEvents::ERR));
        assert!(immediate_events.contains(IoEvents::HUP));
        assert_eq!(
            client.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }

        assert!(wakes.load(Ordering::SeqCst) > 0);
        let events = client.poll();
        assert!(events.contains(IoEvents::RDHUP));
        assert!(events.contains(IoEvents::ERR));
        assert!(events.contains(IoEvents::HUP));

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

        let credentials = UnixCredentials::new(1, 2, 3);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(alloc::string::String::from(
            "/tmp/axnet-listener-drop-batch",
        )));
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
            client.register(&mut context, IoEvents::IN | IoEvents::OUT);
            clients.push(client);
        }

        drop(listener);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        for client in &clients {
            let events = client.poll();
            assert!(events.contains(IoEvents::RDHUP));
            assert!(events.contains(IoEvents::ERR));
            assert!(events.contains(IoEvents::HUP));
            assert_eq!(
                client.send(&b"x"[..], SendOptions::default()),
                Err(AxError::BrokenPipe)
            );
        }

        while super::super::has_deferred_receive_cleanup_work() {
            super::super::drain_deferred_receive_cleanup_work();
        }
        assert!(wakes.load(Ordering::SeqCst) > 0);
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

        let credentials = UnixCredentials::new(1, 2, 3);
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
    fn connected_streams_snapshot_the_opposite_peer_credentials() {
        let left = UnixCredentials::new(11, 12, 13);
        let right = UnixCredentials::new(21, 22, 23);
        let (left_channel, right_channel) = new_channels(left, right).unwrap();
        let left_transport = StreamTransport::new_channel(Some(left_channel)).unwrap();
        let right_transport = StreamTransport::new_channel(Some(right_channel)).unwrap();

        assert_eq!(peer_credentials(&left_transport), right);
        assert_eq!(peer_credentials(&right_transport), left);
    }

    #[test]
    fn stream_connect_snapshots_listen_and_connect_time_credentials() {
        let listen_credentials = UnixCredentials::new(101, 102, 103);
        let connect_credentials = UnixCredentials::new(201, 202, 203);
        let listener = StreamTransport::new().unwrap();
        let slot = super::super::BindSlot::default();
        let address = UnixSocketAddr::Path(Arc::new(alloc::string::String::from(
            "/tmp/axnet-operation-credentials",
        )));
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
    fn stream_send_progress_uses_total_and_preserves_nonblocking_short_write() {
        assert_eq!(finish_stream_send(7, 7, false), Ok(7));
        assert_eq!(finish_stream_send(3, 7, true), Ok(3));
        assert_eq!(finish_stream_send(0, 7, true), Err(AxError::WouldBlock));
        assert_eq!(finish_stream_send(3, 7, false), Err(AxError::WouldBlock));
    }

    #[test]
    fn per_call_nonblocking_stream_send_returns_the_queued_prefix() {
        let credentials = UnixCredentials::new(1, 2, 3);
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
        let credentials = UnixCredentials::new(1, 2, 3);
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
        let credentials = UnixCredentials::new(1, 2, 3);
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
        assert!(right.poll().contains(IoEvents::RDHUP));

        let mut byte = [0u8; 1];
        let mut right_channel = right.channel.lock();
        let rx = right_channel.as_mut().unwrap().rx.as_mut().unwrap();
        assert_eq!(rx.pop_slice(&mut byte), 1);
        assert_eq!(byte, [b'x']);
        assert!(!rx.write_is_held());
    }

    #[test]
    fn stream_read_shutdown_breaks_peer_writes() {
        let credentials = UnixCredentials::new(1, 2, 3);
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
