pub(crate) mod dgram;
mod queue;
pub(crate) mod stream;

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    any::Any,
    ptr,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    task::Context,
};

use async_trait::async_trait;
use axerrno::{AxError, AxResult, LinuxError};
use axio::{IoBuf, Read, Write};
use axpoll::{IoEvents, Pollable};
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::future::{block_on, interruptible};
use enum_dispatch::enum_dispatch;
use hashbrown::HashMap;

pub use self::{dgram::DgramTransport, stream::StreamTransport};
use crate::{
    RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketFilter, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
};

/// Address for a Unix domain socket.
#[derive(Default, Clone, Debug)]
pub enum UnixSocketAddr {
    /// Unnamed (anonymous) socket.
    #[default]
    Unnamed,
    /// Abstract namespace address.
    Abstract(Arc<Vec<u8>>),
    /// Filesystem path address.
    Path(Arc<String>),
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
    fn connect(
        &self,
        slot: &BindSlot,
        local_addr: &UnixSocketAddr,
        credentials: UnixCredentials,
    ) -> AxResult;
    /// Start listening with a bounded pending-connection queue.
    fn listen(&self, _slot: &BindSlot, _backlog: usize, _credentials: UnixCredentials) -> AxResult {
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

impl Transport {
    fn rollback_bind(&self, slot: &BindSlot) {
        match self {
            Self::Stream(stream) => stream.rollback_bind(slot),
            Self::Dgram(dgram) => dgram.rollback_bind(slot),
        }
    }
}

/// Holds binding state for stream and datagram transports at a Unix address.
#[derive(Default)]
pub struct BindSlot {
    stream: Mutex<Option<stream::Bind>>,
    dgram: Mutex<Option<dgram::Bind>>,
    address: Mutex<Option<UnixSocketAddr>>,
    active: AtomicBool,
}

impl BindSlot {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn claim_address(&self, address: &UnixSocketAddr) -> AxResult {
        let mut bound = self.address.lock();
        if bound.is_some() {
            return Err(AxError::AddrInUse);
        }
        *bound = Some(address.clone());
        Ok(())
    }

    fn bound_address(&self) -> AxResult<UnixSocketAddr> {
        self.address
            .lock()
            .clone()
            .ok_or(AxError::ConnectionRefused)
    }

    fn clear_address(&self) {
        let retired = self.address.lock().take();
        drop(retired);
    }

    fn retire(&self) {
        // A pathname inode may retain this slot long after its owning socket
        // closes.  Remove transport payloads in the task-context finalizer so
        // that the inode is left with only inert state: the final inode/slot
        // release must not become the place where channel mutexes, queued
        // SCM_RIGHTS records, or registered wakers are destroyed.
        let stream = self.stream.lock().take();
        let dgram = self.dgram.lock().take();
        let address = self.address.lock().take();
        drop(stream);
        drop(dgram);
        drop(address);
    }
}

/// AF_UNIX abstract-name registry owned by one Linux network namespace.
///
/// Pathname sockets deliberately do not use this object: their visibility is
/// determined by VFS/mount namespaces. Keeping the abstract map here prevents
/// same-name conflicts and connectivity across independent `CLONE_NEWNET`
/// stacks.
pub struct UnixNamespace {
    abstract_binds: Mutex<HashMap<Arc<Vec<u8>>, Arc<BindSlot>>>,
}

impl UnixNamespace {
    /// Allocates an empty namespace without publishing any global identity.
    pub fn try_new() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            abstract_binds: Mutex::new(HashMap::new()),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn abstract_slot(&self, name: &Arc<Vec<u8>>) -> AxResult<Arc<BindSlot>> {
        let slot = self
            .abstract_binds
            .lock()
            .get(name)
            .cloned()
            .ok_or(AxError::ConnectionRefused)?;
        if slot.is_active() {
            Ok(slot)
        } else {
            Err(AxError::ConnectionRefused)
        }
    }

    fn insert_abstract_slot(&self, name: Arc<Vec<u8>>, slot: Arc<BindSlot>) -> AxResult {
        let mut binds = self.abstract_binds.lock();
        let retired = match binds.get(&name) {
            Some(existing) if existing.is_active() => return Err(AxError::AddrInUse),
            Some(_) => binds.remove(&name),
            None => None,
        };
        if binds.try_reserve(1).is_err() {
            drop(binds);
            drop(retired);
            return Err(AxError::NoMemory);
        }
        binds.insert(name, slot);
        drop(binds);
        drop(retired);
        Ok(())
    }

    fn remove_abstract_slot(&self, name: &Arc<Vec<u8>>, expected: &Arc<BindSlot>) {
        let retired = {
            let mut binds = self.abstract_binds.lock();
            if binds
                .get(name)
                .is_some_and(|slot| Arc::ptr_eq(slot, expected))
            {
                binds.remove(name)
            } else {
                None
            }
        };
        drop(retired);
    }
}

/// A Unix address whose namespace lookup and access checks have already been
/// completed by the OS personality layer.
///
/// `axnet-ng` deliberately treats this as an opaque endpoint handle. It does
/// not know how pathname namespaces, credentials, ownership, or permissions
/// are represented by its caller.
#[derive(Clone)]
pub struct UnixSocketTarget {
    address: UnixSocketAddr,
    slot: Arc<BindSlot>,
}

impl UnixSocketTarget {
    /// Couples a user-visible address with the explicitly resolved bind slot.
    pub fn new(address: UnixSocketAddr, slot: Arc<BindSlot>) -> AxResult<Self> {
        if matches!(address, UnixSocketAddr::Unnamed) {
            return Err(AxError::InvalidInput);
        }
        Ok(Self { address, slot })
    }

    /// Reconstructs a resolved target from the endpoint identity fixed by
    /// bind, rather than from a symlink or hardlink alias used for lookup.
    pub fn from_bound(slot: Arc<BindSlot>) -> AxResult<Self> {
        let address = slot.bound_address()?;
        Self::new(address, slot)
    }

    fn address(&self) -> &UnixSocketAddr {
        &self.address
    }

    fn slot(&self) -> &BindSlot {
        &self.slot
    }
}

const UNIX_ENDPOINT_CLEANUP_SLOTS: usize = 16_384;
const ENDPOINT_CLEANUP_NODE_BUDGET: usize = 16;

struct UnixEndpointCleanup {
    next: *mut Self,
    namespace: Arc<UnixNamespace>,
    address: UnixSocketAddr,
    slot: Arc<BindSlot>,
    keepalive: Option<Arc<dyn Any + Send + Sync>>,
    remove_abstract: bool,
    _admission: EndpointCleanupAdmission,
}

// `next` is only mutated under unique ownership or the intrusive-list lock.
// Shared references cannot dereference it, and all payloads are Send + Sync.
unsafe impl Send for UnixEndpointCleanup {}
unsafe impl Sync for UnixEndpointCleanup {}

impl UnixEndpointCleanup {
    fn try_new(namespace: &Arc<UnixNamespace>, target: &UnixSocketTarget) -> AxResult<Box<Self>> {
        let admission = EndpointCleanupAdmission::try_acquire()?;
        Box::try_new(Self {
            next: ptr::null_mut(),
            namespace: namespace.clone(),
            address: target.address.clone(),
            slot: target.slot.clone(),
            keepalive: None,
            remove_abstract: false,
            _admission: admission,
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn run(self) {
        if self.remove_abstract
            && let UnixSocketAddr::Abstract(name) = &self.address
        {
            self.namespace.remove_abstract_slot(name, &self.slot);
        }
        self.slot.retire();
    }
}

static ENDPOINT_CLEANUP_ADMISSIONS: AtomicUsize = AtomicUsize::new(0);

struct EndpointCleanupAdmission;

impl EndpointCleanupAdmission {
    fn try_acquire() -> AxResult<Self> {
        ENDPOINT_CLEANUP_ADMISSIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < UNIX_ENDPOINT_CLEANUP_SLOTS).then_some(current + 1)
            })
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self)
    }
}

impl Drop for EndpointCleanupAdmission {
    fn drop(&mut self) {
        ENDPOINT_CLEANUP_ADMISSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct UnixEndpointCleanupList {
    head: *mut UnixEndpointCleanup,
    tail: *mut UnixEndpointCleanup,
}

unsafe impl Send for UnixEndpointCleanupList {}

impl UnixEndpointCleanupList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    fn push(&mut self, work: Box<UnixEndpointCleanup>) {
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

    fn pop(&mut self) -> Option<Box<UnixEndpointCleanup>> {
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

static ENDPOINT_CLEANUP_LIST: SpinNoIrq<UnixEndpointCleanupList> =
    SpinNoIrq::new(UnixEndpointCleanupList::new());
static ENDPOINT_CLEANUP_PENDING: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static UNIX_CLEANUP_TEST_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

fn publish_endpoint_cleanup(work: Box<UnixEndpointCleanup>) {
    let mut list = ENDPOINT_CLEANUP_LIST.lock();
    list.push(work);
    ENDPOINT_CLEANUP_PENDING.store(true, Ordering::Release);
}

fn drain_endpoint_cleanup_work() {
    for _ in 0..ENDPOINT_CLEANUP_NODE_BUDGET {
        let work = {
            let mut list = ENDPOINT_CLEANUP_LIST.lock();
            let work = list.pop();
            if list.head.is_null() {
                ENDPOINT_CLEANUP_PENDING.store(false, Ordering::Release);
            }
            work
        };
        let Some(work) = work else {
            break;
        };
        (*work).run();
    }
}

/// Returns whether bounded Unix transport or endpoint finalization is pending.
#[doc(hidden)]
pub fn has_deferred_receive_cleanup_work() -> bool {
    dgram::has_deferred_receive_cleanup_work() || ENDPOINT_CLEANUP_PENDING.load(Ordering::Acquire)
}

/// Runs one bounded task-context batch of Unix transport/endpoint finalizers.
#[doc(hidden)]
pub fn drain_deferred_receive_cleanup_work() {
    dgram::drain_deferred_receive_cleanup_work();
    drain_endpoint_cleanup_work();
}

#[derive(Default)]
struct LocalEndpoint {
    address: UnixSocketAddr,
    slot: Option<Arc<BindSlot>>,
    cleanup: Option<Box<UnixEndpointCleanup>>,
}

const ENDPOINT_UNBOUND: u8 = 0;
const ENDPOINT_RESERVED: u8 = 1;
const ENDPOINT_BOUND: u8 = 2;

/// Allocation-complete bind state that is still private to its caller.
///
/// Dropping the reservation rolls back transport state and returns the socket
/// to `UNBOUND`. `commit()` contains no fallible work, so an OS layer can make
/// its pathname/abstract namespace entry visible immediately before commit.
pub struct UnixBindReservation<'a> {
    socket: &'a UnixSocket,
    target: UnixSocketTarget,
    cleanup: Option<Box<UnixEndpointCleanup>>,
    committed: bool,
}

impl UnixBindReservation<'_> {
    fn commit_inner(mut self, owns_abstract: bool) {
        let mut cleanup = self.cleanup.take();
        if let Some(cleanup) = cleanup.as_mut() {
            cleanup.remove_abstract = owns_abstract;
        }
        {
            let mut local = self.socket.local.lock();
            local.address = self.target.address.clone();
            local.slot = Some(self.target.slot.clone());
            local.cleanup = cleanup;
        }
        self.socket
            .bind_state
            .store(ENDPOINT_BOUND, Ordering::Release);
        self.committed = true;
    }

    /// Publishes a caller-owned pathname binding after namespace commit.
    pub fn commit(self) {
        self.commit_inner(false);
    }

    /// Commits a pathname binding while retaining the exact backend dentry or
    /// inode-generation token until task-context endpoint finalization.
    pub fn commit_with_keepalive(mut self, keepalive: Arc<dyn Any + Send + Sync>) {
        if let Some(cleanup) = self.cleanup.as_mut() {
            cleanup.keepalive = Some(keepalive);
        }
        self.commit_inner(false);
    }
}

impl Drop for UnixBindReservation<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.target.slot.active.store(false, Ordering::Release);
        self.socket.transport.rollback_bind(self.target.slot());
        self.target.slot.clear_address();
        self.socket
            .bind_state
            .store(ENDPOINT_UNBOUND, Ordering::Release);
    }
}

/// A Unix domain socket.
pub struct UnixSocket {
    transport: Transport,
    namespace: Arc<UnixNamespace>,
    local: Mutex<LocalEndpoint>,
    remote_addr: Mutex<UnixSocketAddr>,
    bind_state: AtomicU8,
    connect_state: AtomicU8,
}
impl UnixSocket {
    /// Create a new Unix socket with the given transport.
    pub fn new(transport: impl Into<Transport>, namespace: Arc<UnixNamespace>) -> Self {
        let transport = transport.into();
        let connect_state = match &transport {
            Transport::Stream(stream) if stream.is_connected() => ENDPOINT_BOUND,
            Transport::Dgram(dgram) if dgram.is_connected() => ENDPOINT_BOUND,
            Transport::Stream(_) | Transport::Dgram(_) => ENDPOINT_UNBOUND,
        };
        Self {
            transport,
            namespace,
            local: Mutex::new(LocalEndpoint::default()),
            remote_addr: Mutex::new(UnixSocketAddr::Unnamed),
            bind_state: AtomicU8::new(ENDPOINT_UNBOUND),
            connect_state: AtomicU8::new(connect_state),
        }
    }

