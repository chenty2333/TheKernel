use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    borrow::Borrow,
    cmp::Ordering,
    mem,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    prune_dead_cached_file_registry_entries_for_inode, remove_cached_file_registry_entry,
    with_sync_and_invalidate_cached_file_pages,
};
use axfs_ng_vfs::{
    AnonymousOptions, CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode,
    DirNodeOps, ExportHandle, ExportHandleMode, FileNode, FileNodeOps, Filesystem, FilesystemOps,
    Metadata, MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps, NodePermission, NodeType,
    NodeUserData, Reference, RenameRequest, StatFs, UnlinkRequest, VfsError, VfsResult,
    WeakDirEntry, XattrProvider, XattrSetMode, path::MAX_NAME_LEN,
};
use axhal::{mem::total_ram_size, time::wall_time};
use axpoll::{IoEvents, Pollable};
#[cfg(not(test))]
use axsync::Mutex;
use hashbrown::{HashMap, HashSet};
use kspin::SpinNoIrq;
use memory_addr::PAGE_SIZE_4K;
#[cfg(test)]
use spin::Mutex;

const TMPFS_BLOCK_SIZE: u64 = PAGE_SIZE_4K as u64;
const STAT_BLOCK_UNIT: u64 = 512;
const MIB: u64 = 1024 * 1024;
const DEFAULT_TMPFS_MIN_BYTES: u64 = 16 * MIB;
const DEFAULT_TMPFS_MAX_BYTES: u64 = 256 * MIB;
/// Until tmpfs inode metadata participates in the kernel's global memory
/// accounting, keep the live identity registry explicitly bounded.  The
/// backing map is reserved before any inode or namespace object is published.
const MAX_TMPFS_INODES: usize = 65_536;
const TMPFS_XATTR_SIZE_MAX: usize = 65_536;

fn default_tmpfs_capacity_bytes() -> u64 {
    let ram = total_ram_size() as u64;
    let max = DEFAULT_TMPFS_MAX_BYTES.min(ram.max(TMPFS_BLOCK_SIZE));
    let min = DEFAULT_TMPFS_MIN_BYTES.min(max);
    (ram / 4).max(min).min(max)
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct FileName(String);

fn try_owned(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    Ok(result)
}

fn try_owned_bytes(value: &[u8]) -> VfsResult<Vec<u8>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.extend_from_slice(value);
    Ok(result)
}

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
    namespace: Mutex<()>,
    inodes: Mutex<HashMap<u64, Arc<Inode>>>,
    next_inode: AtomicU64,
    // `FilesystemInner::drop` may invoke `unmount` without a current task.
    // Keep the final root ownership move independent of axsync's sleeping
    // mutex; clones and destruction still happen outside this guard.
    root: SpinNoIrq<Option<DirEntry>>,
    capacity_pages: Option<u64>,
    allocated_pages: Mutex<u64>,
}

impl MemoryFs {
    /// Creates a new empty memory filesystem.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> VfsResult<Filesystem> {
        Self::new_with_permission(NodePermission::from_bits_truncate(0o755))
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_permission(permission: NodePermission) -> VfsResult<Filesystem> {
        Self::new_with_permission_and_capacity(permission, None)
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_capacity(capacity_bytes: Option<u64>) -> VfsResult<Filesystem> {
        Self::new_with_permission_and_capacity(
            NodePermission::from_bits_truncate(0o755),
            capacity_bytes,
        )
    }

    #[allow(clippy::new_ret_no_self)]
    pub fn new_with_permission_and_capacity(
        permission: NodePermission,
        capacity_bytes: Option<u64>,
    ) -> VfsResult<Filesystem> {
        let capacity_bytes = capacity_bytes.unwrap_or_else(default_tmpfs_capacity_bytes);
        let fs = Arc::try_new(Self {
            namespace: Mutex::new(()),
            inodes: Mutex::new(HashMap::new()),
            next_inode: AtomicU64::new(1),
            root: SpinNoIrq::new(None),
            capacity_pages: Some(capacity_bytes.div_ceil(TMPFS_BLOCK_SIZE)),
            allocated_pages: Mutex::new(0),
        })
        .map_err(|_| VfsError::NoMemory)?;
        // Allocate the generic wrapper before installing the backend's root
        // self-reference.  If a later root allocation fails, dropping this
        // local wrapper calls `unmount` and cannot strand an Arc cycle.
        let filesystem = Filesystem::try_new(fs.clone())?;
        let root_ino = fs.try_reserve_inode_number()?;
        let root_inode =
            Inode::try_new_unpublished(&fs, root_ino, None, NodeType::Directory, permission)?;
        let root = MemoryNode::try_new_entry(
            fs.clone(),
            root_inode.clone(),
            NodeType::Directory,
            Reference::root(),
        )?;
        fs.publish_inode(root_inode)?;
        *fs.root.lock() = Some(root);
        Ok(filesystem)
    }

    fn get(&self, ino: u64) -> Option<Arc<Inode>> {
        self.inodes.lock().get(&ino).cloned()
    }

    /// Reserves both one backing-map slot and a non-reusable inode number.
    /// Callers hold `namespace` across this admission and the matching
    /// `publish_inode`, so no other creator can consume the reserved capacity.
    fn try_reserve_inode_number(&self) -> VfsResult<u64> {
        let mut inodes = self.inodes.lock();
        if inodes.len() >= MAX_TMPFS_INODES {
            return Err(VfsError::NoMemory);
        }
        inodes.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        self.next_inode
            .try_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| VfsError::StorageFull)
    }

    fn publish_inode(&self, inode: Arc<Inode>) -> VfsResult<()> {
        let mut inodes = self.inodes.lock();
        if inodes.contains_key(&inode.ino) {
            return Err(VfsError::Io);
        }
        // `try_reserve_inode_number` admitted this insertion while namespace
        // serialization prevented a competing creator from consuming it.
        inodes.insert(inode.ino, inode);
        Ok(())
    }

    fn reserve_pages(&self, pages: u64) -> AxResult<()> {
        if pages == 0 {
            return Ok(());
        }

        let mut allocated = self.allocated_pages.lock();
        if let Some(capacity) = self.capacity_pages {
            let Some(next) = allocated.checked_add(pages) else {
                return Err(AxError::StorageFull);
            };
            if next > capacity {
                return Err(AxError::StorageFull);
            }
            *allocated = next;
        } else {
            *allocated = allocated.saturating_add(pages);
        }
        Ok(())
    }

    fn reserve_pages_vfs(&self, pages: u64) -> VfsResult<()> {
        self.reserve_pages(pages).map_err(|err| match err {
            AxError::StorageFull => VfsError::StorageFull,
            _ => VfsError::InvalidInput,
        })
    }

    fn release_pages(&self, pages: u64) {
        if pages == 0 {
            return;
        }
        let mut allocated = self.allocated_pages.lock();
        *allocated = allocated.saturating_sub(pages);
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
        let allocated = *self.allocated_pages.lock();
        let total = self
            .capacity_pages
            .unwrap_or_else(|| default_tmpfs_capacity_bytes() / TMPFS_BLOCK_SIZE)
            .max(allocated);
        let free = total.saturating_sub(allocated);
        Ok(StatFs {
            fs_type: 0x01021994,
            block_size: TMPFS_BLOCK_SIZE as u32,
            blocks: total,
            blocks_free: free,
            blocks_available: free,
            file_count: 0,
            free_file_count: 0,
            name_length: MAX_NAME_LEN as _,
            fragment_size: TMPFS_BLOCK_SIZE as u32,
            mount_flags: 0,
        })
    }

