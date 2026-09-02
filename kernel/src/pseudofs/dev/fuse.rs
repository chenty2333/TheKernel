//! FUSE device transport.
//!
//! This is deliberately a kernel-side protocol endpoint, rather than a
//! userspace library shim.  Every operation issued by a mounted FUSE
//! filesystem is represented by one retained request until the daemon's
//! reply, an interrupt, or connection teardown supplies its terminal state.

use alloc::{
    borrow::Cow,
    collections::{BTreeMap, vec_deque::VecDeque},
    format,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    any::Any,
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FileAttr, FileAttrProvider, FileLock, FileNode, FileNodeOps, FileRangeOperation,
    FileRangeRequest, Filesystem, FilesystemOps, FsName, FsNameBuf, Location, LockOps, Metadata,
    MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps, NodePermission, NodeType, NodeUserData,
    NowaitAdmission, ObjectKey, Reference, RenameRequest, StatFs, Timestamp, UnlinkRequest,
    VfsError, VfsResult, WeakDirEntry, XattrProvider, XattrSetMode,
};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axtask::current;
use spin::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, anon_inode_stat},
    pseudofs::{DeviceOpen, DeviceOps},
    readiness::{block_on_poll_io, block_on_poll_io_interruptible_if},
    task::{AsThread, has_pending_fatal_signal},
};

pub const FUSE_KERNEL_MAJOR: u32 = 7;
pub const FUSE_KERNEL_MINOR: u32 = 45;
pub const FUSE_ROOT_ID: u64 = 1;

pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_SYMLINK: u32 = 6;
pub const FUSE_MKNOD: u32 = 8;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_LINK: u32 = 13;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_FSYNC_FDATASYNC: u32 = 1;
pub const FUSE_GETLK: u32 = 31;
pub const FUSE_SETLK: u32 = 32;
pub const FUSE_SETLKW: u32 = 33;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_INTERRUPT: u32 = 36;
/// Set on an interrupt packet's `unique`; the body carries the unmodified
/// unique of the request being interrupted.
pub const FUSE_INT_REQ_BIT: u64 = 1 << 63;
pub const FUSE_BMAP: u32 = 37;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_IOCTL: u32 = 39;
pub const FUSE_POLL: u32 = 40;
pub const FUSE_NOTIFY_REPLY: u32 = 41;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_FALLOCATE: u32 = 43;
pub const FUSE_READDIRPLUS: u32 = 44;
pub const FUSE_RENAME2: u32 = 45;
pub const FUSE_LSEEK: u32 = 46;
pub const FUSE_COPY_FILE_RANGE: u32 = 47;
pub const FUSE_SYNCFS: u32 = 50;

pub const FUSE_NOTIFY_POLL: u32 = 1;
pub const FUSE_NOTIFY_INVAL_INODE: u32 = 2;
pub const FUSE_NOTIFY_INVAL_ENTRY: u32 = 3;
pub const FUSE_NOTIFY_STORE: u32 = 4;
pub const FUSE_NOTIFY_RETRIEVE: u32 = 5;
pub const FUSE_NOTIFY_DELETE: u32 = 6;

pub const FUSE_ASYNC_READ: u64 = 1 << 0;
pub const FUSE_BIG_WRITES: u64 = 1 << 5;
pub const FUSE_AUTO_INVAL_DATA: u64 = 1 << 12;
pub const FUSE_WRITEBACK_CACHE: u64 = 1 << 16;
// This is a daemon-negotiated INIT capability, not a feature the kernel can
// safely infer from the protocol minor alone.
pub const FUSE_DO_READDIRPLUS: u64 = 1 << 13;
pub const FUSE_POSIX_ACL: u64 = 1 << 20;
pub const FUSE_ABORT_ERROR: u64 = 1 << 21;
pub const FUSE_MAX_PAGES: u64 = 1 << 22;
pub const FUSE_EXPLICIT_INVAL_DATA: u64 = 1 << 25;
pub const FUSE_ALLOW_IDMAP: u64 = 1 << 40;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_PENDING_REQUESTS: usize = 4096;
const MAX_FUSE_NOTIFICATIONS: usize = 1024;
const MAX_FUSE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FUSE_INODE_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FUSE_INODE_CACHE_RANGES: usize = 4096;
const MAX_FUSE_DENTRY_CACHE_ENTRIES: usize = 8192;
const FUSE_CACHE_NODE_OVERHEAD: usize = 128;
const FUSE_CACHE_RANGE_OVERHEAD: usize = 128;
const FUSE_CACHE_DENTRY_OVERHEAD: usize = 128;
const MAX_FUSE_NOTIFICATION_BYTES: usize = 8 * 1024 * 1024;
const MAX_FUSE_DEFERRED_NOTIFICATION_BYTES: usize = MAX_REQUEST_BYTES;

fn fuse_range_cost(range: &FuseCachedRange) -> usize {
    FUSE_CACHE_RANGE_OVERHEAD.saturating_add(range.data.len())
}
fn fuse_inode_cost(inode: &FuseInodeCache) -> usize {
    FUSE_CACHE_NODE_OVERHEAD.saturating_add(inode.ranges.iter().map(fuse_range_cost).sum::<usize>())
}
fn fuse_dentry_cost(name: &[u8]) -> usize {
    FUSE_CACHE_DENTRY_OVERHEAD.saturating_add(name.len())
}
fn fuse_notification_cost(notification: &FuseNotification) -> usize {
    match notification {
        FuseNotification::InvalidateInode { .. } => 24,
        FuseNotification::InvalidateEntry { name, .. } => 16 + name.len(),
        FuseNotification::Delete { name, .. } => 24 + name.len(),
        FuseNotification::Store { data, .. } => 24 + data.len(),
    }
}
static NEXT_FUSE_FILESYSTEM_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FUSE_DESTROY_TICKET: AtomicU64 = AtomicU64::new(1);
static NEXT_FUSE_DESTROY_PARTICIPANT: AtomicU64 = AtomicU64::new(1);
static NEXT_FUSE_TEARDOWN_RECEIPT: AtomicU64 = AtomicU64::new(1);
static FUSE_MOUNT_CONNECTIONS: Mutex<Vec<(u64, alloc::sync::Weak<FuseConnection>)>> =
    Mutex::new(Vec::new());
