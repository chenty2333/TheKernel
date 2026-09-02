//! rpc_pipefs rendezvous for rpc.gssd context construction.
//!
//! Context payloads are published through the keyring construction path; this
//! namespace carries only bounded request/reply records and OFD readiness.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    FileNodeOps, Filesystem, FilesystemOps, FsName, FsNameBuf, Metadata, MetadataUpdate, NodeFlags,
    NodeOps, NodePermission, NodeType, NodeUserData, VfsError, VfsResult,
};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;
use inherit_methods_macro::inherit_methods;

use crate::{
    file::{IoDst, IoSrc},
    pseudofs::{
        DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFs, SimpleFsNode, try_boxed_names,
    },
};

/// Linux auth_gss uses a 256-byte v1 text upcall and rejects downcalls over
/// `MSG_BUF_MAXSIZE` (1024 bytes).
pub(crate) const RPC_GSSD_MAX_MESSAGE: usize = 1024;
const UPCALL_MAX_MESSAGE: usize = 256;
const UID_PREFIX: usize = core::mem::size_of::<u32>();

/// Short-lived downcall/key-import storage.  It is deliberately not `Clone`
/// or `Debug`; all exits from pipe usercopy and parser paths erase it.
struct ZeroizingBytes(Vec<u8>);
impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        self.0.fill(0)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GssdUpcall {
    pub id: u64,
    pub uid: u32,
    pub target: Vec<u8>,
    pub service: Vec<u8>,
    pub payload: Vec<u8>,
    claimed: bool,
}

#[derive(Clone)]
pub(crate) struct GssdReply {
    pub id: u64,
    pub uid: u32,
    pub daemon_generation: u64,
    /// Downcalls carry only a uid.  Once matched to the one allowed claimed
    /// request for that uid, retain the complete identity for mount-side and
    /// keyring authorization; never let a same-uid different-server reply
    /// become interchangeable.
    pub target: Vec<u8>,
    pub service: Vec<u8>,
    pub context: GssContext,
}

/// Parsed Linux gss_cl_ctx downcall.  The imported mechanism context stays
/// opaque here; only a selected mechanism may turn it into MIC/wrap state.
#[derive(Clone)]
pub(crate) struct GssContext {
    /// Opaque serial in the kernel-only keyring context store.  The raw
    /// imported blob below is retained only long enough to hand it to the
    /// selected mechanism; callers should use this serial thereafter.
    pub key_serial: u64,
    pub timeout_seconds: u32,
    pub window_size: u32,
    pub wire_context: Vec<u8>,
    pub imported_context: Vec<u8>,
    pub acceptor: Vec<u8>,
}
impl Drop for GssContext {
    fn drop(&mut self) {
        self.wire_context.fill(0);
        self.imported_context.fill(0);
        self.acceptor.fill(0);
    }
}
impl GssContext {
    /// Explicitly transfer the selected mechanism's public wire context out
    /// of a zeroizing Drop type.  Remaining import/acceptor material stays
    /// owned here and is erased on every exit.
    pub(crate) fn take_wire_context(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.wire_context)
    }
}

struct GssdQueueState {
    pending: Vec<GssdUpcall>,
    replies: Vec<GssdReply>,
    closed: bool,
    daemon_open: bool,
    generation: u64,
    handed: Vec<(u64, u64)>,
}

/// The pipe queue is independent of NFS mounts: daemon close can cancel all
/// pending requests without retaining a socket, mount, or key object.
pub(crate) struct GssdQueue {
    state: Mutex<GssdQueueState>,
    waiters: PollSet,
}
/// Owns a claimed gssd downcall serial while mount construction is in flight.
/// Any failure after take_reply revokes the key material; successful consume
/// disarms the guard only after the mechanism owns it.
pub(crate) struct HandedLeaseGuard {
    queue: Arc<GssdQueue>,
    serial: u64,
    active: bool,
}
impl HandedLeaseGuard {
    pub(crate) fn consume(mut self) {
        self.active = false;
    }
}
impl Drop for HandedLeaseGuard {
    fn drop(&mut self) {
        if self.active {
            crate::keyring::revoke_nfs_gss_context(self.serial)
        }
    }
}

