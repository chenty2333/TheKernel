mod dir;
mod file;
mod xattr;

use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    any::{Any, TypeId},
    fmt,
    hash::Hash,
    mem,
    ops::Deref,
    ptr,
    sync::atomic::{
        AtomicBool, AtomicI64, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
        fence,
    },
    task::Context,
};

use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use bitflags::bitflags;
pub use dir::*;
pub use file::*;
use inherit_methods_macro::inherit_methods;
use smallvec::SmallVec;
pub use xattr::*;

use crate::{
    FilesystemOps, Metadata, MetadataCapabilities, MetadataUpdate, Mutex, MutexGuard, NodeType, VfsError, VfsResult,
    path::{FsName, FsNameBuf, FsPathBuf, try_build_absolute_path},
};

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct NodeFlags: u32 {
        /// Indicates that this file behaves like a stream.
        ///
        /// Presence of this flag could inform the higher layers to omit
        /// maintaining a position for this file. `read_at` and `write_at` would
        /// be called with zero offset instead.
        const STREAM = 0x0001;

        /// Indicates that this file should not be cached.
        ///
        /// For instance, files in `/proc` or `/sys` may contain dynamic data
        /// that should not be cached.
        const NON_CACHEABLE = 0x0002;

        /// Indicates that this file should always be cached.
        ///
        /// For instance, files in tmpfs relies on page caching and do not have
        /// a backing device.
        const ALWAYS_CACHE = 0x0004;

        /// Indicates that operations on this file are always blocking.
        ///
        /// This could prevent higher layers from attempting to add unnecessary
        /// non-blocking handling.
        const BLOCKING = 0x0008;

        /// Indicates a filesystem-declared magic/jump link whose resolution
        /// transfers to another object identity rather than following an
        /// ordinary stored pathname.
        ///
        /// This is independent of whether a textual target happens to be
        /// generated dynamically. Pathwalk policy can distinguish the semantic
        /// capability without depending on filesystem names or path strings.
        const MAGIC_LINK = 0x0010;

        /// O_APPEND writes use the open file description's current position
        /// instead of asking the inode to discover a persistent end offset.
        /// This is required by proc controls whose only writable position is
        /// zero and whose successful write advances that OFD position.
        const POSITIONED_APPEND = 0x0020;

        /// I/O on this node requires the immutable credential captured by the
        /// open file description. Higher layers may skip credential-context
        /// installation for every other node.
        const OPEN_CREDENTIAL = 0x0040;

        /// Explicit-offset writes are not supported even though ordinary
        /// writes advance an open file description position. Linux proc
        /// controls with a legacy `.write` operation have this shape: lseek
        /// and pread may work, while pwrite must fail with `ESPIPE`.
        const NO_POSITIONED_WRITE = 0x0080;

        /// Explicit-offset reads are not supported. This is independent of
        /// [`STREAM`](Self::STREAM): stream nodes such as `/dev/zero` may omit
        /// an OFD cursor while still accepting `pread`.
        const NO_POSITIONED_READ = 0x0100;

        /// Seeking is not supported. This is independent of whether the node
        /// maintains an OFD cursor; Linux devices such as `/dev/null` accept
        /// lseek while TTYs reject it.
        const NO_SEEK = 0x0200;
    }
}

/// Stable backend object identity.  It is intentionally unrelated to a
/// pathname/dentry reference: rename and hard-link operations preserve it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectKey {
    pub filesystem: u64,
    pub object: u64,
    pub generation: u64,
}

impl ObjectKey {
    pub const fn new(filesystem: u64, object: u64, generation: u64) -> Self {
        Self {
            filesystem,
            object,
            generation,
        }
    }
}

/// Linux inode file attributes shared by `file_getattr`, `file_setattr`, and
/// filesystem-specific ioctls.  Policy enforcement is owned by VFS callers;
/// providers only persist and report their native inode state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileAttr {
    /// Linux `fsxattr.fsx_xflags` value, not a filesystem-private inode bitmap.
    pub xflags: u64,
    pub extsize: u32,
    /// Output-only allocated-extent count.
    pub nextents: u32,
    pub project_id: u32,
    pub cowextsize: u32,
}

pub trait FileAttrProvider: Send + Sync {
    fn get_file_attr(&self) -> VfsResult<FileAttr>;
    /// Reads native immutable/append attributes without waiting. Providers
    /// must return [`VfsError::WouldBlock`] instead of falling back to their
    /// ordinary potentially blocking query when their serialization is busy.
    fn try_get_file_attr(&self) -> VfsResult<FileAttr> {
        Err(VfsError::WouldBlock)
    }
    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()>;

    /// Native `FS_IOC_GETFLAGS` representation.  This deliberately remains
    /// separate from `FS_XFLAG_*`: ext4 exposes additional user-visible inode
    /// flags which must not disappear through an xflag round-trip.
    fn get_legacy_flags(&self) -> VfsResult<u32> {
        Err(VfsError::OperationNotSupported)
    }

    /// Native `FS_IOC_SETFLAGS` publication.  Callers have already enforced
    /// the common VFS ownership/capability/LSM transaction.
    fn set_legacy_flags(&self, _flags: u32) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

/// A POSIX byte-range lock carried by a provider rather than reconstructed
/// from a pathname.  Remote filesystems use this to preserve daemon lock
/// ownership and the exact FUSE GETLK/SETLK/SETLKW operation selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileLock {
    pub start: u64,
    pub end: u64,
    pub kind: u32,
    pub pid: u32,
}

pub trait LockOps: Send + Sync {
    fn get_lock(&self, _owner: u64, _lock: FileLock) -> VfsResult<FileLock> {
        Err(VfsError::OperationNotSupported)
    }
    fn set_lock(&self, _owner: u64, _lock: FileLock, _wait: bool) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

/// Backend-reported quota state for the object identity supplied to the
/// provider.  Values are byte counts as defined by NFS FATTR4_QUOTA_*;
/// `None` means the server did not advertise that particular limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QuotaUsage {
    pub hard_available: Option<u64>,
    pub soft_available: Option<u64>,
    pub used: u64,
}