// Mount teardown prepares before VFS commit.  Record those pending removals
// so two concurrent unmounts of shared namespace/bind replicas cannot both
// mistake the other as a surviving mounted instance and omit DESTROY.
static FUSE_PENDING_MOUNT_REMOVALS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
const FUSE_IOCTL_UNRESTRICTED: u32 = 1 << 1;
const FUSE_IOCTL_RETRY: u32 = 1 << 2;
const FUSE_IOCTL_DIR: u32 = 1 << 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct InHeader {
    len: u32,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    total_extlen: u16,
    padding: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OutHeader {
    len: u32,
    error: i32,
    unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InitIn {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    flags2: u32,
    unused: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InitOut {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    max_background: u16,
    congestion_threshold: u16,
    max_write: u32,
    time_gran: u32,
    max_pages: u16,
    map_alignment: u16,
    flags2: u32,
    max_stack_depth: u32,
    request_timeout: u16,
    unused: [u16; 11],
}

const _: () = assert!(size_of::<InHeader>() == 40);
const _: () = assert!(size_of::<OutHeader>() == 16);
const _: () = assert!(size_of::<InitIn>() == 64);
const _: () = assert!(size_of::<InitOut>() == 64);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FuseInit {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) max_readahead: u32,
    pub(crate) max_write: u32,
    pub(crate) max_background: u16,
    pub(crate) congestion_threshold: u16,
    pub(crate) time_gran: u32,
    pub(crate) max_pages: u16,
    pub(crate) flags: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum FuseReply {
    Data(Vec<u8>),
    Error(i32),
    Cancelled,
}

#[derive(Clone, Debug)]
pub(crate) enum FuseNotification {
    InvalidateInode {
        nodeid: u64,
        offset: i64,
        length: i64,
    },
    InvalidateEntry {
        parent: u64,
        name: Vec<u8>,
    },
    Delete {
        parent: u64,
        child: u64,
        name: Vec<u8>,
    },
    Store {
        nodeid: u64,
        offset: u64,
        data: Vec<u8>,
    },
}

#[derive(Clone)]
struct FuseCachedRange {
    offset: u64,
    data: Vec<u8>,
    epoch: u64,
    inflight: bool,
    /// A dirty range is owned by the exact open handle which admitted the
    /// write.  It must be written through that handle before RELEASE makes
    /// the handle invalid.
    dirty_fh: Option<u64>,
    owner: Option<Arc<FuseHandleLease>>,
}

struct FuseHandleLease {
    nodeid: u64,
    fh: u64,
    generation: u64,
    live: AtomicBool,
}

#[derive(Default)]
struct FuseInodeCache {
    ranges: Vec<FuseCachedRange>,
    writeback_epoch: u64,
}

struct FuseCachedDentry {
    entry: WeakDirEntry,
    deadline: u64,
    epoch: u64,
}

#[derive(Default)]
struct FuseCache {
    inodes: BTreeMap<u64, FuseInodeCache>,
    dentries: BTreeMap<(u64, Vec<u8>), FuseCachedDentry>,
    bytes: usize,
}

struct FusePollState {
    events: Mutex<IoEvents>,
    needs_query: AtomicBool,
    waiters: PollSet,
}

impl FusePollState {
    fn try_new() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            events: Mutex::new(IoEvents::empty()),
            needs_query: AtomicBool::new(true),
            waiters: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn invalidate(&self) {
        self.needs_query.store(true, Ordering::Release);
        self.waiters.wake();
    }

    fn publish(&self, events: IoEvents) {
        *self.events.lock() = events;
        self.needs_query.store(false, Ordering::Release);
        self.waiters.wake();
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RequestState {
    Queued,
    Delivered,
    Replied,
    Cancelled,
}

struct Request {
    unique: u64,
    opcode: u32,
    bytes: Vec<u8>,
    state: RequestState,
    reply: Option<FuseReply>,
    expects_reply: bool,
    /// The submitting syscall returned EINTR after the daemon had already
    /// received this request.  Its eventual original reply is still consumed
    /// for transport cleanup, but has no waiter to publish to.
    waiter_interrupted: bool,
    /// Set only after the complete packet has crossed into daemon custody.
    /// A provisional terminal teardown can then distinguish a locally
    /// rejected OPEN from a delivered request with an ambiguous outcome.
    delivery: Option<Arc<AtomicBool>>,
}

struct ConnectionState {
    requests: VecDeque<Request>,
    /// Interrupts are not ordinary no-reply requests.  They borrow the
    /// original request's lifetime and are assembled directly into the
    /// daemon read buffer, so a signal cannot strand an allocation-owned
    /// pseudo-request.  This queue is serviced ahead of normal and forget
    /// traffic, but never ahead of the terminal DESTROY outbox.
    interrupts: VecDeque<u64>,
    /// A daemon which replies `-ENOSYS` to an interrupt has permanently
    /// declined this optional protocol feature for this connection.
    no_interrupt: bool,
    /// Terminal packets may reserve their queue position before an operation
    /// is sent. Ordinary requests leave these slots alone so a malformed
    /// successful CREATE can always enqueue DESTROY and retire an fh whose
    /// identity could not be decoded.
    terminal_reservations: usize,
    notifications: VecDeque<FuseNotification>,
    notification_bytes: usize,
    deferred_notification: Option<(FuseNotification, usize)>,
    /// A notification remains charged while it is being applied.  This makes
    /// a failed apply/requeue an ownership transfer, never a drop-and-try-to-
    /// re-admit allocation that could lose the notification under pressure.
    processing_notification: Option<(FuseNotification, usize)>,
    init: Option<FuseInit>,
    dead: bool,
    /// A successful namespace unmount queues DESTROY before it releases the
    /// mount registry.  It is deliberately distinct from `dead`: the daemon
    /// must still be able to read DESTROY, while descriptor close/abort may
    /// still tear the connection down immediately.
    destroy_queued: bool,
    destroy_prepared: Option<(u64, Request)>,
    lazy_destroy_participant: Option<u64>,
    destroy_participants: Vec<(u64, u8)>, // prepared, committed, cancelled
    final_unmount_seen: bool,
    destroy: Option<Request>,
    forget_nodes: Vec<u64>,
    /// Linux's bounded forget preference.  Under ordinary request load this
    /// permits a short forget burst, then yields to pending traffic; with no
    /// pending request forgets drain without artificial delay.
    forget_batch: i32,
    poll_states: BTreeMap<u64, alloc::sync::Weak<FusePollState>>,
    known_nodes: BTreeMap<u64, u32>,
    cache: Option<alloc::sync::Weak<Mutex<FuseCache>>>,
}

/// A daemon connection.  It is shared by `/dev/fuse`, the mounted superblock,
/// and all per-open FUSE file handles; closing the daemon descriptor tears the
/// whole graph down and wakes every waiter.
pub(crate) struct FuseConnection {
    next_unique: AtomicU64,
    state: Mutex<ConnectionState>,
    waiters: PollSet,
}

impl FuseConnection {
    pub(crate) fn try_new() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            next_unique: AtomicU64::new(1),
            state: Mutex::new(ConnectionState {
                requests: VecDeque::new(),
                interrupts: VecDeque::new(),
                no_interrupt: false,
                terminal_reservations: 0,
                notifications: VecDeque::new(),
                notification_bytes: 0,
                deferred_notification: None,
                processing_notification: None,
                init: None,
                dead: false,
                destroy_queued: false,
                destroy_prepared: None,
                lazy_destroy_participant: None,
                destroy_participants: Vec::new(),
                final_unmount_seen: false,
                destroy: None,
                forget_nodes: Vec::new(),
                forget_batch: 16,
                poll_states: BTreeMap::new(),
                known_nodes: BTreeMap::new(),
                cache: None,
            }),
            waiters: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn init(&self) -> Option<FuseInit> {
        self.state.lock().init
    }
    pub(crate) fn is_dead(&self) -> bool {
        self.state.lock().dead
    }

    /// Non-mutating FUSE request admission for RWF_NOWAIT.  The subsequent
    /// request still competes normally, but a dead connection or saturated
    /// daemon queue is rejected before it can enqueue or wait.
    fn nowait_request_admit(&self) -> bool {
        let Some(state) = self.state.try_lock() else {
            return false;
        };
        !state.dead
            && !state.destroy_queued
            && state.init.is_some()
            && state
                .requests
                .len()
                .saturating_add(state.terminal_reservations)
                < MAX_PENDING_REQUESTS
    }

    fn wake(&self) {
        self.waiters.wake();
    }

    fn append_pod<T: Copy>(bytes: &mut Vec<u8>, value: &T) -> AxResult<()> {
        let raw = unsafe {
            core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>())
        };
        bytes
            .try_reserve(raw.len())
            .map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(raw);
        Ok(())
    }

    fn write_pod<T: Copy>(dst: &mut IoDst, value: &T) -> AxResult<()> {
        let raw = unsafe {
            core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>())
        };
        dst.write(raw).map(|_| ())
    }

    fn build_request(
        &self,
        unique: u64,
        opcode: u32,
        nodeid: u64,
        body: &[u8],
    ) -> AxResult<Vec<u8>> {
        let len = size_of::<InHeader>()
            .checked_add(body.len())
            .ok_or(AxError::InvalidInput)?;
        if len > MAX_REQUEST_BYTES {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve(len).map_err(|_| AxError::NoMemory)?;
        // Capture identity once while the operation is admitted.  This keeps
        // a daemon request stable across blocking/retry rather than sampling
        // credentials after a concurrent setresuid or exec transition.
        let task = current();
        let thread = task.as_thread();
        let cred = thread.current_cred();
        let header = InHeader {
            len: len as u32,
            opcode,
            unique,
            nodeid,
            uid: cred.ids().fsuid.into_raw(),
            gid: cred.ids().fsgid.into_raw(),
            pid: thread.proc_data.proc.pid() as u32,
            total_extlen: 0,
            padding: 0,
        };
        Self::append_pod(&mut bytes, &header)?;
        bytes.extend_from_slice(body);
        Ok(bytes)
    }

    /// Queues a protocol operation and waits on the shared readiness engine.
    /// The caller receives the daemon error number unmodified, avoiding an
    /// accidental collapse of remote errno semantics into a generic VFS error.
    pub(crate) fn request(&self, opcode: u32, nodeid: u64, body: &[u8]) -> AxResult<FuseReply> {
        self.request_tracked(opcode, nodeid, body, None)
    }

    /// `delivery` becomes true only once `/dev/fuse` has written this packet
    /// to its daemon reader.  It is intentionally a transport receipt rather
    /// than an inference from a reply: interrupted or malformed replies are
    /// precisely the cases where that distinction matters.
    fn request_tracked(
        &self,
        opcode: u32,
        nodeid: u64,
        body: &[u8],
        delivery: Option<Arc<AtomicBool>>,
    ) -> AxResult<FuseReply> {
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed).max(1);
        let bytes = self.build_request(unique, opcode, nodeid, body)?;
        {
            let mut state = self.state.lock();
            if state.dead || state.destroy_queued {
                return Err(LinuxError::ENODEV.into());
            }
            if state
                .requests
                .len()
                .saturating_add(state.terminal_reservations)
                >= MAX_PENDING_REQUESTS
            {
                return Err(AxError::WouldBlock);
            }
            state
                .requests
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            state.requests.push_back(Request {
                unique,
                opcode,
                bytes,
                state: RequestState::Queued,
                reply: None,
                expects_reply: true,
                waiter_interrupted: false,
                delivery,
            });
        }
        self.wake();
        // An interrupted syscall only queues FUSE_INTERRUPT after its
        // original request crossed into daemon custody.  Unlike an ordinary
        // no-reply packet, the interrupt retains the original request in the
        // processing set: the daemon's late original reply still owns its
        // cleanup and is accepted normally.
        block_on_poll_io_interruptible_if(
            self,
            IoEvents::READABLE,
            false,
            || self.take_reply(unique),
            || self.interrupt_request(unique, has_pending_fatal_signal(current().as_thread())),
        )
    }

    /// Reserves one already-materialized, no-reply teardown packet. The
    /// packet and queue capacity exist before a side-effecting operation is
    /// issued; activating DESTROY after a malformed success is therefore
    /// allocation-free.
    fn prepare_create_teardown(&self, opcode: u32, nodeid: u64, body: &[u8]) -> AxResult<Request> {
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed).max(1);
        let bytes = self.build_request(unique, opcode, nodeid, body)?;
        let mut state = self.state.lock();
        if state.dead || state.destroy_queued {
            return Err(LinuxError::ENODEV.into());
        }
        if state
            .requests
            .len()
            .saturating_add(state.terminal_reservations)
            >= MAX_PENDING_REQUESTS
        {
            return Err(AxError::WouldBlock);
        }
        let terminal_reservations = state.terminal_reservations;
        state
            .requests
            .try_reserve(terminal_reservations.saturating_add(1))
            .map_err(|_| AxError::NoMemory)?;
        state.terminal_reservations += 1;
        Ok(Request {
            unique,
            opcode,
            bytes,
            state: RequestState::Queued,
            reply: None,
            expects_reply: false,
            waiter_interrupted: false,
            delivery: None,
        })
    }

    fn discard_prepared_teardown(&self) {
        let mut state = self.state.lock();
        state.terminal_reservations = state.terminal_reservations.saturating_sub(1);
    }

    /// Transfers a materialized terminal packet out of pre-publication
    /// admission.  The owner may later activate that packet through the
    /// independent `destroy` slot without consuming ordinary request queue
    /// capacity while a published OFD remains open.
    fn release_prepared_teardown_slot(&self) {
        let mut state = self.state.lock();
        state.terminal_reservations = state.terminal_reservations.saturating_sub(1);
    }

    fn activate_prepared_destroy(&self, request: Request) {
        let mut state = self.state.lock();
        state.terminal_reservations = state.terminal_reservations.saturating_sub(1);
        if !state.dead && state.destroy.is_none() {
            state.destroy_queued = true;
            state.destroy = Some(request);
            drop(state);
            self.wake();
        }
    }

    /// Activates a packet whose pre-publication queue reservation has already
    /// been released to avoid charging every live OFD against normal request
    /// admission.  The terminal `destroy` outbox is separately materialized.
    fn activate_materialized_destroy(&self, request: Request) {
        let mut state = self.state.lock();
        if !state.dead && state.destroy.is_none() {
            state.destroy_queued = true;
            state.destroy = Some(request);
            drop(state);
            self.wake();
        }
    }

    /// Handles a signal against one waiter.  Reply, signal, and connection
    /// abort are linearized by `state`.  A queued request is removed because
    /// it was never visible to the daemon.  A delivered request remains in
    /// `requests` until its original reply or teardown, while this method
    /// records only its target unique in the high-priority interrupt queue.
    fn interrupt_request(&self, target: u64, fatal_signal: bool) -> bool {
        let mut state = self.state.lock();
        if state.dead {
            return false;
        }
        let Some(index) = state
            .requests
            .iter()
            .position(|request| request.unique == target)
        else {
            return false;
        };
        match state.requests[index].state {
            RequestState::Queued => {
                if state.no_interrupt && !fatal_signal {
                    return false;
                }
                // This request has never crossed into daemon custody.  Its
                // sole waiter is returning EINTR, so removing it here is the
                // terminal transition and must not emit FUSE_INTERRUPT.
                state.requests.remove(index);
                drop(state);
                self.wake();
                return true;
            }
            RequestState::Delivered if state.no_interrupt => {
                // Once the daemon has declined FUSE_INTERRUPT, Linux waits
                // a delivered request through its original reply even for a
                // fatal signal.  Only a still-pending request can be removed
                // locally in that mode.
                return false;
            }
            RequestState::Delivered => {}
            RequestState::Replied | RequestState::Cancelled => return false,
        }
        state.requests[index].waiter_interrupted = true;
        // FUSE_INTERRUPT itself has no separately allocated request object.
        // Failure to grow this bookkeeping queue does not alter the original
        // request's ownership: its later reply or abort still retires it.
        if !state.interrupts.iter().any(|unique| *unique == target)
            && state.interrupts.try_reserve(1).is_ok()
        {
            state.interrupts.push_back(target);
        }
        drop(state);
        self.wake();
        true
    }

    fn queue_forget(&self, nodeid: u64) {
        let mut state = self.state.lock();
        if state.dead {
            return;
        };
        // Bound destructor-side memory.  A full batch is materialized by the
        // next daemon read; an overflow falls back to an individual forget.
        if state.forget_nodes.len() < 256 {
            if state.forget_nodes.try_reserve(1).is_ok() {
                state.forget_nodes.push(nodeid);
                drop(state);
                self.wake();
                return;
            }
        }
        drop(state);
        self.queue_forget_one(nodeid);
    }
    fn queue_forget_one(&self, nodeid: u64) {
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed).max(1);
        let body = 1u64.to_ne_bytes();
        let Ok(bytes) = self.build_request(unique, FUSE_FORGET, nodeid, &body) else {
            return;
        };
        let mut state = self.state.lock();
        if state.dead
            || state
                .requests
                .len()
                .saturating_add(state.terminal_reservations)
                >= MAX_PENDING_REQUESTS
        {
            return;
        }
        if state.requests.try_reserve(1).is_err() {
            return;
        }
        state.requests.push_back(Request {
            unique,
            opcode: FUSE_FORGET,
            bytes,
            state: RequestState::Queued,
            reply: None,
            expects_reply: false,
            waiter_interrupted: false,
            delivery: None,
        });
        drop(state);
        self.wake();
    }

    /// Converts a completed protocol reply into the kernel's errno domain.
    /// Keep this at the transport boundary so every FUSE provider operation
    /// preserves daemon-selected errors instead of silently reporting EIO.
    pub(crate) fn reply_data(reply: FuseReply) -> AxResult<Vec<u8>> {
        match reply {
            FuseReply::Data(data) => Ok(data),
            FuseReply::Error(errno) => Err(LinuxError::try_from(errno)
                .unwrap_or(LinuxError::EIO)
                .into()),
            FuseReply::Cancelled => Err(LinuxError::ENODEV.into()),
        }
    }

    /// Sends INIT exactly once.  FUSE's major-version retry is handled by
    /// asking the daemon to reply to this same operation; a peer with an
    /// incompatible major version is rejected before a superblock exists.
    pub(crate) fn negotiate(&self) -> AxResult<FuseInit> {
        if let Some(init) = self.init() {
            return Ok(init);
        }
        let input = InitIn {
            major: FUSE_KERNEL_MAJOR,
            minor: FUSE_KERNEL_MINOR,
            max_readahead: 1024 * 1024,
            // POSIX ACL is opt-in: the daemon's returned flag remains the
            // authoritative capability, but omitting it here makes a capable
            // server indistinguishable from one that cannot honor create-time
            // ACL semantics.
            // These cache bits are offered only because the mounted
            // superblock now has byte-ranged shared data/dentry/attribute
            // state and handles all notification variants below.  The daemon
            // reply remains the gate; an unnegotiated bit never changes I/O.
            flags: (FUSE_ASYNC_READ
                | FUSE_BIG_WRITES
                | FUSE_AUTO_INVAL_DATA
                | FUSE_WRITEBACK_CACHE
                | FUSE_MAX_PAGES
                | FUSE_POSIX_ACL
                | FUSE_EXPLICIT_INVAL_DATA) as u32,
            flags2: ((FUSE_ALLOW_IDMAP >> 32) & u64::from(u32::MAX)) as u32,
            unused: [0; 11],
        };
        let body = unsafe {
            core::slice::from_raw_parts((&input as *const InitIn).cast::<u8>(), size_of::<InitIn>())
        };
        let reply = Self::reply_data(self.request(FUSE_INIT, FUSE_ROOT_ID, body)?)?;
        if reply.len() < 8 {
            return Err(LinuxError::EPROTO.into());
        }
        let mut output = InitOut::default();
        let n = reply.len().min(size_of::<InitOut>());
        unsafe {
            core::ptr::copy_nonoverlapping(
                reply.as_ptr(),
                (&mut output as *mut InitOut).cast::<u8>(),
                n,
            );
        }
        if output.major != FUSE_KERNEL_MAJOR || output.minor == 0 {
            return Err(LinuxError::EPROTO.into());
        }
        let init = FuseInit {
            major: output.major,
            minor: output.minor.min(FUSE_KERNEL_MINOR),
            max_readahead: output.max_readahead,
            max_write: output.max_write.max(4096).min(MAX_REQUEST_BYTES as u32),
            max_background: output.max_background,
            congestion_threshold: output.congestion_threshold,
            time_gran: output.time_gran,
            max_pages: output.max_pages,
            flags: u64::from(output.flags) | (u64::from(output.flags2) << 32),
        };
        let mut state = self.state.lock();
        if state.dead {
            return Err(LinuxError::ENODEV.into());
        }
        state.init = Some(init);
        drop(state);
        self.wake();
        Ok(init)
    }

    fn take_reply(&self, unique: u64) -> AxResult<FuseReply> {
        let mut state = self.state.lock();
        let Some(index) = state
            .requests
            .iter()
            .position(|request| request.unique == unique)
        else {
            return Err(LinuxError::ENODEV.into());
        };
        let dead = state.dead;
        let request = &mut state.requests[index];
        match request.state {
            RequestState::Replied => Ok(state
                .requests
                .remove(index)
                .expect("indexed request")
                .reply
                .expect("reply state")),
            RequestState::Cancelled => {
                state.requests.remove(index);
                Ok(FuseReply::Cancelled)
            }
            RequestState::Queued | RequestState::Delivered if dead => {
                state.requests.remove(index);
                Ok(FuseReply::Cancelled)
            }
            RequestState::Queued | RequestState::Delivered => Err(AxError::WouldBlock),
        }
    }

    /// Dequeues an on-demand FUSE_INTERRUPT packet.  The target stays on the
    /// processing request list; only this short-lived queue entry is removed.
    /// A stale entry is harmless (for example after a racing original reply).
    fn dequeue_interrupt(state: &mut ConnectionState, dst: &mut IoDst) -> AxResult<Option<usize>> {
        loop {
            let Some(&target) = state.interrupts.front() else {
                return Ok(None);
            };
            let live = state.requests.iter().any(|request| {
                request.unique == target
                    && request.state == RequestState::Delivered
                    && request.reply.is_none()
            });
            if !live {
                state.interrupts.pop_front();
                continue;
            }
            let header = InHeader {
                len: (size_of::<InHeader>() + size_of::<u64>()) as u32,
                opcode: FUSE_INTERRUPT,
                unique: target | FUSE_INT_REQ_BIT,
                nodeid: 0,
                uid: 0,
                gid: 0,
                pid: 0,
                total_extlen: 0,
                padding: 0,
            };
            if dst.remaining_mut() < header.len as usize {
                return Err(AxError::InvalidInput);
            }
            Self::write_pod(dst, &header)?;
            Self::write_pod(dst, &target)?;
            state.interrupts.pop_front();
            return Ok(Some(header.len as usize));
        }
    }

    fn forget_preferred(state: &mut ConnectionState) -> bool {
        if state.forget_nodes.is_empty() {
            return false;
        }
        if !state
            .requests
            .iter()
            .any(|request| request.state == RequestState::Queued)
        {
            return true;
        }
        let send_forget = state.forget_batch > 0;
        state.forget_batch -= 1;
        if !send_forget && state.forget_batch <= -8 {
            state.forget_batch = 16;
        }
        send_forget
    }

    fn dequeue_forget(&self, state: &mut ConnectionState, dst: &mut IoDst) -> AxResult<usize> {
        let fixed = size_of::<InHeader>() + 8;
        let max = dst
            .remaining_mut()
            .checked_sub(fixed)
            .map(|room| room / 16)
            .unwrap_or(0);
        if max == 0 {
            return Err(AxError::InvalidInput);
        }
        let count = state.forget_nodes.len().min(max);
        let mut body = Vec::new();
        body.try_reserve(8 + count * 16)
            .map_err(|_| AxError::NoMemory)?;
        body.extend_from_slice(&(count as u32).to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        for nodeid in state.forget_nodes.iter().take(count) {
            body.extend_from_slice(&nodeid.to_ne_bytes());
            body.extend_from_slice(&1u64.to_ne_bytes());
        }
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed).max(1);
        let bytes = self.build_request(unique, FUSE_BATCH_FORGET, 0, &body)?;
        dst.write(&bytes)?;
        state.forget_nodes.drain(..count);
        Ok(bytes.len())
    }

    fn dequeue_for_daemon(&self, dst: &mut IoDst) -> AxResult<usize> {
        let mut state = self.state.lock();
        if state.dead {
            return Err(LinuxError::ENODEV.into());
        }
        if let Some(request) = state.destroy.as_ref() {
            if dst.remaining_mut() < request.bytes.len() {
                return Err(AxError::InvalidInput);
            }
            dst.write(&request.bytes)?;
            let len = request.bytes.len();
            state.destroy.take();
            // DESTROY is terminal for this session.  Do not let a daemon
            // observe a later normal packet after it has been instructed to
            // discard filesystem state.
            state.dead = true;
            state.interrupts.clear();
            for request in &mut state.requests {
                if request.reply.is_none() {
                    request.state = RequestState::Cancelled;
                    request.reply = Some(FuseReply::Cancelled);
                }
            }
            drop(state);
            self.wake();
            return Ok(len);
        }
        if let Some(len) = Self::dequeue_interrupt(&mut state, dst)? {
            return Ok(len);
        }
        if Self::forget_preferred(&mut state) {
            return self.dequeue_forget(&mut state, dst);
        }
        let Some(index) = state
            .requests
            .iter()
            .position(|request| request.state == RequestState::Queued)
        else {
            return Err(AxError::WouldBlock);
        };
        let request = &mut state.requests[index];
        if dst.remaining_mut() < request.bytes.len() {
            return Err(AxError::InvalidInput);
        }
        dst.write(&request.bytes)?;
        let len = request.bytes.len();
        if let Some(delivery) = &request.delivery {
            delivery.store(true, Ordering::Release);
        }
        if request.expects_reply {
            request.state = RequestState::Delivered;
        } else {
            state.requests.remove(index);
        }
        Ok(len)
    }

    /// Nonblocking `/dev/fuse` dequeue.  This owns the real connection-state
    /// lock for the complete consume transition; unlike a readiness probe it
    /// never falls through to the blocking legacy dequeue on contention.
    fn try_dequeue_for_daemon(&self, dst: &mut IoDst) -> AxResult<usize> {
        let Some(mut state) = self.state.try_lock() else {
            return Err(AxError::WouldBlock);
        };
        if state.dead {
            return Err(LinuxError::ENODEV.into());
        }
        if let Some(request) = state.destroy.as_ref() {
            if dst.remaining_mut() < request.bytes.len() {
                return Err(AxError::InvalidInput);
            }
            dst.write(&request.bytes)?;
            let len = request.bytes.len();
            state.destroy.take();
            state.dead = true;
            state.interrupts.clear();
            for request in &mut state.requests {
                if request.reply.is_none() {
                    request.state = RequestState::Cancelled;
                    request.reply = Some(FuseReply::Cancelled);
                }
            }
            drop(state);
            self.wake();
            return Ok(len);
        }
        if let Some(len) = Self::dequeue_interrupt(&mut state, dst)? {
            return Ok(len);
        }
        if Self::forget_preferred(&mut state) {
            return self.dequeue_forget(&mut state, dst);
        }
        let Some(index) = state
            .requests
            .iter()
            .position(|request| request.state == RequestState::Queued)
        else {
            return Err(AxError::WouldBlock);
        };
        let request = &mut state.requests[index];
        if dst.remaining_mut() < request.bytes.len() {
            return Err(AxError::InvalidInput);
        }
        dst.write(&request.bytes)?;
        let len = request.bytes.len();
        if let Some(delivery) = &request.delivery {
            delivery.store(true, Ordering::Release);
        }
        if request.expects_reply {
            request.state = RequestState::Delivered;
        } else {
            state.requests.remove(index);
        }
        Ok(len)
    }

    fn parse_notification(&self, opcode: u32, payload: &[u8]) -> AxResult<()> {
        fn u64_at(bytes: &[u8], offset: usize) -> AxResult<u64> {
            bytes
                .get(offset..offset + 8)
                .and_then(|v| v.try_into().ok())
                .map(u64::from_ne_bytes)
                .ok_or(AxError::InvalidInput)
        }
        fn i64_at(bytes: &[u8], offset: usize) -> AxResult<i64> {
            Ok(u64_at(bytes, offset)? as i64)
        }
        fn u32_at(bytes: &[u8], offset: usize) -> AxResult<u32> {
            bytes
                .get(offset..offset + 4)
                .and_then(|v| v.try_into().ok())
                .map(u32::from_ne_bytes)
                .ok_or(AxError::InvalidInput)
        }
        if opcode == FUSE_NOTIFY_POLL && payload.len() == 8 {
            let kh = u64_at(payload, 0)?;
            if let Some(poll) = self
                .state
                .lock()
                .poll_states
                .get(&kh)
                .and_then(alloc::sync::Weak::upgrade)
            {
                poll.invalidate();
            }
            return Ok(());
        }
        let notification = match opcode {
            FUSE_NOTIFY_INVAL_INODE if payload.len() == 24 => FuseNotification::InvalidateInode {
                nodeid: u64_at(payload, 0)?,
                offset: i64_at(payload, 8)?,
                length: i64_at(payload, 16)?,
            },
            FUSE_NOTIFY_INVAL_ENTRY if payload.len() >= 16 => {
                let parent = u64_at(payload, 0)?;
                let len = u32_at(payload, 8)? as usize;
                // Unlike pathname-bearing requests, notification names are a
                // counted wire field and have no trailing NUL.
                let name = payload
                    .get(16..)
                    .filter(|name| name.len() == len && !name.is_empty() && !name.contains(&0))
                    .ok_or(AxError::InvalidInput)?;
                FuseNotification::InvalidateEntry {
                    parent,
                    name: name.to_vec(),
                }
            }
            FUSE_NOTIFY_DELETE if payload.len() >= 24 => {
                let parent = u64_at(payload, 0)?;
                let child = u64_at(payload, 8)?;
                let len = u32_at(payload, 16)? as usize;
                let name = payload
                    .get(24..)
                    .filter(|name| name.len() == len && !name.is_empty() && !name.contains(&0))
                    .ok_or(AxError::InvalidInput)?;
                FuseNotification::Delete {
                    parent,
                    child,
                    name: name.to_vec(),
                }
            }
            FUSE_NOTIFY_STORE if payload.len() >= 24 => {
                let nodeid = u64_at(payload, 0)?;
                let offset = u64_at(payload, 8)?;
                let len = u32_at(payload, 16)? as usize;
                if !self.state.lock().known_nodes.contains_key(&nodeid) {
                    return Err(LinuxError::ENOENT.into());
                }
                let data = payload
                    .get(24..)
                    .filter(|d| d.len() == len)
                    .ok_or(AxError::InvalidInput)?;
                FuseNotification::Store {
                    nodeid,
                    offset,
                    data: data.to_vec(),
                }
            }
            FUSE_NOTIFY_RETRIEVE if payload.len() == 32 => {
                let unique = u64_at(payload, 0)?;
                let nodeid = u64_at(payload, 8)?;
                let offset = u64_at(payload, 16)?;
                let size = u32_at(payload, 24)? as usize;
                if size > MAX_REQUEST_BYTES {
                    return Err(AxError::InvalidInput);
                }
                // RETRIEVE asks the kernel page cache for resident bytes.
                // It is not a request to recursively read from the daemon;
                // without one of the cache-negotiating INIT flags Linux
                // rejects the notification instead of fabricating a reply.
                if !self.state.lock().init.is_some_and(|init| {
                    init.flags
                        & (FUSE_WRITEBACK_CACHE | FUSE_AUTO_INVAL_DATA | FUSE_EXPLICIT_INVAL_DATA)
                        != 0
                }) {
                    return Err(LinuxError::ENOSYS.into());
                }
                offset
                    .checked_add(size as u64)
                    .ok_or(AxError::InvalidInput)?;
                if !self.state.lock().known_nodes.contains_key(&nodeid) {
                    return Err(LinuxError::ENOENT.into());
                }
                let mut data = Vec::new();
                data.try_reserve(size).map_err(|_| AxError::NoMemory)?;
                data.resize(size, 0);
                let cached = self
                    .state
                    .lock()
                    .cache
                    .as_ref()
                    .and_then(alloc::sync::Weak::upgrade)
                    .and_then(|cache| {
                        let cache = cache.lock();
                        let ranges = &cache.inodes.get(&nodeid)?.ranges;
                        let mut cursor = offset;
                        let end = offset.checked_add(size as u64)?;
                        while cursor < end {
                            let range = ranges.iter().rev().find(|range| {
                                range.offset <= cursor
                                    && range.offset.saturating_add(range.data.len() as u64) > cursor
                            })?;
                            let n = (range.offset + range.data.len() as u64 - cursor)
                                .min(end - cursor) as usize;
                            let from = (cursor - range.offset) as usize;
                            let to = (cursor - offset) as usize;
                            data[to..to + n].copy_from_slice(&range.data[from..from + n]);
                            cursor += n as u64;
                        }
                        Some(())
                    })
                    .is_some();
                self.queue_notify_reply(unique, nodeid, offset, if cached { &data } else { &[] })?;
                return Ok(());
            }
            // Notification codes are a daemon ABI, not malformed user input.
            // A feature the connection did not negotiate (notably RETRIEVE
            // without a kernel page cache) is rejected with Linux's ENOSYS
            // rather than being collapsed into a synthetic EOPNOTSUPP.
            _ => return Err(LinuxError::ENOSYS.into()),
        };
        let mut state = self.state.lock();
        if state.dead {
            return Err(LinuxError::ENODEV.into());
        }
        if state.deferred_notification.is_some()
            || state.processing_notification.is_some()
            || state.notifications.len() >= MAX_FUSE_NOTIFICATIONS
            || payload.len() > MAX_FUSE_NOTIFICATION_BYTES.saturating_sub(state.notification_bytes)
        {
            return Err(AxError::WouldBlock);
        }
        state
            .notifications
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        state.notification_bytes += payload.len();
        state.notifications.push_back(notification);
        drop(state);
        self.wake();
        Ok(())
    }

    fn daemon_reply(&self, src: &mut IoSrc) -> AxResult<usize> {
        if src.remaining() < size_of::<OutHeader>() {
            return Err(AxError::InvalidInput);
        }
        let mut header_bytes = [0; size_of::<OutHeader>()];
        src.read(&mut header_bytes)?;
        let header =
            unsafe { core::ptr::read_unaligned(header_bytes.as_ptr().cast::<OutHeader>()) };
        if (header.len as usize) < size_of::<OutHeader>()
            || (header.len as usize) > MAX_REQUEST_BYTES
            || (header.len as usize) - size_of::<OutHeader>() != src.remaining()
        {
            return Err(AxError::InvalidInput);
        }
        let payload_len = (header.len as usize) - size_of::<OutHeader>();
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_len)
            .map_err(|_| AxError::NoMemory)?;
        payload.resize(payload_len, 0);
        src.read(&mut payload)?;
        if header.unique == 0 {
            // Notifications use a positive enum in `error`; ordinary replies
            // retain the strict negative-errno ABI below.
            if header.error <= 0 {
                return Err(AxError::InvalidInput);
            }
            self.parse_notification(header.error as u32, &payload)?;
            return Ok(header.len as usize);
        }
        // FUSE reserves -512 and below.  An error reply has no operation
        // payload; accepting trailing bytes would desynchronize the original
        // request's usercopy contract.
        if header.error <= -512 || header.error > 0 || (header.error != 0 && payload_len != 0) {
            return Err(AxError::InvalidInput);
        }
        let errno = header.error.checked_neg().ok_or(AxError::InvalidInput)?;
        let mut state = self.state.lock();
        if state.dead {
            return Err(LinuxError::ENODEV.into());
        }
        let interrupt = header.unique & FUSE_INT_REQ_BIT != 0;
        let target = header.unique & !FUSE_INT_REQ_BIT;
        if interrupt {
            let request = state
                .requests
                .iter()
                .find(|request| request.unique == target)
                .ok_or(LinuxError::ENOENT)?;
            if request.state != RequestState::Delivered || request.reply.is_some() {
                return Err(LinuxError::ENOENT.into());
            }
            // Interrupt replies are header-only.  ENOSYS disables the feature
            // permanently, EAGAIN retries this exact target, and every other
            // legal empty reply merely consumes the interrupt attempt; the
            // original request remains reply-bearing in all three cases.
            if payload_len != 0 {
                return Err(AxError::InvalidInput);
            }
            match LinuxError::try_from(errno).ok() {
                Some(LinuxError::ENOSYS) => {
                    state.no_interrupt = true;
                    // This forbids new interrupt admission only.  Entries
                    // already queued (or already read by the daemon) retain
                    // their normal per-target convergence.
                }
                Some(LinuxError::EAGAIN) => {
                    if !state.no_interrupt
                        && !state.interrupts.iter().any(|unique| *unique == target)
                        && state.interrupts.try_reserve(1).is_ok()
                    {
                        state.interrupts.push_back(target);
                    }
                }
                _ => {}
            }
            drop(state);
            self.wake();
            return Ok(header.len as usize);
        }
        let index = state
            .requests
            .iter()
            .position(|request| request.unique == target)
            .ok_or(LinuxError::ENOENT)?;
        if state.requests[index].state != RequestState::Delivered
            || state.requests[index].reply.is_some()
        {
            return Err(LinuxError::ENOENT.into());
        }
        if state.requests[index].waiter_interrupted {
            // The signal already returned the original caller.  The protocol
            // reply is nevertheless valid and is the final release point for
            // this retained request.
            state.requests.remove(index);
        } else {
            let request = &mut state.requests[index];
            request.reply = Some(if header.error == 0 {
                FuseReply::Data(payload)
            } else {
                FuseReply::Error(errno)
            });
            request.state = RequestState::Replied;
        }
        drop(state);
        self.wake();
        Ok(header.len as usize)
    }

    pub(crate) fn interrupt(&self, target: u64) -> AxResult<()> {
        self.interrupt_request(target, true)
            .then_some(())
            .ok_or(LinuxError::ENOENT.into())
    }

    pub(crate) fn take_notification(&self) -> Option<FuseNotification> {
        let mut state = self.state.lock();
        if state.processing_notification.is_some() {
            return None;
        }
        let (notification, bytes) = state.deferred_notification.take().or_else(|| {
            state.notifications.pop_front().map(|notification| {
                let bytes = fuse_notification_cost(&notification);
                (notification, bytes)
            })
        })?;
        // Retain the byte charge until the applier commits or requeues this
        // exact owner.  The daemon consequently receives backpressure rather
        // than having a deferred notification silently discarded.
        state.processing_notification = Some((notification.clone(), bytes));
        Some(notification)
    }

    fn complete_notification(&self) {
        let mut state = self.state.lock();
        if let Some((_, bytes)) = state.processing_notification.take() {
            state.notification_bytes = state.notification_bytes.checked_sub(bytes).unwrap_or(0);
        }
    }

    fn requeue_notification_front(&self, notification: FuseNotification) {
        let mut state = self.state.lock();
        let Some((owned, bytes)) = state.processing_notification.take() else {
            return;
        };
        // The caller has only a borrowed semantic view of the notification;
        // move the charged, original owner so accounting cannot drift even if
        // its payload is large or producers filled the ordinary queue.
        let _ = notification;
        if state.dead {
            state.notification_bytes = state.notification_bytes.checked_sub(bytes).unwrap_or(0);
            return;
        }
        if state.notifications.len() < MAX_FUSE_NOTIFICATIONS {
            state.notifications.push_front(owned);
            return;
        }
        // The deferred owner is charged in the same `notification_bytes`
        // budget; it is not a second, unbounded allowance.
        if state.deferred_notification.is_none() && bytes <= MAX_FUSE_DEFERRED_NOTIFICATION_BYTES {
            state.deferred_notification = Some((owned, bytes));
        } else {
            // A processing notification blocks producers, so this can only
            // happen after an internal invariant violation. Retain ownership
            // and its charge rather than lose data or underflow accounting.
            state.processing_notification = Some((owned, bytes));
        }
    }
    fn register_poll_state(&self, key: u64, state: &Arc<FusePollState>) {
        self.state
            .lock()
            .poll_states
            .insert(key, Arc::downgrade(state));
    }
    fn unregister_poll_state(&self, key: u64) {
        let mut state = self.state.lock();
        state.poll_states.remove(&key);
        state
            .poll_states
            .retain(|_, value| value.strong_count() != 0);
    }
    fn register_node(&self, nodeid: u64) {
        let mut state = self.state.lock();
        *state.known_nodes.entry(nodeid).or_insert(0) += 1;
    }
    fn attach_cache(&self, cache: &Arc<Mutex<FuseCache>>) {
        self.state.lock().cache = Some(Arc::downgrade(cache));
    }
    fn unregister_node(&self, nodeid: u64) {
        let mut state = self.state.lock();
        if let Some(count) = state.known_nodes.get_mut(&nodeid) {
            *count -= 1;
            if *count == 0 {
                state.known_nodes.remove(&nodeid);
            }
        }
    }

    /// Completes a daemon initiated RETRIEVE with bytes from the kernel-side
    /// cache.  `notify_unique` is supplied by the daemon and is intentionally
    /// used as the request unique; allocating a fresh request id would make a
    /// compliant daemon unable to associate the reply with its notification.
    fn queue_notify_reply(
        &self,
        notify_unique: u64,
        nodeid: u64,
        offset: u64,
        data: &[u8],
    ) -> AxResult<()> {
        let mut body = Vec::new();
        // `fuse_notify_retrieve_in` is fixed-size (the first and trailing
        // words are protocol padding), followed by the requested cache data.
        body.try_reserve(40 + data.len())
            .map_err(|_| AxError::NoMemory)?;
        body.extend_from_slice(&0u64.to_ne_bytes());
        body.extend_from_slice(&offset.to_ne_bytes());
        body.extend_from_slice(&(data.len() as u32).to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        body.extend_from_slice(&0u64.to_ne_bytes());
        body.extend_from_slice(&0u64.to_ne_bytes());
        body.extend_from_slice(data);
        let bytes = self.build_request(notify_unique, FUSE_NOTIFY_REPLY, nodeid, &body)?;
        let mut state = self.state.lock();
        if state.dead || state.destroy_queued {
            return Err(LinuxError::ENODEV.into());
        }
        if state
            .requests
            .len()
            .saturating_add(state.terminal_reservations)
            >= MAX_PENDING_REQUESTS
        {
            return Err(AxError::WouldBlock);
        }
        state
            .requests
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        state.requests.push_back(Request {
            unique: notify_unique,
            opcode: FUSE_NOTIFY_REPLY,
            bytes,
            state: RequestState::Queued,
            reply: None,
            expects_reply: false,
            waiter_interrupted: false,
            delivery: None,
        });
        drop(state);
        self.wake();
        Ok(())
    }

    pub(crate) fn abort(&self) {
        let mut state = self.state.lock();
        if state.dead {
            return;
        }
        state.dead = true;
        state.destroy_prepared.take();
        state.lazy_destroy_participant.take();
        state.destroy_participants.clear();
        state.destroy.take();
        state.interrupts.clear();
        state.notifications.clear();
        state.notification_bytes = 0;
        state.deferred_notification.take();
        state.processing_notification.take();
        for poll in state
            .poll_states
            .values()
            .filter_map(alloc::sync::Weak::upgrade)
        {
            *poll.events.lock() = IoEvents::HANGUP | IoEvents::ERROR;
            poll.invalidate();
        }
        state.poll_states.clear();
        for request in &mut state.requests {
            if request.reply.is_none() {
                request.state = RequestState::Cancelled;
                request.reply = Some(FuseReply::Cancelled);
            }
        }
        // No-reply packets (FORGET, notify replies, cleanup) have no waiter
        // to drain them.  They must not pin arbitrary daemon-provided buffers
        // for the lifetime of a dead mounted superblock.
        state.requests.retain(|request| request.expects_reply);
        drop(state);
        self.wake();
    }

    /// Starts the normal-unmount half of connection teardown.  DESTROY is a
    /// daemon notification, not a reply-bearing operation, and is queued
    /// before mount registry removal so fusectl never exposes a disconnected
    /// mount as live.  Descriptor close remains free to abort this queue.
    fn prepare_destroy(&self) -> AxResult<Option<u64>> {
        let participant = NEXT_FUSE_DESTROY_PARTICIPANT
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        let unique = self.next_unique.fetch_add(1, Ordering::Relaxed).max(1);
        let bytes = self.build_request(unique, FUSE_DESTROY, FUSE_ROOT_ID, &[])?;
        let mut state = self.state.lock();
        if state.dead || state.destroy_queued {
            return Ok(None);
        }
        state
            .destroy_participants
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        // This packet occupies its own preallocated outbox rather than the
        // bounded request queue, so activation after mount commit cannot
        // allocate or fail.
        if state.destroy_prepared.is_none() {
            let ticket = NEXT_FUSE_DESTROY_TICKET
                .fetch_add(1, Ordering::Relaxed)
                .max(1);
            state.destroy_prepared = Some((
                ticket,
                Request {
                    unique,
                    opcode: FUSE_DESTROY,
                    bytes,
                    state: RequestState::Queued,
                    reply: None,
                    expects_reply: false,
                    waiter_interrupted: false,
                    delivery: None,
                },
            ));
        }
        state.destroy_participants.push((participant, 0));
        Ok(Some(participant))
    }

    fn commit_destroy(&self, participant: u64) {
        let mut state = self.state.lock();
        if state.dead || state.destroy_queued {
            return;
        }
        if let Some((_, status)) = state
            .destroy_participants
            .iter_mut()
            .find(|(candidate, _)| *candidate == participant)
        {
            *status = 1;
        }
        drop(state);
    }

    fn activate_destroy_if_resolved(&self) {
        let registry = FUSE_MOUNT_CONNECTIONS.lock();
        let ready = {
            let state = self.state.lock();
            state.destroy_prepared.is_some()
                && !state.destroy_participants.is_empty()
                && state
                    .destroy_participants
                    .iter()
                    .all(|(_, status)| *status == 1)
                && (state.lazy_destroy_participant.is_none() || state.final_unmount_seen)
        };
        if !ready {
            return;
        }
        let survivor = registry.iter().any(|(_, connection)| {
            connection
                .upgrade()
                .is_some_and(|connection| core::ptr::eq(connection.as_ref(), self))
        });
        if survivor {
            return;
        }
        let mut state = self.state.lock();
        if state.destroy_prepared.is_some()
            && state
                .destroy_participants
                .iter()
                .all(|(_, status)| *status == 1)
        {
            let (_, request) = state.destroy_prepared.take().expect("prepared destroy");
            state.destroy = Some(request);
            state.destroy_queued = true;
            state.lazy_destroy_participant.take();
            drop(state);
            drop(registry);
            self.wake();
        }
    }

    fn accepts_mount_registration(&self) -> bool {
        let state = self.state.lock();
        !state.dead && !state.destroy_queued && state.destroy_prepared.is_none()
    }

    fn defer_destroy(&self, participant: u64) {
        let mut state = self.state.lock();
        if let Some((_, status)) = state
            .destroy_participants
            .iter_mut()
            .find(|(candidate, _)| *candidate == participant)
        {
            *status = 1;
            state.lazy_destroy_participant = Some(participant);
        }
    }

    fn cancel_destroy(&self, participant: u64) {
        let mut state = self.state.lock();
        let known = state
            .destroy_participants
            .iter()
            .any(|(candidate, _)| *candidate == participant);
        let abort = known && state.destroy_prepared.is_some() && state.final_unmount_seen;
        if known {
            state.destroy_prepared.take();
            state.lazy_destroy_participant.take();
            state.destroy_participants.clear();
        }
        drop(state);
        if abort {
            self.abort();
        }
    }

    fn destroy_queued(&self) -> bool {
        let state = self.state.lock();
        state.destroy_queued
    }

    fn activate_lazy_destroy_on_final_unmount(&self) -> bool {
        let mut state = self.state.lock();
        state.final_unmount_seen = true;
        let ticket = state.lazy_destroy_participant;
        let armed = state.destroy_prepared.is_some() || state.destroy_queued;
        drop(state);
        if let Some(ticket) = ticket {
            self.commit_destroy(ticket);
            self.activate_destroy_if_resolved();
            true
        } else {
            armed
        }
    }

    fn ctl_values(&self) -> (bool, usize, usize, usize, bool, u16, u16) {
        let state = self.state.lock();
        let queued = state
            .requests
            .iter()
            .filter(|request| request.expects_reply && request.state == RequestState::Queued)
            .count();
        let active = state
            .requests
            .iter()
            .filter(|request| request.state == RequestState::Delivered)
            .count();
        let waiting = state
            .requests
            .iter()
            .filter(|request| {
                request.expects_reply
                    && matches!(
                        request.state,
                        RequestState::Queued | RequestState::Delivered
                    )
            })
            .count();
        let max_background = state.init.map_or(0, |init| init.max_background);
        let threshold = state.init.map_or(0, |init| init.congestion_threshold);
        (
            !state.dead,
            waiting,
            queued,
            active,
            threshold != 0 && active >= threshold as usize,
            max_background,
            threshold,
        )
    }
    fn set_ctl_limit(&self, background: bool, value: u16) -> VfsResult<()> {
        let mut state = self.state.lock();
        let init = state.init.as_mut().ok_or(VfsError::InvalidInput)?;
        if background {
            init.max_background = value;
            if init.congestion_threshold > value {
                init.congestion_threshold = value;
            }
        } else if value > init.max_background {
            return Err(VfsError::InvalidInput);
        } else {
            init.congestion_threshold = value;
        }
        drop(state);
        self.wake();
        Ok(())
    }
}

/// Retains the daemon connection for an installed FUSE mount so `fspick` can
/// reconstruct a real fscontext instead of creating a detached FUSE shell.
pub(crate) fn register_mount_connection(
    mount_id: u64,
    connection: &Arc<FuseConnection>,
) -> AxResult<()> {
    let mut mounts = FUSE_MOUNT_CONNECTIONS.lock();
    if !connection.accepts_mount_registration() {
        return Err(LinuxError::ENODEV.into());
    }
    mounts.retain(|(_, connection)| connection.strong_count() != 0);
    mounts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    mounts.push((mount_id, Arc::downgrade(connection)));
    Ok(())
}

pub(crate) fn mount_connection(mount_id: u64) -> Option<Arc<FuseConnection>> {
    FUSE_MOUNT_CONNECTIONS
        .lock()
        .iter()
        .find_map(|(id, connection)| (*id == mount_id).then(|| connection.upgrade()).flatten())
}

/// A no-allocation commit receipt for normal mount teardown.  It owns both
/// the registry IDs to remove and the last-instance DESTROY packets prepared
/// before VFS unmount; dropping an uncommitted receipt rolls those packets
/// back without disturbing a live mount.
pub(crate) struct PreparedFuseMountTeardown {
    receipt_id: u64,
    mount_ids: Vec<u64>,
    destroy_connections: Vec<(Arc<FuseConnection>, u64)>,
    active: bool,
}

impl PreparedFuseMountTeardown {
    pub(crate) fn commit(mut self) {
        for (connection, ticket) in &self.destroy_connections {
            connection.commit_destroy(*ticket);
        }
        let mut mounts = FUSE_MOUNT_CONNECTIONS.lock();
        mounts.retain(|(id, connection)| {
            !self.mount_ids.contains(id) && connection.strong_count() != 0
        });
        let mut pending = FUSE_PENDING_MOUNT_REMOVALS.lock();
        pending.retain(|(receipt, _)| *receipt != self.receipt_id);
        drop(pending);
        drop(mounts);
        for (connection, _) in &self.destroy_connections {
            connection.activate_destroy_if_resolved();
        }
        self.active = false;
    }

    pub(crate) fn commit_deferred(mut self) {
        for (connection, ticket) in &self.destroy_connections {
            connection.defer_destroy(*ticket);
        }
        let mut mounts = FUSE_MOUNT_CONNECTIONS.lock();
        mounts.retain(|(id, connection)| {
            !self.mount_ids.contains(id) && connection.strong_count() != 0
        });
        let mut pending = FUSE_PENDING_MOUNT_REMOVALS.lock();
        pending.retain(|(receipt, _)| *receipt != self.receipt_id);
        drop(pending);
        drop(mounts);
        for (connection, _) in &self.destroy_connections {
            connection.activate_destroy_if_resolved();
        }
        self.active = false;
    }
}

impl Drop for PreparedFuseMountTeardown {
    fn drop(&mut self) {
        if self.active {
            for (connection, ticket) in &self.destroy_connections {
                connection.cancel_destroy(*ticket);
            }
            let mut pending = FUSE_PENDING_MOUNT_REMOVALS.lock();
            pending.retain(|(receipt, _)| *receipt != self.receipt_id);
        }
    }
}

/// Reserves FUSE teardown before the VFS's potentially failing unmount
/// commit.  A connection is destroyed only when none of its registered mount
/// instances survive this removal (binds and namespace replicas share it).
pub(crate) fn prepare_mount_teardown(mount_ids: Vec<u64>) -> AxResult<PreparedFuseMountTeardown> {
    let receipt_id = NEXT_FUSE_TEARDOWN_RECEIPT
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    let mut candidates = Vec::new();
    candidates
        .try_reserve(mount_ids.len())
        .map_err(|_| AxError::NoMemory)?;
    {
        let mounts = FUSE_MOUNT_CONNECTIONS.lock();
        let mut pending = FUSE_PENDING_MOUNT_REMOVALS.lock();
        pending
            .try_reserve(mount_ids.len())
            .map_err(|_| AxError::NoMemory)?;
        // A teardown receipt is a closed description of the exact VFS
        // removal set.  Silently skipping a missing registration would let a
        // mount disappear from the namespace while retaining its FUSE
        // connection entry (and can make the final DESTROY decision wrong).
        // Validate the complete set before reserving anything in `pending`.
        for mount_id in &mount_ids {
            if pending.iter().any(|(_, pending_id)| pending_id == mount_id)
                || mount_ids
                    .iter()
                    .filter(|candidate| *candidate == mount_id)
                    .count()
                    != 1
            {
                return Err(AxError::ResourceBusy);
            }
            if !mounts
                .iter()
                .any(|(id, connection)| *id == *mount_id && connection.upgrade().is_some())
            {
                return Err(AxError::NotFound);
            }
        }
        pending.extend(
            mount_ids
                .iter()
                .copied()
                .map(|mount_id| (receipt_id, mount_id)),
        );
        for mount_id in &mount_ids {
            let connection = mounts
                .iter()
                .find_map(|(id, connection)| {
                    (*id == *mount_id).then(|| connection.upgrade()).flatten()
                })
                .expect("validated FUSE teardown registration");
            if candidates
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, &connection))
            {
                continue;
            }
            let has_survivor = mounts.iter().any(|(id, candidate)| {
                !mount_ids.contains(id)
                    && !pending.iter().any(|(_, pending_id)| pending_id == id)
                    && candidate
                        .upgrade()
                        .is_some_and(|candidate| Arc::ptr_eq(&candidate, &connection))
            });
            if !has_survivor {
                candidates.push(connection);
            }
        }
    }
    let mut destroy_connections = Vec::new();
    destroy_connections
        .try_reserve(candidates.len())
        .map_err(|_| AxError::NoMemory)?;
    for connection in candidates {
        match connection.prepare_destroy() {
            Ok(Some(ticket)) => destroy_connections.push((connection, ticket)),
            Ok(None) => {}
            Err(error) => {
                for (prepared, ticket) in &destroy_connections {
                    prepared.cancel_destroy(*ticket);
                }
                let mut pending = FUSE_PENDING_MOUNT_REMOVALS.lock();
                pending.retain(|(receipt, _)| *receipt != receipt_id);
                return Err(error);
            }
        }
    }
    Ok(PreparedFuseMountTeardown {
        receipt_id,
        mount_ids,
        destroy_connections,
        active: true,
    })
}