impl GssdQueue {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(GssdQueueState {
                pending: Vec::new(),
                replies: Vec::new(),
                closed: true,
                daemon_open: false,
                generation: 0,
                handed: Vec::new(),
            }),
            waiters: PollSet::new(),
        }
    }

    /// Serializes the Linux v1 rpc.gssd upcall.  This is deliberately text,
    /// not a private request id framing; matching downcalls use native uid.
    pub(crate) fn submit_v1(&self, uid: u32, target: &[u8], service: &[u8]) -> AxResult<u64> {
        fn atom(value: &[u8]) -> bool {
            !value.is_empty()
                && !value
                    .iter()
                    .any(|byte| byte.is_ascii_whitespace() || *byte == b'=' || *byte == 0)
        }
        if !atom(target) || !atom(service) {
            return Err(AxError::InvalidInput);
        }
        let mut text = alloc::format!("mech=krb5 uid={uid} target=").into_bytes();
        text.try_reserve(
            target
                .len()
                .saturating_add(service.len())
                .saturating_add(10),
        )
        .map_err(|_| AxError::NoMemory)?;
        text.extend_from_slice(target);
        text.extend_from_slice(b" service=");
        text.extend_from_slice(service);
        text.push(b'\n');
        self.submit_identity(uid, target, service, text)
    }

    fn submit_identity(
        &self,
        uid: u32,
        target: &[u8],
        service: &[u8],
        payload: Vec<u8>,
    ) -> AxResult<u64> {
        if payload.len() > UPCALL_MAX_MESSAGE {
            return Err(AxError::InvalidInput);
        }
        let id = NEXT_UPCALL.fetch_add(1, Ordering::Relaxed).max(1);
        let mut state = self.state.lock();
        if state.closed {
            return Err(LinuxError::EPIPE.into());
        }
        // auth_gss coalesces a pending credential refresh by uid.  Its
        // downcall ABI has no request cookie, therefore publishing two
        // claimed records for one uid would make a valid daemon reply
        // ambiguous.
        if let Some(existing) = state.pending.iter().find(|request| {
            request.uid == uid && request.target == target && request.service == service
        }) {
            return Ok(existing.id);
        }
        state
            .pending
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        state.pending.push(GssdUpcall {
            id,
            uid,
            target: target.to_vec(),
            service: service.to_vec(),
            payload,
            claimed: false,
        });
        drop(state);
        self.waiters.wake();
        Ok(id)
    }

    fn prepare_upcall(&self) -> AxResult<(u64, Vec<u8>)> {
        let mut state = self.state.lock();
        // The native auth_gss downcall has no request cookie, only uid.  Keep
        // at most one claimed request per uid so a daemon reply is never
        // ambiguously associated with another target/service.
        let Some(index) = state.pending.iter().position(|request| {
            !request.claimed
                && !state
                    .pending
                    .iter()
                    .any(|other| other.claimed && other.uid == request.uid)
        }) else {
            return if state.closed {
                Err(LinuxError::EPIPE.into())
            } else {
                Err(AxError::WouldBlock)
            };
        };
        let request = &state.pending[index];
        let wire = request.payload.clone();
        let id = request.id;
        state.pending[index].claimed = true;
        Ok((id, wire))
    }

    fn rollback_claim(&self, id: u64) {
        let mut state = self.state.lock();
        if let Some(index) = state.pending.iter().position(|request| request.id == id) {
            state.pending[index].claimed = false;
        }
        drop(state);
        self.waiters.wake();
    }

    fn read_upcall(&self, destination: &mut IoDst) -> AxResult<usize> {
        let (id, wire) = self.prepare_upcall()?;
        if destination.remaining_mut() < wire.len() {
            self.rollback_claim(id);
            return Err(AxError::InvalidInput);
        }
        let result = destination.write_all(&wire).map(|_| wire.len());
        if result.is_err() {
            self.rollback_claim(id);
        }
        result
    }

    fn read_upcall_slice(&self, destination: &mut [u8]) -> VfsResult<usize> {
        let (id, wire) = self.prepare_upcall().map_err(VfsError::from)?;
        if destination.len() < wire.len() {
            self.rollback_claim(id);
            return Err(VfsError::InvalidInput);
        }
        destination[..wire.len()].copy_from_slice(&wire);
        Ok(wire.len())
    }

    fn write_reply(&self, source: &mut IoSrc) -> AxResult<usize> {
        let length = source.remaining();
        if !(UID_PREFIX..=RPC_GSSD_MAX_MESSAGE).contains(&length) {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = ZeroizingBytes(Vec::new());
        bytes
            .0
            .try_reserve_exact(length)
            .map_err(|_| AxError::NoMemory)?;
        bytes.0.resize(length, 0);
        source.read_exact(&mut bytes.0)?;
        let mut reply = GssdReply::decode(&bytes.0)?;
        let mut state = self.state.lock();
        if state.closed {
            return Err(LinuxError::EPIPE.into());
        }
        if state.replies.iter().any(|current| current.uid == reply.uid) {
            return Err(AxError::AlreadyExists);
        }
        let request = state
            .pending
            .iter()
            .position(|request| request.uid == reply.uid && request.claimed)
            .ok_or(AxError::InvalidInput)?;
        state
            .replies
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        reply.id = state.pending[request].id;
        reply.daemon_generation = state.generation;
        reply.target = state.pending[request].target.clone();
        reply.service = state.pending[request].service.clone();
        reply.context.key_serial = crate::keyring::publish_nfs_gss_context(
            reply.uid,
            &reply.target,
            &reply.service,
            &reply.context.imported_context,
        )?;
        reply.context.imported_context.fill(0);
        reply.context.imported_context.clear();
        state.replies.push(reply);
        state.pending.remove(request);
        drop(state);
        self.waiters.wake();
        Ok(length)
    }

    fn write_reply_slice(&self, source: &[u8]) -> VfsResult<usize> {
        let length = source.len();
        if !(UID_PREFIX..=RPC_GSSD_MAX_MESSAGE).contains(&length) {
            return Err(VfsError::InvalidInput);
        }
        let mut reply = GssdReply::decode(source).map_err(VfsError::from)?;
        let mut state = self.state.lock();
        if state.closed {
            return Err(VfsError::from(LinuxError::EPIPE));
        }
        if state.replies.iter().any(|current| current.uid == reply.uid) {
            return Err(VfsError::AlreadyExists);
        }
        let request = state
            .pending
            .iter()
            .position(|request| request.uid == reply.uid && request.claimed)
            .ok_or(VfsError::InvalidInput)?;
        state
            .replies
            .try_reserve(1)
            .map_err(|_| VfsError::NoMemory)?;
        reply.id = state.pending[request].id;
        reply.daemon_generation = state.generation;
        reply.target = state.pending[request].target.clone();
        reply.service = state.pending[request].service.clone();
        reply.context.key_serial = crate::keyring::publish_nfs_gss_context(
            reply.uid,
            &reply.target,
            &reply.service,
            &reply.context.imported_context,
        )
        .map_err(VfsError::from)?;
        reply.context.imported_context.fill(0);
        reply.context.imported_context.clear();
        state.replies.push(reply);
        state.pending.remove(request);
        drop(state);
        self.waiters.wake();
        Ok(length)
    }

    pub(crate) fn daemon_close(&self) {
        let mut state = self.state.lock();
        if !state.daemon_open {
            return;
        }
        state.daemon_open = false;
        state.closed = true;
        state.pending.clear();
        let mut serials: Vec<u64> = state
            .replies
            .iter()
            .map(|reply| reply.context.key_serial)
            .filter(|serial| *serial != 0)
            .collect();
        serials.extend(state.handed.iter().map(|(_, serial)| *serial));
        state.handed.clear();
        state.replies.clear();
        drop(state);
        for serial in serials {
            crate::keyring::revoke_nfs_gss_context(serial);
        }
        self.waiters.wake();
    }

    fn daemon_open(&self) -> AxResult {
        let mut state = self.state.lock();
        if state.daemon_open {
            return Err(AxError::ResourceBusy);
        }
        state.daemon_open = true;
        state.closed = false;
        state.generation = state.generation.wrapping_add(1).max(1);
        drop(state);
        self.waiters.wake();
        Ok(())
    }

    pub(crate) fn take_reply(&self, id: u64) -> Option<GssdReply> {
        let mut state = self.state.lock();
        let index = state.replies.iter().position(|reply| reply.id == id)?;
        let reply = state.replies.remove(index);
        if reply.context.key_serial != 0 {
            state
                .handed
                .push((reply.daemon_generation, reply.context.key_serial));
        }
        Some(reply)
    }

    /// A reply handed to a mount worker remains leased to this daemon
    /// generation until it is consumed.  daemon_close revokes these serials
    /// too, closing the take-reply/close race.
    pub(crate) fn validate_lease(&self, generation: u64, serial: u64) -> AxResult<()> {
        let state = self.state.lock();
        if !state.daemon_open
            || state.closed
            || state.generation != generation
            || !state
                .handed
                .iter()
                .any(|(g, s)| *g == generation && *s == serial)
        {
            return Err(LinuxError::EPIPE.into());
        }
        Ok(())
    }
    pub(crate) fn consume_lease(&self, generation: u64, serial: u64) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .handed
            .iter()
            .position(|(g, s)| *g == generation && *s == serial)
        {
            state.handed.remove(index);
        }
    }
    pub(crate) fn abandon_handoff(&self, generation: u64, serial: u64) {
        self.consume_lease(generation, serial);
        crate::keyring::revoke_nfs_gss_context(serial);
    }
    /// Atomically transfer a handed reply out of daemon-close revocation.
    /// From this point the RAII guard, not the queue, owns revocation.
    pub(crate) fn claim_handoff(
        self: &Arc<Self>,
        generation: u64,
        serial: u64,
    ) -> AxResult<HandedLeaseGuard> {
        let mut state = self.state.lock();
        if !state.daemon_open || state.closed || state.generation != generation {
            return Err(LinuxError::EPIPE.into());
        }
        let index = state
            .handed
            .iter()
            .position(|(g, s)| *g == generation && *s == serial)
            .ok_or(LinuxError::EPIPE)?;
        state.handed.remove(index);
        Ok(HandedLeaseGuard {
            queue: self.clone(),
            serial,
            active: true,
        })
    }

    /// Sleep on the same lifecycle wakeup that drives poll.  Daemon close is
    /// terminal for an outstanding auth_gss construction, matching Linux'
    /// `gss_pipe_release()` cancellation rather than leaving a mount worker
    /// parked behind an orphaned pipe message.
    pub(crate) fn wait_reply(&self, id: u64) -> AxResult<GssdReply> {
        crate::readiness::block_on_poll_set(&self.waiters, || {
            if let Some(reply) = self.take_reply(id) {
                return Ok(reply);
            }
            if self.state.lock().closed {
                return Err(LinuxError::EPIPE.into());
            }
            Err(AxError::WouldBlock)
        })
    }

    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        if state.closed {
            IoEvents::HANGUP
        } else {
            let mut events = IoEvents::WRITABLE;
            if state.pending.iter().any(|request| !request.claimed) {
                events |= IoEvents::READABLE;
            }
            events
        }
    }
}