pub trait QuotaOps: Send + Sync {
    fn quota_usage(&self) -> VfsResult<QuotaUsage> {
        Err(VfsError::OperationNotSupported)
    }
}

/// Filesystem node operationss
#[allow(clippy::len_without_is_empty)]
pub trait NodeOps: Send + Sync + 'static {
    /// Gets the inode number of the node.
    fn inode(&self) -> u64;

    /// Returns a stable backend identity. Backends with reusable inode numbers
    /// must override this with their generation-aware native identity.
    fn object_key(&self) -> ObjectKey {
        ObjectKey::new(0, self.inode(), 0)
    }

    /// Gets the metadata of the node.
    fn metadata(&self) -> VfsResult<Metadata>;

    /// Declares optional fields backed by this provider for the metadata
    /// snapshot. The default deliberately makes no optional statx claims.
    fn metadata_capabilities(&self, _metadata: &Metadata) -> MetadataCapabilities {
        MetadataCapabilities::default()
    }

    /// Updates the metadata of the node.
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    /// Gets the filesystem
    fn filesystem(&self) -> &dyn FilesystemOps;

    /// Gets the size of the node.
    fn len(&self) -> VfsResult<u64> {
        self.metadata().map(|m| m.size)
    }

    /// Synchronizes the file to disk.
    fn sync(&self, data_only: bool) -> VfsResult<()>;

    /// Casts the node to a `&dyn core::any::Any`.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// Returns the flags of the node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }

    /// Admits one open before a high-level file backend is constructed.
    ///
    /// Dynamic filesystems can use this to enforce open-time policy which
    /// cannot be reconstructed from a later read or write. `O_PATH`-style
    /// handles call this with both access bits clear.
    fn open(&self, _read: bool, _write: bool) -> VfsResult<()> {
        Ok(())
    }

    /// Returns backend state whose lifetime follows one stable inode
    /// generation rather than one replaceable directory-entry cache object.
    ///
    /// Backends that support prepared runtime attachments (for example Unix
    /// socket endpoints) must return the same cell from every lookup and
    /// hardlink alias of that inode generation.
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        None
    }

    /// Returns the writeback error sequence owned by this stable backend node.
    /// The default uses persistent inode data when the backend exposes it.
    fn writeback_error_state(&self) -> VfsResult<Arc<WritebackErrorState>> {
        self.persistent_user_data()
            .ok_or(VfsError::OperationNotSupported)?
            .writeback_error_state()
    }

    /// Returns the stable per-inode extended-attribute provider, when this
    /// backend has one. A missing provider means honest `EOPNOTSUPP` rather
    /// than an ephemeral VFS-side store.
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        None
    }

    /// Returns native inode file-attribute storage when supported.
    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        None
    }

    /// Owned file-attribute operation facade.  Unlike exposing a borrowed
    /// provider, this lets a composed filesystem perform copy-up, acquire a
    /// transaction, or replace its active upper inode before the caller sees
    /// the operation.  Native providers retain the small borrowed adapter.
    fn get_file_attr(&self) -> VfsResult<FileAttr> {
        self.file_attr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .get_file_attr()
    }

    /// Try-only native file-attribute query used by NOWAIT admission. A
    /// missing provider remains the historical "no native restrictions"
    /// case; a present provider may never be queried through its blocking
    /// implementation on this route.
    fn try_get_file_attr(&self) -> VfsResult<FileAttr> {
        self.file_attr_provider()
            .map_or(Err(VfsError::OperationNotSupported), |provider| {
                provider.try_get_file_attr()
            })
    }

    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        self.file_attr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_file_attr(attr)
    }

    fn get_legacy_file_flags(&self) -> VfsResult<u32> {
        self.file_attr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .get_legacy_flags()
    }

    fn set_legacy_file_flags(&self, flags: u32) -> VfsResult<()> {
        self.file_attr_provider()
            .ok_or(VfsError::OperationNotSupported)?
            .set_legacy_flags(flags)
    }

    fn lock_ops(&self) -> Option<&dyn LockOps> {
        None
    }

    fn quota_ops(&self) -> Option<&dyn QuotaOps> {
        None
    }
}

enum Node {
    File(FileNode),
    Dir(DirNode),
}

impl Node {
    pub fn clone_inner(&self) -> Arc<dyn NodeOps> {
        match self {
            Node::File(file) => file.inner().clone(),
            Node::Dir(dir) => dir.inner().clone(),
        }
    }
}

impl Deref for Node {
    type Target = dyn NodeOps;

    fn deref(&self) -> &Self::Target {
        match &self {
            Node::File(file) => file.deref(),
            Node::Dir(dir) => dir.deref(),
        }
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::File(file) => write!(f, "FileNode({})", file.inode()),
            Node::Dir(dir) => write!(f, "DirNode({})", dir.inode()),
        }
    }
}

static ANONYMOUS_REFERENCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct Reference {
    parent: Option<DirEntry>,
    name: FsNameBuf,
    anonymous_id: Option<u64>,
    path_hash: u64,
}

impl Reference {
    fn extend_path_hash(mut hash: u64, name: &FsName) -> u64 {
        const FNV_PRIME: u64 = 0x100000001b3;
        for byte in name
            .as_bytes()
            .len()
            .to_le_bytes()
            .iter()
            .chain(name.as_bytes())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    pub fn new(parent: Option<DirEntry>, name: FsNameBuf) -> Self {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        let parent_hash = parent
            .as_ref()
            .map_or(FNV_OFFSET, |parent| parent.0.reference.path_hash);
        let path_hash = Self::extend_path_hash(parent_hash, &name);
        Self {
            parent,
            name,
            anonymous_id: None,
            path_hash,
        }
    }

    /// Fallibly constructs a path reference for a userspace-triggered lookup.
    pub fn try_new(parent: Option<DirEntry>, name: &FsName) -> VfsResult<Self> {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(name.as_bytes().len())
            .map_err(|_| VfsError::NoMemory)?;
        bytes.extend_from_slice(name.as_bytes());
        let owned_name = FsNameBuf::from_vec(bytes)?;
        let parent_hash = parent
            .as_ref()
            .map_or(FNV_OFFSET, |parent| parent.0.reference.path_hash);
        let path_hash = Self::extend_path_hash(parent_hash, &owned_name);
        Ok(Self {
            parent,
            name: owned_name,
            anonymous_id: None,
            path_hash,
        })
    }

    pub fn root() -> Self {
        Self::new(None, FsNameBuf::new())
    }

    pub fn anonymous() -> Self {
        let anonymous_id = ANONYMOUS_REFERENCE_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            parent: None,
            name: FsNameBuf::new(),
            anonymous_id: Some(anonymous_id),
            path_hash: anonymous_id,
        }
    }

