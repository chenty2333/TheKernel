use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
#[cfg(feature = "times")]
use core::sync::atomic::AtomicU8;
use core::{
    num::NonZeroUsize,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axalloc::{UsageKind, global_allocator};
use axfs_ng_vfs::{
    FileNode, Location, MetadataUpdate, Mountpoint, NodeFlags, NodePermission, NodeType, VfsError,
    VfsResult, WeakDirEntry, path::Path,
};
use axhal::mem::{PhysAddr, VirtAddr, total_ram_size, virt_to_phys};
use axio::{SeekFrom, prelude::*};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use lru::LruCache;
use spin::{Once, RwLock};

use super::FsContext;

bitflags::bitflags! {
    /// Flags describing the access mode of an opened file.
    #[derive(Debug, Clone, Copy)]
    pub struct FileFlags: u8 {
        /// Read access.
        const READ = 1;
        /// Write access.
        const WRITE = 2;
        /// Execute access.
        const EXECUTE = 4;
        /// Append mode — writes always go to the end of the file.
        const APPEND = 8;
        /// Path-only handle, no actual I/O is permitted.
        const PATH = 16;
        /// Suppress access-time updates on successful reads.
        const NOATIME = 32;
        /// Direct-I/O mode requested by the opener.
        const DIRECT = 64;
    }
}

/// Results returned by [`OpenOptions::open`].
pub enum OpenResult {
    /// The opened path is a regular file.
    File(File),
    /// The opened path is a directory.
    Dir(Location),
}

impl OpenResult {
    /// Converts into a [`File`], returning an error if this is a directory.
    pub fn into_file(self) -> VfsResult<File> {
        match self {
            Self::File(file) => Ok(file),
            Self::Dir(_) => Err(VfsError::IsADirectory),
        }
    }

    /// Converts into a [`Location`], returning an error if this is a file.
    pub fn into_dir(self) -> VfsResult<Location> {
        match self {
            Self::Dir(dir) => Ok(dir),
            Self::File(_) => Err(VfsError::NotADirectory),
        }
    }

    /// Extracts the underlying [`Location`] regardless of variant.
    pub fn into_location(self) -> Location {
        match self {
            Self::File(file) => file.location().clone(),
            Self::Dir(dir) => dir,
        }
    }
}

/// Options and flags which can be used to configure how a file is opened.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    // generic
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    directory: bool,
    no_follow: bool,
    direct: bool,
    no_atime: bool,
    user: Option<(u32, u32)>,
    path: bool,
    node_type: NodeType,
    // system-specific
    mode: u32,
}

impl OpenOptions {
    /// Creates a blank new set of options ready for configuration.
    pub fn new() -> Self {
        Self {
            // generic
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            directory: false,
            no_follow: false,
            direct: false,
            no_atime: false,
            user: None,
            path: false,
            node_type: NodeType::RegularFile,
            // system-specific
            mode: 0o666,
        }
    }

    /// Sets the option for read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Sets the option for write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Sets the option for the append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Sets the option for truncating a previous file.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Sets the option to create a new file, or open it if it already exists.
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Sets the option to create a new file, failing if it already exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Sets the option to open directory instead.
    pub fn directory(&mut self, directory: bool) -> &mut Self {
        self.directory = directory;
        self
    }

    /// Sets the option to not follow symlinks.
    pub fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    /// Sets the option to open the file with direct I/O.\
    pub fn direct(&mut self, direct: bool) -> &mut Self {
        self.direct = direct;
        self
    }

    /// Sets the option to suppress access time updates on read.
    pub fn no_atime(&mut self, no_atime: bool) -> &mut Self {
        self.no_atime = no_atime;
        self
    }

    /// Sets the user and group id to open the file with.
    pub fn user(&mut self, uid: u32, gid: u32) -> &mut Self {
        self.user = Some((uid, gid));
        self
    }

    /// Sets the option for path only access.
    pub fn path(&mut self, path: bool) -> &mut Self {
        self.path = path;
        self
    }

    /// Sets the node type for the file.
    ///
    /// This will only be used if the file is created.
    pub fn node_type(&mut self, node_type: NodeType) -> &mut Self {
        self.node_type = node_type;
        self
    }

    /// Sets the mode bits that a new file will be created with.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    fn _open(&self, loc: Location) -> VfsResult<OpenResult> {
        let flags = self.to_flags()?;