static NEXT_UPCALL: AtomicU64 = AtomicU64::new(1);
static GLOBAL_GSSD_QUEUE: Mutex<Option<Arc<GssdQueue>>> = Mutex::new(None);
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
static GSSD_CLIENTS: Mutex<Vec<FsNameBuf>> = Mutex::new(Vec::new());

fn client_name(id: u64) -> AxResult<FsNameBuf> {
    FsNameBuf::from_vec(alloc::format!("clnt{id:x}").into_bytes())
        .map_err(|_| AxError::InvalidInput)
}

/// A mounted NFSv4.1 client registers one directory.  Lookup and getdents
/// share this registry, so a daemon never observes a fabricated fixed client
/// name after the real transport has gone away.
pub(crate) fn register_nfs_client() -> AxResult<FsNameBuf> {
    let name = client_name(NEXT_CLIENT.fetch_add(1, Ordering::Relaxed).max(1))?;
    let mut clients = GSSD_CLIENTS.lock();
    clients.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    clients.push(name.clone());
    Ok(name)
}

pub(crate) fn unregister_nfs_client(name: &FsName) {
    if name.as_bytes() == b"clnt0" {
        return;
    }
    let mut clients = GSSD_CLIENTS.lock();
    if let Some(index) = clients.iter().position(|client| client.as_name() == name) {
        clients.remove(index);
    }
}

