//! Connection-oriented Unix `SOCK_SEQPACKET` transport.
//!
//! This deliberately owns a listener lifecycle distinct from both stream and
//! datagram sockets.  Its connected data plane is a record queue, so one send
//! is one receive record (including its SCM payload), while its bind/connect/
//! accept state follows the connection-oriented Unix model.

use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axio::{IoBuf, Read, Write};
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;

use crate::{
    RecvOptions, SendOptions, Shutdown, SocketFilter,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketCredentials, SocketFault},
    unix::{
        BindSlot, Transport, TransportOps, UnixSocketAddr,
        dgram::DgramTransport,
        queue::{PermitSendError, Receiver, RecvReservation, SendPermit, Sender, try_bounded},
    },
};

const SEQPACKET_LISTEN_QUEUE: usize = 128;

struct Listener {
    tx: Sender<ConnRequest>,
    credentials: Mutex<SocketCredentials>,
    backlog: AtomicUsize,
    passcred: Arc<core::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub struct Bind(Arc<Listener>);

impl Bind {
    fn new(
        tx: Sender<ConnRequest>,
        passcred: Arc<core::sync::atomic::AtomicBool>,
    ) -> AxResult<Self> {
        Arc::try_new(Listener {
            tx,
            credentials: Mutex::new(SocketCredentials::UNKNOWN),
            backlog: AtomicUsize::new(0),
            passcred,
        })
        .map(Self)
        .map_err(|_| AxError::NoMemory)
    }
    fn reserve(&self) -> AxResult<SendPermit<ConnRequest>> {
        self.0
            .tx
            .try_reserve(self.0.backlog.load(Ordering::Acquire))
            .map_err(|_| AxError::ConnectionRefused)
    }
    pub(super) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<()>() as usize
    }
    fn listen(&self, backlog: usize, credentials: SocketCredentials) -> AxResult<()> {
        if self.0.tx.is_closed() {
            return Err(AxError::InvalidInput);
        }
        *self.0.credentials.lock() = credentials;
        self.0
            .backlog
            .store(backlog.clamp(1, SEQPACKET_LISTEN_QUEUE), Ordering::Release);
        Ok(())
    }
}

struct ConnRequest {
    transport: DgramTransport,
    address: UnixSocketAddr,
}
impl ConnRequest {
    fn identity(&self) -> usize {
        self.transport.identity()
    }
}

pub(super) struct SeqPacketConnectReservation<'a> {
    transport: &'a SeqPacketTransport,
    listener: Bind,
    permit: Option<SendPermit<ConnRequest>>,
    client: Option<DgramTransport>,
    request: Option<ConnRequest>,
    committed: bool,
}
impl SeqPacketConnectReservation<'_> {
    pub(super) fn listener_identity(&self) -> usize {
        self.listener.identity()
    }
    pub(super) fn accepted_identity(&self) -> usize {
        self.request
            .as_ref()
            .expect("active seqpacket connect reservation")
            .identity()
    }
    pub(super) fn commit(mut self) -> AxResult<()> {
        let permit = self.permit.take().expect("active seqpacket queue permit");
        let request = self.request.take().expect("active seqpacket request");
        if let Err(PermitSendError::Closed(request)) = permit.send(request) {
            drop(request);
            drop(self.client.take());
            return Err(AxError::ConnectionRefused);
        }
        let client = self.client.take().expect("active seqpacket client");
        client.set_option(SetSocketOption::NonBlocking(
            &self.transport.nonblocking.load(Ordering::Acquire),
        ))?;
        client.set_option(SetSocketOption::PassCredentials(
            &self.transport.passcred.load(Ordering::Acquire),
        ))?;
        *self.transport.data.lock() = Some(client);
        self.transport.connected.store(true, Ordering::Release);
        self.transport.poll.wake();
        self.committed = true;
        Ok(())
    }
}
impl Drop for SeqPacketConnectReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.transport.connected.store(false, Ordering::Release);
        }
    }
}

pub(super) struct SeqPacketAcceptReservation {
    request: Option<RecvReservation<ConnRequest>>,
}
pub(super) struct PreparedSeqPacketAccept {
    receiver: Receiver<ConnRequest>,
}
impl PreparedSeqPacketAccept {
    pub(super) async fn wait(&mut self) -> AxResult<SeqPacketAcceptReservation> {
        Ok(SeqPacketAcceptReservation {
            request: Some(self.receiver.reserve().await?),
        })
    }
}
impl SeqPacketAcceptReservation {
    pub(super) fn accepted_identity(&self) -> usize {
        self.request
            .as_ref()
            .expect("active seqpacket accept reservation")
            .item()
            .identity()
    }
    pub(super) fn commit(mut self) -> AxResult<(Transport, UnixSocketAddr)> {
        let request = self
            .request
            .take()
            .expect("active seqpacket accept reservation")
            .commit()
            .map_err(|request| {
                drop(request);
                AxError::ConnectionReset
            })?;
        Ok((
            Transport::SeqPacket(SeqPacketTransport::from_connected(request.transport)?),
            request.address,
        ))
    }
}
impl Drop for SeqPacketAcceptReservation {
    fn drop(&mut self) {
        if let Some(reservation) = self.request.take() {
            drop(reservation.cancel());
        }
    }
}