    fn is_root(&self) -> bool {
        self.parent.is_none() && self.anonymous_id.is_none()
    }
}

#[derive(Default)]
pub struct TypeMap(SmallVec<[(TypeId, Arc<dyn Any + Send + Sync>); 2]>);
impl TypeMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        let id = TypeId::of::<T>();
        let value: Arc<dyn Any + Send + Sync> = Arc::new(value);
        if let Some((_, slot)) = self.0.iter_mut().find(|(existing, _)| *existing == id) {
            *slot = value;
        } else {
            self.0.push((id, value));
        }
    }

    /// Fallibly prepares both the erased value ownership and an additional
    /// inline-map slot before publishing either. Replacing an existing type
    /// needs no container growth, but the new `Arc` is still admitted first.
    pub fn try_insert<T: Any + Send + Sync>(&mut self, value: T) -> VfsResult<Option<Arc<T>>> {
        let value = Arc::try_new(value).map_err(|_| VfsError::NoMemory)?;
        self.try_insert_shared(value)
    }

    /// Fallibly publishes an already admitted shared value.
    ///
    /// Callers that must prepare all owned state before a namespace mutation
    /// can allocate the [`Arc`] first, then attach that exact allocation to a
    /// newly created entry without wrapping it in another allocation.
    pub(crate) fn try_insert_shared<T: Any + Send + Sync>(
        &mut self,
        value: Arc<T>,
    ) -> VfsResult<Option<Arc<T>>> {
        let id = TypeId::of::<T>();
        let value: Arc<dyn Any + Send + Sync> = value;
        let retired =
            if let Some((_, slot)) = self.0.iter_mut().find(|(existing, _)| *existing == id) {
                Some(mem::replace(slot, value))
            } else {
                self.0.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                self.0.push((id, value));
                None
            };
        retired
            .map(|value| value.downcast::<T>().map_err(|_| VfsError::Io))
            .transpose()
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0
            .iter()
            .find_map(|(id, value)| {
                if id == &TypeId::of::<T>() {
                    Some(value.clone())
                } else {
                    None
                }
            })
            .and_then(|value| value.downcast().ok())
    }

    pub fn get_ref<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.0.iter().find_map(|(id, value)| {
            (id == &TypeId::of::<T>())
                .then(|| value.as_ref().downcast_ref::<T>())
                .flatten()
        })
    }

    pub fn get_or_insert_with<T: Any + Send + Sync>(&mut self, f: impl FnOnce() -> T) -> Arc<T> {
        if let Some(value) = self.get::<T>() {
            value
        } else {
            let value = f();
            self.insert(value);
            self.get::<T>().unwrap()
        }
    }

    pub fn try_get_or_insert_with<T: Any + Send + Sync>(
        &mut self,
        f: impl FnOnce() -> T,
    ) -> VfsResult<Arc<T>> {
        if let Some(value) = self.get::<T>() {
            return Ok(value);
        }
        let retired = self.try_insert(f())?;
        debug_assert!(retired.is_none());
        drop(retired);
        self.get::<T>().ok_or(VfsError::Io)
    }
}

/// Type-erased runtime attachments owned by one stable backend inode.
pub struct NodeUserData {
    data: Mutex<TypeMap>,
    // Kept outside the bounded generic attachment map: writeback error
    // reporting is core inode state and must remain available even when
    // optional runtime attachments have filled that map.
    writeback_errors: Mutex<Option<Arc<WritebackErrorState>>>,
    file_attr_mutation: Arc<FileAttrMutationState>,
}

const FILE_ATTR_UPDATE: usize = 1usize << (usize::BITS - 1);
struct FileAttrMutationState(AtomicUsize);

/// Stable-inode active mutation admission.  The guard is deliberately only
/// an atomic ownership token: no borrowed TypeMap/cache/provider lock is held
/// while callers acquire their own native serialization.
pub struct FileAttrMutationGuard(Arc<FileAttrMutationState>);
pub struct FileAttrUpdateGuard(Arc<FileAttrMutationState>);

/// A non-blocking, inode-generation-stable projection of deferred inode
/// timestamps.
///
/// Cached `RWF_NOWAIT` I/O cannot call a provider merely to persist atime or
/// mtime.  The cache installs this object in a backend supplied
/// [`NodeUserData`] cell before accepting such I/O.  Readers use a bounded
/// seqlock snapshot: an in-progress writer is treated as no projection rather
/// than making `stat` wait.  A reservation is separate from publication, so a
/// failed NOWAIT request leaves the last published projection intact.
pub struct MetadataTimeOverlay {
    writer: AtomicBool,
    sequence: AtomicU64,
    present: AtomicU8,
    atime_seconds: AtomicI64,
    atime_nanoseconds: AtomicU32,
    mtime_seconds: AtomicI64,
    mtime_nanoseconds: AtomicU32,
    ctime_seconds: AtomicI64,
    ctime_nanoseconds: AtomicU32,
}

const OVERLAY_ATIME: u8 = 1;
const OVERLAY_MTIME: u8 = 2;
const OVERLAY_CTIME: u8 = 4;

/// An admitted single-writer timestamp transaction.  Dropping an unpublished
/// reservation only releases admission; publishing is allocation-free and
/// cannot fail after I/O has mutated user-visible data.
pub struct MetadataTimeOverlayReservation {
    overlay: Arc<MetadataTimeOverlay>,
}

