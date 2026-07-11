use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::spin::SpinNoIrq as Mutex;
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
    unix::{Transport, TransportOps, UnixSocketAddr},
};

// Match the default socket send buffer so large splice/socketpair transfers do
// not deadlock behind an unrealistically tiny in-kernel Unix stream queue.
const BUF_SIZE: usize = TCP_TX_BUF_LEN;

fn new_uni_channel() -> (HeapProd<u8>, HeapCons<u8>) {
    let rb = HeapRb::new(BUF_SIZE);
    rb.split()
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
) -> (Channel, Channel) {
    let (client_tx, server_rx) = new_uni_channel();
    let (server_tx, client_rx) = new_uni_channel();
    let poll_update = Arc::new(PollSet::new());
    (
        Channel {
            tx: Some(client_tx),
            rx: Some(client_rx),
            poll_update: poll_update.clone(),
            peer_credentials: right_credentials,
        },
        Channel {
            tx: Some(server_tx),
            rx: Some(server_rx),
            poll_update,
            peer_credentials: left_credentials,
        },
    )
}

struct Channel {
    tx: Option<HeapProd<u8>>,
    rx: Option<HeapCons<u8>>,
    // TODO: granularity
    poll_update: Arc<PollSet>,
    peer_credentials: UnixCredentials,
}

pub struct Bind {
    /// New connections are sent to this channel.
    conn_tx: async_channel::Sender<ConnRequest>,
    poll_new_conn: Arc<PollSet>,
    credentials: UnixCredentials,
    backlog: usize,
}
impl Bind {
    fn connect(
        &self,
        local_addr: UnixSocketAddr,
        credentials: UnixCredentials,
    ) -> AxResult<Channel> {
        if self.backlog == 0 || self.conn_tx.len() >= self.backlog {
            return Err(AxError::ConnectionRefused);
        }
        let (client_chan, server_chan) = new_channels(credentials, self.credentials);
        self.conn_tx
            .try_send(ConnRequest {
                channel: server_chan,
                addr: local_addr,
            })
            .map_err(|_| AxError::ConnectionRefused)?;
        self.poll_new_conn.wake();
        Ok(client_chan)
    }
}

struct ConnRequest {
    channel: Channel,
    addr: UnixSocketAddr,
}

/// Stream transport for Unix domain sockets.
pub struct StreamTransport {
    channel: Mutex<Option<Channel>>,
    conn_rx: Mutex<Option<(async_channel::Receiver<ConnRequest>, Arc<PollSet>)>>,
    poll_state: PollSet,
    general: GeneralOptions,
    credentials: UnixCredentials,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
}
impl StreamTransport {
    /// Create a new unconnected stream transport.
    pub fn new(credentials: UnixCredentials) -> Self {
        StreamTransport::new_channel(None, credentials)
    }

    fn new_channel(channel: Option<Channel>, credentials: UnixCredentials) -> Self {
        StreamTransport {
            channel: Mutex::new(channel),
            conn_rx: Mutex::new(None),
            poll_state: PollSet::new(),
            general: GeneralOptions::default(),
            credentials,
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
        }
    }

    pub fn set_filter(&self, _filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        Err(AxError::Unsupported)
    }

    pub fn is_connected(&self) -> bool {
        self.channel.lock().is_some()
    }

