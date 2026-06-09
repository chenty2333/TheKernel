use alloc::{
    borrow::ToOwned,
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{any::Any, borrow::Borrow, cmp::Ordering, mem, task::Context};

use axerrno::{AxError, AxResult};
use axfs::{
    prune_dead_cached_file_registry_entries_for_inode, remove_cached_file_registry_entry,
    sync_and_invalidate_cached_file_pages,
};
use axfs_ng_vfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, Filesystem,
    FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType,
    Reference, StatFs, VfsError, VfsResult, WeakDirEntry,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use hashbrown::HashMap;
use memory_addr::PAGE_SIZE_4K;
use slab::Slab;

use crate::pseudofs::dummy_stat_fs;

const TMPFS_BLOCK_SIZE: u64 = PAGE_SIZE_4K as u64;
const STAT_BLOCK_UNIT: u64 = 512;

#[derive(PartialEq, Eq, Hash, Clone)]
struct FileName(String);

impl PartialOrd for FileName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FileName {
    fn cmp(&self, other: &Self) -> Ordering {
        fn index(s: &str) -> u8 {
            match s {
                "." => 0,
                ".." => 1,
                _ => 2,
            }
        }
        (index(&self.0), &self.0).cmp(&(index(&other.0), &other.0))
    }
}

impl<T> From<T> for FileName
where
    T: Into<String>,
{
    fn from(name: T) -> Self {
        Self(name.into())
    }
}

impl Borrow<str> for FileName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A simple in-memory filesystem that supports basic file operations.
pub struct MemoryFs {
    inodes: Mutex<Slab<Arc<Inode>>>,
    root: Mutex<Option<DirEntry>>,
}

impl MemoryFs {
    /// Creates a new empty memory filesystem.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Filesystem {
        Self::new_with_permission(NodePermission::from_bits_truncate(0o755))
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_permission(permission: NodePermission) -> Filesystem {
        let fs = Arc::new(Self {
            inodes: Mutex::new(Slab::new()),
            root: Mutex::default(),
        });
        let root_ino = Inode::new(&fs, None, NodeType::Directory, permission);
        *fs.root.lock() = Some(DirEntry::new_dir(
            |this| DirNode::new(MemoryNode::new(fs.clone(), root_ino, Some(this))),
            Reference::root(),
        ));
        Filesystem::new(fs)
    }

    fn get(&self, ino: u64) -> Arc<Inode> {
        self.inodes.lock()[ino as usize - 1].clone()
    }
}

impl FilesystemOps for MemoryFs {
    fn name(&self) -> &str {
        "tmpfs"
    }

    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(dummy_stat_fs(0x01021994))
    }
}

fn finalize_unlinked_inode(fs: &MemoryFs, inode: &Arc<Inode>) {
    let mut inodes = fs.inodes.lock();
    let metadata = inode.metadata.lock();
    if metadata.nlink != 0 {
        return;
    }
    let device = metadata.device;
    let ino = metadata.inode;
    drop(metadata);

    remove_cached_file_registry_entry(device, ino);
    prune_dead_cached_file_registry_entries_for_inode(ino);
    if Arc::strong_count(inode) == 2 {
        if let NodeContent::File(file) = &inode.content {
            file.clear_storage();
        }
        inodes.remove(ino as usize - 1);
    }
}

fn release_inode(fs: &MemoryFs, inode: &Arc<Inode>, nlink: u64) {
    let mut metadata = inode.metadata.lock();
    metadata.nlink -= nlink;
    let is_unlinked = metadata.nlink == 0;
    drop(metadata);
    if is_unlinked {
        finalize_unlinked_inode(fs, inode);
    }
}

#[derive(Default)]
struct FileContent {
    /// The length of the file content.
    ///
    /// We only need to store the length here because we delegate the actual
    /// content management to page cache.
    length: Mutex<u64>,
    symlink: Mutex<Option<String>>,
    pages: Mutex<BTreeMap<u64, Box<[u8; PAGE_SIZE_4K]>>>,
    allocated_pages: Mutex<BTreeSet<u64>>,
    hole_pages: Mutex<BTreeSet<u64>>,
}