        if self.directory {
            if flags.contains(FileFlags::WRITE) {
                return Err(VfsError::IsADirectory);
            }
            loc.check_is_dir()?;
        }
        if loc.is_dir()
            && (self.write || self.append || self.truncate || self.create || self.create_new)
        {
            return Err(VfsError::IsADirectory);
        }
        Ok(if loc.is_dir() {
            OpenResult::Dir(loc)
        } else {
            // TODO(mivik): is this correct?
            let non_cacheable_type = matches!(
                loc.metadata()?.node_type,
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            );

            let direct = non_cacheable_type
                || self.path
                || self.direct
                || loc.flags().contains(NodeFlags::NON_CACHEABLE);
            let backend = if !direct || loc.flags().contains(NodeFlags::ALWAYS_CACHE) {
                FileBackend::new_cached(loc)
            } else {
                FileBackend::new_direct(loc)
            };
            if self.truncate {
                backend.set_len(0)?;
            }
            OpenResult::File(File::new(backend, flags))
        })
    }

    /// Opens a file at the given [`Location`] using these options.
    pub fn open_loc(&self, loc: Location) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }
        self._open(loc)
    }

    /// Opens a file at the given path relative to the provided [`FsContext`].
    pub fn open(&self, context: &FsContext, path: impl AsRef<Path>) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        let loc = match context.resolve_parent(path.as_ref()) {
            Ok((parent, name)) => {
                let mut loc = parent.open_file(
                    &name,
                    &axfs_ng_vfs::OpenOptions {
                        create: self.create,
                        create_new: self.create_new,
                        node_type: self.node_type,
                        permission: NodePermission::from_bits_truncate(self.mode as _),
                        user: self.user,
                    },
                )?;
                if !self.no_follow {
                    loc = context
                        .with_current_dir(parent)?
                        .try_resolve_symlink(loc, &mut 0)?;
                }
                loc
            }
            Err(VfsError::InvalidInput) => {
                // root directory
                context.root_dir().clone()
            }
            Err(err) => return Err(err),
        };
        self._open(loc)
    }

    pub(crate) fn to_flags(&self) -> VfsResult<FileFlags> {
        if self.path {
            return Ok(FileFlags::PATH);
        }
        let mut flags = match (self.read, self.write, self.append) {
            (true, false, false) => FileFlags::READ,
            (false, true, false) => FileFlags::WRITE,
            (true, true, false) => FileFlags::READ | FileFlags::WRITE,
            (false, _, true) => FileFlags::WRITE | FileFlags::APPEND,
            (true, _, true) => FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
            (false, false, false) => return Err(VfsError::InvalidInput),
        };
        if self.no_atime {
            flags |= FileFlags::NOATIME;
        }
        if self.direct {
            flags |= FileFlags::DIRECT;
        }
        Ok(flags)
    }

    pub(crate) fn is_valid(&self) -> bool {
        if self.path {
            return !self.read
                && !self.write
                && !self.append
                && !self.truncate
                && !self.create
                && !self.create_new
                && !self.no_atime;
        }
        if !self.read && !self.write && !self.append {
            return false;
        }
        if self.append && self.truncate && !self.create_new {
            return false;
        }
        true
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

const PAGE_SIZE: usize = 4096;
/// Sequential-read readahead window in pages. On a cache miss we issue one
/// device read for up to this many pages and populate the page cache ahead of
/// the scan, amortizing the per-request ext4-lock + lwext4 + virtio-blk cost.
const READAHEAD_PAGES: usize = 64;
const MAX_DIRTY_WRITEBACK_PAGES: usize = 64;
const IN_MEMORY_PAGE_CACHE_PAGES: usize = 1024;
static DIRTY_PAGE_CACHE_PFNS: Once<Mutex<BTreeSet<usize>>> = Once::new();
static FILE_CACHE_REGISTRY: Once<Mutex<BTreeMap<(u64, u64), FileUserData>>> = Once::new();

fn dirty_page_cache_pfns() -> &'static Mutex<BTreeSet<usize>> {
    DIRTY_PAGE_CACHE_PFNS.call_once(|| Mutex::new(BTreeSet::new()))
}

fn file_cache_registry() -> &'static Mutex<BTreeMap<(u64, u64), FileUserData>> {
    FILE_CACHE_REGISTRY.call_once(|| Mutex::new(BTreeMap::new()))
}

fn cached_file_registry_key(location: &Location) -> (u64, u64) {
    (location.mountpoint().device(), location.inode())
}

fn cached_file_is_in_memory(location: &Location) -> bool {
    location.flags().contains(NodeFlags::ALWAYS_CACHE) || location.filesystem().name() == "tmpfs"
}

fn cached_file_shared_for_location(location: &Location) -> Option<Arc<CachedFileShared>> {
    let key = cached_file_registry_key(location);
    file_cache_registry()
        .lock()
        .get(&key)
        .and_then(FileUserData::shared)
        .or_else(|| {
            location
                .user_data()
                .get::<FileUserData>()
                .and_then(|it| it.shared())
        })
}

fn cached_file_shared_for_location_or_create(location: &Location) -> Arc<CachedFileShared> {
    let key = cached_file_registry_key(location);
    let mut registry = file_cache_registry().lock();

    if let Some(shared) = registry.get(&key).and_then(FileUserData::shared) {
        return shared;
    }

    if let Some(shared) = location
        .user_data()
        .get::<FileUserData>()
        .and_then(|it| it.shared())
    {
        registry.insert(key, FileUserData::new(location, &shared));
        return shared;
    }

    let shared = Arc::new(CachedFileShared::new(cached_file_is_in_memory(location)));
    registry.insert(key, FileUserData::new(location, &shared));

    location
        .user_data()
        .insert(FileUserData::new(location, &shared));

    shared
}