/// Discards registrations for a clone whose topology publication failed.  It
/// intentionally does not queue DESTROY because the source mount remains
/// live and shares the same connection.
pub(crate) fn unregister_mount_connection(mount_id: u64) {
    let mut mounts = FUSE_MOUNT_CONNECTIONS.lock();
    mounts.retain(|(id, connection)| *id != mount_id && connection.strong_count() != 0);
}

fn live_connections() -> Vec<(u64, Arc<FuseConnection>)> {
    let mut mounts = FUSE_MOUNT_CONNECTIONS.lock();
    mounts.retain(|(_, connection)| connection.strong_count() != 0);
    mounts
        .iter()
        .filter_map(|(id, connection)| connection.upgrade().map(|connection| (*id, connection)))
        .collect()
}

struct FuseCtlRoot {
    fs: Arc<crate::pseudofs::SimpleFs>,
}
impl crate::pseudofs::SimpleDirOps for FuseCtlRoot {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        let connections = live_connections();
        let mut names = Vec::new();
        names
            .try_reserve(connections.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (id, _) in connections {
            names.push(Cow::Owned(FsNameBuf::from_vec(
                format!("{id}").into_bytes(),
            )?));
        }
        crate::pseudofs::try_boxed_names(names.into_iter())
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<crate::pseudofs::NodeOpsMux> {
        let id = name
            .as_bytes()
            .iter()
            .try_fold(0u64, |value, digit| {
                digit
                    .is_ascii_digit()
                    .then(|| value.checked_mul(10)?.checked_add(u64::from(*digit - b'0')))
                    .flatten()
            })
            .ok_or(VfsError::NotFound)?;
        let connection = live_connections()
            .into_iter()
            .find_map(|(candidate, connection)| (candidate == id).then_some(connection))
            .ok_or(VfsError::NotFound)?;
        Ok(crate::pseudofs::SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(FuseCtlConnection {
                fs: self.fs.clone(),
                connection,
            }),
        )
        .into())
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}
struct FuseCtlConnection {
    fs: Arc<crate::pseudofs::SimpleFs>,
    connection: Arc<FuseConnection>,
}
impl crate::pseudofs::SimpleDirOps for FuseCtlConnection {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        crate::pseudofs::try_boxed_names(
            [
                "abort",
                "connected",
                "waiting",
                "queued",
                "active",
                "congested",
                "max_background",
                "congestion_threshold",
            ]
            .into_iter()
            .map(|name| Cow::Borrowed(FsName::new(name.as_bytes()))),
        )
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<crate::pseudofs::NodeOpsMux> {
        use crate::pseudofs::{RwFile, SimpleFile, SimpleFileOperation};
        let file = match name.as_bytes() {
            b"abort" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(
                    self.fs.clone(),
                    RwFile::new_root_writable(move |op| -> VfsResult<Option<String>> {
                        match op {
                            SimpleFileOperation::Read => Ok(Some(if connection.is_dead() {
                                "1\n".into()
                            } else {
                                "0\n".into()
                            })),
                            SimpleFileOperation::Write(_) => {
                                connection.abort();
                                Ok(Some(String::new()))
                            }
                        }
                    }),
                )
            }
            b"connected" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(self.fs.clone(), move || -> VfsResult<String> {
                    Ok(format!("{}\n", connection.ctl_values().0 as u8))
                })
            }
            b"waiting" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(self.fs.clone(), move || -> VfsResult<String> {
                    Ok(format!("{}\n", connection.ctl_values().1))
                })
            }
            b"queued" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(self.fs.clone(), move || -> VfsResult<String> {
                    Ok(format!("{}\n", connection.ctl_values().2))
                })
            }
            b"active" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(self.fs.clone(), move || -> VfsResult<String> {
                    Ok(format!("{}\n", connection.ctl_values().3))
                })
            }
            b"congested" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(self.fs.clone(), move || -> VfsResult<String> {
                    Ok(format!("{}\n", connection.ctl_values().4 as u8))
                })
            }
            b"max_background" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(
                    self.fs.clone(),
                    RwFile::new_root_writable(move |op| -> VfsResult<Option<String>> {
                        match op {
                            SimpleFileOperation::Read => {
                                Ok(Some(format!("{}\n", connection.ctl_values().5)))
                            }
                            SimpleFileOperation::Write(value) => {
                                let value = core::str::from_utf8(value)
                                    .ok()
                                    .and_then(|v| v.trim().parse().ok())
                                    .ok_or(VfsError::InvalidInput)?;
                                connection.set_ctl_limit(true, value)?;
                                Ok(Some(String::new()))
                            }
                        }
                    }),
                )
            }
            b"congestion_threshold" => {
                let connection = self.connection.clone();
                SimpleFile::new_regular(
                    self.fs.clone(),
                    RwFile::new_root_writable(move |op| -> VfsResult<Option<String>> {
                        match op {
                            SimpleFileOperation::Read => {
                                Ok(Some(format!("{}\n", connection.ctl_values().6)))
                            }
                            SimpleFileOperation::Write(value) => {
                                let value = core::str::from_utf8(value)
                                    .ok()
                                    .and_then(|v| v.trim().parse().ok())
                                    .ok_or(VfsError::InvalidInput)?;
                                connection.set_ctl_limit(false, value)?;
                                Ok(Some(String::new()))
                            }
                        }
                    }),
                )
            }
            _ => return Err(VfsError::NotFound),
        };
        Ok(file.into())
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}
pub(crate) fn new_fusectl() -> Filesystem {
    use crate::pseudofs::{SimpleDir, SimpleFs};
    SimpleFs::new_with("fusectl".into(), 0x6573_5543, |fs| {
        SimpleDir::new_maker(fs.clone(), Arc::new(FuseCtlRoot { fs }))
    })
}

/// Per-open daemon descriptor.  The node is global only as a device endpoint;
/// the connection itself is OFD-owned, exactly like Linux's `/dev/fuse`.
pub(crate) struct FuseDeviceFile {
    connection: Arc<FuseConnection>,
    nonblocking: AtomicBool,
}

impl FuseDeviceFile {
    pub(crate) fn connection(&self) -> Arc<FuseConnection> {
        self.connection.clone()
    }
}

impl FileLike for FuseDeviceFile {
    // Linux's FUSE character-device UAPI has no IORING_OP_URING_CMD command
    // family; FUSE requests remain the daemon protocol read/write transport.
    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn pre_close(&self) {
        self.connection.abort();
    }
    fn final_close(&self) {
        self.connection.abort();
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        block_on_poll_io(
            self.connection.as_ref(),
            IoEvents::READABLE,
            self.nonblocking.load(Ordering::Acquire),
            || self.connection.dequeue_for_daemon(dst),
        )
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.connection.daemon_reply(src)
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[fuse]",
        )))
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        self.connection.wake();
        Ok(())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
}