impl FileContent {
    fn set_len(&self, len: u64) {
        *self.length.lock() = len;
        let last_page = len.div_ceil(TMPFS_BLOCK_SIZE);
        let mut pages = self.pages.lock();
        pages.retain(|page, _| *page < last_page);
        if len > 0 {
            let last_used_page = (len - 1) / TMPFS_BLOCK_SIZE;
            let last_used_len = ((len - 1) % TMPFS_BLOCK_SIZE + 1) as usize;
            if let Some(page) = pages.get_mut(&last_used_page) {
                page[last_used_len..].fill(0);
            }
        }
        drop(pages);
        self.allocated_pages.lock().retain(|page| *page < last_page);
        self.hole_pages.lock().retain(|page| *page < last_page);
    }

    fn clear_storage(&self) {
        *self.length.lock() = 0;
        *self.symlink.lock() = None;
        self.pages.lock().clear();
        self.allocated_pages.lock().clear();
        self.hole_pages.lock().clear();
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = *self.length.lock();
        if offset >= len {
            return Ok(0);
        }
        let total = buf.len().min((len - offset) as usize);
        let pages = self.pages.lock();
        let holes = self.hole_pages.lock();
        let mut done = 0;
        while done < total {
            let pos = offset + done as u64;
            let page = pos / TMPFS_BLOCK_SIZE;
            let page_off = (pos % TMPFS_BLOCK_SIZE) as usize;
            let chunk = (total - done).min(PAGE_SIZE_4K - page_off);
            let dst = &mut buf[done..done + chunk];
            if holes.contains(&page) {
                dst.fill(0);
            } else if let Some(src) = pages.get(&page) {
                dst.copy_from_slice(&src[page_off..page_off + chunk]);
            } else {
                dst.fill(0);
            }
            done += chunk;
        }
        Ok(total)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        let current_len = *self.length.lock();
        if end > current_len {
            self.set_len(end);
        }
        let mut pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        let mut holes = self.hole_pages.lock();
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let page = pos / TMPFS_BLOCK_SIZE;
            let page_off = (pos % TMPFS_BLOCK_SIZE) as usize;
            let chunk = (buf.len() - done).min(PAGE_SIZE_4K - page_off);
            let dst = pages
                .entry(page)
                .or_insert_with(|| Box::new([0; PAGE_SIZE_4K]));
            dst[page_off..page_off + chunk].copy_from_slice(&buf[done..done + chunk]);
            allocated.insert(page);
            holes.remove(&page);
            done += chunk;
        }
        Ok(buf.len())
    }

    fn reserve_range(&self, offset: u64, len: u64) {
        let Some((start, end)) = page_range(offset, len) else {
            return;
        };
        let mut allocated = self.allocated_pages.lock();
        let mut holes = self.hole_pages.lock();
        for page in start..end {
            allocated.insert(page);
            holes.remove(&page);
        }
    }

    fn punch_hole(&self, offset: u64, len: u64) {
        let Some((start, end)) = full_page_range(offset, len) else {
            return;
        };
        let mut pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        let mut holes = self.hole_pages.lock();
        for page in start..end {
            pages.remove(&page);
            allocated.remove(&page);
            holes.insert(page);
        }
    }

    fn collapse_range(&self, offset: u64, len: u64) {
        let Some((start, end)) = full_page_range(offset, len) else {
            return;
        };
        let delta = end - start;

        let remap_pages = |pages: &mut BTreeSet<u64>| {
            let current = pages.iter().copied().collect::<Vec<_>>();
            pages.clear();
            for page in current {
                if page < start {
                    pages.insert(page);
                } else if page >= end {
                    pages.insert(page - delta);
                }
            }
        };

        let mut data_pages = self.pages.lock();
        let current = mem::take(&mut *data_pages);
        for (page, data) in current {
            if page < start {
                data_pages.insert(page, data);
            } else if page >= end {
                data_pages.insert(page - delta, data);
            }
        }
        drop(data_pages);
        remap_pages(&mut self.allocated_pages.lock());
        remap_pages(&mut self.hole_pages.lock());
    }

    fn insert_range(&self, offset: u64, len: u64) {
        let Some((start, end)) = full_page_range(offset, len) else {
            return;
        };
        let delta = end - start;

        let remap_pages = |pages: &mut BTreeSet<u64>| {
            let current = pages.iter().copied().collect::<Vec<_>>();
            pages.clear();
            for page in current {
                if page < start {
                    pages.insert(page);
                } else {
                    pages.insert(page + delta);
                }
            }
        };

        let mut data_pages = self.pages.lock();
        let current = mem::take(&mut *data_pages);
        for (page, data) in current {
            if page < start {
                data_pages.insert(page, data);
            } else {
                data_pages.insert(page + delta, data);
            }
        }
        drop(data_pages);
        remap_pages(&mut self.allocated_pages.lock());
        remap_pages(&mut self.hole_pages.lock());
        self.hole_pages.lock().extend(start..end);
    }

    fn blocks(&self) -> u64 {
        self.allocated_pages.lock().len() as u64 * (TMPFS_BLOCK_SIZE / STAT_BLOCK_UNIT)
    }

    fn seek_data_or_hole(&self, size: u64, offset: u64, seek_hole: bool) -> AxResult<u64> {
        if offset > size {
            return Err(AxError::InvalidInput);
        }
        if offset == size {
            return if seek_hole {
                Ok(size)
            } else {
                Err(AxError::NotFound)
            };
        }

        let allocated = self.allocated_pages.lock();
        let holes = self.hole_pages.lock();
        let mut page = offset / TMPFS_BLOCK_SIZE;
        let mut pos = offset;
        let last_page = size.div_ceil(TMPFS_BLOCK_SIZE);

        while page < last_page {
            let is_data = allocated.contains(&page) && !holes.contains(&page);
            if seek_hole != is_data {
                return Ok(pos.min(size));
            }
            page += 1;
            pos = page * TMPFS_BLOCK_SIZE;
        }

        if seek_hole {
            Ok(size)
        } else {
            Err(AxError::NotFound)
        }
    }
}

