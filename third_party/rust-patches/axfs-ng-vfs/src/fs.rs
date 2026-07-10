use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{DirEntry, MetadataUpdateCapabilities, VfsResult};

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

    /// Returns which inode metadata fields this filesystem can persist.
    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        MetadataUpdateCapabilities::ALL
    }

    /// Flushes the filesystem, ensuring all data is written to disk
    fn flush(&self) -> VfsResult<()> {
        Ok(())
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
    device: u64,
    lifetime: Arc<()>,
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

    pub fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        self.inner.ops.metadata_update_capabilities()
    }

    pub fn flush(&self) -> VfsResult<()> {
        self.inner.ops.flush()
    }

    pub fn new(ops: Arc<dyn FilesystemOps>) -> Self {
        static DEVICE_COUNTER: AtomicU64 = AtomicU64::new(1);

        Self::new_with_device(ops, DEVICE_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Creates a filesystem with an existing stable device identity.
    ///
    /// This is used by views such as bind mounts that expose the same
    /// filesystem through a different root directory.
    pub fn new_with_device(ops: Arc<dyn FilesystemOps>, device: u64) -> Self {
        Self {
            inner: Arc::new(FilesystemInner {
                ops,
                device,
                lifetime: Arc::new(()),
            }),
        }
    }

    /// Returns the stable identity shared by all mounts of this filesystem.
    pub fn device(&self) -> u64 {
        self.inner.device
    }

    /// Returns a weak handle that remains live while this filesystem instance
    /// is owned by a mount or an unattached filesystem handle.
    pub fn lifetime_handle(&self) -> Weak<()> {
        Arc::downgrade(&self.inner.lifetime)
    }
}

impl core::fmt::Debug for Filesystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Filesystem")
            .field("name", &self.name())
            .field("device", &self.inner.device)
            .finish_non_exhaustive()
    }
}
