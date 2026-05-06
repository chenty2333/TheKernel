use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use core::{num::NonZeroUsize, ops::Range, task::Context};

use axalloc::{UsageKind, global_allocator};
use axfs_ng_vfs::{
    FileNode, Location, MetadataUpdate, NodeFlags, NodePermission, NodeType, VfsError, VfsResult,
    path::Path,
};
use axhal::mem::{PhysAddr, VirtAddr, total_ram_size, virt_to_phys};
use axio::{SeekFrom, prelude::*};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use lru::LruCache;
use spin::RwLock;

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
        Ok(match (self.read, self.write, self.append) {
            (true, false, false) => FileFlags::READ,
            (false, true, false) => FileFlags::WRITE,
            (true, true, false) => FileFlags::READ | FileFlags::WRITE,
            (false, _, true) => FileFlags::WRITE | FileFlags::APPEND,
            (true, _, true) => FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
            (false, false, false) => return Err(VfsError::InvalidInput),
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        if self.path {
            return !self.read
                && !self.write
                && !self.append
                && !self.truncate
                && !self.create
                && !self.create_new;
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

    /// Marks this page as dirty so it will be flushed on eviction.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
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
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}

type EvictListenerFn = Box<dyn Fn(u32, &PageCache) + Send + Sync>;

// ---- Global page cache budget ----
//
// Tracks the total number of cached pages across all CachedFileShared
// instances.  When the budget is exceeded, clean pages are evicted from
// the least-recently-used caches.

static GLOBAL_PAGE_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn global_page_cache_budget_pages() -> usize {
    const MIB: usize = 1024 * 1024;
    let ram = total_ram_size();
    if ram <= 256 * MIB {
        1024                          // 4 MiB
    } else if ram <= 512 * MIB {
        8192                          // 32 MiB
    } else if ram <= 1024 * MIB {
        32768                         // 128 MiB
    } else {
        65536                         // 256 MiB
    }
}

fn inc_global_page_count() {
    GLOBAL_PAGE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

fn dec_global_page_count() {
    GLOBAL_PAGE_COUNT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
}

fn global_page_count() -> usize {
    GLOBAL_PAGE_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}

/// Evict clean pages from the least-recently-used registered cache until
/// the global budget is met.  Called when a new page allocation would
/// exceed the budget.
fn reclaim_global_budget() {
    let budget = global_page_cache_budget_pages();
    if global_page_count() <= budget {
        return;
    }

    let mut reg = PAGE_CACHE_REGISTRY.lock();
    reg.retain(|w| w.upgrade().is_some());

    // Sort caches by last_access_tick so we reclaim from the coldest ones.
    let mut caches: Vec<_> = reg.iter().filter_map(|w| w.upgrade()).collect();
    caches.sort_by_key(|c| c.last_access_tick());

    let to_free = global_page_count().saturating_sub(budget);
    let mut freed = 0;

    for cache in &caches {
        if freed >= to_free {
            break;
        }
        // Pop clean (non-dirty) pages from the LRU tail.
        loop {
            let Some((pn, mut page)) = cache.pop_lru_if_clean() else {
                break;
            };
            // Notify evict listeners before dropping the page so that mmap
            // backends can unmap PTEs pointing to this physical page.
            for listener in cache.evict_listeners.lock().iter() {
                (listener.listener)(pn, &page);
            }
            dec_global_page_count();
            drop(page);
            freed += 1;
            if freed >= to_free {
                break;
            }
        }
    }
}

fn per_file_page_cache_capacity() -> NonZeroUsize {
    const MIB: usize = 1024 * 1024;
    const GIB: usize = 1024 * MIB;
    let ram = total_ram_size();
    let pages = if ram <= 512 * MIB {
        64
    } else if ram <= 2 * GIB {
        256
    } else {
        let extra_gib = (ram - 2 * GIB) / GIB;
        512usize.saturating_add(extra_gib.saturating_mul(128))
    };
    NonZeroUsize::new(pages).unwrap()
}

struct EvictListener {
    listener: EvictListenerFn,
    link: LinkedListAtomicLink,
}

intrusive_adapter!(EvictListenerAdapter = Box<EvictListener>: EvictListener { link: LinkedListAtomicLink });

struct CachedFileShared {
    page_cache: Mutex<LruCache<u32, PageCache>>,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    fs_name: String,
    inner: Location,
    last_access_tick: AtomicU64,
    in_memory: bool,
}

impl CachedFileShared {
    pub fn new(fs_name: &str, location: &Location) -> Self {
        Self {
            page_cache: Mutex::new(new_bounded_page_cache_store()),
            evict_listeners: Mutex::new(LinkedList::default()),
            fs_name: fs_name.into(),
            inner: location.clone(),
            last_access_tick: AtomicU64::new(0),
            in_memory: fs_name == "tmpfs",
        }
    }

    pub fn new_unbounded(fs_name: &str, location: &Location) -> Self {
        Self {
            page_cache: Mutex::new(new_unbounded_page_cache_store()),
            evict_listeners: Mutex::new(LinkedList::default()),
            fs_name: fs_name.into(),
            inner: location.clone(),
            last_access_tick: AtomicU64::new(0),
            in_memory: fs_name == "tmpfs",
        }
    }

    fn fs_name(&self) -> &str {
        &self.fs_name
    }

    fn last_access_tick(&self) -> u64 {
        self.last_access_tick.load(Ordering::Relaxed)
    }

    fn touch(&self) {
        // A simple monotonically increasing tick for LRU ordering.
        static TICK: AtomicU64 = AtomicU64::new(0);
        self.last_access_tick
            .store(TICK.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Pop the least-recently-used page from the cache ONLY if it is clean.
    fn pop_lru_if_clean(&self) -> Option<(u32, PageCache)> {
        let mut guard = self.page_cache.lock();
        let lru = guard.iter().find_map(|(pn, page)| {
            (!page.dirty).then_some(*pn)
        })?;
        guard.pop(&lru)
    }
}

#[cfg(target_arch = "loongarch64")]
fn new_bounded_page_cache_store() -> LruCache<u32, PageCache> {
    LruCache::unbounded()
}

#[cfg(not(target_arch = "loongarch64"))]
fn new_bounded_page_cache_store() -> LruCache<u32, PageCache> {
    LruCache::new(per_file_page_cache_capacity())
}

#[cfg(target_arch = "loongarch64")]
fn new_unbounded_page_cache_store() -> LruCache<u32, PageCache> {
    LruCache::unbounded()
}

#[cfg(not(target_arch = "loongarch64"))]
fn new_unbounded_page_cache_store() -> LruCache<u32, PageCache> {
    LruCache::unbounded()
}

/// A file handle with an LRU page cache for buffered I/O.
pub struct CachedFile {
    inner: Location,
    shared: Arc<CachedFileShared>,
    in_memory: bool,
    /// Only one thread can append to the file at a time, while multiple writers
    /// are permitted.
    append_lock: RwLock<()>,
}

impl Clone for CachedFile {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
            in_memory: self.in_memory,
            append_lock: RwLock::new(()),
        }
    }
}

struct FileUserData(Arc<CachedFileShared>);

impl FileUserData {
    pub fn get(&self) -> Arc<CachedFileShared> {
        self.0.clone()
    }
}

// ---- Page cache registry ----
//
// Every CachedFileShared is tracked here so that sync / syncfs can find
// dirty caches that outlive the last fd (held alive by FileUserData on
// the inode's Location).

use axsync::Mutex as RegistryMutex;

static PAGE_CACHE_REGISTRY: RegistryMutex<Vec<alloc::sync::Weak<CachedFileShared>>> =
    RegistryMutex::new(Vec::new());

fn register_cache(cache: &Arc<CachedFileShared>) {
    let mut reg = PAGE_CACHE_REGISTRY.lock();
    reg.retain(|w| w.upgrade().is_some());
    reg.push(Arc::downgrade(cache));
}

/// Discard cached pages for `location` in `[offset, offset+len)` without
/// flushing dirty data.  For use after fallocate / truncate operations that
/// have already modified the backing store.
pub fn discard_page_cache(location: &Location, offset: u64, len: u64) {
    let guard = location.user_data();
    if let Some(ud) = guard.get::<FileUserData>() {
        let shared = ud.get();
        let cached = CachedFile {
            inner: location.clone(),
            shared,
            in_memory: location.filesystem().name() == "tmpfs",
            append_lock: RwLock::new(()),
        };
        cached.invalidate_range(offset, len);
    }
}

/// Flush dirty pages for `location` and discard the affected cache range.
/// Called by O_DIRECT writes and external truncate operations to keep the
/// page cache coherent with the on-disk state.
pub fn invalidate_page_cache(location: &Location, offset: u64, len: u64) -> VfsResult<()> {
    let guard = location.user_data();
    if let Some(ud) = guard.get::<FileUserData>() {
        let shared = ud.get();
        let cached = CachedFile {
            inner: location.clone(),
            shared,
            in_memory: location.filesystem().name() == "tmpfs",
            append_lock: RwLock::new(()),
        };
        cached.flush_and_invalidate_range(offset, len)?;
    }
    Ok(())
}

/// Discard all cached pages for `location` (e.g. when a file is unlinked
/// or its inode is replaced).
pub fn invalidate_page_cache_all(location: &Location) {
    let guard = location.user_data();
    if let Some(ud) = guard.get::<FileUserData>() {
        let shared = ud.get();
        let mut guard = shared.page_cache.lock();
        let count = guard.iter().count();
        guard.clear();
        if count > 0 {
            GLOBAL_PAGE_COUNT.fetch_sub(count, Ordering::Relaxed);
        }
    }
}

/// Walk all registered page caches and flush dirty pages.
/// Called by `sync()` and at shutdown.
pub fn sync_all_page_caches() -> VfsResult<()> {
    // Collect strong references under the registry lock, then flush outside
    // to avoid holding the registry lock across filesystem I/O.
    let caches: Vec<Arc<CachedFileShared>> = {
        let mut reg = PAGE_CACHE_REGISTRY.lock();
        reg.retain(|w| w.upgrade().is_some());
        reg.iter().filter_map(|w| w.upgrade()).collect()
    };
    for cache in &caches {
        flush_cache_dirty(cache)?;
    }
    Ok(())
}

/// Flush dirty pages whose filesystem matches `fs_name`.
/// Called by `syncfs(fd)`.
pub fn sync_page_caches_for_fs(fs_name: &str) -> VfsResult<()> {
    let caches: Vec<Arc<CachedFileShared>> = {
        let mut reg = PAGE_CACHE_REGISTRY.lock();
        reg.retain(|w| w.upgrade().is_some());
        reg.iter()
            .filter_map(|w| w.upgrade())
            .filter(|c| c.fs_name() == fs_name)
            .collect()
    };
    for cache in &caches {
        flush_cache_dirty(cache)?;
    }
    Ok(())
}

fn flush_cache_dirty(cache: &CachedFileShared) -> VfsResult<()> {
    if cache.in_memory {
        return Ok(());
    }
    let file = cache.inner.entry().as_file()?;
    let mut guard = cache.page_cache.lock();
    let dirty_pages: Vec<u32> = guard
        .iter()
        .filter_map(|(pn, page)| if page.dirty { Some(*pn) } else { None })
        .collect();
    for pn in dirty_pages {
        if let Some(page) = guard.get_mut(&pn) {
            if page.dirty {
                let page_start = pn as u64 * PAGE_SIZE as u64;
                let len =
                    (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
                if len > 0 {
                    file.write_at(&page.data()[..len], page_start)?;
                }
                page.dirty = false;
            }
        }
    }
    Ok(())
}

impl CachedFile {
    /// Returns an existing cached file for `location`, or creates a new one.
    pub fn get_or_create(location: Location) -> Self {
        let fs_name = location.filesystem().name();
        let in_memory = fs_name == "tmpfs";

        let shared = if in_memory {
            let mut guard = location.user_data();
            let shared = if let Some(shared) = guard.get::<FileUserData>().map(|it| it.get()) {
                shared
            } else {
                let shared = Arc::new(CachedFileShared::new_unbounded(fs_name, &location));
                guard.insert(FileUserData(shared.clone()));
                register_cache(&shared);
                shared
            };
            drop(guard);
            shared
        } else {
            let mut guard = location.user_data();
            let shared = if let Some(shared) = guard.get::<FileUserData>().map(|it| it.get()) {
                shared
            } else {
                let shared = Arc::new(CachedFileShared::new(fs_name, &location));
                guard.insert(FileUserData(shared.clone()));
                register_cache(&shared);
                shared
            };
            drop(guard);
            shared
        };

        Self {
            inner: location,
            shared,
            in_memory,
            append_lock: RwLock::new(()),
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
            listener: Box::new(listener),
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

    fn evict_cache(&self, file: &FileNode, pn: u32, page: &mut PageCache) -> VfsResult<()> {
        for listener in self.shared.evict_listeners.lock().iter() {
            (listener.listener)(pn, page);
        }
        if page.dirty {
            let page_start = pn as u64 * PAGE_SIZE as u64;
            let len = (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
            if len > 0 {
                file.write_at(&page.data()[..len], page_start)?;
            }
            page.dirty = false;
        }
        Ok(())
    }

    /// Remove and drop a page from the cache, notifying evict listeners and
    /// maintaining the global page counter.  This is the single path through
    /// which all page removals should flow.
    fn remove_page(&self, file: &FileNode, pn: u32, page: &mut PageCache) -> VfsResult<()> {
        self.evict_cache(file, pn, page)?;
        dec_global_page_count();
        // Page is consumed by the caller who already removed it from the LRU.
        Ok(())
    }

    fn pop_lru_page(&self) -> Option<(u32, PageCache)> {
        self.shared.page_cache.lock().pop_lru()
    }

    fn pop_cached_page(&self, pn: u32) -> Option<PageCache> {
        self.shared.page_cache.lock().pop(&pn)
    }

    fn drain_cache(&self, file: &FileNode) -> VfsResult<()> {
        while let Some((pn, mut page)) = self.pop_lru_page() {
            self.remove_page(file, pn, &mut page)?;
        }
        Ok(())
    }

    fn ensure_page_cached(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
    ) -> VfsResult<Option<(u32, PageCache)>> {
        if cache.contains(&pn) {
            self.shared.touch();
            return Ok(None);
        }

        // Budget enforcement moved to callers — reclaim_global_budget()
        // must run BEFORE the per-cache lock is taken to avoid deadlock.

        let mut evicted = None;
        if cache.len() == cache.cap().get() {
            // Cache is full, remove the least recently used page
            if let Some((pn, mut page)) = cache.pop_lru() {
                self.evict_cache(file, pn, &mut page)?;
                dec_global_page_count();
                evicted = Some((pn, page));
            }
        }

        // Page not in cache, read it
        let mut page = PageCache::new()?;
        inc_global_page_count();
        if self.in_memory {
            page.data().fill(0);
        } else {
            let data = page.data();
            data.fill(0);
            let read = file.read_at(data, pn as u64 * PAGE_SIZE as u64)?;
            if read < PAGE_SIZE {
                data[read..].fill(0);
            }
        }
        cache.put(pn, page);
        Ok(evicted)
    }

    /// Invokes `f` with the cached page at `pn`, or `None` if it is not cached.
    pub fn with_page<R>(&self, pn: u32, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        f(self.shared.page_cache.lock().get_mut(&pn))
    }

    /// Invokes `f` with the cached page at `pn`, loading it from disk if absent.
    ///
    /// If loading the page causes an eviction, the evicted `(page_number, page)`
    /// pair is also passed to `f`.
    pub fn with_page_or_insert<R>(
        &self,
        pn: u32,
        f: impl FnOnce(&mut PageCache, Option<(u32, PageCache)>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        // Enforce the global budget before taking the per-file lock so that
        // reclaim never tries to lock the cache we already hold.
        reclaim_global_budget();
        let mut guard = self.shared.page_cache.lock();
        let evicted = self.ensure_page_cached(self.inner.entry().as_file()?, &mut guard, pn)?;
        let page = guard.get_mut(&pn).unwrap();
        f(page, evicted)
    }

    fn with_pages<T>(
        &self,
        range: Range<u64>,
        page_initial: impl FnOnce(&FileNode) -> VfsResult<T>,
        mut page_each: impl FnMut(T, &mut PageCache, u64, Range<usize>) -> VfsResult<T>,
    ) -> VfsResult<T> {
        let file = self.inner.entry().as_file()?;
        let mut initial = page_initial(file)?;
        let start_page = (range.start / PAGE_SIZE as u64) as u32;
        let end_page = range.end.div_ceil(PAGE_SIZE as u64) as u32;
        reclaim_global_budget();
        let mut page_offset = (range.start % PAGE_SIZE as u64) as usize;
        for pn in start_page..end_page {
            let page_start = pn as u64 * PAGE_SIZE as u64;

            let mut guard = self.shared.page_cache.lock();
            self.ensure_page_cached(file, &mut guard, pn)?;
            let page = guard.get_mut(&pn).unwrap();

            initial = page_each(
                initial,
                page,
                page_start,
                page_offset..(range.end - page_start).min(PAGE_SIZE as u64) as usize,
            )?;
            page_offset = 0;
        }

        Ok(initial)
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let end = (offset.saturating_add(dst.remaining_mut() as u64)).min(len);
        if end <= offset {
            return Ok(0);
        }
        self.with_pages(
            offset..end,
            |_| Ok(0),
            |read, page, _page_start, range| {
                let len = range.end - range.start;
                dst.write(&page.data()[range.start..range.end])?;
                Ok(read + len)
            },
        )
    }

    fn write_at_locked(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let end = offset.saturating_add(buf.remaining() as u64);
        self.with_pages(
            offset..end,
            |file| {
                if end > file.len()? {
                    file.set_len(end)?;
                }
                Ok(0)
            },
            |written, page, _page_start, range| {
                let len = range.end - range.start;
                buf.read(&mut page.data()[range.start..range.end])?;
                if !self.in_memory {
                    page.mark_dirty();
                }
                Ok(written + len)
            },
        )
    }

    /// Writes `buf` to the file at `offset`.
    pub fn write_at(&self, buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let _guard = self.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    /// Appends `buf` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, buf: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        let _guard = self.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        self.write_at_locked(buf, len)
            .map(|written| (written, len + written as u64))
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
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
                if let Some(mut page) = self.pop_cached_page(pn)
                    && !self.in_memory
                {
                    // Don't write back pages since they're discarded.
                    page.dirty = false;
                    self.evict_cache(file, pn, &mut page)?;
                }
            }
        } else if old_len > len {
            let mut guard = self.shared.page_cache.lock();
            if let Some(page) = guard.get_mut(&new_last_page) {
                let page_start = new_last_page as u64 * PAGE_SIZE as u64;
                let new_page_offset = len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[new_page_offset..].fill(0);
            }
        }
        Ok(())
    }

    /// Flushes all cached pages back to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        if self.in_memory {
            return Ok(());
        }
        let file = self.inner.entry().as_file()?;
        self.drain_cache(file)?;
        file.sync(data_only)?;
        Ok(())
    }

    /// Write back dirty pages overlapping `[offset, offset + len)` without
    /// evicting them from the cache.  Used before Direct I/O reads to ensure
    /// that the read sees the latest data.
    pub fn flush_dirty_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        if self.in_memory || len == 0 {
            return Ok(());
        }
        let file = self.inner.entry().as_file()?;
        let start_pn = (offset / PAGE_SIZE as u64) as u32;
        let end_pn = offset.saturating_add(len).div_ceil(PAGE_SIZE as u64) as u32;
        let mut guard = self.shared.page_cache.lock();
        for pn in start_pn..end_pn {
            if let Some(page) = guard.get_mut(&pn) {
                if page.dirty {
                    let page_start = pn as u64 * PAGE_SIZE as u64;
                    let page_len =
                        (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
                    if page_len > 0 {
                        file.write_at(&page.data()[..page_len], page_start)?;
                    }
                    page.dirty = false;
                }
            }
        }
        Ok(())
    }

    /// Discard cached pages overlapping `[offset, offset + len)`.
    /// Used after Direct I/O writes to prevent stale-cache reads.
    pub fn invalidate_range(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let start_pn = (offset / PAGE_SIZE as u64) as u32;
        let end_pn = offset.saturating_add(len).div_ceil(PAGE_SIZE as u64) as u32;
        let mut guard = self.shared.page_cache.lock();
        for pn in start_pn..end_pn {
            if let Some(mut page) = guard.pop(&pn) {
                // Notify evict listeners (mmap backends) before dropping so
                // they can unmap their PTEs that reference this physical page.
                drop(guard);
                for listener in self.shared.evict_listeners.lock().iter() {
                    (listener.listener)(pn, &page);
                }
                dec_global_page_count();
                drop(page);
                guard = self.shared.page_cache.lock();
            }
        }
    }

    /// Write back and discard the range.  Used before Direct I/O writes
    /// to preserve coherence: flush any dirty data that's newer than what
    /// was written directly, then invalidate so that the next cached read
    /// goes to disk.
    pub fn flush_and_invalidate_range(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.flush_dirty_range(offset, len)?;
        self.invalidate_range(offset, len);
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
        if self.in_memory {
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
    const DIRECT_IO_CHUNK: usize = 64 * 1024;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    fn get_cached_for(loc: &Location) -> Option<CachedFile> {
        let guard = loc.user_data();
        guard
            .get::<FileUserData>()
            .map(|ud| CachedFile {
                inner: loc.clone(),
                shared: ud.get(),
                in_memory: loc.filesystem().name() == "tmpfs",
                append_lock: RwLock::new(()),
            })
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at(dst, offset),
            Self::Direct(loc) => {
                // Flush dirty cached pages in the read range so the direct
                // read sees the latest writeback data.
                if let Some(cached) = Self::get_cached_for(loc) {
                    cached.flush_dirty_range(offset, dst.remaining_mut() as u64)?;
                }

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
                // Flush any dirty cached pages overlapping the write range,
                // then invalidate them so the next cached read goes to disk.
                let data_len = src.remaining() as u64;
                if let Some(cached) = Self::get_cached_for(loc) {
                    cached.flush_and_invalidate_range(offset, data_len)?;
                }

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

                Ok(total)
            }
        }
    }

    /// Appends `src` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, mut src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append(src),
            Self::Direct(loc) => {
                let old_len = loc.entry().as_file()?.len()?;
                // Flush dirty cached pages that overlap the old EOF page
                // before appending beyond it through the direct backend.
                let data_len = src.remaining() as u64;
                if let Some(cached) = Self::get_cached_for(loc) {
                    cached.flush_and_invalidate_range(old_len, data_len)?;
                }

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
            Self::Direct(loc) => loc.entry().as_file()?.sync(data_only),
        }
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.set_len(len),
            Self::Direct(loc) => {
                let old_len = loc.entry().as_file()?.len()?;
                loc.entry().as_file()?.set_len(len)?;
                // If we shortened the file, invalidate cached pages beyond
                // the new length.  For the partial last page, zero the tail
                // and flush any dirty prefix — the retained bytes are still
                // valid and must not be discarded.
                if len < old_len {
                    if let Some(cached) = Self::get_cached_for(loc) {
                        let new_last_page = (len / PAGE_SIZE as u64) as u32;
                        let old_last_page = (old_len / PAGE_SIZE as u64) as u32;
                        // Full pages beyond new last page
                        if old_last_page > new_last_page {
                            cached.invalidate_range(
                                (new_last_page + 1) as u64 * PAGE_SIZE as u64,
                                (old_last_page - new_last_page) as u64 * PAGE_SIZE as u64,
                            );
                        }
                        // Partial last page: zero the tail, preserve the prefix
                        if let Some(page) = cached.shared.page_cache.lock().get_mut(&new_last_page) {
                            let page_start = new_last_page as u64 * PAGE_SIZE as u64;
                            let end_offset = len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                            page.data()[end_offset..].fill(0);
                        }
                    }
                }
                Ok(())
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
        let read = self.access(FileFlags::READ)?.read_at(dst, offset)?;
        #[cfg(feature = "times")]
        if read > 0 {
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
            self.record_time_flags(3);
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
                            self.record_time_flags(3);
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
