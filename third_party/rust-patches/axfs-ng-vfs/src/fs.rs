use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Once;

use crate::{
    DirEntry, Metadata, MetadataUpdateCapabilities, Mutex, VfsError, VfsResult, WeakDirEntry,
    WritebackErrorState,
};

/// Callback used by [`FilesystemOps::enumerate_inodes`].
///
/// The callback receives a point-in-time metadata snapshot for every inode
/// which is still live in the filesystem, including an unlinked inode held
/// open by a file descriptor.  Filesystems which cannot make that guarantee
/// must return [`VfsError::OperationNotSupported`] rather than walking their
/// visible directory tree and claiming that it is complete.
pub type InodeVisitor<'a> = dyn FnMut(Metadata) -> VfsResult<()> + 'a;
/// Filesystem-owned opaque identity for one live inode generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportHandle {
    pub inode: u64,
    pub generation: u64,
}

pub struct StatFs {
    pub fs_type: u32,
    pub block_size: u32,
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,

    pub file_count: u64,
    pub free_file_count: u64,

    pub name_length: u32,
    pub fragment_size: u32,
    pub mount_flags: u32,
}

/// Trait for filesystem operations
pub trait FilesystemOps: Send + Sync {
    /// Gets the name of the filesystem
    fn name(&self) -> &str;

    /// Gets the root directory entry of the filesystem
    fn root_dir(&self) -> DirEntry;

    /// Returns statistics about the filesystem
    fn stat(&self) -> VfsResult<StatFs>;

    /// Enumerates all live inode identities in this filesystem.
    ///
    /// This is deliberately a visitor, rather than a returned collection: an
    /// ext4 image can contain far more allocated inodes than fit in a
    /// temporary VFS allocation.  The default is fail-closed because a
    /// namespace walk misses unlinked-but-open files and hard-link aliases.
    fn enumerate_inodes(&self, _visitor: &mut InodeVisitor<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    /// Exports a backend-validated inode generation.  The VFS deliberately
    /// does not synthesize a handle from a pathname or a bare inode number.
    fn encode_export_handle(&self, _entry: &DirEntry) -> VfsResult<ExportHandle> {
        Err(crate::VfsError::OperationNotSupported)
    }

    /// Resolves a previously exported live inode generation.  `NotFound`
    /// means stale; other errors preserve backend failure information.
    fn decode_export_handle(&self, _handle: ExportHandle) -> VfsResult<DirEntry> {
        Err(crate::VfsError::OperationNotSupported)
    }

    /// Tests whether an exported inode is reachable through `ancestor` in the
    /// live namespace.  Export decoding may deliberately return an anonymous
    /// reference, so callers must not infer ancestry from that decoded alias.
    fn export_handle_is_descendant(
        &self,
        _ancestor: &DirEntry,
        _handle: ExportHandle,
    ) -> VfsResult<bool> {
        Ok(false)
    }

    /// Returns which inode metadata fields this filesystem can persist.
    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        MetadataUpdateCapabilities::ALL
    }