#[derive(Clone, Copy, Default)]
pub struct MetadataTimeOverlaySnapshot {
    pub atime: Option<crate::Timestamp>,
    pub mtime: Option<crate::Timestamp>,
    pub ctime: Option<crate::Timestamp>,
}

impl MetadataTimeOverlaySnapshot {
    pub fn apply_to(self, metadata: &mut Metadata) {
        if let Some(value) = self.atime {
            metadata.atime = value;
        }
        if let Some(value) = self.mtime {
            metadata.mtime = value;
        }
        if let Some(value) = self.ctime {
            metadata.ctime = value;
        }
    }

    pub fn merge_update(&mut self, update: MetadataUpdate) {
        if update.atime.is_some() {
            self.atime = update.atime;
        }
        if update.mtime.is_some() {
            self.mtime = update.mtime;
        }
        if update.ctime.is_some() {
            self.ctime = update.ctime;
        }
    }

    pub fn is_empty(self) -> bool {
        self.atime.is_none() && self.mtime.is_none() && self.ctime.is_none()
    }
}

impl Default for MetadataTimeOverlay {
    fn default() -> Self {
        Self {
            writer: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            present: AtomicU8::new(0),
            atime_seconds: AtomicI64::new(0),
            atime_nanoseconds: AtomicU32::new(0),
            mtime_seconds: AtomicI64::new(0),
            mtime_nanoseconds: AtomicU32::new(0),
            ctime_seconds: AtomicI64::new(0),
            ctime_nanoseconds: AtomicU32::new(0),
        }
    }
}

impl MetadataTimeOverlay {
    pub fn try_reserve(self: &Arc<Self>) -> Option<MetadataTimeOverlayReservation> {
        self.writer
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MetadataTimeOverlayReservation {
                overlay: self.clone(),
            })
    }

    /// Takes the writer permit for a blocking provider transaction.  This is
    /// intentionally a spin acquisition because it is only used outside the
    /// NOWAIT path; stable metadata readers never join it.
    pub fn reserve_blocking(self: &Arc<Self>) -> MetadataTimeOverlayReservation {
        loop {
            if let Some(reservation) = self.try_reserve() {
                return reservation;
            }
            core::hint::spin_loop();
        }
    }

    pub fn snapshot(&self) -> MetadataTimeOverlaySnapshot {
        for _ in 0..2 {
            let before = self.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                return MetadataTimeOverlaySnapshot::default();
            }
            let present = self.present.load(Ordering::Relaxed);
            let snapshot = MetadataTimeOverlaySnapshot {
                atime: (present & OVERLAY_ATIME != 0).then(|| {
                    crate::Timestamp::new(
                        self.atime_seconds.load(Ordering::Relaxed),
                        self.atime_nanoseconds.load(Ordering::Relaxed),
                    )
                }),
                mtime: (present & OVERLAY_MTIME != 0).then(|| {
                    crate::Timestamp::new(
                        self.mtime_seconds.load(Ordering::Relaxed),
                        self.mtime_nanoseconds.load(Ordering::Relaxed),
                    )
                }),
                ctime: (present & OVERLAY_CTIME != 0).then(|| {
                    crate::Timestamp::new(
                        self.ctime_seconds.load(Ordering::Relaxed),
                        self.ctime_nanoseconds.load(Ordering::Relaxed),
                    )
                }),
            };
            if self.sequence.load(Ordering::SeqCst) == before {
                return snapshot;
            }
        }
        MetadataTimeOverlaySnapshot::default()
    }

    fn publish_locked(&self, update: MetadataTimeOverlaySnapshot) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        // Publish the odd sequence before touching payload.  The explicit
        // SeqCst store/fence pair is intentional: a reader which observes an
        // even sequence must never observe a payload store from this writer
        // before it has a chance to reject the odd generation.
        self.sequence
            .store(sequence.wrapping_add(1) | 1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut present = self.present.load(Ordering::Relaxed);
        if let Some(value) = update.atime {
            self.atime_seconds.store(value.seconds(), Ordering::Relaxed);
            self.atime_nanoseconds
                .store(value.subsec_nanos(), Ordering::Relaxed);
            present |= OVERLAY_ATIME;
        }
        if let Some(value) = update.mtime {
            self.mtime_seconds.store(value.seconds(), Ordering::Relaxed);
            self.mtime_nanoseconds
                .store(value.subsec_nanos(), Ordering::Relaxed);
            present |= OVERLAY_MTIME;
        }
        if let Some(value) = update.ctime {
            self.ctime_seconds.store(value.seconds(), Ordering::Relaxed);
            self.ctime_nanoseconds
                .store(value.subsec_nanos(), Ordering::Relaxed);
            present |= OVERLAY_CTIME;
        }
        self.present.store(present, Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(2) & !1, Ordering::Release);
    }

    /// Clears only the exact snapshot that a successful provider flush made
    /// durable. A newer NOWAIT publication is retained for the next flush.
    fn clear_snapshot_locked(&self, snapshot: MetadataTimeOverlaySnapshot) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1) | 1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let current = self.snapshot_unchecked();
        let mut present = self.present.load(Ordering::Relaxed);
        if snapshot.atime.is_some() && snapshot.atime == current.atime {
            present &= !OVERLAY_ATIME;
        }
        if snapshot.mtime.is_some() && snapshot.mtime == current.mtime {
            present &= !OVERLAY_MTIME;
        }
        if snapshot.ctime.is_some() && snapshot.ctime == current.ctime {
            present &= !OVERLAY_CTIME;
        }
        self.present.store(present, Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(2) & !1, Ordering::Release);
    }

    fn snapshot_unchecked(&self) -> MetadataTimeOverlaySnapshot {
        let present = self.present.load(Ordering::Relaxed);
        MetadataTimeOverlaySnapshot {
            atime: (present & OVERLAY_ATIME != 0).then(|| {
                crate::Timestamp::new(
                    self.atime_seconds.load(Ordering::Relaxed),
                    self.atime_nanoseconds.load(Ordering::Relaxed),
                )
            }),
            mtime: (present & OVERLAY_MTIME != 0).then(|| {
                crate::Timestamp::new(
                    self.mtime_seconds.load(Ordering::Relaxed),
                    self.mtime_nanoseconds.load(Ordering::Relaxed),
                )
            }),
            ctime: (present & OVERLAY_CTIME != 0).then(|| {
                crate::Timestamp::new(
                    self.ctime_seconds.load(Ordering::Relaxed),
                    self.ctime_nanoseconds.load(Ordering::Relaxed),
                )
            }),
        }
    }
}