impl Pollable for FuseDeviceFile {
    fn poll(&self) -> IoEvents {
        self.connection.poll()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.connection.register(context, events)
    }
}

impl Pollable for FuseConnection {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        if state.dead {
            return IoEvents::HANGUP | IoEvents::ERROR;
        }
        let mut events = IoEvents::WRITABLE;
        events.set(
            IoEvents::READABLE,
            state.destroy.is_some()
                || !state.interrupts.is_empty()
                || !state.forget_nodes.is_empty()
                || state
                    .requests
                    .iter()
                    .any(|request| request.state == RequestState::Queued),
        );
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::HANGUP | IoEvents::ERROR,
        ) {
            PollRegistration::single(&self.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

pub(crate) struct FuseDevice;

impl DeviceOps for FuseDevice {
    fn open_description(&self, _location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let connection = FuseConnection::try_new().map_err(VfsError::from)?;
        let file: Arc<dyn FileLike> = Arc::try_new(FuseDeviceFile {
            connection,
            nonblocking: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_SEEK
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
    }
}

/// Builds a real remote FUSE superblock after the daemon has negotiated INIT.
/// The root inode is fetched from the daemon before anything is published to
/// mount topology, so a dead/malformed daemon never creates a fake mount.
pub(crate) fn mount_filesystem(connection: Arc<FuseConnection>) -> AxResult<Filesystem> {
    connection.negotiate()?;
    let root_reply =
        FuseConnection::reply_data(connection.request(FUSE_GETATTR, FUSE_ROOT_ID, &[])?)?;
    let root_metadata = parse_attr_out(&root_reply)?;
    if root_metadata.node_type != NodeType::Directory {
        return Err(LinuxError::ENOTDIR.into());
    }
    let cache = Arc::try_new(Mutex::new(FuseCache::default())).map_err(|_| AxError::NoMemory)?;
    let fs = Arc::try_new(FuseFilesystem {
        connection,
        identity: NEXT_FUSE_FILESYSTEM_ID
            .fetch_add(1, Ordering::Relaxed)
            .max(1),
        root: Mutex::new(None),
        nodes: Mutex::new(Vec::new()),
        cache: cache.clone(),
        handles: Mutex::new(BTreeMap::new()),
        next_handle_generation: AtomicU64::new(1),
        polls: Mutex::new(BTreeMap::new()),
        next_poll_key: AtomicU64::new(1),
        notification_apply: Mutex::new(()),
        self_ref: Mutex::new(None),
    })
    .map_err(|_| AxError::NoMemory)?;
    fs.connection.attach_cache(&cache);
    *fs.self_ref.lock() = Some(Arc::downgrade(&fs));
    let root = fs.entry_for(FUSE_ROOT_ID, root_metadata, Reference::root())?;
    *fs.root.lock() = Some(root);
    Filesystem::try_new(fs).map_err(AxError::from)
}

struct FuseFilesystem {
    connection: Arc<FuseConnection>,
    identity: u64,
    root: Mutex<Option<DirEntry>>,
    // A weak registry retains identity only while a dentry/OFD owns the
    // inode.  This avoids inode-number reuse becoming a stale page-cache key.
    nodes: Mutex<Vec<(u64, alloc::sync::Weak<FuseNode>)>>,
    /// The FUSE protocol cache is superblock-wide, never an OFD-local
    /// optimisation.  That preserves coherent reads between independent
    /// opens while each dirty byte remains attributed to its originating fh.
    cache: Arc<Mutex<FuseCache>>,
    handles: Mutex<BTreeMap<(u64, u64, u64), Arc<FuseHandleLease>>>,
    next_handle_generation: AtomicU64,
    polls: Mutex<BTreeMap<u64, alloc::sync::Weak<FusePollState>>>,
    next_poll_key: AtomicU64,
    notification_apply: Mutex<()>,
    self_ref: Mutex<Option<alloc::sync::Weak<FuseFilesystem>>>,
}

struct FuseIoctlReply {
    result: i32,
    flags: u32,
    in_iovs: u32,
    out_iovs: u32,
    data: Vec<u8>,
}

impl FuseFilesystem {
    fn retain_node(&self, nodeid: u64) -> Option<Arc<FuseNode>> {
        let mut nodes = self.nodes.lock();
        nodes.retain(|(_, node)| node.strong_count() != 0);
        nodes
            .iter()
            .rev()
            .find(|(id, _)| *id == nodeid)
            .and_then(|(_, node)| node.upgrade())
    }

    fn cache_active(&self) -> bool {
        self.connection.init().is_some_and(|init| {
            init.flags & (FUSE_WRITEBACK_CACHE | FUSE_AUTO_INVAL_DATA | FUSE_EXPLICIT_INVAL_DATA)
                != 0
        })
    }

    fn replace_cached_range(
        ranges: &mut Vec<FuseCachedRange>,
        replacement: FuseCachedRange,
    ) -> VfsResult<()> {
        let end = replacement
            .offset
            .checked_add(replacement.data.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        let mut next = Vec::new();
        next.try_reserve(ranges.len().saturating_add(2))
            .map_err(|_| VfsError::NoMemory)?;
        // Build an entire replacement image before touching `ranges`.  A
        // failed prefix/suffix allocation must leave dirty data, ownership and
        // in-flight state exactly as they were.
        for old in ranges.iter() {
            let old_end = old
                .offset
                .checked_add(old.data.len() as u64)
                .ok_or(VfsError::Io)?;
            if old_end <= replacement.offset || old.offset >= end {
                next.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                next.push(old.clone());
                continue;
            }
            if old.offset < replacement.offset {
                let keep = (replacement.offset - old.offset) as usize;
                let mut data = Vec::new();
                data.try_reserve(keep).map_err(|_| VfsError::NoMemory)?;
                data.extend_from_slice(&old.data[..keep]);
                next.push(FuseCachedRange {
                    offset: old.offset,
                    data,
                    epoch: old.epoch,
                    inflight: old.inflight,
                    dirty_fh: old.dirty_fh,
                    owner: old.owner.clone(),
                });
            }
            if old_end > end {
                let start = (end - old.offset) as usize;
                let mut data = Vec::new();
                data.try_reserve(old.data.len() - start)
                    .map_err(|_| VfsError::NoMemory)?;
                data.extend_from_slice(&old.data[start..]);
                next.push(FuseCachedRange {
                    offset: end,
                    data,
                    epoch: old.epoch,
                    inflight: old.inflight,
                    dirty_fh: old.dirty_fh,
                    owner: old.owner.clone(),
                });
            }
        }
        next.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        next.push(replacement);
        *ranges = next;
        Ok(())
    }

    fn cached_read(&self, nodeid: u64, offset: u64, out: &mut [u8]) -> Option<VfsResult<usize>> {
        if !self.cache_active() {
            return None;
        }
        let end = match offset.checked_add(out.len() as u64) {
            Some(end) => end,
            None => return Some(Err(VfsError::InvalidInput)),
        };
        let cache = self.cache.lock();
        let ranges = &cache.inodes.get(&nodeid)?.ranges;
        let mut cursor = offset;
        while cursor < end {
            let Some(range) = ranges.iter().rev().find(|range| {
                range.offset <= cursor
                    && range.offset.saturating_add(range.data.len() as u64) > cursor
            }) else {
                return None;
            };
            let available = range.offset + range.data.len() as u64 - cursor;
            let count = available.min(end - cursor) as usize;
            let from = (cursor - range.offset) as usize;
            let to = (cursor - offset) as usize;
            out[to..to + count].copy_from_slice(&range.data[from..from + count]);
            cursor += count as u64;
        }
        Some(Ok(out.len()))
    }

    /// Overlay unflushed writer-owned bytes after a daemon read that filled a
    /// cache hole. `cached_read` may already have copied a dirty prefix before
    /// discovering that hole; the remote response must never replace it.
    fn overlay_dirty_cached(&self, nodeid: u64, offset: u64, out: &mut [u8]) {
        let end = offset.saturating_add(out.len() as u64);
        let cache = self.cache.lock();
        let Some(inode) = cache.inodes.get(&nodeid) else {
            return;
        };
        for range in inode.ranges.iter().filter(|range| range.dirty_fh.is_some()) {
            let range_end = range.offset.saturating_add(range.data.len() as u64);
            let start = range.offset.max(offset);
            let stop = range_end.min(end);
            if start < stop {
                let from = (start - range.offset) as usize;
                let to = (start - offset) as usize;
                out[to..to + (stop - start) as usize]
                    .copy_from_slice(&range.data[from..from + (stop - start) as usize]);
            }
        }
    }

    fn cache_store(
        &self,
        nodeid: u64,
        offset: u64,
        data: &[u8],
        owner: Option<Arc<FuseHandleLease>>,
    ) -> VfsResult<()> {
        if !self.cache_active() || data.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve(data.len())
            .map_err(|_| VfsError::NoMemory)?;
        bytes.extend_from_slice(data);
        let dirty = owner.is_some();
        let mut cache = self.cache.lock();
        let global_bytes = cache.bytes;
        let (before, staged) = {
            let existing = cache.inodes.get(&nodeid);
            let end = offset.saturating_add(data.len() as u64);
            // A daemon read may span cached holes and an unflushed write. It
            // is never allowed to replace the dirty/in-flight portion with
            // stale remote bytes; bypass this opportunistic clean fill.
            if owner.is_none()
                && existing.is_some_and(|inode| {
                    inode.ranges.iter().any(|range| {
                        range.dirty_fh.is_some()
                            && range.offset < end
                            && range.offset.saturating_add(range.data.len() as u64) > offset
                    })
                })
            {
                return Ok(());
            }
            let inode_bytes = existing.map(fuse_inode_cost).unwrap_or(0);
            let mut ranges = Vec::new();
            ranges
                .try_reserve(existing.map_or(3, |inode| inode.ranges.len().saturating_add(3)))
                .map_err(|_| VfsError::NoMemory)?;
            if let Some(inode) = existing {
                ranges.extend(inode.ranges.iter().cloned());
            }
            let epoch = existing
                .map(|inode| inode.writeback_epoch.wrapping_add(1).max(1))
                .unwrap_or(1);
            let dirty_fh = owner.as_ref().map(|owner| owner.fh);
            Self::replace_cached_range(
                &mut ranges,
                FuseCachedRange {
                    offset,
                    data: bytes,
                    epoch,
                    inflight: false,
                    dirty_fh,
                    owner,
                },
            )?;
            (
                inode_bytes,
                FuseInodeCache {
                    ranges,
                    writeback_epoch: epoch,
                },
            )
        };
        let after = fuse_inode_cost(&staged);
        let other_inode_bytes = global_bytes.saturating_sub(before);
        if staged.ranges.len() > MAX_FUSE_INODE_CACHE_RANGES
            || after > MAX_FUSE_INODE_CACHE_BYTES
            || after > MAX_FUSE_CACHE_BYTES.saturating_sub(other_inode_bytes)
        {
            // Clean daemon fills remain opportunistic; a dirty write must
            // retain backpressure so its acknowledged user write is never
            // silently lost.
            return if !dirty {
                Ok(())
            } else {
                Err(VfsError::ResourceBusy)
            };
        }
        cache.inodes.insert(nodeid, staged);
        cache.bytes = cache.bytes.saturating_sub(before).saturating_add(after);
        Ok(())
    }

    fn cached_retrieve(&self, nodeid: u64, offset: u64, size: usize) -> VfsResult<Vec<u8>> {
        if size > MAX_REQUEST_BYTES {
            return Err(VfsError::InvalidInput);
        }
        let mut data = Vec::new();
        data.try_reserve(size).map_err(|_| VfsError::NoMemory)?;
        data.resize(size, 0);
        match self.cached_read(nodeid, offset, &mut data) {
            Some(Ok(_)) => Ok(data),
            // FUSE_NOTIFY_RETRIEVE is a cache query, not permission to fetch
            // data from the daemon recursively.  Holes/nonresident bytes are
            // returned as the short zero-length reply specified by FUSE.
            Some(Err(error)) => Err(error),
            None => Ok(Vec::new()),
        }
    }

    fn take_dirty(&self, nodeid: u64, fh: Option<u64>) -> Vec<FuseCachedRange> {
        let mut cache = self.cache.lock();
        let Some(inode) = cache.inodes.get_mut(&nodeid) else {
            return Vec::new();
        };
        let mut dirty = Vec::new();
        for range in &mut inode.ranges {
            if !range.inflight
                && range
                    .dirty_fh
                    .is_some_and(|owner| fh.is_none_or(|wanted| wanted == owner))
            {
                range.inflight = true;
                dirty.push(range.clone());
            }
        }
        dirty
    }

    fn restore_dirty(&self, nodeid: u64, ranges: Vec<FuseCachedRange>) {
        let mut cache = self.cache.lock();
        {
            let Some(inode) = cache.inodes.get_mut(&nodeid) else {
                return;
            };
            for range in ranges {
                if let Some(current) = inode
                    .ranges
                    .iter_mut()
                    .find(|current| current.epoch == range.epoch && current.offset == range.offset)
                {
                    current.inflight = false;
                }
            }
        }
    }

    fn complete_dirty(&self, nodeid: u64, ranges: &[FuseCachedRange]) {
        let mut cache = self.cache.lock();
        let Some(inode) = cache.inodes.get_mut(&nodeid) else {
            return;
        };
        let before = fuse_inode_cost(inode);
        inode.ranges.retain(|current| {
            !ranges.iter().any(|done| {
                current.epoch == done.epoch && current.offset == done.offset && current.inflight
            })
        });
        let after = fuse_inode_cost(inode);
        cache.bytes = cache.bytes.saturating_sub(before).saturating_add(after);
    }

    fn flush_cached_handle(&self, nodeid: u64, fh: u64) -> VfsResult<()> {
        // Share the notification serialization gate. A notifier must not see
        // a range as clean, then race a writer that has made it in-flight.
        let _serial = self.notification_apply.lock();
        self.flush_cached_handle_serialized(nodeid, fh)
    }

    fn flush_cached_handle_serialized(&self, nodeid: u64, fh: u64) -> VfsResult<()> {
        let dirty = self.take_dirty(nodeid, Some(fh));
        for (index, range) in dirty.iter().enumerate() {
            match fuse_write(self, nodeid, fh, &range.data, range.offset) {
                Ok(written) if written == range.data.len() => {
                    self.complete_dirty(nodeid, core::slice::from_ref(range))
                }
                Ok(written) => {
                    // The daemon acknowledged exactly the prefix.  Retire
                    // the old dirty record, retain that prefix clean, and
                    // re-admit only the unwritten suffix with its original
                    // generation-specific owner lease.
                    // Stage both replacement fragments first.  If either
                    // allocation/admission fails, the original inflight dirty
                    // record remains recoverable.
                    let staged = (|| {
                        if written < range.data.len() {
                            self.cache_store(
                                nodeid,
                                range.offset + written as u64,
                                &range.data[written..],
                                range.owner.clone(),
                            )?;
                        }
                        if written != 0 {
                            self.cache_store(nodeid, range.offset, &range.data[..written], None)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = staged {
                        self.restore_dirty(nodeid, dirty[index..].to_vec());
                        return Err(error);
                    }
                    self.complete_dirty(nodeid, core::slice::from_ref(range));
                    self.restore_dirty(nodeid, dirty[index + 1..].to_vec());
                    return Err(VfsError::Io);
                }
                Err(error) => {
                    self.restore_dirty(nodeid, dirty[index..].to_vec());
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn has_dirty_owned_by_other(&self, nodeid: u64, fh: u64) -> bool {
        self.cache.lock().inodes.get(&nodeid).is_some_and(|inode| {
            inode
                .ranges
                .iter()
                .any(|range| range.dirty_fh.is_some_and(|owner| owner != fh))
        })
    }
    fn has_dead_dirty_owner(&self, nodeid: u64) -> bool {
        self.cache.lock().inodes.get(&nodeid).is_some_and(|inode| {
            inode.ranges.iter().any(|range| {
                range.dirty_fh.is_some()
                    && range
                        .owner
                        .as_ref()
                        .is_none_or(|owner| !owner.live.load(Ordering::Acquire))
            })
        })
    }
    fn has_inflight_dirty(&self, nodeid: u64) -> bool {
        self.cache.lock().inodes.get(&nodeid).is_some_and(|inode| {
            inode
                .ranges
                .iter()
                .any(|range| range.inflight && range.dirty_fh.is_some())
        })
    }

    fn invalidate_cached_inode(&self, nodeid: u64, offset: i64, length: i64) {
        let mut cache = self.cache.lock();
        let Some(inode) = cache.inodes.get_mut(&nodeid) else {
            return;
        };
        let before = fuse_inode_cost(inode);
        if offset < 0 {
            inode.ranges.clear();
            cache.bytes = cache.bytes.saturating_sub(before);
            return;
        }
        if length <= 0 {
            // FUSE_NOTIFY_INVAL_INODE uses non-positive len for the suffix
            // through EOF; zero is not an empty range.
            inode.ranges.retain(|range| {
                range.offset.saturating_add(range.data.len() as u64) <= offset as u64
            });
        } else {
            let end = (offset as u64)
                .checked_add(length as u64)
                .unwrap_or(u64::MAX);
            inode.ranges.retain(|range| {
                range.offset.saturating_add(range.data.len() as u64) <= offset as u64
                    || range.offset >= end
            });
        }
        let after = fuse_inode_cost(inode);
        cache.bytes = cache.bytes.saturating_sub(before).saturating_add(after);
    }

    /// A direct temporary-fh operation has already reached the daemon.  It
    /// may race a later writeback-cache publication, so discard only clean
    /// overlapping bytes and never erase that newer dirty owner record.
    fn invalidate_cached_clean_inode(&self, nodeid: u64, offset: i64, length: i64) {
        let mut cache = self.cache.lock();
        let Some(inode) = cache.inodes.get_mut(&nodeid) else {
            return;
        };
        let before = fuse_inode_cost(inode);
        if offset < 0 || length < 0 {
            inode.ranges.retain(|range| range.dirty_fh.is_some());
        } else {
            let end = (offset as u64)
                .checked_add(length as u64)
                .unwrap_or(u64::MAX);
            inode.ranges.retain(|range| {
                range.dirty_fh.is_some()
                    || range.offset.saturating_add(range.data.len() as u64) <= offset as u64
                    || range.offset >= end
            });
        }
        let after = fuse_inode_cost(inode);
        cache.bytes = cache.bytes.saturating_sub(before).saturating_add(after);
    }

    fn register_poll(&self, state: &Arc<FusePollState>) -> VfsResult<u64> {
        let key = self.next_poll_key.fetch_add(1, Ordering::Relaxed).max(1);
        self.polls.lock().insert(key, Arc::downgrade(state));
        self.connection.register_poll_state(key, state);
        Ok(key)
    }

    fn retire_handle(&self, lease: &FuseHandleLease) {
        let mut handles = self.handles.lock();
        handles.remove(&(lease.nodeid, lease.fh, lease.generation));
        let last = !handles
            .values()
            .any(|other| other.nodeid == lease.nodeid && other.live.load(Ordering::Acquire));
        if last {
            // Keep handles held across cache eviction. New opens install their
            // generation under this same lock before they can expose/cache
            // data, so an old close cannot evict a newer lease's inode.
            let mut cache = self.cache.lock();
            // A failed close writeback can leave a dirty suffix whose daemon
            // fh is now dead. Keep the record as an explicit durable failure
            // rather than silently throwing it away with the last lease.
            let keep_dirty = cache
                .inodes
                .get(&lease.nodeid)
                .is_some_and(|inode| inode.ranges.iter().any(|range| range.dirty_fh.is_some()));
            if !keep_dirty && let Some(inode) = cache.inodes.remove(&lease.nodeid) {
                cache.bytes = cache.bytes.saturating_sub(fuse_inode_cost(&inode));
            }
        }
    }

    fn invalidate_cached_dentry(&self, parent: u64, name: &FsName) {
        let mut cache = self.cache.lock();
        if cache
            .dentries
            .remove(&(parent, name.as_bytes().to_vec()))
            .is_some()
        {
            cache.bytes = cache
                .bytes
                .saturating_sub(fuse_dentry_cost(name.as_bytes()));
        }
    }

    fn apply_notifications(&self) {
        let _serial = self.notification_apply.lock();
        self.apply_notifications_serialized();
    }

    /// Applies every notification already admitted by the connection while
    /// the caller owns `notification_apply`.  Dentry cache lookup uses this
    /// form so draining notifications, sampling the namespace epoch and
    /// reading the cache are one indivisible local operation.
    fn apply_notifications_serialized(&self) {
        if self.connection.is_dead() {
            let mut cache = self.cache.lock();
            cache.inodes.clear();
            cache.dentries.clear();
            cache.bytes = 0;
            for state in self
                .polls
                .lock()
                .values()
                .filter_map(alloc::sync::Weak::upgrade)
            {
                *state.events.lock() = IoEvents::HANGUP | IoEvents::ERROR;
                state.invalidate();
            }
            return;
        }
        while let Some(notification) = self.connection.take_notification() {
            self.nodes
                .lock()
                .retain(|(_, node)| node.strong_count() != 0);
            let nodes: Vec<(u64, alloc::sync::Weak<FuseNode>)> =
                self.nodes.lock().iter().cloned().collect();
            match notification {
                FuseNotification::InvalidateInode {
                    nodeid,
                    offset,
                    length,
                } => {
                    // Dirty writeback must complete before invalidation drops
                    // the corresponding cache bytes.  A fresh fh is used for
                    // a remote invalidation because the original writer may
                    // already have closed; RELEASE itself always flushes its
                    // owner first.
                    if let Some(node) = nodes
                        .iter()
                        .find(|(id, _)| *id == nodeid)
                        .and_then(|(_, node)| node.upgrade())
                    {
                        if self.has_inflight_dirty(nodeid) {
                            self.connection.requeue_notification_front(
                                FuseNotification::InvalidateInode {
                                    nodeid,
                                    offset,
                                    length,
                                },
                            );
                            break;
                        }
                        if node.flush_all_cached_serialized().is_err() {
                            if self.has_dead_dirty_owner(nodeid) {
                                self.connection.complete_notification();
                                continue;
                            }
                            self.connection.requeue_notification_front(
                                FuseNotification::InvalidateInode {
                                    nodeid,
                                    offset,
                                    length,
                                },
                            );
                            break;
                        }
                        node.metadata_valid.store(false, Ordering::Release);
                        node.namespace_epoch.fetch_add(1, Ordering::AcqRel);
                    } else if self.cache.lock().inodes.contains_key(&nodeid) {
                        if self.has_dead_dirty_owner(nodeid) {
                            self.connection.complete_notification();
                            continue;
                        }
                        self.connection.requeue_notification_front(
                            FuseNotification::InvalidateInode {
                                nodeid,
                                offset,
                                length,
                            },
                        );
                        break;
                    }
                    self.invalidate_cached_inode(nodeid, offset, length);
                }
                FuseNotification::InvalidateEntry { parent, name } => {
                    self.invalidate_cached_dentry(parent, FsName::new(&name));
                    if let Some(node) = nodes
                        .iter()
                        .find(|(id, _)| *id == parent)
                        .and_then(|(_, node)| node.upgrade())
                    {
                        node.namespace_epoch.fetch_add(1, Ordering::AcqRel);
                    }
                }
                FuseNotification::Delete {
                    parent,
                    child,
                    name,
                } => {
                    self.invalidate_cached_dentry(parent, FsName::new(&name));
                    for (nodeid, node) in &nodes {
                        if (*nodeid == parent || *nodeid == child)
                            && let Some(node) = node.upgrade()
                        {
                            node.metadata_valid.store(false, Ordering::Release);
                            node.namespace_epoch.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
                FuseNotification::Store {
                    nodeid,
                    offset,
                    data,
                } => {
                    // STORE is daemon-authoritative data.  Replacing only the
                    // notified byte range retains unaffected cached pages.
                    if let Some(node) = nodes
                        .iter()
                        .find(|(id, _)| *id == nodeid)
                        .and_then(|(_, node)| node.upgrade())
                    {
                        if self.has_inflight_dirty(nodeid) {
                            self.connection
                                .requeue_notification_front(FuseNotification::Store {
                                    nodeid,
                                    offset,
                                    data,
                                });
                            break;
                        }
                        if node.flush_all_cached_serialized().is_err() {
                            if self.has_dead_dirty_owner(nodeid) {
                                self.connection.complete_notification();
                                continue;
                            }
                            self.connection
                                .requeue_notification_front(FuseNotification::Store {
                                    nodeid,
                                    offset,
                                    data,
                                });
                            break;
                        }
                        node.metadata_valid.store(false, Ordering::Release);
                        let end = offset.saturating_add(data.len() as u64);
                        let mut metadata = node.metadata.lock();
                        metadata.size = metadata.size.max(end);
                    } else {
                        self.connection.complete_notification();
                        continue;
                    }
                    if self.cache_store(nodeid, offset, &data, None).is_err() {
                        self.connection
                            .requeue_notification_front(FuseNotification::Store {
                                nodeid,
                                offset,
                                data,
                            });
                        break;
                    }
                }
            }
            self.connection.complete_notification();
        }
    }
    fn entry_for(
        self: &Arc<Self>,
        nodeid: u64,
        metadata: Metadata,
        reference: Reference,
    ) -> VfsResult<DirEntry> {
        let node_type = metadata.node_type;
        let node = Arc::try_new(FuseNode {
            fs: self.clone(),
            nodeid,
            metadata: Mutex::new(metadata),
            metadata_valid: AtomicBool::new(false),
            attr_deadline: AtomicU64::new(0),
            entry_deadline: AtomicU64::new(0),
            entry: Mutex::new(None),
            user_data: NodeUserData::new(),
            namespace_epoch: AtomicU64::new(1),
        })
        .map_err(|_| VfsError::NoMemory)?;
        self.connection.register_node(nodeid);
        self.nodes
            .lock()
            .try_reserve(1)
            .map_err(|_| VfsError::NoMemory)?;
        self.nodes.lock().push((nodeid, Arc::downgrade(&node)));
        if node.metadata.lock().node_type == NodeType::Directory {
            Ok(DirEntry::new_dir(
                |weak| {
                    *node.entry.lock() = Some(weak);
                    DirNode::new(node)
                },
                reference,
            ))
        } else {
            Ok(DirEntry::new_file(
                FileNode::new(node),
                node_type,
                reference,
            ))
        }
    }
    fn node_entry(
        &self,
        nodeid: u64,
        metadata: Metadata,
        parent: DirEntry,
        name: &FsName,
    ) -> VfsResult<DirEntry> {
        let fs = self
            .self_ref
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
            .ok_or(VfsError::Io)?;
        fs.entry_for(nodeid, metadata, Reference::try_new(Some(parent), name)?)
    }

    fn ioctl_raw(
        &self,
        nodeid: u64,
        fh: u64,
        cmd: u32,
        arg: u64,
        flags: u32,
        input: &[u8],
        output_len: usize,
    ) -> VfsResult<FuseIoctlReply> {
        self.apply_notifications();
        let mut body = Vec::new();
        body.try_reserve(40 + input.len())
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(&fh.to_ne_bytes());
        body.extend_from_slice(&flags.to_ne_bytes());
        body.extend_from_slice(&cmd.to_ne_bytes());
        body.extend_from_slice(&arg.to_ne_bytes());
        body.extend_from_slice(&(input.len() as u32).to_ne_bytes());
        body.extend_from_slice(&(output_len as u32).to_ne_bytes());
        body.extend_from_slice(input);
        let reply = FuseConnection::reply_data(
            self.connection
                .request(FUSE_IOCTL, nodeid, &body)
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        if reply.len() < 16 {
            return Err(VfsError::Io);
        }
        let result = i32::from_ne_bytes(reply[..4].try_into().expect("bounded"));
        let reply_flags = u32::from_ne_bytes(reply[4..8].try_into().expect("bounded"));
        Ok(FuseIoctlReply {
            result,
            flags: reply_flags,
            in_iovs: u32::from_ne_bytes(reply[8..12].try_into().expect("bounded")),
            out_iovs: u32::from_ne_bytes(reply[12..16].try_into().expect("bounded")),
            data: reply[16..].to_vec(),
        })
    }
    fn ioctl_fh(
        &self,
        nodeid: u64,
        fh: u64,
        cmd: u32,
        input: &[u8],
        output_len: usize,
        directory: bool,
    ) -> VfsResult<Vec<u8>> {
        let reply = self.ioctl_raw(
            nodeid,
            fh,
            cmd,
            0,
            if directory { FUSE_IOCTL_DIR } else { 0 },
            input,
            output_len,
        )?;
        if reply.flags & FUSE_IOCTL_RETRY != 0 {
            return Err(VfsError::Io);
        }
        if reply.result < 0 {
            return Err(VfsError::from(
                LinuxError::try_from(-reply.result).unwrap_or(LinuxError::EIO),
            ));
        }
        if reply.data.len() != output_len {
            return Err(VfsError::Io);
        }
        Ok(reply.data)
    }
}

impl FilesystemOps for FuseFilesystem {
    fn name(&self) -> &str {
        "fuse"
    }
    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().expect("published FUSE root")
    }
    fn stat(&self) -> VfsResult<StatFs> {
        let reply = FuseConnection::reply_data(
            self.connection
                .request(FUSE_STATFS, FUSE_ROOT_ID, &[])
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        if reply.len() < 48 {
            return Err(VfsError::Io);
        }
        let u64_at =
            |offset| u64::from_ne_bytes(reply[offset..offset + 8].try_into().expect("checked"));
        Ok(StatFs {
            fs_type: 0x6573_5546,
            block_size: u32::from_ne_bytes(reply[40..44].try_into().expect("checked")),
            blocks: u64_at(0),
            blocks_free: u64_at(8),
            blocks_available: u64_at(16),
            file_count: u64_at(24),
            free_file_count: u64_at(32),
            name_length: u32::from_ne_bytes(reply[44..48].try_into().expect("checked")),
            fragment_size: 0,
            mount_flags: 0,
        })
    }
    fn flush(&self) -> VfsResult<()> {
        self.apply_notifications();
        let nodes: Vec<alloc::sync::Weak<FuseNode>> = self
            .nodes
            .lock()
            .iter()
            .map(|(_, node)| node.clone())
            .collect();
        let mut cached_error = None;
        for node in nodes.into_iter().filter_map(|node| node.upgrade()) {
            if let Err(error) = node.flush_all_cached() {
                cached_error.get_or_insert(error);
            }
        }
        let sync_result = FuseConnection::reply_data(
            self.connection
                .request(FUSE_SYNCFS, FUSE_ROOT_ID, &[])
                .map_err(VfsError::from)?,
        )
        .map(|_| ())
        .map_err(VfsError::from);
        cached_error.map_or(sync_result, Err)
    }
    fn unmount(&self) {
        // A mounted filesystem has already queued DESTROY during namespace
        // teardown.  Do not race that packet away when the final superblock
        // reference is dropped; an uninstalled/failed fscontext still aborts.
        if !self.connection.activate_lazy_destroy_on_final_unmount()
            && !self.connection.destroy_queued()
        {
            self.connection.abort();
        }
        self.root.lock().take();
    }
}

struct FuseNode {
    fs: Arc<FuseFilesystem>,
    nodeid: u64,
    metadata: Mutex<Metadata>,
    metadata_valid: AtomicBool,
    attr_deadline: AtomicU64,
    entry_deadline: AtomicU64,
    entry: Mutex<Option<WeakDirEntry>>,
    user_data: NodeUserData,
    namespace_epoch: AtomicU64,
}

/// Owns a decoded daemon file handle and the terminal packet reserved before
/// its OPEN/OPENDIR was sent.  It has three terminal outcomes: a successful
/// RELEASE/RELEASEDIR discards the reservation; a failed release activates
/// the already-materialized DESTROY; and a locally undelivered request merely
/// discards it.  No name based cleanup is ever attempted.
struct FuseOpenedHandle<'a> {
    node: &'a FuseNode,
    fh: u64,
    open_flags: u32,
    directory: bool,
    teardown: Arc<Mutex<Option<Request>>>,
    reservation: bool,
    armed: bool,
}

impl<'a> FuseOpenedHandle<'a> {
    fn new(
        node: &'a FuseNode,
        fh: u64,
        open_flags: u32,
        directory: bool,
        teardown: Arc<Mutex<Option<Request>>>,
    ) -> Self {
        Self {
            node,
            fh,
            open_flags,
            directory,
            teardown,
            reservation: true,
            armed: true,
        }
    }

    fn fh(&self) -> u64 {
        self.fh
    }
    fn open_flags(&self) -> u32 {
        self.open_flags
    }

    fn discard_reservation(&mut self) {
        self.teardown.lock().take();
        if self.reservation {
            self.node.fs.connection.discard_prepared_teardown();
        }
        self.reservation = false;
        self.armed = false;
    }

    fn teardown_connection(&mut self) {
        if let Some(teardown) = self.teardown.lock().take() {
            if self.reservation {
                self.node.fs.connection.activate_prepared_destroy(teardown);
            } else {
                self.node
                    .fs
                    .connection
                    .activate_materialized_destroy(teardown);
            }
        }
        self.reservation = false;
        self.armed = false;
    }

    fn release(&mut self) -> VfsResult<()> {
        if !self.armed {
            return Ok(());
        }
        let result = if self.directory {
            self.node.release_dir_fh_result(self.fh)
        } else {
            self.node.release_fh_result(self.fh)
        };
        self.armed = false;
        match result {
            Ok(()) => {
                self.teardown.lock().take();
                if self.reservation {
                    self.node.fs.connection.discard_prepared_teardown();
                }
                self.reservation = false;
                Ok(())
            }
            Err(error) => {
                self.teardown_connection();
                Err(error)
            }
        }
    }

    fn into_rollback(mut self) -> (&'a FuseNode, u64, Arc<Mutex<Option<Request>>>, bool) {
        self.armed = false;
        (self.node, self.fh, self.teardown.clone(), self.reservation)
    }
}

impl Drop for FuseOpenedHandle<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.release();
        }
    }
}

/// Owns the daemon-side FUSE_OPEN result until all local publication steps
/// have succeeded.  FUSE allocates `fh` before Rust has allocated the poll
/// state, lease, and OFD, so every fallible local step must be able to undo
/// exactly that still-unpublished generation.
struct FuseOpenRollback<'a> {
    node: &'a FuseNode,
    fh: u64,
    poll_key: Option<u64>,
    lease: Option<Arc<FuseHandleLease>>,
    teardown: Arc<Mutex<Option<Request>>>,
    reservation: bool,
    armed: bool,
}

impl<'a> FuseOpenRollback<'a> {
    fn from_opened(opened: FuseOpenedHandle<'a>) -> Self {
        let (node, fh, teardown, reservation) = opened.into_rollback();
        Self {
            node,
            fh,
            poll_key: None,
            lease: None,
            teardown,
            reservation,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
        if self.reservation {
            self.node.fs.connection.release_prepared_teardown_slot();
            self.reservation = false;
        }
    }
}

impl Drop for FuseOpenRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(lease) = self.lease.take() {
            lease.live.store(false, Ordering::Release);
            self.node.fs.retire_handle(&lease);
        }
        if let Some(key) = self.poll_key.take() {
            self.node.fs.polls.lock().remove(&key);
            self.node.fs.connection.unregister_poll_state(key);
        }
        // The known fh is released first.  If that request cannot complete,
        // the pre-reserved terminal packet retires the connection rather than
        // letting the daemon retain an otherwise identity-less resource.
        let release_failed = self.node.release_fh_result(self.fh).is_err();
        if let Some(teardown) = self.teardown.lock().take() {
            if release_failed {
                self.node.fs.connection.activate_prepared_destroy(teardown);
            } else if self.reservation {
                self.node.fs.connection.discard_prepared_teardown();
            }
        }
    }
}

/// OPENDIR has no page-cache lease, but it has the same daemon-owned fh
/// lifetime rule as OPEN: local dentry/OFD construction must not leak it.
struct FuseOpenDirRollback<'a> {
    node: &'a FuseNode,
    fh: u64,
    teardown: Arc<Mutex<Option<Request>>>,
    reservation: bool,
    armed: bool,
}
impl<'a> FuseOpenDirRollback<'a> {
    fn from_opened(opened: FuseOpenedHandle<'a>) -> Self {
        let (node, fh, teardown, reservation) = opened.into_rollback();
        Self {
            node,
            fh,
            teardown,
            reservation,
            armed: true,
        }
    }
    fn commit(&mut self) {
        self.armed = false;
        if self.reservation {
            self.node.fs.connection.release_prepared_teardown_slot();
            self.reservation = false;
        }
    }
}
impl Drop for FuseOpenDirRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let release_failed = self.node.release_dir_fh_result(self.fh).is_err();
        if let Some(teardown) = self.teardown.lock().take() {
            if release_failed {
                self.node.fs.connection.activate_prepared_destroy(teardown);
            } else if self.reservation {
                self.node.fs.connection.discard_prepared_teardown();
            }
        }
    }
}

/// Owns resources returned by a FUSE create while the local dentry is being
/// materialized.  A remote create is not rolled back by name: after daemon
/// delivery an interrupt is ambiguous, and UNLINK/RMDIR cannot identify the
/// inode installed by this exact request.  Issuing either from Drop could
/// therefore delete a later winner.  As with other remote filesystems, a
/// confirmed daemon side effect may remain visible when subsequent local
/// allocation or attribute setup fails.  A regular FUSE_CREATE provisional
/// fh is still released, and an unparseable successful CREATE tears down the
/// session so an identity-less fh cannot leak.
struct FuseCreateRollback<'a> {
    parent: &'a FuseNode,
    name: &'a FsName,
    teardown: Option<Request>,
    fh: Option<u64>,
    namespace_may_have_changed: bool,
    armed: bool,
}

impl<'a> FuseCreateRollback<'a> {
    fn try_new(parent: &'a FuseNode, name: &'a FsName) -> VfsResult<Self> {
        let teardown =
            match parent
                .fs
                .connection
                .prepare_create_teardown(FUSE_DESTROY, FUSE_ROOT_ID, &[])
            {
                Ok(request) => request,
                Err(error) => return Err(VfsError::from(error)),
            };
        Ok(Self {
            parent,
            name,
            teardown: Some(teardown),
            fh: None,
            namespace_may_have_changed: false,
            armed: true,
        })
    }

    fn request(&mut self, opcode: u32, body: &[u8]) -> AxResult<FuseReply> {
        // Once request admission begins, interruption or transport failure
        // cannot prove that the daemon did not install the name. Conservatively
        // invalidate the local namespace generation on every ambiguous exit.
        self.namespace_may_have_changed = true;
        self.parent
            .fs
            .connection
            .request(opcode, self.parent.nodeid, body)
    }

    fn publish_namespace_change(&mut self) {
        if !self.namespace_may_have_changed {
            return;
        }
        self.parent
            .fs
            .invalidate_cached_dentry(self.parent.nodeid, self.name);
        self.parent.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        self.namespace_may_have_changed = false;
    }

    fn observe_external_namespace_change(&mut self) {
        self.namespace_may_have_changed = true;
        self.publish_namespace_change();
    }

    /// The daemon returned an explicit error, so this request has no namespace
    /// side effect and only its reserved teardown slot must be released.
    fn discard(&mut self) {
        self.namespace_may_have_changed = false;
        self.armed = false;
        if self.teardown.take().is_some() {
            self.parent.fs.connection.discard_prepared_teardown();
        }
    }

    fn commit_namespace_change(&mut self) {
        self.publish_namespace_change();
        self.armed = false;
        if self.teardown.take().is_some() {
            self.parent.fs.connection.discard_prepared_teardown();
        }
    }

    fn release_fh(&mut self) -> VfsResult<()> {
        let Some(fh) = self.fh.take() else {
            return Ok(());
        };
        if let Err(error) = self.parent.release_fh_result(fh) {
            self.teardown_session();
            return Err(error);
        }
        Ok(())
    }

    fn teardown_session(&mut self) {
        self.publish_namespace_change();
        if let Some(teardown) = self.teardown.take() {
            self.parent
                .fs
                .connection
                .activate_prepared_destroy(teardown);
        }
        self.armed = false;
    }
}

impl Drop for FuseCreateRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.publish_namespace_change();
        let release_failed = self
            .fh
            .take()
            .is_some_and(|fh| self.parent.release_fh_result(fh).is_err());
        if let Some(teardown) = self.teardown.take() {
            if release_failed {
                self.parent
                    .fs
                    .connection
                    .activate_prepared_destroy(teardown);
            } else {
                self.parent.fs.connection.discard_prepared_teardown();
            }
        }
    }
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        // LOOKUP references are released without waiting: FUSE_FORGET has no
        // reply, and destructor context must never block behind a daemon.
        if self.nodeid != FUSE_ROOT_ID {
            self.fs.connection.queue_forget(self.nodeid);
        }
        self.fs.connection.unregister_node(self.nodeid);
        // This is deliberately the same handles -> cache critical section as
        // `retire_handle`.  In particular, do not observe "last" under one
        // lock and evict after dropping it: a concurrently opened generation
        // could otherwise become live between those two operations and lose
        // its just-established inode cache.
        let handles = self.fs.handles.lock();
        let live = handles
            .values()
            .any(|lease| lease.nodeid == self.nodeid && lease.live.load(Ordering::Acquire));
        if !live {
            let mut cache = self.fs.cache.lock();
            let keep_dirty = cache
                .inodes
                .get(&self.nodeid)
                .is_some_and(|inode| inode.ranges.iter().any(|range| range.dirty_fh.is_some()));
            if !keep_dirty && let Some(inode) = cache.inodes.remove(&self.nodeid) {
                cache.bytes = cache.bytes.saturating_sub(fuse_inode_cost(&inode));
            }
        }
    }
}

impl FuseNode {
    fn parent_entry(&self) -> VfsResult<DirEntry> {
        self.entry
            .lock()
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::NotFound)
    }
    fn request(&self, opcode: u32, body: &[u8]) -> VfsResult<Vec<u8>> {
        self.fs.apply_notifications();
        FuseConnection::reply_data(
            self.fs
                .connection
                .request(opcode, self.nodeid, body)
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)
    }
    fn refresh(&self) -> VfsResult<Metadata> {
        let now = axhal::time::monotonic_time_nanos();
        if self.metadata_valid.load(Ordering::Acquire)
            && now < self.attr_deadline.load(Ordering::Acquire)
        {
            return Ok(self.metadata.lock().clone());
        }
        let reply = self.request(FUSE_GETATTR, &[])?;
        let metadata = parse_attr_out(&reply)?;
        *self.metadata.lock() = metadata.clone();
        self.attr_deadline.store(
            fuse_ttl_deadline(&reply, 0, 8).unwrap_or(now),
            Ordering::Release,
        );
        self.metadata_valid.store(true, Ordering::Release);
        Ok(metadata)
    }

    fn flush_all_cached(&self) -> VfsResult<()> {
        let _serial = self.fs.notification_apply.lock();
        self.flush_all_cached_serialized()
    }

    /// Caller holds `notification_apply`, the shared per-superblock
    /// writeback/invalidation gate.
    fn flush_all_cached_serialized(&self) -> VfsResult<()> {
        if !self.fs.cache_active() {
            return Ok(());
        }
        let dirty = self.fs.take_dirty(self.nodeid, None);
        for (index, range) in dirty.iter().enumerate() {
            let Some(owner) = range
                .owner
                .as_ref()
                .filter(|owner| owner.live.load(Ordering::Acquire))
            else {
                self.fs.restore_dirty(self.nodeid, dirty[index..].to_vec());
                // The only fh permitted to write this range was released
                // after an unsuccessful writeback. Retrying through a new
                // owner would violate FUSE open-file semantics; make this a
                // terminal I/O failure for this inode rather than an invalid
                // cross-owner retry. The notification path recognizes this
                // retained failed owner and drops the stale notification
                // rather than endlessly requeueing it.
                return Err(VfsError::Io);
            };
            match fuse_write(&self.fs, self.nodeid, owner.fh, &range.data, range.offset) {
                Ok(written) if written == range.data.len() => self
                    .fs
                    .complete_dirty(self.nodeid, core::slice::from_ref(range)),
                Ok(written) => {
                    let staged = (|| {
                        if written < range.data.len() {
                            self.fs.cache_store(
                                self.nodeid,
                                range.offset + written as u64,
                                &range.data[written..],
                                range.owner.clone(),
                            )?;
                        }
                        if written != 0 {
                            self.fs.cache_store(
                                self.nodeid,
                                range.offset,
                                &range.data[..written],
                                None,
                            )?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = staged {
                        self.fs.restore_dirty(self.nodeid, dirty[index..].to_vec());
                        return Err(error);
                    }
                    self.fs
                        .complete_dirty(self.nodeid, core::slice::from_ref(range));
                    self.fs
                        .restore_dirty(self.nodeid, dirty[index + 1..].to_vec());
                    return Err(VfsError::Io);
                }
                Err(error) => {
                    self.fs.restore_dirty(self.nodeid, dirty[index..].to_vec());
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    fn open_fh(&self, flags: u32) -> VfsResult<FuseOpenedHandle<'_>> {
        self.open_fh_tracked(FUSE_OPEN, flags, false)
    }
    fn open_dir_fh(&self, flags: u32) -> VfsResult<FuseOpenedHandle<'_>> {
        self.open_fh_tracked(FUSE_OPENDIR, flags, true)
    }
    /// Reserve a no-reply DESTROY before the daemon can see OPEN/OPENDIR.
    /// Any FUSE success reply other than the fixed-size `fuse_open_out` is a
    /// protocol violation.  A reply shorter than its `fh` may represent a
    /// daemon allocation the kernel cannot name in RELEASE, so it is
    /// connection-terminal rather than ordinary EIO cleanup.
    fn open_fh_tracked(
        &self,
        opcode: u32,
        flags: u32,
        directory: bool,
    ) -> VfsResult<FuseOpenedHandle<'_>> {
        let delivery = Arc::try_new(AtomicBool::new(false)).map_err(|_| VfsError::NoMemory)?;
        // This owner is allocated before OPEN reaches the daemon, so both a
        // pre-publication rollback and a published OFD can retain the exact
        // materialized terminal packet without a later allocation point.
        let teardown_owner = Arc::try_new(Mutex::new(None)).map_err(|_| VfsError::NoMemory)?;
        let teardown = self
            .fs
            .connection
            .prepare_create_teardown(FUSE_DESTROY, FUSE_ROOT_ID, &[])
            .map_err(VfsError::from)?;
        *teardown_owner.lock() = Some(teardown);
        let mut input = [0u8; 8];
        input[..4].copy_from_slice(&flags.to_ne_bytes());
        let reply =
            self.fs
                .connection
                .request_tracked(opcode, self.nodeid, &input, Some(delivery.clone()));
        match reply {
            Ok(FuseReply::Data(reply)) => {
                let fh = reply
                    .get(..8)
                    .and_then(|value| value.try_into().ok())
                    .map(u64::from_ne_bytes);
                if reply.len() != 16 {
                    // The first half can identify an fh, so RELEASE it before
                    // retiring the session.  A shorter reply cannot supply
                    // exactly one fixed-size fuse_open_out and is never
                    // accepted with flags silently defaulted or trailing
                    // protocol bytes ignored.
                    let release_failed = fh.is_some_and(|fh| {
                        if directory {
                            self.release_dir_fh_result(fh).is_err()
                        } else {
                            self.release_fh_result(fh).is_err()
                        }
                    });
                    let teardown = teardown_owner.lock().take();
                    if let Some(teardown) = teardown {
                        if release_failed || fh.is_none() {
                            self.fs.connection.activate_prepared_destroy(teardown);
                        } else {
                            self.fs.connection.discard_prepared_teardown();
                        }
                    }
                    return Err(VfsError::Io);
                }
                let fh = fh.expect("16-byte FUSE open reply contains fh");
                let open_flags =
                    u32::from_ne_bytes(reply[8..12].try_into().expect("bounded FUSE open flags"));
                Ok(FuseOpenedHandle::new(
                    self,
                    fh,
                    open_flags,
                    directory,
                    teardown_owner,
                ))
            }
            Ok(FuseReply::Error(errno)) => {
                teardown_owner.lock().take();
                self.fs.connection.discard_prepared_teardown();
                Err(VfsError::from(
                    LinuxError::try_from(errno).unwrap_or(LinuxError::EIO),
                ))
            }
            Ok(FuseReply::Cancelled) => {
                let teardown = teardown_owner.lock().take();
                if delivery.load(Ordering::Acquire) {
                    if let Some(teardown) = teardown {
                        self.fs.connection.activate_prepared_destroy(teardown);
                    }
                } else {
                    self.fs.connection.discard_prepared_teardown();
                }
                Err(VfsError::from(LinuxError::ENODEV))
            }
            Err(error) => {
                let teardown = teardown_owner.lock().take();
                if delivery.load(Ordering::Acquire) {
                    if let Some(teardown) = teardown {
                        self.fs.connection.activate_prepared_destroy(teardown);
                    }
                } else {
                    self.fs.connection.discard_prepared_teardown();
                }
                Err(VfsError::from(error))
            }
        }
    }

    fn release_fh_result(&self, fh: u64) -> VfsResult<()> {
        if self.fs.connection.is_dead() {
            return Ok(());
        }
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&fh.to_ne_bytes());
        self.request(FUSE_RELEASE, &body).map(|_| ())
    }
    fn release_fh(&self, fh: u64) {
        let _ = self.release_fh_result(fh);
    }
    fn release_dir_fh_result(&self, fh: u64) -> VfsResult<()> {
        if self.fs.connection.is_dead() {
            return Ok(());
        }
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&fh.to_ne_bytes());
        self.request(FUSE_RELEASEDIR, &body).map(|_| ())
    }
    fn release_dir_fh(&self, fh: u64) {
        let _ = self.release_dir_fh_result(fh);
    }

    /// Applies the VFS create transaction's attributes after the daemon has
    /// made the object, but before its dentry is returned to the namespace
    /// publisher.  FUSE's CREATE/MKDIR/MKNOD/SYMLINK messages do not carry
    /// project ids, default ACL records, or an idmapped final owner.  Treating
    /// that wire limitation as a permanent `EOPNOTSUPP` makes a FUSE mount
    /// silently fail ordinary VFS inheritance.  Instead, use the daemon's
    /// native setattr/ioctl/xattr operations. A later local failure does not
    /// unlink by name because the FUSE protocol cannot prove that a concurrent
    /// replacement is still the object installed by this request.
    fn apply_prepared_create(
        &self,
        entry: &DirEntry,
        options: &NamedCreateOptions,
    ) -> VfsResult<()> {
        let node = entry.downcast::<FuseNode>()?;
        if let Some(owner) = options.owner {
            let metadata = fuse_setattr(
                &node.fs,
                node.nodeid,
                None,
                MetadataUpdate {
                    owner: Some(owner),
                    ..Default::default()
                },
            )?;
            *node.metadata.lock() = metadata;
            node.metadata_valid.store(true, Ordering::Release);
        }

        let attrs = &options.initial_attributes;
        if attrs.project_id.is_some() || attrs.project_inherit {
            let mut file_attr = FileAttrProvider::get_file_attr(node.as_ref())?;
            if let Some(project_id) = attrs.project_id {
                file_attr.project_id = project_id;
            }
            // FS_XFLAG_PROJINHERIT.  The fileattr provider owns the native
            // representation; this only selects the UAPI bit requested by
            // the already-admitted parent inheritance snapshot.
            if attrs.project_inherit {
                file_attr.xflags |= 0x0000_0200;
            }
            FileAttrProvider::set_file_attr(node.as_ref(), file_attr)?;
        }
        if let Some(access) = attrs.access_acl.as_ref() {
            node.set_xattr(
                crate::file::posix_acl::ACCESS_XATTR,
                access.as_bytes(),
                XattrSetMode::Upsert,
            )?;
        }
        if let Some(default) = attrs.default_acl.as_ref() {
            node.set_xattr(
                crate::file::posix_acl::DEFAULT_XATTR,
                default.as_bytes(),
                XattrSetMode::Upsert,
            )?;
        }
        Ok(())
    }

    fn finish_prepared_create(
        &self,
        entry: DirEntry,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        self.apply_prepared_create(&entry, options)?;
        Ok(entry)
    }

    fn lookup_remote(&self, name: &FsName) -> VfsResult<DirEntry> {
        // Bind a cache publication to the parent namespace generation seen
        // before the daemon request.  An invalidation arriving while LOOKUP
        // is in flight may make the returned reference immediately stale (as
        // with any racing path lookup), but it must not repopulate the dentry
        // cache after that invalidation has advanced the generation.
        let epoch = self.namespace_epoch.load(Ordering::Acquire);
        let mut body = Vec::new();
        body.try_reserve(name.as_bytes().len() + 1)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        let reply = self.request(FUSE_LOOKUP, &body)?;
        let (nodeid, metadata, entry_deadline, attr_deadline) = parse_entry_out(&reply)?;
        let entry = self
            .fs
            .node_entry(nodeid, metadata, self.parent_entry()?, name)?;
        if let Ok(node) = entry.downcast::<FuseNode>() {
            node.entry_deadline.store(entry_deadline, Ordering::Release);
            node.attr_deadline.store(attr_deadline, Ordering::Release);
            node.metadata_valid.store(true, Ordering::Release);
        }
        if self.fs.cache_active() && self.namespace_epoch.load(Ordering::Acquire) == epoch {
            let mut key = Vec::new();
            key.try_reserve(name.as_bytes().len())
                .map_err(|_| VfsError::NoMemory)?;
            key.extend_from_slice(name.as_bytes());
            let mut cache = self.fs.cache.lock();
            // Recheck under the cache lock so notification application cannot
            // advance the parent and remove an entry between validation and
            // this insertion.
            if self.namespace_epoch.load(Ordering::Acquire) == epoch {
                let cost = fuse_dentry_cost(&key);
                if cache.dentries.len() < MAX_FUSE_DENTRY_CACHE_ENTRIES
                    && cost <= MAX_FUSE_CACHE_BYTES.saturating_sub(cache.bytes)
                {
                    let old = cache.dentries.insert(
                        (self.nodeid, key),
                        FuseCachedDentry {
                            entry: entry.downgrade(),
                            deadline: entry_deadline,
                            epoch,
                        },
                    );
                    cache.bytes += cost;
                    if old.is_some() {
                        cache.bytes = cache.bytes.saturating_sub(cost);
                    }
                }
            }
        }
        Ok(entry)
    }

    fn finish_named_create_error(
        &self,
        name: &FsName,
        disposition: CreateDisposition,
        errno: i32,
        rollback: &mut FuseCreateRollback<'_>,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        // An EEXIST reply is the daemon-side serialization point for
        // OpenOrCreate.  A lookup performed before the create request is only
        // a fast path: another client may publish the name before the daemon
        // consumes our exclusive CREATE/MKDIR/MKNOD.  Resolve the winner only
        // after EEXIST and never apply this caller's prepared attributes to it.
        let linux_error = LinuxError::try_from(errno).unwrap_or(LinuxError::EIO);
        if linux_error == LinuxError::EEXIST {
            // Our exclusive create did not mutate the directory, but EEXIST
            // proves the requested name is present. Retire the old generation
            // before either returning EEXIST or issuing the authoritative
            // OpenOrCreate LOOKUP; this also excludes older in-flight LOOKUP
            // publication under the prior generation.
            rollback.observe_external_namespace_change();
            rollback.discard();
            if disposition == CreateDisposition::OpenOrCreate {
                return self.lookup_remote(name).map(|entry| CreateOutcome {
                    entry,
                    created: false,
                });
            }
            return Err(VfsError::from(linux_error));
        }
        rollback.discard();
        Err(VfsError::from(linux_error))
    }

    fn with_fh<R>(&self, flags: u32, operation: impl FnOnce(u64) -> VfsResult<R>) -> VfsResult<R> {
        let mut opened = self.open_fh(flags)?;
        let result = operation(opened.fh());
        let release = opened.release();
        match (result, release) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn with_fh_cache_transaction<R>(
        &self,
        operation: impl FnOnce(u64) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let mut opened = self.open_fh(0)?;
        let fh = opened.fh();
        let result = {
            let _serial = self.fs.notification_apply.lock();
            self.flush_all_cached_serialized()
                .and_then(|_| operation(fh))
        };
        let release = opened.release();
        match (result, release) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn attribute_ioctl(&self, cmd: u32, input: &[u8], output_len: usize) -> VfsResult<Vec<u8>> {
        if self.metadata.lock().node_type == NodeType::Directory {
            let mut opened = self.open_dir_fh(0)?;
            let result = self
                .fs
                .ioctl_fh(self.nodeid, opened.fh(), cmd, input, output_len, true);
            let release = opened.release();
            match (result, release) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        } else {
            self.with_fh(0, |fh| {
                self.fs
                    .ioctl_fh(self.nodeid, fh, cmd, input, output_len, false)
            })
        }
    }

    fn rename_into(
        &self,
        dst: &FuseNode,
        src_name: &FsName,
        dst_name: &FsName,
        flags: u32,
    ) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.fs, &dst.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let mut body = Vec::new();
        body.try_reserve(16 + src_name.as_bytes().len() + dst_name.as_bytes().len() + 2)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(&dst.nodeid.to_ne_bytes());
        body.extend_from_slice(&flags.to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        body.extend_from_slice(src_name.as_bytes());
        body.push(0);
        body.extend_from_slice(dst_name.as_bytes());
        body.push(0);
        // RENAME2 is the v6.18 wire operation, including its required flag
        // field.  Pre-7.23 daemons never received it, so keep their original
        // RENAME framing instead of manufacturing an unsupported operation.
        let result = if self
            .fs
            .connection
            .init()
            .is_some_and(|init| init.minor >= 23)
        {
            self.request(FUSE_RENAME2, &body).map(|_| ())
        } else if flags == 0 {
            let mut legacy = Vec::new();
            legacy
                .try_reserve(8 + src_name.as_bytes().len() + dst_name.as_bytes().len() + 2)
                .map_err(|_| VfsError::NoMemory)?;
            legacy.extend_from_slice(&dst.nodeid.to_ne_bytes());
            legacy.extend_from_slice(src_name.as_bytes());
            legacy.push(0);
            legacy.extend_from_slice(dst_name.as_bytes());
            legacy.push(0);
            self.request(FUSE_RENAME, &legacy).map(|_| ())
        } else {
            Err(VfsError::OperationNotSupported)
        };
        if result.is_ok() {
            self.fs.invalidate_cached_dentry(self.nodeid, src_name);
            self.fs.invalidate_cached_dentry(dst.nodeid, dst_name);
            self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            dst.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        }
        result
    }
}

fn fuse_setattr(
    fs: &FuseFilesystem,
    nodeid: u64,
    fh: Option<u64>,
    update: MetadataUpdate,
) -> VfsResult<Metadata> {
    // fuse_setattr_in is fixed-size.  Keep all fields zero unless the valid
    // bitmap authorizes them; in particular, never leak a stale local inode
    // snapshot back to a remote daemon on an unrelated setattr.
    let mut input = [0u8; 88];
    let mut valid = 0u32;
    if let Some(fh) = fh {
        valid |= 1 << 6;
        input[8..16].copy_from_slice(&fh.to_ne_bytes());
    }
    if let Some(mode) = update.mode {
        valid |= 1;
        input[68..72].copy_from_slice(&(mode.bits() as u32).to_ne_bytes());
    }
    if let Some((uid, gid)) = update.owner {
        valid |= 2 | 4;
        input[76..80].copy_from_slice(&uid.to_ne_bytes());
        input[80..84].copy_from_slice(&gid.to_ne_bytes());
    }
    if let Some(atime) = update.atime {
        valid |= 1 << 4;
        input[32..40].copy_from_slice(&(atime.seconds() as u64).to_ne_bytes());
        input[56..60].copy_from_slice(&atime.subsec_nanos().to_ne_bytes());
    }
    if let Some(mtime) = update.mtime {
        valid |= 1 << 5;
        input[40..48].copy_from_slice(&(mtime.seconds() as u64).to_ne_bytes());
        input[60..64].copy_from_slice(&mtime.subsec_nanos().to_ne_bytes());
    }
    input[..4].copy_from_slice(&valid.to_ne_bytes());
    let reply = FuseConnection::reply_data(
        fs.connection
            .request(FUSE_SETATTR, nodeid, &input)
            .map_err(VfsError::from)?,
    )
    .map_err(VfsError::from)?;
    parse_attr_out(&reply)
}

impl NodeOps for FuseNode {
    fn inode(&self) -> u64 {
        self.nodeid
    }
    fn object_key(&self) -> ObjectKey {
        ObjectKey::new(self.fs.identity, self.nodeid, 0)
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        self.refresh()
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let metadata = fuse_setattr(&self.fs, self.nodeid, None, update)?;
        *self.metadata.lock() = metadata;
        Ok(())
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        let cached = self.flush_all_cached();
        let remote = self.request(FUSE_FSYNC, &[0; 16]).map(|_| ());
        cached.and(remote)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.user_data)
    }
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        Some(self)
    }
}

impl Pollable for FuseNode {
    fn poll(&self) -> IoEvents {
        if self.fs.connection.is_dead() {
            IoEvents::HANGUP | IoEvents::ERROR
        } else {
            IoEvents::READABLE | IoEvents::WRITABLE
        }
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl DirNodeOps for FuseNode {
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        {
            let _serial = self.fs.notification_apply.lock();
            self.fs.apply_notifications_serialized();
            if !self.fs.cache_active() {
                drop(_serial);
                return self.lookup_remote(name);
            }
            let now = axhal::time::monotonic_time_nanos();
            let epoch = self.namespace_epoch.load(Ordering::Acquire);
            let cached = self
                .fs
                .cache
                .lock()
                .dentries
                .get(&(self.nodeid, name.as_bytes().to_vec()))
                .filter(|cached| cached.epoch == epoch && now < cached.deadline)
                .and_then(|cached| cached.entry.upgrade());
            if let Some(entry) = cached {
                return Ok(entry);
            }
        }
        self.lookup_remote(name)
    }
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        DirNodeOps::open_handle(self, 0)?
            .ok_or(VfsError::Io)?
            .read_dir(offset, sink)
    }
    fn namespace_epoch(&self) -> u64 {
        self.namespace_epoch.load(Ordering::Acquire)
    }
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::RegularFile
                | NodeType::Directory
                | NodeType::CharacterDevice
                | NodeType::BlockDevice
                | NodeType::Fifo
                | NodeType::Socket
        )
    }
    fn supports_symlink(&self) -> bool {
        true
    }
    fn supports_hard_links(&self) -> bool {
        true
    }
    fn supports_unlink(&self) -> bool {
        true
    }
    fn supports_rmdir(&self) -> bool {
        true
    }
    fn supports_rename(&self) -> bool {
        true
    }
    fn supports_rename_exchange(&self) -> bool {
        self.fs
            .connection
            .init()
            .is_some_and(|init| init.minor >= 23)
    }
    fn supports_rename_whiteout(&self) -> bool {
        self.fs
            .connection
            .init()
            .is_some_and(|init| init.minor >= 23)
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        if disposition == CreateDisposition::OpenOrCreate {
            match self.lookup(name) {
                Ok(entry) => {
                    return Ok(CreateOutcome {
                        entry,
                        created: false,
                    });
                }
                Err(VfsError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let mut body = Vec::new();
        let mode = ((options.node_type as u32) << 12) | u32::from(options.permission.bits());
        match options.node_type {
            NodeType::Directory => {
                body.extend_from_slice(&mode.to_ne_bytes());
                body.extend_from_slice(&0u32.to_ne_bytes());
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                let mut rollback = FuseCreateRollback::try_new(self, name)?;
                let reply = match rollback.request(FUSE_MKDIR, &body) {
                    Ok(FuseReply::Data(reply)) => reply,
                    Ok(FuseReply::Error(errno)) => {
                        return self.finish_named_create_error(
                            name,
                            disposition,
                            errno,
                            &mut rollback,
                        );
                    }
                    // A cancellation/transport failure can happen after the
                    // daemon has consumed CREATE.  Keep rollback custody.
                    Ok(FuseReply::Cancelled) => return Err(VfsError::from(LinuxError::ENODEV)),
                    Err(error) => return Err(VfsError::from(error)),
                };
                let (id, meta, ..) = parse_entry_out(&reply)?;
                let entry = self.fs.node_entry(id, meta, self.parent_entry()?, name)?;
                let entry = self.finish_prepared_create(entry, options)?;
                rollback.commit_namespace_change();
                Ok(CreateOutcome {
                    entry,
                    created: true,
                })
            }
            NodeType::RegularFile => {
                // FUSE_CREATE makes name installation and daemon open one
                // transaction.  VFS returns an entry here, so release this
                // provisional handle; the subsequent open obtains its own
                // OFD-private fh rather than sharing a creation handle.
                // CREATE without O_EXCL may return an already existing name;
                // it cannot then claim the caller's prepared attributes were
                // installed.  Create exclusively and convert EEXIST to the
                // single OpenOrCreate lookup result instead.
                let open_flags =
                    (linux_raw_sys::general::O_CREAT | linux_raw_sys::general::O_EXCL) as u32;
                body.extend_from_slice(&open_flags.to_ne_bytes());
                body.extend_from_slice(&mode.to_ne_bytes());
                body.extend_from_slice(&0u32.to_ne_bytes());
                body.extend_from_slice(&0u32.to_ne_bytes());
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                let mut rollback = FuseCreateRollback::try_new(self, name)?;
                let reply = match rollback.request(FUSE_CREATE, &body) {
                    Ok(FuseReply::Data(reply)) => reply,
                    Ok(FuseReply::Error(errno)) => {
                        return self.finish_named_create_error(
                            name,
                            disposition,
                            errno,
                            &mut rollback,
                        );
                    }
                    Ok(FuseReply::Cancelled) => return Err(VfsError::from(LinuxError::ENODEV)),
                    Err(error) => return Err(VfsError::from(error)),
                };
                // fuse_create_out is entry_out followed by open_out.  A
                // successful daemon CREATE necessarily owns a provisional fh
                // even when a malformed reply prevents entry parsing.
                rollback.fh = reply
                    .get(128..136)
                    .and_then(|v| v.try_into().ok())
                    .map(u64::from_ne_bytes);
                if rollback.fh.is_none() {
                    // The daemon has created an open handle but made its
                    // identity unrecoverable.  Tear the connection down so
                    // the daemon releases every handle rather than leaking
                    // this one forever.
                    rollback.teardown_session();
                    return Err(VfsError::Io);
                }
                if reply.len() < 144 {
                    return Err(VfsError::Io);
                }
                let (id, meta, ..) = parse_entry_out(&reply)?;
                let entry = self.fs.node_entry(id, meta, self.parent_entry()?, name)?;
                let entry = self.finish_prepared_create(entry, options)?;
                rollback.release_fh()?;
                rollback.commit_namespace_change();
                Ok(CreateOutcome {
                    entry,
                    created: true,
                })
            }
            _ => {
                let rdev = options.rdev.unwrap_or_default().0;
                body.extend_from_slice(&mode.to_ne_bytes());
                body.extend_from_slice(&(rdev as u32).to_ne_bytes());
                body.extend_from_slice(&0u32.to_ne_bytes());
                body.extend_from_slice(&0u32.to_ne_bytes());
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                let mut rollback = FuseCreateRollback::try_new(self, name)?;
                let reply = match rollback.request(FUSE_MKNOD, &body) {
                    Ok(FuseReply::Data(reply)) => reply,
                    Ok(FuseReply::Error(errno)) => {
                        return self.finish_named_create_error(
                            name,
                            disposition,
                            errno,
                            &mut rollback,
                        );
                    }
                    Ok(FuseReply::Cancelled) => return Err(VfsError::from(LinuxError::ENODEV)),
                    Err(error) => return Err(VfsError::from(error)),
                };
                let (id, meta, ..) = parse_entry_out(&reply)?;
                let entry = self.fs.node_entry(id, meta, self.parent_entry()?, name)?;
                let entry = self.finish_prepared_create(entry, options)?;
                rollback.commit_namespace_change();
                Ok(CreateOutcome {
                    entry,
                    created: true,
                })
            }
        }
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        self.create_symlink_prepared(
            name,
            target,
            &NamedCreateOptions {
                node_type: NodeType::Symlink,
                permission,
                owner: user,
                rdev: None,
                initial_data: None,
                initial_attributes: Default::default(),
            },
        )
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink {
            return Err(VfsError::InvalidInput);
        }
        let mut rollback = FuseCreateRollback::try_new(self, name)?;
        let mut body = Vec::new();
        body.try_reserve(target.as_bytes().len() + name.as_bytes().len() + 2)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(target.as_bytes());
        body.push(0);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        let reply = match rollback.request(FUSE_SYMLINK, &body) {
            Ok(FuseReply::Data(reply)) => reply,
            Ok(FuseReply::Error(errno)) => {
                let linux_error = LinuxError::try_from(errno).unwrap_or(LinuxError::EIO);
                if linux_error == LinuxError::EEXIST {
                    rollback.observe_external_namespace_change();
                }
                rollback.discard();
                return Err(VfsError::from(linux_error));
            }
            Ok(FuseReply::Cancelled) => return Err(VfsError::from(LinuxError::ENODEV)),
            Err(error) => return Err(VfsError::from(error)),
        };
        let (id, meta, ..) = parse_entry_out(&reply)?;
        let entry = self.fs.node_entry(id, meta, self.parent_entry()?, name)?;
        let entry = self.finish_prepared_create(entry, options)?;
        rollback.commit_namespace_change();
        Ok(entry)
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        let target = node.downcast::<FuseNode>()?;
        let mut body = Vec::new();
        body.extend_from_slice(&target.nodeid.to_ne_bytes());
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        let reply = self.request(FUSE_LINK, &body)?;
        let (id, meta, ..) = parse_entry_out(&reply)?;
        self.fs.node_entry(id, meta, self.parent_entry()?, name)
    }
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let mut body = Vec::new();
        body.extend_from_slice(request.name.as_bytes());
        body.push(0);
        self.request(
            if request.is_dir {
                FUSE_RMDIR
            } else {
                FUSE_UNLINK
            },
            &body,
        )
        .map(|_| {
            self.fs.invalidate_cached_dentry(self.nodeid, request.name);
        })
    }
    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst = request.dst_dir.downcast::<FuseNode>()?;
        self.rename_into(dst.as_ref(), request.src_name, request.dst_name, 0)
    }
    fn rename_whiteout(&self, request: axfs_ng_vfs::RenameWhiteoutRequest<'_>) -> VfsResult<()> {
        let dst = request.dst_dir.downcast::<FuseNode>()?;
        self.rename_into(dst.as_ref(), request.src_name, request.dst_name, 0x4)
    }
    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        let dst = request.dst_dir.downcast::<FuseNode>()?;
        self.rename_into(dst.as_ref(), request.src_name, request.dst_name, 0x2)
    }
    fn open_handle(&self, flags: u32) -> VfsResult<Option<Arc<dyn DirNodeOps>>> {
        let opened = self.open_dir_fh(flags)?;
        let fh = opened.fh();
        let mut rollback = FuseOpenDirRollback::from_opened(opened);
        // Retain the exact published inode node.  Constructing an anonymous
        // surrogate here loses its parent reference, breaks relative LOOKUP
        // and leaks a second FUSE lookup reference on every opendir.
        let node = self.parent_entry()?.downcast::<FuseNode>()?;
        let open = Arc::try_new(FuseOpenDir {
            node,
            fh,
            teardown: rollback.teardown.clone(),
            // A directory handle is an OFD-owned daemon resource.  Serialize
            // every operation that borrows it with RELEASEDIR so a concurrent
            // close cannot make an already queued FSYNCDIR/READDIR refer to a
            // released fh.
            operation: Mutex::new(()),
            released: AtomicBool::new(false),
        })
        .map(|v| Some(v as Arc<dyn DirNodeOps>))
        .map_err(|_| VfsError::NoMemory)?;
        rollback.commit();
        Ok(open)
    }
}

impl FileNodeOps for FuseNode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if self.metadata.lock().node_type == NodeType::Symlink {
            let link = self.request(FUSE_READLINK, &[])?;
            let start = (offset as usize).min(link.len());
            let count = buf.len().min(link.len() - start);
            buf[..count].copy_from_slice(&link[start..start + count]);
            Ok(count)
        } else {
            self.with_fh_cache_transaction(|fh| fuse_read(&self.fs, self.nodeid, fh, buf, offset))
        }
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.with_fh_cache_transaction(|fh| {
            let written = fuse_write(&self.fs, self.nodeid, fh, buf, offset)?;
            self.fs
                .invalidate_cached_inode(self.nodeid, offset as i64, written as i64);
            Ok(written)
        })
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.with_fh_cache_transaction(|fh| {
            let offset = self.metadata()?.size;
            let count = fuse_write(&self.fs, self.nodeid, fh, buf, offset)?;
            self.fs
                .invalidate_cached_inode(self.nodeid, offset as i64, count as i64);
            Ok((count, offset.saturating_add(count as u64)))
        })
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        let metadata = self.with_fh_cache_transaction(|fh| {
            let metadata = fuse_set_len(&self.fs, self.nodeid, fh, len)?;
            self.fs.invalidate_cached_inode(self.nodeid, len as i64, -1);
            Ok(metadata)
        })?;
        *self.metadata.lock() = metadata;
        self.metadata_valid.store(true, Ordering::Release);
        Ok(())
    }
    fn set_symlink(&self, _target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
    fn open_handle(
        &self,
        read: bool,
        write: bool,
        flags: u32,
    ) -> VfsResult<Option<Arc<dyn FileNodeOps>>> {
        // Keep the inode identity alive for the OFD lifetime. In particular,
        // an unlinked-but-open file remains a known notification target.
        let node = self.fs.retain_node(self.nodeid).ok_or(VfsError::NotFound)?;
        let opened = self.open_fh(flags)?;
        let fh = opened.fh();
        // FOPEN_DIRECT_IO suppresses the kernel page cache for this exact
        // handle even when the connection supports writeback caching.
        let open_flags = opened.open_flags();
        let mut rollback = FuseOpenRollback::from_opened(opened);
        // Unless the daemon explicitly grants KEEP_CACHE, a new open must
        // not inherit clean bytes from a prior open.  Dirty bytes are first
        // serialized through their owner/fallback handle and only then is the
        // old cache discarded.
        if self.fs.cache_active() && open_flags & 2 == 0 {
            let _serial = self.fs.notification_apply.lock();
            self.flush_all_cached_serialized()?;
            self.fs.invalidate_cached_inode(self.nodeid, 0, -1);
        }
        let poll = FusePollState::try_new().map_err(VfsError::from)?;
        let poll_key = self.fs.register_poll(&poll)?;
        rollback.poll_key = Some(poll_key);
        let generation = self
            .fs
            .next_handle_generation
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        let lease = Arc::try_new(FuseHandleLease {
            nodeid: self.nodeid,
            fh,
            generation,
            live: AtomicBool::new(true),
        })
        .map_err(|_| VfsError::NoMemory)?;
        rollback.lease = Some(lease.clone());
        // `retire_handle` holds this lock through its last-lease cache
        // decision.  Publishing the new generation here therefore serializes
        // open and close: an old last close cannot evict this lease's cache.
        self.fs
            .handles
            .lock()
            .insert((self.nodeid, fh, generation), lease.clone());
        let node = Arc::try_new(FuseOpenFile {
            fs: self.fs.clone(),
            nodeid: self.nodeid,
            node,
            metadata: Mutex::new(self.metadata.lock().clone()),
            fh,
            read,
            write,
            cache: self.fs.cache_active() && open_flags & 1 == 0,
            lease: lease.clone(),
            poll,
            poll_key,
            teardown: rollback.teardown.clone(),
            released: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)?;
        rollback.commit();
        Ok(Some(node))
    }
}

impl XattrProvider for FuseNode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        let mut body = Vec::new();
        body.try_reserve(name.len() + 9)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(&0u32.to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        body.extend_from_slice(name);
        body.push(0);
        let probe = self.request(FUSE_GETXATTR, &body)?;
        let size = probe
            .get(..4)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_ne_bytes)
            .ok_or(VfsError::Io)? as usize;
        if size > MAX_REQUEST_BYTES {
            return Err(VfsError::InvalidInput);
        };
        body[..4].copy_from_slice(&(size as u32).to_ne_bytes());
        let value = self.request(FUSE_GETXATTR, &body)?;
        if value.len() != size {
            return Err(VfsError::Io);
        }
        Ok(value)
    }
    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let probe = self.request(FUSE_LISTXATTR, &[0; 8])?;
        let size = probe
            .get(..4)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_ne_bytes)
            .ok_or(VfsError::Io)? as usize;
        if size > MAX_REQUEST_BYTES {
            return Err(VfsError::InvalidInput);
        };
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&(size as u32).to_ne_bytes());
        let value = self.request(FUSE_LISTXATTR, &body)?;
        if value.len() != size {
            return Err(VfsError::Io);
        }
        Ok(value)
    }
    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        let flags = match mode {
            XattrSetMode::Create => 1,
            XattrSetMode::Replace => 2,
            XattrSetMode::Upsert => 0u32,
            XattrSetMode::CreateAndReplace => 3u32,
        };
        let mut body = Vec::new();
        body.try_reserve(16 + name.len() + value.len() + 1)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(&(value.len() as u32).to_ne_bytes());
        body.extend_from_slice(&flags.to_ne_bytes());
        body.extend_from_slice(&0u64.to_ne_bytes());
        body.extend_from_slice(name);
        body.push(0);
        body.extend_from_slice(value);
        self.request(FUSE_SETXATTR, &body).map(|_| ())
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        let mut body = Vec::new();
        body.try_reserve(name.len() + 1)
            .map_err(|_| VfsError::NoMemory)?;
        body.extend_from_slice(name);
        body.push(0);
        self.request(FUSE_REMOVEXATTR, &body).map(|_| ())
    }
}