    /// Flushes the filesystem, ensuring all data is written to disk
    fn flush(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Superblock-scoped asynchronous writeback errors observed by syncfs.
    /// Backends without asynchronous writeback need not allocate this state.
    fn syncfs_writeback_error_state(&self) -> Option<Arc<WritebackErrorState>> {
        None
    }

    /// Breaks filesystem-owned root references after a mount is detached.
    ///
    /// Open nodes may continue to keep the filesystem alive. Implementations
    /// should only release self-references here; resource ownership remains tied
    /// to the final filesystem object drop.
    fn unmount(&self) {}
}

struct FilesystemInner {
    ops: Arc<dyn FilesystemOps>,
    identity: FilesystemIdentity,
    root_cache_owner: Arc<RootCacheOwner>,
    // Linux keeps errseq state at superblock scope for syncfs.  This is
    // deliberately distinct from the inode state carried by DirEntry.
    writeback_errors: Arc<WritebackErrorState>,
}

struct RootCacheOwner {
    root: Mutex<Option<WeakDirEntry>>,
}

impl RootCacheOwner {
    fn new() -> Self {
        Self {
            root: Mutex::new(None),
        }
    }
}

impl Drop for RootCacheOwner {
    fn drop(&mut self) {
        let root = self.root.lock().take().and_then(|root| root.upgrade());
        if let Some(root) = root {
            crate::node::defer_dentry_cache_cleanup(root);
        }
    }
}

#[derive(Debug)]
struct FilesystemIdentityInner {
    device: u64,
}

static FILESYSTEM_RELEASE_HOOK: Once<fn(u64)> = Once::new();
static FILESYSTEM_DEVICE_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Drop for FilesystemIdentityInner {
    fn drop(&mut self) {
        if let Some(hook) = FILESYSTEM_RELEASE_HOOK.get() {
            hook(self.device);
        }
    }
}

/// Installs a callback invoked synchronously when the final strong filesystem
/// identity is released.
///
/// The identity may be dropped from an arbitrary context. The callback must
/// therefore perform only bounded, non-sleeping, allocation-free work and
/// defer policy processing to a safe execution context.
pub fn set_filesystem_release_hook(hook: fn(u64)) {
    FILESYSTEM_RELEASE_HOOK.call_once(|| hook);
}

#[derive(Clone, Debug)]
pub struct FilesystemIdentity(Arc<FilesystemIdentityInner>);

impl FilesystemIdentity {
    pub fn device(&self) -> u64 {
        self.0.device
    }

    pub fn downgrade(&self) -> WeakFilesystemIdentity {
        WeakFilesystemIdentity(Arc::downgrade(&self.0))
    }
}

#[derive(Clone, Debug)]
pub struct WeakFilesystemIdentity(Weak<FilesystemIdentityInner>);

impl WeakFilesystemIdentity {
    pub fn upgrade(&self) -> Option<FilesystemIdentity> {
        self.0.upgrade().map(FilesystemIdentity)
    }
}

impl Drop for FilesystemInner {
    fn drop(&mut self) {
        self.ops.unmount();
    }
}

#[derive(Clone)]
pub struct Filesystem {
    inner: Arc<FilesystemInner>,
}

impl Filesystem {
    pub fn name(&self) -> &str {
        self.inner.ops.name()
    }

    pub fn root_dir(&self) -> DirEntry {
        self.inner.ops.root_dir()
    }

    pub fn stat(&self) -> VfsResult<StatFs> {
        self.inner.ops.stat()
    }

<<<<<<< HEAD
    /// Enumerates every live inode supplied by the backend.
    pub fn enumerate_inodes(&self, visitor: &mut InodeVisitor<'_>) -> VfsResult<()> {
        self.inner.ops.enumerate_inodes(visitor)
=======
    pub fn encode_export_handle(&self, entry: &DirEntry) -> VfsResult<ExportHandle> {
        self.inner.ops.encode_export_handle(entry)
    }

    pub fn decode_export_handle(&self, handle: ExportHandle) -> VfsResult<DirEntry> {
        self.inner.ops.decode_export_handle(handle)
>>>>>>> 955e94c8 (feat(vfs): add generation-safe export handles)
    }

    pub fn export_handle_is_descendant(
        &self,
        ancestor: &DirEntry,
        handle: ExportHandle,
    ) -> VfsResult<bool> {
        self.inner.ops.export_handle_is_descendant(ancestor, handle)
    }

    pub fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        self.inner.ops.metadata_update_capabilities()
    }

    pub fn flush(&self) -> VfsResult<()> {
        self.inner.ops.flush()
    }

    pub fn writeback_error_state(&self) -> Arc<WritebackErrorState> {
        self.inner
            .ops
            .syncfs_writeback_error_state()
            .unwrap_or_else(|| self.inner.writeback_errors.clone())
    }