impl MetadataTimeOverlayReservation {
    pub fn pending_snapshot(&self) -> MetadataTimeOverlaySnapshot {
        self.overlay.snapshot_unchecked()
    }
    pub fn publish(self, update: MetadataTimeOverlaySnapshot) {
        self.overlay.publish_locked(update);
    }
    pub fn clear_flushed(self, snapshot: MetadataTimeOverlaySnapshot) {
        self.overlay.clear_snapshot_locked(snapshot);
    }
}

impl Drop for MetadataTimeOverlayReservation {
    fn drop(&mut self) {
        self.overlay.writer.store(false, Ordering::Release);
    }
}

impl Default for NodeUserData {
    fn default() -> Self {
        Self {
            data: Mutex::default(),
            writeback_errors: Mutex::new(None),
            file_attr_mutation: Arc::new(FileAttrMutationState(AtomicUsize::new(0))),
        }
    }
}

/// Shared writeback-error state for one live VFS entry.  File descriptions
/// hold independent cursors against this sequence.
#[derive(Default)]
pub struct WritebackErrorState {
    record: Mutex<WritebackErrorRecord>,
}

#[derive(Default)]
struct WritebackErrorRecord {
    sequence: u64,
    seen: bool,
    error: Option<VfsError>,
}

impl WritebackErrorState {
    pub fn publish(&self, error: VfsError) {
        let mut record = self.record.lock();
        record.sequence = record.sequence.wrapping_add(1);
        record.seen = false;
        record.error = Some(error);
    }

    /// Linux `errseq_sample()`: an outstanding unconsumed error is sampled as
    /// zero so a newly opened OFD reports it once; once any fsync advances the
    /// sequence, later opens begin at the current value.
    pub fn sample(&self) -> u64 {
        let record = self.record.lock();
        if record.error.is_some() && !record.seen {
            0
        } else {
            record.sequence
        }
    }

    /// Linux `errseq_check_and_advance()` equivalent for one OFD cursor.
    pub fn check_and_advance(&self, cursor: &mut u64) -> Option<VfsError> {
        let mut record = self.record.lock();
        if *cursor == record.sequence {
            return None;
        }
        *cursor = record.sequence;
        let error = record.error;
        if error.is_some() {
            record.seen = true;
        }
        error
    }
}

fn install_initial_data_cell(cell: &Mutex<TypeMap>, data: InitialNodeData) -> VfsResult<()> {
    let mut candidate = Some(data);
    let error = {
        let mut map = cell.lock();
        let data = candidate.as_ref().ok_or(VfsError::Io)?;
        if map.0.iter().any(|(id, _)| *id == data.type_id) {
            Some(VfsError::AlreadyExists)
        } else if map.0.len() >= 2 {
            Some(VfsError::NoMemory)
        } else {
            let data = candidate.take().ok_or(VfsError::Io)?;
            map.0.push((data.type_id, data.value));
            None
        }
    };
    drop(candidate);
    error.map_or(Ok(()), Err)
}

impl NodeUserData {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, TypeMap> {
        self.data.lock()
    }

    /// Installs one preallocated value without allocating while the cell is
    /// locked. This is used before a backend publishes a fresh inode name.
    pub fn install_initial_data(&self, data: InitialNodeData) -> VfsResult<()> {
        install_initial_data_cell(&self.data, data)
    }

    pub fn writeback_error_state(&self) -> VfsResult<Arc<WritebackErrorState>> {
        let mut state = self.writeback_errors.lock();
        if let Some(state) = state.as_ref() {
            return Ok(state.clone());
        }
        let created =
            Arc::try_new(WritebackErrorState::default()).map_err(|_| VfsError::NoMemory)?;
        *state = Some(created.clone());
        Ok(created)
    }

    /// Returns the stable inode timestamp overlay if the cache installed one.
    /// This never creates state while a metadata read is in progress.
    pub fn metadata_time_overlay(&self) -> Option<Arc<MetadataTimeOverlay>> {
        self.lock().get::<MetadataTimeOverlay>()
    }

    /// Creates the timestamp overlay before an I/O request can enter a
    /// NOWAIT completion path. Backends without persistent inode data cannot
    /// use this facility: a dentry-local overlay would split hardlink aliases.
    pub fn get_or_try_install_metadata_time_overlay(&self) -> VfsResult<Arc<MetadataTimeOverlay>> {
        self.lock()
            .try_get_or_insert_with(MetadataTimeOverlay::default)
    }

    /// NOWAIT variant: the inode attachment map is itself an admission
    /// domain, so contention is reported as `EAGAIN` rather than spinning
    /// behind an unrelated runtime attachment operation.
    pub fn try_get_or_install_metadata_time_overlay(&self) -> VfsResult<Arc<MetadataTimeOverlay>> {
        let Some(mut data) = self.data.try_lock() else {
            return Err(VfsError::WouldBlock);
        };
        data.try_get_or_insert_with(MetadataTimeOverlay::default)
    }