fn page_range(offset: u64, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let end = offset.checked_add(len)?;
    Some((offset / TMPFS_BLOCK_SIZE, end.div_ceil(TMPFS_BLOCK_SIZE)))
}

fn full_page_range(offset: u64, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let end = offset.checked_add(len)?;
    let start_page = offset.div_ceil(TMPFS_BLOCK_SIZE);
    let end_page = end / TMPFS_BLOCK_SIZE;
    (start_page < end_page).then_some((start_page, end_page))
}

fn file_content_for(loc: &axfs_ng_vfs::Location) -> Option<Arc<Inode>> {
    let node = loc.entry().downcast::<MemoryNode>().ok()?;
    node.inode.as_file().ok()?;
    Some(node.inode.clone())
}

pub fn xattr_store(loc: &axfs_ng_vfs::Location) -> Option<Arc<Mutex<BTreeMap<String, Vec<u8>>>>> {
    let node = loc.entry().downcast::<MemoryNode>().ok()?;
    Some(node.inode.xattrs.clone())
}

pub fn reserve_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
    extend: bool,
) -> Option<AxResult<()>> {
    let inode = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    if let Err(err) = sync_and_invalidate_cached_file_pages(loc) {
        return Some(Err(err));
    }
    if extend {
        let Some(end) = offset.checked_add(len) else {
            return Some(Err(AxError::InvalidInput));
        };
        if end > *file.length.lock() {
            file.set_len(end);
        }
    }
    file.reserve_range(offset, len);
    Some(Ok(()))
}

pub fn punch_hole_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let inode = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    if let Err(err) = sync_and_invalidate_cached_file_pages(loc) {
        return Some(Err(err));
    }
    file.punch_hole(offset, len);
    Some(Ok(()))
}

pub fn collapse_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let inode = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    if let Err(err) = sync_and_invalidate_cached_file_pages(loc) {
        return Some(Err(err));
    }
    file.collapse_range(offset, len);
    Some(Ok(()))
}

pub fn insert_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let inode = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    if let Err(err) = sync_and_invalidate_cached_file_pages(loc) {
        return Some(Err(err));
    }
    file.insert_range(offset, len);
    Some(Ok(()))
}

