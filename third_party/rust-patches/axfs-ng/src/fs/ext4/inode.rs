use alloc::{string::String, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FileNode, FileNodeOps, FilesystemOps, Metadata, MetadataUpdate, NamedCreateOptions, NodeFlags,
    NodeOps, NodePermission, NodeType, NodeUserData, Reference, RenameRequest, UnlinkRequest,
    VfsError, VfsResult, WeakDirEntry, XattrProvider, XattrSetMode,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use lwext4_rust::{
    Ext4Error, FileAttr, InodeToken, InodeType,
    ffi::{EEXIST, ENODATA, ENOENT},
};
use spin::Once;

use super::{
    Ext4Filesystem, RuntimeReservation,
    util::{into_vfs_err, into_vfs_type},
};

fn combine_vfs_cleanup<T>(operation: VfsResult<T>, cleanup: VfsResult<()>) -> VfsResult<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(cleanup_err)) => {
            log::error!("secondary ext4 VFS cleanup failure: {cleanup_err}");
            Err(err)
        }
    }
}

fn try_owned(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    Ok(result)
}

fn admit_xattr_set_mode(mode: XattrSetMode, exists: bool) -> lwext4_rust::Ext4Result<()> {
    match (mode, exists) {
        (XattrSetMode::Create, true) => Err(Ext4Error::new(EEXIST as _, "xattr already exists")),
        (XattrSetMode::Replace, false) => Err(Ext4Error::new(ENODATA as _, "xattr does not exist")),
        _ => Ok(()),
    }
}

const fn ext4_named_create_inode_type(node_type: NodeType) -> Option<InodeType> {
    match node_type {
        NodeType::Fifo => Some(InodeType::Fifo),
        NodeType::CharacterDevice => Some(InodeType::CharacterDevice),
        NodeType::Directory => Some(InodeType::Directory),
        NodeType::BlockDevice => Some(InodeType::BlockDevice),
        NodeType::RegularFile => Some(InodeType::RegularFile),
        NodeType::Socket => Some(InodeType::Socket),
        NodeType::Symlink | NodeType::Unknown => None,
    }
}

struct InodeBinding {
    token: InodeToken,
    namespace_epoch: Arc<AtomicU64>,
}

pub(crate) struct PreparedInodeEntry {
    inode: Arc<Inode>,
    entry: DirEntry,
    runtime_reservation: Option<(RuntimeReservation, Arc<NodeUserData>)>,
}

impl PreparedInodeEntry {
    pub(crate) fn install_initial_data(&mut self, options: &NamedCreateOptions) -> VfsResult<()> {
        let Some(initial_data) = options.initial_data.clone() else {
            return Ok(());
        };
        let runtime = Arc::try_new(NodeUserData::new()).map_err(|_| VfsError::NoMemory)?;
        runtime.install_initial_data(initial_data)?;
        let reservation = self.inode.fs.reserve_runtime_attachment()?;
        self.runtime_reservation = Some((reservation, runtime));
        Ok(())
    }

    pub(crate) fn bind(mut self, token: InodeToken, namespace_epoch: Arc<AtomicU64>) -> DirEntry {
        if let Some((reservation, runtime)) = self.runtime_reservation.take() {
            let runtime = reservation.commit(token, runtime);
            self.inode.attach_runtime(runtime);
        }
        self.inode.bind(token, namespace_epoch);
        self.entry
    }
}

pub struct Inode {
    fs: Arc<Ext4Filesystem>,
    binding: Once<InodeBinding>,
    this: Once<WeakDirEntry>,
    runtime: Once<Arc<NodeUserData>>,
}

