use alloc::{string::String, sync::Arc};
use core::{
    mem,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use hashbrown::{HashMap, hash_map::IntoIter};

use super::DirEntry;
use crate::{
    DeviceId, Mountpoint, Mutex, NodeOps, NodePermission, NodeType, VfsError, VfsResult,
    path::{DOT, DOTDOT, MAX_NAME_LEN, verify_entry_name},
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
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool;
}

impl<F: FnMut(&str, u64, NodeType, u64) -> bool> DirEntrySink for F {
    fn accept(&mut self, name: &str, ino: u64, node_type: NodeType, offset: u64) -> bool {
        self(name, ino, node_type, offset)
    }
}

struct CachedDirEntry {
    backend_epoch: u64,
    entry: DirEntry,
}

type DirChildren = HashMap<String, CachedDirEntry>;

pub trait DirNodeOps: NodeOps {
    /// Reads directory entries.
    ///
    /// Returns the number of entries read.
    ///
    /// Implementations should ensure that `.` and `..` are present in the
    /// result.
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize>;

    /// Lookups a directory entry by name.
    fn lookup(&self, name: &str) -> VfsResult<DirEntry>;

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
        name: &str,
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
        _name: &str,
        _target: &str,
        _permission: NodePermission,
        _user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    /// Creates an inode that is not installed in the directory namespace.
    ///
    /// Filesystems that cannot preserve same-inode link semantics should keep
    /// the default honest unsupported result.
    fn create_anonymous(&self, _options: &AnonymousOptions) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    /// Creates a link to a node. The filesystem must atomically reject a
    /// zero-link inode unless it still owns anonymous publication state.
    fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry>;

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
}

/// One backend-serialized unlink operation.
#[derive(Clone, Copy)]
pub struct UnlinkRequest<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    /// When present, the name must still denote this backend object. Backends
    /// must compare a stable object identity, not VFS dentry pointer identity
    /// or a reusable inode number alone.
    pub expected: Option<&'a DirEntry>,
}

/// One backend-serialized rename operation.
#[derive(Clone, Copy)]
pub struct RenameRequest<'a> {
    pub src_name: &'a str,
    pub src: &'a DirEntry,
    pub dst_dir: &'a DirNode,
    pub dst_name: &'a str,
    /// The destination observed by the caller, or `None` when it was absent.
    /// The backend must reject a changed snapshot before committing.
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
    cache_cleanup: Mutex<Option<IntoIter<String, CachedDirEntry>>>,
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

    fn forget_entry(children: &mut DirChildren, name: &str) {
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

    fn prepare_cache_name(&self, name: &str) -> Option<String> {
        if !self.ops.is_cacheable() {
            return None;
        }
        let mut cache_name = String::new();
        cache_name.try_reserve_exact(name.len()).ok()?;
        cache_name.push_str(name);
        Some(cache_name)
    }

    /// Invalidates one cached name before a backend namespace mutation.
    ///
    /// The returned generation may be used to publish the backend result only
    /// if no later mutation has started. The cache lock is deliberately
    /// released before the backend operation runs.
    fn begin_name_mutation(&self, name: &str) -> u64 {
        let mut children = self.cache.lock();
        Self::forget_entry(&mut children, name);
        self.bump_cache_generation()
    }

    fn cache_entry_if_current(
        &self,
        cache_name: Option<String>,
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
    pub fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
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
    pub fn lookup_cache(&self, name: &str) -> Option<DirEntry> {
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
    pub fn insert_cache(&self, name: String, entry: DirEntry) -> Option<DirEntry> {
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

    /// Creates a link to a node.
    pub fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let entry = self.ops.link(name, node)?;
        let backend_epoch = self.ops.namespace_epoch();
        Ok(self.cache_entry_if_current(cache_name, entry, generation, backend_epoch))
    }

    /// Unlinks a directory entry by name.
    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        self.unlink_inner(name, is_dir, None)
    }

    /// Unlinks `name` only if it still denotes `expected`.
    pub fn unlink_checked(&self, name: &str, is_dir: bool, expected: &DirEntry) -> VfsResult<()> {
        self.unlink_inner(name, is_dir, Some(expected))
    }

    fn unlink_inner(&self, name: &str, is_dir: bool, expected: Option<&DirEntry>) -> VfsResult<()> {
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
        self.read_dir(0, &mut |name: &str, _, _, _| {
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
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        verify_entry_name(name)?;
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
        name: &str,
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
            },
            CreateDisposition::Exclusive,
        )
        .map(|outcome| outcome.entry)
    }

    /// Creates a fully initialized symbolic link and then publishes it in the
    /// directory cache.
    pub fn create_symlink(
        &self,
        name: &str,
        target: &str,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        verify_entry_name(name)?;
        let cache_name = self.prepare_cache_name(name);
        let generation = self.begin_name_mutation(name);
        let entry = self.ops.create_symlink(name, target, permission, user)?;
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
        src_name: &str,
        src: &DirEntry,
        dst_dir: &Self,
        dst_name: &str,
        dst: Option<&DirEntry>,
    ) -> VfsResult<()> {
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

    /// Opens (or creates) a file in the directory.
    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<DirEntry> {
        self.open_file_with_status(name, options)
            .map(|(entry, _created)| entry)
    }

    /// Opens (or creates) a file and reports whether this call created it.
    ///
    /// The backend decides the status under its namespace serialization. The
    /// dentry cache is never used as the authority for an open-or-create race.
    pub fn open_file_with_status(
        &self,
        name: &str,
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
    // Backend-transaction behavior is exercised by concrete filesystem tests;
    // the generic VFS no longer performs pointer-identity validation itself.
}