    fn enumerate_inodes(&self, visitor: &mut axfs_ng_vfs::InodeVisitor<'_>) -> VfsResult<()> {
        // Never acquire an inode's metadata lock while holding the registry
        // lock: unlink finalization takes these locks in the opposite order.
        // The registry is explicitly bounded, so snapshotting Arc handles is
        // bounded as well and keeps quota callbacks out of tmpfs lock domains.
        let inodes = {
            let registry = self.inodes.lock();
            let mut inodes = Vec::new();
            inodes.try_reserve_exact(registry.len()).map_err(|_| VfsError::NoMemory)?;
            inodes.extend(registry.values().cloned());
            inodes
        };
        for inode in inodes {
            visitor(inode.snapshot_metadata())?;
        }
        Ok(())
    }
    fn encode_export_handle(
        &self,
        entry: &DirEntry,
        mode: ExportHandleMode,
    ) -> VfsResult<ExportHandle> {
        let node = entry.downcast::<MemoryNode>()?;
        if !core::ptr::eq(self, node.fs.as_ref()) {
            return Err(VfsError::CrossesDevices);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(16)
            .map_err(|_| VfsError::NoMemory)?;
        bytes.extend_from_slice(&node.inode.ino.to_ne_bytes());
        bytes.extend_from_slice(&0u64.to_ne_bytes());
        let _ = mode;
        Ok(ExportHandle {
            handle_type: 1,
            bytes,
        })
    }

    fn decode_export_handle(&self, handle_type: i32, bytes: &[u8]) -> VfsResult<DirEntry> {
        if handle_type != 1 || bytes.len() != 16 {
            return Err(VfsError::NotFound);
        }
        let inode_number =
            u64::from_ne_bytes(bytes[..8].try_into().map_err(|_| VfsError::NotFound)?);
        let generation = u64::from_ne_bytes(bytes[8..].try_into().map_err(|_| VfsError::NotFound)?);
        if generation != 0 {
            return Err(VfsError::NotFound);
        }
        let inode = self.get(inode_number).ok_or(VfsError::NotFound)?;
        let node_type = inode.metadata.lock().node_type;
        // This is an anonymous VFS alias, not a namespace link: it retains
        // the exact live inode generation without changing nlink or inventing
        // a pathname. Once unlink drops the final inode registry reference,
        // future handle decoding returns ESTALE at the syscall boundary.
        let root = self.root_dir();
        let fs = root.downcast::<MemoryNode>()?.fs.clone();
        MemoryNode::try_new_entry(fs, inode, node_type, Reference::anonymous())
    }

    fn export_handle_is_descendant(
        &self,
        ancestor: &DirEntry,
        descendant: &DirEntry,
    ) -> VfsResult<bool> {
        let target = descendant.downcast::<MemoryNode>()?;
        let ancestor = ancestor.downcast::<MemoryNode>()?;
        if !core::ptr::eq(self, ancestor.fs.as_ref()) || !core::ptr::eq(self, target.fs.as_ref()) {
            return Err(VfsError::CrossesDevices);
        }

        // The decoded export alias has Reference::anonymous(), so walk the
        // namespace's stable inode-parent graph instead of its reference.
        let _namespace = self.namespace.lock();
        let inode_count = self.inodes.lock().len();
        let mut pending = Vec::new();
        let mut visited = HashSet::new();
        visited
            .try_reserve(inode_count)
            .map_err(|_| VfsError::NoMemory)?;
        pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        visited.insert(ancestor.inode.ino);
        pending.push(ancestor.inode.clone());
        while let Some(inode) = pending.pop() {
            if Arc::ptr_eq(&inode, &target.inode) {
                return Ok(true);
            }
            let Ok(directory) = inode.as_dir() else {
                continue;
            };
            for (name, child) in directory.entries.lock().iter() {
                if name.0 == "." || name.0 == ".." {
                    continue;
                }
                let child = child.get().ok_or(VfsError::Io)?;
                if visited.insert(child.ino) {
                    pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    pending.push(child);
                }
            }
        }
        Ok(false)
    }

    fn unmount(&self) {
        let root = self.root.lock().take();
        drop(root);
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
            file.clear_storage(fs);
        }
        inodes.remove(&ino);
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
    pages: Mutex<HashMap<u64, Box<[u8; PAGE_SIZE_4K]>>>,
    allocated_pages: Mutex<HashSet<u64>>,
}

impl FileContent {
    fn set_len(&self, fs: &MemoryFs, len: u64) {
        let old_len = *self.length.lock();
        *self.length.lock() = len;
        let last_page = len.div_ceil(TMPFS_BLOCK_SIZE);
        let old_last_page = old_len.div_ceil(TMPFS_BLOCK_SIZE);
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
        if last_page < old_last_page {
            let mut allocated = self.allocated_pages.lock();
            let released = allocated.iter().filter(|page| **page >= last_page).count() as u64;
            allocated.retain(|page| *page < last_page);
            drop(allocated);
            fs.release_pages(released);
        }
    }

    fn clear_storage(&self, fs: &MemoryFs) {
        *self.length.lock() = 0;
        *self.symlink.lock() = None;
        self.pages.lock().clear();
        let mut allocated = self.allocated_pages.lock();
        let released = allocated.len() as u64;
        allocated.clear();
        drop(allocated);
        fs.release_pages(released);
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = *self.length.lock();
        if offset >= len {
            return Ok(0);
        }
        let total = buf.len().min((len - offset) as usize);
        let pages = self.pages.lock();
        let mut done = 0;
        while done < total {
            let pos = offset + done as u64;
            let page = pos / TMPFS_BLOCK_SIZE;
            let page_off = (pos % TMPFS_BLOCK_SIZE) as usize;
            let chunk = (total - done).min(PAGE_SIZE_4K - page_off);
            let dst = &mut buf[done..done + chunk];
            if let Some(src) = pages.get(&page) {
                dst.copy_from_slice(&src[page_off..page_off + chunk]);
            } else {
                dst.fill(0);
            }
            done += chunk;
        }
        Ok(total)
    }

    fn write_at(&self, fs: &MemoryFs, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        let mut pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        let Some((start, end_page)) = page_range(offset, buf.len() as u64) else {
            return Ok(0);
        };
        let new_pages = (start..end_page)
            .filter(|page| !allocated.contains(page))
            .count() as u64;
        let missing_data_pages = (start..end_page)
            .filter(|page| !pages.contains_key(page))
            .count();
        fs.reserve_pages_vfs(new_pages)?;
        let preparation = (|| {
            let mut prepared = Vec::new();
            prepared
                .try_reserve_exact(missing_data_pages)
                .map_err(|_| VfsError::NoMemory)?;
            for page in start..end_page {
                if !pages.contains_key(&page) {
                    let data = Box::try_new([0; PAGE_SIZE_4K]).map_err(|_| VfsError::NoMemory)?;
                    prepared.push((page, data));
                }
            }
            pages
                .try_reserve(prepared.len())
                .map_err(|_| VfsError::NoMemory)?;
            allocated
                .try_reserve(usize::try_from(new_pages).map_err(|_| VfsError::NoMemory)?)
                .map_err(|_| VfsError::NoMemory)?;
            Ok::<_, VfsError>(prepared)
        })();
        let prepared = match preparation {
            Ok(prepared) => prepared,
            Err(err) => {
                fs.release_pages(new_pages);
                return Err(err);
            }
        };
        for (page, data) in prepared {
            pages.insert(page, data);
        }
        let current_len = *self.length.lock();
        if end > current_len {
            *self.length.lock() = end;
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let page = pos / TMPFS_BLOCK_SIZE;
            let page_off = (pos % TMPFS_BLOCK_SIZE) as usize;
            let chunk = (buf.len() - done).min(PAGE_SIZE_4K - page_off);
            let dst = pages.get_mut(&page).ok_or(VfsError::Io)?;
            dst[page_off..page_off + chunk].copy_from_slice(&buf[done..done + chunk]);
            allocated.insert(page);
            done += chunk;
        }
        Ok(buf.len())
    }

    fn reserve_range(&self, fs: &MemoryFs, offset: u64, len: u64) -> AxResult<()> {
        let Some((start, end)) = page_range(offset, len) else {
            return Ok(());
        };
        // Reject an impossible reservation before walking the requested page
        // range. A sparse file may have a near-u64::MAX length, but a tmpfs
        // operation must not spend time proportional to that unbacked hole.
        if fs
            .capacity_pages
            .is_some_and(|capacity| end.saturating_sub(start) > capacity)
        {
            return Err(AxError::StorageFull);
        }
        let mut allocated = self.allocated_pages.lock();
        let new_pages = (start..end)
            .filter(|page| !allocated.contains(page))
            .count() as u64;
        fs.reserve_pages(new_pages)?;
        if allocated
            .try_reserve(usize::try_from(new_pages).map_err(|_| AxError::NoMemory)?)
            .is_err()
        {
            fs.release_pages(new_pages);
            return Err(AxError::NoMemory);
        }
        for page in start..end {
            allocated.insert(page);
        }
        Ok(())
    }

    fn punch_hole(&self, fs: &MemoryFs, offset: u64, len: u64) {
        let Some((start, end)) = full_page_range(offset, len) else {
            return;
        };
        let mut pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        let mut released = 0;
        // Walk only materialized state (bounded by tmpfs capacity), not every
        // page in a potentially enormous sparse user range.
        pages.retain(|page, _| *page < start || *page >= end);
        allocated.retain(|page| {
            let keep = *page < start || *page >= end;
            if !keep {
                released += 1;
            }
            keep
        });
        drop(allocated);
        fs.release_pages(released);
    }

