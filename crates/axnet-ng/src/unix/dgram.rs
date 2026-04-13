use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
};

use async_channel::TryRecvError;
use async_trait::async_trait;
use axerrno::{AxError, AxResult};
use axio::{Read, Write};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex;
use spin::RwLock;

use crate::{
    CMsgData, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
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

struct Channel {
    data_tx: async_channel::Sender<Packet>,
    poll_update: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    peer_general: Arc<GeneralOptions>,
}

impl Channel {
    fn capacity(&self, local_general: &GeneralOptions) -> usize {
        local_general
            .send_buffer()
            .min(self.peer_general.recv_buffer())
            .max(1)
    }

    fn writable(&self, local_general: &GeneralOptions) -> bool {
        !self.data_tx.is_closed()
            && self.queued_bytes.load(Ordering::Acquire) < self.capacity(local_general)
    }

    fn try_send(&self, local_general: &GeneralOptions, mut packet: Packet) -> AxResult<()> {
        let capacity = self.capacity(local_general);
        let charge = packet.data.len().max(1).min(capacity);

        loop {
            let queued = self.queued_bytes.load(Ordering::Acquire);
            if queued.saturating_add(charge) > capacity {
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

        packet.charge = charge;
        if let Err(err) = self.data_tx.try_send(packet) {
            self.queued_bytes.fetch_sub(charge, Ordering::AcqRel);
            return Err(match err {
                async_channel::TrySendError::Full(_) => AxError::WouldBlock,
                async_channel::TrySendError::Closed(_) => AxError::BrokenPipe,
            });
        }

        self.poll_update.wake();
        Ok(())
    }
}

pub struct Bind {
    data_tx: async_channel::Sender<Packet>,
    poll_update: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    queued_bytes: Arc<AtomicUsize>,
    general: Arc<GeneralOptions>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.data_tx.clone();
        Channel {
            data_tx: tx,
            poll_update: self.poll_update.clone(),
            filter: self.filter.clone(),
            queued_bytes: self.queued_bytes.clone(),
            peer_general: self.general.clone(),
        }
    }
}

/// Datagram transport for Unix domain sockets.
pub struct DgramTransport {
    data_rx: Mutex<
        Option<(
            async_channel::Receiver<Packet>,
            Arc<PollSet>,
            Arc<AtomicUsize>,
        )>,
    >,
    connected: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixSocketAddr>,
    poll_state: Arc<PollSet>,
    filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
    general: Arc<GeneralOptions>,
    pid: u32,
}
impl DgramTransport {
    /// Create a new unconnected datagram transport.
    pub fn new(pid: u32) -> Self {
        DgramTransport {
            data_rx: Mutex::new(None),
            connected: RwLock::new(None),
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: Arc::default(),
            filter: Arc::new(RwLock::new(None)),
            general: Arc::new(GeneralOptions::default()),
            pid,
        }
    }

    fn new_connected(
        data_rx: (
            async_channel::Receiver<Packet>,
            Arc<PollSet>,
            Arc<AtomicUsize>,
        ),
        connected: Channel,
        filter: Arc<RwLock<Option<Arc<dyn SocketFilter>>>>,
        general: Arc<GeneralOptions>,
        pid: u32,
    ) -> Self {
        DgramTransport {
            data_rx: Mutex::new(Some(data_rx)),
            connected: RwLock::new(Some(connected)),
            local_addr: RwLock::new(UnixSocketAddr::Unnamed),
            poll_state: Arc::default(),
            filter,
            general,
            pid,
        }
    }

    /// Create a connected pair of datagram transports.
    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let queued1 = Arc::new(AtomicUsize::new(0));
        let queued2 = Arc::new(AtomicUsize::new(0));
        let filter1 = Arc::new(RwLock::new(None));
        let filter2 = Arc::new(RwLock::new(None));
        let general1 = Arc::new(GeneralOptions::default());
        let general2 = Arc::new(GeneralOptions::default());
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone(), queued1.clone()),
            Channel {
                data_tx: tx2,
                poll_update: poll2.clone(),
                filter: filter2.clone(),
                queued_bytes: queued2.clone(),
                peer_general: general2.clone(),
            },
            filter1.clone(),
            general1.clone(),
            pid,
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone(), queued2),
            Channel {
                data_tx: tx1,
                poll_update: poll1.clone(),
                filter: filter1.clone(),
                queued_bytes: queued1,
                peer_general: general1,
            },
            filter2.clone(),
            general2,
            pid,
        );
        (transport1, transport2)
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
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(opt)? {
            return Ok(true);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                // Datagram sockets are stateless and do not have a peer, so we
                // return the credentials of the process that created the
                // socket.
                **cred = UnixCredentials::new(self.pid);
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
            O::PassCredentials(_) => {}
            _ => return Ok(false),
        }
        Ok(true)
    }
}
#[async_trait]
impl TransportOps for DgramTransport {
    fn bind(&self, slot: &super::BindSlot, local_addr: &UnixSocketAddr) -> AxResult {
        let mut slot = slot.dgram.lock();
        if slot.is_some() {
            return Err(AxError::AddrInUse);
        }
        let mut guard = self.data_rx.lock();
        if guard.is_some() {
            return Err(AxError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll_update = Arc::new(PollSet::new());
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        *slot = Some(Bind {
            data_tx: tx,
            poll_update: poll_update.clone(),
            filter: self.filter.clone(),
            queued_bytes: queued_bytes.clone(),
            general: self.general.clone(),
        });
        *guard = Some((rx, poll_update, queued_bytes));
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

    fn send(&self, mut src: impl Read, options: SendOptions) -> AxResult<usize> {
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        let mut packet = Packet {
            data: message,
            cmsg: options.cmsg,
            sender: self.local_addr.read().clone(),
            charge: 0,
        };

        let connected = self.connected.read();
        if let Some(addr) = options.to {
            let addr = addr.into_unix()?;
            with_slot(&addr, |slot| {
                if let Some(bind) = slot.dgram.lock().as_ref() {
                    if let Some(filter) = bind.filter.read().as_ref() {
                        let keep = filter.filter(&mut packet.data)?;
                        if keep == 0 {
                            return Ok(());
                        }
                        packet.data.truncate(keep.min(packet.data.len()));
                    }
                    bind.connect().try_send(&self.general, packet)?;
                    Ok(())
                } else {
                    Err(AxError::NotConnected)
                }
            })?;
        } else if let Some(chan) = connected.as_ref() {
            if let Some(filter) = chan.filter.read().as_ref() {
                let keep = filter.filter(&mut packet.data)?;
                if keep == 0 {
                    return Ok(len);
                }
                packet.data.truncate(keep.min(packet.data.len()));
            }
            chan.try_send(&self.general, packet)?;
        } else {
            return Err(AxError::NotConnected);
        }
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> AxResult<usize> {
        self.general.recv_poller(self, move || {
            let mut guard = self.data_rx.lock();
            let Some((rx, poll_update, queued_bytes)) = guard.as_mut() else {
                return Err(AxError::NotConnected);
            };

            let Packet {
                data,
                cmsg,
                sender,
                charge,
            } = match rx.try_recv() {
                Ok(packet) => packet,
                Err(TryRecvError::Empty) => {
                    return Err(AxError::WouldBlock);
                }
                Err(TryRecvError::Closed) => {
                    return Ok(0);
                }
            };
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
                dst.extend(cmsg);
            }

            Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                data.len()
            } else {
                count
            })
        })
    }
}

impl Pollable for DgramTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some((rx, ..)) = self.data_rx.lock().as_ref() {
            events.set(IoEvents::IN, !rx.is_empty());
        }
        events.set(
            IoEvents::OUT,
            self.connected
                .read()
                .as_ref()
                .is_none_or(|chan| chan.writable(&self.general)),
        );
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if let Some((_, poll, _)) = self.data_rx.lock().as_ref() {
            if events.contains(IoEvents::IN) {
                poll.register(context.waker());
            }
        }
        if let Some(chan) = self.connected.read().as_ref()
            && events.contains(IoEvents::OUT)
        {
            chan.poll_update.register(context.waker());
        }
        self.poll_state.register(context.waker());
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.connected.write().take() {
            chan.poll_update.wake();
        }
    }
}