/// Drops the shared page-cache registry entry for a fully released inode.
pub fn remove_cached_file_registry_entry(device: u64, inode: u64) {
    file_cache_registry().lock().remove(&(device, inode));
}

/// Prunes dead cache registry entries for a released inode.
pub fn prune_dead_cached_file_registry_entries_for_inode(inode: u64) {
    file_cache_registry()
        .lock()
        .retain(|(_, entry_inode), entry| *entry_inode != inode || entry.shared().is_some());
}

/// Flushes and drops cached pages before backend storage is changed out-of-band.
pub fn sync_and_invalidate_cached_file_pages(location: &Location) -> VfsResult<()> {
    let shared = cached_file_shared_for_location(location);
    if let Some(shared) = shared {
        let file = location.entry().as_file()?;
        loop {
            let Some((pn, mut page)) = ({
                let mut cache = shared.page_cache.lock();
                cache.pop_lru()
            }) else {
                break;
            };
            let _ = writeback_cached_page(&shared, file, pn, &mut page)?;
        }
    }
    Ok(())
}

/// Returns whether the page-cache page with the given PFN is currently dirty.
pub fn page_cache_pfn_is_dirty(pfn: usize) -> bool {
    dirty_page_cache_pfns().lock().contains(&pfn)
}

fn discard_cached_pages(shared: &CachedFileShared) {
    let mut guard = shared.page_cache.lock();
    while let Some((_pn, mut page)) = guard.pop_lru() {
        page.clear_dirty();
    }
}

/// Marks cached pages for an inode whose final directory entry is being removed.
pub fn mark_cached_file_unlinked(location: &Location) {
    if let Some(shared) = cached_file_shared_for_location(location) {
        shared.unlinked.store(true, Ordering::Release);
    }
}

/// Flushes all live dirty cached file pages before a global sync.
pub fn sync_all_cached_file_pages() -> VfsResult<()> {
    let entries = {
        let mut registry = file_cache_registry().lock();
        registry.retain(|_, entry| entry.shared().is_some() && entry.location().is_some());
        registry
            .values()
            .filter_map(|entry| Some((entry.shared()?, entry.location()?)))
            .collect::<Vec<_>>()
    };

    for (shared, location) in entries {
        if shared.unlinked.load(Ordering::Acquire) {
            continue;
        }
        let file = location.entry().as_file()?;
        let cached = CachedFile {
            in_memory: cached_file_is_in_memory(&location),
            inner: location.clone(),
            shared,
        };
        cached.flush_dirty_cache(file)?;
    }
    Ok(())
}

/// A single page-sized cache entry backed by a physical page.
#[derive(Debug)]
pub struct PageCache {
    addr: VirtAddr,
    dirty: bool,
}

impl PageCache {
    fn new() -> VfsResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE, UsageKind::PageCache)
            .inspect_err(|err| {
                warn!("Failed to allocate page cache: {:?}", err);
            })?;
        Ok(Self {
            addr: addr.into(),
            dirty: false,
        })
    }

    /// Returns the physical address of this page.
    pub fn paddr(&self) -> PhysAddr {
        virt_to_phys(self.addr)
    }

    fn pfn(&self) -> usize {
        self.paddr().as_usize() / PAGE_SIZE
    }

    /// Marks this page as dirty so it will be flushed on eviction.
    pub fn mark_dirty(&mut self) {
        if !self.dirty {
            dirty_page_cache_pfns().lock().insert(self.pfn());
        }
        self.dirty = true;
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        if self.dirty {
            dirty_page_cache_pfns().lock().remove(&self.pfn());
        }
        self.dirty = false;
    }

    /// Returns a mutable slice over the page data.
    pub fn data(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE) }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        if self.dirty {
            warn!("dirty page dropped without flushing");
            dirty_page_cache_pfns().lock().remove(&self.pfn());
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}

type EvictListenerFn = Arc<dyn Fn(u32, &PageCache) + Send + Sync>;

fn evict_listeners_snapshot(shared: &CachedFileShared) -> Vec<EvictListenerFn> {
    shared
        .evict_listeners
        .lock()
        .iter()
        .map(|listener| listener.listener.clone())
        .collect()
}