    #[cfg(test)]
    fn is_bound(&self) -> bool {
        self.bind_state.load(Ordering::Acquire) != ENDPOINT_UNBOUND
    }

    /// Completes all transport-side bind admission while the target remains
    /// private. Namespace publication belongs to the caller.
    pub fn reserve_bind(&self, target: UnixSocketTarget) -> AxResult<UnixBindReservation<'_>> {
        if self
            .bind_state
            .compare_exchange(
                ENDPOINT_UNBOUND,
                ENDPOINT_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(AxError::InvalidInput);
        }

        let cleanup = match UnixEndpointCleanup::try_new(&self.namespace, &target) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                self.bind_state.store(ENDPOINT_UNBOUND, Ordering::Release);
                return Err(error);
            }
        };
        if let Err(error) = target.slot.claim_address(target.address()) {
            self.bind_state.store(ENDPOINT_UNBOUND, Ordering::Release);
            return Err(error);
        }
        if let Err(error) = self.transport.bind(target.slot(), target.address()) {
            target.slot.clear_address();
            self.bind_state.store(ENDPOINT_UNBOUND, Ordering::Release);
            return Err(error);
        }
        target.slot.active.store(true, Ordering::Release);
        Ok(UnixBindReservation {
            socket: self,
            target,
            cleanup: Some(cleanup),
            committed: false,
        })
    }

    fn connect_target(&self, target: UnixSocketTarget, credentials: UnixCredentials) -> AxResult {
        if matches!(&self.transport, Transport::Dgram(_)) {
            // SOCK_DGRAM connect is an atomic peer replacement. Serialize the
            // public address with the transport swap; failure leaves both the
            // old channel and old getpeername identity intact.
            if !target.slot.is_active() {
                return Err(AxError::ConnectionRefused);
            }
            let local_addr = self.local.lock().address.clone();
            let mut remote_addr = self.remote_addr.lock();
            self.transport
                .connect(target.slot(), &local_addr, credentials)?;
            *remote_addr = target.address;
            self.connect_state.store(ENDPOINT_BOUND, Ordering::Release);
            return Ok(());
        }

        if self
            .connect_state
            .compare_exchange(
                ENDPOINT_UNBOUND,
                ENDPOINT_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(AxError::AlreadyConnected);
        }
        if !target.slot.is_active() {
            self.connect_state
                .store(ENDPOINT_UNBOUND, Ordering::Release);
            return Err(AxError::ConnectionRefused);
        }
        let local_addr = self.local.lock().address.clone();
        if let Err(error) = self
            .transport
            .connect(target.slot(), &local_addr, credentials)
        {
            self.connect_state
                .store(ENDPOINT_UNBOUND, Ordering::Release);
            return Err(error);
        }
        *self.remote_addr.lock() = target.address;
        self.connect_state.store(ENDPOINT_BOUND, Ordering::Release);
        Ok(())
    }

    /// Disconnects a Unix datagram socket using connect(AF_UNSPEC) semantics.
    /// Stream sockets reject the operation with EINVAL.
    pub fn disconnect(&self) -> AxResult<()> {
        let Transport::Dgram(dgram) = &self.transport else {
            return Err(AxError::InvalidInput);
        };
        let mut remote_addr = self.remote_addr.lock();
        dgram.disconnect();
        *remote_addr = UnixSocketAddr::Unnamed;
        self.connect_state
            .store(ENDPOINT_UNBOUND, Ordering::Release);
        Ok(())
    }

    /// Connects to a pathname target resolved and admitted by the caller.
    pub fn connect_resolved(&self, target: UnixSocketTarget) -> AxResult {
        self.connect_resolved_as(target, UnixCredentials::UNKNOWN)
    }

    /// Connects to a pathname target using the caller identity captured at
    /// connect(2), which is the identity exposed to the accepting peer.
    pub fn connect_resolved_as(
        &self,
        target: UnixSocketTarget,
        credentials: UnixCredentials,
    ) -> AxResult {
        if !matches!(target.address(), UnixSocketAddr::Path(_)) {
            return Err(AxError::InvalidInput);
        }
        self.connect_target(target, credentials)
    }

    /// Connects an abstract Unix socket using an operation-time credential
    /// snapshot supplied by the OS personality layer.
    pub fn connect_as(&self, remote_addr: SocketAddrEx, credentials: UnixCredentials) -> AxResult {
        let remote_addr = remote_addr.into_unix()?;
        match &remote_addr {
            UnixSocketAddr::Unnamed => Err(AxError::InvalidInput),
            UnixSocketAddr::Path(_) => Err(AxError::OperationNotSupported),
            UnixSocketAddr::Abstract(name) => {
                let slot = self.namespace.abstract_slot(name)?;
                self.connect_target(UnixSocketTarget::from_bound(slot)?, credentials)
            }
        }
    }

    /// Starts listening with the identity captured at listen(2).
    pub fn listen_as(&self, backlog: usize, credentials: UnixCredentials) -> AxResult<()> {
        let slot = self
            .local
            .lock()
            .slot
            .clone()
            .ok_or(AxError::InvalidInput)?;
        self.transport.listen(&slot, backlog, credentials)
    }

    /// Sends a datagram to a pathname target resolved and admitted by the caller.
    pub fn send_to_resolved(
        &self,
        src: impl Read + IoBuf,
        options: SendOptions,
        target: UnixSocketTarget,
    ) -> AxResult<usize> {
        if !matches!(target.address(), UnixSocketAddr::Path(_)) {
            return Err(AxError::InvalidInput);
        }
        match &self.transport {
            Transport::Dgram(dgram) if target.slot.is_active() => {
                dgram.send_to_slot(src, options, target.slot())
            }
            Transport::Dgram(_) => Err(AxError::ConnectionRefused),
            Transport::Stream(_) => Err(AxError::InvalidInput),
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
        match &local_addr {
            UnixSocketAddr::Unnamed => Err(AxError::InvalidInput),
            UnixSocketAddr::Path(_) => Err(AxError::OperationNotSupported),
            UnixSocketAddr::Abstract(name) => {
                let name = name.clone();
                let slot = Arc::try_new(BindSlot::default()).map_err(|_| AxError::NoMemory)?;
                let target = UnixSocketTarget::new(local_addr, slot.clone())?;
                let reservation = self.reserve_bind(target)?;
                // Only transport-ready slots enter the public abstract map.
                // From this point commit is allocation-free and infallible.
                self.namespace.insert_abstract_slot(name, slot)?;
                reservation.commit_inner(true);
                Ok(())
            }
        }
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> AxResult {
        self.connect_as(remote_addr, UnixCredentials::UNKNOWN)
    }

    fn listen(&self, backlog: usize) -> AxResult {
        self.listen_as(backlog, UnixCredentials::UNKNOWN)
    }

    fn accept(&self) -> AxResult<Socket> {
        let (transport, peer_addr) = block_on(interruptible(self.transport.accept()))??;
        Ok(Socket::Unix(Self {
            transport,
            namespace: self.namespace.clone(),
            local: Mutex::new(LocalEndpoint {
                address: self.local.lock().address.clone(),
                slot: None,
                cleanup: None,
            }),
            remote_addr: Mutex::new(peer_addr),
            bind_state: AtomicU8::new(ENDPOINT_BOUND),
            connect_state: AtomicU8::new(ENDPOINT_BOUND),
        }))
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if let (Transport::Dgram(dgram), Some(SocketAddrEx::Unix(UnixSocketAddr::Abstract(name)))) =
            (&self.transport, options.to.as_ref())
        {
            let slot = self.namespace.abstract_slot(name)?;
            return dgram.send_to_slot(src, options, &slot);
        }
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> AxResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.local.lock().address.clone()))
    }

    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        let remote_addr = self.remote_addr.lock();
        if self.connect_state.load(Ordering::Acquire) != ENDPOINT_BOUND {
            return Err(AxError::NotConnected);
        }
        Ok(SocketAddrEx::Unix(remote_addr.clone()))
    }

    fn shutdown(&self, how: Shutdown) -> AxResult {
        self.transport.shutdown(how)
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        if let Some(cleanup) = self.local.get_mut().cleanup.take() {
            cleanup.slot.active.store(false, Ordering::Release);
            publish_endpoint_cleanup(cleanup);
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
    use crate::{CMsgData, SendFlags, options::UnixCredentials};

    fn test_abstract_name(name: &[u8]) -> Arc<Vec<u8>> {
        Arc::new(name.to_vec())
    }

    fn test_path(path: &str) -> UnixSocketAddr {
        UnixSocketAddr::Path(Arc::new(String::from(path)))
    }

    fn test_socket(transport: impl Into<Transport>) -> UnixSocket {
        UnixSocket::new(transport, UnixNamespace::try_new().unwrap())
    }

    fn test_socket_in(
        namespace: &Arc<UnixNamespace>,
        transport: impl Into<Transport>,
    ) -> UnixSocket {
        UnixSocket::new(transport, namespace.clone())
    }

    fn drain_all_unix_cleanup() {
        for _ in 0..65_536 {
            if !has_deferred_receive_cleanup_work() {
                return;
            }
            drain_deferred_receive_cleanup_work();
        }
        assert!(!has_deferred_receive_cleanup_work());
    }

    #[test]
    fn abstract_binding_is_released_with_its_owner() {
        let _guard = UNIX_CLEANUP_TEST_LOCK.lock();
        drain_all_unix_cleanup();
        let name = test_abstract_name(b"axnet-ng-drop-test");
        let namespace = UnixNamespace::try_new().unwrap();
        let socket = test_socket_in(&namespace, DgramTransport::new().unwrap());
        socket
            .bind(SocketAddrEx::Unix(UnixSocketAddr::Abstract(name.clone())))
            .unwrap();
        assert!(namespace.abstract_binds.lock().contains_key(&name));
        drop(socket);

        // Namespace removal and the final slot/Sender destructors are owned by
        // the task-context worker, not by the final socket-release context.
        assert!(namespace.abstract_binds.lock().contains_key(&name));
        drain_all_unix_cleanup();
        assert!(!namespace.abstract_binds.lock().contains_key(&name));
    }

    #[test]
    fn pathname_operations_require_an_explicit_target() {
        let socket = test_socket(DgramTransport::new().unwrap());
        let path = test_path("/tmp/axnet-layer-test");

        assert_eq!(
            socket.bind(SocketAddrEx::Unix(path.clone())).unwrap_err(),
            AxError::OperationNotSupported
        );
        assert_eq!(
            socket
                .connect(SocketAddrEx::Unix(path.clone()))
                .unwrap_err(),
            AxError::OperationNotSupported
        );
        assert!(!socket.is_bound());
    }

    #[test]
    fn resolved_path_binding_keeps_the_slot_without_a_filesystem_dependency() {
        let socket = test_socket(DgramTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        let path = test_path("/tmp/axnet-explicit-target");
        socket
            .reserve_bind(UnixSocketTarget::new(path.clone(), slot.clone()).unwrap())
            .unwrap()
            .commit();

        assert!(socket.is_bound());
        assert!(Arc::ptr_eq(
            socket.local.lock().slot.as_ref().unwrap(),
            &slot
        ));
        assert!(matches!(
            socket.local_addr().unwrap(),
            SocketAddrEx::Unix(UnixSocketAddr::Path(_))
        ));
    }

    #[test]
    fn endpoint_cleanup_leaves_a_retained_path_slot_inert() {
        let _guard = UNIX_CLEANUP_TEST_LOCK.lock();
        drain_all_unix_cleanup();
        let socket = test_socket(DgramTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        socket
            .reserve_bind(
                UnixSocketTarget::new(test_path("/tmp/axnet-retired-slot"), slot.clone()).unwrap(),
            )
            .unwrap()
            .commit();
        assert!(slot.dgram.lock().is_some());

        drop(socket);
        assert!(slot.dgram.lock().is_some());
        drain_all_unix_cleanup();

        assert!(!slot.is_active());
        assert!(slot.dgram.lock().is_none());
        assert!(slot.stream.lock().is_none());
        assert!(slot.address.lock().is_none());
    }

    #[test]
    fn resolved_target_uses_the_identity_fixed_at_bind() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let listener = test_socket(StreamTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        let canonical = test_path("/tmp/axnet-canonical-name");
        listener
            .reserve_bind(UnixSocketTarget::new(canonical.clone(), slot.clone()).unwrap())
            .unwrap()
            .commit();
        listener.listen_as(1, credentials).unwrap();

        // A VFS lookup may have used a symlink or hardlink alias, but it hands
        // axnet only the exact slot. Peer identity comes from bind state.
        let resolved = UnixSocketTarget::from_bound(slot).unwrap();
        let UnixSocketAddr::Path(resolved_path) = &resolved.address else {
            panic!("resolved pathname target changed address kind");
        };
        assert_eq!(resolved_path.as_str(), "/tmp/axnet-canonical-name");
        let client = test_socket(StreamTransport::new().unwrap());
        client.connect_resolved_as(resolved, credentials).unwrap();
        let SocketAddrEx::Unix(UnixSocketAddr::Path(peer_path)) = client.peer_addr().unwrap()
        else {
            panic!("connected pathname peer changed address kind");
        };
        assert_eq!(peer_path.as_str(), "/tmp/axnet-canonical-name");
    }

    #[test]
    fn failed_datagram_bind_reservation_can_retry_on_the_same_socket() {
        let socket = test_socket(DgramTransport::new().unwrap());
        let first_slot = Arc::new(BindSlot::default());
        let first =
            UnixSocketTarget::new(test_path("/tmp/axnet-rollback-first"), first_slot.clone())
                .unwrap();
        drop(socket.reserve_bind(first).unwrap());

        assert!(!socket.is_bound());
        assert!(first_slot.dgram.lock().is_none());

        let second_slot = Arc::new(BindSlot::default());
        socket
            .reserve_bind(
                UnixSocketTarget::new(test_path("/tmp/axnet-rollback-second"), second_slot.clone())
                    .unwrap(),
            )
            .unwrap()
            .commit();
        assert!(socket.is_bound());
        assert!(second_slot.dgram.lock().is_some());
    }

    struct RollbackDropProbe(Arc<AtomicUsize>);

    impl Drop for RollbackDropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn rollback_drains_traffic_reached_through_a_cloned_private_target() {
        let path = test_path("/tmp/axnet-private-rollback");
        let listener = test_socket(DgramTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        let target = UnixSocketTarget::new(path.clone(), slot).unwrap();
        let reservation = listener.reserve_bind(target.clone()).unwrap();
        let sender = test_socket(DgramTransport::new().unwrap());
        let drops = Arc::new(AtomicUsize::new(0));
        let byte = [7u8; 1];
        sender
            .send_to_resolved(
                &byte[..],
                SendOptions {
                    to: Some(SocketAddrEx::Unix(path)),
                    flags: SendFlags::DONT_WAIT,
                    cmsg: alloc::vec![
                        CMsgData::new(Box::new(RollbackDropProbe(drops.clone())), 1,)
                    ],
                },
                target.clone(),
            )
            .unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 0);

        drop(reservation);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!target.slot.is_active());
        assert!(target.slot.dgram.lock().is_none());
    }

    #[test]
    fn stream_rollback_closes_clients_reached_before_namespace_commit() {
        let credentials = UnixCredentials::new(31, 32, 33);
        let listener = test_socket(StreamTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        let target =
            UnixSocketTarget::new(test_path("/tmp/axnet-private-stream-rollback"), slot).unwrap();
        let reservation = listener.reserve_bind(target.clone()).unwrap();
        listener
            .transport
            .listen(target.slot(), 1, credentials)
            .unwrap();
        let client = test_socket(StreamTransport::new().unwrap());
        client
            .connect_resolved_as(target.clone(), credentials)
            .unwrap();

        drop(reservation);
        let events = client.poll();
        assert!(events.contains(IoEvents::RDHUP));
        assert!(events.contains(IoEvents::ERR));
        assert!(events.contains(IoEvents::HUP));
        assert!(!target.slot.is_active());
        assert!(target.slot.stream.lock().is_none());
    }

    #[test]
    fn abstract_publication_failure_rolls_back_and_name_can_be_reused() {
        let name = test_abstract_name(b"axnet-ng-abstract-admission-test");
        let address = SocketAddrEx::Unix(UnixSocketAddr::Abstract(name));
        let namespace = UnixNamespace::try_new().unwrap();
        let first = test_socket_in(&namespace, DgramTransport::new().unwrap());
        let loser = test_socket_in(&namespace, DgramTransport::new().unwrap());

        first.bind(address.clone()).unwrap();
        assert_eq!(loser.bind(address.clone()), Err(AxError::AddrInUse));
        assert!(!loser.is_bound());

        drop(first);
        loser.bind(address).unwrap();
        assert!(loser.is_bound());
    }

    #[test]
    fn concurrent_abstract_publication_has_one_winner_and_no_stale_name() {
        use std::sync::{Arc as StdArc, Barrier};

        let name = test_abstract_name(b"axnet-ng-abstract-race-test");
        let address = SocketAddrEx::Unix(UnixSocketAddr::Abstract(name));
        let namespace = UnixNamespace::try_new().unwrap();
        let left = StdArc::new(test_socket_in(&namespace, DgramTransport::new().unwrap()));
        let right = StdArc::new(test_socket_in(&namespace, DgramTransport::new().unwrap()));
        let start = StdArc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let run = |socket: StdArc<UnixSocket>| {
                let start = start.clone();
                let address = address.clone();
                scope.spawn(move || {
                    start.wait();
                    socket.bind(address)
                })
            };
            let left_result = run(left.clone());
            let right_result = run(right.clone());
            [left_result.join().unwrap(), right_result.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(AxError::AddrInUse))
                .count(),
            1
        );

        drop(left);
        drop(right);
        let replacement = test_socket_in(&namespace, DgramTransport::new().unwrap());
        replacement.bind(address).unwrap();
    }

    #[test]
    fn abstract_names_are_isolated_between_network_namespaces() {
        let name = test_abstract_name(b"axnet-ng-netns-isolation-test");
        let address = SocketAddrEx::Unix(UnixSocketAddr::Abstract(name.clone()));
        let left_namespace = UnixNamespace::try_new().unwrap();
        let right_namespace = UnixNamespace::try_new().unwrap();
        let left = test_socket_in(&left_namespace, DgramTransport::new().unwrap());
        let right = test_socket_in(&right_namespace, DgramTransport::new().unwrap());

        left.bind(address.clone()).unwrap();
        right.bind(address.clone()).unwrap();

        let left_client = test_socket_in(&left_namespace, DgramTransport::new().unwrap());
        let right_client = test_socket_in(&right_namespace, DgramTransport::new().unwrap());
        left_client.connect(address.clone()).unwrap();
        right_client.connect(address).unwrap();

        let left_remote = left_namespace.abstract_slot(&name).unwrap();
        let right_remote = right_namespace.abstract_slot(&name).unwrap();
        assert!(!Arc::ptr_eq(&left_remote, &right_remote));

        let left_only_name = test_abstract_name(b"axnet-ng-netns-left-only-test");
        let left_only = SocketAddrEx::Unix(UnixSocketAddr::Abstract(left_only_name));
        let left_only_server = test_socket_in(&left_namespace, DgramTransport::new().unwrap());
        left_only_server.bind(left_only.clone()).unwrap();
        let cross_namespace_client =
            test_socket_in(&right_namespace, DgramTransport::new().unwrap());
        assert_eq!(
            cross_namespace_client.connect(left_only),
            Err(AxError::ConnectionRefused)
        );
    }

    #[test]
    fn resolved_targets_reject_the_wrong_unix_transport() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let dgram_listener = test_socket(DgramTransport::new().unwrap());
        let dgram_slot = Arc::new(BindSlot::default());
        let dgram_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-dgram-target"), dgram_slot).unwrap();
        dgram_listener
            .reserve_bind(dgram_target.clone())
            .unwrap()
            .commit();
        let stream_client = test_socket(StreamTransport::new().unwrap());
        assert_eq!(
            stream_client.connect_resolved_as(dgram_target, credentials),
            Err(AxError::from(LinuxError::EPROTOTYPE))
        );

        let stream_listener = test_socket(StreamTransport::new().unwrap());
        let stream_slot = Arc::new(BindSlot::default());
        let stream_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-stream-target"), stream_slot).unwrap();
        stream_listener
            .reserve_bind(stream_target.clone())
            .unwrap()
            .commit();
        let dgram_client = test_socket(DgramTransport::new().unwrap());
        assert_eq!(
            dgram_client.connect_resolved_as(stream_target, credentials),
            Err(AxError::from(LinuxError::EPROTOTYPE))
        );
    }

    #[test]
    fn stale_path_slots_report_connection_refused() {
        let credentials = UnixCredentials::new(1, 2, 3);

        let dgram_slot = Arc::new(BindSlot::default());
        let dgram_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-stale-dgram"), dgram_slot).unwrap();
        let dgram_listener = test_socket(DgramTransport::new().unwrap());
        dgram_listener
            .reserve_bind(dgram_target.clone())
            .unwrap()
            .commit();
        drop(dgram_listener);
        let dgram_client = test_socket(DgramTransport::new().unwrap());
        assert_eq!(
            dgram_client.connect_resolved(dgram_target),
            Err(AxError::ConnectionRefused)
        );

        let stream_slot = Arc::new(BindSlot::default());
        let stream_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-stale-stream"), stream_slot).unwrap();
        let stream_listener = test_socket(StreamTransport::new().unwrap());
        stream_listener
            .reserve_bind(stream_target.clone())
            .unwrap()
            .commit();
        stream_listener.listen_as(1, credentials).unwrap();
        drop(stream_listener);
        let stream_client = test_socket(StreamTransport::new().unwrap());
        assert_eq!(
            stream_client.connect_resolved_as(stream_target, credentials),
            Err(AxError::ConnectionRefused)
        );
    }

    #[test]
    fn datagram_reconnect_is_atomic_and_disconnect_is_idempotent() {
        let first_server = test_socket(DgramTransport::new().unwrap());
        let first_slot = Arc::new(BindSlot::default());
        let first_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-dgram-peer-a"), first_slot).unwrap();
        first_server
            .reserve_bind(first_target.clone())
            .unwrap()
            .commit();

        let second_server = test_socket(DgramTransport::new().unwrap());
        let second_slot = Arc::new(BindSlot::default());
        let second_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-dgram-peer-b"), second_slot).unwrap();
        second_server
            .reserve_bind(second_target.clone())
            .unwrap()
            .commit();

        let client = test_socket(DgramTransport::new().unwrap());
        client.connect_resolved(first_target).unwrap();
        client.connect_resolved(second_target).unwrap();
        let SocketAddrEx::Unix(UnixSocketAddr::Path(peer)) = client.peer_addr().unwrap() else {
            panic!("reconnected datagram peer lost pathname identity");
        };
        assert_eq!(peer.as_str(), "/tmp/axnet-dgram-peer-b");

        let stale_server = test_socket(DgramTransport::new().unwrap());
        let stale_slot = Arc::new(BindSlot::default());
        let stale_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-dgram-stale-peer"), stale_slot).unwrap();
        stale_server
            .reserve_bind(stale_target.clone())
            .unwrap()
            .commit();
        drop(stale_server);
        assert_eq!(
            client.connect_resolved(stale_target),
            Err(AxError::ConnectionRefused)
        );
        let SocketAddrEx::Unix(UnixSocketAddr::Path(peer)) = client.peer_addr().unwrap() else {
            panic!("failed reconnect changed the old peer identity");
        };
        assert_eq!(peer.as_str(), "/tmp/axnet-dgram-peer-b");

        client.disconnect().unwrap();
        client.disconnect().unwrap();
        assert!(matches!(client.peer_addr(), Err(AxError::NotConnected)));
        let byte = [0u8; 1];
        assert_eq!(
            client.send(&byte[..], SendOptions::default()),
            Err(AxError::NotConnected)
        );
    }

    #[test]
    fn connected_pairs_report_unnamed_peers() {
        let credentials = UnixCredentials::new(41, 42, 43);
        let namespace = UnixNamespace::try_new().unwrap();
        let (stream_left, stream_right) = StreamTransport::new_pair(credentials).unwrap();
        let stream_left = UnixSocket::new(stream_left, namespace.clone());
        let stream_right = UnixSocket::new(stream_right, namespace.clone());
        assert!(matches!(
            stream_left.peer_addr(),
            Ok(SocketAddrEx::Unix(UnixSocketAddr::Unnamed))
        ));
        assert!(matches!(
            stream_right.peer_addr(),
            Ok(SocketAddrEx::Unix(UnixSocketAddr::Unnamed))
        ));

        let (dgram_left, dgram_right) = DgramTransport::new_pair(credentials).unwrap();
        let dgram_left = UnixSocket::new(dgram_left, namespace.clone());
        let dgram_right = UnixSocket::new(dgram_right, namespace);
        assert!(matches!(
            dgram_left.peer_addr(),
            Ok(SocketAddrEx::Unix(UnixSocketAddr::Unnamed))
        ));
        assert!(matches!(
            dgram_right.peer_addr(),
            Ok(SocketAddrEx::Unix(UnixSocketAddr::Unnamed))
        ));
    }

    #[test]
    fn concurrent_bind_reservation_has_one_transport_winner() {
        use std::sync::{Arc as StdArc, Barrier};

        let socket = StdArc::new(test_socket(DgramTransport::new().unwrap()));
        let start = StdArc::new(Barrier::new(2));
        let admitted = StdArc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let run = |index: usize| {
                let socket = socket.clone();
                let start = start.clone();
                let admitted = admitted.clone();
                scope.spawn(move || {
                    let slot = Arc::new(BindSlot::default());
                    let path = Arc::new(String::from(if index == 0 {
                        "/tmp/axnet-concurrent-bind-a"
                    } else {
                        "/tmp/axnet-concurrent-bind-b"
                    }));
                    start.wait();
                    let reservation = socket.reserve_bind(
                        UnixSocketTarget::new(UnixSocketAddr::Path(path), slot.clone()).unwrap(),
                    );
                    admitted.wait();
                    match reservation {
                        Ok(reservation) => {
                            reservation.commit();
                            (Ok(()), slot.dgram.lock().is_some())
                        }
                        Err(error) => (Err(error), slot.dgram.lock().is_some()),
                    }
                })
            };
            let left = run(0);
            let right = run(1);
            [left.join().unwrap(), right.join().unwrap()]
        });

        assert_eq!(
            results.iter().filter(|(result, _)| result.is_ok()).count(),
            1
        );
        assert_eq!(results.iter().filter(|(_, touched)| *touched).count(), 1);
    }

    #[test]
    fn concurrent_connect_enqueues_only_one_stream_peer() {
        use std::sync::{Arc as StdArc, Barrier};

        let credentials = UnixCredentials::new(1, 2, 3);
        let listener = test_socket(StreamTransport::new().unwrap());
        let slot = Arc::new(BindSlot::default());
        let target =
            UnixSocketTarget::new(test_path("/tmp/axnet-concurrent-connect"), slot).unwrap();
        listener.reserve_bind(target.clone()).unwrap().commit();
        listener.listen_as(2, credentials).unwrap();

        let client = StdArc::new(test_socket(StreamTransport::new().unwrap()));
        let barrier = StdArc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let run = || {
                let client = client.clone();
                let target = target.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    client.connect_resolved_as(target, credentials)
                })
            };
            let left = run();
            let right = run();
            [left.join().unwrap(), right.join().unwrap()]
        });

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(AxError::AlreadyConnected))
                .count(),
            1
        );
        let Transport::Stream(listener_transport) = &listener.transport else {
            unreachable!();
        };
        assert_eq!(listener_transport.pending_connections(), 1);
    }

    #[test]
    fn resolved_path_target_keeps_old_listener_distinct_from_rebound_name() {
        let credentials = UnixCredentials::new(1, 2, 3);
        let old_listener = test_socket(StreamTransport::new().unwrap());
        let old_slot = Arc::new(BindSlot::default());
        let old_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-rebound-name"), old_slot.clone()).unwrap();
        old_listener
            .reserve_bind(old_target.clone())
            .unwrap()
            .commit();
        old_listener.listen_as(1, credentials).unwrap();

        // Model unlink by dropping the namespace's Arc. The listener and an
        // already resolved connector still retain the old slot.
        drop(old_slot);
        let old_client = test_socket(StreamTransport::new().unwrap());
        old_client
            .connect_resolved_as(old_target.clone(), credentials)
            .unwrap();

        let new_listener = test_socket(StreamTransport::new().unwrap());
        let new_slot = Arc::new(BindSlot::default());
        let new_target =
            UnixSocketTarget::new(test_path("/tmp/axnet-rebound-name"), new_slot).unwrap();
        assert!(!Arc::ptr_eq(&old_target.slot, &new_target.slot));
        new_listener
            .reserve_bind(new_target.clone())
            .unwrap()
            .commit();
        new_listener.listen_as(1, credentials).unwrap();
        let new_client = test_socket(StreamTransport::new().unwrap());
        new_client
            .connect_resolved_as(new_target, credentials)
            .unwrap();

        let Transport::Stream(old_transport) = &old_listener.transport else {
            unreachable!();
        };
        let Transport::Stream(new_transport) = &new_listener.transport else {
            unreachable!();
        };
        assert_eq!(old_transport.pending_connections(), 1);
        assert_eq!(new_transport.pending_connections(), 1);
    }
}
