use alloc::{string::String, sync::Arc};
use core::{any::Any, mem, ops::Deref, sync::atomic::Ordering, time::Duration};

use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FilesystemOps, Metadata, MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps, NodeType,
    NodeUserData, Reference, RenameRequest, UnlinkRequest, VfsError, VfsResult, WeakDirEntry,
};
use spin::Once;

use super::{
    FsRef, ff,
    file::FatFileNode,
    fs::{FatEntryIdentity, FatEntryState, FatFilesystem, FatFilesystemInner},
    util::{file_metadata, into_vfs_err},
};

fn try_ascii_lowercase(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    result.make_ascii_lowercase();
    Ok(result)
}

fn try_clone_string(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    Ok(result)
}

const fn fat_named_create_is_directory(node_type: NodeType) -> Option<bool> {
    match node_type {
        NodeType::RegularFile => Some(false),
        NodeType::Directory => Some(true),
        _ => None,
    }
}

pub struct FatDirNode {
    fs: Arc<FatFilesystem>,
    pub(crate) inner: FsRef<Option<ff::Dir<'static>>>,
    inode: u64,
    state: FatEntryState,
    this: Once<WeakDirEntry>,
}

enum PendingFatNode {
    File(Arc<FatFileNode>),
    Directory(Arc<FatDirNode>),
}

impl FatDirNode {
    fn try_new_pending(
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
            this: Once::new(),
        }) {
            Ok(node) => node,
            Err(_) => {
                fs.release_inode(inode);
                return Err(VfsError::NoMemory);
            }
        };
        let entry = DirEntry::try_new_dir(DirNode::new(node.clone()), reference)?;
        node.this.call_once(|| entry.downgrade());
        Ok((entry, node))
    }

    pub(crate) fn try_new_initialized(
        fs: Arc<FatFilesystem>,
        dir: ff::Dir,
        inode: u64,
        state: FatEntryState,
        reference: Reference,
        fs_guard: &FatFilesystemInner,
    ) -> VfsResult<DirEntry> {
        let (entry, node) = Self::try_new_pending(fs, inode, state, reference)?;
        node.install_inner(fs_guard, dir);
        Ok(entry)
    }

    fn install_inner(&self, fs: &FatFilesystemInner, dir: ff::Dir) {
        // SAFETY: FsRef ties the backend handle to the filesystem guard which
        // owns the actual fatfs object for at least as long as this node.
        *self.inner.borrow_mut(fs) =
            Some(unsafe { mem::transmute::<ff::Dir<'_>, ff::Dir<'static>>(dir) });
    }

    fn inner<'a>(&self, fs: &'a FatFilesystemInner) -> VfsResult<&'a ff::Dir<'static>> {
        self.inner.borrow(fs).as_ref().ok_or(VfsError::Io)
    }

    fn inner_mut<'a>(&self, fs: &'a FatFilesystemInner) -> VfsResult<&'a mut ff::Dir<'static>> {
        self.inner.borrow_mut(fs).as_mut().ok_or(VfsError::Io)
    }

    fn this_entry(&self) -> VfsResult<DirEntry> {
        self.this
            .get()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::NotFound)
    }

    fn create_entry(
        &self,
        fs_guard: &FatFilesystemInner,
        entry: ff::DirEntry,
        name: String,
        inode: u64,
    ) -> VfsResult<DirEntry> {
        let state = match self.fs.entry_state(entry.entry_position()) {
            Ok(state) => state,
            Err(error) => {
                self.fs.release_inode(inode);
                return Err(error);
            }
        };
        let parent = match self.this_entry() {
            Ok(parent) => parent,
            Err(error) => {
                self.fs.release_inode(inode);
                return Err(error);
            }
        };
        let reference = Reference::new(Some(parent), name);
        if entry.is_file() {
            FatFileNode::try_new_initialized(
                self.fs.clone(),
                entry.to_file(),
                inode,
                state,
                reference,
                fs_guard,
            )
        } else {
            Self::try_new_initialized(
                self.fs.clone(),
                entry.to_dir(),
                inode,
                state,
                reference,
                fs_guard,
            )
        }
    }

    fn matches_expected(
        &self,
        expected: &DirEntry,
        node_type: NodeType,
        identity: FatEntryIdentity,
    ) -> bool {
        if node_type == NodeType::Directory {
            expected.downcast::<Self>().is_ok_and(|expected| {
                Arc::ptr_eq(&self.fs, &expected.fs) && expected.state.identity() == identity
            })
        } else {
            expected
                .downcast::<FatFileNode>()
                .is_ok_and(|expected| expected.matches_identity(&self.fs, identity))
        }
    }
}