fn writeback_cached_page(
    shared: &CachedFileShared,
    file: &FileNode,
    pn: u32,
    page: &mut PageCache,
) -> VfsResult<bool> {
    let listeners = evict_listeners_snapshot(shared);
    let had_evict_listener = !listeners.is_empty();
    for listener in listeners {
        listener(pn, page);
    }
    if page.dirty {
        let page_start = pn as u64 * PAGE_SIZE as u64;
        let len = (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
        if len > 0 {
            file.write_at(&page.data()[..len], page_start)?;
        }
        page.clear_dirty();
    }
    Ok(had_evict_listener)
}

/// A page evicted while inserting a new cached file page.
pub struct EvictedPage {
    pn: u32,
    _page: Option<PageCache>,
}

impl EvictedPage {
    /// Returns the file page number that was evicted.
    pub fn page_number(&self) -> u32 {
        self.pn
    }
}

fn per_file_page_cache_capacity() -> NonZeroUsize {
    const MIB: usize = 1024 * 1024;
    const GIB: usize = 1024 * MIB;
    let ram = total_ram_size();
    let pages = if ram <= 512 * MIB {
        64
    } else if ram <= 2 * GIB {
        2048
    } else {
        let extra_gib = (ram - 2 * GIB) / GIB;
        2048usize.saturating_add(extra_gib.saturating_mul(256))
    };
    NonZeroUsize::new(pages).unwrap()
}

fn in_memory_page_cache_capacity() -> NonZeroUsize {
    NonZeroUsize::new(IN_MEMORY_PAGE_CACHE_PAGES).unwrap()
}

struct EvictListener {
    listener: EvictListenerFn,
    link: LinkedListAtomicLink,
}

intrusive_adapter!(EvictListenerAdapter = Box<EvictListener>: EvictListener { link: LinkedListAtomicLink });

struct CachedFileShared {
    page_cache: Mutex<LruCache<u32, PageCache>>,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    unlinked: AtomicBool,
    /// Serializes cached page-cache users with direct-I/O cache drains.
    direct_io_lock: RwLock<()>,
    /// Serializes O_APPEND writes across all cached handles for this inode.
    append_lock: RwLock<()>,
}

impl CachedFileShared {
    pub fn new(in_memory: bool) -> Self {
        let capacity = if in_memory {
            in_memory_page_cache_capacity()
        } else {
            per_file_page_cache_capacity()
        };
        Self {
            page_cache: Mutex::new(new_bounded_page_cache_store(capacity)),
            evict_listeners: Mutex::new(LinkedList::default()),
            unlinked: AtomicBool::new(false),
            direct_io_lock: RwLock::new(()),
            append_lock: RwLock::new(()),
        }
    }
}

fn new_bounded_page_cache_store(capacity: NonZeroUsize) -> LruCache<u32, PageCache> {
    LruCache::new(capacity)
}

/// A file handle with an LRU page cache for buffered I/O.
pub struct CachedFile {
    inner: Location,
    shared: Arc<CachedFileShared>,
    in_memory: bool,
}

impl Clone for CachedFile {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
            in_memory: self.in_memory,
        }
    }
}

struct FileUserData {
    shared: Weak<CachedFileShared>,
    mountpoint: Weak<Mountpoint>,
    entry: WeakDirEntry,
}

impl FileUserData {
    fn new(location: &Location, shared: &Arc<CachedFileShared>) -> Self {
        Self {
            shared: Arc::downgrade(shared),
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
    }

    pub fn shared(&self) -> Option<Arc<CachedFileShared>> {
        self.shared.upgrade()
    }

    pub fn location(&self) -> Option<Location> {
        Some(Location::new(self.mountpoint.upgrade()?, self.entry.upgrade()?))
    }
}

impl CachedFile {
    /// Returns an existing cached file for `location`, or creates a new one.
    pub fn get_or_create(location: Location) -> Self {
        let in_memory = cached_file_is_in_memory(&location);
        let shared = cached_file_shared_for_location_or_create(&location);

        Self {
            inner: location,
            shared,
            in_memory,
        }
    }

    /// Returns `true` if both handles refer to the same shared state.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Returns `true` if this file is backed by an in-memory filesystem (e.g. tmpfs).
    pub fn in_memory(&self) -> bool {
        self.in_memory
    }

    /// Registers a listener that is called when a page is evicted from cache.
    ///
    /// Returns a handle that can later be passed to
    /// [`remove_evict_listener`](Self::remove_evict_listener).
    pub fn add_evict_listener<F>(&self, listener: F) -> usize
    where
        F: Fn(u32, &PageCache) + Send + Sync + 'static,
    {
        let pointer = Box::new(EvictListener {
            listener: Arc::new(listener),
            link: LinkedListAtomicLink::new(),
        });
        let handle = pointer.as_ref() as *const EvictListener as usize;
        self.shared.evict_listeners.lock().push_back(pointer);
        handle
    }

    /// # Safety
    /// The handle must be valid, that means:
    /// - It must be returned by a previous call to `add_evict_listener` on the same `CachedFile`.
    /// - It must not be removed by a previous call to `remove_evict_listener`.
    pub unsafe fn remove_evict_listener(&self, handle: usize) {
        let mut guard = self.shared.evict_listeners.lock();
        let mut cursor = unsafe { guard.cursor_mut_from_ptr(handle as *const EvictListener) };
        cursor.remove();
    }

    fn evict_cache(&self, file: &FileNode, pn: u32, page: &mut PageCache) -> VfsResult<bool> {
        writeback_cached_page(&self.shared, file, pn, page)
    }

    fn pop_lru_page(&self) -> Option<(u32, PageCache)> {
        self.shared.page_cache.lock().pop_lru()
    }