/// A separate connection-oriented Unix transport with datagram record queues.
pub struct SeqPacketTransport {
    data: Mutex<Option<DgramTransport>>,
    listener: Mutex<Option<Receiver<ConnRequest>>>,
    poll: PollSet,
    connected: core::sync::atomic::AtomicBool,
    nonblocking: core::sync::atomic::AtomicBool,
    passcred: Arc<core::sync::atomic::AtomicBool>,
}
impl SeqPacketTransport {
    pub fn endpoint_identity(&self) -> Option<usize> {
        self.data.lock().as_ref().map(DgramTransport::identity)
    }
    pub fn new() -> AxResult<Self> {
        Ok(Self {
            data: Mutex::new(None),
            listener: Mutex::new(None),
            poll: PollSet::new(),
            connected: core::sync::atomic::AtomicBool::new(false),
            nonblocking: core::sync::atomic::AtomicBool::new(false),
            passcred: Arc::try_new(core::sync::atomic::AtomicBool::new(false))
                .map_err(|_| AxError::NoMemory)?,
        })
    }
    fn from_connected(data: DgramTransport) -> AxResult<Self> {
        let passcred = data.passcred_enabled();
        Ok(Self {
            data: Mutex::new(Some(data)),
            listener: Mutex::new(None),
            poll: PollSet::new(),
            connected: core::sync::atomic::AtomicBool::new(true),
            nonblocking: core::sync::atomic::AtomicBool::new(false),
            passcred: Arc::try_new(core::sync::atomic::AtomicBool::new(passcred))
                .map_err(|_| AxError::NoMemory)?,
        })
    }
    pub fn new_pair(credentials: SocketCredentials) -> AxResult<(Self, Self)> {
        let (left, right) = DgramTransport::new_record_pair(credentials, credentials)?;
        Ok((Self::from_connected(left)?, Self::from_connected(right)?))
    }
    pub(super) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .retry_transfer(direction, nonblocking, attempt)
    }
    pub(super) fn recv_pending_len(&self) -> AxResult<usize> {
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .recv_pending_len()
    }
    pub(super) fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .set_filter(filter)
    }
    pub(super) fn set_record_sender_address(&self, address: UnixSocketAddr) {
        if let Some(data) = self.data.lock().as_ref() {
            data.set_record_sender_address(address);
        }
    }
    pub(super) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    pub(super) fn prepare_connect<'a>(
        &'a self,
        slot: &BindSlot,
        local: &UnixSocketAddr,
        credentials: SocketCredentials,
    ) -> AxResult<SeqPacketConnectReservation<'a>> {
        if self
            .connected
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AxError::AlreadyConnected);
        }
        let result = (|| {
            let bind = slot.seqpacket.lock().as_ref().cloned().ok_or_else(|| {
                if slot.stream.lock().is_some() || slot.dgram.lock().is_some() {
                    AxError::OperationNotSupported
                } else {
                    AxError::ConnectionRefused
                }
            })?;
            let permit = bind.reserve()?;
            let peer = *bind.0.credentials.lock();
            let (client, server) = DgramTransport::new_record_pair(credentials, peer)?;
            client.set_record_sender_address(local.clone());
            server.set_option(SetSocketOption::PassCredentials(
                &bind.0.passcred.load(Ordering::Acquire),
            ))?;
            Ok(SeqPacketConnectReservation {
                transport: self,
                listener: bind,
                permit: Some(permit),
                client: Some(client),
                request: Some(ConnRequest {
                    transport: server,
                    address: local.clone(),
                }),
                committed: false,
            })
        })();
        if result.is_err() {
            self.connected.store(false, Ordering::Release);
        }
        result
    }
    pub(super) fn prepare_accept(&self) -> AxResult<PreparedSeqPacketAccept> {
        Ok(PreparedSeqPacketAccept {
            receiver: self
                .listener
                .lock()
                .as_ref()
                .cloned()
                .ok_or(AxError::NotConnected)?,
        })
    }
    // `data` is installed exactly once before `connected` becomes true and is
    // never replaced afterwards.  A borrowed `&self` prevents Drop, so this
    // stable heap address can safely back a poll registration after releasing
    // the short state lock.
    fn connected_data(&self) -> Option<&DgramTransport> {
        let pointer = self
            .data
            .lock()
            .as_ref()
            .map(|data| data as *const DgramTransport)?;
        // SAFETY: data is installed once and never replaced; the &self borrow
        // keeps its containing transport alive and immovable for this reference.
        Some(unsafe { &*pointer })
    }
    pub(super) fn rollback_bind(&self, slot: &BindSlot) {
        let bind = slot.seqpacket.lock().take();
        let receiver = self.listener.lock().take();
        if let Some(receiver) = receiver.as_ref() {
            receiver.close();
            while let Ok((request, completion)) = receiver.try_recv_deferred_wake() {
                drop(request);
                completion.complete();
            }
        }
        drop(bind);
        drop(receiver);
        self.poll.wake();
    }
}