    pub fn try_begin_file_attr_mutation(&self) -> VfsResult<FileAttrMutationGuard> {
        let gate = self.file_attr_mutation.clone();
        let mut state = gate.0.load(Ordering::Acquire);
        loop {
            if state & FILE_ATTR_UPDATE != 0 {
                return Err(VfsError::WouldBlock);
            }
            if state == FILE_ATTR_UPDATE - 1 {
                return Err(VfsError::ResourceBusy);
            }
            let next = state + 1;
            match gate
                .0
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(FileAttrMutationGuard(gate)),
                Err(observed) => state = observed,
            }
        }
    }

    pub fn begin_file_attr_mutation(&self) -> FileAttrMutationGuard {
        loop {
            if let Ok(guard) = self.try_begin_file_attr_mutation() {
                return guard;
            }
            core::hint::spin_loop();
        }
    }

    pub fn begin_file_attr_update(&self) -> FileAttrUpdateGuard {
        let gate = self.file_attr_mutation.clone();
        loop {
            if gate
                .0
                .compare_exchange(0, FILE_ATTR_UPDATE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return FileAttrUpdateGuard(gate);
            }
            core::hint::spin_loop();
        }
    }
}

impl Drop for FileAttrMutationGuard {
    fn drop(&mut self) {
        let previous = self.0.0.fetch_sub(1, Ordering::Release);
        debug_assert!(previous != 0 && previous & FILE_ATTR_UPDATE == 0);
    }
}

impl Drop for FileAttrUpdateGuard {
    fn drop(&mut self) {
        self.0.0.store(0, Ordering::Release);
    }
}

/// One already admitted, type-erased value that may be attached to a fresh
/// directory entry before its name is published.
///
/// This is data, not an arbitrary callback: filesystem backends can install
/// it while holding namespace serialization without permitting allocation,
/// re-entrancy, or caller code under that lock.
#[derive(Clone)]
pub struct InitialNodeData {
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
}

impl InitialNodeData {
    pub fn from_shared<T: Any + Send + Sync>(value: Arc<T>) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            value,
        }
    }
}

struct Inner {
    node: Node,
    node_type: NodeType,
    reference: Reference,
    user_data: Mutex<TypeMap>,
    /// Set while this entry is queued, processed, or suspended below a child.
    cleanup_active: AtomicBool,
    /// Allocation-free link owned by the deferred-cleanup queue.
    cleanup_next: AtomicPtr<Inner>,
    /// Suspended parent resumed after this directory subtree is drained.
    cleanup_return: Mutex<Option<DirEntry>>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("node", &self.node)
            .field("node_type", &self.node_type)
            .field("reference", &self.reference)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry(Arc<Inner>);

#[derive(Debug, Clone)]
pub struct WeakDirEntry(Weak<Inner>);

impl WeakDirEntry {
    pub fn upgrade(&self) -> Option<DirEntry> {
        self.0.upgrade().map(DirEntry)
    }
}

/// One task-context pass processes at most this many directory work items.
const DEFERRED_DENTRY_CLEANUP_BATCH: usize = 64;

// Producers publish the new head with one atomic swap and then fill its link.
// A consumer that observes this sentinel leaves the queue for the next safe
// point instead of spinning in an arbitrary destruction context.
const CLEANUP_LINKING: *mut Inner = 1usize as *mut Inner;
static DEFERRED_DENTRY_CLEANUP_HEAD: AtomicPtr<Inner> = AtomicPtr::new(ptr::null_mut());
static DEFERRED_DENTRY_CLEANUP_DRAINING: AtomicBool = AtomicBool::new(false);

fn push_active_dentry_cleanup(entry: DirEntry) {
    let raw = Arc::into_raw(entry.0) as *mut Inner;
    // SAFETY: `raw` owns the strong Arc reference transferred to the queue.
    // `cleanup_active` ensures this embedded link occurs at most once until a
    // consumer removes it, and the raw Arc keeps `Inner` alive throughout.
    unsafe {
        (*raw)
            .cleanup_next
            .store(CLEANUP_LINKING, Ordering::Relaxed);
    }
    let previous = DEFERRED_DENTRY_CLEANUP_HEAD.swap(raw, Ordering::AcqRel);
    // SAFETY: the queue still owns `raw`; publishing the real link completes
    // the constant-time producer protocol described above.
    unsafe {
        (*raw).cleanup_next.store(previous, Ordering::Release);
    }
}

fn pop_active_dentry_cleanup() -> Option<DirEntry> {
    let head = DEFERRED_DENTRY_CLEANUP_HEAD.load(Ordering::Acquire);
    if head.is_null() {
        return None;
    }
    // SAFETY: a non-null queue head owns one raw Arc, so `Inner` stays alive.
    let next = unsafe { (*head).cleanup_next.load(Ordering::Acquire) };
    if next == CLEANUP_LINKING {
        return None;
    }
    if DEFERRED_DENTRY_CLEANUP_HEAD
        .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    // SAFETY: the successful exchange transferred the queue's one raw Arc to
    // this consumer. No other consumer runs concurrently, and producers cannot
    // enqueue this entry while `cleanup_active` remains set.
    unsafe {
        (*head)
            .cleanup_next
            .store(ptr::null_mut(), Ordering::Relaxed);
        Some(DirEntry(Arc::from_raw(head)))
    }
}

fn claim_dentry_cleanup(entry: &DirEntry) -> bool {
    let Ok(dir) = entry.as_dir() else {
        return false;
    };
    if dir.cache_is_retired() {
        return false;
    }
    entry
        .0
        .cleanup_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub(crate) fn defer_dentry_cache_cleanup(entry: DirEntry) {
    if claim_dentry_cleanup(&entry) {
        push_active_dentry_cleanup(entry);
    }
}

/// Returns whether a directory-cache reclamation root is waiting for the
/// dedicated task-context worker.
pub fn has_deferred_dentry_cache_cleanup_work() -> bool {
    !DEFERRED_DENTRY_CLEANUP_HEAD
        .load(Ordering::Acquire)
        .is_null()
}

fn finish_dentry_cleanup(entry: DirEntry) {
    let return_to = entry.0.cleanup_return.lock().take();
    entry.0.cleanup_active.store(false, Ordering::Release);
    drop(entry);
    if let Some(parent) = return_to {
        push_active_dentry_cleanup(parent);
    }
}

fn process_dentry_cleanup(entry: DirEntry) -> Option<DirEntry> {
    let Ok(dir) = entry.as_dir() else {
        finish_dentry_cleanup(entry);
        return None;
    };
    let Some((child, complete)) = dir.try_take_cache_cleanup_step() else {
        return Some(entry);
    };
    if let Some(child) = child
        && claim_dentry_cleanup(&child)
    {
        // A claimed child is neither queued nor processing, so its return link
        // is empty. Keeping the parent here yields an iterative depth-first
        // walk without recursion or a separately allocated subtree queue.
        *child.0.cleanup_return.lock() = Some(entry);
        push_active_dentry_cleanup(child);
        return None;
    }
    if complete {
        finish_dentry_cleanup(entry);
    } else {
        push_active_dentry_cleanup(entry);
    }
    None
}

struct DentryCleanupDrainGuard;

impl Drop for DentryCleanupDrainGuard {
    fn drop(&mut self) {
        DEFERRED_DENTRY_CLEANUP_DRAINING.store(false, Ordering::Release);
    }
}

/// Performs one bounded pass of deferred directory-cache reclamation.
///
/// Final filesystem release only publishes an intrusive work item. Consumers
/// must call this function from a context where dropping dentries and freeing
/// their cache allocations is safe. The fixed batch prevents a large cached
/// tree from turning one scheduler safe point into an unbounded pause. Until a
/// consumer runs, the queue's raw Arc deliberately keeps every pending root
/// alive: this preserves memory safety but provides no reclamation liveness.
///
/// Returns whether the queue still appeared non-empty at the end of this pass.
/// A concurrent producer may race with that advisory result; future safe
/// points should therefore call this function unconditionally.
pub fn drain_deferred_dentry_cache_cleanup() -> bool {
    // Empty safe points dominate normal operation. Avoid bouncing the global
    // single-consumer cacheline when there is no queued root. A producer that
    // races immediately after this advisory load is still observed by a later
    // unconditional scheduler safe point.
    if !has_deferred_dentry_cache_cleanup_work() {
        return false;
    }
    if DEFERRED_DENTRY_CLEANUP_DRAINING.swap(true, Ordering::AcqRel) {
        return !DEFERRED_DENTRY_CLEANUP_HEAD
            .load(Ordering::Acquire)
            .is_null();
    }
    let _guard = DentryCleanupDrainGuard;
    for _ in 0..DEFERRED_DENTRY_CLEANUP_BATCH {
        let Some(entry) = pop_active_dentry_cleanup() else {
            break;
        };
        if let Some(busy) = process_dentry_cleanup(entry) {
            push_active_dentry_cleanup(busy);
            break;
        }
    }
    !DEFERRED_DENTRY_CLEANUP_HEAD
        .load(Ordering::Acquire)
        .is_null()
}

impl From<Node> for Arc<dyn NodeOps> {
    fn from(node: Node) -> Self {
        match node {
            Node::File(file) => file.into(),
            Node::Dir(dir) => dir.into(),
        }
    }
}

#[inherit_methods(from = "self.0.node")]
impl DirEntry {
    pub fn object_key(&self) -> ObjectKey {
        self.0.node.object_key()
    }

    pub fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        self.0.node.file_attr_provider()
    }

    pub fn get_file_attr(&self) -> VfsResult<FileAttr> {
        self.0.node.get_file_attr()
    }

    pub fn try_get_file_attr(&self) -> VfsResult<FileAttr> {
        self.0.node.try_get_file_attr()
    }

    pub fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        self.0.node.set_file_attr(attr)
    }