unsafe impl Send for FatDirNode {}

unsafe impl Sync for FatDirNode {}

impl NodeOps for FatDirNode {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let fs = self.fs.lock();
        let dir = self.inner(&fs)?;
        if let Some(file) = dir.as_file() {
            return file_metadata(&fs, file, self.inode, NodeType::Directory);
        }

        // root directory
        let block_size = fs.inner.bytes_per_sector() as u64;
        Ok(Metadata {
            inode: self.inode(),
            device: 0,
            nlink: 1,
            mode: fs.mount_options.dir_mode,
            node_type: NodeType::Directory,
            uid: fs.mount_options.uid,
            gid: fs.mount_options.gid,
            project_id: 0,
            size: block_size,
            block_size,
            blocks: 1,
            rdev: DeviceId::default(),
            atime: fs.root_atime.into(),
            btime: Duration::default().into(),
            mtime: fs.root_mtime.into(),
            ctime: Duration::default().into(),
        })
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        if update.mode.is_some()
            || update.owner.is_some()
            || update.rdev.is_some()
            || update.ctime.is_some()
        {
            return Err(VfsError::Unsupported);
        }

        let mut fs = self.fs.lock();
        let dir = self.inner_mut(&fs)?;
        if let Some(file) = dir.as_file_mut() {
            return super::util::update_file_metadata(file, update);
        }
        if let Some(atime) = update.atime {
            fs.root_atime = atime.try_into_duration().ok_or(VfsError::InvalidInput)?;
        }
        if let Some(mtime) = update.mtime {
            fs.root_mtime = mtime.try_into_duration().ok_or(VfsError::InvalidInput)?;
        }
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.deref()
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

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.state.user_data())
    }
}

