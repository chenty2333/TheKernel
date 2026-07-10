use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{any::Any, task::Context};

use axfs_ng_vfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, FilesystemOps,
    Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError,
    VfsResult, WeakDirEntry,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, Pollable};
use lwext4_rust::{FileAttr, InodeType};

use super::{
    Ext4Filesystem,
    util::{LwExt4Filesystem, into_vfs_err, into_vfs_type},
};

pub struct Inode {
    fs: Arc<Ext4Filesystem>,
    ino: u32,
    this: Option<WeakDirEntry>,
}

impl Inode {
    pub(crate) fn new(fs: Arc<Ext4Filesystem>, ino: u32, this: Option<WeakDirEntry>) -> Arc<Self> {
        Arc::new(Self { fs, ino, this })
    }

    fn create_entry(&self, entry: &lwext4_rust::DirEntry, name: impl Into<String>) -> DirEntry {
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.into(),
        );
        if entry.inode_type() == InodeType::Directory {
            DirEntry::new_dir(
                |this| DirNode::new(Inode::new(self.fs.clone(), entry.ino(), Some(this))),
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new(self.fs.clone(), entry.ino(), None)),
                into_vfs_type(entry.inode_type()),
                reference,
            )
        }
    }

    fn lookup_locked(&self, fs: &mut LwExt4Filesystem, name: &str) -> VfsResult<DirEntry> {
        let mut result = fs.lookup(self.ino, name).map_err(into_vfs_err)?;
        let entry = result.entry();
        Ok(self.create_entry(&entry, name))
    }

    fn update_ctime_locked(&self, fs: &mut LwExt4Filesystem, ino: u32) -> VfsResult<()> {
        fs.with_inode_ref(ino, |ino| {
            ino.update_ctime();
            Ok(())
        })
        .map_err(into_vfs_err)
    }
}

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        self.ino as _
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut attr = FileAttr::default();
        self.fs
            .lock()
            .get_attr(self.ino, &mut attr)
            .map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: self.ino as _,
            device: attr.device,
            nlink: attr.nlink,
            mode: NodePermission::from_bits_truncate(attr.mode as u16),
            node_type: into_vfs_type(attr.node_type),
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            block_size: attr.block_size,
            blocks: attr.blocks,
            rdev: DeviceId(attr.rdev),
            atime: attr.atime,
            btime: attr.btime,
            mtime: attr.mtime,
            ctime: attr.ctime,
        })
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let mut fs = self.fs.lock();
        fs.with_inode_ref(self.ino, |inode| {
            let mut status_changed = false;
            if let Some(mode) = update.mode {
                inode.set_mode((inode.mode() & !0xfff) | (mode.bits() as u32));
                status_changed = true;
            }
            if let Some((uid, gid)) = update.owner {
                inode.set_owner(uid as _, gid as _);
                status_changed = true;
            }
            if let Some(rdev) = update.rdev {
                inode.set_rdev(rdev.0);
                status_changed = true;
            }
            if let Some(atime) = update.atime {
                inode.set_atime(&atime);
            }
            if let Some(mtime) = update.mtime {
                inode.set_mtime(&mtime);
                status_changed = true;
            }
            if let Some(ctime) = update.ctime {
                inode.set_ctime(&ctime);
            } else if status_changed {
                inode.update_ctime();
            }
            Ok(())
        })
        .map_err(into_vfs_err)?;
        Ok(())
    }

    fn len(&self) -> VfsResult<u64> {
        self.fs
            .lock()
            .with_inode_ref(self.ino, |inode| Ok(inode.size()))
            .map_err(into_vfs_err)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, data_only: bool) -> VfsResult<()> {
        // lwext4 exposes writeback at the filesystem level. Reopen-after-sync
        // workloads require previous updates to be visible, so route inode sync through the
        // ext4-wide flush path.
        if data_only {
            crate::highlevel::record_file_sync_data_only_metadata_fallback();
        }
        self.fs.lock().flush().map_err(into_vfs_err)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
}

