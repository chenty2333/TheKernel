use alloc::{sync::Arc, vec::Vec};
use core::{
    mem,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use hashbrown::{HashMap, hash_map::IntoIter};

use super::DirEntry;
use crate::{
    DeviceId, Mountpoint, Mutex, NodeOps, NodePermission, NodeType, VfsError, VfsResult,
    path::{DOT, DOTDOT, FsName, FsNameBuf, FsPath, MAX_NAME_LEN, verify_entry_name},
};

/// A trait for a sink that can receive directory entries.
pub trait DirEntrySink {
    /// Accept a directory entry, returns `false` if the sink is full.
    ///
    /// `offset` is the offset of the next entry to be read.
    ///
    /// It's not recommended to operate on the node inside the `accept`
    /// function, since some filesystem may impose a lock while iterating the
    /// directory, and operating on the node may cause deadlock.
    fn accept(&mut self, name: &FsName, ino: u64, node_type: NodeType, offset: u64) -> bool;
}

impl<F: FnMut(&FsName, u64, NodeType, u64) -> bool> DirEntrySink for F {
    fn accept(&mut self, name: &FsName, ino: u64, node_type: NodeType, offset: u64) -> bool {
        self(name, ino, node_type, offset)
    }
}

struct CachedDirEntry {
    backend_epoch: u64,
    entry: DirEntry,
}

type DirChildren = HashMap<FsNameBuf, CachedDirEntry>;

pub trait DirNodeOps: NodeOps {
    /// Builds state for one opened directory.  The default is deliberately
    /// stateless; remote providers use this to retain their daemon-issued
    /// directory handle through getdents/release without sharing it between
    /// unrelated open file descriptions.
    fn open_handle(&self, _flags: u32) -> VfsResult<Option<Arc<dyn DirNodeOps>>> {
        Ok(None)
    }

    /// Releases one opened-directory operation object at its OFD boundary.
    fn release_handle(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Reads directory entries.
    ///
    /// Returns the number of entries read.
    ///
    /// Implementations should ensure that `.` and `..` are present in the
    /// result.
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize>;

    /// Lookups a directory entry by name.
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry>;

    /// Returns whether directory entries can be cached.
    ///
    /// Some filesystems (like '/proc') may not support caching directory
    /// entries, as they may change frequently or not be backed by persistent
    /// storage.
    ///
    /// If this returns `false`, the directory will not be cached in dentry and
    /// each call to [`DirNode::lookup`] will end up calling [`lookup`].
    /// Implementations should take care to handle cases where [`lookup`] is
    /// called multiple times for the same name.
    fn is_cacheable(&self) -> bool {
        true
    }

    /// Returns the current backend namespace epoch for this physical
    /// directory.
    ///
    /// Every alias that denotes the same directory must observe the same
    /// epoch, and every namespace mutation must advance it before changing
    /// visible names. The VFS tags cache entries with this value so a mutation
    /// performed through one alias cannot leave another alias serving stale
    /// dentries.
    fn namespace_epoch(&self) -> u64 {
        0
    }

    /// Looks up or creates a fully initialized directory entry.
    ///
    /// When a new inode is required, implementations must initialize every
    /// supported field in `options` before publishing `name` in the backend
    /// namespace. In particular, implementations must not create a visible
    /// inode and then apply its owner or device identity through a separate
    /// metadata update.
    ///
    /// [`CreateDisposition::OpenOrCreate`] must decide lookup versus creation
    /// under the backend's namespace serialization. A VFS-level
    /// lookup-followed-by-create sequence is not an atomic replacement for
    /// that contract.
    ///
    /// When `options.initial_data` is present, a newly created entry must
    /// contain that exact prepared allocation before the backend makes `name`
    /// visible. Existing entries returned by [`CreateDisposition::OpenOrCreate`]
    /// are left unchanged. Backends must use [`NamedCreateOptions::install_initial_data`]
    /// rather than invoking caller code under namespace serialization.
    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>>;

    /// Creates and initializes a symbolic link before publishing its name.
    ///
    /// Backends must not expose a directory entry until `target` is durable in
    /// its in-memory metadata and `user`, when provided, has been installed as
    /// the initial owner. A backend without such a primitive should return the
    /// honest default unsupported result instead of publishing an empty or
    /// temporarily mis-owned link and filling it in afterward.
    fn create_symlink(
        &self,
        _name: &FsName,
        _target: &FsPath,
        _permission: NodePermission,
        _user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink
            || options.initial_attributes.project_id.is_some()
            || options.initial_attributes.project_inherit
            || options.initial_attributes.access_acl.is_some()
            || options.initial_attributes.default_acl.is_some()
        {
            return Err(VfsError::OperationNotSupported);
        }
        self.create_symlink(name, target, options.permission, options.owner)
    }

    /// Creates an inode that is not installed in the directory namespace.
    ///
    /// Filesystems that cannot preserve same-inode link semantics should keep
    /// the default honest unsupported result.
    fn create_anonymous(&self, _options: &AnonymousOptions) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    /// Returns whether this directory backend can atomically publish a named
    /// inode of `node_type` through [`Self::create_named`].
    ///
    /// This is a pure, immutable mechanism capability. It must not inspect a
    /// particular name, perform permission checks, or mutate namespace state.
    /// Backends opt in by type; the fail-closed default prevents a higher layer
    /// from running policy hooks for an operation that cannot be published.
    /// Symbolic-link publication has the separate [`Self::supports_symlink`]
    /// capability because it must install its target before publication.
    fn supports_named_create(&self, _node_type: NodeType) -> bool {
        false
    }

    /// Returns whether this directory backend can atomically initialize and
    /// publish a symbolic link through [`Self::create_symlink`].
    ///
    /// This is a pure mechanism capability. Linux permission, security-hook,
    /// and errno policy belong to a higher layer.
    fn supports_symlink(&self) -> bool {
        false
    }

    /// Returns whether this directory backend implements same-inode hard-link
    /// publication.
    ///
    /// This is a pure, immutable mechanism capability. It must not perform
    /// permission checks, inspect credentials, or decide whether one particular
    /// source inode may be linked. Backends opt in explicitly so higher layers
    /// can avoid running policy hooks for an operation the filesystem cannot
    /// perform at all.
    fn supports_hard_links(&self) -> bool {
        false
    }

    /// Returns whether this directory backend implements removal of
    /// non-directory entries.
    ///
    /// This is a pure mechanism capability. It must not inspect a particular
    /// name or victim, perform permission checks, or mutate namespace state.
    fn supports_unlink(&self) -> bool {
        false
    }

    /// Returns whether this directory backend implements directory removal.
    ///
    /// This is a pure mechanism capability. In particular, it must not perform
    /// the per-victim emptiness check owned by [`Self::unlink`].
    fn supports_rmdir(&self) -> bool {
        false
    }

    /// Returns whether this directory backend implements ordinary rename.
    ///
    /// This is a fail-closed mechanism capability. It must not inspect one
    /// particular source/destination, perform permission checks, or mutate
    /// namespace state. Linux-ABI flag policy remains in a higher layer.
    fn supports_rename(&self) -> bool {
        false
    }

    /// Returns whether this backend has one native namespace transaction for
    /// Linux `RENAME_EXCHANGE`.  This is intentionally distinct from ordinary
    /// rename: spelling a swap as three ordinary renames exposes an
    /// intermediate name and is never ABI-correct.
    fn supports_rename_exchange(&self) -> bool {
        false
    }

    /// Returns whether the backend can commit Linux `RENAME_WHITEOUT`: move
    /// the source and install a whiteout at its old name in one namespace
    /// transaction.  Overlayfs uses this when the upper filesystem exposes
    /// the native primitive; callers must not synthesize it from unlink plus
    /// create because that resurrects lower entries between commits.
    fn supports_rename_whiteout(&self) -> bool {
        false
    }

    /// Creates a link to a node. The filesystem must atomically reject a
    /// zero-link inode unless it still owns anonymous publication state.
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry>;

    /// Unlinks a directory entry by name.
    ///
    /// If the entry is a non-empty directory, it should return `ENOTEMPTY`
    /// error.
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()>;

    /// Renames a directory entry, replacing the original entry (dst) if it
    /// already exists.
    ///
    /// If src and dst link to the same file, this should do nothing and return
    /// `Ok(())`.
    ///
    /// Implementations must validate the expected source and destination
    /// identities, the complete type matrix, and destination-directory
    /// emptiness while holding the same backend namespace serialization used
    /// for the commit. A same-object source and destination is a successful
    /// no-op.
    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()>;

    /// Atomically performs [`Self::rename`] and publishes a source-name
    /// whiteout.  The concrete filesystem owns device-node/xattr encoding and
    /// journal ordering.  The default remains fail-closed.
    fn rename_whiteout(&self, _request: RenameWhiteoutRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    /// Atomically exchanges two existing names.  The request carries both
    /// exact dentry identities sampled by the VFS; implementations must
    /// revalidate them while holding their namespace serialization.
    fn rename_exchange(&self, _request: RenameExchangeRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

/// One backend-serialized unlink operation.
#[derive(Clone, Copy)]
pub struct UnlinkRequest<'a> {
    pub name: &'a FsName,
    pub is_dir: bool,
    /// When present, the name must still denote this backend object. Backends
    /// must compare a stable object identity, not VFS dentry pointer identity
    /// or a reusable inode number alone.
    pub expected: Option<&'a DirEntry>,
}

/// One backend-serialized rename operation.
#[derive(Clone, Copy)]
pub struct RenameRequest<'a> {
    pub src_name: &'a FsName,
    pub src: &'a DirEntry,
    pub dst_dir: &'a DirNode,
    pub dst_name: &'a FsName,
    /// The destination observed by the caller, or `None` when it was absent.
    /// The backend must reject a changed snapshot before committing.
    pub dst: Option<&'a DirEntry>,
}

/// One backend-serialized Linux `RENAME_EXCHANGE` operation.  This deliberately
/// does not reuse [`RenameRequest`]: both names must exist and neither is a
/// replacement victim.
#[derive(Clone, Copy)]
pub struct RenameExchangeRequest<'a> {
    pub src_name: &'a FsName,
    pub src: &'a DirEntry,
    pub dst_dir: &'a DirNode,
    pub dst_name: &'a FsName,
    pub dst: &'a DirEntry,
}

/// One backend-serialized Linux `RENAME_WHITEOUT` operation.  It is separate
/// from ordinary replacement because publication of the source-name whiteout
/// is part of the same transaction, not cleanup after `rename` returns.
#[derive(Clone, Copy)]
pub struct RenameWhiteoutRequest<'a> {
    pub src_name: &'a FsName,
    pub src: &'a DirEntry,
    pub dst_dir: &'a DirNode,
    pub dst_name: &'a FsName,
    pub dst: Option<&'a DirEntry>,
}

/// Snapshot of one directory namespace used to revalidate a prepared
/// Linux-visible mutation before publication.
///
/// `cache_generation` covers mutations started through this VFS object while
/// `backend_epoch` covers the same directory reached through another alias.
/// Exact source/destination identities remain the final authority inside the
/// backend's namespace lock.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NamespaceGeneration {
    cache_generation: u64,
    backend_epoch: u64,
}