    fn collapse_range(&self, fs: &MemoryFs, offset: u64, len: u64) -> AxResult<()> {
        let Some((start, end)) = full_page_range(offset, len) else {
            return Ok(());
        };
        let delta = end - start;
        let mut data_pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        let mut remapped_data = HashMap::new();
        remapped_data
            .try_reserve(data_pages.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut remapped_allocated = HashSet::new();
        remapped_allocated
            .try_reserve(allocated.len())
            .map_err(|_| AxError::NoMemory)?;
        let current = mem::take(&mut *data_pages);
        for (page, data) in current {
            if page < start {
                remapped_data.insert(page, data);
            } else if page >= end {
                remapped_data.insert(page - delta, data);
            }
        }
        *data_pages = remapped_data;
        let mut released = 0;
        for page in mem::take(&mut *allocated) {
            if page < start {
                remapped_allocated.insert(page);
            } else if page < end {
                released += 1;
            } else {
                remapped_allocated.insert(page - delta);
            }
        }
        *allocated = remapped_allocated;
        drop(allocated);
        drop(data_pages);
        fs.release_pages(released);
        Ok(())
    }

    fn insert_range(&self, offset: u64, len: u64) -> AxResult<()> {
        let Some((start, end)) = full_page_range(offset, len) else {
            return Ok(());
        };
        let delta = end - start;
        let mut data_pages = self.pages.lock();
        let mut allocated = self.allocated_pages.lock();
        if data_pages
            .keys()
            .chain(allocated.iter())
            .any(|page| *page >= start && page.checked_add(delta).is_none())
        {
            return Err(AxError::InvalidInput);
        }
        let mut remapped_data = HashMap::new();
        remapped_data
            .try_reserve(data_pages.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut remapped_allocated = HashSet::new();
        remapped_allocated
            .try_reserve(allocated.len())
            .map_err(|_| AxError::NoMemory)?;
        let current = mem::take(&mut *data_pages);
        for (page, data) in current {
            if page < start {
                remapped_data.insert(page, data);
            } else {
                remapped_data.insert(page + delta, data);
            }
        }
        *data_pages = remapped_data;
        for page in mem::take(&mut *allocated) {
            remapped_allocated.insert(if page < start { page } else { page + delta });
        }
        *allocated = remapped_allocated;
        drop(allocated);
        drop(data_pages);
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.allocated_pages.lock().len() as u64 * (TMPFS_BLOCK_SIZE / STAT_BLOCK_UNIT)
    }

    fn seek_data_or_hole(&self, size: u64, offset: u64, seek_hole: bool) -> AxResult<u64> {
        if offset >= size {
            return Err(AxError::from(LinuxError::ENXIO));
        }

        let allocated = self.allocated_pages.lock();
        let page = offset / TMPFS_BLOCK_SIZE;
        let last_page = size.div_ceil(TMPFS_BLOCK_SIZE);

        if !seek_hole {
            if allocated.contains(&page) {
                return Ok(offset);
            }
            return allocated
                .iter()
                .copied()
                .filter(|candidate| *candidate > page && *candidate < last_page)
                .min()
                .map(|candidate| candidate.saturating_mul(TMPFS_BLOCK_SIZE).min(size))
                .ok_or_else(|| AxError::from(LinuxError::ENXIO));
        }

        if !allocated.contains(&page) {
            return Ok(offset);
        }
        let mut hole = page.saturating_add(1);
        // Every successful iteration consumes one accounted page, so this is
        // bounded by the filesystem capacity even for an enormous sparse file.
        while hole < last_page && allocated.contains(&hole) {
            hole = hole.saturating_add(1);
        }
        Ok(hole.saturating_mul(TMPFS_BLOCK_SIZE).min(size))
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

fn file_content_for(loc: &axfs_ng_vfs::Location) -> Option<(Arc<MemoryFs>, Arc<Inode>)> {
    let node = loc.entry().downcast::<MemoryNode>().ok()?;
    node.inode.as_file().ok()?;
    Some((node.fs.clone(), node.inode.clone()))
}

/// Reports whether this location exposes tmpfs range-mutation primitives.
/// Upper layers use this to reject unsupported operations before emulation can
/// modify file contents.
pub fn supports_fallocate_range(loc: &axfs_ng_vfs::Location) -> bool {
    file_content_for(loc).is_some()
}

pub fn reserve_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
    extend: bool,
) -> Option<AxResult<()>> {
    let (fs, inode) = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(with_sync_and_invalidate_cached_file_pages(loc, || {
        if extend {
            let Some(end) = offset.checked_add(len) else {
                return Err(AxError::InvalidInput);
            };
            let extend_to = (end > *file.length.lock()).then_some(end);
            let result = file.reserve_range(&fs, offset, len);
            if result.is_ok()
                && let Some(end) = extend_to
            {
                file.set_len(&fs, end);
            }
            return result;
        }
        file.reserve_range(&fs, offset, len)
    }))
}

pub fn punch_hole_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let (fs, inode) = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(with_sync_and_invalidate_cached_file_pages(loc, || {
        file.punch_hole(&fs, offset, len);
        Ok(())
    }))
}

pub fn collapse_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let (fs, inode) = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(with_sync_and_invalidate_cached_file_pages(loc, || {
        file.collapse_range(&fs, offset, len)
    }))
}

pub fn insert_fallocate_range(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    len: u64,
) -> Option<AxResult<()>> {
    let (_, inode) = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(with_sync_and_invalidate_cached_file_pages(loc, || {
        file.insert_range(offset, len)
    }))
}

pub fn seek_data_or_hole(
    loc: &axfs_ng_vfs::Location,
    offset: u64,
    seek_hole: bool,
) -> Option<AxResult<u64>> {
    let (_, inode) = file_content_for(loc)?;
    let file = inode.as_file().ok()?;
    Some(file.seek_data_or_hole(*file.length.lock(), offset, seek_hole))
}

struct DirContent {
    entries: Mutex<HashMap<FileName, InodeRef>>,
    namespace_epoch: AtomicU64,
}

impl DirContent {
    fn try_new() -> VfsResult<Self> {
        let mut entries = HashMap::new();
        entries.try_reserve(2).map_err(|_| VfsError::NoMemory)?;
        Ok(Self {
            entries: Mutex::new(entries),
            namespace_epoch: AtomicU64::new(0),
        })
    }
}

enum NodeContent {
    File(FileContent),
    Dir(DirContent),
}

struct Inode {
    ino: u64,
    metadata: Mutex<Metadata>,
    anonymous_linkable: Mutex<bool>,
    content: NodeContent,
    xattrs: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
    user_data: NodeUserData,
}

impl Inode {
    fn snapshot_metadata(&self) -> Metadata {
        let mut metadata = self.metadata.lock().clone();
        match &self.content {
            NodeContent::File(content) => {
                metadata.size = *content.length.lock();
                metadata.block_size = TMPFS_BLOCK_SIZE;
                metadata.blocks = content.blocks();
            }
            NodeContent::Dir(dir) => metadata.size = dir.entries.lock().len() as u64,
        }
        metadata
    }