    /// Create a connected pair of stream transports.
    pub fn new_pair(credentials: UnixCredentials) -> (Self, Self) {
        let (chan1, chan2) = new_channels(credentials, credentials);
        let transport1 = StreamTransport::new_channel(Some(chan1), credentials);
        let transport2 = StreamTransport::new_channel(Some(chan2), credentials);
        (transport1, transport2)
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
        let mut slot = slot.stream.lock();
        if slot.is_some() {
            return Err(AxError::AddrInUse);
        }
        let mut guard = self.conn_rx.lock();
        if guard.is_some() {
            return Err(AxError::InvalidInput);
        }
        let (tx, rx) = async_channel::bounded(LISTEN_QUEUE_SIZE);
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            conn_tx: tx,
            poll_new_conn: poll.clone(),
            credentials: self.credentials,
            backlog: 0,
        });
        *guard = Some((rx, poll));
        self.poll_state.wake();
        Ok(())
    }

    fn listen(&self, slot: &super::BindSlot, backlog: usize) -> AxResult<()> {
        if self.conn_rx.lock().is_none() {
            return Err(AxError::InvalidInput);
        }
        let mut slot = slot.stream.lock();
        let bind = slot.as_mut().ok_or(AxError::InvalidInput)?;
        bind.backlog = backlog.clamp(1, LISTEN_QUEUE_SIZE);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(&self, slot: &super::BindSlot, local_addr: &UnixSocketAddr) -> AxResult<()> {
        let mut guard = self.channel.lock();
        if guard.is_some() {
            return Err(AxError::AlreadyConnected);
        }
        *guard = Some(
            slot.stream
                .lock()
                .as_ref()
                .ok_or(AxError::NotConnected)?
                .connect(local_addr.clone(), self.credentials)?,
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> AxResult<(Transport, UnixSocketAddr)> {
        let Some((rx, _)) = self.conn_rx.lock().clone() else {
            return Err(AxError::NotConnected);
        };
        let ConnRequest {
            channel,
            addr: peer_addr,
        } = rx.recv().await.map_err(|_| AxError::ConnectionReset)?;
        Ok((
            Transport::Stream(StreamTransport::new_channel(
                Some(channel),
                self.credentials,
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
                let Some(tx) = chan.tx.as_mut() else {
                    return Err(AxError::BrokenPipe);
                };
                if !tx.read_is_held() {
                    return Err(AxError::BrokenPipe);
                }

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
                if count > 0 {
                    chan.poll_update.wake();
                }

                finish_stream_send(total, size, effective_nonblocking)
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
                if count > 0 {
                    chan.poll_update.wake();
                    Ok(count)
                } else if !rx.write_is_held() {
                    Ok(0)
                } else {
                    Err(AxError::WouldBlock)
                }
            })
    }

    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        let mut channel = self.channel.lock();
        let channel = channel.as_mut().ok_or(AxError::NotConnected)?;
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
            channel.rx.take();
        }
        if how.has_write() {
            self.tx_closed.store(true, Ordering::Release);
            channel.tx.take();
        }
        channel.poll_update.wake();
        self.poll_state.wake();
        Ok(())
    }
}

impl Pollable for StreamTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some(chan) = self.channel.lock().as_ref() {
            let peer_write_closed = chan.rx.as_ref().is_some_and(|rx| !rx.write_is_held());
            let peer_read_closed = chan.tx.as_ref().is_some_and(|tx| !tx.read_is_held());
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
        } else if let Some((conn_tx, _)) = self.conn_rx.lock().as_ref() {
            events.set(IoEvents::IN, !conn_tx.is_empty());
        }
        self.general.add_pending_error_event(events)
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if let Some(chan) = self.channel.lock().as_ref() {
            if events.intersects(IoEvents::IN | IoEvents::OUT) {
                chan.poll_update.register(context.waker());
            }
        } else if let Some((_, poll_new_conn)) = self.conn_rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            poll_new_conn.register(context.waker());
        }
        self.poll_state.register(context.waker());
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.channel.lock().take() {
            let poll_update = chan.poll_update.clone();
            drop(chan);
            poll_update.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_credentials(transport: &StreamTransport) -> UnixCredentials {
        let mut credentials = UnixCredentials::default();
        transport
            .get_option(GetSocketOption::PeerCredentials(&mut credentials))
            .unwrap();
        credentials
    }

    #[test]
    fn unconnected_stream_reports_unknown_peer_credentials() {
        let transport = StreamTransport::new(UnixCredentials::new(1, 2, 3));
        assert_eq!(peer_credentials(&transport), UnixCredentials::UNKNOWN);
    }

    #[test]
    fn connected_streams_snapshot_the_opposite_peer_credentials() {
        let left = UnixCredentials::new(11, 12, 13);
        let right = UnixCredentials::new(21, 22, 23);
        let (left_channel, right_channel) = new_channels(left, right);
        let left_transport = StreamTransport::new_channel(Some(left_channel), left);
        let right_transport = StreamTransport::new_channel(Some(right_channel), right);

        assert_eq!(peer_credentials(&left_transport), right);
        assert_eq!(peer_credentials(&right_transport), left);
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
        let (left, right) = StreamTransport::new_pair(credentials);
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
        let (tx, rx) = async_channel::bounded(LISTEN_QUEUE_SIZE);
        let mut bind = Bind {
            conn_tx: tx,
            poll_new_conn: Arc::new(PollSet::new()),
            credentials,
            backlog: 0,
        };

        assert_eq!(
            bind.connect(UnixSocketAddr::Unnamed, credentials)
                .err()
                .unwrap(),
            AxError::ConnectionRefused
        );

        bind.backlog = 1;
        let _client = bind.connect(UnixSocketAddr::Unnamed, credentials).unwrap();
        assert_eq!(
            bind.connect(UnixSocketAddr::Unnamed, credentials)
                .err()
                .unwrap(),
            AxError::ConnectionRefused
        );
        drop(rx.try_recv().unwrap());
        assert!(bind.connect(UnixSocketAddr::Unnamed, credentials).is_ok());
    }

    #[test]
    fn stream_write_shutdown_preserves_queued_data_and_closes_peer_writer() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let (left, right) = StreamTransport::new_pair(credentials);

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
        let (left, right) = StreamTransport::new_pair(credentials);

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