/// Options for opening (or creating) a directory entry.
///
/// See [`DirNode::open_file`] for more details.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub create: bool,
    pub create_new: bool,
    pub node_type: NodeType,
    pub permission: NodePermission,
    pub user: Option<(u32, u32)>, // (uid, gid)
    /// Attributes admitted by the Linux pathname layer for an O_CREAT
    /// publication.  They travel through `open_file_with_status` into the
    /// provider's one namespace transaction rather than becoming post-create
    /// repairs.
    pub initial_attributes: PreparedInitialAttributes,
}

/// Initial attributes for an inode published under a directory name.
///
/// A filesystem may ignore fields that its
/// [`crate::FilesystemOps::metadata_update_capabilities`] does not advertise.
/// Every advertised field, however, must have its requested value before the
/// new name becomes visible.
#[derive(Clone)]
pub struct NamedCreateOptions {
    pub node_type: NodeType,
    pub permission: NodePermission,
    pub owner: Option<(u32, u32)>,
    pub rdev: Option<DeviceId>,
    pub initial_data: Option<super::InitialNodeData>,
    /// Fully prepared inode attributes which must be committed while the
    /// backend still owns namespace serialization and before `name` can be
    /// discovered.  ACLs are typed validated values rather than universal
    /// xattr byte vectors, so each provider must state its native contract.
    pub initial_attributes: PreparedInitialAttributes,
}