struct RpcClientDirectoryOps {
    fs: Arc<SimpleFs>,
    queue: Arc<GssdQueue>,
}

impl SimpleDirOps for RpcClientDirectoryOps {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        let clients = GSSD_CLIENTS.lock();
        let mut names = Vec::new();
        names
            .try_reserve_exact(clients.len())
            .map_err(|_| VfsError::NoMemory)?;
        for name in clients.iter() {
            names.push(name.clone());
        }
        try_boxed_names(names.into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if !GSSD_CLIENTS
            .lock()
            .iter()
            .any(|client| client.as_name() == name)
        {
            return Err(VfsError::NotFound);
        }
        let mut entries = DirMapping::new();
        entries.add(
            "gssd",
            RpcPipeNode::new(self.fs.clone(), self.queue.clone()),
        );
        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(entries),
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

pub(crate) fn global_gssd_queue() -> AxResult<Arc<GssdQueue>> {
    let mut slot = GLOBAL_GSSD_QUEUE.lock();
    if let Some(queue) = slot.as_ref() {
        return Ok(queue.clone());
    }
    let queue = Arc::try_new(GssdQueue::new()).map_err(|_| AxError::NoMemory)?;
    *slot = Some(queue.clone());
    Ok(queue)
}

/// Per-daemon open-file-description state. Duplicated descriptors and fork
/// share this object, so only final OFD close tears down outstanding upcalls.
pub(crate) struct RpcPipeOpenHandle {
    node: SimpleFsNode,
    queue: Arc<GssdQueue>,
    readable: bool,
    writable: bool,
    closed: AtomicBool,
}

impl RpcPipeOpenHandle {
    fn try_new(
        fs: Arc<SimpleFs>,
        queue: Arc<GssdQueue>,
        readable: bool,
        writable: bool,
    ) -> VfsResult<Arc<Self>> {
        if !readable || !writable {
            return Err(VfsError::PermissionDenied);
        }
        queue.daemon_open().map_err(VfsError::from)?;
        Arc::try_new(Self {
            node: SimpleFsNode::try_new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )?,
            queue,
            readable,
            writable,
            closed: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn close_once(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.queue.daemon_close();
        }
    }

    /// Fault-aware stream read used by the Linux `FileLike` layer. A failed
    /// usercopy rolls the claim back, so rpc.gssd can retry the same record.
    pub(crate) fn read_user(&self, destination: &mut IoDst) -> AxResult<usize> {
        if !self.readable {
            return Err(AxError::BadFileDescriptor);
        }
        self.queue.read_upcall(destination)
    }

    /// Complete reply records are copied from userspace before publication.
    pub(crate) fn write_user(&self, source: &mut IoSrc) -> AxResult<usize> {
        if !self.writable {
            return Err(AxError::BadFileDescriptor);
        }
        self.queue.write_reply(source)
    }
}

impl Drop for RpcPipeOpenHandle {
    fn drop(&mut self) {
        self.close_once();
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for RpcPipeOpenHandle {
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata>;
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, data_only: bool) -> VfsResult<()>;
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        rpc_pipe_flags()
    }
}

impl FileNodeOps for RpcPipeOpenHandle {
    fn read_at(&self, destination: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if !self.readable {
            return Err(VfsError::BadFileDescriptor);
        }
        self.queue.read_upcall_slice(destination)
    }

    fn write_at(&self, source: &[u8], _offset: u64) -> VfsResult<usize> {
        if !self.writable {
            return Err(VfsError::BadFileDescriptor);
        }
        self.queue.write_reply_slice(source)
    }

    fn append(&self, source: &[u8]) -> VfsResult<(usize, u64)> {
        self.write_at(source, 0).map(|written| (written, 0))
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn set_symlink(&self, _target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn release_handle(&self) -> VfsResult<()> {
        self.close_once();
        Ok(())
    }
}

impl Pollable for RpcPipeOpenHandle {
    fn poll(&self) -> IoEvents {
        self.queue.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::HANGUP) {
            PollRegistration::single(&self.queue.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

struct RpcPipeNode {
    node: SimpleFsNode,
    fs: Arc<SimpleFs>,
    queue: Arc<GssdQueue>,
    user_data: NodeUserData,
}

impl RpcPipeNode {
    fn new(fs: Arc<SimpleFs>, queue: Arc<GssdQueue>) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs.clone(),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            ),
            fs,
            queue,
            user_data: NodeUserData::new(),
        })
    }
}

fn rpc_pipe_flags() -> NodeFlags {
    NodeFlags::NON_CACHEABLE
        | NodeFlags::STREAM
        | NodeFlags::NO_POSITIONED_READ
        | NodeFlags::NO_POSITIONED_WRITE
        | NodeFlags::NO_SEEK
}

#[inherit_methods(from = "self.node")]
impl NodeOps for RpcPipeNode {
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata>;
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, data_only: bool) -> VfsResult<()>;
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        rpc_pipe_flags()
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.user_data)
    }
}

impl FileNodeOps for RpcPipeNode {
    fn open_handle(
        &self,
        readable: bool,
        writable: bool,
        _flags: u32,
    ) -> VfsResult<Option<Arc<dyn FileNodeOps>>> {
        RpcPipeOpenHandle::try_new(self.fs.clone(), self.queue.clone(), readable, writable)
            .map(|handle| Some(handle as Arc<dyn FileNodeOps>))
    }