    fn pop_cached_page(&self, pn: u32) -> Option<PageCache> {
        self.shared.page_cache.lock().pop(&pn)
    }

    fn drain_cache(&self, file: &FileNode) -> VfsResult<()> {
        while let Some((pn, mut page)) = self.pop_lru_page() {
            let _ = self.evict_cache(file, pn, &mut page)?;
        }
        Ok(())
    }

    fn discard_cache(&self) {
        discard_cached_pages(&self.shared);
    }

    fn sync_in_memory_cache(&self) {
        let Ok(file) = self.inner.entry().as_file() else {
            return;
        };
        if let Err(err) = self.flush_dirty_cache(file) {
            warn!("Failed to flush in-memory file cache: {err:?}");
        }
    }

    fn flush_dirty_cache(&self, file: &FileNode) -> VfsResult<()> {
        let mut guard = self.shared.page_cache.lock();
        let file_len = file.len()?;
        let mut dirty_pages = guard
            .iter()
            .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
            .collect::<Vec<_>>();
        dirty_pages.sort_unstable();

        let mut start = 0;
        while start < dirty_pages.len() {
            let first_pn = dirty_pages[start];
            let end_limit = (start + MAX_DIRTY_WRITEBACK_PAGES).min(dirty_pages.len());
            let mut end = start + 1;
            while end < end_limit && dirty_pages[end] == dirty_pages[end - 1] + 1 {
                end += 1;
            }

            let page_start = first_pn as u64 * PAGE_SIZE as u64;
            let max_len = file_len
                .saturating_sub(page_start)
                .min(((end - start) * PAGE_SIZE) as u64) as usize;
            if max_len == 0 {
                for pn in &dirty_pages[start..end] {
                    if let Some(page) = guard.get_mut(pn) {
                        page.clear_dirty();
                    }
                }
                start = end;
                continue;
            }

            let mut data = vec![0; max_len];
            for (idx, pn) in dirty_pages[start..end].iter().enumerate() {
                let Some(page) = guard.get_mut(pn) else {
                    continue;
                };
                for listener in evict_listeners_snapshot(&self.shared) {
                    listener(*pn, page);
                }
                let dst_start = idx * PAGE_SIZE;
                if dst_start >= max_len {
                    continue;
                }
                let len = (max_len - dst_start).min(PAGE_SIZE);
                data[dst_start..dst_start + len].copy_from_slice(&page.data()[..len]);
            }

            file.write_at(&data, page_start)?;

            for pn in &dirty_pages[start..end] {
                if let Some(page) = guard.get_mut(pn) {
                    page.clear_dirty();
                }
            }

            start = end;
        }
        Ok(())
    }

    fn ensure_page_cached(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
    ) -> VfsResult<Option<EvictedPage>> {
        self.ensure_page_cached_with(file, cache, pn, true)
    }

    fn ensure_page_cached_with(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
        load_from_file: bool,
    ) -> VfsResult<Option<EvictedPage>> {
        if cache.contains(&pn) {
            return Ok(None);
        }
        let cap = cache.cap().get();
        let mut evicted = None;
        // Make room for the requested page. The caller may receive this
        // EvictedPage; any further evictions done for readahead below are
        // written back and dropped.
        if cache.len() >= cap {
            if let Some((epn, mut epage)) = cache.pop_lru() {
                let retain_page = self.evict_cache(file, epn, &mut epage)?;
                let page = retain_page.then_some(epage);
                evicted = Some(EvictedPage { pn: epn, _page: page });
            }
        }

        if !load_from_file {
            let mut page = PageCache::new()?;
            page.data().fill(0);
            cache.put(pn, page);
            return Ok(evicted);
        }

        // Readahead: read up to READAHEAD_PAGES pages in one device read into a
        // scratch buffer, then populate the cache for the requested page and the
        // following pages. Subsequent reads in a sequential scan hit the cache
        // instead of re-issuing device I/O.
        let avail = cap.saturating_sub(cache.len());
        let ra = READAHEAD_PAGES.min(avail).max(1);
        let base = pn as u64 * PAGE_SIZE as u64;
        let mut buf = vec![0u8; ra * PAGE_SIZE];
        let read = file.read_at(&mut buf, base)?;

        // The requested page.
        let mut page = PageCache::new()?;
        let data = page.data();
        data.fill(0);
        let n0 = read.min(PAGE_SIZE);
        data[..n0].copy_from_slice(&buf[..n0]);
        cache.put(pn, page);

        // Prefetch the subsequent pages already fetched into buf.
        for i in 1..ra {
            let off = i * PAGE_SIZE;
            if off >= read {
                break; // reached EOF
            }
            let next_pn = pn + i as u32;
            if cache.contains(&next_pn) {
                continue;
            }
            if cache.len() >= cap {
                if let Some((epn, mut epage)) = cache.pop_lru() {
                    let _ = self.evict_cache(file, epn, &mut epage)?;
                }
            }
            let mut np = PageCache::new()?;
            let nd = np.data();
            nd.fill(0);
            let chunk_end = (off + PAGE_SIZE).min(read);
            nd[..chunk_end - off].copy_from_slice(&buf[off..chunk_end]);
            cache.put(next_pn, np);
        }

        Ok(evicted)
    }