    /// Builds a complete inode without publishing it in the filesystem's live
    /// identity map. Directory dot entries and the per-inode xattr ownership
    /// are admitted here, so later registry/namespace insertion cannot fail.
    fn try_new_unpublished(
        fs: &Arc<MemoryFs>,
        ino: u64,
        parent: Option<&Arc<Inode>>,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<Arc<Inode>> {
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 0,
            mode: permission,
            node_type,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: 0,
            block_size: TMPFS_BLOCK_SIZE,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now.into(),
            btime: now.into(),
            mtime: now.into(),
            ctime: now.into(),
        };
        let content = match node_type {
            NodeType::Directory => NodeContent::Dir(DirContent::try_new()?),
            _ => NodeContent::File(FileContent::default()),
        };
        let dot_names = if node_type == NodeType::Directory {
            Some((FileName(try_owned(".")?), FileName(try_owned("..")?)))
        } else {
            None
        };
        let xattrs = Arc::try_new(Mutex::new(HashMap::new())).map_err(|_| VfsError::NoMemory)?;
        let result = Arc::try_new(Self {
            ino,
            metadata: Mutex::new(metadata),
            anonymous_linkable: Mutex::new(false),
            content,
            xattrs,
            user_data: NodeUserData::new(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        if let (NodeContent::Dir(dir), Some((dot_name, dotdot_name))) = (&result.content, dot_names)
        {
            let dot = InodeRef::try_new_named(fs, &result)?;
            let dotdot = InodeRef::try_new_named(fs, parent.unwrap_or(&result))?;
            let mut entries = dir.entries.lock();
            // `DirContent::try_new` reserved both nodes before this inode was
            // visible, so these inserts cannot allocate.
            entries.insert(dot_name, dot);
            entries.insert(dotdot_name, dotdot);
        }
        Ok(result)
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
    fs: Weak<MemoryFs>,
    ino: u64,
}

impl InodeRef {
    /// Prepares a link-count reference directly against an owned inode. This
    /// works before that inode is in the live registry and avoids an
    /// allocation-or-panic lookup during namespace publication.
    fn try_new_named(fs: &Arc<MemoryFs>, inode: &Arc<Inode>) -> VfsResult<Self> {
        let mut metadata = inode.metadata.lock();
        metadata.nlink = metadata.nlink.checked_add(1).ok_or(VfsError::StorageFull)?;
        drop(metadata);
        Ok(Self {
            fs: Arc::downgrade(fs),
            ino: inode.ino,
        })
    }

    fn new_link(
        fs: &Arc<MemoryFs>,
        inode: &Arc<Inode>,
        ctime: core::time::Duration,
    ) -> VfsResult<Self> {
        let mut metadata = inode.metadata.lock();
        if metadata.nlink == 0 {
            let mut anonymous_linkable = inode.anonymous_linkable.lock();
            if !*anonymous_linkable {
                return Err(VfsError::NotFound);
            }
            *anonymous_linkable = false;
        }
        metadata.nlink = metadata.nlink.checked_add(1).ok_or(VfsError::StorageFull)?;
        metadata.ctime = ctime.into();
        Ok(Self {
            fs: Arc::downgrade(fs),
            ino: inode.ino,
        })
    }

    fn get(&self) -> Option<Arc<Inode>> {
        self.fs.upgrade()?.get(self.ino)
    }
}

impl Drop for InodeRef {
    fn drop(&mut self) {
        let Some(fs) = self.fs.upgrade() else {
            return;
        };
        let Some(inode) = fs.get(self.ino) else {
            return;
        };
        release_inode(&fs, &inode, 1);
    }
}

struct MemoryNode {
    fs: Arc<MemoryFs>,
    inode: Arc<Inode>,
    this: Mutex<Option<WeakDirEntry>>,
}

impl MemoryNode {
    fn try_new(fs: Arc<MemoryFs>, inode: Arc<Inode>) -> VfsResult<Arc<Self>> {
        Arc::try_new(Self {
            fs,
            inode,
            this: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn bind(&self, this: WeakDirEntry) {
        *self.this.lock() = Some(this);
    }

    fn try_new_entry(
        fs: Arc<MemoryFs>,
        inode: Arc<Inode>,
        node_type: NodeType,
        reference: Reference,
    ) -> VfsResult<DirEntry> {
        let node = Self::try_new(fs, inode)?;
        if node_type == NodeType::Directory {
            let entry = DirEntry::try_new_dir(DirNode::new(node.clone()), reference)?;
            node.bind(entry.downgrade());
            Ok(entry)
        } else {
            DirEntry::try_new_file(FileNode::new(node), node_type, reference)
        }
    }

    fn new_entry(&self, name: &str, node_type: NodeType, inode: Arc<Inode>) -> VfsResult<DirEntry> {
        let reference = Reference::new(
            self.this.lock().as_ref().and_then(WeakDirEntry::upgrade),
            try_owned(name)?,
        );
        Self::try_new_entry(self.fs.clone(), inode, node_type, reference)
    }

    fn matches_expected(&self, expected: &DirEntry, actual: &Arc<Inode>) -> bool {
        expected.downcast::<Self>().is_ok_and(|expected| {
            Arc::ptr_eq(&self.fs, &expected.fs) && Arc::ptr_eq(actual, &expected.inode)
        })
    }

    fn touch_directory(inode: &Inode, now: core::time::Duration) {
        let mut metadata = inode.metadata.lock();
        metadata.mtime = now.into();
        metadata.ctime = now.into();
    }

    fn touch_renamed_inodes(source: &Inode, victim: Option<&Inode>, now: core::time::Duration) {
        source.metadata.lock().ctime = now.into();
        if let Some(victim) = victim {
            victim.metadata.lock().ctime = now.into();
        }
    }

    fn validate_rename(
        &self,
        request: RenameRequest<'_>,
        src_entries: &HashMap<FileName, InodeRef>,
        dst_entries: &HashMap<FileName, InodeRef>,
    ) -> VfsResult<(Arc<Inode>, Option<Arc<Inode>>, bool)> {
        let src_inode = src_entries
            .get(request.src_name)
            .and_then(InodeRef::get)
            .ok_or(VfsError::NotFound)?;
        if !self.matches_expected(request.src, &src_inode) {
            return Err(VfsError::NotFound);
        }

        let dst_inode = dst_entries.get(request.dst_name).and_then(InodeRef::get);
        match (request.dst, dst_inode.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(actual)) if self.matches_expected(expected, actual) => {}
            _ => return Err(VfsError::NotFound),
        }

        let same_object = dst_inode
            .as_ref()
            .is_some_and(|dst| Arc::ptr_eq(&src_inode, dst));
        if same_object {
            return Ok((src_inode, dst_inode, true));
        }

        let src_is_dir = src_inode.metadata.lock().node_type == NodeType::Directory;
        let dst_is_dir = dst_inode
            .as_ref()
            .is_some_and(|inode| inode.metadata.lock().node_type == NodeType::Directory);
        match (src_is_dir, dst_inode.is_some(), dst_is_dir) {
            (true, true, false) => return Err(VfsError::NotADirectory),
            (false, true, true) => return Err(VfsError::IsADirectory),
            _ => {}
        }
        if let Some(dst_inode) = dst_inode.as_ref()
            && let NodeContent::Dir(dir) = &dst_inode.content
            && dir.entries.lock().len() > 2
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        Ok((src_inode, dst_inode, false))
    }

    fn retire_replaced_directory(inode: Option<&Arc<Inode>>) {
        if let Some(inode) = inode
            && let NodeContent::Dir(dir) = &inode.content
        {
            dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
            dir.entries.lock().clear();
        }
    }

    fn new_anonymous_entry(&self, node_type: NodeType, inode: Arc<Inode>) -> VfsResult<DirEntry> {
        if node_type == NodeType::Directory {
            return Err(VfsError::OperationNotSupported);
        }
        DirEntry::try_new_file(
            FileNode::new(Self::try_new(self.fs.clone(), inode)?),
            node_type,
            Reference::anonymous(),
        )
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
        if let Some(project_id) = update.project_id {
            metadata.project_id = project_id;
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
            metadata.ctime = wall_time().into();
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

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.inode.user_data)
    }

    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
}

impl XattrProvider for MemoryNode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        let xattrs = self.inode.xattrs.lock();
        let value = xattrs
            .get(name)
            .ok_or_else(|| VfsError::from(LinuxError::ENODATA))?;
        try_owned_bytes(value)
    }

    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let xattrs = self.inode.xattrs.lock();
        let required = xattrs.keys().try_fold(0usize, |required, name| {
            required
                .checked_add(name.len())
                .and_then(|required| required.checked_add(1))
                .ok_or(VfsError::NoMemory)
        })?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(required)
            .map_err(|_| VfsError::NoMemory)?;
        for name in xattrs.keys() {
            result.extend_from_slice(name);
            result.push(0);
        }
        Ok(result)
    }

    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        // Admit allocations independent of the map before taking the inode
        // lock. The existence decision itself remains serialized with
        // publication.
        let owned_name = try_owned_bytes(name)?;
        let owned_value = try_owned_bytes(value)?;
        let replacement_size = name
            .len()
            .checked_add(1)
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| VfsError::from(LinuxError::ENOSPC))?;

        let mut xattrs = self.inode.xattrs.lock();
        let exists = xattrs.contains_key(name);
        match (mode, exists) {
            (XattrSetMode::Create, true) => return Err(LinuxError::EEXIST.into()),
            (XattrSetMode::Replace, false) => return Err(LinuxError::ENODATA.into()),
            _ => {}
        }

        let used_without_replaced = xattrs.iter().try_fold(0usize, |used, (key, old)| {
            if key.as_slice() == name {
                return Ok(used);
            }
            key.len()
                .checked_add(1)
                .and_then(|size| size.checked_add(old.len()))
                .and_then(|size| used.checked_add(size))
                .ok_or_else(|| VfsError::from(LinuxError::ENOSPC))
        })?;
        if used_without_replaced
            .checked_add(replacement_size)
            .is_none_or(|used| used > TMPFS_XATTR_SIZE_MAX)
        {
            return Err(LinuxError::ENOSPC.into());
        }

        let retired = if let Some(current) = xattrs.get_mut(name) {
            Some(mem::replace(current, owned_value))
        } else {
            xattrs.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            xattrs.insert(owned_name, owned_value);
            None
        };
        drop(xattrs);
        drop(retired);
        Ok(())
    }

    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        let retired = self
            .inode
            .xattrs
            .lock()
            .remove_entry(name)
            .ok_or_else(|| VfsError::from(LinuxError::ENODATA))?;
        drop(retired);
        Ok(())
    }
}

impl FileNodeOps for MemoryNode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let file = self.inode.as_file()?;
        if let Some(symlink) = file.symlink.lock().as_ref() {
            let Ok(offset) = usize::try_from(offset) else {
                return Ok(0);
            };
            let Some(remaining) = symlink.as_bytes().get(offset..) else {
                return Ok(0);
            };
            let len = buf.len().min(remaining.len());
            buf[..len].copy_from_slice(&remaining[..len]);
            return Ok(len);
        }
        file.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let file = self.inode.as_file()?;
        file.write_at(&self.fs, buf, offset)
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let file = self.inode.as_file()?;
        if buf.is_empty() {
            return Ok((0, *file.length.lock()));
        }
        let offset = *file.length.lock();
        let written = file.write_at(&self.fs, buf, offset)?;
        Ok((written, offset + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let file = self.inode.as_file()?;
        file.set_len(&self.fs, len);
        Ok(())
    }

    fn set_len_failure_is_atomic(&self) -> bool {
        // The only fallible step is the file-kind check above, before mutation.
        true
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        let file = self.inode.as_file()?;
        let target = try_owned(target)?;
        *file.length.lock() = target.len() as u64;
        *file.symlink.lock() = Some(target);
        Ok(())
    }
}
impl Pollable for MemoryNode {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl DirNodeOps for MemoryNode {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::Fifo
                | NodeType::CharacterDevice
                | NodeType::Directory
                | NodeType::BlockDevice
                | NodeType::RegularFile
                | NodeType::Socket
        )
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
        self.inode
            .as_dir()
            .map_or(0, |dir| dir.namespace_epoch.load(AtomicOrdering::Acquire))
    }

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
            let inode = entry.get().ok_or(VfsError::Io)?;
            if !sink.accept(
                &name.0,
                entry.ino,
                inode.metadata.lock().node_type,
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
        let inode = entry.get().ok_or(VfsError::NotFound)?;
        let node_type = inode.metadata.lock().node_type;
        self.new_entry(name, node_type, inode)
    }

