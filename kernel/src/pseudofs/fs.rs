use alloc::{string::String, sync::Arc};
use core::any::Any;

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
            metadata: Mutex::new(metadata),
        }
    }
}

impl Drop for SimpleFsNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.ino);
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