    /// Invokes `f` with the cached page at `pn`, or `None` if it is not cached.
    pub fn with_page<R>(&self, pn: u32, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        f(self.shared.page_cache.lock().get_mut(&pn))
    }

    /// Invokes `f` with the cached page at `pn`, loading it from disk if absent.
    ///
    /// If loading the page causes an eviction, the evicted page is also passed
    /// to `f`.
    pub fn with_page_or_insert<R>(
        &self,
        pn: u32,
        f: impl FnOnce(&mut PageCache, Option<EvictedPage>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let mut guard = self.shared.page_cache.lock();
        let evicted = self.ensure_page_cached(self.inner.entry().as_file()?, &mut guard, pn)?;
        let page = guard.get_mut(&pn).unwrap();
        f(page, evicted)
    }

    /// Runs `f` while direct I/O is excluded from this inode's page cache.
    pub fn with_direct_io_excluded<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.shared.direct_io_lock.read();
        f()
    }

    fn with_pages<T>(
        &self,
        range: Range<u64>,
        page_initial: impl FnOnce(&FileNode) -> VfsResult<T>,
        mut load_page: impl FnMut(u64, &Range<usize>) -> bool,
        mut page_each: impl FnMut(T, &mut PageCache, u64, Range<usize>) -> VfsResult<T>,
    ) -> VfsResult<T> {
        let file = self.inner.entry().as_file()?;
        let mut initial = page_initial(file)?;
        let start_page = (range.start / PAGE_SIZE as u64) as u32;
        let end_page = range.end.div_ceil(PAGE_SIZE as u64) as u32;
        let mut page_offset = (range.start % PAGE_SIZE as u64) as usize;
        for pn in start_page..end_page {
            let page_start = pn as u64 * PAGE_SIZE as u64;

            let mut guard = self.shared.page_cache.lock();
            let page_range =
                page_offset..(range.end - page_start).min(PAGE_SIZE as u64) as usize;
            let load_from_file = load_page(page_start, &page_range);
            self.ensure_page_cached_with(file, &mut guard, pn, load_from_file)?;
            let page = guard.get_mut(&pn).unwrap();

            initial = page_each(initial, page, page_start, page_range)?;
            page_offset = 0;
        }

        Ok(initial)
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let len = self.inner.len()?;
        let end = (offset + dst.remaining_mut() as u64).min(len);
        if end <= offset {
            return Ok(0);
        }
        self.with_pages(
            offset..end,
            |_| Ok(0),
            |_, _| true,
            |read, page, _page_start, range| {
                let len = range.end - range.start;
                dst.write(&page.data()[range.start..range.end])?;
                Ok(read + len)
            },
        )
    }

    fn write_at_locked(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let end = offset + buf.remaining() as u64;
        self.with_pages(
            offset..end,
            |file| {
                if end > file.len()? {
                    file.set_len(end)?;
                }
                Ok(0)
            },
            |_, range| !(range.start == 0 && range.end == PAGE_SIZE),
            |written, page, _page_start, range| {
                let len = range.end - range.start;
                buf.read(&mut page.data()[range.start..range.end])?;
                page.mark_dirty();
                Ok(written + len)
            },
        )
    }