    fn read_at(&self, _destination: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn write_at(&self, _source: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn append(&self, _source: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn set_symlink(&self, _target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

impl Pollable for RpcPipeNode {
    fn poll(&self) -> IoEvents {
        self.queue.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::HANGUP) {
            PollRegistration::single(&self.queue.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

/// Creates an independent rpc_pipefs superblock. The global gssd rendezvous
/// survives mount namespace churn, while each mount owns its inode/dentry tree.
///
/// The client-directory shape is the Linux layout (`nfs/clnt*/gssd`), rather
/// than a device-like global `gssd` file. Client lookup is non-cacheable and
/// obtains its names from the shared registration table.
pub(crate) fn new_rpc_pipefs() -> AxResult<Filesystem> {
    let queue = global_gssd_queue()?;
    {
        let mut clients = GSSD_CLIENTS.lock();
        if clients.is_empty() {
            clients.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            clients.push(client_name(0)?);
        }
    }
    Ok(SimpleFs::new_with(
        "rpc_pipefs".into(),
        0x6759_6969,
        move |fs| {
            let mut root = DirMapping::new();
            root.add(
                "nfs",
                SimpleDir::new_maker(
                    fs.clone(),
                    Arc::new(RpcClientDirectoryOps {
                        fs: fs.clone(),
                        queue,
                    }),
                ),
            );
            SimpleDir::new_maker(fs, Arc::new(root))
        },
    ))
}

impl GssdReply {
    /// Linux auth_gss downcall: native uid, native timeout/window, a wire
    /// context netobj, imported mechanism context, then optional acceptor.
    /// Parse every length before publishing the reply to a waiter.
    fn decode(bytes: &[u8]) -> AxResult<Self> {
        if !(UID_PREFIX..=RPC_GSSD_MAX_MESSAGE).contains(&bytes.len()) {
            return Err(AxError::InvalidInput);
        }
        let uid = u32::from_ne_bytes(
            bytes[..UID_PREFIX]
                .try_into()
                .map_err(|_| AxError::InvalidInput)?,
        );
        let mut at = UID_PREFIX;
        fn word(bytes: &[u8], at: &mut usize) -> AxResult<u32> {
            let end = at.checked_add(4).ok_or(AxError::InvalidInput)?;
            let raw = bytes.get(*at..end).ok_or(AxError::InvalidInput)?;
            *at = end;
            Ok(u32::from_ne_bytes(
                raw.try_into().map_err(|_| AxError::InvalidInput)?,
            ))
        }
        fn netobj(bytes: &[u8], at: &mut usize) -> AxResult<Vec<u8>> {
            let length = word(bytes, at)? as usize;
            let end = at.checked_add(length).ok_or(AxError::InvalidInput)?;
            let raw = bytes.get(*at..end).ok_or(AxError::InvalidInput)?;
            *at = end;
            let mut copy = Vec::new();
            copy.try_reserve_exact(raw.len())
                .map_err(|_| AxError::NoMemory)?;
            copy.extend_from_slice(raw);
            Ok(copy)
        }
        let timeout_seconds = word(bytes, &mut at)?;
        let window_size = word(bytes, &mut at)?;
        // Linux returns EACCES (or EKEYEXPIRED internally) through a zero
        // window. This queue has no errno side channel, so reject it rather
        // than converting it into a malformed usable credential.
        if timeout_seconds == 0 || window_size == 0 || window_size > 64 {
            return Err(LinuxError::EACCES.into());
        }
        let mut wire_context = ZeroizingBytes(netobj(bytes, &mut at)?);
        let imported_length = word(bytes, &mut at)? as usize;
        let imported_end = at
            .checked_add(imported_length)
            .ok_or(AxError::InvalidInput)?;
        let imported = bytes.get(at..imported_end).ok_or(AxError::InvalidInput)?;
        at = imported_end;
        let mut imported_context = ZeroizingBytes(Vec::new());
        imported_context
            .0
            .try_reserve_exact(imported.len())
            .map_err(|_| AxError::NoMemory)?;
        imported_context.0.extend_from_slice(imported);
        // rpc.gssd's opaque import blob is not generic GSS data.  The only
        // enabled client mechanism is krb5, whose Linux v2 import shape is
        // validated now, before it enters the protected context store.
        axfs::Krb5ImportedContext::parse(&imported_context.0).map_err(|_| AxError::InvalidInput)?;
        let acceptor = if at == bytes.len() {
            Vec::new()
        } else {
            netobj(bytes, &mut at)?
        };
        if at != bytes.len() {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            id: 0,
            uid,
            daemon_generation: 0,
            target: Vec::new(),
            service: Vec::new(),
            context: GssContext {
                key_serial: 0,
                timeout_seconds,
                window_size,
                wire_context: core::mem::take(&mut wire_context.0),
                imported_context: core::mem::take(&mut imported_context.0),
                acceptor,
            },
        })
    }
}