impl Configurable for SeqPacketTransport {
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn get_option_inner(&self, option: &mut GetSocketOption) -> AxResult<bool> {
        if let GetSocketOption::PassCredentials(enabled) = option {
            **enabled = self.passcred.load(Ordering::Acquire);
            return Ok(true);
        }
        if let Some(data) = self.data.lock().as_ref() {
            return data.get_option_inner(option);
        }
        if let GetSocketOption::PeerCredentials(credentials) = option {
            **credentials = SocketCredentials::UNKNOWN;
            return Ok(true);
        }
        Ok(false)
    }
    fn set_option_inner(&self, option: SetSocketOption) -> AxResult<bool> {
        if let SetSocketOption::NonBlocking(value) = option {
            self.nonblocking.store(*value, Ordering::Release);
            if let Some(data) = self.data.lock().as_ref() {
                data.set_option_inner(option)?;
            }
            return Ok(true);
        }
        if let SetSocketOption::PassCredentials(value) = option {
            self.passcred.store(*value, Ordering::Release);
            if let Some(data) = self.data.lock().as_ref() {
                data.set_option_inner(option)?;
            }
            return Ok(true);
        }
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .set_option_inner(option)
    }
}
impl TransportOps for SeqPacketTransport {
    fn set_pending_error(&self, error: SocketFault) {
        if let Some(data) = self.data.lock().as_ref() {
            data.set_pending_error(error);
        }
    }
    fn bind(&self, slot: &BindSlot, _: &UnixSocketAddr) -> AxResult<()> {
        let (tx, rx) = try_bounded(SEQPACKET_LISTEN_QUEUE)?;
        let bind = Bind::new(tx, self.passcred.clone())?;
        let mut target = slot.seqpacket.lock();
        if target.is_some() {
            return Err(AxError::AddrInUse);
        }
        if self.listener.lock().is_some() {
            return Err(AxError::InvalidInput);
        }
        *target = Some(bind);
        *self.listener.lock() = Some(rx);
        self.poll.wake();
        Ok(())
    }
    fn listen(
        &self,
        slot: &BindSlot,
        backlog: usize,
        credentials: SocketCredentials,
    ) -> AxResult<()> {
        let bind = slot
            .seqpacket
            .lock()
            .as_ref()
            .cloned()
            .ok_or(AxError::InvalidInput)?;
        bind.listen(backlog, credentials)?;
        self.poll.wake();
        Ok(())
    }
    fn connect(
        &self,
        slot: &BindSlot,
        local: &UnixSocketAddr,
        credentials: SocketCredentials,
    ) -> AxResult<()> {
        self.prepare_connect(slot, local, credentials)?.commit()
    }
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if options.to.is_some() {
            return Err(AxError::InvalidInput);
        }
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .send(src, options)
            .map_err(|error| {
                if error == AxError::ConnectionRefused {
                    AxError::BrokenPipe
                } else {
                    error
                }
            })
    }
    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> AxResult<usize> {
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .recv(dst, options)
    }
    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        self.data
            .lock()
            .as_ref()
            .ok_or(AxError::NotConnected)?
            .shutdown(how)
    }
}
impl Pollable for SeqPacketTransport {
    fn poll(&self) -> IoEvents {
        let mut events = self
            .connected_data()
            .map_or(IoEvents::empty(), Pollable::poll);

        if self
            .listener
            .lock()
            .as_ref()
            .is_some_and(|rx| !rx.is_empty())
        {
            events |= IoEvents::READABLE;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let listener = self
            .listener
            .lock()
            .as_ref()
            .map(Receiver::read_poll_source);
        let mut prepared = PreparedPollRegistration::try_new(2 + usize::from(listener.is_some()))?;
        if let Some(data) = self.connected_data() {
            // Peer close is observable even for a caller requesting only HUP.
            prepared.arm_nested(|| data.register(context, events | IoEvents::READABLE))?;
        }
        if let Some(listener) = listener {
            prepared.arm_owned(listener, context.waker())?;
        }
        prepared.arm(&self.poll, context.waker())?;
        prepared.commit()
    }
}
impl Drop for SeqPacketTransport {
    fn drop(&mut self) {
        if let Some(receiver) = self.listener.get_mut().take() {
            receiver.close();
            while let Ok((request, completion)) = receiver.try_recv_deferred_wake() {
                drop(request);
                completion.complete();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_shutdown_delivers_queued_records_then_eof_and_preserves_reverse_send() {
        let (client, server) =
            SeqPacketTransport::new_pair(SocketCredentials::new(1, 0, 0)).unwrap();
        client.send(&b"one"[..], SendOptions::default()).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(client.poll(), IoEvents::WRITABLE);
        assert_eq!(
            server.poll(),
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::READ_HANGUP
        );
        assert_eq!(
            client.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        let mut buffer = [0u8; 3];
        assert_eq!(
            server
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            3
        );
        assert_eq!(&buffer, b"one");
        assert_eq!(
            server
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            0
        );
        server.send(&b"two"[..], SendOptions::default()).unwrap();
        assert_eq!(
            client
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            3
        );
        assert_eq!(&buffer, b"two");
        server.shutdown(Shutdown::Write).unwrap();
        for endpoint in [&client, &server] {
            assert_eq!(
                endpoint.poll(),
                IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::READ_HANGUP | IoEvents::HANGUP
            );
        }
    }

    #[test]
    fn read_shutdown_preserves_local_send_and_full_shutdown_reports_hangup() {
        let (client, server) =
            SeqPacketTransport::new_pair(SocketCredentials::new(1, 0, 0)).unwrap();
        server.send(&b"in"[..], SendOptions::default()).unwrap();
        client.shutdown(Shutdown::Read).unwrap();
        assert_eq!(
            client.poll(),
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::READ_HANGUP
        );
        assert_eq!(server.poll(), IoEvents::WRITABLE);
        assert_eq!(
            server.send(&b"x"[..], SendOptions::default()),
            Err(AxError::BrokenPipe)
        );
        let mut buffer = [0u8; 2];
        assert_eq!(
            client
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            2
        );
        assert_eq!(&buffer, b"in");
        assert_eq!(
            client
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            0
        );
        client.send(&b"ok"[..], SendOptions::default()).unwrap();
        assert_eq!(
            server
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            2
        );
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            server.poll(),
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::READ_HANGUP | IoEvents::HANGUP
        );
    }

    #[test]
    fn write_shutdown_wakes_rdhup_only_registration() {
        let (client, server) =
            SeqPacketTransport::new_pair(SocketCredentials::new(1, 0, 0)).unwrap();
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = core::task::Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let _registration = server
            .register(&mut context, IoEvents::READ_HANGUP)
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert!(wake.0.load(Ordering::Relaxed) > 0);
        assert!(server.poll().contains(IoEvents::READ_HANGUP));
        assert!(!server.poll().contains(IoEvents::HANGUP));
    }

    struct CountWake(AtomicUsize);
    impl alloc::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn peer_close_wakes_hangup_only_registration() {
        let (client, server) =
            SeqPacketTransport::new_pair(SocketCredentials::new(1, 0, 0)).unwrap();
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = core::task::Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let _registration = client.register(&mut context, IoEvents::HANGUP).unwrap();
        drop(server);
        while super::super::dgram::has_deferred_receive_cleanup_work() {
            super::super::dgram::drain_deferred_receive_cleanup_work();
        }
        assert!(wake.0.load(Ordering::Relaxed) > 0);
        assert!(client.poll().contains(IoEvents::HANGUP));
    }

    #[test]
    fn peer_close_is_readable_after_last_record_is_consumed() {
        let (client, server) =
            SeqPacketTransport::new_pair(SocketCredentials::new(1, 0, 0)).unwrap();
        server.send(&b"reply"[..], SendOptions::default()).unwrap();
        drop(server);
        while super::super::dgram::has_deferred_receive_cleanup_work() {
            super::super::dgram::drain_deferred_receive_cleanup_work();
        }
        let mut buffer = [0u8; 5];
        assert_eq!(
            client
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            5
        );
        assert_eq!(&buffer, b"reply");
        assert!(
            client
                .poll()
                .contains(IoEvents::READABLE | IoEvents::READ_HANGUP | IoEvents::HANGUP)
        );
        assert_eq!(
            client
                .recv(&mut buffer[..], RecvOptions::default())
                .unwrap(),
            0
        );
    }
}