    pub fn new(ops: Arc<dyn FilesystemOps>) -> Self {
        Self::new_with_identity_inner(
            ops,
            FilesystemIdentity(Arc::new(FilesystemIdentityInner {
                device: FILESYSTEM_DEVICE_COUNTER.fetch_add(1, Ordering::Relaxed),
            })),
            Arc::new(RootCacheOwner::new()),
        )
    }

    /// Fallibly constructs a filesystem wrapper for backends whose mount path
    /// must report allocation failure instead of aborting the kernel.
    pub fn try_new(ops: Arc<dyn FilesystemOps>) -> VfsResult<Self> {
        let identity = Arc::try_new(FilesystemIdentityInner {
            device: FILESYSTEM_DEVICE_COUNTER.fetch_add(1, Ordering::Relaxed),
        })
        .map(FilesystemIdentity)
        .map_err(|_| crate::VfsError::NoMemory)?;
        let root_cache_owner =
            Arc::try_new(RootCacheOwner::new()).map_err(|_| crate::VfsError::NoMemory)?;
        let inner = Arc::try_new(FilesystemInner {
            ops,
            identity,
            root_cache_owner,
            writeback_errors: Arc::try_new(WritebackErrorState::default())
                .map_err(|_| crate::VfsError::NoMemory)?,
        })
        .map_err(|_| crate::VfsError::NoMemory)?;
        Ok(Self { inner })
    }

    /// Creates an independent filesystem tree with an existing stable identity.
    ///
    /// Sharing the identity keeps device mappings live, but does not couple the
    /// two root-dentry cache lifetimes. Use [`Self::new_view`] when `ops`
    /// exposes a view of an existing filesystem tree.
    pub fn new_with_identity(ops: Arc<dyn FilesystemOps>, identity: FilesystemIdentity) -> Self {
        Self::new_with_identity_inner(ops, identity, Arc::new(RootCacheOwner::new()))
    }

    /// Creates a filesystem view that shares the source tree's cache lifetime.
    pub fn new_view(ops: Arc<dyn FilesystemOps>, source: &Filesystem) -> Self {
        Self::new_with_identity_inner(
            ops,
            source.identity(),
            Arc::clone(&source.inner.root_cache_owner),
        )
    }

    /// Fallibly creates a filesystem view that shares the source tree's cache
    /// lifetime. Runtime mount paths should use this variant so allocation
    /// failure is reported before publishing any mount topology.
    pub fn try_new_view(ops: Arc<dyn FilesystemOps>, source: &Filesystem) -> VfsResult<Self> {
        let inner = Arc::try_new(FilesystemInner {
            ops,
            identity: source.identity(),
            root_cache_owner: Arc::clone(&source.inner.root_cache_owner),
            writeback_errors: source.writeback_error_state(),
        })
        .map_err(|_| crate::VfsError::NoMemory)?;
        Ok(Self { inner })
    }

    fn new_with_identity_inner(
        ops: Arc<dyn FilesystemOps>,
        identity: FilesystemIdentity,
        root_cache_owner: Arc<RootCacheOwner>,
    ) -> Self {
        Self {
            inner: Arc::new(FilesystemInner {
                ops,
                identity,
                root_cache_owner,
                writeback_errors: Arc::new(WritebackErrorState::default()),
            }),
        }
    }

    pub(crate) fn retain_mount_root(&self, root: &DirEntry) {
        let mut owner_root = self.inner.root_cache_owner.root.lock();
        if owner_root
            .as_ref()
            .is_none_or(|root| root.upgrade().is_none())
        {
            *owner_root = Some(root.downgrade());
        }
    }

    /// Returns the stable identity shared by all mounts of this filesystem.
    pub fn device(&self) -> u64 {
        self.inner.identity.device()
    }

    pub fn identity(&self) -> FilesystemIdentity {
        self.inner.identity.clone()
    }

    pub fn identity_weak(&self) -> WeakFilesystemIdentity {
        self.inner.identity.downgrade()
    }
}

impl core::fmt::Debug for Filesystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Filesystem")
            .field("name", &self.name())
            .field("device", &self.inner.identity.device())
            .finish_non_exhaustive()
    }
}