    pub fn get_legacy_file_flags(&self) -> VfsResult<u32> {
        self.0.node.get_legacy_file_flags()
    }

    pub fn set_legacy_file_flags(&self, flags: u32) -> VfsResult<()> {
        self.0.node.set_legacy_file_flags(flags)
    }
    pub fn inode(&self) -> u64;

    pub fn filesystem(&self) -> &dyn FilesystemOps;

    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64>;

    pub fn flags(&self) -> NodeFlags;

    pub fn open(&self, read: bool, write: bool) -> VfsResult<()>;

    pub fn sync(&self, data_only: bool) -> VfsResult<()>;

    pub fn writeback_error_state(&self) -> VfsResult<Arc<WritebackErrorState>>;
}

impl DirEntry {
    /// Fallibly constructs a non-directory entry.
    ///
    /// Filesystems which cannot roll back a namespace mutation should use this
    /// constructor to admit the dentry before committing the backend change.
    pub fn try_new_file(
        node: FileNode,
        node_type: NodeType,
        reference: Reference,
    ) -> VfsResult<Self> {
        Arc::try_new(Inner {
            node: Node::File(node),
            node_type,
            reference,
            user_data: Mutex::default(),
            cleanup_active: AtomicBool::new(false),
            cleanup_next: AtomicPtr::new(ptr::null_mut()),
            cleanup_return: Mutex::new(None),
        })
        .map(Self)
        .map_err(|_| VfsError::NoMemory)
    }

    /// Fallibly constructs a directory entry from an already allocated node.
    ///
    /// Unlike [`Self::new_dir`], this does not manufacture a cyclic weak
    /// reference. Backends which need the dentry weak reference can bind
    /// `entry.downgrade()` to their preallocated node before publication.
    pub fn try_new_dir(node: DirNode, reference: Reference) -> VfsResult<Self> {
        Arc::try_new(Inner {
            node: Node::Dir(node),
            node_type: NodeType::Directory,
            reference,
            user_data: Mutex::default(),
            cleanup_active: AtomicBool::new(false),
            cleanup_next: AtomicPtr::new(ptr::null_mut()),
            cleanup_return: Mutex::new(None),
        })
        .map(Self)
        .map_err(|_| VfsError::NoMemory)
    }

    pub fn new_file(node: FileNode, node_type: NodeType, reference: Reference) -> Self {
        Self(Arc::new(Inner {
            node: Node::File(node),
            node_type,
            reference,
            user_data: Mutex::default(),
            cleanup_active: AtomicBool::new(false),
            cleanup_next: AtomicPtr::new(ptr::null_mut()),
            cleanup_return: Mutex::new(None),
        }))
    }