pub fn seek_data_or_hole(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    seek_hole: bool,
) -> Option<AxResult<u64>> {
    let inode = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(file.seek_data_or_hole(*file.length.lock(), offset, seek_hole))
}

#[derive(Default)]
struct DirContent {
    entries: Mutex<HashMap<FileName, InodeRef>>,
}

enum NodeContent {
    File(FileContent),
    Dir(DirContent),
}

struct Inode {
    ino: u64,
    metadata: Mutex<Metadata>,
    content: NodeContent,
    xattrs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl Inode {
    pub fn new(
        fs: &Arc<MemoryFs>,
        parent: Option<u64>,
        node_type: NodeType,
        permission: NodePermission,
    ) -> Arc<Inode> {
        let mut inodes = fs.inodes.lock();
        let entry = inodes.vacant_entry();
        let ino = entry.key() as u64 + 1;
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 0,
            mode: permission,
            node_type,
            uid: 0,
            gid: 0,
            size: 0,
            block_size: TMPFS_BLOCK_SIZE,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now,
            mtime: now,
            ctime: now,
        };
        let content = match node_type {
            NodeType::Directory => NodeContent::Dir(DirContent::default()),
            _ => NodeContent::File(FileContent::default()),
        };
        let result = Arc::new(Self {
            ino,
            metadata: Mutex::new(metadata),
            content,
            xattrs: Arc::new(Mutex::default()),
        });
        entry.insert(result.clone());
        drop(inodes);
        if let NodeContent::Dir(dir) = &result.content {
            let mut entries = dir.entries.lock();
            entries.insert(".".into(), InodeRef::new(fs.clone(), ino));
            entries.insert(
                "..".into(),
                InodeRef::new(fs.clone(), parent.unwrap_or(ino)),
            );
        }
        result
    }

