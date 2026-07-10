use alloc::sync::Arc;
use core::{marker::PhantomPinned, time::Duration};

use axdriver::{SharedBlockDevice, prelude::BlockDriverOps};
use axfs_ng_vfs::{
    DirEntry, Filesystem, FilesystemOps, MetadataUpdateCapabilities, Reference, StatFs, VfsError,
    VfsResult,
    path::MAX_NAME_LEN,
};
use kspin::{SpinNoPreempt as Mutex, SpinNoPreemptGuard as MutexGuard};
use slab::Slab;

use super::{dir::FatDirNode, disk::SeekableDisk, ff, util::into_vfs_err};
use crate::{FatMountOptions, MountedBlockDevice};

pub struct FatFilesystemInner {
    pub inner: ff::FileSystem,
    pub mount_options: FatMountOptions,
    pub root_atime: Duration,
    pub root_mtime: Duration,
    inode_allocator: Slab<()>,
    _pinned: PhantomPinned,
}

impl FatFilesystemInner {
    pub(crate) fn alloc_inode(&mut self) -> u64 {
        self.inode_allocator.insert(()) as u64 + 1
    }

    pub(crate) fn release_inode(&mut self, ino: u64) {
        self.inode_allocator.remove(ino as usize - 1);
    }
}

pub struct FatFilesystem {
    inner: Mutex<FatFilesystemInner>,
    root_dir: Mutex<Option<DirEntry>>,
    device: SharedBlockDevice,
}

impl FatFilesystem {
    pub fn new(dev: MountedBlockDevice) -> VfsResult<Filesystem> {
        Self::new_with_options(dev, FatMountOptions::default())
    }

    pub fn new_with_options(
        dev: MountedBlockDevice,
        mount_options: FatMountOptions,
    ) -> VfsResult<Filesystem> {
        let device = dev.device().clone();
        let mut inner = FatFilesystemInner {
            inner: ff::FileSystem::new(SeekableDisk::new(dev), fatfs::FsOptions::new())
                .map_err(into_vfs_err)?,
            mount_options,
            root_atime: Duration::ZERO,
            root_mtime: Duration::ZERO,
            inode_allocator: Slab::new(),
            _pinned: PhantomPinned,
        };
        let root_inode = inner.alloc_inode();
        let result = Arc::new(Self {
            inner: Mutex::new(inner),
            root_dir: Mutex::default(),
            device,
        });

        let root_dir = DirEntry::new_dir(
            |this| {
                FatDirNode::new(
                    result.clone(),
                    result.lock().inner.root_dir(),
                    root_inode,
                    this,
                )
            },
            Reference::root(),
        );
        *result.root_dir.lock() = Some(root_dir);
        Ok(Filesystem::new(result))
    }
}

impl FatFilesystem {
    pub(crate) fn lock(&self) -> MutexGuard<'_, FatFilesystemInner> {
        self.inner.lock()
    }
}

impl FilesystemOps for FatFilesystem {
    fn name(&self) -> &str {
        "vfat"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_dir.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        let fs = self.inner.lock();
        let stats = fs.inner.stats().map_err(into_vfs_err)?;
        let cluster_size = stats.cluster_size() as u32;
        Ok(StatFs {
            fs_type: 0x4d44,
            block_size: cluster_size,
            blocks: stats.total_clusters() as _,
            blocks_free: stats.free_clusters() as _,
            blocks_available: stats.free_clusters() as _,

            file_count: 0,
            free_file_count: 0,

            name_length: MAX_NAME_LEN as _,
            fragment_size: cluster_size,
            mount_flags: 0,
        })
    }

    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        MetadataUpdateCapabilities::ATIME | MetadataUpdateCapabilities::MTIME
    }

    fn flush(&self) -> VfsResult<()> {
        crate::highlevel::sync_cached_file_pages_for_filesystem(self)?;
        self.inner.lock().inner.flush().map_err(into_vfs_err)?;
        self.device.lock().flush().map_err(|_| VfsError::Io)
    }

    fn unmount(&self) {
        self.root_dir.lock().take();
    }
}
