use alloc::{sync::Arc, vec};
use core::{any::Any, mem, ops::Deref, task::Context};

use axfs_ng_vfs::{
    DirEntry, FileNode, FileNodeOps, FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps,
    NodeType, Reference, VfsError, VfsResult,
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use fatfs::{Read, Seek, SeekFrom, Write};

use super::{
    FsRef, ff,
    fs::{FatEntryIdentity, FatEntryState, FatFilesystem},
    util::{file_metadata, into_vfs_err, update_file_metadata},
};
use crate::fs::fat::fs::FatFilesystemInner;

pub struct FatFileNode {
    fs: Arc<FatFilesystem>,
    inner: FsRef<Option<ff::File<'static>>>,
    inode: u64,
    state: FatEntryState,
}

impl FatFileNode {
    pub(crate) fn try_new_pending(
        fs: Arc<FatFilesystem>,
        inode: u64,
        state: FatEntryState,
        reference: Reference,
    ) -> VfsResult<(DirEntry, Arc<Self>)> {
        let node = match Arc::try_new(Self {
            fs: fs.clone(),
            inner: FsRef::new(None),
            inode,
            state,
        }) {
            Ok(node) => node,
            Err(_) => {
                fs.release_inode(inode);
                return Err(VfsError::NoMemory);
            }
        };
        let entry = DirEntry::try_new_file(
            FileNode::new(node.clone()),
            NodeType::RegularFile,
            reference,
        )?;
        Ok((entry, node))
    }

    pub(crate) fn try_new_initialized(
        fs: Arc<FatFilesystem>,
        file: ff::File,
        inode: u64,
        state: FatEntryState,
        reference: Reference,
        fs_guard: &FatFilesystemInner,
    ) -> VfsResult<DirEntry> {
        let (entry, node) = Self::try_new_pending(fs, inode, state, reference)?;
        node.install_inner(fs_guard, file);
        Ok(entry)
    }

    pub(crate) fn install_inner(&self, fs: &FatFilesystemInner, file: ff::File) {
        // SAFETY: FsRef ties the backend handle to the filesystem guard which
        // owns the actual fatfs object for at least as long as this node.
        *self.inner.borrow_mut(fs) =
            Some(unsafe { mem::transmute::<ff::File<'_>, ff::File<'static>>(file) });
    }

    fn inner<'a>(&self, fs: &'a FatFilesystemInner) -> VfsResult<&'a mut ff::File<'static>> {
        self.inner.borrow_mut(fs).as_mut().ok_or(VfsError::Io)
    }

    pub(crate) fn matches_identity(
        &self,
        fs: &Arc<FatFilesystem>,
        identity: FatEntryIdentity,
    ) -> bool {
        Arc::ptr_eq(&self.fs, fs) && self.state.identity() == identity
    }
}

fn regular_file_size(file: &ff::File<'_>) -> VfsResult<u64> {
    file.size().map(u64::from).ok_or(VfsError::Io)
}

fn grow_file(fs: &FatFilesystemInner, file: &mut ff::File<'static>, len: u64) -> VfsResult<()> {
    // rust-fatfs does not support growing files directly. We need to
    // pad with zeros manually.
    let mut pos = file.seek(SeekFrom::End(0)).map_err(into_vfs_err)?;
    let block_size = fs.inner.bytes_per_sector() as usize;
    let block = vec![0; block_size];

    while pos < len {
        let write = (block_size - (pos as usize & (block_size - 1))).min((len - pos) as usize);
        file.write(&block[0..write]).map_err(into_vfs_err)?;
        pos += write as u64;
    }
    Ok(())
}

unsafe impl Send for FatFileNode {}

unsafe impl Sync for FatFileNode {}

impl NodeOps for FatFileNode {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        file_metadata(&fs, file, self.inode, NodeType::RegularFile)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        // FatFS has no ownership & permission

        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        update_file_metadata(file, update)
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.deref()
    }

    fn len(&self) -> VfsResult<u64> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        regular_file_size(file)
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        self.fs.flush()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
}

impl FileNodeOps for FatFileNode {
    fn read_at(&self, mut buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        file.seek(SeekFrom::Start(offset)).map_err(into_vfs_err)?;

        let mut read = 0;
        loop {
            let n = file.read(buf).map_err(into_vfs_err)?;
            if n == 0 {
                return Ok(read);
            }
            read += n;
            buf = &mut buf[n..];
        }
    }

    fn write_at(&self, mut buf: &[u8], offset: u64) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        if offset > regular_file_size(file)? {
            grow_file(&fs, file, offset)?;
        }
        file.seek(SeekFrom::Start(offset)).map_err(into_vfs_err)?;

        let mut written = 0;
        loop {
            let n = file.write(buf).map_err(into_vfs_err)?;
            if n == 0 {
                return Ok(written);
            }
            written += n;
            buf = &buf[n..];
        }
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        file.seek(SeekFrom::End(0)).map_err(into_vfs_err)?;
        let written = file.write(buf).map_err(into_vfs_err)?;
        Ok((written, regular_file_size(file)?))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let fs = self.fs.lock();
        let file = self.inner(&fs)?;
        if len <= regular_file_size(file)? {
            file.seek(SeekFrom::Start(len)).map_err(into_vfs_err)?;
            file.truncate().map_err(into_vfs_err)
        } else {
            grow_file(&fs, file, len)
        }
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }
}

impl Pollable for FatFileNode {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl Drop for FatFileNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.inode);
    }
}
