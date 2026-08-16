mod dir;
mod file;
mod xattr;

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    any::{Any, TypeId},
    fmt,
    hash::{Hash, Hasher},
    mem,
    ops::Deref,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    task::Context,
};

use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use bitflags::bitflags;
pub use dir::*;
pub use file::*;
use hashbrown::Equivalent;
use inherit_methods_macro::inherit_methods;
use smallvec::SmallVec;
pub use xattr::*;

use crate::{
    FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard, NodeType, VfsError, VfsResult,
    path::{PathBuf, try_build_absolute_path},
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

/// Filesystem node operationss
#[allow(clippy::len_without_is_empty)]
pub trait NodeOps: Send + Sync + 'static {
    /// Gets the inode number of the node.
    fn inode(&self) -> u64;

    /// Gets the metadata of the node.
    fn metadata(&self) -> VfsResult<Metadata>;

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

    /// Returns the stable per-inode extended-attribute provider, when this
    /// backend has one. A missing provider means honest `EOPNOTSUPP` rather
    /// than an ephemeral VFS-side store.
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceKey {
    path_hash: u64,
    components: Vec<String>,
    anonymous_id: Option<u64>,
}

impl Hash for ReferenceKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path_hash.hash(state);
        self.anonymous_id.hash(state);
    }
}

pub struct ReferenceKeyRef<'a> {
    reference: &'a Reference,
}

impl Hash for ReferenceKeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.reference.path_hash.hash(state);
        self.reference.anonymous_id.hash(state);
    }
}

impl Equivalent<ReferenceKey> for ReferenceKeyRef<'_> {
    fn equivalent(&self, key: &ReferenceKey) -> bool {
        if self.reference.anonymous_id != key.anonymous_id
            || self.reference.path_hash != key.path_hash
        {
            return false;
        }
        if self.reference.anonymous_id.is_some() {
            return true;
        }

        let mut current = Some(self.reference);
        for expected in &key.components {
            let Some(reference) = current else {
                return false;
            };
            if reference.name != *expected {
                return false;
            }
            current = reference.parent.as_ref().map(|parent| &parent.0.reference);
        }
        current.is_none()
    }
}

static ANONYMOUS_REFERENCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct Reference {
    parent: Option<DirEntry>,
    name: String,
    anonymous_id: Option<u64>,
    path_hash: u64,
}

impl Reference {
    fn extend_path_hash(mut hash: u64, name: &str) -> u64 {
        const FNV_PRIME: u64 = 0x100000001b3;
        for byte in name.len().to_le_bytes().iter().chain(name.as_bytes()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    pub fn new(parent: Option<DirEntry>, name: String) -> Self {
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
    pub fn try_new(parent: Option<DirEntry>, name: &str) -> VfsResult<Self> {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        let mut owned_name = String::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| VfsError::NoMemory)?;
        owned_name.push_str(name);
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
        Self::new(None, String::new())
    }

    pub fn anonymous() -> Self {
        let anonymous_id = ANONYMOUS_REFERENCE_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            parent: None,
            name: String::new(),
            anonymous_id: Some(anonymous_id),
            path_hash: anonymous_id,
        }
    }

    pub fn try_key(&self) -> VfsResult<ReferenceKey> {
        let mut components = Vec::new();
        if self.anonymous_id.is_none() {
            let mut current = Some(self);
            while let Some(reference) = current {
                components.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                let mut name = String::new();
                name.try_reserve(reference.name.len())
                    .map_err(|_| VfsError::NoMemory)?;
                name.push_str(&reference.name);
                components.push(name);
                current = reference.parent.as_ref().map(|parent| &parent.0.reference);
            }
        }
        Ok(ReferenceKey {
            path_hash: self.path_hash,
            components,
            anonymous_id: self.anonymous_id,
        })
    }

    pub fn key_ref(&self) -> ReferenceKeyRef<'_> {
        ReferenceKeyRef { reference: self }
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
#[derive(Default)]
pub struct NodeUserData(Mutex<TypeMap>);

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
        self.0.lock()
    }

    /// Installs one preallocated value without allocating while the cell is
    /// locked. This is used before a backend publishes a fresh inode name.
    pub fn install_initial_data(&self, data: InitialNodeData) -> VfsResult<()> {
        install_initial_data_cell(&self.0, data)
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
    pub fn inode(&self) -> u64;

    pub fn filesystem(&self) -> &dyn FilesystemOps;

    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64>;

    pub fn flags(&self) -> NodeFlags;

    pub fn open(&self, read: bool, write: bool) -> VfsResult<()>;

    pub fn sync(&self, data_only: bool) -> VfsResult<()>;
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

    pub fn metadata(&self) -> VfsResult<Metadata> {
        self.0.node.metadata().map(|mut metadata| {
            metadata.node_type = self.0.node_type;
            metadata
        })
    }

    pub(crate) fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
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

    pub fn try_key(&self) -> VfsResult<ReferenceKey> {
        self.0.reference.try_key()
    }

    pub fn key_ref(&self) -> ReferenceKeyRef<'_> {
        self.0.reference.key_ref()
    }

    pub fn node_type(&self) -> NodeType {
        self.0.node_type
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.reference.parent.clone()
    }

    pub fn name(&self) -> &str {
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

    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
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

    pub fn read_link(&self) -> VfsResult<String> {
        if self.node_type() != NodeType::Symlink {
            return Err(VfsError::InvalidData);
        }
        let file = self.as_file()?;
        let mut buf = vec![0; file.len()? as usize];
        file.read_at(&mut buf, 0)?;
        String::from_utf8(buf).map_err(|_| VfsError::InvalidData)
    }

    pub fn user_data(&self) -> MutexGuard<'_, TypeMap> {
        self.0
            .node
            .persistent_user_data()
            .map_or_else(|| self.0.user_data.lock(), NodeUserData::lock)
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