    /// Writes `buf` to the file at `offset`.
    pub fn write_at(&self, buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let _guard = self.shared.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    /// Appends `buf` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, buf: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let _guard = self.shared.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        self.write_at_locked(buf, len)
            .map(|written| (written, len + written as u64))
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
        if self.in_memory && old_len > len {
            self.flush_dirty_cache(file)?;
        }
        file.set_len(len)?;

        let old_last_page = (old_len / PAGE_SIZE as u64) as u32;
        let new_last_page = (len / PAGE_SIZE as u64) as u32;
        if old_len < len {
            let mut guard = self.shared.page_cache.lock();
            if let Some(page) = guard.get_mut(&old_last_page) {
                let page_start = old_last_page as u64 * PAGE_SIZE as u64;
                let old_page_offset = (old_len - page_start) as usize;
                let new_page_offset = (len - page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[old_page_offset..new_page_offset].fill(0);
            }
        } else if old_last_page > new_last_page {
            // For truncating, we need to remove all pages that are beyond the
            // new length
            // TODO(mivik): can this be more efficient?
            let keys = {
                let mut guard = self.shared.page_cache.lock();
                if let Some(page) = guard.get_mut(&new_last_page) {
                    let page_start = new_last_page as u64 * PAGE_SIZE as u64;
                    let new_page_offset =
                        len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                    page.data()[new_page_offset..].fill(0);
                }
                guard
                    .iter()
                    .map(|(k, _)| *k)
                    .filter(|it| *it > new_last_page)
                    .collect::<Vec<_>>()
            };
            for pn in keys {
                if let Some(mut page) = self.pop_cached_page(pn) {
                    // Don't write back pages since they're discarded.
                    page.clear_dirty();
                    let _ = self.evict_cache(file, pn, &mut page)?;
                }
            }
        } else if old_len > len {
            let mut guard = self.shared.page_cache.lock();
            if let Some(page) = guard.get_mut(&new_last_page) {
                let page_start = new_last_page as u64 * PAGE_SIZE as u64;
                let new_page_offset = len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[new_page_offset..].fill(0);
                // Preserve dirty state for retained bytes; writeback clamps to the new length.
                page.mark_dirty();
            }
        }
        Ok(())
    }

    /// Flushes all cached pages back to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        let _direct_guard = self.shared.direct_io_lock.read();
        if self.in_memory {
            self.sync_in_memory_cache();
            return Ok(());
        }
        let file = self.inner.entry().as_file()?;
        self.flush_dirty_cache(file)?;
        file.sync(data_only)?;
        Ok(())
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        &self.inner
    }
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shared) > 1 {
            // If there are other references to this cached file, we don't
            // need to drop it.
            return;
        }
        if self.shared.unlinked.load(Ordering::Acquire) {
            self.discard_cache();
            return;
        }
        let file = match self.inner.entry().as_file() {
            Ok(file) => file,
            Err(err) => {
                warn!("Failed to access file for cache drop: {err:?}");
                return;
            }
        };
        if let Err(err) = self.drain_cache(file) {
            // `close(2)` is not required to persist data to the device. Keep
            // the explicit flush path on `fsync`/`fdatasync`, and only make
            // dirty cached pages visible to the inode here.
            warn!("Failed to drain cached file pages on drop: {err:?}");
        }
    }
}

/// Low-level interface for file operations.
#[derive(Clone)]
pub enum FileBackend {
    /// File I/O goes through the page cache.
    Cached(CachedFile),
    /// File I/O bypasses the page cache and hits the VFS directly.
    Direct(Location),
}

impl FileBackend {
    const DIRECT_IO_CHUNK: usize = PAGE_SIZE;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    fn direct_io_shared(location: &Location) -> Arc<CachedFileShared> {
        cached_file_shared_for_location_or_create(location)
    }

    fn sync_direct_cache(location: &Location) -> VfsResult<()> {
        sync_and_invalidate_cached_file_pages(location)
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at(dst, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while dst.remaining_mut() > 0 {
                    let limit = dst.remaining_mut().min(chunk.len());
                    let read = file.read_at(&mut chunk[..limit], offset)?;
                    if read == 0 {
                        break;
                    }
                    let written = dst.write(&chunk[..read])?;
                    if written == 0 {
                        break;
                    }
                    offset += written as u64;
                    total += written;
                    if written < read || read < limit {
                        break;
                    }
                }

                Ok(total)
            }
        }
    }

    /// Writes `src` to the file at `offset`.
    pub fn write_at(&self, mut src: impl Read + IoBuf, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at(src, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while src.remaining() > 0 {
                    let limit = src.remaining().min(chunk.len());
                    let read = src.read(&mut chunk[..limit])?;
                    if read == 0 {
                        break;
                    }
                    let written = file.write_at(&chunk[..read], offset)?;
                    if written == 0 {
                        break;
                    }
                    offset += written as u64;
                    total += written;
                    if written < read {
                        break;
                    }
                }

                if total > 0 {
                    Self::sync_direct_cache(loc)?;
                }
                Ok(total)
            }
        }
    }

    /// Appends `src` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, mut src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append(src),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let mut total = 0;
                let mut end = file.len()?;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while src.remaining() > 0 {
                    let limit = src.remaining().min(chunk.len());
                    let read = src.read(&mut chunk[..limit])?;
                    if read == 0 {
                        break;
                    }
                    let (written, new_end) = file.append(&chunk[..read])?;
                    if written == 0 {
                        break;
                    }
                    total += written;
                    end = new_end;
                    if written < read {
                        break;
                    }
                }

                if total > 0 {
                    Self::sync_direct_cache(loc)?;
                }
                Ok((total, end))
            }
        }
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        match self {
            Self::Cached(cached) => cached.location(),
            Self::Direct(loc) => loc,
        }
    }

    /// Flushes cached data (and optionally metadata) to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.sync(data_only),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                loc.entry().as_file()?.sync(data_only)
            }
        }
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.set_len(len),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                loc.entry().as_file()?.set_len(len)?;
                Self::sync_direct_cache(loc)
            }
        }
    }
}

/// Provides `std::fs::File`-like interface.
pub struct File {
    inner: FileBackend,
    flags: FileFlags,
    position: Option<Mutex<u64>>,
    #[cfg(feature = "times")]
    access_flags: AtomicU8,
}

impl File {
    #[cfg(feature = "times")]
    fn record_time_flags(&self, flags: u8) {
        self.access_flags.fetch_or(flags, Ordering::AcqRel);
    }