impl FileAttrProvider for FuseNode {
    fn get_file_attr(&self) -> VfsResult<FileAttr> {
        let bytes = self.attribute_ioctl(crate::file::inode_flags::FS_IOC_FSGETXATTR, &[], 20)?;
        let word = |index: usize| -> VfsResult<u32> {
            bytes
                .get(index * 4..index * 4 + 4)
                .and_then(|value| value.try_into().ok())
                .map(u32::from_ne_bytes)
                .ok_or(VfsError::Io)
        };
        Ok(FileAttr {
            xflags: word(0)? as u64,
            extsize: word(1)?,
            nextents: word(2)?,
            project_id: word(3)?,
            cowextsize: word(4)?,
        })
    }

    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        let mut bytes = [0u8; 20];
        for (index, value) in [
            attr.xflags as u32,
            attr.extsize,
            attr.nextents,
            attr.project_id,
            attr.cowextsize,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        self.attribute_ioctl(crate::file::inode_flags::FS_IOC_FSSETXATTR, &bytes, 0)
            .map(|_| ())
    }

    fn get_legacy_flags(&self) -> VfsResult<u32> {
        let bytes = self.attribute_ioctl(crate::file::inode_flags::FS_IOC_GETFLAGS, &[], 4)?;
        bytes
            .as_slice()
            .try_into()
            .map(u32::from_ne_bytes)
            .map_err(|_| VfsError::Io)
    }