    pub fn new_dir(node_fn: impl FnOnce(WeakDirEntry) -> DirNode, reference: Reference) -> Self {
        Self(Arc::new_cyclic(|this| Inner {
            node: Node::Dir(node_fn(WeakDirEntry(this.clone()))),
            node_type: NodeType::Directory,
            reference,
            user_data: Mutex::default(),
            cleanup_active: AtomicBool::new(false),
            cleanup_next: AtomicPtr::new(ptr::null_mut()),
            cleanup_return: Mutex::new(None),
        }))
    }

    pub fn metadata_capabilities(&self, metadata: &Metadata) -> MetadataCapabilities {
        self.0.node.metadata_capabilities(metadata)
    }

    pub fn metadata(&self) -> VfsResult<Metadata> {
        self.0.node.metadata().map(|mut metadata| {
            metadata.node_type = self.0.node_type;
            metadata
        })
    }

    /// Returns the stable xattr provider for this inode.  Composed
    /// filesystems need this to recognize backend-owned whiteout, opaque, and
    /// origin records without attempting to turn their object identities back
    /// into path strings.
    pub fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        self.0.node.xattr_provider()
    }

    pub fn downcast<T: NodeOps>(&self) -> VfsResult<Arc<T>> {
        self.0
            .node
            .clone_inner()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    pub fn downgrade(&self) -> WeakDirEntry {
        WeakDirEntry(Arc::downgrade(&self.0))
    }

    pub fn node_type(&self) -> NodeType {
        self.0.node_type
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.reference.parent.clone()
    }

    pub fn name(&self) -> &FsName {
        &self.0.reference.name
    }

    /// Checks if the entry is a root of a mount point.
    pub fn is_root_of_mount(&self) -> bool {
        self.0.reference.is_root()
    }

    pub fn is_ancestor_of(&self, other: &Self) -> VfsResult<bool> {
        let mut current = other.clone();
        loop {
            if current.ptr_eq(self) {
                return Ok(true);
            }
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
        Ok(false)
    }

    pub(crate) fn try_collect_absolute_path(&self, components: &mut Vec<Self>) -> VfsResult<()> {
        let mut current = self.clone();
        loop {
            components.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            components.push(current.clone());
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
        Ok(())
    }

    pub fn absolute_path(&self) -> VfsResult<FsPathBuf> {
        let mut components = Vec::new();
        self.try_collect_absolute_path(&mut components)?;
        try_build_absolute_path(&components, Self::name)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.0.node, Node::File(_))
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.0.node, Node::Dir(_))
    }

    pub fn as_file(&self) -> VfsResult<&FileNode> {
        match &self.0.node {
            Node::File(file) => Ok(file),
            _ => Err(VfsError::IsADirectory),
        }
    }

    pub fn as_dir(&self) -> VfsResult<&DirNode> {
        match &self.0.node {
            Node::Dir(dir) => Ok(dir),
            _ => Err(VfsError::NotADirectory),
        }
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn as_ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Returns an allocation-free erased ownership token for this exact
    /// dentry. Linux-ABI adapters may retain it while an external endpoint
    /// needs the backend inode generation to remain discoverable.
    pub fn lifetime_token(&self) -> Arc<dyn Any + Send + Sync> {
        self.0.clone()
    }

    pub fn read_link(&self) -> VfsResult<FsPathBuf> {
        if self.node_type() != NodeType::Symlink {
            return Err(VfsError::InvalidData);
        }
        let file = self.as_file()?;
        let mut buf = vec![0; file.len()? as usize];
        file.read_at(&mut buf, 0)?;
        Ok(FsPathBuf::from_vec(buf))
    }

    pub fn user_data(&self) -> MutexGuard<'_, TypeMap> {
        self.0
            .node
            .persistent_user_data()
            .map_or_else(|| self.0.user_data.lock(), NodeUserData::lock)
    }

    /// Stable inode-generation data, if the backend supplies it.  Callers
    /// needing cross-hardlink state must not fall back to [`Self::user_data`].
    pub fn persistent_user_data(&self) -> Option<&NodeUserData> {
        self.0.node.persistent_user_data()
    }

    /// Installs one prepared value on an unpublished entry without allocating
    /// or invoking caller code while the entry's spin lock is held.
    pub(crate) fn install_initial_data(&self, data: InitialNodeData) -> VfsResult<()> {
        // `NamedCreateOptions::initial_data` promises inode-generation-stable
        // attachment. Falling back to this dentry's cache-local cell would
        // make creation appear successful and then silently lose the value
        // after a namespace epoch invalidates the dentry. Backends without a
        // persistent cell must reject the capability before publishing.
        self.0
            .node
            .persistent_user_data()
            .ok_or(VfsError::OperationNotSupported)?
            .install_initial_data(data)
    }
}

impl Pollable for DirEntry {
    fn poll(&self) -> IoEvents {
        match &self.0.node {
            Node::File(file) => file.poll(),
            Node::Dir(_dir) => IoEvents::READABLE | IoEvents::WRITABLE,
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        match &self.0.node {
            Node::File(file) => file.register(context, events),
            Node::Dir(_) => PollRegistration::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_map_publishes_the_exact_preallocated_arc() {
        let mut map = TypeMap::new();
        let prepared = Arc::new(42_u64);

        assert!(map.try_insert_shared(prepared.clone()).unwrap().is_none());
        let installed = map.get::<u64>().unwrap();
        assert!(Arc::ptr_eq(&prepared, &installed));
    }

    #[test]
    fn shared_type_map_replacement_returns_retired_ownership() {
        let mut map = TypeMap::new();
        let old = Arc::new(1_u64);
        let new = Arc::new(2_u64);
        map.try_insert_shared(old.clone()).unwrap();

        let retired = map.try_insert_shared(new.clone()).unwrap().unwrap();
        assert!(Arc::ptr_eq(&old, &retired));
        assert!(Arc::ptr_eq(&new, &map.get::<u64>().unwrap()));
    }
}
