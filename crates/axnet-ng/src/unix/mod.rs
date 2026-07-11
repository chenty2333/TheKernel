pub(crate) mod dgram;
pub(crate) mod stream;

use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng::{FS_CONTEXT, OpenOptions};
use axfs_ng_vfs::NodeType;
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, Pollable};
use axsync::spin::SpinNoIrq as Mutex;
use axtask::future::{block_on, interruptible};
use enum_dispatch::enum_dispatch;
use hashbrown::HashMap;
use lazy_static::lazy_static;

#[doc(hidden)]
pub use self::dgram::{drain_deferred_receive_cleanup_work, has_deferred_receive_cleanup_work};
pub use self::{dgram::DgramTransport, stream::StreamTransport};
use crate::{
    RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketFilter, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

/// Address for a Unix domain socket.
#[derive(Default, Clone, Debug)]
pub enum UnixSocketAddr {
    /// Unnamed (anonymous) socket.
    #[default]
    Unnamed,
    /// Abstract namespace address.
    Abstract(Arc<[u8]>),
    /// Filesystem path address.
    Path(Arc<str>),
}

/// Abstract transport trait for Unix sockets.
#[async_trait]
#[enum_dispatch]
pub trait TransportOps: Configurable + Pollable + Send + Sync {
    /// Stores a deferred socket error.
    fn set_pending_error(&self, error: LinuxError);
    /// Bind the transport to the given address.
    fn bind(&self, slot: &BindSlot, local_addr: &UnixSocketAddr) -> AxResult;
    /// Connect the transport to a remote address.
    fn connect(&self, slot: &BindSlot, local_addr: &UnixSocketAddr) -> AxResult;
    /// Start listening with a bounded pending-connection queue.
    fn listen(&self, _slot: &BindSlot, _backlog: usize) -> AxResult {
        Err(AxError::OperationNotSupported)
    }

    /// Accept an incoming connection, returning the new transport and peer address.
    async fn accept(&self) -> AxResult<(Transport, UnixSocketAddr)>;

    /// Send data through the transport.
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize>;
    /// Receive data from the transport.
    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> AxResult<usize>;

    /// Shutdown the transport.
    fn shutdown(&self, _how: Shutdown) -> AxResult {
        Err(AxError::OperationNotSupported)
    }
}

/// Unix domain transport type (stream or datagram).
#[enum_dispatch(Configurable, TransportOps)]
pub enum Transport {
    /// Stream-oriented transport.
    Stream(StreamTransport),
    /// Datagram-oriented transport.
    Dgram(DgramTransport),
}
impl Pollable for Transport {
    fn poll(&self) -> IoEvents {
        match self {
            Transport::Stream(stream) => stream.poll(),
            Transport::Dgram(dgram) => dgram.poll(),
        }
    }

    fn register(&self, context: &mut core::task::Context<'_>, events: IoEvents) {
        match self {
            Transport::Stream(stream) => stream.register(context, events),
            Transport::Dgram(dgram) => dgram.register(context, events),
        }
    }
}

/// Holds binding state for stream and datagram transports at a Unix address.
#[derive(Default)]
pub struct BindSlot {
    stream: Mutex<Option<stream::Bind>>,
    dgram: Mutex<Option<dgram::Bind>>,
}

lazy_static! {
    static ref ABSTRACT_BINDS: Mutex<HashMap<Arc<[u8]>, BindSlot>> = Mutex::new(HashMap::new());
}

pub(crate) fn with_slot<R>(
    addr: &UnixSocketAddr,
    f: impl FnOnce(&BindSlot) -> AxResult<R>,
) -> AxResult<R> {
    match addr {
        UnixSocketAddr::Unnamed => Err(AxError::InvalidInput),
        UnixSocketAddr::Abstract(name) => {
            let binds = ABSTRACT_BINDS.lock();
            if let Some(slot) = binds.get(name) {
                f(slot)
            } else {
                Err(AxError::NotFound)
            }
        }
        UnixSocketAddr::Path(path) => {
            let loc = FS_CONTEXT.lock().resolve(path.as_ref())?;
            if loc.metadata()?.node_type != NodeType::Socket {
                return Err(AxError::NotASocket);
            }
            f(loc
                .user_data()
                .get::<BindSlot>()
                .ok_or(AxError::ConnectionRefused)?
                .as_ref())
        }
    }
}
fn with_slot_or_insert<R>(
    addr: &UnixSocketAddr,
    f: impl FnOnce(&BindSlot) -> AxResult<R>,
) -> AxResult<R> {
    match addr {
        UnixSocketAddr::Unnamed => Err(AxError::InvalidInput),
        UnixSocketAddr::Abstract(name) => {
            let mut binds = ABSTRACT_BINDS.lock();
            f(binds.entry(name.clone()).or_default())
        }
        UnixSocketAddr::Path(path) => {
            let loc = OpenOptions::new()
                .write(true)
                .create(true)
                .node_type(NodeType::Socket)
                .open(&FS_CONTEXT.lock(), path.as_ref())?
                .into_location();
            if loc.metadata()?.node_type != NodeType::Socket {
                return Err(AxError::NotASocket);
            }
            f(loc
                .user_data()
                .get_or_insert_with(BindSlot::default)
                .as_ref())
        }
    }
}

/// A Unix domain socket.
pub struct UnixSocket {
    transport: Transport,
    local_addr: Mutex<UnixSocketAddr>,
    remote_addr: Mutex<UnixSocketAddr>,
    owns_bind: AtomicBool,
}
impl UnixSocket {
    /// Create a new Unix socket with the given transport.
    pub fn new(transport: impl Into<Transport>) -> Self {
        Self {
            transport: transport.into(),
            local_addr: Mutex::new(UnixSocketAddr::Unnamed),
            remote_addr: Mutex::new(UnixSocketAddr::Unnamed),
            owns_bind: AtomicBool::new(false),
        }
    }

    pub fn set_filter(&self, filter: Option<Arc<dyn SocketFilter>>) -> AxResult<()> {
        match &self.transport {
            Transport::Stream(stream) => stream.set_filter(filter),
            Transport::Dgram(dgram) => dgram.set_filter(filter),
        }
    }

    pub fn is_connected(&self) -> bool {
        match &self.transport {
            Transport::Stream(stream) => stream.is_connected(),
            Transport::Dgram(dgram) => dgram.is_connected(),
        }
    }

    pub(crate) fn set_pending_error(&self, error: LinuxError) {
        self.transport.set_pending_error(error);
    }
}
impl Configurable for UnixSocket {
    fn nonblocking(&self) -> bool {
        self.transport.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool> {
        self.transport.set_option_inner(opt)
    }
}
impl SocketOps for UnixSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> AxResult {
        let local_addr = local_addr.into_unix()?;
        let mut guard = self.local_addr.lock();
        if matches!(&*guard, UnixSocketAddr::Unnamed) {
            with_slot_or_insert(&local_addr, |slot| self.transport.bind(slot, &local_addr))?;
            *guard = local_addr;
            self.owns_bind.store(true, Ordering::Release);
        } else {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        let remote_addr = remote_addr.into_unix()?;
        let local_addr = self.local_addr.lock().clone();
        let mut guard = self.remote_addr.lock();
        if matches!(&*guard, UnixSocketAddr::Unnamed) {
            with_slot(&remote_addr, |slot| {
                self.transport.connect(slot, &local_addr)
            })?;
            *guard = remote_addr;
        } else {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    fn listen(&self, backlog: usize) -> AxResult {
        let local_addr = self.local_addr.lock().clone();
        with_slot(&local_addr, |slot| self.transport.listen(slot, backlog))
    }

    fn accept(&self) -> AxResult<Socket> {
        let (transport, peer_addr) = block_on(interruptible(self.transport.accept()))??;
        Ok(Socket::Unix(Self {
            transport,
            local_addr: Mutex::new(self.local_addr.lock().clone()),
            remote_addr: Mutex::new(peer_addr),
            owns_bind: AtomicBool::new(false),
        }))
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> AxResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.local_addr.lock().clone()))
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.remote_addr.lock().clone()))
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        self.transport.shutdown(how)
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        if !self.owns_bind.load(Ordering::Acquire) {
            return;
        }
        if let UnixSocketAddr::Abstract(name) = &*self.local_addr.lock() {
            ABSTRACT_BINDS.lock().remove(name);
        }
    }
}

impl Pollable for UnixSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.transport.register(context, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstract_binding_is_released_with_its_owner() {
        let name: Arc<[u8]> = Arc::from(&b"axnet-ng-drop-test"[..]);
        ABSTRACT_BINDS
            .lock()
            .insert(name.clone(), BindSlot::default());

        let socket = UnixSocket {
            transport: DgramTransport::new().unwrap().into(),
            local_addr: Mutex::new(UnixSocketAddr::Abstract(name.clone())),
            remote_addr: Mutex::new(UnixSocketAddr::Unnamed),
            owns_bind: AtomicBool::new(true),
        };
        drop(socket);

        assert!(!ABSTRACT_BINDS.lock().contains_key(&name));
    }
}