    fn set_legacy_flags(&self, flags: u32) -> VfsResult<()> {
        self.attribute_ioctl(
            crate::file::inode_flags::FS_IOC_SETFLAGS,
            &flags.to_ne_bytes(),
            0,
        )
        .map(|_| ())
    }
}

pub(crate) struct FuseOpenFile {
    fs: Arc<FuseFilesystem>,
    nodeid: u64,
    node: Arc<FuseNode>,
    metadata: Mutex<Metadata>,
    fh: u64,
    read: bool,
    write: bool,
    cache: bool,
    lease: Arc<FuseHandleLease>,
    poll: Arc<FusePollState>,
    poll_key: u64,
    teardown: Arc<Mutex<Option<Request>>>,
    released: AtomicBool,
}
impl FuseOpenFile {
    fn retire_claimed(&self) {
        self.lease.live.store(false, Ordering::Release);
        self.fs.retire_handle(&self.lease);
        self.fs.polls.lock().remove(&self.poll_key);
        self.fs.connection.unregister_poll_state(self.poll_key);
    }

    fn release_remote_best_effort(&self) -> VfsResult<()> {
        if self.fs.connection.is_dead() {
            return Ok(());
        }
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&self.fh.to_ne_bytes());
        FuseConnection::reply_data(
            self.fs
                .connection
                .request(FUSE_RELEASE, self.nodeid, &body)
                .map_err(VfsError::from)?,
        )
        .map(|_| ())
        .map_err(VfsError::from)
    }
    fn finish_teardown(&self, release_failed: bool) {
        if let Some(teardown) = self.teardown.lock().take() {
            if release_failed {
                self.fs.connection.activate_materialized_destroy(teardown);
            }
        }
    }
    fn read_cached(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.fs.apply_notifications();
        if !self.cache {
            let _serial = self.fs.notification_apply.lock();
            self.fs
                .flush_cached_handle_serialized(self.nodeid, self.fh)?;
            if self.fs.has_dirty_owned_by_other(self.nodeid, self.fh) {
                return Err(VfsError::ResourceBusy);
            }
            self.fs.invalidate_cached_inode(self.nodeid, 0, -1);
            drop(_serial);
            return fuse_read(&self.fs, self.nodeid, self.fh, buf, offset);
        }
        // Plan/fill away from the caller's user buffer.  A partial cache hit
        // can contain an unflushed dirty prefix followed by a remote hole;
        // fetching that hole directly into `buf` would overwrite the prefix
        // before we can overlay it again.
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(buf.len())
            .map_err(|_| VfsError::NoMemory)?;
        staged.resize(buf.len(), 0);
        if let Some(result) = self.fs.cached_read(self.nodeid, offset, &mut staged) {
            let read = result?;
            buf[..read].copy_from_slice(&staged[..read]);
            return Ok(read);
        }
        let _serial = self.fs.notification_apply.lock();
        let read = fuse_read(&self.fs, self.nodeid, self.fh, &mut staged, offset)?;
        self.fs
            .overlay_dirty_cached(self.nodeid, offset, &mut staged[..read]);
        self.fs
            .cache_store(self.nodeid, offset, &staged[..read], None)?;
        buf[..read].copy_from_slice(&staged[..read]);
        Ok(read)
    }

    fn write_cached(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.fs.apply_notifications();
        if self.fs.connection.is_dead() {
            return Err(VfsError::from(LinuxError::ENODEV));
        }
        if self.cache
            && self
                .fs
                .connection
                .init()
                .is_some_and(|init| init.flags & FUSE_WRITEBACK_CACHE != 0)
        {
            // Publish a dirty range under the same gate used by notification
            // invalidation and writeback selection.
            let _serial = self.fs.notification_apply.lock();
            self.fs
                .cache_store(self.nodeid, offset, buf, Some(self.lease.clone()))?;
            let mut metadata = self.metadata.lock();
            metadata.size = metadata.size.max(offset.saturating_add(buf.len() as u64));
            return Ok(buf.len());
        }
        if !self.cache {
            let _serial = self.fs.notification_apply.lock();
            self.fs
                .flush_cached_handle_serialized(self.nodeid, self.fh)?;
            if self.fs.has_dirty_owned_by_other(self.nodeid, self.fh) {
                return Err(VfsError::ResourceBusy);
            }
            let written = fuse_write(&self.fs, self.nodeid, self.fh, buf, offset)?;
            self.fs
                .invalidate_cached_inode(self.nodeid, offset as i64, written as i64);
            return Ok(written);
        }
        let _serial = self.fs.notification_apply.lock();
        let written = fuse_write(&self.fs, self.nodeid, self.fh, buf, offset)?;
        self.fs
            .cache_store(self.nodeid, offset, &buf[..written], None)?;
        Ok(written)
    }

    fn query_poll(&self, schedule_notify: bool) -> VfsResult<IoEvents> {
        self.fs.apply_notifications();
        if !self.poll.needs_query.load(Ordering::Acquire) {
            return Ok(*self.poll.events.lock());
        }
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&self.fh.to_ne_bytes());
        body[8..16].copy_from_slice(&self.poll_key.to_ne_bytes());
        body[16..20].copy_from_slice(&(schedule_notify as u32).to_ne_bytes());
        body[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
        let reply = FuseConnection::reply_data(
            self.fs
                .connection
                .request(FUSE_POLL, self.nodeid, &body)
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        let revents = reply
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_ne_bytes)
            .ok_or(VfsError::Io)?;
        // FUSE_POLL carries Linux poll bits, whereas IoEvents deliberately
        // has its own representation.  Keep the conversion at this ABI edge.
        let mut events = IoEvents::empty();
        use linux_raw_sys::general::{
            POLLERR, POLLHUP, POLLIN, POLLMSG, POLLNVAL, POLLOUT, POLLPRI, POLLRDBAND, POLLRDHUP,
            POLLRDNORM, POLLWRBAND, POLLWRNORM,
        };
        if revents & POLLIN != 0 {
            events |= IoEvents::READABLE;
        }
        if revents & POLLPRI != 0 {
            events |= IoEvents::PRIORITY;
        }
        if revents & POLLOUT != 0 {
            events |= IoEvents::WRITABLE;
        }
        if revents & POLLERR != 0 {
            events |= IoEvents::ERROR;
        }
        if revents & POLLHUP != 0 {
            events |= IoEvents::HANGUP;
        }
        if revents & POLLNVAL != 0 {
            events |= IoEvents::INVALID;
        }
        if revents & POLLRDNORM != 0 {
            events |= IoEvents::READ_NORMAL;
        }
        if revents & POLLRDBAND != 0 {
            events |= IoEvents::READ_BAND;
        }
        if revents & POLLWRNORM != 0 {
            events |= IoEvents::WRITE_NORMAL;
        }
        if revents & POLLWRBAND != 0 {
            events |= IoEvents::WRITE_BAND;
        }
        if revents & POLLMSG != 0 {
            events |= IoEvents::MESSAGE;
        }
        if revents & POLLRDHUP != 0 {
            events |= IoEvents::READ_HANGUP;
        }
        self.poll.publish(events);
        Ok(events)
    }
    /// Executes the native FUSE allocation operation on this exact open
    /// handle.  The syscall layer owns Linux flag/range/permission admission;
    /// this method only serializes the daemon ABI once that transaction is
    /// committed to this OFD.
    pub(crate) fn fallocate(&self, mode: u32, offset: u64, length: u64) -> AxResult<()> {
        let _serial = self.fs.notification_apply.lock();
        self.node
            .flush_all_cached_serialized()
            .map_err(AxError::from)?;
        let mut body = [0u8; 32];
        body[..8].copy_from_slice(&self.fh.to_ne_bytes());
        body[8..16].copy_from_slice(&offset.to_ne_bytes());
        body[16..24].copy_from_slice(&length.to_ne_bytes());
        body[24..28].copy_from_slice(&mode.to_ne_bytes());
        FuseConnection::reply_data(self.fs.connection.request(
            FUSE_FALLOCATE,
            self.nodeid,
            &body,
        )?)?;
        // Several FALLOCATE modes shift or zero bytes outside the supplied
        // extent. All dirty ranges were serialized above, so a whole-inode
        // clean invalidation is the conservative coherent result.
        self.fs.invalidate_cached_inode(self.nodeid, 0, -1);
        Ok(())
    }

    /// FUSE_LSEEK is needed for remote sparse files: scanning bytes cannot
    /// distinguish a data extent containing zeroes from a hole.
    pub(crate) fn lseek(&self, offset: u64, whence: u32) -> AxResult<u64> {
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&self.fh.to_ne_bytes());
        body[8..16].copy_from_slice(&offset.to_ne_bytes());
        body[16..20].copy_from_slice(&whence.to_ne_bytes());
        let reply = FuseConnection::reply_data(self.fs.connection.request(
            FUSE_LSEEK,
            self.nodeid,
            &body,
        )?)?;
        reply
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_ne_bytes)
            .ok_or(AxError::from(LinuxError::EIO))
    }

    /// Delegates an in-filesystem range transfer without reopening either
    /// side.  Both fhs remain pinned by their OFDs through the request.
    pub(crate) fn copy_file_range_to(
        &self,
        destination: &FuseOpenFile,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
        flags: u64,
    ) -> AxResult<u64> {
        if !Arc::ptr_eq(&self.fs, &destination.fs) {
            return Err(LinuxError::EXDEV.into());
        }
        let _serial = self.fs.notification_apply.lock();
        self.node
            .flush_all_cached_serialized()
            .map_err(AxError::from)?;
        if self.nodeid != destination.nodeid {
            destination
                .node
                .flush_all_cached_serialized()
                .map_err(AxError::from)?;
        }
        let mut body = [0u8; 56];
        body[..8].copy_from_slice(&self.fh.to_ne_bytes());
        body[8..16].copy_from_slice(&source_offset.to_ne_bytes());
        body[16..24].copy_from_slice(&destination.nodeid.to_ne_bytes());
        body[24..32].copy_from_slice(&destination.fh.to_ne_bytes());
        body[32..40].copy_from_slice(&destination_offset.to_ne_bytes());
        body[40..48].copy_from_slice(&length.to_ne_bytes());
        body[48..56].copy_from_slice(&flags.to_ne_bytes());
        let reply = FuseConnection::reply_data(self.fs.connection.request(
            FUSE_COPY_FILE_RANGE,
            self.nodeid,
            &body,
        )?)?;
        let copied = reply
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_ne_bytes)
            .ok_or(AxError::from(LinuxError::EIO))?;
        self.fs.invalidate_cached_inode(destination.nodeid, 0, -1);
        Ok(copied)
    }

    /// Maps a file block through the daemon.  This is retained as a typed
    /// provider primitive for block-mapping ioctls, not guessed from cached
    /// pages or a synthetic device geometry.
    pub(crate) fn bmap(&self, block: u64, block_size: u32) -> AxResult<u64> {
        let mut body = [0u8; 16];
        body[..8].copy_from_slice(&block.to_ne_bytes());
        body[8..12].copy_from_slice(&block_size.to_ne_bytes());
        let reply = FuseConnection::reply_data(self.fs.connection.request(
            FUSE_BMAP,
            self.nodeid,
            &body,
        )?)?;
        reply
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_ne_bytes)
            .ok_or(AxError::from(LinuxError::EIO))
    }

    fn update_remote_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let metadata = fuse_setattr(&self.fs, self.nodeid, Some(self.fh), update)?;
        *self.metadata.lock() = metadata;
        Ok(())
    }

    /// Generic FUSE ioctl transport.  The ordinary syscall path is restricted
    /// exactly as Linux's fuse_file_ioctl: `_IOC` supplies the one in/out
    /// vector, so a daemon cannot request arbitrary user-memory retries.
    pub(crate) fn ioctl(
        &self,
        context: &crate::file::IoctlContext,
        cmd: u32,
        arg: usize,
    ) -> Option<AxResult<usize>> {
        let direction = cmd >> 30;
        let size = ((cmd >> 16) & 0x3fff) as usize;
        if size > MAX_REQUEST_BYTES {
            return Some(Err(AxError::InvalidInput));
        }
        // Size-less/private ioctls have no ABI-decodable buffer.  They use
        // the unrestricted retry path so a compliant daemon can request its
        // exact deep-copy iovecs instead of being rejected at admission.
        if size == 0 {
            return Some(self.ioctl_unrestricted(context, cmd, arg));
        }
        let input_len = if direction & 1 != 0 { size } else { 0 };
        let output_len = if direction & 2 != 0 { size } else { 0 };
        let mut initialized = Vec::new();
        if input_len != 0 {
            if initialized.try_reserve_exact(input_len).is_err() {
                return Some(Err(AxError::NoMemory));
            }
            initialized.resize(input_len, 0);
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    initialized
                        .as_mut_ptr()
                        .cast::<core::mem::MaybeUninit<u8>>(),
                    input_len,
                )
            };
            if let Err(error) = context.user_memory().read_bytes(arg, dst) {
                return Some(Err(crate::mm::map_usercopy_error(error)));
            }
        }
        let result = self
            .fs
            .ioctl_raw(
                self.nodeid,
                self.fh,
                cmd,
                arg as u64,
                0,
                &initialized,
                output_len,
            )
            .and_then(|reply| {
                if reply.flags & FUSE_IOCTL_RETRY != 0 || reply.data.len() != output_len {
                    return Err(VfsError::Io);
                }
                if reply.result < 0 {
                    return Err(VfsError::from(
                        LinuxError::try_from(-reply.result).unwrap_or(LinuxError::EIO),
                    ));
                }
                if output_len != 0 {
                    context
                        .user_memory()
                        .write_bytes(arg, &reply.data)
                        .map_err(crate::mm::map_usercopy_error)?;
                }
                Ok(reply.result as usize)
            })
            .map_err(AxError::from);
        Some(result)
    }

    /// Internal unrestricted transport used by FUSE-controlled operations
    /// which genuinely require deep-copy retry.  The daemon supplies bounded
    /// iovecs, every byte is copied through the captured user-memory
    /// capability, and the retry request carries only those copied bytes.
    /// No daemon ever receives direct access to task memory.
    pub(crate) fn ioctl_unrestricted(
        &self,
        context: &crate::file::IoctlContext,
        cmd: u32,
        arg: usize,
    ) -> AxResult<usize> {
        let mut input = Vec::new();
        let mut output = Vec::new();
        let mut output_iovs: Vec<(usize, usize)> = Vec::new();
        let mut round = 0usize;
        loop {
            if round >= 8 {
                return Err(LinuxError::EIO.into());
            }
            let flags = FUSE_IOCTL_UNRESTRICTED | if round != 0 { FUSE_IOCTL_RETRY } else { 0 };
            let reply = self
                .fs
                .ioctl_raw(
                    self.nodeid,
                    self.fh,
                    cmd,
                    arg as u64,
                    flags,
                    &input,
                    output.len(),
                )
                .map_err(AxError::from)?;
            if reply.flags & FUSE_IOCTL_RETRY == 0 {
                if reply.result < 0 {
                    return Err(LinuxError::try_from(-reply.result)
                        .unwrap_or(LinuxError::EIO)
                        .into());
                }
                if reply.data.len() != output.len() {
                    return Err(LinuxError::EIO.into());
                }
                let mut offset = 0usize;
                for (base, len) in &output_iovs {
                    context
                        .user_memory()
                        .write_bytes(*base, &reply.data[offset..offset + *len])
                        .map_err(crate::mm::map_usercopy_error)?;
                    offset += *len;
                }
                return Ok(reply.result as usize);
            }
            let count = reply
                .in_iovs
                .checked_add(reply.out_iovs)
                .ok_or(LinuxError::EIO)? as usize;
            if count > 256 || reply.data.len() != count.checked_mul(16).ok_or(LinuxError::EIO)? {
                return Err(LinuxError::EIO.into());
            }
            input.clear();
            output.clear();
            output_iovs.clear();
            for index in 0..count {
                let base = u64::from_ne_bytes(
                    reply.data[index * 16..index * 16 + 8]
                        .try_into()
                        .expect("bounded"),
                ) as usize;
                let len = u64::from_ne_bytes(
                    reply.data[index * 16 + 8..index * 16 + 16]
                        .try_into()
                        .expect("bounded"),
                ) as usize;
                if len > MAX_REQUEST_BYTES
                    || input.len().checked_add(len).ok_or(LinuxError::EIO)? > MAX_REQUEST_BYTES
                {
                    return Err(LinuxError::EIO.into());
                }
                if index < reply.in_iovs as usize {
                    let start = input.len();
                    input
                        .try_reserve_exact(len)
                        .map_err(|_| AxError::NoMemory)?;
                    input.resize(start + len, 0);
                    let dst = unsafe {
                        core::slice::from_raw_parts_mut(
                            input[start..]
                                .as_mut_ptr()
                                .cast::<core::mem::MaybeUninit<u8>>(),
                            len,
                        )
                    };
                    context
                        .user_memory()
                        .read_bytes(base, dst)
                        .map_err(crate::mm::map_usercopy_error)?;
                } else {
                    output
                        .try_reserve_exact(len)
                        .map_err(|_| AxError::NoMemory)?;
                    output.resize(output.len() + len, 0);
                    output_iovs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    output_iovs.push((base, len));
                }
            }
            round += 1;
        }
    }
}
impl NodeOps for FuseOpenFile {
    fn inode(&self) -> u64 {
        self.nodeid
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(self.metadata.lock().clone())
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.update_remote_metadata(update)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, data_only: bool) -> VfsResult<()> {
        let cache_result = self.fs.flush_cached_handle(self.nodeid, self.fh);
        let mut body = Vec::new();
        body.extend_from_slice(&self.fh.to_ne_bytes());
        body.extend_from_slice(&(data_only as u32).to_ne_bytes());
        body.extend_from_slice(&0u32.to_ne_bytes());
        let sync_result = FuseConnection::reply_data(
            self.fs
                .connection
                .request(FUSE_FSYNC, self.nodeid, &body)
                .map_err(VfsError::from)?,
        )
        .map(|_| ())
        .map_err(VfsError::from);
        cache_result.and(sync_result)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn lock_ops(&self) -> Option<&dyn LockOps> {
        Some(self)
    }
}
impl FileNodeOps for FuseOpenFile {
    fn nowait_read_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        // Queue capacity is not completion readiness: a FUSE request still
        // waits synchronously for a daemon reply.  Until this provider owns a
        // nonblocking completion path, RWF_NOWAIT must refuse it.
        Ok(NowaitAdmission::WouldBlock)
    }
    fn nowait_write_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        Ok(NowaitAdmission::WouldBlock)
    }
    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        let mode = match request.operation {
            FileRangeOperation::Allocate { keep_size: false } => 0,
            FileRangeOperation::Allocate { keep_size: true } => 0x01,
            FileRangeOperation::PunchHole => 0x03,
            FileRangeOperation::ZeroRange { keep_size: false } => 0x10,
            FileRangeOperation::ZeroRange { keep_size: true } => 0x11,
            FileRangeOperation::CollapseRange => 0x08,
            FileRangeOperation::InsertRange => 0x20,
            FileRangeOperation::UnshareRange => 0x40,
        };
        self.fallocate(mode, request.offset, request.length)
            .map_err(VfsError::from)
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if !self.read {
            return Err(VfsError::BadFileDescriptor);
        }
        self.read_cached(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        self.write_cached(buf, offset)
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        let offset = self.metadata()?.size;
        let count = self.write_cached(buf, offset)?;
        Ok((count, offset.saturating_add(count as u64)))
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        let _serial = self.fs.notification_apply.lock();
        self.node.flush_all_cached_serialized()?;
        let metadata = fuse_set_len(&self.fs, self.nodeid, self.fh, len)?;
        self.fs.invalidate_cached_inode(self.nodeid, len as i64, -1);
        *self.metadata.lock() = metadata;
        Ok(())
    }
    fn set_symlink(&self, _: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
    fn release_handle(&self) -> VfsResult<()> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let writeback = self.fs.flush_cached_handle(self.nodeid, self.fh);
        let flush = if writeback.is_ok() && !self.fs.connection.is_dead() {
            let mut body = [0u8; 24];
            body[..8].copy_from_slice(&self.fh.to_ne_bytes());
            match self.fs.connection.request(FUSE_FLUSH, self.nodeid, &body) {
                Ok(reply) => FuseConnection::reply_data(reply)
                    .map(|_| ())
                    .map_err(VfsError::from),
                Err(error) => Err(VfsError::from(error)),
            }
        } else {
            Ok(())
        };
        // Always release the daemon fh even when writeback or FLUSH failed.
        // Cleanup is unconditional and atomic with respect to duplicate Drop.
        let release = self.release_remote_best_effort();
        self.finish_teardown(release.is_err());
        self.retire_claimed();
        writeback.and(flush).and(release)
    }
}