impl FileNodeOps for Inode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if {
            let fs = self.fs.lock();
            fs.is_block_aligned_range(offset, buf.len())
        } {
            self.fs
                .lock()
                .read_at_aligned_hot(self.ino, buf, offset)
                .map_err(into_vfs_err)
        } else {
            self.fs
                .lock()
                .read_at(self.ino, buf, offset)
                .map_err(into_vfs_err)
        }
    }

    fn read_at_vectored(&self, bufs: &mut [&mut [u8]], mut offset: u64) -> VfsResult<usize> {
        let len = bufs.iter().map(|buf| buf.len()).sum();
        if {
            let fs = self.fs.lock();
            fs.is_block_aligned_range(offset, len)
        } {
            let mut fs = self.fs.lock();
            if let Some(read) = fs
                .read_at_aligned_hot_vectored(self.ino, bufs, offset)
                .map_err(into_vfs_err)?
            {
                return Ok(read);
            }
        }

        let mut fs = self.fs.lock();
        let mut total = 0usize;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = fs.read_at(self.ino, buf, offset).map_err(into_vfs_err)?;
            total += read;
            offset = offset
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn try_read_at_vectored_async(
        &self,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let submission = {
            let mut fs = self.fs.lock();
            fs.read_at_aligned_hot_vectored_async_submit(self.ino, bufs, offset)
                .map_err(into_vfs_err)?
        };
        let Some(submission) = submission else {
            return Ok(None);
        };
        self.fs.wait_async_read(&submission)?;
        Ok(Some(submission.bytes))
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        if fs.is_block_aligned_range(offset, buf.len()) {
            fs.write_at_aligned_hot(self.ino, buf, offset)
        } else {
            fs.write_at(self.ino, buf, offset)
        }
        .map_err(into_vfs_err)
    }

    fn write_at_vectored(&self, bufs: &[&[u8]], mut offset: u64) -> VfsResult<usize> {
        let len = bufs.iter().map(|buf| buf.len()).sum();
        let mut fs = self.fs.lock();
        if fs.is_block_aligned_range(offset, len)
            && let Some(written) = fs
                .write_at_aligned_hot_vectored(self.ino, bufs, offset)
                .map_err(into_vfs_err)?
        {
            return Ok(written);
        }

        let mut total = 0usize;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = fs.write_at(self.ino, buf, offset).map_err(into_vfs_err)?;
            total += written;
            offset = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn try_write_at_vectored_async(&self, bufs: &[&[u8]], offset: u64) -> VfsResult<Option<usize>> {
        let submission = {
            let mut fs = self.fs.lock();
            fs.write_at_aligned_hot_vectored_async_submit(self.ino, bufs, offset)
                .map_err(into_vfs_err)?
        };
        let Some(submission) = submission else {
            return Ok(None);
        };
        self.fs.wait_async_write(&submission)?;
        Ok(Some(submission.bytes))
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let mut fs = self.fs.lock();
        let length = fs
            .with_inode_ref(self.ino, |inode| Ok(inode.size()))
            .map_err(into_vfs_err)?;
        let written = fs.write_at(self.ino, buf, length).map_err(into_vfs_err)?;
        Ok((written, length + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.fs.lock().set_len(self.ino, len).map_err(into_vfs_err)
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        self.fs
            .lock()
            .set_symlink(self.ino, target.as_bytes())
            .map_err(into_vfs_err)
    }
}

impl Pollable for Inode {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl DirNodeOps for Inode {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let mut reader = fs.read_dir(self.ino, offset).map_err(into_vfs_err)?;
        let mut count = 0;
        while let Some(entry) = reader.current() {
            let name = core::str::from_utf8(entry.name())
                .map_err(|_| VfsError::InvalidData)?
                .to_owned();
            let ino = entry.ino() as u64;
            let node_type = into_vfs_type(entry.inode_type());
            reader.step().map_err(into_vfs_err)?;
            if !sink.accept(&name, ino, node_type, reader.offset()) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let mut fs = self.fs.lock();
        self.lookup_locked(&mut fs, name)
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        let inode_type = match node_type {
            NodeType::Fifo => InodeType::Fifo,
            NodeType::CharacterDevice => InodeType::CharacterDevice,
            NodeType::Directory => InodeType::Directory,
            NodeType::BlockDevice => InodeType::BlockDevice,
            NodeType::RegularFile => InodeType::RegularFile,
            NodeType::Symlink => InodeType::Symlink,
            NodeType::Socket => InodeType::Socket,
            NodeType::Unknown => {
                return Err(VfsError::InvalidData);
            }
        };
        let mut fs = self.fs.lock();
        if fs.lookup(self.ino, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let ino = fs
            .create(self.ino, name, inode_type, permission.bits() as _)
            .map_err(into_vfs_err)?;
        let now = wall_time();
        fs.with_inode_ref(ino, |inode| {
            inode.set_atime(&now);
            inode.set_btime(&now);
            inode.set_mtime(&now);
            inode.update_ctime();
            Ok(())
        })
        .map_err(into_vfs_err)?;
        self.update_ctime_locked(&mut fs, ino)?;

        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.to_owned(),
        );
        Ok(if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| DirNode::new(Inode::new(self.fs.clone(), ino, Some(this))),
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(Inode::new(self.fs.clone(), ino, None)),
                node_type,
                reference,
            )
        })
    }

    fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
        let mut fs = self.fs.lock();
        fs.link(self.ino, name, node.inode() as _)
            .map_err(into_vfs_err)?;
        self.update_ctime_locked(&mut fs, node.inode() as _)?;
        self.lookup_locked(&mut fs, name)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        self.fs.lock().unlink(self.ino, name).map_err(into_vfs_err)
    }

    fn rename(&self, src_name: &str, dst_dir: &DirNode, dst_name: &str) -> VfsResult<()> {
        let dst_dir: Arc<Self> = dst_dir.downcast().map_err(|_| VfsError::InvalidInput)?;
        let mut fs = self.fs.lock();
        fs.rename(self.ino, src_name, dst_dir.ino, dst_name)
            .map_err(into_vfs_err)
    }
}