    fn as_file(&self) -> VfsResult<&FileContent> {
        match self.content {
            NodeContent::File(ref content) => Ok(content),
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn as_dir(&self) -> VfsResult<&DirContent> {
        match self.content {
            NodeContent::Dir(ref content) => Ok(content),
            _ => Err(VfsError::NotADirectory),
        }
    }
}

struct InodeRef {
    fs: Arc<MemoryFs>,
    ino: u64,
}

impl InodeRef {
    pub fn new(fs: Arc<MemoryFs>, ino: u64) -> Self {
        fs.get(ino).metadata.lock().nlink += 1;
        Self { fs, ino }
    }

    fn get(&self) -> Arc<Inode> {
        self.fs.get(self.ino)
    }
}

impl Drop for InodeRef {
    fn drop(&mut self) {
        release_inode(&self.fs, &self.get(), 1);
    }
}

struct MemoryNode {
    fs: Arc<MemoryFs>,
    inode: Arc<Inode>,
    this: Option<WeakDirEntry>,
}

impl MemoryNode {
    pub fn new(fs: Arc<MemoryFs>, inode: Arc<Inode>, this: Option<WeakDirEntry>) -> Arc<Self> {
        Arc::new(Self { fs, inode, this })
    }

    fn new_entry(&self, name: &str, node_type: NodeType, inode: Arc<Inode>) -> VfsResult<DirEntry> {
        let fs = self.fs.clone();
        let reference = Reference::new(
            self.this.as_ref().and_then(WeakDirEntry::upgrade),
            name.to_owned(),
        );
        Ok(if node_type == NodeType::Directory {
            DirEntry::new_dir(
                |this| DirNode::new(MemoryNode::new(fs, inode, Some(this))),
                reference,
            )
        } else {
            DirEntry::new_file(
                FileNode::new(MemoryNode::new(fs, inode, None)),
                node_type,
                reference,
            )
        })
    }
}

impl NodeOps for MemoryNode {
    fn inode(&self) -> u64 {
        self.inode.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.inode.metadata.lock().clone();
        match &self.inode.content {
            NodeContent::File(content) => {
                metadata.size = *content.length.lock();
                metadata.block_size = TMPFS_BLOCK_SIZE;
                metadata.blocks = content.blocks();
            }
            NodeContent::Dir(dir) => {
                metadata.size = dir.entries.lock().len() as u64;
            }
        }
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let mut metadata = self.inode.metadata.lock();
        if let Some(mode) = update.mode {
            metadata.mode = mode;
        }
        if let Some((uid, gid)) = update.owner {
            metadata.uid = uid;
            metadata.gid = gid;
        }
        if let Some(rdev) = update.rdev {
            metadata.rdev = rdev;
        }
        if let Some(atime) = update.atime {
            metadata.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            metadata.mtime = mtime;
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

    fn flags(&self) -> NodeFlags {
        NodeFlags::ALWAYS_CACHE
    }
}

impl FileNodeOps for MemoryNode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let file = self.inode.as_file()?;
        if let Some(symlink) = file.symlink.lock().as_ref() {
            assert_eq!(offset, 0);
            let len = buf.len().min(symlink.len());
            buf[..len].copy_from_slice(&symlink.as_bytes()[..len]);
            return Ok(len);
        }
        file.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.inode.as_file()?.write_at(buf, offset)
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let file = self.inode.as_file()?;
        let offset = *file.length.lock();
        let written = file.write_at(buf, offset)?;
        Ok((written, offset + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.inode.as_file()?.set_len(len);
        Ok(())
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        let file = self.inode.as_file()?;
        *file.length.lock() = target.len() as u64;
        *file.symlink.lock() = Some(target.to_owned());
        Ok(())
    }
}
impl Pollable for MemoryNode {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl DirNodeOps for MemoryNode {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut count = 0;
        for (i, (name, entry)) in self
            .inode
            .as_dir()?
            .entries
            .lock()
            .iter()
            .enumerate()
            .skip(offset as usize)
        {
            if !sink.accept(
                &name.0,
                entry.ino,
                entry.get().metadata.lock().node_type,
                i as u64 + 1,
            ) {
                return Ok(count);
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let dir = self.inode.as_dir()?;
        let entries = dir.entries.lock();

        let entry = entries.get(name).ok_or(VfsError::NotFound)?;
        let inode = entry.get();
        let node_type = inode.metadata.lock().node_type;
        self.new_entry(name, node_type, inode)
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = Inode::new(&self.fs, Some(self.inode.ino), node_type, permission);
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        self.new_entry(name, node_type, inode)
    }

    fn link(&self, name: &str, target: &DirEntry) -> VfsResult<DirEntry> {
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        let target = target.downcast::<Self>()?;

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let inode = target.inode.clone();
        let node_type = target.metadata()?.node_type;
        entries.insert(name.into(), InodeRef::new(self.fs.clone(), inode.ino));
        self.new_entry(name, node_type, inode)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        let Some(inode) = entries.get(name).map(InodeRef::get) else {
            return Err(VfsError::NotFound);
        };
        if let NodeContent::Dir(DirContent {
            entries: child_entries,
        }) = &inode.content
        {
            let mut child_entries = child_entries.lock();
            if child_entries.len() > 2 {
                return Err(VfsError::DirectoryNotEmpty);
            }
            child_entries.clear();
        }
        drop(inode);
        entries.remove(name);

        Ok(())
    }

    // TODO: atomicity
    fn rename(&self, src_name: &str, dst_dir: &DirNode, dst_name: &str) -> VfsResult<()> {
        let dst_node = dst_dir.downcast::<Self>()?;
        if let Ok(entry) = dst_dir.lookup(dst_name) {
            let src_entry = self.lookup(src_name)?;
            if entry.inode() == src_entry.inode() {
                return Ok(());
            }
        }

        let src_entry = self
            .inode
            .as_dir()?
            .entries
            .lock()
            .remove(src_name)
            .ok_or(VfsError::NotFound)?;
        dst_node
            .inode
            .as_dir()?
            .entries
            .lock()
            .insert(dst_name.into(), src_entry);
        Ok(())
    }
}

impl Drop for MemoryNode {
    fn drop(&mut self) {
        let is_unlinked_dir = self.inode.metadata.lock().nlink <= 1;
        if is_unlinked_dir && let NodeContent::Dir(dir) = &self.inode.content {
            dir.entries.lock().clear();
        }
        release_inode(&self.fs, &self.inode, 0);
    }
}