impl Drop for FuseOpenFile {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let release = self.release_remote_best_effort();
        self.finish_teardown(release.is_err());
        self.retire_claimed();
    }
}

impl Pollable for FuseOpenFile {
    fn poll(&self) -> IoEvents {
        self.query_poll(false).unwrap_or(IoEvents::ERROR)
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.is_empty() {
            return PollRegistration::empty();
        }
        let _ = self.query_poll(true);
        PollRegistration::single(&self.poll.waiters, context.waker())
    }
}

impl LockOps for FuseOpenFile {
    fn get_lock(&self, owner: u64, lock: FileLock) -> VfsResult<FileLock> {
        let mut body = Vec::new();
        body.extend_from_slice(&self.fh.to_ne_bytes());
        body.extend_from_slice(&owner.to_ne_bytes());
        body.extend_from_slice(&lock.start.to_ne_bytes());
        body.extend_from_slice(&lock.end.to_ne_bytes());
        body.extend_from_slice(&lock.kind.to_ne_bytes());
        body.extend_from_slice(&lock.pid.to_ne_bytes());
        body.extend_from_slice(&0u64.to_ne_bytes());
        let reply = FuseConnection::reply_data(
            self.fs
                .connection
                .request(FUSE_GETLK, self.nodeid, &body)
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        if reply.len() != 24 {
            return Err(VfsError::Io);
        }
        Ok(FileLock {
            start: u64::from_ne_bytes(reply[0..8].try_into().expect("bounded")),
            end: u64::from_ne_bytes(reply[8..16].try_into().expect("bounded")),
            kind: u32::from_ne_bytes(reply[16..20].try_into().expect("bounded")),
            pid: u32::from_ne_bytes(reply[20..24].try_into().expect("bounded")),
        })
    }
    fn set_lock(&self, owner: u64, lock: FileLock, wait: bool) -> VfsResult<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&self.fh.to_ne_bytes());
        body.extend_from_slice(&owner.to_ne_bytes());
        body.extend_from_slice(&lock.start.to_ne_bytes());
        body.extend_from_slice(&lock.end.to_ne_bytes());
        body.extend_from_slice(&lock.kind.to_ne_bytes());
        body.extend_from_slice(&lock.pid.to_ne_bytes());
        body.extend_from_slice(&0u64.to_ne_bytes());
        FuseConnection::reply_data(
            self.fs
                .connection
                .request(
                    if wait { FUSE_SETLKW } else { FUSE_SETLK },
                    self.nodeid,
                    &body,
                )
                .map_err(VfsError::from)?,
        )
        .map(|_| ())
        .map_err(VfsError::from)
    }
}