/// A validated Linux `posix_acl_xattr` value prepared before a name is
/// published.  The VFS deliberately keeps the original bytes: a provider
/// needs the exact on-wire/on-disk representation, but must not be handed an
/// untyped blob which it could confuse with an arbitrary xattr.
#[derive(Debug, Clone)]
pub struct PreparedPosixAcl(Vec<u8>);

impl PreparedPosixAcl {
    pub fn parse(raw: Vec<u8>) -> VfsResult<Self> {
        const HEADER: usize = 4;
        const ENTRY: usize = 8;
        const VERSION: u32 = 0x0002;
        const USER_OBJ: u16 = 0x01;
        const USER: u16 = 0x02;
        const GROUP_OBJ: u16 = 0x04;
        const GROUP: u16 = 0x08;
        const MASK: u16 = 0x10;
        const OTHER: u16 = 0x20;
        if raw.len() < HEADER
            || (raw.len() - HEADER) % ENTRY != 0
            || u32::from_le_bytes(
                raw[..HEADER]
                    .try_into()
                    .map_err(|_| VfsError::InvalidInput)?,
            ) != VERSION
        {
            return Err(VfsError::InvalidInput);
        }
        let entries = &raw[HEADER..];
        let count = entries.len() / ENTRY;
        if !(3..=0x1_0000).contains(&count) {
            return Err(VfsError::InvalidInput);
        }
        let mut tags = Vec::new();
        tags.try_reserve_exact(count)
            .map_err(|_| VfsError::NoMemory)?;
        for entry in entries.chunks_exact(ENTRY) {
            let tag =
                u16::from_le_bytes(entry[..2].try_into().map_err(|_| VfsError::InvalidInput)?);
            let perm =
                u16::from_le_bytes(entry[2..4].try_into().map_err(|_| VfsError::InvalidInput)?);
            let id =
                u32::from_le_bytes(entry[4..8].try_into().map_err(|_| VfsError::InvalidInput)?);
            if perm & !7 != 0
                || !matches!(tag, USER_OBJ | USER | GROUP_OBJ | GROUP | MASK | OTHER)
                || (matches!(tag, USER_OBJ | GROUP_OBJ | MASK | OTHER) && id != u32::MAX)
                || (matches!(tag, USER | GROUP) && id == u32::MAX)
            {
                return Err(VfsError::InvalidInput);
            }
            tags.push((tag, id));
        }
        if tags.first().map(|entry| entry.0) != Some(USER_OBJ)
            || tags.last().map(|entry| entry.0) != Some(OTHER)
        {
            return Err(VfsError::InvalidInput);
        }
        let mut at = 1;
        let mut previous = None;
        while at < tags.len() && tags[at].0 == USER {
            if previous.is_some_and(|id| id >= tags[at].1) {
                return Err(VfsError::InvalidInput);
            }
            previous = Some(tags[at].1);
            at += 1;
        }
        if tags.get(at).map(|entry| entry.0) != Some(GROUP_OBJ) {
            return Err(VfsError::InvalidInput);
        }
        at += 1;
        previous = None;
        while at < tags.len() && tags[at].0 == GROUP {
            if previous.is_some_and(|id| id >= tags[at].1) {
                return Err(VfsError::InvalidInput);
            }
            previous = Some(tags[at].1);
            at += 1;
        }
        let extended = tags.iter().any(|entry| matches!(entry.0, USER | GROUP));
        if extended {
            if tags.get(at).map(|entry| entry.0) != Some(MASK) {
                return Err(VfsError::InvalidInput);
            }
            at += 1;
        }
        if at + 1 != tags.len() || tags[at].0 != OTHER {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self(raw))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
impl AsRef<[u8]> for PreparedPosixAcl {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}
impl Deref for PreparedPosixAcl {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Initial namespace-visible inode state prepared by the Linux adapter.
///
/// `project_id` and ACL blobs are not post-create fixups. A writable provider
/// either installs every requested member before publishing the dentry or
/// returns an error with no visible name.
#[derive(Debug, Clone, Default)]
pub struct PreparedInitialAttributes {
    pub project_id: Option<u32>,
    pub project_inherit: bool,
    pub access_acl: Option<PreparedPosixAcl>,
    pub default_acl: Option<PreparedPosixAcl>,
}

impl NamedCreateOptions {
    /// Installs already admitted opaque data while the backend still excludes
    /// lookup of the new name. No caller callback is executed here.
    pub fn install_initial_data(&self, entry: &DirEntry) -> VfsResult<()> {
        if let Some(data) = self.initial_data.as_ref() {
            entry.install_initial_data(data.clone())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CreateDisposition {
    /// Fail if the name already exists.
    Exclusive,
    /// Return the existing entry or atomically create a new one.
    OpenOrCreate,
}

#[derive(Debug, Clone)]
pub struct CreateOutcome<T> {
    pub entry: T,
    pub created: bool,
}

impl<T> CreateOutcome<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> CreateOutcome<U> {
        CreateOutcome {
            entry: f(self.entry),
            created: self.created,
        }
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            create: false,
            create_new: false,
            node_type: NodeType::RegularFile,
            permission: NodePermission::default(),
            user: None,
            initial_attributes: PreparedInitialAttributes::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnonymousOptions {
    pub node_type: NodeType,
    pub permission: NodePermission,
    pub user: Option<(u32, u32)>,
    pub linkable: bool,
}

pub struct DirNode {
    ops: Arc<dyn DirNodeOps>,
    cache: Mutex<DirChildren>,
    cache_generation: AtomicU64,
    cache_cleanup: Mutex<Option<IntoIter<FsNameBuf, CachedDirEntry>>>,
    cache_retired: AtomicBool,
    pub(crate) mountpoint: Mutex<Option<Arc<Mountpoint>>>,
}

impl Deref for DirNode {
    type Target = dyn NodeOps;

    fn deref(&self) -> &Self::Target {
        &*self.ops
    }
}

impl From<DirNode> for Arc<dyn NodeOps> {
    fn from(node: DirNode) -> Self {
        node.ops.clone()
    }
}

impl DirNode {
    /// Opens an OFD-private directory operation handle when the backend needs
    /// one.  The returned handle must be released at the matching OFD close.
    pub fn open_handle(&self, flags: u32) -> VfsResult<Option<Arc<dyn DirNodeOps>>> {
        self.ops.open_handle(flags)
    }

    pub fn new(ops: Arc<dyn DirNodeOps>) -> Self {
        Self {
            ops,
            cache: Mutex::default(),
            cache_generation: AtomicU64::new(0),
            cache_cleanup: Mutex::new(None),
            cache_retired: AtomicBool::new(false),
            mountpoint: Mutex::default(),
        }
    }

    pub fn inner(&self) -> &Arc<dyn DirNodeOps> {
        &self.ops
    }

    pub fn downcast<T: DirNodeOps>(&self) -> VfsResult<Arc<T>> {
        self.ops
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }

    fn forget_entry(children: &mut DirChildren, name: &FsName) {
        if let Some(cached) = children.remove(name) {
            super::defer_dentry_cache_cleanup(cached.entry);
        }
    }

    fn defer_uncached_directory(entry: &DirEntry) {
        // A directory not anchored in its parent's cache cannot be discovered
        // by final root cleanup. Retire its own cache asynchronously so an
        // external handle cannot create an unreachable child/parent cycle.
        if entry.is_dir() {
            super::defer_dentry_cache_cleanup(entry.clone());
        }
    }

    fn bump_cache_generation(&self) -> u64 {
        self.cache_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// Captures the current local and backend namespace generations.
    pub fn namespace_generation(&self) -> NamespaceGeneration {
        NamespaceGeneration {
            cache_generation: self.cache_generation.load(Ordering::Acquire),
            backend_epoch: self.ops.namespace_epoch(),
        }
    }

    /// Returns whether no namespace mutation has invalidated `generation`.
    pub fn namespace_generation_is_current(&self, generation: NamespaceGeneration) -> bool {
        self.namespace_generation() == generation
    }

    fn prepare_cache_name(&self, name: &FsName) -> Option<FsNameBuf> {
        if !self.ops.is_cacheable() {
            return None;
        }
        let mut bytes = alloc::vec::Vec::new();
        bytes.try_reserve_exact(name.as_bytes().len()).ok()?;
        bytes.extend_from_slice(name.as_bytes());
        FsNameBuf::from_vec(bytes).ok()
    }

    /// Invalidates one cached name before a backend namespace mutation.
    ///
    /// The returned generation may be used to publish the backend result only
    /// if no later mutation has started. The cache lock is deliberately
    /// released before the backend operation runs.
    fn begin_name_mutation(&self, name: &FsName) -> u64 {
        let mut children = self.cache.lock();
        Self::forget_entry(&mut children, name);
        self.bump_cache_generation()
    }

    fn cache_entry_if_current(
        &self,
        cache_name: Option<FsNameBuf>,
        entry: DirEntry,
        generation: u64,
        backend_epoch: u64,
    ) -> DirEntry {
        let Some(cache_name) = cache_name else {
            Self::defer_uncached_directory(&entry);
            return entry;
        };

        let mut children = self.cache.lock();
        if self.cache_retired.load(Ordering::Acquire)
            || self.cache_generation.load(Ordering::Acquire) != generation
            || self.ops.namespace_epoch() != backend_epoch
        {
            drop(children);
            Self::defer_uncached_directory(&entry);
            return entry;
        }

        if let Some(cached) = children.get(&cache_name)
            && cached.backend_epoch == backend_epoch
        {
            let cached = cached.entry.clone();
            drop(children);
            Self::defer_uncached_directory(&entry);
            return cached;
        }
        Self::forget_entry(&mut children, &cache_name);
        if children.try_reserve(1).is_err() {
            drop(children);
            Self::defer_uncached_directory(&entry);
            return entry;
        }
        children.insert(
            cache_name,
            CachedDirEntry {
                backend_epoch,
                entry: entry.clone(),
            },
        );
        entry
    }

    /// Looks up a directory entry by name.
    pub fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if !self.ops.is_cacheable() {
            return self.ops.lookup(name).inspect(|entry| {
                Self::defer_uncached_directory(entry);
            });
        }

        let cache_name = self.prepare_cache_name(name);
        let backend_epoch = self.ops.namespace_epoch();
        let generation = {
            let mut children = self.cache.lock();
            if !self.cache_retired.load(Ordering::Acquire)
                && let Some(cached) = children.get(name)
                && cached.backend_epoch == backend_epoch
            {
                return Ok(cached.entry.clone());
            }
            Self::forget_entry(&mut children, name);
            self.cache_generation.load(Ordering::Acquire)
        };

        // Filesystem lookup may sleep or perform device I/O. Never carry the
        // spin-based dentry cache lock across it.
        let entry = self.ops.lookup(name)?;
        Ok(self.cache_entry_if_current(cache_name, entry, generation, backend_epoch))
    }

    /// Looks up a directory entry by name in cache.
    pub fn lookup_cache(&self, name: &FsName) -> Option<DirEntry> {
        if self.ops.is_cacheable() {
            let backend_epoch = self.ops.namespace_epoch();
            let mut cache = self.cache.lock();
            if self.cache_retired.load(Ordering::Acquire) {
                return None;
            }
            if let Some(cached) = cache.get(name)
                && cached.backend_epoch == backend_epoch
            {
                return Some(cached.entry.clone());
            }
            Self::forget_entry(&mut cache, name);
            None
        } else {
            None
        }
    }

    /// Inserts a directory entry into the cache.
    pub fn insert_cache(&self, name: FsNameBuf, entry: DirEntry) -> Option<DirEntry> {
        if self.ops.is_cacheable() {
            let backend_epoch = self.ops.namespace_epoch();
            let mut cache = self.cache.lock();
            if self.cache_retired.load(Ordering::Acquire) {
                drop(cache);
                super::defer_dentry_cache_cleanup(entry);
                None
            } else if cache.contains_key(&name) || cache.try_reserve(1).is_ok() {
                cache
                    .insert(
                        name,
                        CachedDirEntry {
                            backend_epoch,
                            entry,
                        },
                    )
                    .map(|cached| cached.entry)
            } else {
                drop(cache);
                Self::defer_uncached_directory(&entry);
                None
            }
        } else {
            Self::defer_uncached_directory(&entry);
            None
        }
    }

    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.ops.read_dir(offset, sink)
    }

    /// Returns whether this directory backend implements named publication for
    /// `node_type`.
    pub fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.ops.supports_named_create(node_type)
    }

    /// Returns whether this directory backend implements symbolic-link
    /// publication.
    pub fn supports_symlink(&self) -> bool {
        self.ops.supports_symlink()
    }

    /// Returns whether this directory backend implements hard links.
    pub fn supports_hard_links(&self) -> bool {
        self.ops.supports_hard_links()
    }

    /// Returns whether this directory backend implements non-directory removal.
    pub fn supports_unlink(&self) -> bool {
        self.ops.supports_unlink()
    }

    /// Returns whether this directory backend implements directory removal.
    pub fn supports_rmdir(&self) -> bool {
        self.ops.supports_rmdir()
    }

    /// Returns whether the backend implements ordinary rename.
    pub fn supports_rename(&self) -> bool {
        self.ops.supports_rename()
    }

    /// Returns whether the backend implements a native atomic rename exchange.
    pub fn supports_rename_exchange(&self) -> bool {
        self.ops.supports_rename_exchange()
    }

    /// Returns whether the backend implements a native rename-with-whiteout.
    pub fn supports_rename_whiteout(&self) -> bool {
        self.ops.supports_rename_whiteout()
    }

    /// Creates a link to a node.
    pub fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let entry = self.ops.link(name, node)?;
        let backend_epoch = self.ops.namespace_epoch();
        Ok(self.cache_entry_if_current(cache_name, entry, generation, backend_epoch))
    }

    /// Unlinks a directory entry by name.
    pub fn unlink(&self, name: &FsName, is_dir: bool) -> VfsResult<()> {
        self.unlink_inner(name, is_dir, None)
    }

    /// Unlinks `name` only if it still denotes `expected`.
    pub fn unlink_checked(
        &self,
        name: &FsName,
        is_dir: bool,
        expected: &DirEntry,
    ) -> VfsResult<()> {
        self.unlink_inner(name, is_dir, Some(expected))
    }

    fn unlink_inner(
        &self,
        name: &FsName,
        is_dir: bool,
        expected: Option<&DirEntry>,
    ) -> VfsResult<()> {
        verify_entry_name(name)?;
        self.begin_name_mutation(name);
        self.ops.unlink(UnlinkRequest {
            name,
            is_dir,
            expected,
        })
    }

    /// Returns whether the directory contains children.
    pub fn has_children(&self) -> VfsResult<bool> {
        let mut has_children = false;
        self.read_dir(0, &mut |name: &FsName, _, _, _| {
            if name != DOT && name != DOTDOT {
                has_children = true;
                false
            } else {
                true
            }
        })?;
        Ok(has_children)
    }

    /// Atomically looks up or creates a fully initialized named inode.
    pub fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        verify_entry_name(name)?;
        if !self.supports_named_create(options.node_type) {
            return Err(VfsError::OperationNotPermitted);
        }
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let outcome = self.ops.create_named(name, options, disposition)?;
        let backend_epoch = self.ops.namespace_epoch();
        Ok(CreateOutcome {
            entry: self.cache_entry_if_current(
                cache_name,
                outcome.entry,
                generation,
                backend_epoch,
            ),
            created: outcome.created,
        })
    }

    /// Creates a directory entry.
    pub fn create(
        &self,
        name: &FsName,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        self.create_named(
            name,
            &NamedCreateOptions {
                node_type,
                permission,
                owner: None,
                rdev: None,
                initial_data: None,
                initial_attributes: Default::default(),
            },
            CreateDisposition::Exclusive,
        )
        .map(|outcome| outcome.entry)
    }

    /// Creates a fully initialized symbolic link and then publishes it in the
    /// directory cache.
    pub fn create_symlink(
        &self,
        name: &FsName,
        target: &FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        if !self.supports_symlink() {
            return Err(VfsError::OperationNotPermitted);
        }
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let entry = self.ops.create_symlink(name, target, permission, user)?;
        let backend_epoch = self.ops.namespace_epoch();
        Ok(self.cache_entry_if_current(cache_name, entry, generation, backend_epoch))
    }

    pub fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let entry = self.ops.create_symlink_prepared(name, target, options)?;
        let backend_epoch = self.ops.namespace_epoch();
        Ok(self.cache_entry_if_current(cache_name, entry, generation, backend_epoch))
    }

    /// Creates an inode without inserting a named directory-cache entry.
    pub fn create_anonymous(&self, options: &AnonymousOptions) -> VfsResult<DirEntry> {
        self.ops.create_anonymous(options).inspect(|entry| {
            Self::defer_uncached_directory(entry);
        })
    }

    /// Renames a directory entry.
    pub fn rename(
        &self,
        src_name: &FsName,
        src: &DirEntry,
        dst_dir: &Self,
        dst_name: &FsName,
        dst: Option<&DirEntry>,
    ) -> VfsResult<()> {
        if !self.supports_rename() {
            return Err(VfsError::OperationNotPermitted);
        }
        verify_entry_name(src_name)?;
        verify_entry_name(dst_name)?;
        self.begin_name_mutation(src_name);
        dst_dir.begin_name_mutation(dst_name);
        self.ops.rename(RenameRequest {
            src_name,
            src,
            dst_dir,
            dst_name,
            dst,
        })
    }

    /// Backend-native `RENAME_WHITEOUT` transaction.  Cache invalidation is
    /// identical to ordinary rename, but no observer can see the old lower
    /// name between the move and whiteout publication.
    pub fn rename_whiteout(
        &self,
        src_name: &FsName,
        src: &DirEntry,
        dst_dir: &Self,
        dst_name: &FsName,
        dst: Option<&DirEntry>,
    ) -> VfsResult<()> {
        if !self.ops.supports_rename_whiteout() {
            return Err(VfsError::OperationNotSupported);
        }
        verify_entry_name(src_name)?;
        verify_entry_name(dst_name)?;
        self.begin_name_mutation(src_name);
        dst_dir.begin_name_mutation(dst_name);
        self.ops.rename_whiteout(RenameWhiteoutRequest {
            src_name,
            src,
            dst_dir,
            dst_name,
            dst,
        })
    }

    /// Backend-native `RENAME_EXCHANGE` transaction.
    pub fn rename_exchange(
        &self,
        src_name: &FsName,
        src: &DirEntry,
        dst_dir: &Self,
        dst_name: &FsName,
        dst: &DirEntry,
    ) -> VfsResult<()> {
        if !self.ops.supports_rename_exchange() {
            return Err(VfsError::OperationNotSupported);
        }
        verify_entry_name(src_name)?;
        verify_entry_name(dst_name)?;
        self.begin_name_mutation(src_name);
        dst_dir.begin_name_mutation(dst_name);
        self.ops.rename_exchange(RenameExchangeRequest {
            src_name,
            src,
            dst_dir,
            dst_name,
            dst,
        })
    }

    /// Opens (or creates) a file in the directory.
    pub fn open_file(&self, name: &FsName, options: &OpenOptions) -> VfsResult<DirEntry> {
        self.open_file_with_status(name, options)
            .map(|(entry, _created)| entry)
    }

    /// Opens (or creates) a file and reports whether this call created it.
    ///
    /// The backend decides the status under its namespace serialization. The
    /// dentry cache is never used as the authority for an open-or-create race.
    pub fn open_file_with_status(
        &self,
        name: &FsName,
        options: &OpenOptions,
    ) -> VfsResult<(DirEntry, bool)> {
        verify_entry_name(name)?;

        if options.create {
            let outcome = self.create_named(
                name,
                &NamedCreateOptions {
                    node_type: options.node_type,
                    permission: options.permission,
                    owner: options.user,
                    rdev: None,
                    initial_data: None,
                    initial_attributes: options.initial_attributes.clone(),
                },
                if options.create_new {
                    CreateDisposition::Exclusive
                } else {
                    CreateDisposition::OpenOrCreate
                },
            )?;
            return Ok((outcome.entry, outcome.created));
        }

        let entry = self.lookup(name)?;
        if options.create_new {
            return Err(VfsError::AlreadyExists);
        }
        Ok((entry, false))
    }

    pub fn mountpoint(&self) -> Option<Arc<Mountpoint>> {
        self.mountpoint.lock().clone()
    }

    pub fn is_mountpoint(&self) -> bool {
        self.mountpoint.lock().is_some()
    }

    pub(super) fn cache_is_retired(&self) -> bool {
        self.cache_retired.load(Ordering::Acquire)
    }

    /// Retires this cache and releases at most one cached child.
    ///
    /// The detached hash-map iterator persists in the dentry between passes,
    /// so neither traversal storage nor a recursive call stack is needed.
    pub(super) fn try_take_cache_cleanup_step(&self) -> Option<(Option<DirEntry>, bool)> {
        let mut cache = self.cache.try_lock()?;
        let mut cleanup = self.cache_cleanup.try_lock()?;
        if !self.cache_retired.load(Ordering::Acquire) {
            self.cache_retired.store(true, Ordering::Release);
            let entries = mem::take(&mut *cache);
            *cleanup = Some(entries.into_iter());
        }
        drop(cache);

        let child = cleanup
            .as_mut()
            .and_then(Iterator::next)
            .map(|(_, cached)| cached.entry);
        let complete = cleanup.as_ref().is_none_or(|entries| entries.len() == 0);
        if complete {
            cleanup.take();
        }
        Some((child, complete))
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{
        CreateOutcome, FilesystemOps, Metadata, MetadataUpdate, NodeOps, Reference, RenameRequest,
        UnlinkRequest,
    };

    #[derive(Default)]
    struct DefaultMutationCapabilities {
        named_create_calls: AtomicUsize,
        symlink_calls: AtomicUsize,
    }

    impl NodeOps for DefaultMutationCapabilities {
        fn inode(&self) -> u64 {
            0
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            unreachable!()
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            unreachable!()
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            unreachable!()
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            unreachable!()
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for DefaultMutationCapabilities {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            unreachable!()
        }

        fn lookup(&self, _name: &FsName) -> VfsResult<DirEntry> {
            unreachable!()
        }

        fn create_named(
            &self,
            _name: &FsName,
            _options: &NamedCreateOptions,
            _disposition: CreateDisposition,
        ) -> VfsResult<CreateOutcome<DirEntry>> {
            self.named_create_calls.fetch_add(1, Ordering::Relaxed);
            Err(VfsError::Unsupported)
        }

        fn create_symlink(
            &self,
            _name: &FsName,
            _target: &FsPath,
            _permission: NodePermission,
            _user: Option<(u32, u32)>,
        ) -> VfsResult<DirEntry> {
            self.symlink_calls.fetch_add(1, Ordering::Relaxed);
            Err(VfsError::Unsupported)
        }

        fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
            unreachable!()
        }

        fn unlink(&self, _request: UnlinkRequest<'_>) -> VfsResult<()> {
            unreachable!()
        }

        fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
            unreachable!()
        }
    }

    #[test]
    fn mutation_capabilities_are_fail_closed_by_default() {
        let backend = Arc::new(DefaultMutationCapabilities::default());
        let entry =
            DirEntry::try_new_dir(DirNode::new(backend.clone()), Reference::anonymous()).unwrap();
        let directory = entry.as_dir().unwrap();

        for node_type in [
            NodeType::Unknown,
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::Directory,
            NodeType::BlockDevice,
            NodeType::RegularFile,
            NodeType::Symlink,
            NodeType::Socket,
        ] {
            assert!(!backend.supports_named_create(node_type));
            assert!(!directory.supports_named_create(node_type));
        }
        assert!(!backend.supports_symlink());
        assert!(!directory.supports_symlink());
        assert!(!backend.supports_hard_links());
        assert!(!backend.supports_unlink());
        assert!(!backend.supports_rmdir());
        assert!(!backend.supports_rename());
        assert!(!directory.supports_hard_links());
        assert!(!directory.supports_unlink());
        assert!(!directory.supports_rmdir());
        assert!(!directory.supports_rename());

        let generation = directory.namespace_generation();
        let create_options = NamedCreateOptions {
            node_type: NodeType::RegularFile,
            permission: NodePermission::from_bits_truncate(0o600),
            owner: None,
            rdev: None,
            initial_data: None,
            initial_attributes: Default::default(),
        };
        assert_eq!(
            directory
                .create_named(
                    FsName::new(b"file"),
                    &create_options,
                    CreateDisposition::Exclusive,
                )
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
        assert!(directory.namespace_generation_is_current(generation));
        assert_eq!(backend.named_create_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            directory
                .create_symlink(
                    FsName::new(b"link"),
                    FsPath::new(b"target"),
                    NodePermission::from_bits_truncate(0o777),
                    None,
                )
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
        assert!(directory.namespace_generation_is_current(generation));
        assert_eq!(backend.symlink_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            directory
                .rename(
                    FsName::new(b"source"),
                    &entry,
                    directory,
                    FsName::new(b"target"),
                    None,
                )
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
    }
}