impl DirNodeOps for FatDirNode {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        fat_named_create_is_directory(node_type).is_some()
    }

    fn supports_unlink(&self) -> bool {
        true
    }

    fn supports_rmdir(&self) -> bool {
        true
    }

    fn namespace_epoch(&self) -> u64 {
        self.state.namespace_epoch().load(Ordering::Acquire)
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let fs = self.fs.lock();
        let dir = self.inner(&fs)?;
        let this_entry = self.this_entry()?;
        let dir_node = this_entry.as_dir()?;

        let mut count = 0;
        for entry in dir.iter().skip(offset as usize) {
            let entry = entry.map_err(into_vfs_err)?;
            let mut name = entry.try_file_name().map_err(|_| VfsError::NoMemory)?;
            name.make_ascii_lowercase();
            let node_type = if entry.is_file() {
                NodeType::RegularFile
            } else {
                NodeType::Directory
            };
            if let Some(entry) = dir_node.lookup_cache(&name) {
                if !sink.accept(&name, entry.inode(), node_type, offset + count + 1) {
                    break;
                }
            } else {
                let reference_name = try_clone_string(&name)?;
                let inode = self.fs.alloc_inode()?;
                let entry = self.create_entry(&fs, entry, reference_name, inode)?;
                let inode = entry.inode();
                let accepted = sink.accept(&name, inode, node_type, offset + count + 1);
                dir_node.insert_cache(name, entry);
                if !accepted {
                    break;
                }
            }
            count += 1;
        }
        Ok(count as usize)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let fs = self.fs.lock();
        let dir = self.inner(&fs)?;
        for entry in dir.iter() {
            let entry = entry.map_err(into_vfs_err)?;
            if entry.eq_name(name) {
                let reference_name = try_ascii_lowercase(name)?;
                let inode = self.fs.alloc_inode()?;
                return self.create_entry(&fs, entry, reference_name, inode);
            }
        }
        Err(VfsError::NotFound)
    }

    fn create_named(
        &self,
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        let fs = self.fs.lock();
        let dir = self.inner(&fs)?;
        for existing in dir.iter() {
            let existing = existing.map_err(into_vfs_err)?;
            if existing.eq_name(name) {
                if disposition == CreateDisposition::Exclusive {
                    return Err(VfsError::AlreadyExists);
                }
                let reference_name = try_ascii_lowercase(name)?;
                let inode = self.fs.alloc_inode()?;
                let entry = self.create_entry(&fs, existing, reference_name, inode)?;
                return Ok(CreateOutcome {
                    entry,
                    created: false,
                });
            }
        }
        let create_directory = fat_named_create_is_directory(options.node_type)
            .ok_or(VfsError::OperationNotSupported)?;
        let reference_name = try_ascii_lowercase(name)?;
        let reference = Reference::new(Some(self.this_entry()?), reference_name);
        let admission = self.fs.prepare_entry_state()?;
        let inode = self.fs.alloc_inode()?;
        let (entry, pending) = if create_directory {
            let (entry, node) =
                Self::try_new_pending(self.fs.clone(), inode, admission.state(), reference)?;
            (entry, PendingFatNode::Directory(node))
        } else {
            let (entry, node) =
                FatFileNode::try_new_pending(self.fs.clone(), inode, admission.state(), reference)?;
            (entry, PendingFatNode::File(node))
        };
        options.install_initial_data(&entry)?;

        let position = match pending {
            PendingFatNode::File(node) => {
                let file = dir.create_file(name).map_err(into_vfs_err)?;
                let Some(position) = file.entry_position() else {
                    if dir.remove(name).is_err() {
                        dir.poison();
                    }
                    return Err(VfsError::Io);
                };
                node.install_inner(&fs, file);
                position
            }
            PendingFatNode::Directory(node) => {
                let child_dir = dir.create_dir(name).map_err(into_vfs_err)?;
                let Some(position) = child_dir.entry_position() else {
                    if dir.remove(name).is_err() {
                        dir.poison();
                    }
                    return Err(VfsError::Io);
                };
                node.install_inner(&fs, child_dir);
                position
            }
        };
        admission.commit(position);
        self.state.namespace_epoch().fetch_add(1, Ordering::AcqRel);
        Ok(CreateOutcome {
            entry,
            created: true,
        })
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        //  EPERM  The filesystem containing oldpath and newpath does not
        //         support the creation of hard links.
        Err(VfsError::PermissionDenied)
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let fs = self.fs.lock();
        let (position, node_type) = {
            let dir = self.inner(&fs)?;
            let mut current = None;
            for entry in dir.iter() {
                let entry = entry.map_err(into_vfs_err)?;
                if entry.eq_name(request.name) {
                    current = Some(entry);
                    break;
                }
            }
            let current = current.ok_or(VfsError::NotFound)?;
            let node_type = if current.is_dir() {
                NodeType::Directory
            } else {
                NodeType::RegularFile
            };
            (current.entry_position(), node_type)
        };
        let state = self.fs.entry_state(position)?;
        if request
            .expected
            .is_some_and(|expected| !self.matches_expected(expected, node_type, state.identity()))
        {
            return Err(VfsError::NotFound);
        }
        match (node_type == NodeType::Directory, request.is_dir) {
            (true, false) => return Err(VfsError::IsADirectory),
            (false, true) => return Err(VfsError::NotADirectory),
            _ => {}
        }
        {
            let dir = self.inner(&fs)?;
            if let Err(error) = dir.remove(request.name) {
                if dir.is_poisoned() {
                    self.fs.forget_entry(position);
                    self.state.namespace_epoch().fetch_add(1, Ordering::AcqRel);
                }
                return Err(into_vfs_err(error));
            }
        }
        self.fs.forget_entry(position);
        self.state.namespace_epoch().fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_dir: Arc<Self> = request
            .dst_dir
            .downcast()
            .map_err(|_| VfsError::InvalidInput)?;
        if !Arc::ptr_eq(&self.fs, &dst_dir.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let fs = self.fs.lock();
        let (src_position, src_type, dst_info) = {
            let dir = self.inner(&fs)?;
            let dst_inner = dst_dir.inner(&fs)?;
            let mut src = None;
            for entry in dir.iter() {
                let entry = entry.map_err(into_vfs_err)?;
                if entry.eq_name(request.src_name) {
                    src = Some(entry);
                    break;
                }
            }
            let src = src.ok_or(VfsError::NotFound)?;
            let src_type = if src.is_dir() {
                NodeType::Directory
            } else {
                NodeType::RegularFile
            };

            let mut dst = None;
            for entry in dst_inner.iter() {
                let entry = entry.map_err(into_vfs_err)?;
                if entry.eq_name(request.dst_name) {
                    dst = Some(entry);
                    break;
                }
            }
            let dst_info = match dst {
                Some(dst) => {
                    let node_type = if dst.is_dir() {
                        NodeType::Directory
                    } else {
                        NodeType::RegularFile
                    };
                    let empty = !dst.is_dir() || dst.to_dir().is_empty().map_err(into_vfs_err)?;
                    Some((dst.entry_position(), node_type, empty))
                }
                None => None,
            };
            (src.entry_position(), src_type, dst_info)
        };
        let src_state = self.fs.entry_state(src_position)?;
        if !self.matches_expected(request.src, src_type, src_state.identity()) {
            return Err(VfsError::NotFound);
        }
        let dst_state = match dst_info.as_ref() {
            Some((position, ..)) => Some(self.fs.entry_state(*position)?),
            None => None,
        };
        match (request.dst, dst_info.as_ref(), dst_state.as_ref()) {
            (None, None, None) => {}
            (Some(expected), Some((_, node_type, _)), Some(state)) => {
                if !self.matches_expected(expected, *node_type, state.identity()) {
                    return Err(VfsError::NotFound);
                }
            }
            _ => return Err(VfsError::NotFound),
        }
        if dst_state
            .as_ref()
            .is_some_and(|dst| dst.identity() == src_state.identity())
        {
            return Ok(());
        }
        if let Some((_, dst_type, empty)) = dst_info.as_ref() {
            match (
                src_type == NodeType::Directory,
                *dst_type == NodeType::Directory,
            ) {
                (true, false) => return Err(VfsError::NotADirectory),
                (false, true) => return Err(VfsError::IsADirectory),
                _ => {}
            }
            if *dst_type == NodeType::Directory && !empty {
                return Err(VfsError::DirectoryNotEmpty);
            }
        }

        {
            let dir = self.inner(&fs)?;
            let dst_inner = dst_dir.inner(&fs)?;
            let result = if dst_info.is_some() {
                dir.rename_replace(request.src_name, dst_inner, request.dst_name)
            } else {
                dir.rename(request.src_name, dst_inner, request.dst_name)
            };
            if let Err(error) = result {
                if dir.is_poisoned() {
                    self.fs.forget_entry(src_position);
                    if let Some((dst_position, ..)) = dst_info.as_ref() {
                        self.fs.forget_entry(*dst_position);
                    }
                    self.state.namespace_epoch().fetch_add(1, Ordering::AcqRel);
                    if !core::ptr::eq(
                        self.state.namespace_epoch(),
                        dst_dir.state.namespace_epoch(),
                    ) {
                        dst_dir
                            .state
                            .namespace_epoch()
                            .fetch_add(1, Ordering::AcqRel);
                    }
                }
                return Err(into_vfs_err(error));
            }
        }
        if let Some((dst_position, ..)) = dst_info {
            self.fs.forget_entry(dst_position);
        }
        self.fs.forget_entry(src_position);
        self.state.namespace_epoch().fetch_add(1, Ordering::AcqRel);
        if !core::ptr::eq(
            self.state.namespace_epoch(),
            dst_dir.state.namespace_epoch(),
        ) {
            dst_dir
                .state
                .namespace_epoch()
                .fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
}

impl Drop for FatDirNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.inode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_named_create_capabilities_match_backend_primitives() {
        assert_eq!(
            fat_named_create_is_directory(NodeType::RegularFile),
            Some(false)
        );
        assert_eq!(
            fat_named_create_is_directory(NodeType::Directory),
            Some(true)
        );
        for node_type in [
            NodeType::Unknown,
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::BlockDevice,
            NodeType::Symlink,
            NodeType::Socket,
        ] {
            assert!(fat_named_create_is_directory(node_type).is_none());
        }
    }
}