struct FuseOpenDir {
    node: Arc<FuseNode>,
    fh: u64,
    teardown: Arc<Mutex<Option<Request>>>,
    operation: Mutex<()>,
    released: AtomicBool,
}

impl FuseOpenDir {
    fn ensure_live(&self) -> VfsResult<()> {
        if self.released.load(Ordering::Acquire) {
            Err(VfsError::BadFileDescriptor)
        } else {
            Ok(())
        }
    }

    fn release_locked(&self) -> VfsResult<()> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let release = self.node.release_dir_fh_result(self.fh);
        if let Some(teardown) = self.teardown.lock().take() {
            if release.is_err() {
                self.node
                    .fs
                    .connection
                    .activate_materialized_destroy(teardown);
            }
        }
        release
    }
}

impl NodeOps for FuseOpenDir {
    fn inode(&self) -> u64 {
        self.node.nodeid
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        self.node.metadata()
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.node.update_metadata(update)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.filesystem()
    }
    fn sync(&self, data_only: bool) -> VfsResult<()> {
        let _operation = self.operation.lock();
        self.ensure_live()?;
        // fuse_fsync_in is exactly { u64 fh, u32 fsync_flags, u32 padding }.
        // FSYNCDIR arrived with protocol 7.2; never send an opcode a mounted
        // legacy daemon cannot decode.
        if !self
            .node
            .fs
            .connection
            .init()
            .is_some_and(|init| init.minor >= 2)
        {
            return Err(VfsError::OperationNotSupported);
        }
        let mut input = [0u8; 16];
        input[..8].copy_from_slice(&self.fh.to_ne_bytes());
        input[8..12]
            .copy_from_slice(&(if data_only { FUSE_FSYNC_FDATASYNC } else { 0 }).to_ne_bytes());
        // The remaining four bytes are protocol-required zero padding.
        let reply = FuseConnection::reply_data(
            self.node
                .fs
                .connection
                .request(FUSE_FSYNCDIR, self.node.nodeid, &input)
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        // FSYNCDIR is a simple request.  Payload bytes have no defined
        // meaning, so accepting them would silently desynchronize malformed
        // daemon replies from the fixed protocol contract.
        if reply.is_empty() {
            Ok(())
        } else {
            Err(VfsError::Io)
        }
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}
impl DirNodeOps for FuseOpenDir {
    fn open_handle(&self, flags: u32) -> VfsResult<Option<Arc<dyn DirNodeOps>>> {
        DirNodeOps::open_handle(self.node.as_ref(), flags)
    }
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        self.node.lookup(name)
    }
    fn namespace_epoch(&self) -> u64 {
        self.node.namespace_epoch()
    }
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.node.supports_named_create(node_type)
    }
    fn supports_symlink(&self) -> bool {
        self.node.supports_symlink()
    }
    fn supports_hard_links(&self) -> bool {
        self.node.supports_hard_links()
    }
    fn supports_unlink(&self) -> bool {
        self.node.supports_unlink()
    }
    fn supports_rmdir(&self) -> bool {
        self.node.supports_rmdir()
    }
    fn supports_rename(&self) -> bool {
        self.node.supports_rename()
    }
    fn supports_rename_exchange(&self) -> bool {
        self.node.supports_rename_exchange()
    }
    fn supports_rename_whiteout(&self) -> bool {
        self.node.supports_rename_whiteout()
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        self.node.create_named(name, options, disposition)
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        self.node.create_symlink(name, target, permission, user)
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        self.node.create_symlink_prepared(name, target, options)
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        self.node.link(name, node)
    }
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        self.node.unlink(request)
    }
    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        self.node.rename(request)
    }
    fn rename_whiteout(&self, request: axfs_ng_vfs::RenameWhiteoutRequest<'_>) -> VfsResult<()> {
        self.node.rename_whiteout(request)
    }
    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        self.node.rename_exchange(request)
    }
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let _operation = self.operation.lock();
        self.ensure_live()?;
        let mut body = Vec::new();
        body.extend_from_slice(&self.fh.to_ne_bytes());
        body.extend_from_slice(&offset.to_ne_bytes());
        body.extend_from_slice(&(64 * 1024u32).to_ne_bytes());
        body.extend_from_slice(&[0; 20]);
        let plus = self
            .node
            .fs
            .connection
            .init()
            .is_some_and(|init| init.minor >= 21 && init.flags & FUSE_DO_READDIRPLUS != 0);
        let reply = FuseConnection::reply_data(
            self.node
                .fs
                .connection
                .request(
                    if plus { FUSE_READDIRPLUS } else { FUSE_READDIR },
                    self.node.nodeid,
                    &body,
                )
                .map_err(VfsError::from)?,
        )
        .map_err(VfsError::from)?;
        let mut pos = 0;
        let mut count = 0;
        while pos < reply.len() {
            // `fuse_direntplus` starts with a 128-byte entry_out followed by
            // the ordinary 24-byte dirent header.  We deliberately validate
            // the entry as well as the dirent: a malformed PLUS reply must
            // not leave a lookup reference that cannot later be FORGETed.
            let entry = if plus { 128 } else { 0 };
            if reply.len().saturating_sub(pos) < entry + 24 {
                return Err(VfsError::Io);
            }
            let dirent = pos + entry;
            let ino = u64::from_ne_bytes(reply[dirent..dirent + 8].try_into().expect("bounded"));
            let next =
                u64::from_ne_bytes(reply[dirent + 8..dirent + 16].try_into().expect("bounded"));
            let len =
                u32::from_ne_bytes(reply[dirent + 16..dirent + 20].try_into().expect("bounded"))
                    as usize;
            let ty = reply[dirent + 20];
            let end = dirent.checked_add(24 + len).ok_or(VfsError::Io)?;
            if end > reply.len() {
                return Err(VfsError::Io);
            }
            let name = FsName::new(&reply[dirent + 24..end]);
            if name.is_empty() || name.as_bytes().contains(&0) || name.as_bytes().contains(&b'/') {
                return Err(VfsError::Io);
            }
            if plus {
                let nodeid = u64::from_ne_bytes(reply[pos..pos + 8].try_into().expect("bounded"));
                if nodeid == 0 {
                    return Err(VfsError::Io);
                }
                let metadata = parse_attr(&reply[pos..], 40)?;
                // Creating and dropping this entry is intentional: FUSE has
                // handed the kernel a lookup reference, whose Drop emits the
                // matching asynchronous FORGET while the VFS still retains
                // only its caller-owned dirent projection.
                let _lookup_ref =
                    self.node
                        .fs
                        .node_entry(nodeid, metadata, self.node.parent_entry()?, name)?;
            }
            if !sink.accept(name, ino, NodeType::from(ty), next) {
                break;
            }
            count += 1;
            pos = (end + 7) & !7;
        }
        Ok(count)
    }
    fn release_handle(&self) -> VfsResult<()> {
        let _operation = self.operation.lock();
        self.release_locked()
    }
}

impl Drop for FuseOpenDir {
    fn drop(&mut self) {
        let _operation = self.operation.lock();
        let _ = self.release_locked();
    }
}

fn parse_attr_out(bytes: &[u8]) -> VfsResult<Metadata> {
    parse_attr(bytes, 16)
}
fn fuse_read(
    fs: &FuseFilesystem,
    nodeid: u64,
    fh: u64,
    buf: &mut [u8],
    offset: u64,
) -> VfsResult<usize> {
    let mut body = Vec::new();
    body.try_reserve(40).map_err(|_| VfsError::NoMemory)?;
    body.extend_from_slice(&fh.to_ne_bytes());
    body.extend_from_slice(&offset.to_ne_bytes());
    body.extend_from_slice(&(buf.len() as u32).to_ne_bytes());
    body.extend_from_slice(&[0; 20]);
    let reply = FuseConnection::reply_data(
        fs.connection
            .request(FUSE_READ, nodeid, &body)
            .map_err(VfsError::from)?,
    )
    .map_err(VfsError::from)?;
    if reply.len() > buf.len() {
        return Err(VfsError::Io);
    }
    buf[..reply.len()].copy_from_slice(&reply);
    Ok(reply.len())
}
fn fuse_write(
    fs: &FuseFilesystem,
    nodeid: u64,
    fh: u64,
    buf: &[u8],
    offset: u64,
) -> VfsResult<usize> {
    let mut body = Vec::new();
    body.try_reserve(40 + buf.len())
        .map_err(|_| VfsError::NoMemory)?;
    body.extend_from_slice(&fh.to_ne_bytes());
    body.extend_from_slice(&offset.to_ne_bytes());
    body.extend_from_slice(&(buf.len() as u32).to_ne_bytes());
    body.extend_from_slice(&[0; 20]);
    body.extend_from_slice(buf);
    let reply = FuseConnection::reply_data(
        fs.connection
            .request(FUSE_WRITE, nodeid, &body)
            .map_err(VfsError::from)?,
    )
    .map_err(VfsError::from)?;
    let count = reply
        .get(..4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or(VfsError::Io)? as usize;
    if count > buf.len() {
        return Err(VfsError::Io);
    }
    Ok(count)
}
fn fuse_set_len(fs: &FuseFilesystem, nodeid: u64, fh: u64, len: u64) -> VfsResult<Metadata> {
    // FATTR_SIZE, with the exact open handle, prevents a concurrent FUSE
    // daemon lease from being retargeted through a second temporary open.
    let mut body = [0u8; 88];
    body[..4].copy_from_slice(&(1u32 << 3).to_ne_bytes());
    body[8..16].copy_from_slice(&fh.to_ne_bytes());
    body[16..24].copy_from_slice(&len.to_ne_bytes());
    let reply = FuseConnection::reply_data(
        fs.connection
            .request(FUSE_SETATTR, nodeid, &body)
            .map_err(VfsError::from)?,
    )
    .map_err(VfsError::from)?;
    parse_attr_out(&reply)
}
fn fuse_ttl_deadline(bytes: &[u8], seconds_offset: usize, nanos_offset: usize) -> VfsResult<u64> {
    let seconds = bytes
        .get(seconds_offset..seconds_offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_ne_bytes)
        .ok_or(VfsError::Io)?;
    let nanos = bytes
        .get(nanos_offset..nanos_offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or(VfsError::Io)?;
    if nanos >= 1_000_000_000 {
        return Err(VfsError::Io);
    }
    Ok(axhal::time::monotonic_time_nanos()
        .saturating_add(seconds.saturating_mul(1_000_000_000))
        .saturating_add(nanos as u64))
}
fn parse_entry_out(bytes: &[u8]) -> VfsResult<(u64, Metadata, u64, u64)> {
    let nodeid = bytes
        .get(..8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_ne_bytes)
        .ok_or(VfsError::Io)?;
    Ok((
        nodeid,
        parse_attr(bytes, 40)?,
        fuse_ttl_deadline(bytes, 16, 32)?,
        fuse_ttl_deadline(bytes, 24, 36)?,
    ))
}
fn parse_attr(bytes: &[u8], offset: usize) -> VfsResult<Metadata> {
    let get64 = |n| {
        bytes
            .get(offset + n..offset + n + 8)
            .and_then(|v| v.try_into().ok())
            .map(u64::from_ne_bytes)
            .ok_or(VfsError::Io)
    };
    let get32 = |n| {
        bytes
            .get(offset + n..offset + n + 4)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_ne_bytes)
            .ok_or(VfsError::Io)
    };
    let mode = get32(60)?;
    let node_type = NodeType::from(((mode >> 12) & 0xf) as u8);
    let time = |sec, nsec| Timestamp::try_new(sec as i64, nsec).ok_or(VfsError::Io);
    Ok(Metadata {
        device: 0,
        inode: get64(0)?,
        nlink: get32(64)? as u64,
        mode: NodePermission::from_bits_truncate((mode & 0o7777) as u16),
        node_type,
        uid: get32(68)?,
        gid: get32(72)?,
        project_id: 0,
        size: get64(8)?,
        block_size: get32(80)? as u64,
        blocks: get64(16)?,
        rdev: DeviceId::default(),
        atime: time(get64(24)?, get32(48)?)?,
        btime: Timestamp::ZERO,
        mtime: time(get64(32)?, get32(52)?)?,
        ctime: time(get64(40)?, get32(56)?)?,
    })
}
