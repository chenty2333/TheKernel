use alloc::{string::String, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
};

use axfs_ng_vfs::{
    DeviceId, DirEntry, DirNode, Filesystem, FilesystemOps, Metadata, MetadataUpdate, NodeOps,
    NodePermission, NodeType, Reference, StatFs, VfsResult, path::MAX_NAME_LEN,
};
use axhal::time::wall_time;
use axsync::Mutex;
use slab::Slab;

use super::DirMaker;

/// Returns statistics for a metadata-only pseudo filesystem.
pub fn pseudo_stat_fs(fs_type: u32) -> StatFs {
    StatFs {
        fs_type,
        block_size: 4096,
        blocks: 0,
        blocks_free: 0,
        blocks_available: 0,

        file_count: 0,
        free_file_count: 0,

        name_length: MAX_NAME_LEN as _,
        fragment_size: 4096,
        mount_flags: 0,
    }
}

/// A simple filesystem implementation that uses a slab allocator for inodes.
pub struct SimpleFs {
    name: String,
    fs_type: u32,
    inodes: Mutex<Slab<()>>,
    next_ephemeral_inode: AtomicU64,
    root: Mutex<Option<DirEntry>>,
}

impl SimpleFs {
    /// Creates a new simple filesystem.
    pub fn new_with(
        name: String,
        fs_type: u32,
        root: impl FnOnce(Arc<Self>) -> DirMaker,
    ) -> Filesystem {
        let fs = Arc::new(Self {
            name,
            fs_type,
            inodes: Mutex::new(Slab::new()),
            // Keep fallibly constructed, non-cacheable nodes in a disjoint
            // inode range without growing the tracking slab.
            next_ephemeral_inode: AtomicU64::new(1 << 63),
            root: Mutex::new(None),
        });
        let root = root(fs.clone());
        fs.set_root(DirEntry::new_dir(
            |this| DirNode::new(root(this)),
            Reference::root(),
        ));
        Filesystem::new(fs)
    }

    fn set_root(&self, root: DirEntry) {
        *self.root.lock() = Some(root);
    }

    fn alloc_inode(&self) -> u64 {
        self.inodes.lock().insert(()) as u64 + 1
    }

    fn try_alloc_ephemeral_inode(&self) -> VfsResult<u64> {
        self.next_ephemeral_inode
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| axfs_ng_vfs::VfsError::StorageFull)
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(ino as usize - 1);
    }
}

impl FilesystemOps for SimpleFs {
    fn name(&self) -> &str {
        &self.name
    }

    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(pseudo_stat_fs(self.fs_type))
    }

    fn unmount(&self) {
        self.root.lock().take();
    }
}

/// Filesystem node for [`SimpleFs`].
pub struct SimpleFsNode {
    fs: Arc<SimpleFs>,
    ino: u64,
    tracked_inode: bool,
    pub(crate) metadata: Mutex<Metadata>,
}

impl SimpleFsNode {
    /// Creates a new filesystem node.
    pub fn new(fs: Arc<SimpleFs>, node_type: NodeType, mode: NodePermission) -> Self {
        let ino = fs.alloc_inode();
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 1,
            mode,
            node_type,
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now,
            btime: now,
            mtime: now,
            ctime: now,
        };
        Self {
            fs,
            ino,
            tracked_inode: true,
            metadata: Mutex::new(metadata),
        }
    }

    /// Creates a userspace-triggered pseudo node without an abort-on-OOM slab
    /// growth. The reserved inode is released if a later fallible constructor
    /// step drops this node before publication.
    pub fn try_new(
        fs: Arc<SimpleFs>,
        node_type: NodeType,
        mode: NodePermission,
    ) -> VfsResult<Self> {
        let ino = fs.try_alloc_ephemeral_inode()?;
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 1,
            mode,
            node_type,
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now,
            btime: now,
            mtime: now,
            ctime: now,
        };
        Ok(Self {
            fs,
            ino,
            tracked_inode: false,
            metadata: Mutex::new(metadata),
        })
    }
}

impl Drop for SimpleFsNode {
    fn drop(&mut self) {
        if self.tracked_inode {
            self.fs.release_inode(self.ino);
        }
    }
}

impl NodeOps for SimpleFsNode {
    fn inode(&self) -> u64 {
        self.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.metadata.lock().clone();
        metadata.size = self.len()?;
        Ok(metadata)
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(0)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let mut metadata = self.metadata.lock();
        let mut status_changed = false;
        if let Some(mode) = update.mode {
            metadata.mode = mode;
            status_changed = true;
        }
        if let Some((uid, gid)) = update.owner {
            metadata.uid = uid;
            metadata.gid = gid;
            status_changed = true;
        }
        if let Some(rdev) = update.rdev {
            metadata.rdev = rdev;
            status_changed = true;
        }
        if let Some(atime) = update.atime {
            metadata.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            metadata.mtime = mtime;
            status_changed = true;
        }
        if let Some(ctime) = update.ctime {
            metadata.ctime = ctime;
        } else if status_changed {
            metadata.ctime = wall_time();
        }
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_nodes_use_unique_allocation_free_inode_range() {
        let fs = Arc::new(SimpleFs {
            name: String::new(),
            fs_type: 0,
            inodes: Mutex::new(Slab::new()),
            next_ephemeral_inode: AtomicU64::new(1 << 63),
            root: Mutex::new(None),
        });

        let first = SimpleFsNode::try_new(fs.clone(), NodeType::Symlink, NodePermission::default())
            .unwrap();
        let second =
            SimpleFsNode::try_new(fs, NodeType::Symlink, NodePermission::default()).unwrap();

        assert_eq!(first.inode(), 1 << 63);
        assert_eq!(second.inode(), (1 << 63) + 1);
    }
}