impl Inode {
    pub(crate) fn try_prepare_entry(
        fs: Arc<Ext4Filesystem>,
        inode_type: InodeType,
        reference: Reference,
    ) -> VfsResult<PreparedInodeEntry> {
        let inode = Arc::try_new(Self {
            fs,
            binding: Once::new(),
            this: Once::new(),
            runtime: Once::new(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let entry = if inode_type == InodeType::Directory {
            let entry = DirEntry::try_new_dir(DirNode::new(inode.clone()), reference)?;
            inode.this.call_once(|| entry.downgrade());
            entry
        } else {
            DirEntry::try_new_file(
                FileNode::new(inode.clone()),
                into_vfs_type(inode_type),
                reference,
            )?
        };
        Ok(PreparedInodeEntry {
            inode,
            entry,
            runtime_reservation: None,
        })
    }

    fn bind(&self, token: InodeToken, namespace_epoch: Arc<AtomicU64>) {
        self.binding.call_once(|| InodeBinding {
            token,
            namespace_epoch,
        });
    }

    fn attach_runtime(&self, runtime: Arc<NodeUserData>) {
        let installed = self.runtime.call_once(|| runtime.clone());
        debug_assert!(Arc::ptr_eq(installed, &runtime));
    }

    fn token(&self) -> Option<InodeToken> {
        self.binding.get().map(|binding| binding.token)
    }

    fn ino(&self) -> u32 {
        self.token().map_or(0, InodeToken::ino)
    }

    fn try_prepare_child(
        &self,
        inode_type: InodeType,
        name: String,
    ) -> VfsResult<PreparedInodeEntry> {
        Self::try_prepare_entry(
            self.fs.clone(),
            inode_type,
            Reference::new(self.this.get().and_then(WeakDirEntry::upgrade), name),
        )
    }

    /// Completes a retained low-level inode identity outside the ext4 spin
    /// lock. If VFS allocation fails, the retained handle is released before
    /// returning to the caller.
    fn try_finish_retained_entry(
        &self,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        inode_type: InodeType,
        name: String,
    ) -> VfsResult<DirEntry> {
        match self.try_prepare_child(inode_type, name) {
            Ok(prepared) => {
                if let Some(runtime) = self.fs.runtime_attachment(token) {
                    prepared.inode.attach_runtime(runtime);
                }
                Ok(prepared.bind(token, namespace_epoch))
            }
            Err(err) => {
                self.fs.lock().release_inode_handle(token);
                Err(err)
            }
        }
    }

    fn try_finish_retained_name(
        &self,
        token: InodeToken,
        namespace_epoch: Arc<AtomicU64>,
        inode_type: InodeType,
        name: &str,
    ) -> VfsResult<DirEntry> {
        match try_owned(name) {
            Ok(name) => self.try_finish_retained_entry(token, namespace_epoch, inode_type, name),
            Err(err) => {
                self.fs.lock().release_inode_handle(token);
                Err(err)
            }
        }
    }

    fn bump_namespace_epoch(&self) {
        if let Some(binding) = self.binding.get() {
            binding.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn namespace_epoch_handle(&self) -> Option<&Arc<AtomicU64>> {
        self.binding.get().map(|binding| &binding.namespace_epoch)
    }

    fn expected_token(&self, expected: &DirEntry) -> Option<InodeToken> {
        expected
            .downcast::<Self>()
            .ok()
            .and_then(|expected| Arc::ptr_eq(&self.fs, &expected.fs).then(|| expected.token()))
            .flatten()
    }
}

impl NodeOps for Inode {
    fn inode(&self) -> u64 {
        self.ino() as _
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut attr = FileAttr::default();
        self.fs
            .lock()
            .get_attr(self.ino(), &mut attr)
            .map_err(into_vfs_err)?;
        Ok(Metadata {
            inode: self.ino() as _,
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
        fs.with_inode_ref_mut(self.ino(), |inode| {
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
            .with_inode_ref(self.ino(), |inode| Ok(inode.size()))
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

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        self.runtime.get().map(Arc::as_ref)
    }

    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
}

impl XattrProvider for Inode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<alloc::vec::Vec<u8>> {
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| inode.get_xattr(name))
            .map_err(into_vfs_err)
    }

    fn list_xattrs(&self) -> VfsResult<alloc::vec::Vec<u8>> {
        self.fs
            .lock()
            .with_inode_ref(self.ino(), |inode| inode.list_xattrs())
            .map_err(into_vfs_err)
    }

    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| {
                let exists = inode.has_xattr(name)?;
                admit_xattr_set_mode(mode, exists)?;
                inode.set_xattr(name, value)
            })
            .map_err(into_vfs_err)
    }

    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        self.fs
            .lock()
            .with_inode_ref_mut(self.ino(), |inode| inode.remove_xattr(name))
            .map_err(into_vfs_err)
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        if let Some(binding) = self.binding.get() {
            self.fs.lock().release_inode_handle(binding.token);
        }
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
                .read_at_aligned_hot(self.ino(), buf, offset)
                .map_err(into_vfs_err)
        } else {
            self.fs
                .lock()
                .read_at(self.ino(), buf, offset)
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
                .read_at_aligned_hot_vectored(self.ino(), bufs, offset)
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
            let read = match fs.read_at(self.ino(), buf, offset).map_err(into_vfs_err) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
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
            fs.read_at_aligned_hot_vectored_async_submit(self.ino(), bufs, offset)
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
            fs.write_at_aligned_hot(self.ino(), buf, offset)
        } else {
            fs.write_at(self.ino(), buf, offset)
        }
        .map_err(into_vfs_err)
    }

    fn write_at_vectored(&self, bufs: &[&[u8]], mut offset: u64) -> VfsResult<usize> {
        let len = bufs.iter().map(|buf| buf.len()).sum();
        let mut fs = self.fs.lock();
        if fs.is_block_aligned_range(offset, len)
            && let Some(written) = fs
                .write_at_aligned_hot_vectored(self.ino(), bufs, offset)
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
            let written = match fs.write_at(self.ino(), buf, offset).map_err(into_vfs_err) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
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
            fs.write_at_aligned_hot_vectored_async_submit(self.ino(), bufs, offset)
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
            .with_inode_ref(self.ino(), |inode| Ok(inode.size()))
            .map_err(into_vfs_err)?;
        let written = fs.write_at(self.ino(), buf, length).map_err(into_vfs_err)?;
        Ok((written, length + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.fs
            .lock()
            .set_len(self.ino(), len)
            .map_err(into_vfs_err)
    }

    fn set_len_failure_is_atomic(&self) -> bool {
        // ext4_fs_truncate_inode updates allocation metadata incrementally;
        // Fs::set_len poisons metadata after a failed shrinking truncate.
        false
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        self.fs
            .lock()
            .set_symlink(self.ino(), target.as_bytes())
            .map_err(into_vfs_err)
    }
}

impl Pollable for Inode {
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

impl DirNodeOps for Inode {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        ext4_named_create_inode_type(node_type).is_some()
    }

    fn supports_symlink(&self) -> bool {
        true
    }

    fn supports_hard_links(&self) -> bool {
        true
    }

    fn supports_unlink(&self) -> bool {
        true
    }

    fn supports_rmdir(&self) -> bool {
        true
    }

    fn supports_rename(&self) -> bool {
        true
    }

    fn namespace_epoch(&self) -> u64 {
        self.binding
            .get()
            .map_or(0, |binding| binding.namespace_epoch.load(Ordering::Acquire))
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut fs = self.fs.lock();
        let mut reader = fs.read_dir(self.ino(), offset).map_err(into_vfs_err)?;
        let operation = (|| {
            let mut count = 0;
            while let Some(entry) = reader.current() {
                let name = try_owned(
                    core::str::from_utf8(entry.name()).map_err(|_| VfsError::InvalidData)?,
                )?;
                let ino = entry.ino() as u64;
                let node_type = into_vfs_type(entry.inode_type());
                reader.step().map_err(into_vfs_err)?;
                if !sink.accept(&name, ino, node_type, reader.offset()) {
                    break;
                }
                count += 1;
            }
            Ok(count)
        })();
        combine_vfs_cleanup(operation, reader.finish().map_err(into_vfs_err))
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let entry_name = try_owned(name)?;
        let (token, namespace_epoch, inode_type) = {
            let mut fs = self.fs.lock();
            let (ino, inode_type) = fs.lookup_inode(self.ino(), name).map_err(into_vfs_err)?;
            let (token, namespace_epoch) = fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
            (token, namespace_epoch, inode_type)
        };
        self.try_finish_retained_entry(token, namespace_epoch, inode_type, entry_name)
    }

    fn create_named(
        &self,
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        let inode_type = match ext4_named_create_inode_type(options.node_type) {
            Some(inode_type) => inode_type,
            None if options.node_type == NodeType::Symlink => {
                return Err(VfsError::OperationNotSupported);
            }
            None => return Err(VfsError::InvalidData),
        };

        // Fast existing-name path: retain the stable identity while serialized,
        // then build its VFS node after releasing the ext4 spin lock.
        let existing = {
            let mut fs = self.fs.lock();
            match fs.lookup_inode(self.ino(), name) {
                Ok((ino, existing_type)) => {
                    if disposition == CreateDisposition::Exclusive {
                        return Err(VfsError::AlreadyExists);
                    }
                    let (token, namespace_epoch) =
                        fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
                    Some((token, namespace_epoch, existing_type))
                }
                Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => None,
                Err(err) => return Err(into_vfs_err(err)),
            }
        };
        if let Some((token, namespace_epoch, existing_type)) = existing {
            let entry =
                self.try_finish_retained_name(token, namespace_epoch, existing_type, name)?;
            return Ok(CreateOutcome {
                entry,
                created: false,
            });
        }

        let entry_name = try_owned(name)?;
        let mut prepared = self.try_prepare_child(inode_type, entry_name)?;
        prepared.install_initial_data(options)?;
        let mut fs = self.fs.lock();
        // Allocation above opened a race window. Revalidate under the same
        // serialization used by the backend create before committing anything.
        match fs.lookup_inode(self.ino(), name) {
            Ok((ino, existing_type)) => {
                if disposition == CreateDisposition::Exclusive {
                    return Err(VfsError::AlreadyExists);
                }
                let (token, namespace_epoch) = fs.retain_inode_handle(ino).map_err(into_vfs_err)?;
                drop(fs);
                drop(prepared);
                let entry =
                    self.try_finish_retained_name(token, namespace_epoch, existing_type, name)?;
                return Ok(CreateOutcome {
                    entry,
                    created: false,
                });
            }
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
            Err(err) => return Err(into_vfs_err(err)),
        }
        self.bump_namespace_epoch();
        let (token, namespace_epoch) = fs
            .create(
                self.ino(),
                name,
                inode_type,
                options.permission.bits() as _,
                options.owner,
                options.rdev.map(|rdev| rdev.0),
                Some(wall_time()),
            )
            .map_err(into_vfs_err)?;
        Ok(CreateOutcome {
            entry: prepared.bind(token, namespace_epoch),
            created: true,
        })
    }

    fn create_symlink(
        &self,
        name: &str,
        target: &str,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        {
            let mut fs = self.fs.lock();
            match fs.lookup_inode(self.ino(), name) {
                Ok(_) => return Err(VfsError::AlreadyExists),
                Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
                Err(err) => return Err(into_vfs_err(err)),
            }
        }
        let prepared = self.try_prepare_child(InodeType::Symlink, try_owned(name)?)?;
        let mut fs = self.fs.lock();
        match fs.lookup_inode(self.ino(), name) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {}
            Err(err) => return Err(into_vfs_err(err)),
        }
        self.bump_namespace_epoch();
        let (token, namespace_epoch) = fs
            .create_symlink(
                self.ino(),
                name,
                target.as_bytes(),
                permission.bits() as _,
                user,
                Some(wall_time()),
            )
            .map_err(into_vfs_err)?;
        Ok(prepared.bind(token, namespace_epoch))
    }

    fn link(&self, name: &str, node: &DirEntry) -> VfsResult<DirEntry> {
        let target = node
            .downcast::<Self>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let target_token = target.token().ok_or(VfsError::NotFound)?;
        let inode_type = match node.node_type() {
            NodeType::Fifo => InodeType::Fifo,
            NodeType::CharacterDevice => InodeType::CharacterDevice,
            NodeType::Directory => return Err(VfsError::OperationNotPermitted),
            NodeType::BlockDevice => InodeType::BlockDevice,
            NodeType::RegularFile => InodeType::RegularFile,
            NodeType::Symlink => InodeType::Symlink,
            NodeType::Socket => InodeType::Socket,
            NodeType::Unknown => return Err(VfsError::InvalidData),
        };
        let prepared = self.try_prepare_child(inode_type, try_owned(name)?)?;
        if let Some(runtime) = target.runtime.get() {
            prepared.inode.attach_runtime(runtime.clone());
        }
        let mut fs = self.fs.lock();
        let (retained_token, namespace_epoch) = fs
            .retain_inode_handle(target_token.ino())
            .map_err(into_vfs_err)?;
        if retained_token != target_token {
            fs.release_inode_handle(retained_token);
            return Err(VfsError::NotFound);
        }
        self.bump_namespace_epoch();
        if let Err(err) = fs.link(self.ino(), name, target_token.ino(), Some(wall_time())) {
            fs.release_inode_handle(retained_token);
            return Err(into_vfs_err(err));
        }
        Ok(prepared.bind(retained_token, namespace_epoch))
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let expected = match request.expected {
            Some(expected) => Some(self.expected_token(expected).ok_or(VfsError::NotFound)?),
            None => None,
        };
        let mut fs = self.fs.lock();
        self.bump_namespace_epoch();
        let now = wall_time();
        fs.unlink_checked(
            self.ino(),
            request.name,
            expected,
            Some(request.is_dir),
            Some(now),
        )
        .map_err(into_vfs_err)
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_dir: Arc<Self> = request
            .dst_dir
            .downcast()
            .map_err(|_| VfsError::InvalidInput)?;
        if !Arc::ptr_eq(&self.fs, &dst_dir.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let src = self.expected_token(request.src).ok_or(VfsError::NotFound)?;
        let dst = request
            .dst
            .map(|expected| self.expected_token(expected).ok_or(VfsError::NotFound))
            .transpose()?;
        let src_epoch = self.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let dst_epoch = dst_dir.namespace_epoch_handle().ok_or(VfsError::Io)?;
        let mut fs = self.fs.lock();
        self.bump_namespace_epoch();
        if !Arc::ptr_eq(src_epoch, dst_epoch) {
            dst_dir.bump_namespace_epoch();
        }
        let now = wall_time();
        fs.rename(
            self.ino(),
            request.src_name,
            dst_dir.ino(),
            request.dst_name,
            src,
            dst,
            Some(now),
        )
        .map_err(into_vfs_err)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "test-ramdisk")]
    use core::sync::atomic::AtomicBool;
    #[cfg(feature = "test-ramdisk")]
    use std::{
        fs::{self, OpenOptions},
        process::Command,
    };

    #[cfg(feature = "test-ramdisk")]
    use axdriver::{AxBlockDevice, SharedBlockDevice};
    #[cfg(feature = "test-ramdisk")]
    use axfs_ng_vfs::Mountpoint;

    use super::*;

    #[cfg(feature = "test-ramdisk")]
    fn formatted_ext4_device() -> crate::MountedBlockDevice {
        let path = std::env::temp_dir().join(format!(
            "axfs-ext4-xattr-{}-{}.img",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        drop(file);
        let status = Command::new("mke2fs")
            .args([
                "-q",
                "-t",
                "ext4",
                "-F",
                "-b",
                "4096",
                "-O",
                "none,has_journal,ext_attr,dir_index,filetype,extent,64bit,flex_bg,sparse_super,\
                 large_file,huge_file,dir_nlink,extra_isize,metadata_csum",
            ])
            .arg(&path)
            .status()
            .expect("mke2fs is required for the axfs ext4 provider test");
        assert!(status.success());
        let image = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();

        crate::MountedBlockDevice {
            device: SharedBlockDevice::new(AxBlockDevice::from(image.as_slice())),
            mounted: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn ext4_named_create_capabilities_match_backend_primitives() {
        for node_type in [
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::Directory,
            NodeType::BlockDevice,
            NodeType::RegularFile,
            NodeType::Socket,
        ] {
            assert!(ext4_named_create_inode_type(node_type).is_some());
        }
        assert!(ext4_named_create_inode_type(NodeType::Symlink).is_none());
        assert!(ext4_named_create_inode_type(NodeType::Unknown).is_none());
    }

    #[test]
    fn ext4_xattr_modes_preserve_linux_existence_errors() {
        for (mode, exists) in [
            (XattrSetMode::Upsert, false),
            (XattrSetMode::Upsert, true),
            (XattrSetMode::Create, false),
            (XattrSetMode::Replace, true),
        ] {
            assert!(admit_xattr_set_mode(mode, exists).is_ok());
        }
        assert_eq!(
            admit_xattr_set_mode(XattrSetMode::Create, true)
                .unwrap_err()
                .code,
            EEXIST as i32
        );
        assert_eq!(
            admit_xattr_set_mode(XattrSetMode::Replace, false)
                .unwrap_err()
                .code,
            ENODATA as i32
        );
    }

    #[cfg(feature = "test-ramdisk")]
    #[test]
    fn ext4_xattr_provider_preserves_raw_name_bytes() {
        let filesystem = Ext4Filesystem::new(formatted_ext4_device()).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let root = mount.root_location();
        let file = root
            .create(
                "first-xattr",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let raw_name = b"user.raw-\xff-name";
        let mut boundary_name = b"user.".to_vec();
        boundary_name.resize(255, 0xfe);
        assert_eq!(boundary_name.len(), 255);

        file.set_xattr(b"user.first", b"provider", XattrSetMode::Create)
            .unwrap();
        file.set_xattr(raw_name, b"raw", XattrSetMode::Create)
            .unwrap();
        file.set_xattr(&boundary_name, b"boundary", XattrSetMode::Create)
            .unwrap();
        assert_eq!(file.get_xattr(b"user.first").unwrap(), b"provider");
        assert_eq!(file.get_xattr(raw_name).unwrap(), b"raw");
        assert_eq!(file.get_xattr(&boundary_name).unwrap(), b"boundary");
        let listed = file.list_xattrs().unwrap();
        let names = listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect::<alloc::vec::Vec<_>>();
        assert!(names.contains(&b"user.first".as_slice()));
        assert!(names.contains(&raw_name.as_slice()));
        assert!(names.contains(&boundary_name.as_slice()));

        file.remove_xattr(raw_name).unwrap();
        file.remove_xattr(&boundary_name).unwrap();
        assert_eq!(file.list_xattrs().unwrap(), b"user.first\0");

        drop(file);
        drop(root);
        drop(mount);
        drop(filesystem);
        while axfs_ng_vfs::drain_deferred_dentry_cache_cleanup() {}
        crate::drain_deferred_filesystem_finalizers(|| {});
    }
}