    fn create_named(
        &self,
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        let _namespace = self.fs.namespace.lock();
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        if let Some(existing) = entries.get(name) {
            if disposition == CreateDisposition::Exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let inode = existing.get().ok_or(VfsError::NotFound)?;
            let node_type = inode.metadata.lock().node_type;
            return Ok(CreateOutcome {
                entry: self.new_entry(name, node_type, inode)?,
                created: false,
            });
        }
        if !self.supports_named_create(options.node_type) {
            // A symbolic link must be initialized through `create_symlink`,
            // and unknown inode types have no tmpfs representation. Keep both
            // classes out of generic named publication even for direct backend
            // callers that bypass the VFS capability seam.
            return Err(VfsError::OperationNotSupported);
        }
        if options.rdev.is_some()
            && !matches!(
                options.node_type,
                NodeType::CharacterDevice | NodeType::BlockDevice
            )
        {
            return Err(VfsError::InvalidInput);
        }
        entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let cache_name = FileName(try_owned(name)?);
        let ino = self.fs.try_reserve_inode_number()?;
        let inode = Inode::try_new_unpublished(
            &self.fs,
            ino,
            Some(&self.inode),
            options.node_type,
            options.permission,
        )?;
        {
            let mut metadata = inode.metadata.lock();
            if let Some((uid, gid)) = options.owner {
                metadata.uid = uid;
                metadata.gid = gid;
            }
            if let Some(rdev) = options.rdev {
                metadata.rdev = rdev;
            }
        }
        let inode_ref = InodeRef::try_new_named(&self.fs, &inode)?;
        let entry = self.new_entry(name, options.node_type, inode.clone())?;
        options.install_initial_data(&entry)?;
        self.fs.publish_inode(inode)?;
        dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        entries.insert(cache_name, inode_ref);
        let now = wall_time();
        drop(entries);
        Self::touch_directory(&self.inode, now);
        Ok(CreateOutcome {
            entry,
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
        let target = try_owned(target)?;
        let cache_name = FileName(try_owned(name)?);
        let _namespace = self.fs.namespace.lock();
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();
        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;

        let ino = self.fs.try_reserve_inode_number()?;
        let inode = Inode::try_new_unpublished(
            &self.fs,
            ino,
            Some(&self.inode),
            NodeType::Symlink,
            permission,
        )?;
        let file = inode.as_file()?;
        *file.length.lock() = target.len() as u64;
        *file.symlink.lock() = Some(target);
        if let Some((uid, gid)) = user {
            let mut metadata = inode.metadata.lock();
            metadata.uid = uid;
            metadata.gid = gid;
        }
        let inode_ref = InodeRef::try_new_named(&self.fs, &inode)?;
        let entry = self.new_entry(name, NodeType::Symlink, inode.clone())?;
        self.fs.publish_inode(inode)?;
        dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        entries.insert(cache_name, inode_ref);
        let now = wall_time();
        drop(entries);
        Self::touch_directory(&self.inode, now);
        Ok(entry)
    }

    fn create_anonymous(&self, options: &AnonymousOptions) -> VfsResult<DirEntry> {
        let _namespace = self.fs.namespace.lock();
        self.inode.as_dir()?;
        if options.node_type == NodeType::Directory {
            return Err(VfsError::OperationNotSupported);
        }
        let ino = self.fs.try_reserve_inode_number()?;
        let inode = Inode::try_new_unpublished(
            &self.fs,
            ino,
            Some(&self.inode),
            options.node_type,
            options.permission,
        )?;
        if let Some((uid, gid)) = options.user {
            let mut metadata = inode.metadata.lock();
            metadata.uid = uid;
            metadata.gid = gid;
        }
        *inode.anonymous_linkable.lock() = options.linkable;
        let entry = self.new_anonymous_entry(options.node_type, inode.clone())?;
        self.fs.publish_inode(inode)?;
        Ok(entry)
    }

    fn link(&self, name: &str, target: &DirEntry) -> VfsResult<DirEntry> {
        let cache_name = FileName(try_owned(name)?);
        let _namespace = self.fs.namespace.lock();
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        let target = target.downcast::<Self>()?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(VfsError::CrossesDevices);
        }

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let inode = target.inode.clone();
        let node_type = target.metadata()?.node_type;
        if node_type == NodeType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }
        // Admit the dentry before consuming an anonymous inode's one-shot
        // linkability. Once `new_link` succeeds, the reserved namespace slot
        // makes the remaining publication infallible, so an allocation failure
        // leaves O_TMPFILE linkability retryable.
        let entry = self.new_entry(name, node_type, inode)?;
        let now = wall_time();
        let inode_ref = InodeRef::new_link(&self.fs, &target.inode, now)?;
        dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        entries.insert(cache_name, inode_ref);
        drop(entries);
        Self::touch_directory(&self.inode, now);
        Ok(entry)
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let _namespace = self.fs.namespace.lock();
        let dir = self.inode.as_dir()?;
        let mut entries = dir.entries.lock();

        let Some(inode) = entries.get(request.name).and_then(InodeRef::get) else {
            return Err(VfsError::NotFound);
        };
        if request
            .expected
            .is_some_and(|expected| !self.matches_expected(expected, &inode))
        {
            return Err(VfsError::NotFound);
        }
        let node_type = inode.metadata.lock().node_type;
        match (node_type == NodeType::Directory, request.is_dir) {
            (true, false) => return Err(VfsError::IsADirectory),
            (false, true) => return Err(VfsError::NotADirectory),
            _ => {}
        }
        if let NodeContent::Dir(DirContent {
            entries: child_entries,
            namespace_epoch,
        }) = &inode.content
        {
            let mut child_entries = child_entries.lock();
            if child_entries.len() > 2 {
                return Err(VfsError::DirectoryNotEmpty);
            }
            namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
            child_entries.clear();
        }
        dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        entries.remove(request.name);
        let now = wall_time();
        inode.metadata.lock().ctime = now.into();
        drop(entries);
        Self::touch_directory(&self.inode, now);

        Ok(())
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_node = request.dst_dir.downcast::<Self>()?;
        if !Arc::ptr_eq(&self.fs, &dst_node.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let dst_key = FileName(try_owned(request.dst_name)?);
        let _namespace = self.fs.namespace.lock();
        let src_dir = self.inode.as_dir()?;
        let dst_dir = dst_node.inode.as_dir()?;
        let same_parent = Arc::ptr_eq(&self.inode, &dst_node.inode);

        if same_parent {
            let mut entries = src_dir.entries.lock();
            let (src_inode, dst_inode, same_object) =
                self.validate_rename(request, &entries, &entries)?;
            if same_object {
                return Ok(());
            }
            entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            src_dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
            let src_ref = entries.remove(request.src_name).ok_or(VfsError::NotFound)?;
            let old_dst = entries.remove(request.dst_name);
            entries.insert(dst_key, src_ref);
            Self::retire_replaced_directory(dst_inode.as_ref());
            drop(old_dst);
            let now = wall_time();
            Self::touch_renamed_inodes(&src_inode, dst_inode.as_deref(), now);
            drop(src_inode);
            drop(entries);
            Self::touch_directory(&self.inode, now);
            return Ok(());
        }

        let mut src_entries = src_dir.entries.lock();
        let mut dst_entries = dst_dir.entries.lock();
        let (src_inode, dst_inode, same_object) =
            self.validate_rename(request, &src_entries, &dst_entries)?;
        if same_object {
            return Ok(());
        }
        dst_entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;

        let mut new_parent_ref = if src_inode.metadata.lock().node_type == NodeType::Directory {
            Some(InodeRef::try_new_named(&self.fs, &dst_node.inode)?)
        } else {
            None
        };
        let mut child_entries = if new_parent_ref.is_some() {
            Some(src_inode.as_dir()?.entries.lock())
        } else {
            None
        };
        let mut parent_slot = match child_entries.as_mut() {
            Some(entries) => Some(entries.get_mut("..").ok_or(VfsError::Io)?),
            None => None,
        };

        src_dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        dst_dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
        let src_ref = src_entries
            .remove(request.src_name)
            .ok_or(VfsError::NotFound)?;
        let old_dst = dst_entries.remove(request.dst_name);
        if let (Some(entry), Some(parent_ref)) = (parent_slot.as_mut(), new_parent_ref.take()) {
            let old_parent = mem::replace(*entry, parent_ref);
            if let NodeContent::Dir(dir) = &src_inode.content {
                dir.namespace_epoch.fetch_add(1, AtomicOrdering::AcqRel);
            }
            drop(old_parent);
        }
        dst_entries.insert(dst_key, src_ref);
        Self::retire_replaced_directory(dst_inode.as_ref());
        drop(old_dst);
        drop(parent_slot);
        drop(child_entries);
        drop(src_entries);
        drop(dst_entries);
        let now = wall_time();
        Self::touch_renamed_inodes(&src_inode, dst_inode.as_deref(), now);
        Self::touch_directory(&self.inode, now);
        Self::touch_directory(&dst_node.inode, now);
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

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{
        string::{String, ToString},
        sync::Arc,
        vec,
        vec::Vec,
    };
    use std::{sync::Barrier, thread};

    use axerrno::LinuxError;
    use axfs_ng_vfs::{
        AnonymousOptions, CreateDisposition, DeviceId, ExportHandleDecodeMode, ExportHandleMode, InitialNodeData,
        MetadataUpdate, Mountpoint, NamedCreateOptions, NodePermission, NodeType, Timestamp,
        VfsError, XattrSetMode,
    };

    use super::{MemoryFs, TMPFS_XATTR_SIZE_MAX};

    fn regular_file(name: &str) -> axfs_ng_vfs::Location {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        mount
            .root_location()
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap()
    }

    #[test]
    fn xattr_provider_serializes_modes_and_persists_across_hard_links() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let file = root
            .create(
                "xattr-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let alias = root.link("xattr-alias", &file).unwrap();

        assert_eq!(file.get_xattr(b"user.key"), Err(LinuxError::ENODATA.into()));
        file.set_xattr(b"user.key", b"first", XattrSetMode::Create)
            .unwrap();
        assert_eq!(alias.get_xattr(b"user.key").unwrap(), b"first");

        assert_eq!(
            alias.set_xattr(b"user.key", b"wrong", XattrSetMode::Create),
            Err(LinuxError::EEXIST.into())
        );
        assert_eq!(file.get_xattr(b"user.key").unwrap(), b"first");
        assert_eq!(
            file.set_xattr(b"user.missing", b"wrong", XattrSetMode::Replace),
            Err(LinuxError::ENODATA.into())
        );

        alias
            .set_xattr(b"user.key", b"second", XattrSetMode::Replace)
            .unwrap();
        file.set_xattr(b"security.test", b"value", XattrSetMode::Upsert)
            .unwrap();
        assert_eq!(file.get_xattr(b"user.key").unwrap(), b"second");

        let listed = alias.list_xattrs().unwrap();
        let mut names = listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(|name| core::str::from_utf8(name).unwrap())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["security.test", "user.key"]);

        alias.remove_xattr(b"user.key").unwrap();
        assert_eq!(
            file.remove_xattr(b"user.key"),
            Err(LinuxError::ENODATA.into())
        );
    }

    #[test]
    fn xattr_provider_preserves_raw_names_through_list_and_remove() {
        let file = regular_file("raw-xattr-names");
        let raw_name = b"user.raw-\xff-name";
        let mut boundary_name = b"user.".to_vec();
        boundary_name.resize(255, 0xfe);
        assert_eq!(boundary_name.len(), 255);

        file.set_xattr(raw_name, b"raw", XattrSetMode::Create)
            .unwrap();
        file.set_xattr(&boundary_name, b"boundary", XattrSetMode::Create)
            .unwrap();
        assert_eq!(file.get_xattr(raw_name).unwrap(), b"raw");
        assert_eq!(file.get_xattr(&boundary_name).unwrap(), b"boundary");

        let listed = file.list_xattrs().unwrap();
        let names = listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        assert!(names.contains(&raw_name.as_slice()));
        assert!(names.contains(&boundary_name.as_slice()));

        file.remove_xattr(raw_name).unwrap();
        file.remove_xattr(&boundary_name).unwrap();
        assert_eq!(file.get_xattr(raw_name), Err(LinuxError::ENODATA.into()));
        assert_eq!(
            file.get_xattr(&boundary_name),
            Err(LinuxError::ENODATA.into())
        );
        assert!(file.list_xattrs().unwrap().is_empty());
    }

    #[test]
    fn concurrent_xattr_create_has_one_atomic_winner_across_aliases() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let file = root
            .create(
                "xattr-create-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let alias = root.link("xattr-create-alias", &file).unwrap();
        let start = Arc::new(Barrier::new(3));
        let first_start = start.clone();
        let first = file.clone();
        let first = thread::spawn(move || {
            first_start.wait();
            first.set_xattr(b"user.atomic", b"first", XattrSetMode::Create)
        });
        let second_start = start.clone();
        let second = alias.clone();
        let second = thread::spawn(move || {
            second_start.wait();
            second.set_xattr(b"user.atomic", b"second", XattrSetMode::Create)
        });
        start.wait();

        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(LinuxError::EEXIST.into()))
                .count(),
            1
        );
        let value = file.get_xattr(b"user.atomic").unwrap();
        assert!(value.as_slice() == b"first" || value.as_slice() == b"second");
    }