    #[cfg(feature = "times")]
    fn flush_times(&self) {
        let flags = self.access_flags.swap(0, Ordering::AcqRel);
        if flags == 0 {
            return;
        }

        let now = axhal::time::wall_time();
        let mut update = MetadataUpdate::default();
        if flags & 1 != 0 {
            update.atime = Some(now);
        }
        if flags & 2 != 0 {
            update.mtime = Some(now);
            update.ctime = Some(now);
        }
        if let Err(err) = self.inner.location().update_metadata(update) {
            warn!("Failed to update file times: {err:?}");
            self.access_flags.fetch_or(flags, Ordering::AcqRel);
        }
    }

    /// Creates a new [`File`] from a [`FileBackend`] and access flags.
    pub fn new(inner: FileBackend, flags: FileFlags) -> Self {
        let position = if inner.location().flags().contains(NodeFlags::STREAM) {
            None
        } else {
            Some(Mutex::new(if flags.contains(FileFlags::APPEND) {
                inner.location().len().unwrap_or_default()
            } else {
                0
            }))
        };
        Self {
            inner,
            flags,
            position,
            #[cfg(feature = "times")]
            access_flags: AtomicU8::new(0),
        }
    }

    /// Opens an existing file for reading.
    pub fn open(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new()
            .read(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    /// Opens a file for writing, creating it if it does not exist and
    /// truncating it if it does.
    pub fn create(context: &FsContext, path: impl AsRef<Path>) -> VfsResult<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(context, path.as_ref())
            .and_then(OpenResult::into_file)
    }

    /// Checks that the file has the required `flags` and returns the backend.
    pub fn access(&self, flags: FileFlags) -> VfsResult<&FileBackend> {
        if self.flags.contains(flags) && !self.is_path() {
            Ok(&self.inner)
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    /// Returns `true` if this is a path-only handle (no I/O permitted).
    pub fn is_path(&self) -> bool {
        self.flags.contains(FileFlags::PATH)
    }

    /// Returns the access flags this file was opened with.
    pub fn flags(&self) -> FileFlags {
        self.flags
    }

    /// Returns a reference to the underlying [`FileBackend`].
    pub fn backend(&self) -> VfsResult<&FileBackend> {
        self.access(FileFlags::empty())?;
        Ok(&self.inner)
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        self.inner.location()
    }

    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        let requested = dst.remaining_mut();
        let read = self.access(FileFlags::READ)?.read_at(dst, offset)?;
        #[cfg(feature = "times")]
        if requested > 0
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(read)
    }

    /// Writes a number of bytes starting from a given offset.
    pub fn write_at(&self, src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let written = self.access(FileFlags::WRITE)?.write_at(src, offset)?;
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    /// Attempts to sync OS-internal file content and metadata to disk.
    ///
    /// If `data_only` is `true`, only the file data is synced, not the
    /// metadata.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        self.inner.sync(data_only)
    }

    /// Reads data from the current position, advancing the cursor.
    pub fn read(&self, dst: impl Write + IoBufMut) -> axio::Result<usize> {
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at(dst, 0)
        }
    }

    /// Writes data at the current position (or appends), advancing the cursor.
    pub fn write(&self, src: impl Read + IoBuf) -> axio::Result<usize> {
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if let Ok(f) = self.access(FileFlags::APPEND) {
                f.append(src)
                    .inspect(|(written, _)| {
                        #[cfg(feature = "times")]
                        if *written > 0 {
                            self.record_time_flags(2);
                            self.flush_times();
                        }
                    })
                    .map(|(written, new_size)| {
                        *pos = new_size;
                        written
                    })
            } else {
                self.write_at(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            }
        } else {
            self.write_at(src, 0)
        }
    }

    /// Flushes any internally buffered data. Currently a no-op.
    pub fn flush(&self) -> axio::Result {
        self.access(FileFlags::empty())?;
        Ok(())
    }
}

impl Read for &File {
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        (*self).read(buf)
    }
}

impl Write for &File {
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        (*self).write(buf)
    }

    fn flush(&mut self) -> axio::Result {
        (*self).flush()
    }
}

impl Seek for &File {
    fn seek(&mut self, pos: SeekFrom) -> axio::Result<u64> {
        self.access(FileFlags::empty())?;

        if let Some(guard) = self.position.as_ref() {
            let mut guard = guard.lock();
            let new_pos = match pos {
                SeekFrom::Start(pos) => pos,
                SeekFrom::End(off) => {
                    let size = self.access(FileFlags::empty())?.location().len()?;
                    size.checked_add_signed(off).ok_or(VfsError::InvalidInput)?
                }
                SeekFrom::Current(off) => guard
                    .checked_add_signed(off)
                    .ok_or(VfsError::InvalidInput)?,
            };
            *guard = new_pos;
            Ok(new_pos)
        } else {
            Ok(0)
        }
    }
}

impl Pollable for File {
    fn poll(&self) -> IoEvents {
        self.inner.location().poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner.location().register(context, events)
    }
}

#[cfg(feature = "times")]
impl Drop for File {
    fn drop(&mut self) {
        self.flush_times();
    }
}