    #[test]
    fn xattr_provider_reports_inode_capacity_as_enospc_without_publication() {
        let file = regular_file("provider-capacity");
        let oversized = vec![0; TMPFS_XATTR_SIZE_MAX];

        assert_eq!(
            file.set_xattr(b"user.too-large", &oversized, XattrSetMode::Create),
            Err(LinuxError::ENOSPC.into())
        );
        assert_eq!(
            file.get_xattr(b"user.too-large"),
            Err(LinuxError::ENODATA.into())
        );
    }

    fn directory_names(root: &axfs_ng_vfs::Location) -> Vec<String> {
        let mut names = Vec::new();
        root.read_dir(0, &mut |name: &str, _, _, _| {
            names.push(name.to_string());
            true
        })
        .unwrap();
        names.sort();
        names
    }

    #[test]
    fn mutation_capabilities_are_explicit_and_mount_identity_is_exact() {
        let fs = MemoryFs::new().unwrap();
        let first_mount = Mountpoint::new_root(&fs);
        let second_mount = Mountpoint::new_root(&fs);
        let first_root = first_mount.root_location();
        let second_root = second_mount.root_location();
        let file = first_root
            .create(
                "file",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        assert!(first_root.supports_hard_links());
        assert!(first_root.supports_unlink());
        assert!(first_root.supports_rmdir());
        assert!(first_root.supports_rename());
        for node_type in [
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::Directory,
            NodeType::BlockDevice,
            NodeType::RegularFile,
            NodeType::Socket,
        ] {
            assert!(first_root.supports_named_create(node_type));
        }
        assert!(!first_root.supports_named_create(NodeType::Symlink));
        assert!(!first_root.supports_named_create(NodeType::Unknown));
        assert!(first_root.supports_symlink());
        assert!(first_root.same_mount(&file));
        assert!(!first_root.same_mount(&second_root));
        assert_eq!(first_mount.device(), second_mount.device());
        assert_ne!(first_mount.mount_id(), second_mount.mount_id());
    }

    #[test]
    fn hard_link_updates_source_ctime_and_parent_times_together() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let source = root
            .create(
                "timestamp-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let sentinel = Timestamp::from(core::time::Duration::MAX);
        root.update_metadata(MetadataUpdate {
            mtime: Some(sentinel),
            ctime: Some(sentinel),
            ..Default::default()
        })
        .unwrap();
        source
            .update_metadata(MetadataUpdate {
                ctime: Some(sentinel),
                ..Default::default()
            })
            .unwrap();

        let linked = root.link("timestamp-link", &source).unwrap();
        let source_metadata = source.metadata().unwrap();
        let linked_metadata = linked.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();

        assert_ne!(source_metadata.ctime, sentinel);
        assert_eq!(linked_metadata.ctime, source_metadata.ctime);
        assert_eq!(parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(parent_metadata.ctime, source_metadata.ctime);
    }

    fn metadata_state(
        location: &axfs_ng_vfs::Location,
    ) -> (u64, Timestamp, Timestamp, Timestamp, Timestamp) {
        let metadata = location.metadata().unwrap();
        (
            metadata.nlink,
            metadata.atime,
            metadata.btime,
            metadata.mtime,
            metadata.ctime,
        )
    }

    fn install_removal_timestamp_sentinels(
        parent: &axfs_ng_vfs::Location,
        victim: &axfs_ng_vfs::Location,
    ) {
        let sentinel = Timestamp::from(core::time::Duration::MAX);
        parent
            .update_metadata(MetadataUpdate {
                mtime: Some(sentinel),
                ctime: Some(sentinel),
                ..Default::default()
            })
            .unwrap();
        victim
            .update_metadata(MetadataUpdate {
                ctime: Some(sentinel),
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn unlink_updates_ctime_for_nonlast_and_last_links_with_one_parent_timestamp() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let source = root
            .create(
                "unlink-first",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let alias = root.link("unlink-last", &source).unwrap();
        assert_eq!(source.metadata().unwrap().nlink, 2);

        install_removal_timestamp_sentinels(&root, &source);
        root.unlink_checked("unlink-first", false, &source).unwrap();
        let source_metadata = source.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();
        assert_eq!(source_metadata.nlink, 1);
        assert_ne!(
            source_metadata.ctime,
            Timestamp::from(core::time::Duration::MAX)
        );
        assert_eq!(parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(parent_metadata.ctime, source_metadata.ctime);

        install_removal_timestamp_sentinels(&root, &source);
        root.unlink_checked("unlink-last", false, &alias).unwrap();
        let source_metadata = source.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();
        assert_eq!(source_metadata.nlink, 0);
        assert_ne!(
            source_metadata.ctime,
            Timestamp::from(core::time::Duration::MAX)
        );
        assert_eq!(parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(parent_metadata.ctime, source_metadata.ctime);
    }

    #[test]
    fn rmdir_updates_victim_and_parent_times_without_changing_link_count_semantics() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let directory = root
            .create(
                "empty-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let parent_nlink = root.metadata().unwrap().nlink;
        assert_eq!(directory.metadata().unwrap().nlink, 2);

        install_removal_timestamp_sentinels(&root, &directory);
        root.unlink_checked("empty-directory", true, &directory)
            .unwrap();

        let victim_metadata = directory.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();
        assert_eq!(victim_metadata.nlink, 0);
        assert_eq!(parent_metadata.nlink, parent_nlink - 1);
        assert_ne!(
            victim_metadata.ctime,
            Timestamp::from(core::time::Duration::MAX)
        );
        assert_eq!(parent_metadata.mtime, victim_metadata.ctime);
        assert_eq!(parent_metadata.ctime, victim_metadata.ctime);
    }

    #[test]
    fn failed_unlink_admission_preserves_timestamps_and_link_counts() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let victim = root
            .create(
                "victim",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let wrong_identity = root
            .create(
                "wrong-identity",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        install_removal_timestamp_sentinels(&root, &victim);
        let parent_before = metadata_state(&root);
        let victim_before = metadata_state(&victim);

        assert_eq!(
            root.unlink_checked("victim", false, &wrong_identity)
                .unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(metadata_state(&root), parent_before);
        assert_eq!(metadata_state(&victim), victim_before);

        assert_eq!(
            root.unlink_checked("victim", true, &victim).unwrap_err(),
            VfsError::NotADirectory
        );
        assert_eq!(metadata_state(&root), parent_before);
        assert_eq!(metadata_state(&victim), victim_before);

        let directory = root
            .create(
                "nonempty-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        directory
            .create(
                "child",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        install_removal_timestamp_sentinels(&root, &directory);
        let parent_before = metadata_state(&root);
        let directory_before = metadata_state(&directory);

        assert_eq!(
            root.unlink_checked("nonempty-directory", true, &directory)
                .unwrap_err(),
            VfsError::DirectoryNotEmpty
        );
        assert_eq!(metadata_state(&root), parent_before);
        assert_eq!(metadata_state(&directory), directory_before);
    }

    #[test]
    fn anonymous_inode_stays_hidden_until_same_inode_link() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let before = directory_names(&root);
        let directory_options = AnonymousOptions {
            node_type: NodeType::Directory,
            permission: NodePermission::from_bits_truncate(0o700),
            user: Some((1000, 1001)),
            linkable: true,
        };
        assert_eq!(
            root.create_anonymous(&directory_options).unwrap_err(),
            axfs_ng_vfs::VfsError::OperationNotSupported
        );
        assert_eq!(directory_names(&root), before);

        let options = AnonymousOptions {
            node_type: NodeType::RegularFile,
            permission: NodePermission::from_bits_truncate(0o640),
            user: Some((1000, 1001)),
            linkable: true,
        };

        let anonymous = root.create_anonymous(&options).unwrap();
        let anonymous_meta = anonymous.metadata().unwrap();
        assert_eq!(anonymous_meta.nlink, 0);
        assert_eq!((anonymous_meta.uid, anonymous_meta.gid), (1000, 1001));
        assert!(!anonymous.entry().is_root_of_mount());
        assert_ne!(
            anonymous.entry().try_key().unwrap(),
            root.entry().try_key().unwrap()
        );
        assert_eq!(
            anonymous.absolute_path().unwrap_err(),
            axfs_ng_vfs::VfsError::InvalidInput
        );
        assert_eq!(directory_names(&root), before);

        anonymous
            .entry()
            .as_file()
            .unwrap()
            .write_at(b"same inode", 0)
            .unwrap();
        let linked = root.link("published", &anonymous).unwrap();
        let linked_meta = linked.metadata().unwrap();
        assert_eq!(linked_meta.inode, anonymous_meta.inode);
        assert_eq!(linked_meta.nlink, 1);

        let mut contents = [0; 10];
        linked
            .entry()
            .as_file()
            .unwrap()
            .read_at(&mut contents, 0)
            .unwrap();
        assert_eq!(&contents, b"same inode");

        let after = directory_names(&root);
        assert_eq!(after.len(), before.len() + 1);
        assert!(after.iter().any(|name| name == "published"));
        assert!(!after.iter().any(|name| name.starts_with(".tmpfile-")));

        let second = root.link("published-again", &anonymous).unwrap();
        assert_eq!(second.metadata().unwrap().nlink, 2);
        root.unlink("published", false).unwrap();
        root.unlink("published-again", false).unwrap();
        assert_eq!(anonymous.metadata().unwrap().nlink, 0);
        assert_eq!(
            root.link("resurrected", &anonymous).unwrap_err(),
            axfs_ng_vfs::VfsError::NotFound
        );

        let mut unpublishable_options = options;
        unpublishable_options.linkable = false;
        let unpublishable = root.create_anonymous(&unpublishable_options).unwrap();
        assert_eq!(
            root.link("exclusive", &unpublishable).unwrap_err(),
            axfs_ng_vfs::VfsError::NotFound
        );
    }

    #[test]
    fn export_handle_descendant_uses_namespace_inode_ancestry() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let subtree = root
            .create(
                "subtree",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let child = subtree
            .create(
                "child",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let sibling = root
            .create(
                "sibling",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let child_handle = mount
            .encode_export_handle(&child, ExportHandleMode::Openable)
            .unwrap();
        let sibling_handle = mount
            .encode_export_handle(&sibling, ExportHandleMode::Openable)
            .unwrap();
        let child = mount
            .decode_export_handle(
                child_handle.handle_type,
                &child_handle.bytes,
                axfs_ng_vfs::ExportHandleDecodeMode::Any,
            )
            .unwrap();
        let sibling = mount
            .decode_export_handle(
                sibling_handle.handle_type,
                &sibling_handle.bytes,
                axfs_ng_vfs::ExportHandleDecodeMode::Any,
            )
            .unwrap();
        assert!(mount.export_handle_is_descendant(&subtree, &child).unwrap());
        assert!(
            !mount
                .export_handle_is_descendant(&subtree, &sibling)
                .unwrap()
        );
    }

    #[test]
    fn directory_only_export_decode_rejects_regular_inode_as_stale() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let file = mount
            .root_location()
            .create(
                "file",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let handle = mount
            .encode_export_handle(&file, ExportHandleMode::Openable)
            .unwrap();

        assert_eq!(
            mount
                .decode_export_handle(
                    handle.handle_type,
                    &handle.bytes,
                    ExportHandleDecodeMode::DirectoryOnly,
                )
                .unwrap_err(),
            VfsError::NotFound
        );
    }

    #[test]
    fn symlink_is_initialized_and_owned_before_publication() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();

        assert_eq!(
            root.create(
                "empty-link",
                NodeType::Symlink,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap_err(),
            axfs_ng_vfs::VfsError::OperationNotPermitted
        );
        assert_eq!(
            root.lookup_no_follow("empty-link").unwrap_err(),
            axfs_ng_vfs::VfsError::NotFound
        );

        let link = root
            .create_symlink(
                "link",
                "target",
                NodePermission::from_bits_truncate(0o777),
                Some((1000, 1001)),
            )
            .unwrap();
        let metadata = link.metadata().unwrap();
        assert_eq!(metadata.node_type, NodeType::Symlink);
        assert_eq!(metadata.mode.bits() & 0o777, 0o777);
        assert_eq!((metadata.uid, metadata.gid), (1000, 1001));
        assert_eq!(link.read_link().unwrap(), "target");
    }

    #[test]
    fn named_create_initializes_attributes_before_namespace_commit() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let options = NamedCreateOptions {
            node_type: NodeType::CharacterDevice,
            permission: NodePermission::from_bits_truncate(0o2640),
            owner: Some((1000, 1001)),
            rdev: Some(DeviceId(0x1234)),
            initial_data: None,
        };

        let created = root
            .create_named("device", &options, CreateDisposition::Exclusive)
            .unwrap();
        assert!(created.created);
        let metadata = created.entry.metadata().unwrap();
        assert_eq!(metadata.node_type, NodeType::CharacterDevice);
        assert_eq!(metadata.mode.bits(), 0o2640);
        assert_eq!((metadata.uid, metadata.gid), (1000, 1001));
        assert_eq!(metadata.rdev, DeviceId(0x1234));

        let existing = root
            .create_named(
                "device",
                &NamedCreateOptions {
                    owner: Some((2000, 2001)),
                    ..options.clone()
                },
                CreateDisposition::OpenOrCreate,
            )
            .unwrap();
        assert!(!existing.created);
        assert_eq!(existing.entry.inode(), created.entry.inode());
        let metadata = existing.entry.metadata().unwrap();
        assert_eq!((metadata.uid, metadata.gid), (1000, 1001));
        assert_eq!(metadata.rdev, DeviceId(0x1234));

        let invalid = NamedCreateOptions {
            node_type: NodeType::RegularFile,
            rdev: Some(DeviceId(7)),
            ..options.clone()
        };
        assert_eq!(
            root.create_named("invalid", &invalid, CreateDisposition::Exclusive)
                .unwrap_err(),
            axfs_ng_vfs::VfsError::InvalidInput
        );
        assert_eq!(
            root.lookup_no_follow("invalid").unwrap_err(),
            axfs_ng_vfs::VfsError::NotFound
        );
    }

    #[test]
    fn named_create_publishes_the_exact_prepared_user_data() {
        struct Marker(u64);

        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let marker = Arc::new(Marker(0xfeed_beef));
        let created = root
            .create_named(
                "socket",
                &NamedCreateOptions {
                    node_type: NodeType::Socket,
                    permission: NodePermission::from_bits_truncate(0o750),
                    owner: Some((1000, 1001)),
                    rdev: None,
                    initial_data: Some(InitialNodeData::from_shared(marker.clone())),
                },
                CreateDisposition::Exclusive,
            )
            .unwrap();
        let attached = created.entry.user_data().get::<Marker>().unwrap();
        assert!(Arc::ptr_eq(&attached, &marker));
        assert_eq!(attached.0, 0xfeed_beef);

        let _alias = root.link("socket-alias", &created.entry).unwrap();
        let unrelated = root
            .create(
                "unrelated",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        root.unlink_checked("unrelated", false, &unrelated).unwrap();

        let visible = root.lookup_no_follow("socket").unwrap();
        let visible_marker = visible.user_data().get::<Marker>().unwrap();
        assert!(Arc::ptr_eq(&visible_marker, &marker));
        let alias = root.lookup_no_follow("socket-alias").unwrap();
        let alias_marker = alias.user_data().get::<Marker>().unwrap();
        assert!(Arc::ptr_eq(&alias_marker, &marker));
    }

    #[test]
    fn open_or_create_checks_existing_before_new_inode_options() {
        struct Marker;

        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let created = root
            .create(
                "existing",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let outcome = root
            .create_named(
                "existing",
                &NamedCreateOptions {
                    node_type: NodeType::RegularFile,
                    permission: NodePermission::empty(),
                    owner: Some((123, 456)),
                    rdev: Some(DeviceId(7)),
                    initial_data: Some(InitialNodeData::from_shared(Arc::new(Marker))),
                },
                CreateDisposition::OpenOrCreate,
            )
            .unwrap();
        assert!(!outcome.created);
        assert_eq!(outcome.entry.inode(), created.inode());
        assert!(outcome.entry.user_data().get::<Marker>().is_none());
    }

    #[test]
    fn alias_cache_observes_backend_namespace_epoch() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let parent = root
            .create(
                "parent",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        parent
            .create(
                "victim",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let alias = root
            .entry()
            .as_dir()
            .unwrap()
            .inner()
            .lookup("parent")
            .unwrap();
        let alias_dir = alias.as_dir().unwrap();
        alias_dir.lookup("victim").unwrap();

        parent.unlink("victim", false).unwrap();
        assert_eq!(alias_dir.lookup("victim").unwrap_err(), VfsError::NotFound);
    }

    #[test]
    fn alias_unlink_uses_backend_identity_not_dentry_pointer() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let parent = root
            .create(
                "parent",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        parent
            .create(
                "victim",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let alias = root
            .entry()
            .as_dir()
            .unwrap()
            .inner()
            .lookup("parent")
            .unwrap();
        let alias_dir = alias.as_dir().unwrap();
        let expected = alias_dir.inner().lookup("victim").unwrap();
        alias_dir
            .unlink_checked("victim", false, &expected)
            .unwrap();
        assert_eq!(
            parent.lookup_no_follow("victim").unwrap_err(),
            VfsError::NotFound
        );
    }

    #[test]
    fn unlink_rejects_a_replaced_expected_identity() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let old = root
            .create(
                "slot",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        root.unlink("slot", false).unwrap();
        let replacement = root
            .create(
                "slot",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        assert_eq!(
            root.entry()
                .as_dir()
                .unwrap()
                .unlink_checked("slot", false, old.entry())
                .unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(
            root.lookup_no_follow("slot").unwrap().inode(),
            replacement.inode()
        );
    }

    #[test]
    fn rename_same_object_is_noop_and_type_matrix_is_complete() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let dir = root
            .create(
                "dir",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        dir.create(
            "child",
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        root.create(
            "file",
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();

        root.rename("dir", &root, "dir").unwrap();
        assert!(root.lookup_no_follow("dir").unwrap().is_dir());
        assert_eq!(
            root.rename("file", &root, "dir").unwrap_err(),
            VfsError::IsADirectory
        );
        assert_eq!(
            root.rename("dir", &root, "file").unwrap_err(),
            VfsError::NotADirectory
        );
    }

    #[test]
    fn rename_replacement_uses_one_timestamp_for_source_victim_and_parent() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let source = root
            .create(
                "source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let victim = root
            .create(
                "victim",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let sentinel = Timestamp::from(core::time::Duration::MAX);
        root.update_metadata(MetadataUpdate {
            mtime: Some(sentinel),
            ctime: Some(sentinel),
            ..Default::default()
        })
        .unwrap();
        for inode in [&source, &victim] {
            inode
                .update_metadata(MetadataUpdate {
                    ctime: Some(sentinel),
                    ..Default::default()
                })
                .unwrap();
        }

        root.rename("source", &root, "victim").unwrap();

        let source_metadata = source.metadata().unwrap();
        let victim_metadata = victim.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();
        assert_eq!(source_metadata.nlink, 1);
        assert_eq!(victim_metadata.nlink, 0);
        assert_ne!(source_metadata.ctime, sentinel);
        assert_eq!(victim_metadata.ctime, source_metadata.ctime);
        assert_eq!(parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(parent_metadata.ctime, source_metadata.ctime);
    }

    #[test]
    fn cross_parent_rename_uses_one_timestamp_for_all_affected_inodes() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let mode = NodePermission::from_bits_truncate(0o700);
        let old_parent = root.create("old", NodeType::Directory, mode).unwrap();
        let new_parent = root.create("new", NodeType::Directory, mode).unwrap();
        let source = old_parent
            .create("source", NodeType::RegularFile, mode)
            .unwrap();
        let victim = new_parent
            .create("victim", NodeType::RegularFile, mode)
            .unwrap();
        let sentinel = Timestamp::from(core::time::Duration::MAX);
        for parent in [&old_parent, &new_parent] {
            parent
                .update_metadata(MetadataUpdate {
                    mtime: Some(sentinel),
                    ctime: Some(sentinel),
                    ..Default::default()
                })
                .unwrap();
        }
        for inode in [&source, &victim] {
            inode
                .update_metadata(MetadataUpdate {
                    ctime: Some(sentinel),
                    ..Default::default()
                })
                .unwrap();
        }

        old_parent.rename("source", &new_parent, "victim").unwrap();

        let source_metadata = source.metadata().unwrap();
        let victim_metadata = victim.metadata().unwrap();
        let old_parent_metadata = old_parent.metadata().unwrap();
        let new_parent_metadata = new_parent.metadata().unwrap();
        assert_eq!(source_metadata.nlink, 1);
        assert_eq!(victim_metadata.nlink, 0);
        assert_ne!(source_metadata.ctime, sentinel);
        assert_eq!(victim_metadata.ctime, source_metadata.ctime);
        assert_eq!(old_parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(old_parent_metadata.ctime, source_metadata.ctime);
        assert_eq!(new_parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(new_parent_metadata.ctime, source_metadata.ctime);
    }

    #[test]
    fn failed_rename_preserves_inode_and_parent_metadata() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let source = root
            .create(
                "source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let victim = root
            .create(
                "victim",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        install_removal_timestamp_sentinels(&root, &source);
        victim
            .update_metadata(MetadataUpdate {
                ctime: Some(Timestamp::from(core::time::Duration::MAX)),
                ..Default::default()
            })
            .unwrap();
        let parent_before = metadata_state(&root);
        let source_before = metadata_state(&source);
        let victim_before = metadata_state(&victim);

        assert_eq!(
            root.rename("source", &root, "victim").unwrap_err(),
            VfsError::IsADirectory
        );
        assert_eq!(metadata_state(&root), parent_before);
        assert_eq!(metadata_state(&source), source_before);
        assert_eq!(metadata_state(&victim), victim_before);
    }

    #[test]
    fn named_create_updates_parent_mtime_and_ctime() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        root.update_metadata(MetadataUpdate {
            mtime: Some(Timestamp::from(core::time::Duration::MAX)),
            ctime: Some(Timestamp::from(core::time::Duration::MAX)),
            ..Default::default()
        })
        .unwrap();

        root.create(
            "child",
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        )
        .unwrap();
        let metadata = root.metadata().unwrap();
        assert_ne!(metadata.mtime, Timestamp::from(core::time::Duration::MAX));
        assert_ne!(metadata.ctime, Timestamp::from(core::time::Duration::MAX));
    }
}
