use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    hint::spin_loop,
    mem::ManuallyDrop,
    num::NonZeroUsize,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axalloc::{UsageKind, global_allocator};
use axdriver::{
    AsyncBlockWaitPolicy, prelude::BlockResetOutcome, virtio_async_block_enabled,
    virtio_async_block_wait_policy,
};
#[cfg(feature = "times")]
use axfs_ng_vfs::{MetadataUpdate, Timestamp};
#[cfg(not(feature = "ext4"))]
pub use axfs_ng_vfs::PhysicalIoNotSubmittedReason;
use axfs_ng_vfs::{
    AsyncVectoredWriteOutcome, FileExtentMap, FileNode, FilesystemOps, Location, Mountpoint,
    NodeFlags, NodePermission, NodeType,
    PhysicalIoNotSubmittedReason as PhysicalIoAttemptNotSubmittedReason, VfsError, VfsResult,
    WeakDirEntry, WritebackAnchor, path::Path,
};
pub use axfs_ng_vfs::{PhysicalIoAttempt, PhysicalIoSegment};
use axhal::mem::{PhysAddr, VirtAddr, total_ram_size};
#[cfg(target_os = "none")]
use axhal::mem::{phys_to_virt, virt_to_phys};
use axio::{SeekFrom, prelude::*};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
#[cfg(target_os = "none")]
use axsync::Mutex;
use axtask::WaitQueue;
use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use lru::LruCache;
#[cfg(feature = "ext4")]
use lwext4_rust::PhysicalIoEffect as Ext4PhysicalIoEffect;
#[cfg(feature = "ext4")]
pub use lwext4_rust::{
    PhysicalIoCompletion, PhysicalIoCompletionOutcome, PhysicalIoEffectState,
    PhysicalIoNotSubmittedReason, PhysicalIoOperation, PhysicalIoPendingReason, PhysicalIoPlan,
    PhysicalIoPublication, PhysicalIoPublishOutcome, PhysicalIoSettlement,
};
#[cfg(not(target_os = "none"))]
use spin::Mutex;
use spin::{Once, RwLock};

use super::{FsContext, PathwalkPolicy};

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

/// Selects where an ordinary write commits independently of a file's mutable
/// default append status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePlacement {
    /// Write at the open file description's current position and advance it.
    Current,
    /// Atomically append at the file's end and move the description position
    /// to the resulting end.
    End,
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
    no_data: bool,
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
            no_data: false,
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

    /// Opens the object without granting data read or write access.
    ///
    /// This is distinct from a path-only handle: filesystem open callbacks
    /// still run and non-data operations such as metadata queries or device
    /// control may remain available to the embedding kernel.
    pub fn no_data(&mut self, no_data: bool) -> &mut Self {
        self.no_data = no_data;
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

    fn _open(&self, loc: Location, apply_truncate: bool) -> VfsResult<OpenResult> {
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
        // A path-only handle names an object but does not open the underlying
        // filesystem file. This mirrors Linux O_PATH: no filesystem open
        // callback, device/FIFO side effect, or ordinary open notification is
        // implied by constructing the handle.
        if !flags.contains(FileFlags::PATH) {
            loc.open(
                flags.contains(FileFlags::READ),
                flags.contains(FileFlags::WRITE),
            )?;
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
            if self.truncate && apply_truncate {
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
        self._open(loc, true)
    }

    /// Opens a resolved location while deferring an `O_TRUNC`-style length
    /// mutation to the caller.
    ///
    /// This lets an embedding kernel construct and reserve every fallible
    /// open-file-description resource before the destructive truncate commit.
    /// The returned file has completed the filesystem open callback but has
    /// not had its length changed; callers that requested truncation must
    /// explicitly commit it through the returned file backend.
    pub fn open_loc_deferred_truncate(&self, loc: Location) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }
        self._open(loc, false)
    }

    /// Opens a file at the given path relative to the provided [`FsContext`].
    pub fn open(&self, context: &FsContext, path: impl AsRef<Path>) -> VfsResult<OpenResult> {
        self.open_with_admission(context, path, &mut |_| Ok(()))
    }

    /// Opens a file while admitting every directory traversed by path lookup.
    pub fn open_with_admission<F>(
        &self,
        context: &FsContext,
        path: impl AsRef<Path>,
        admission: &mut F,
    ) -> VfsResult<OpenResult>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        let mut allow_create =
            |_dir: &Location, _name: &str, _options: &mut axfs_ng_vfs::OpenOptions| Ok(());
        let (loc, _created) =
            self.resolve_location_with_admission(context, path, admission, &mut allow_create)?;
        self._open(loc, true)
    }

    /// Resolves or creates the exact location that an open operation would
    /// use, without constructing the high-level file backend or applying
    /// truncate semantics.
    ///
    /// A dangling final symlink is followed recursively when creation is
    /// enabled. The same path-admission callback and symlink budget are kept
    /// for the whole operation. `create_admission` is invoked with the actual
    /// directory and final name immediately before a missing component may be
    /// created.
    pub fn resolve_location_with_admission<F, C>(
        &self,
        context: &FsContext,
        path: impl AsRef<Path>,
        admission: &mut F,
        create_admission: &mut C,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &str, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
    {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        context.resolve_open_with_admission(
            path.as_ref(),
            &axfs_ng_vfs::OpenOptions {
                create: self.create,
                create_new: self.create_new,
                node_type: self.node_type,
                permission: NodePermission::from_bits_truncate(self.mode as _),
                user: self.user,
            },
            !self.no_follow,
            admission,
            create_admission,
        )
    }

    pub fn resolve_location_with_policy<F, C, P>(
        &self,
        context: &FsContext,
        path: impl AsRef<Path>,
        admission: &mut F,
        create_admission: &mut C,
        policy: &mut P,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &str, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }

        context.resolve_open_with_policy(
            path.as_ref(),
            &axfs_ng_vfs::OpenOptions {
                create: self.create,
                create_new: self.create_new,
                node_type: self.node_type,
                permission: NodePermission::from_bits_truncate(self.mode as _),
                user: self.user,
            },
            !self.no_follow,
            admission,
            create_admission,
            policy,
        )
    }

    /// Creates an anonymous inode in `dir` using this option set.
    pub fn create_anonymous_location(&self, dir: &Location, linkable: bool) -> VfsResult<Location> {
        if !self.is_valid() || self.directory || self.path {
            return Err(VfsError::InvalidInput);
        }
        dir.create_anonymous(&axfs_ng_vfs::AnonymousOptions {
            node_type: self.node_type,
            permission: NodePermission::from_bits_truncate(self.mode as _),
            user: self.user,
            linkable,
        })
    }

    pub(crate) fn to_flags(&self) -> VfsResult<FileFlags> {
        if self.path {
            return Ok(FileFlags::PATH);
        }
        let mut flags = if self.no_data {
            FileFlags::empty()
        } else {
            match (self.read, self.write, self.append) {
                (true, false, false) => FileFlags::READ,
                (false, true, false) => FileFlags::WRITE,
                (true, true, false) => FileFlags::READ | FileFlags::WRITE,
                (false, _, true) => FileFlags::WRITE | FileFlags::APPEND,
                (true, _, true) => FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
                (false, false, false) => return Err(VfsError::InvalidInput),
            }
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
                && !self.no_data
                && !self.truncate
                && !self.create
                && !self.create_new
                && !self.no_atime;
        }
        if self.no_data && (self.read || self.write || self.append) {
            return false;
        }
        if !self.no_data && !self.read && !self.write && !self.append {
            return false;
        }
        if self.directory && (self.create || self.create_new) {
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

fn page_range(page: u64, count: u64) -> Range<u64> {
    let start = page.saturating_mul(PAGE_SIZE as u64);
    let end = start.saturating_add(count.saturating_mul(PAGE_SIZE as u64));
    start..end.max(start.saturating_add(1))
}
/// Maximum sequential-read readahead window in pages.
const READAHEAD_PAGES: usize = 64;
const FADVISE_READAHEAD_QUEUE_CAPACITY: usize = 16;
/// A WILLNEED worker must not retain an unbounded file/cache lifetime after
/// the syscall returned.  The advice is best effort, so service a bounded
/// prefix of each request and let later advice/read traffic extend it.
const FADVISE_WILLNEED_MAX_PAGES: u64 = 64;
const MAX_DIRTY_WRITEBACK_PAGES: usize = 64;
const IRQ_FIRST_DIRTY_WRITEBACK_PAGES: usize = 8;
const DIRTY_WRITEBACK_SEGMENT_PAGES: usize = 16;
const IN_MEMORY_PAGE_CACHE_PAGES: usize = 1024;
const ALIGNED_BYPASS_CHUNK: usize = 64 * 1024;
const CLOSED_FILE_CACHE_RETAIN_MAX_PAGES: usize = 1024;
/// Bound every system-wide cache walk so memory reporting and pressure work
/// cannot turn one very large inode registry into an unbounded critical path.
const GLOBAL_FILE_CACHE_SCAN_LIMIT: usize = 64;
/// Share one reclaim pass across active inodes instead of draining the first
/// cache found in registry order.
const GLOBAL_FILE_CACHE_RECLAIM_PER_FILE: usize = 16;
/// Total page inspections allowed for one inode in one reclaim pass.  This is
/// shared by every successful removal so an ineligible LRU cannot be rescanned
/// once per target page.
const GLOBAL_FILE_CACHE_RECLAIM_SCAN_PER_FILE: usize = 128;
/// Bound the number of pages inspected per inode while estimating
/// MemAvailable.  Truncation only under-estimates reclaimable memory.
const GLOBAL_FILE_CACHE_ESTIMATE_PER_FILE: usize = 128;
/// One shared nonresident domain is used when no memcg hierarchy exists.
/// Keeping this budget global prevents a many-inode workload from retaining an
/// arbitrary fixed number of shadows per inode.
const MIN_FILE_CACHE_SHADOW_PAGES: usize = 64;

/// Stable identity for one cached inode generation.
///
/// The device/inode pair is only a filesystem-visible slot and can be reused
/// after unlink.  `object` is a monotonically allocated generation token
/// carried by the identity lease in both the per-inode user data and the cache
/// shared state.  The key itself is copyable and non-owning so global
/// registries cannot pin an inode or leak an idle cache entry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CachedFileIdentity {
    device: u64,
    inode: u64,
    object: u64,
}

impl CachedFileIdentity {
    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }

    pub const fn object(self) -> u64 {
        self.object
    }
}

/// The identity generation lease carries no filesystem state.  Its strong
/// reference only keeps the generation attached to a live cache/futex lease;
/// the token itself is never recycled, even if a stale copy remains in a
/// bounded scan cursor.
struct CachedFileIdentityLease {
    object: u64,
}

impl CachedFileIdentityLease {
    const fn object(&self) -> u64 {
        self.object
    }
}

type CachedFileRegistryKey = CachedFileIdentity;
static NEXT_CACHED_FILE_IDENTITY: AtomicU64 = AtomicU64::new(1);
static FILE_CACHE_REGISTRY: Once<Mutex<BTreeMap<CachedFileRegistryKey, FileUserData>>> =
    Once::new();
static FILE_CACHE_ESTIMATE_CURSOR: Once<Mutex<Option<CachedFileRegistryKey>>> = Once::new();
static FILE_CACHE_RECLAIM_CURSOR: Once<Mutex<Option<CachedFileRegistryKey>>> = Once::new();
static FILE_CACHE_RECLAIM_SCAN_EPOCH: AtomicU64 = AtomicU64::new(1);
static FILE_CACHE_NONRESIDENT_AGE: AtomicU64 = AtomicU64::new(0);
static FILE_CACHE_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);
static FILE_CACHE_ACTIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
static FILE_CACHE_SHADOWS: Once<Mutex<LruCache<CachedFileShadowKey, u64>>> = Once::new();
static FILE_CACHE_MANAGED_PAGES_ONCE: Once<()> = Once::new();
static FILE_CACHE_MANAGED_PAGES: AtomicUsize = AtomicUsize::new(0);
static ENABLE_CACHED_FILE_IO_COUNTERS: AtomicBool = AtomicBool::new(false);
static READ_BYPASS_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_HITS: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_BYTES: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_SLICE_HITS: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_SLICE_BYTES: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_REJECT_IN_MEMORY: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_REJECT_UNALIGNED: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_REJECT_CACHED: AtomicU64 = AtomicU64::new(0);
static READ_BYPASS_EOF_RACES: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_HITS: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_BYTES: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_SLICE_HITS: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_SLICE_BYTES: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_REJECT_IN_MEMORY: AtomicU64 = AtomicU64::new(0);
static WRITE_BYPASS_REJECT_UNALIGNED: AtomicU64 = AtomicU64::new(0);
static WRITE_NO_READ_INSERT_PAGES: AtomicU64 = AtomicU64::new(0);
static WRITE_NO_READ_INSERT_BYTES: AtomicU64 = AtomicU64::new(0);
static FLUSH_DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);
static FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
static RANGE_FLUSH_DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);
static RANGE_FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_HITS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_PAGES: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_BYTES: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_ERRORS: AtomicU64 = AtomicU64::new(0);
static ENABLE_ASYNC_DIRTY_FLUSH_SG: AtomicBool = AtomicBool::new(false);
static ASYNC_DIRTY_FLUSH_SG_HITS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_SG_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS: AtomicU64 = AtomicU64::new(0);
static ENABLE_CACHED_READAHEAD: AtomicBool = AtomicBool::new(false);
static READAHEAD_MISSES: AtomicU64 = AtomicU64::new(0);
static READAHEAD_WINDOWS: AtomicU64 = AtomicU64::new(0);
static READAHEAD_PAGES_LOADED: AtomicU64 = AtomicU64::new(0);
static READAHEAD_HITS: AtomicU64 = AtomicU64::new(0);
static READAHEAD_PRESSURE_SKIPS: AtomicU64 = AtomicU64::new(0);
static READAHEAD_RETIRED_UNUSED_PAGES: AtomicU64 = AtomicU64::new(0);
static SYNC_DATA_ONLY_REQUESTS: AtomicU64 = AtomicU64::new(0);
static SYNC_METADATA_REQUESTS: AtomicU64 = AtomicU64::new(0);
static SYNC_DATA_ONLY_METADATA_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static RANGE_INVALIDATE_PAGES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_HITS: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_PAGES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_REOPEN_HITS: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_RELEASES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_TRIM_RELEASES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_TRIM_PAGES: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAIN_EPOCH: AtomicU64 = AtomicU64::new(0);
static CLOSED_FILE_CACHE_RETAINED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Disabled-by-default counters for cached-file direct/bypass experiments.
#[derive(Debug, Clone, Copy, Default)]
pub struct CachedFileIoCounters {
    pub read_bypass_eligible: u64,
    pub read_bypass_hits: u64,
    pub read_bypass_bytes: u64,
    pub read_bypass_slice_hits: u64,
    pub read_bypass_slice_bytes: u64,
    pub read_bypass_reject_in_memory: u64,
    pub read_bypass_reject_unaligned: u64,
    pub read_bypass_reject_cached: u64,
    pub read_bypass_eof_races: u64,
    pub write_bypass_eligible: u64,
    pub write_bypass_hits: u64,
    pub write_bypass_bytes: u64,
    pub write_bypass_slice_hits: u64,
    pub write_bypass_slice_bytes: u64,
    pub write_bypass_reject_in_memory: u64,
    pub write_bypass_reject_unaligned: u64,
    pub write_no_read_insert_pages: u64,
    pub write_no_read_insert_bytes: u64,
    pub flush_dirty_pages: u64,
    pub flush_bytes: u64,
    pub range_flush_dirty_pages: u64,
    pub range_flush_bytes: u64,
    pub async_dirty_flush_hits: u64,
    pub async_dirty_flush_pages: u64,
    pub async_dirty_flush_bytes: u64,
    pub async_dirty_flush_errors: u64,
    pub async_dirty_flush_sg_enabled: u64,
    pub async_dirty_flush_sg_hits: u64,
    pub async_dirty_flush_sg_segments: u64,
    pub async_dirty_flush_sg_async_submit_hits: u64,
    pub async_dirty_flush_sg_async_submit_segments: u64,
    pub async_dirty_flush_bounce_fallbacks: u64,
    pub async_dirty_flush_writeback_restarts: u64,
    pub readahead_enabled: u64,
    pub readahead_window_pages: u64,
    pub readahead_misses: u64,
    pub readahead_windows: u64,
    pub readahead_pages: u64,
    pub readahead_hits: u64,
    pub readahead_pressure_skips: u64,
    pub readahead_retired_unused_pages: u64,
    pub sync_data_only_requests: u64,
    pub sync_metadata_requests: u64,
    pub sync_data_only_metadata_fallbacks: u64,
    pub range_invalidate_pages: u64,
    pub closed_cache_retain_attempts: u64,
    pub closed_cache_retain_hits: u64,
    pub closed_cache_retain_pages: u64,
    pub closed_cache_retain_reject_pages: u64,
    pub closed_cache_reopen_hits: u64,
    pub closed_cache_retain_releases: u64,
    pub closed_cache_trim_releases: u64,
    pub closed_cache_trim_pages: u64,
    pub closed_cache_trim_flush_errors: u64,
    pub closed_cache_retained_pages_current: u64,
}

pub fn set_cached_file_io_counters_enabled(enabled: bool) {
    ENABLE_CACHED_FILE_IO_COUNTERS.store(enabled, Ordering::Relaxed);
}

pub fn set_async_dirty_flush_sg_enabled(enabled: bool) {
    ENABLE_ASYNC_DIRTY_FLUSH_SG.store(enabled, Ordering::Relaxed);
}

pub fn set_cached_readahead_enabled(enabled: bool) {
    ENABLE_CACHED_READAHEAD.store(enabled, Ordering::Relaxed);
}

pub fn reset_cached_file_io_counters() {
    for counter in [
        &READ_BYPASS_ELIGIBLE,
        &READ_BYPASS_HITS,
        &READ_BYPASS_BYTES,
        &READ_BYPASS_SLICE_HITS,
        &READ_BYPASS_SLICE_BYTES,
        &READ_BYPASS_REJECT_IN_MEMORY,
        &READ_BYPASS_REJECT_UNALIGNED,
        &READ_BYPASS_REJECT_CACHED,
        &READ_BYPASS_EOF_RACES,
        &WRITE_BYPASS_ELIGIBLE,
        &WRITE_BYPASS_HITS,
        &WRITE_BYPASS_BYTES,
        &WRITE_BYPASS_SLICE_HITS,
        &WRITE_BYPASS_SLICE_BYTES,
        &WRITE_BYPASS_REJECT_IN_MEMORY,
        &WRITE_BYPASS_REJECT_UNALIGNED,
        &WRITE_NO_READ_INSERT_PAGES,
        &WRITE_NO_READ_INSERT_BYTES,
        &FLUSH_DIRTY_PAGES,
        &FLUSH_BYTES,
        &RANGE_FLUSH_DIRTY_PAGES,
        &RANGE_FLUSH_BYTES,
        &ASYNC_DIRTY_FLUSH_HITS,
        &ASYNC_DIRTY_FLUSH_PAGES,
        &ASYNC_DIRTY_FLUSH_BYTES,
        &ASYNC_DIRTY_FLUSH_ERRORS,
        &ASYNC_DIRTY_FLUSH_SG_HITS,
        &ASYNC_DIRTY_FLUSH_SG_SEGMENTS,
        &ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS,
        &ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS,
        &ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS,
        &ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS,
        &READAHEAD_MISSES,
        &READAHEAD_WINDOWS,
        &READAHEAD_PAGES_LOADED,
        &READAHEAD_HITS,
        &READAHEAD_PRESSURE_SKIPS,
        &READAHEAD_RETIRED_UNUSED_PAGES,
        &SYNC_DATA_ONLY_REQUESTS,
        &SYNC_METADATA_REQUESTS,
        &SYNC_DATA_ONLY_METADATA_FALLBACKS,
        &RANGE_INVALIDATE_PAGES,
        &CLOSED_FILE_CACHE_RETAIN_ATTEMPTS,
        &CLOSED_FILE_CACHE_RETAIN_HITS,
        &CLOSED_FILE_CACHE_RETAIN_PAGES,
        &CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES,
        &CLOSED_FILE_CACHE_REOPEN_HITS,
        &CLOSED_FILE_CACHE_RETAIN_RELEASES,
        &CLOSED_FILE_CACHE_TRIM_RELEASES,
        &CLOSED_FILE_CACHE_TRIM_PAGES,
        &CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn cached_file_io_counters_snapshot() -> CachedFileIoCounters {
    CachedFileIoCounters {
        read_bypass_eligible: READ_BYPASS_ELIGIBLE.load(Ordering::Relaxed),
        read_bypass_hits: READ_BYPASS_HITS.load(Ordering::Relaxed),
        read_bypass_bytes: READ_BYPASS_BYTES.load(Ordering::Relaxed),
        read_bypass_slice_hits: READ_BYPASS_SLICE_HITS.load(Ordering::Relaxed),
        read_bypass_slice_bytes: READ_BYPASS_SLICE_BYTES.load(Ordering::Relaxed),
        read_bypass_reject_in_memory: READ_BYPASS_REJECT_IN_MEMORY.load(Ordering::Relaxed),
        read_bypass_reject_unaligned: READ_BYPASS_REJECT_UNALIGNED.load(Ordering::Relaxed),
        read_bypass_reject_cached: READ_BYPASS_REJECT_CACHED.load(Ordering::Relaxed),
        read_bypass_eof_races: READ_BYPASS_EOF_RACES.load(Ordering::Relaxed),
        write_bypass_eligible: WRITE_BYPASS_ELIGIBLE.load(Ordering::Relaxed),
        write_bypass_hits: WRITE_BYPASS_HITS.load(Ordering::Relaxed),
        write_bypass_bytes: WRITE_BYPASS_BYTES.load(Ordering::Relaxed),
        write_bypass_slice_hits: WRITE_BYPASS_SLICE_HITS.load(Ordering::Relaxed),
        write_bypass_slice_bytes: WRITE_BYPASS_SLICE_BYTES.load(Ordering::Relaxed),
        write_bypass_reject_in_memory: WRITE_BYPASS_REJECT_IN_MEMORY.load(Ordering::Relaxed),
        write_bypass_reject_unaligned: WRITE_BYPASS_REJECT_UNALIGNED.load(Ordering::Relaxed),
        write_no_read_insert_pages: WRITE_NO_READ_INSERT_PAGES.load(Ordering::Relaxed),
        write_no_read_insert_bytes: WRITE_NO_READ_INSERT_BYTES.load(Ordering::Relaxed),
        flush_dirty_pages: FLUSH_DIRTY_PAGES.load(Ordering::Relaxed),
        flush_bytes: FLUSH_BYTES.load(Ordering::Relaxed),
        range_flush_dirty_pages: RANGE_FLUSH_DIRTY_PAGES.load(Ordering::Relaxed),
        range_flush_bytes: RANGE_FLUSH_BYTES.load(Ordering::Relaxed),
        async_dirty_flush_hits: ASYNC_DIRTY_FLUSH_HITS.load(Ordering::Relaxed),
        async_dirty_flush_pages: ASYNC_DIRTY_FLUSH_PAGES.load(Ordering::Relaxed),
        async_dirty_flush_bytes: ASYNC_DIRTY_FLUSH_BYTES.load(Ordering::Relaxed),
        async_dirty_flush_errors: ASYNC_DIRTY_FLUSH_ERRORS.load(Ordering::Relaxed),
        async_dirty_flush_sg_enabled: ENABLE_ASYNC_DIRTY_FLUSH_SG.load(Ordering::Relaxed) as u64,
        async_dirty_flush_sg_hits: ASYNC_DIRTY_FLUSH_SG_HITS.load(Ordering::Relaxed),
        async_dirty_flush_sg_segments: ASYNC_DIRTY_FLUSH_SG_SEGMENTS.load(Ordering::Relaxed),
        async_dirty_flush_sg_async_submit_hits: ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS
            .load(Ordering::Relaxed),
        async_dirty_flush_sg_async_submit_segments: ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS
            .load(Ordering::Relaxed),
        async_dirty_flush_bounce_fallbacks: ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS
            .load(Ordering::Relaxed),
        async_dirty_flush_writeback_restarts: ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS
            .load(Ordering::Relaxed),
        readahead_enabled: ENABLE_CACHED_READAHEAD.load(Ordering::Relaxed) as u64,
        readahead_window_pages: READAHEAD_PAGES as u64,
        readahead_misses: READAHEAD_MISSES.load(Ordering::Relaxed),
        readahead_windows: READAHEAD_WINDOWS.load(Ordering::Relaxed),
        readahead_pages: READAHEAD_PAGES_LOADED.load(Ordering::Relaxed),
        readahead_hits: READAHEAD_HITS.load(Ordering::Relaxed),
        readahead_pressure_skips: READAHEAD_PRESSURE_SKIPS.load(Ordering::Relaxed),
        readahead_retired_unused_pages: READAHEAD_RETIRED_UNUSED_PAGES.load(Ordering::Relaxed),
        sync_data_only_requests: SYNC_DATA_ONLY_REQUESTS.load(Ordering::Relaxed),
        sync_metadata_requests: SYNC_METADATA_REQUESTS.load(Ordering::Relaxed),
        sync_data_only_metadata_fallbacks: SYNC_DATA_ONLY_METADATA_FALLBACKS
            .load(Ordering::Relaxed),
        range_invalidate_pages: RANGE_INVALIDATE_PAGES.load(Ordering::Relaxed),
        closed_cache_retain_attempts: CLOSED_FILE_CACHE_RETAIN_ATTEMPTS.load(Ordering::Relaxed),
        closed_cache_retain_hits: CLOSED_FILE_CACHE_RETAIN_HITS.load(Ordering::Relaxed),
        closed_cache_retain_pages: CLOSED_FILE_CACHE_RETAIN_PAGES.load(Ordering::Relaxed),
        closed_cache_retain_reject_pages: CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES
            .load(Ordering::Relaxed),
        closed_cache_reopen_hits: CLOSED_FILE_CACHE_REOPEN_HITS.load(Ordering::Relaxed),
        closed_cache_retain_releases: CLOSED_FILE_CACHE_RETAIN_RELEASES.load(Ordering::Relaxed),
        closed_cache_trim_releases: CLOSED_FILE_CACHE_TRIM_RELEASES.load(Ordering::Relaxed),
        closed_cache_trim_pages: CLOSED_FILE_CACHE_TRIM_PAGES.load(Ordering::Relaxed),
        closed_cache_trim_flush_errors: CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS.load(Ordering::Relaxed),
        closed_cache_retained_pages_current: CLOSED_FILE_CACHE_RETAINED_PAGES
            .load(Ordering::Relaxed) as u64,
    }
}

#[inline(always)]
fn cached_file_io_counters_enabled() -> bool {
    ENABLE_CACHED_FILE_IO_COUNTERS.load(Ordering::Relaxed)
}

#[inline(always)]
fn record_cached_file_counter(counter: &AtomicU64, value: u64) {
    if cached_file_io_counters_enabled() {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

fn file_cache_registry() -> &'static Mutex<BTreeMap<CachedFileRegistryKey, FileUserData>> {
    FILE_CACHE_REGISTRY.call_once(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct CachedFileShadowKey {
    identity: CachedFileIdentity,
    page: u32,
}

fn file_cache_shadows() -> &'static Mutex<LruCache<CachedFileShadowKey, u64>> {
    FILE_CACHE_SHADOWS.call_once(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MIN_FILE_CACHE_SHADOW_PAGES)
                .expect("nonzero global file-cache shadow budget"),
        ))
    })
}

fn file_cache_shadow_budget() -> NonZeroUsize {
    // This is the one shared workingset domain in the absence of memcgs.
    // Contract it under allocator pressure rather than giving every inode a
    // permanent private shadow allowance.
    FILE_CACHE_MANAGED_PAGES_ONCE.call_once(|| {
        let allocator = axalloc::global_allocator();
        FILE_CACHE_MANAGED_PAGES.store(
            allocator
                .used_pages()
                .saturating_add(allocator.available_pages()),
            Ordering::Release,
        );
    });
    let managed = FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire);
    NonZeroUsize::new((managed / 8).max(MIN_FILE_CACHE_SHADOW_PAGES))
        .expect("nonzero dynamic file-cache shadow budget")
}

fn current_file_cache_nonresident_age() -> u64 {
    FILE_CACHE_NONRESIDENT_AGE.load(Ordering::Acquire)
}

fn advance_file_cache_nonresident_age() {
    let mut shadows = file_cache_shadows().lock();
    advance_file_cache_nonresident_age_locked(&mut shadows);
}

fn advance_file_cache_nonresident_age_locked(shadows: &mut LruCache<CachedFileShadowKey, u64>) {
    if FILE_CACHE_NONRESIDENT_AGE.load(Ordering::Acquire) == u64::MAX {
        shadows.clear();
        FILE_CACHE_NONRESIDENT_AGE.store(1, Ordering::Release);
    } else {
        FILE_CACHE_NONRESIDENT_AGE.fetch_add(1, Ordering::AcqRel);
    }
}

#[inline]
fn file_cache_shadow_is_recent(age: u64, evicted_at: u64, recent_threshold: u64) -> bool {
    age.wrapping_sub(evicted_at) <= recent_threshold
}

fn record_file_cache_shadow(shared: &CachedFileShared, page: u32) {
    let mut shadows = file_cache_shadows().lock();
    shadows.resize(file_cache_shadow_budget());
    // Allocate age and publish the LRU entry under the same domain lock.  On
    // wrap, old ages cannot be compared safely, so expire the domain and
    // restart its generation instead of manufacturing recent refaults.
    let age = current_file_cache_nonresident_age();
    advance_file_cache_nonresident_age_locked(&mut shadows);
    shadows.put(
        CachedFileShadowKey {
            identity: shared.registry_key,
            page,
        },
        age,
    );
}

fn consume_file_cache_shadow(shared: &CachedFileShared, page: u32) {
    file_cache_shadows().lock().pop(&CachedFileShadowKey {
        identity: shared.registry_key,
        page,
    });
}

fn clear_file_cache_shadows<I>(shared: &CachedFileShared, pages: I)
where
    I: IntoIterator<Item = u32>,
{
    let mut shadows = file_cache_shadows().lock();
    for page in pages {
        shadows.pop(&CachedFileShadowKey {
            identity: shared.registry_key,
            page,
        });
    }
}

fn clear_file_cache_shadow_domain(shared: &CachedFileShared, domain: &InvalidationShadowDomain) {
    let mut shadows = file_cache_shadows().lock();
    let keys = shadows
        .iter()
        .filter_map(|(key, _)| {
            (key.identity == shared.registry_key && domain.contains(key.page)).then_some(*key)
        })
        .collect::<Vec<_>>();
    for key in keys {
        shadows.pop(&key);
    }
}

fn clear_all_file_cache_shadows(shared: &CachedFileShared) {
    clear_file_cache_shadow_domain(shared, &InvalidationShadowDomain::All);
}

fn file_cache_resident_add(pages: usize) {
    FILE_CACHE_RESIDENT_PAGES.fetch_add(pages, Ordering::AcqRel);
}

fn file_cache_resident_sub(pages: usize) {
    let _ = FILE_CACHE_RESIDENT_PAGES.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        n.checked_sub(pages)
    });
}

fn file_cache_active_add() {
    FILE_CACHE_ACTIVE_PAGES.fetch_add(1, Ordering::AcqRel);
}

fn file_cache_active_sub(pages: usize) {
    let _ = FILE_CACHE_ACTIVE_PAGES.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        n.checked_sub(pages)
    });
}

fn file_cache_remove_page(page: &PageCache) {
    file_cache_resident_sub(1);
    if page.is_active() {
        file_cache_active_sub(1);
    }
}

fn file_cache_restore_page(page: &PageCache) {
    file_cache_resident_add(1);
    if page.is_active() {
        file_cache_active_add();
    }
}

fn file_cache_record_page_reference(page: &mut PageCache) {
    if page.record_reference() {
        file_cache_active_add();
        advance_file_cache_nonresident_age();
    }
}

fn file_cache_reclaim_cursor() -> &'static Mutex<Option<CachedFileRegistryKey>> {
    FILE_CACHE_RECLAIM_CURSOR.call_once(|| Mutex::new(None))
}

fn file_cache_estimate_cursor() -> &'static Mutex<Option<CachedFileRegistryKey>> {
    FILE_CACHE_ESTIMATE_CURSOR.call_once(|| Mutex::new(None))
}

fn remove_released_cached_file_registry_entry(
    key: CachedFileRegistryKey,
    shared: *const CachedFileShared,
) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_released_shared = registry.get(&key).is_some_and(|entry| {
            entry.retained.is_none()
                && entry.writeback_anchor.is_none()
                && core::ptr::eq(entry.shared.as_ptr(), shared)
        });
        if matches_released_shared {
            registry.remove(&key)
        } else {
            None
        }
    };
    // Weak state and writeback ownership can ultimately release filesystem or
    // inode objects. Keep every such destructor outside the registry lock.
    drop(retired);
}

fn cached_file_registry_key(location: &Location) -> CachedFileRegistryKey {
    cached_file_user_data(location).identity()
}

fn cached_file_user_data(location: &Location) -> Arc<FileUserData> {
    let mut data = location.user_data();
    data.get_or_insert_with(|| FileUserData::new_identity(location))
}

fn cached_file_is_in_memory(location: &Location) -> bool {
    location.flags().contains(NodeFlags::ALWAYS_CACHE) || location.filesystem().name() == "tmpfs"
}

fn cached_file_shared_for_location(location: &Location) -> Option<Arc<CachedFileShared>> {
    let user_data = location.user_data().get::<FileUserData>()?;
    let key = user_data.identity();
    let registry_shared = {
        let registry = file_cache_registry().lock();
        registry.get(&key).and_then(FileUserData::shared)
    };
    registry_shared.or_else(|| user_data.shared())
}

fn cached_file_shared_for_location_or_create(location: &Location) -> Arc<CachedFileShared> {
    // `TypeMap` is the inode-generation synchronization point.  It is shared
    // by hard-link aliases on backends that expose persistent inode data, and
    // falls back to the exact dentry for backends without that capability.
    // Installing the identity before taking the global registry lock makes
    // concurrent first opens observe the same lease rather than allocating
    // two identities for one inode generation.
    let user_data = cached_file_user_data(location);
    let key = user_data.identity();
    let identity_lease = user_data.identity_lease.clone();
    let (shared, retired_entry, released_retained, install_user_data) = 'registry: {
        let mut registry = file_cache_registry().lock();
        let mut released_retained = None;

        if let Some(entry) = registry.get_mut(&key) {
            let shared = entry.shared();
            entry.update_location(location);
            released_retained = entry.release_retained();
            if let Some(shared) = shared {
                break 'registry (shared, None, released_retained, false);
            }
        }

        // A cache shared state can outlive its registry slot while a caller
        // still owns the per-inode user-data attachment.  Restore that exact
        // state instead of manufacturing a second cache for the generation.
        if let Some(shared) = user_data.shared() {
            let retired_entry = registry.insert(key, FileUserData::new(location, &shared));
            break 'registry (shared, retired_entry, released_retained, false);
        }

        let shared = Arc::new(CachedFileShared::with_identity(
            key,
            identity_lease,
            cached_file_is_in_memory(location),
        ));
        let retired_entry = registry.insert(key, FileUserData::new(location, &shared));
        (shared, retired_entry, released_retained, true)
    };

    // Cached ownership may release filesystem or inode state. Never run those
    // destructors while the registry is locked: teardown can re-enter here.
    if released_retained.is_some() {
        record_cached_file_counter(&CLOSED_FILE_CACHE_REOPEN_HITS, 1);
    }
    drop(retired_entry);
    drop(released_retained);
    if install_user_data {
        location
            .user_data()
            .insert(FileUserData::new(location, &shared));
    }
    shared
}

fn retain_cached_file_writeback_anchor_if_dirty(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) {
    let page_cache = shared.page_cache.lock();
    if !page_cache.iter().any(|(_pn, page)| page.is_dirty()) {
        return;
    }

    let key = cached_file_registry_key(location);
    let mut candidate_anchor = Some(location.writeback_anchor());
    let retired_entry = {
        let mut registry = file_cache_registry().lock();
        let entry = registry
            .entry(key)
            .or_insert_with(|| FileUserData::new(location, shared));
        let retired = if !entry
            .shared()
            .is_some_and(|registered| Arc::ptr_eq(&registered, shared))
        {
            Some(core::mem::replace(
                entry,
                FileUserData::new(location, shared),
            ))
        } else {
            None
        };
        entry.update_location(location);
        if entry.writeback_anchor.is_none() {
            entry.writeback_anchor = candidate_anchor.take();
        }
        retired
    };
    // Filesystem and inode teardown may call back into cache bookkeeping.
    // Drop replaced ownership only after releasing both bookkeeping locks.
    drop(page_cache);
    drop(retired_entry);
    drop(candidate_anchor);
}

fn release_cached_file_writeback_anchor_if_clean(shared: &CachedFileShared) {
    let page_cache = shared.page_cache.lock();
    if page_cache.iter().any(|(_pn, page)| page.is_dirty()) {
        return;
    }

    let released = {
        let mut registry = file_cache_registry().lock();
        registry
            .values_mut()
            .filter(|entry| {
                entry
                    .shared()
                    .is_some_and(|registered| core::ptr::eq(Arc::as_ptr(&registered), shared))
            })
            .filter_map(|entry| entry.writeback_anchor.take())
            .collect::<Vec<_>>()
    };
    drop(page_cache);
    drop(released);
}

fn release_unlinked_cached_file_registry_ownership(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) {
    let key = cached_file_registry_key(location);
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_shared = registry
            .get(&key)
            .is_some_and(|entry| entry.references_shared(shared));
        matches_shared.then(|| registry.remove(&key)).flatten()
    };
    // Retention and writeback anchors can own filesystem or inode state whose
    // teardown re-enters cache bookkeeping.
    drop(retired);
}

/// Removes the registry ownership after an unlinked cache has been discarded.
///
/// This variant is used by a range-lease drop, where retaining the original
/// `Location` would defeat the purpose of making the last lease the cleanup
/// trigger. The shared identity is the same generation-checked key used by
/// the location-based helper above.
fn release_unlinked_cached_file_registry_ownership_for_shared(shared: &Arc<CachedFileShared>) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_shared = registry
            .get(&shared.registry_key)
            .is_some_and(|entry| entry.references_shared(shared));
        matches_shared
            .then(|| registry.remove(&shared.registry_key))
            .flatten()
    };
    drop(retired);
}

type CachedFileWritebackSnapshotEntry = (
    CachedFileRegistryKey,
    Arc<CachedFileShared>,
    WritebackAnchor,
);

fn cached_file_writeback_snapshot() -> Vec<CachedFileWritebackSnapshotEntry> {
    let (entries, deferred, retired) = {
        let mut registry = file_cache_registry().lock();
        let mut entries = Vec::new();
        let mut dead_keys = Vec::new();
        let mut deferred = Vec::new();

        for (key, entry) in registry.iter() {
            let shared = entry.shared();
            let anchor = entry.writeback_anchor();
            match (shared, anchor) {
                (Some(shared), Some(anchor)) => entries.push((*key, shared, anchor)),
                (shared, anchor) => {
                    dead_keys.push(*key);
                    deferred.push((shared, anchor));
                }
            }
        }

        let retired = dead_keys
            .into_iter()
            .filter_map(|key| registry.remove(&key))
            .collect::<Vec<_>>();
        (entries, deferred, retired)
    };

    // Snapshot failures and dead registry entries can own filesystem state.
    // Their destruction must happen only after the registry guard is gone.
    drop(deferred);
    drop(retired);
    entries
}

/// Drops the shared page-cache registry entry for a fully released inode.
pub fn remove_cached_file_registry_entry(device: u64, inode: u64) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        // This legacy raw-slot cleanup has no generation token.  Never
        // remove a live entry: an old inode generation may be finishing
        // after a replacement has already occupied the same device/inode
        // slot.  The exact identity path removes live entries on last close;
        // the caller follows this helper with the inode-scoped dead-entry
        // prune.
        let key = registry.iter().find_map(|(key, entry)| {
            (key.device() == device && key.inode() == inode && !entry.has_live_shared())
                .then_some(*key)
        });
        key.and_then(|key| registry.remove(&key))
    };
    drop(retired);
}

/// Prunes dead cache registry entries for a released inode.
pub fn prune_dead_cached_file_registry_entries_for_inode(inode: u64) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let dead_keys = registry
            .iter()
            .filter_map(|(key, entry)| {
                (key.inode() == inode && !entry.has_live_shared()).then_some(*key)
            })
            .collect::<Vec<_>>();
        dead_keys
            .into_iter()
            .filter_map(|key| registry.remove(&key))
            .collect::<Vec<_>>()
    };
    drop(retired);
}

fn cached_file_page_count(shared: &CachedFileShared) -> usize {
    shared.page_cache.lock().len()
}

/// Conservative, bounded snapshot used to estimate immediately reclaimable
/// file-cache memory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileReclaimEstimate {
    /// Clean, unpinned pages observed in ordinary disk-backed caches.
    pub reclaimable_pages: usize,
    /// Registry entries whose cache state was inspected.
    pub scanned_files: usize,
    /// Entries skipped because their page-cache lock was contended.
    pub busy_files: usize,
    /// Entries skipped because they have active mapping listeners.
    pub mapped_files: usize,
    /// Some live registry entries were outside the bounded snapshot.
    pub snapshot_truncated: bool,
}

/// Outcome of one bounded, non-blocking global clean-cache reclaim pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileReclaimStats {
    pub requested_pages: usize,
    /// Registry slots visited, including dead or unsupported entries.
    pub visited_registry_entries: usize,
    /// Registry size observed when this bounded snapshot was taken.
    pub registry_entries: usize,
    pub scanned_files: usize,
    pub scanned_pages: usize,
    pub reclaimed_pages: usize,
    pub dirty_pages: usize,
    pub pinned_pages: usize,
    pub writeback_pages: usize,
    pub busy_files: usize,
    pub mapped_files: usize,
    pub scan_budget_exhausted_files: usize,
    pub snapshot_truncated: bool,
}

struct CachedFileReclaimSnapshot {
    entries: [Option<Arc<CachedFileShared>>; GLOBAL_FILE_CACHE_SCAN_LIMIT],
    len: usize,
    visited: usize,
    registry_entries: usize,
    truncated: bool,
}

fn cached_file_reclaim_snapshot(
    cursor: &Mutex<Option<CachedFileRegistryKey>>,
) -> CachedFileReclaimSnapshot {
    let registry = file_cache_registry().lock();
    let mut snapshot = CachedFileReclaimSnapshot {
        entries: core::array::from_fn(|_| None),
        len: 0,
        visited: 0,
        registry_entries: registry.len(),
        truncated: false,
    };
    let mut cursor = cursor.lock();
    let mut visited = 0usize;
    let mut last_visited = None;
    let start = *cursor;

    macro_rules! visit_entries {
        ($entries:expr) => {
            for (key, entry) in $entries {
                if visited == GLOBAL_FILE_CACHE_SCAN_LIMIT {
                    break;
                }
                visited += 1;
                last_visited = Some(*key);
                if let Some(shared) = entry.shared() {
                    snapshot.entries[snapshot.len] = Some(shared);
                    snapshot.len += 1;
                }
            }
        };
    }

    if let Some(start) = start {
        visit_entries!(registry.range((
            core::ops::Bound::Excluded(start),
            core::ops::Bound::Unbounded,
        )));
        if visited < GLOBAL_FILE_CACHE_SCAN_LIMIT {
            visit_entries!(registry.range(..=start));
        }
    } else {
        visit_entries!(registry.iter());
    }
    if let Some(last_visited) = last_visited {
        *cursor = Some(last_visited);
    }
    snapshot.truncated = visited < registry.len();
    snapshot.visited = visited;
    drop(cursor);
    drop(registry);
    snapshot
}

/// Returns a bounded lower-bound estimate of clean file-cache pages that can
/// be reclaimed without writeback.  tmpfs/ALWAYS_CACHE pages, dirty pages,
/// pinned pages, writeback pages, and contended caches are deliberately not
/// counted.
pub fn cached_file_reclaim_estimate() -> CachedFileReclaimEstimate {
    let snapshot = cached_file_reclaim_snapshot(file_cache_estimate_cursor());
    let mut estimate = CachedFileReclaimEstimate {
        snapshot_truncated: snapshot.truncated,
        ..CachedFileReclaimEstimate::default()
    };
    for shared in snapshot.entries.into_iter().take(snapshot.len).flatten() {
        if shared.in_memory {
            continue;
        }
        let Some(_direct_guard) = shared.direct_io_lock.try_write() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        let Some(admission) = shared.user_io_pin_admission.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        }
        let Some(_writeback_guard) = shared.writeback_lock.try_write() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        let Some(listeners) = shared.evict_listeners.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        if !listeners.is_empty() {
            estimate.mapped_files = estimate.mapped_files.saturating_add(1);
            continue;
        }
        let Some(cache) = shared.page_cache.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        estimate.scanned_files = estimate.scanned_files.saturating_add(1);
        estimate.reclaimable_pages = estimate.reclaimable_pages.saturating_add(
            cache
                .iter()
                .take(GLOBAL_FILE_CACHE_ESTIMATE_PER_FILE)
                .filter(|(_pn, page)| !page.is_dirty() && !page.is_pinned() && !page.is_writeback())
                .count(),
        );
    }
    estimate
}

/// Reconciles closed-cache accounting with the page-cache state observed
/// while the caller holds `direct_io_lock` for writing.
///
/// Using the actual remaining page count is important: the last open handle
/// may establish retention immediately before a pressure pass.  Applying a
/// reclaim delta to that newer retention would otherwise be able to release a
/// shared cache that still contains dirty pages.
fn synchronize_retained_page_count(shared: &Arc<CachedFileShared>, remaining_pages: usize) {
    let released = {
        let mut registry = file_cache_registry().lock();
        let Some(entry) = registry.get_mut(&shared.registry_key) else {
            return;
        };
        let is_retained_shared = entry
            .retained
            .as_ref()
            .is_some_and(|retained| Arc::ptr_eq(retained, shared));
        if !is_retained_shared {
            return;
        }

        let old_pages = entry.retained_pages;
        if remaining_pages > old_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES
                .fetch_add(remaining_pages - old_pages, Ordering::AcqRel);
        } else if old_pages > remaining_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES
                .fetch_sub(old_pages - remaining_pages, Ordering::AcqRel);
        }
        entry.retained_pages = remaining_pages;
        if remaining_pages == 0 {
            entry.release_retained()
        } else {
            None
        }
    };
    drop(released);
}

fn reclaim_clean_pages_from_shared(
    shared: &Arc<CachedFileShared>,
    target_pages: usize,
    stats: &mut CachedFileReclaimStats,
) -> usize {
    reclaim_clean_pages_from_shared_with_scan_budget(
        shared,
        target_pages,
        GLOBAL_FILE_CACHE_RECLAIM_SCAN_PER_FILE,
        stats,
    )
}

fn reclaim_clean_pages_from_shared_with_scan_budget(
    shared: &Arc<CachedFileShared>,
    target_pages: usize,
    scan_budget_per_pass: usize,
    stats: &mut CachedFileReclaimStats,
) -> usize {
    if target_pages == 0 || shared.in_memory {
        return 0;
    }

    let reclaimed = {
        let Some(_direct_guard) = shared.direct_io_lock.try_write() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Ok(_mutation) = CachedFile::try_begin_shared_cache_invalidating_mutation(shared) else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Some(_writeback_guard) = shared.writeback_lock.try_write() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Some(listeners) = shared.evict_listeners.try_lock() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        if !listeners.is_empty() {
            // A mapped page needs PTE teardown and a TLB grace period.  The
            // first pressure slice deliberately leaves that work to a future
            // batched interface instead of issuing one shootdown per page.
            stats.mapped_files = stats.mapped_files.saturating_add(1);
            return 0;
        }

        let scan_epoch = FILE_CACHE_RECLAIM_SCAN_EPOCH.load(Ordering::Acquire);
        let inode_scan_epoch = shared.pressure_reclaim_scan_epoch.load(Ordering::Acquire);
        let mut cycle_scan_remaining = shared
            .pressure_reclaim_scan_remaining
            .load(Ordering::Acquire);
        if inode_scan_epoch == scan_epoch && cycle_scan_remaining == 0 {
            // This inode has already completed a full LRU traversal in the
            // current system-wide epoch. Do not silently start another cycle:
            // different inode sizes would otherwise have to align before the
            // worker could ever observe one complete no-progress sweep.
            return 0;
        }

        let mut reclaimed = 0usize;
        let mut remaining_pages_after_reclaim = None;
        let mut cycle_scan_initialized = inode_scan_epoch == scan_epoch;
        let mut remaining_scan_budget = scan_budget_per_pass;
        while reclaimed < target_pages && remaining_scan_budget != 0 {
            let (candidate, remaining_pages) = {
                let Some(mut cache) = shared.page_cache.try_lock() else {
                    stats.busy_files = stats.busy_files.saturating_add(1);
                    break;
                };
                if !cycle_scan_initialized {
                    cycle_scan_remaining = cache.len();
                    shared
                        .pressure_reclaim_scan_remaining
                        .store(cycle_scan_remaining, Ordering::Relaxed);
                    shared
                        .pressure_reclaim_scan_epoch
                        .store(scan_epoch, Ordering::Release);
                    cycle_scan_initialized = true;
                } else {
                    cycle_scan_remaining = cycle_scan_remaining.min(cache.len());
                }
                let scan = pop_clean_unpinned_lru_page(
                    &mut cache,
                    remaining_scan_budget.min(cycle_scan_remaining),
                );
                remaining_scan_budget = remaining_scan_budget.saturating_sub(scan.scanned);
                cycle_scan_remaining = cycle_scan_remaining.saturating_sub(scan.scanned);
                shared
                    .pressure_reclaim_scan_remaining
                    .store(cycle_scan_remaining, Ordering::Release);
                stats.scanned_pages = stats.scanned_pages.saturating_add(scan.scanned);
                stats.dirty_pages = stats.dirty_pages.saturating_add(scan.dirty);
                stats.pinned_pages = stats.pinned_pages.saturating_add(scan.pinned);
                stats.writeback_pages = stats.writeback_pages.saturating_add(scan.writeback);
                (scan.page, cache.len())
            };
            remaining_pages_after_reclaim = Some(remaining_pages);
            let Some((pn, page)) = candidate else {
                break;
            };
            // The listener lock is still held and known empty, so no mapping
            // can begin observing this cache page before it is released.
            record_file_cache_shadow(shared, pn);
            drop(page);
            reclaimed += 1;
        }
        if reclaimed < target_pages && remaining_scan_budget == 0 && cycle_scan_remaining != 0 {
            stats.scan_budget_exhausted_files = stats.scan_budget_exhausted_files.saturating_add(1);
        }

        if let (true, Some(remaining_pages)) = (reclaimed != 0, remaining_pages_after_reclaim) {
            synchronize_retained_page_count(shared, remaining_pages);
        }
        reclaimed
    };

    reclaimed
}

/// Reclaims at most a fixed system-wide batch of clean, unpinned,
/// non-writeback pages from ordinary disk-backed files.  The operation never
/// waits for a contended cache/direct-I/O path and never initiates writeback.
pub fn reclaim_clean_cached_file_pages(target_pages: usize) -> CachedFileReclaimStats {
    let bounded_target = target_pages
        .min(GLOBAL_FILE_CACHE_SCAN_LIMIT.saturating_mul(GLOBAL_FILE_CACHE_RECLAIM_PER_FILE));
    let mut stats = CachedFileReclaimStats {
        requested_pages: bounded_target,
        ..CachedFileReclaimStats::default()
    };
    if bounded_target == 0 {
        return stats;
    }
    let snapshot = cached_file_reclaim_snapshot(file_cache_reclaim_cursor());
    stats.visited_registry_entries = snapshot.visited;
    stats.registry_entries = snapshot.registry_entries;
    stats.snapshot_truncated = snapshot.truncated;
    for shared in snapshot.entries.into_iter().take(snapshot.len).flatten() {
        if stats.reclaimed_pages == bounded_target {
            break;
        }
        stats.scanned_files = stats.scanned_files.saturating_add(1);
        let per_file_target = bounded_target
            .saturating_sub(stats.reclaimed_pages)
            .min(GLOBAL_FILE_CACHE_RECLAIM_PER_FILE);
        stats.reclaimed_pages =
            stats
                .reclaimed_pages
                .saturating_add(reclaim_clean_pages_from_shared(
                    &shared,
                    per_file_target,
                    &mut stats,
                ));
    }
    stats
}

/// Starts a new system-wide bounded LRU scan after the previous epoch reached
/// a complete no-progress registry sweep. Individual inode cursors are reset
/// lazily, so advancing the epoch is constant-time and allocation-free.
pub fn advance_clean_cached_file_reclaim_scan_epoch() {
    FILE_CACHE_RECLAIM_SCAN_EPOCH
        .try_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
            epoch.checked_add(1)
        })
        .expect("clean file-cache reclaim scan epoch exhausted");
}

fn release_closed_cached_file_retention(location: &Location) {
    let key = cached_file_registry_key(location);
    let released = {
        let mut registry = file_cache_registry().lock();
        registry
            .get_mut(&key)
            .and_then(FileUserData::release_retained)
    };
    drop(released);
}

struct ClosedFileCacheTrimCandidate {
    key: CachedFileRegistryKey,
    shared: Arc<CachedFileShared>,
    anchor: WritebackAnchor,
    pages: usize,
    epoch: u64,
}

enum ClosedFileCacheRetentionDecision {
    Retained(Option<Arc<CachedFileShared>>),
    Trim(Vec<ClosedFileCacheTrimCandidate>),
}

fn closed_file_cache_trim_candidates(
    registry: &BTreeMap<CachedFileRegistryKey, FileUserData>,
    preserve_key: CachedFileRegistryKey,
    current_retained_pages: usize,
    required_pages: usize,
) -> Vec<ClosedFileCacheTrimCandidate> {
    if current_retained_pages.saturating_add(required_pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
        return Vec::new();
    }

    let mut candidates = registry
        .iter()
        .filter_map(|(key, entry)| {
            if *key == preserve_key || entry.retained_pages == 0 {
                return None;
            }
            let shared = entry.retained.as_ref()?.clone();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return None;
            }
            Some(ClosedFileCacheTrimCandidate {
                key: *key,
                shared,
                anchor: entry.writeback_anchor()?,
                pages: entry.retained_pages,
                epoch: entry.retained_epoch,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|candidate| candidate.epoch);

    let mut projected = current_retained_pages;
    let mut selected = Vec::new();
    for candidate in candidates {
        projected = projected.saturating_sub(candidate.pages);
        selected.push(candidate);
        if projected.saturating_add(required_pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
            break;
        }
    }
    selected
}

fn flush_and_release_closed_file_cache_candidate(candidate: ClosedFileCacheTrimCandidate) -> bool {
    if candidate.shared.open_handles.load(Ordering::Acquire) != 0 {
        return false;
    }

    let file = match candidate.anchor.entry().as_file() {
        Ok(file) => file,
        Err(err) => {
            warn!("Failed to access retained cached file for trim: {err:?}");
            record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
            return false;
        }
    };
    if let Err(err) = flush_dirty_cache_shared(&candidate.shared, file) {
        warn!("Failed to flush retained cached file before trim: {err:?}");
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
        return false;
    }
    if let Err(err) = discard_cached_pages(&candidate.shared) {
        warn!("Failed to invalidate retained cached file before trim: {err:?}");
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
        return false;
    }

    let released = {
        let mut registry = file_cache_registry().lock();
        let Some(entry) = registry.get_mut(&candidate.key) else {
            return false;
        };
        let still_retained = entry
            .retained
            .as_ref()
            .is_some_and(|shared| Arc::ptr_eq(shared, &candidate.shared));
        if !still_retained || candidate.shared.open_handles.load(Ordering::Acquire) != 0 {
            return false;
        }
        let pages = entry.retained_pages;
        entry.release_retained().map(|retained| (retained, pages))
    };
    if let Some((retained, pages)) = released {
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_RELEASES, 1);
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_PAGES, pages as u64);
        drop(retained);
        return true;
    }
    false
}

fn try_retain_closed_cached_file(location: &Location, shared: &Arc<CachedFileShared>) -> bool {
    if cached_file_is_in_memory(location) || shared.unlinked.load(Ordering::Acquire) {
        return false;
    }
    record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_ATTEMPTS, 1);
    if shared.open_handles.load(Ordering::Acquire) != 0 {
        return false;
    }

    let key = cached_file_registry_key(location);
    loop {
        let (pages, decision) = {
            // Pressure reclaim owns this lock for writing.  Keep the page
            // count and retention publication in one read-side transaction,
            // so a reclaim pass can only run wholly before or wholly after it.
            let _direct_guard = shared.direct_io_lock.read();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return false;
            }
            let pages = cached_file_page_count(shared);
            if pages == 0 {
                return false;
            }
            release_cached_file_writeback_anchor_if_clean(shared);

            let mut registry = file_cache_registry().lock();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return false;
            }
            let entry = registry
                .entry(key)
                .or_insert_with(|| FileUserData::new(location, shared));
            let current_without_entry = CLOSED_FILE_CACHE_RETAINED_PAGES
                .load(Ordering::Acquire)
                .saturating_sub(entry.retained_pages);
            if current_without_entry.saturating_add(pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
                let retired = entry.retain_closed(location, shared, pages);
                (pages, ClosedFileCacheRetentionDecision::Retained(retired))
            } else {
                (
                    pages,
                    ClosedFileCacheRetentionDecision::Trim(closed_file_cache_trim_candidates(
                        &registry,
                        key,
                        current_without_entry,
                        pages,
                    )),
                )
            }
        };

        let trim_candidates = match decision {
            ClosedFileCacheRetentionDecision::Retained(retired) => {
                // Replacing a retained cache can release filesystem-backed
                // state. Keep that destructor outside the registry lock.
                drop(retired);
                record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_HITS, 1);
                record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_PAGES, pages as u64);
                return true;
            }
            ClosedFileCacheRetentionDecision::Trim(candidates) => candidates,
        };

        if trim_candidates.is_empty() {
            record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES, pages as u64);
            return false;
        }

        let mut trimmed = false;
        for candidate in trim_candidates {
            trimmed |= flush_and_release_closed_file_cache_candidate(candidate);
        }
        if !trimmed {
            record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES, pages as u64);
            return false;
        }
    }
}

fn sync_and_invalidate_cached_file_pages_locked(
    location: &Location,
    shared: &Arc<CachedFileShared>,
    mutation: &CachedFileMutationGuard,
) -> VfsResult<()> {
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(shared);
    let file = location.entry().as_file()?;
    let mut invalidation = CachedPageInvalidationTransaction::new(mutation);
    invalidation.stage_all()?;
    invalidation.writeback(file, false)?;
    invalidation.commit_discard();
    release_cached_file_writeback_anchor_if_clean(shared);
    release_closed_cached_file_retention(location);
    Ok(())
}

fn with_cache_invalidating_file_operation<R>(
    location: &Location,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<R> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.write();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    sync_and_invalidate_cached_file_pages_locked(location, &shared, &mutation)?;
    let file = location.entry().as_file()?;
    let result = operation(&shared, file);
    let post_result = sync_and_invalidate_cached_file_pages_locked(location, &shared, &mutation);
    if let Err(error) = post_result {
        return Err(error);
    }
    result
}

/// Runs a direct operation only after a lower filesystem has reported that
/// the exact request is executable.  The preflight and the later operation
/// share one direct-I/O write lock, so an extent/EOF capability decision cannot
/// go stale while cached pages are being invalidated.  In particular, a
/// rejected hole, fragmented run, or EOF request leaves the cache and file
/// untouched; a lower layer may also reject an unavailable device before
/// execution when it can prove that no descriptor was published.
fn with_cache_invalidating_file_operation_after_preflight<R>(
    location: &Location,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.write();
    let file = location.entry().as_file()?;
    if !preflight(&shared, file)? {
        return Ok(None);
    }
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    sync_and_invalidate_cached_file_pages_locked(location, &shared, &mutation)?;
    let file = location.entry().as_file()?;
    let result = operation(&shared, file);
    let post_result = sync_and_invalidate_cached_file_pages_locked(location, &shared, &mutation);
    if let Err(error) = post_result {
        return Err(error);
    }
    result.map(Some)
}

/// Acquires a direct range lease, stages overlapping cache pages, and only
/// then runs the filesystem preflight. Once the preflight accepts, all cache
/// locks and staged-page bookkeeping are released before the lower operation
/// can wait on a synchronous device; the owned range lease remains the sole
/// exclusion token until the operation has revalidated its mapping.
fn with_direct_range_operation_after_preflight<R>(
    location: &Location,
    offset: u64,
    len: usize,
    kind: RangeCacheLeaseKind,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let shared = cached_file_shared_for_location_or_create(location);
    let end = offset
        .checked_add(u64::try_from(len).map_err(|_| VfsError::InvalidInput)?)
        .ok_or(VfsError::InvalidInput)?;
    let range_lease = CachedFileShared::try_range_cache_lease(&shared, offset..end, kind)?;
    let invalidation = {
        let _direct_guard = shared.direct_io_lock.write();
        let _writeback_guard = shared.writeback_lock.write();
        wait_for_all_writeback_clear(&shared);
        let file = location.entry().as_file()?;
        let first_page = offset / PAGE_SIZE as u64;
        let last_page = end.div_ceil(PAGE_SIZE as u64);
        let first_page = u32::try_from(first_page).map_err(|_| VfsError::InvalidInput)?;
        let last_page = u32::try_from(last_page).map_err(|_| VfsError::InvalidInput)?;
        let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
        invalidation.stage_range(first_page..last_page)?;
        invalidation.writeback(file, true)?;
        invalidation
    };
    let file = location.entry().as_file()?;
    if !preflight(&shared, file)? {
        // Drop without commit restores clean and dirty pages exactly as they
        // were observed before this unpublished direct attempt.
        return Ok(None);
    }
    debug_assert!(range_lease.revalidate());
    invalidation.commit_discard();
    let file = location.entry().as_file()?;
    operation(&shared, file).map(Some)
}

fn with_cache_invalidating_truncate(location: &Location, len: u64) -> VfsResult<()> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.write();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(&shared);
    let file = location.entry().as_file()?;
    let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
    invalidation.stage_all()?;
    invalidation.writeback(file, false)?;

    let failure_is_atomic = file.set_len_failure_is_atomic();
    if let Err(error) = file.set_len(len) {
        if !failure_is_atomic {
            // The lower filesystem may have published a partial truncate.
            // Retaining pre-operation cache pages would expose stale data.
            invalidation.commit_discard();
            release_cached_file_writeback_anchor_if_clean(&shared);
            release_closed_cached_file_retention(location);
        }
        return Err(error);
    }
    invalidation.commit_discard();
    release_cached_file_writeback_anchor_if_clean(&shared);
    release_closed_cached_file_retention(location);
    Ok(())
}

/// Runs one out-of-band content mutation between two cache invalidations.
///
/// Direct-I/O exclusion and pin-mutation admission remain held through the
/// lower operation, so no pin window can enter between invalidation and commit.
pub fn with_sync_and_invalidate_cached_file_pages<R>(
    location: &Location,
    operation: impl FnOnce() -> VfsResult<R>,
) -> VfsResult<R> {
    with_cache_invalidating_file_operation(location, |_, _| operation())
}

/// Flushes and drops cached pages before backend storage is changed out-of-band.
pub fn sync_and_invalidate_cached_file_pages(location: &Location) -> VfsResult<()> {
    with_sync_and_invalidate_cached_file_pages(location, || Ok(()))
}

fn discard_cached_pages(shared: &Arc<CachedFileShared>) -> VfsResult<()> {
    let _direct_guard = shared.direct_io_lock.write();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(shared)?;
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(shared);
    let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
    invalidation.stage_all()?;
    invalidation.commit_discard();
    Ok(())
}

/// Tries one task-context cleanup after unlinking has made the file eligible
/// for whole-file discard. The range-lease table is the synchronization
/// authority: a `ResourceBusy` result means another exact lease is still
/// live, so the request remains pending until that lease's Drop calls this
/// helper again. Other errors remain terminal and are not silently swallowed.
fn attempt_unlinked_cached_file_cleanup(shared: &Arc<CachedFileShared>) -> bool {
    if !shared.unlinked.load(Ordering::Acquire) || shared.open_handles.load(Ordering::Acquire) != 0
    {
        return false;
    }
    // Serialize the synchronous attempts themselves. A last range lease can
    // clear its table slot while an earlier attempt is still observing the
    // old conflict; keeping the request bit pending behind this guard lets
    // that lease-drop attempt run immediately after the earlier Busy result.
    let _cleanup_guard = shared.unlinked_cleanup_lock.lock();
    if shared
        .unlinked_cleanup_pending
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }

    match discard_cached_pages(shared) {
        Ok(()) => {
            release_cached_file_writeback_anchor_if_clean(shared);
            release_unlinked_cached_file_registry_ownership_for_shared(shared);
            true
        }
        Err(VfsError::ResourceBusy) => {
            // The exact lease owner will synchronously retry when it drops.
            shared
                .unlinked_cleanup_pending
                .store(true, Ordering::Release);
            false
        }
        Err(error) => {
            // Busy is the only expected transient outcome. Preserve the
            // existing fail-stop behavior for an actual cache/metadata error
            // rather than converting it into a silent cleanup success.
            shared
                .unlinked_cleanup_pending
                .store(true, Ordering::Release);
            panic!("failed to discard unlinked cached pages: {error:?}");
        }
    }
}

fn request_unlinked_cached_file_cleanup(shared: &Arc<CachedFileShared>) -> bool {
    if !shared.unlinked.load(Ordering::Acquire) || shared.open_handles.load(Ordering::Acquire) != 0
    {
        return false;
    }
    shared
        .unlinked_cleanup_pending
        .store(true, Ordering::Release);
    attempt_unlinked_cached_file_cleanup(shared)
}

fn pop_unpinned_lru_page(
    cache: &mut LruCache<u32, PageCache>,
) -> VfsResult<Option<(u32, PageCache)>> {
    let mut skipped = 0;
    let limit = cache.len();
    while skipped < limit {
        let Some((pn, page)) = cache.peek_lru() else {
            return Ok(None);
        };
        let pn = *pn;
        if page.is_pinned() {
            cache.promote(&pn);
            skipped += 1;
            continue;
        }
        let popped = cache.pop_lru();
        if let Some((_, page)) = &popped {
            file_cache_remove_page(page);
        }
        return Ok(popped);
    }
    if limit == 0 {
        Ok(None)
    } else {
        Err(VfsError::ResourceBusy)
    }
}

#[derive(Default)]
struct CleanPageScan {
    page: Option<(u32, PageCache)>,
    scanned: usize,
    dirty: usize,
    pinned: usize,
    writeback: usize,
}

fn pop_clean_unpinned_lru_page(
    cache: &mut LruCache<u32, PageCache>,
    scan_budget: usize,
) -> CleanPageScan {
    let mut scan = CleanPageScan::default();
    let mut fallback = None;
    let limit = cache.len().min(scan_budget);
    while scan.scanned < limit {
        let Some((pn, page)) = cache.peek_lru() else {
            break;
        };
        let pn = *pn;
        let dirty = page.is_dirty();
        let pinned = page.is_pinned();
        let writeback = page.is_writeback();
        let active = page.is_active();
        scan.scanned += 1;
        scan.dirty += usize::from(dirty);
        scan.pinned += usize::from(pinned);
        scan.writeback += usize::from(writeback);
        if active {
            cache.get_mut(&pn).unwrap().demote_active();
            file_cache_active_sub(1);
            cache.promote(&pn);
            continue;
        }
        if !dirty && !pinned && !writeback {
            if page.is_noreuse() {
                scan.page = cache.pop_lru();
                if let Some((_, page)) = &scan.page {
                    file_cache_remove_page(page);
                }
                return scan;
            }
            fallback.get_or_insert(pn);
        }
        // Rotate an ineligible LRU entry so a bounded scan can inspect every
        // resident page without allocating a side list.
        cache.promote(&pn);
    }
    if let Some(pn) = fallback {
        scan.page = cache.pop(&pn).map(|page| (pn, page));
        if let Some((_, page)) = &scan.page {
            file_cache_remove_page(page);
        }
    }
    scan
}

fn pop_unused_readahead_lru_page(cache: &mut LruCache<u32, PageCache>) -> Option<(u32, PageCache)> {
    // NOREUSE pages are explicitly reclaim-priority candidates, not merely
    // a hint that happens to work when they reach the LRU head. Rotate each
    // noncandidate once so a bounded cache walk finds one anywhere in LRU.
    for _ in 0..cache.len() {
        let Some((pn, page)) = cache.peek_lru() else {
            return None;
        };
        let pn = *pn;
        if page.is_unused_prefetched() {
            let popped = cache.pop_lru();
            if let Some((_, page)) = &popped {
                file_cache_remove_page(page);
                record_readahead_retired_unused_page();
            }
            return popped;
        }
        cache.promote(&pn);
    }
    None
}

fn restore_popped_cache_page(cache: &mut LruCache<u32, PageCache>, pn: u32, page: PageCache) {
    file_cache_restore_page(&page);
    assert!(
        cache.put(pn, page).is_none(),
        "restoring an evicted cache page replaced page {pn}"
    );
    cache.demote(&pn); // LRU recency only; preserves PageCache::active.
}

/// Moves selected keys to the LRU end while retaining their encounter order.
/// Calling `demote` in reverse order makes this a stable partition: selected
/// entries become the cold prefix, and every unselected entry retains its
/// relative order.
fn stable_demote_lru_keys<T>(cache: &mut LruCache<u32, T>, keys_lru_to_mru: &[u32]) {
    for pn in keys_lru_to_mru.iter().rev() {
        cache.demote(pn);
    }
}

/// Collect page-cache keys in LRU order without turning advisory reclamation
/// into a mandatory allocation. `None` means pressure prevented the optional
/// resident reprioritization; callers must retain their future policy and
/// still report advisory success.
fn try_collect_noreuse_keys<T>(
    cache: &LruCache<u32, T>,
    offset: u64,
    end: u64,
    reserve: usize,
) -> Option<Vec<u32>> {
    let mut keys = Vec::new();
    keys.try_reserve_exact(reserve).ok()?;
    for (pn, _) in cache.iter().rev() {
        let page_start = u64::from(*pn).saturating_mul(PAGE_SIZE as u64);
        if page_start >= offset && page_start < end {
            keys.push(*pn);
        }
    }
    Some(keys)
}

/// Marks cached pages for an inode whose final directory entry is being removed.
pub fn mark_cached_file_unlinked(location: &Location) {
    if let Some(shared) = cached_file_shared_for_location(location) {
        shared.unlinked.store(true, Ordering::Release);
        if shared.open_handles.load(Ordering::Acquire) == 0 {
            request_unlinked_cached_file_cleanup(&shared);
        }
        release_closed_cached_file_retention(location);
    }
}

/// Flushes all live dirty cached file pages before a global sync.
pub fn sync_all_cached_file_pages() -> VfsResult<()> {
    let entries = cached_file_writeback_snapshot();

    for (_key, shared, location) in entries {
        if shared.unlinked.load(Ordering::Acquire) {
            continue;
        }
        let dirty_pages = cached_dirty_page_numbers(&shared);
        if dirty_pages.is_empty() {
            continue;
        }
        let file = location.entry().as_file()?;
        flush_dirty_page_list(&shared, file, dirty_pages, false)?;
    }
    Ok(())
}

/// Flushes live dirty pages owned by one filesystem instance.
///
/// Filesystem backends call this before flushing their own metadata and block
/// caches so unmount and syncfs preserve writeback ordering.
pub fn sync_cached_file_pages_for_filesystem(filesystem: &dyn FilesystemOps) -> VfsResult<()> {
    let entries = cached_file_writeback_snapshot();

    for (_key, shared, location) in entries {
        if !core::ptr::addr_eq(location.filesystem(), filesystem)
            || shared.unlinked.load(Ordering::Acquire)
        {
            continue;
        }
        let dirty_pages = cached_dirty_page_numbers(&shared);
        if dirty_pages.is_empty() {
            continue;
        }
        let file = location.entry().as_file()?;
        flush_dirty_page_list(&shared, file, dirty_pages, false)?;
    }
    Ok(())
}

/// One physically contiguous segment held stable by the caller for an I/O.
///
/// This type is only a descriptor. Dereferencing it is restricted to the
/// unsafe pinned-segment I/O methods below, whose contracts require the range
/// to remain pinned and accessible for the complete operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedPhysicalSegment {
    paddr: usize,
    len: usize,
}

impl PinnedPhysicalSegment {
    pub const fn new(paddr: usize, len: usize) -> Self {
        Self { paddr, len }
    }

    pub const fn paddr(self) -> usize {
        self.paddr
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

const MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS: usize = 64;
const MAX_PHYSICAL_IO_SEGMENTS: usize = 64;
const PHYSICAL_IO_ALIGNMENT: usize = 512;

#[cfg(target_os = "none")]
fn physical_to_virtual(paddr: PhysAddr) -> VirtAddr {
    phys_to_virt(paddr)
}

#[cfg(not(target_os = "none"))]
fn physical_to_virtual(paddr: PhysAddr) -> VirtAddr {
    VirtAddr::from(usize::from(paddr))
}

#[cfg(target_os = "none")]
fn virtual_to_physical(vaddr: VirtAddr) -> PhysAddr {
    virt_to_phys(vaddr)
}

#[cfg(not(target_os = "none"))]
fn virtual_to_physical(vaddr: VirtAddr) -> PhysAddr {
    PhysAddr::from(usize::from(vaddr))
}

fn validate_pinned_physical_segments(
    segments: &[PinnedPhysicalSegment],
    mutable: bool,
) -> VfsResult<usize> {
    if mutable && segments.len() > MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS {
        return Err(VfsError::InvalidInput);
    }
    let mut total = 0usize;
    let mut ranges = [(0usize, 0usize); MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS];
    let mut ranges_len = 0usize;
    for segment in segments.iter().copied() {
        let end = segment
            .paddr
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        total = total
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        if mutable && segment.len != 0 {
            ranges[ranges_len] = (segment.paddr, end);
            ranges_len += 1;
        }
    }
    if mutable {
        let ranges = &mut ranges[..ranges_len];
        ranges.sort_unstable_by_key(|range| range.0);
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(total)
}

/// Validates a physical SG request before any cache or device state is
/// touched.  Physical direct I/O is intentionally stricter than the
/// bounce-buffer pinned path: each descriptor and the complete request are
/// non-empty, checked, disjoint, and at least 512-byte aligned.
fn validate_physical_io_segments(segments: &[PhysicalIoSegment], offset: u64) -> VfsResult<usize> {
    if segments.is_empty() || segments.len() > MAX_PHYSICAL_IO_SEGMENTS {
        return Err(VfsError::InvalidInput);
    }
    if offset % PHYSICAL_IO_ALIGNMENT as u64 != 0 {
        return Err(VfsError::InvalidInput);
    }

    let mut total = 0usize;
    let mut ranges = [(0usize, 0usize); MAX_PHYSICAL_IO_SEGMENTS];
    for (index, segment) in segments.iter().copied().enumerate() {
        if segment.len == 0
            || segment.paddr % PHYSICAL_IO_ALIGNMENT != 0
            || segment.len % PHYSICAL_IO_ALIGNMENT != 0
        {
            return Err(VfsError::InvalidInput);
        }
        let end = segment
            .paddr
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        total = total
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        ranges[index] = (segment.paddr, end);
    }
    if total == 0 || total % PHYSICAL_IO_ALIGNMENT != 0 {
        return Err(VfsError::InvalidInput);
    }

    ranges[..segments.len()].sort_unstable_by_key(|range| range.0);
    if ranges[..segments.len()]
        .windows(2)
        .any(|pair| pair[0].1 > pair[1].0)
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(total)
}

fn try_zeroed_pinned_io_bounce(len: usize) -> VfsResult<Vec<u8>> {
    let mut bounce = Vec::new();
    bounce
        .try_reserve_exact(len)
        .map_err(|_| VfsError::NoMemory)?;
    bounce.resize(len, 0);
    Ok(bounce)
}

struct PinnedPhysicalCursor<'a> {
    segments: &'a [PinnedPhysicalSegment],
    index: usize,
    offset: usize,
}

impl<'a> PinnedPhysicalCursor<'a> {
    fn new(segments: &'a [PinnedPhysicalSegment]) -> Self {
        Self {
            segments,
            index: 0,
            offset: 0,
        }
    }

    fn take(&mut self, limit: usize) -> Option<(usize, usize)> {
        while let Some(segment) = self.segments.get(self.index).copied() {
            if self.offset == segment.len {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let len = limit.min(segment.len - self.offset);
            let paddr = segment.paddr + self.offset;
            self.offset += len;
            return Some((paddr, len));
        }
        None
    }
}

unsafe fn copy_from_pinned_physical_segments(
    cursor: &mut PinnedPhysicalCursor<'_>,
    dst: &mut [u8],
) -> VfsResult<()> {
    let mut copied = 0usize;
    while copied < dst.len() {
        let (paddr, len) = cursor
            .take(dst.len() - copied)
            .ok_or(VfsError::InvalidInput)?;
        let src = physical_to_virtual(PhysAddr::from(paddr)).as_ptr();
        unsafe { core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr().add(copied), len) };
        copied += len;
    }
    Ok(())
}

unsafe fn copy_to_pinned_physical_segments(
    cursor: &mut PinnedPhysicalCursor<'_>,
    src: &[u8],
) -> VfsResult<()> {
    let mut copied = 0usize;
    while copied < src.len() {
        let (paddr, len) = cursor
            .take(src.len() - copied)
            .ok_or(VfsError::InvalidInput)?;
        let dst = physical_to_virtual(PhysAddr::from(paddr)).as_mut_ptr();
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(copied), dst, len) };
        copied += len;
    }
    Ok(())
}

unsafe fn read_file_into_pinned_bounce(
    file: &FileNode,
    dst: &[PinnedPhysicalSegment],
    offset: u64,
    len: usize,
) -> VfsResult<usize> {
    let mut cursor = PinnedPhysicalCursor::new(dst);
    let mut bounce = try_zeroed_pinned_io_bounce(ALIGNED_BYPASS_CHUNK.min(len).max(1))?;
    let mut total = 0usize;
    while total < len {
        let limit = (len - total).min(bounce.len());
        let current = offset
            .checked_add(total as u64)
            .ok_or(VfsError::InvalidInput)?;
        let read = match file.read_at(&mut bounce[..limit], current) {
            Ok(read) => read,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        crate::account_backing_read(read);
        if read == 0 {
            break;
        }
        unsafe { copy_to_pinned_physical_segments(&mut cursor, &bounce[..read])? };
        total += read;
        if read < limit {
            break;
        }
    }
    Ok(total)
}

unsafe fn write_file_from_pinned_bounce(
    file: &FileNode,
    src: &[PinnedPhysicalSegment],
    offset: u64,
    len: usize,
) -> VfsResult<usize> {
    let mut cursor = PinnedPhysicalCursor::new(src);
    let mut bounce = try_zeroed_pinned_io_bounce(ALIGNED_BYPASS_CHUNK.min(len).max(1))?;
    let mut total = 0usize;
    while total < len {
        let limit = (len - total).min(bounce.len());
        unsafe { copy_from_pinned_physical_segments(&mut cursor, &mut bounce[..limit])? };
        let current = offset
            .checked_add(total as u64)
            .ok_or(VfsError::InvalidInput)?;
        let written = match file.write_at(&bounce[..limit], current) {
            Ok(written) => written,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        crate::account_backing_write(written);
        if written == 0 {
            break;
        }
        total += written;
        if written < limit {
            break;
        }
    }
    Ok(total)
}

/// A single page-sized cache entry backed by a physical page.
#[derive(Debug)]
pub struct PageCache {
    addr: VirtAddr,
    dirty: bool,
    prefetched: bool,
    noreuse: bool,
    referenced: bool,
    active: bool,
    pins: u32,
    writeback: u32,
    shmem: bool,
}

/// Per-file page-cache counters returned to the native cachestat adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileCacheStat {
    pub nr_cache: u64,
    pub nr_dirty: u64,
    pub nr_writeback: u64,
    pub nr_evicted: u64,
    pub nr_recently_evicted: u64,
}

static IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);

pub fn in_memory_page_cache_pages() -> usize {
    IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire)
}

impl PageCache {
    fn new(shmem: bool) -> VfsResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE, UsageKind::PageCache)
            .inspect_err(|err| {
                warn!("Failed to allocate page cache: {:?}", err);
            })?;
        if shmem {
            IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.fetch_add(1, Ordering::Release);
        }
        Ok(Self {
            addr: addr.into(),
            dirty: false,
            prefetched: false,
            noreuse: false,
            referenced: false,
            active: false,
            pins: 0,
            writeback: 0,
            shmem,
        })
    }

    /// Returns the physical address of this page.
    pub fn paddr(&self) -> PhysAddr {
        virtual_to_physical(self.addr)
    }

    /// Marks this page as dirty so it will be flushed on eviction.
    pub fn mark_dirty(&mut self) {
        self.prefetched = false;
        self.noreuse = false;
        self.referenced = true;
        self.dirty = true;
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn is_pinned(&self) -> bool {
        self.pins != 0
    }

    fn is_writeback(&self) -> bool {
        self.writeback != 0
    }

    fn mark_prefetched(&mut self) {
        self.prefetched = true;
    }

    fn clear_prefetched(&mut self) -> bool {
        let was_prefetched = self.prefetched;
        self.prefetched = false;
        was_prefetched
    }

    fn mark_noreuse(&mut self) {
        self.noreuse = true;
    }

    fn is_noreuse(&self) -> bool {
        self.noreuse
    }

    fn is_prefetched(&self) -> bool {
        self.prefetched
    }

    fn record_reference(&mut self) -> bool {
        if self.referenced {
            let promoted = !self.active;
            self.active = true;
            if promoted {
                self.referenced = false;
            }
            promoted
        } else {
            self.referenced = true;
            false
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn demote_active(&mut self) {
        self.active = false;
    }

    fn is_referenced(&self) -> bool {
        self.referenced
    }

    fn is_unused_prefetched(&self) -> bool {
        (self.prefetched || self.noreuse)
            && !self.dirty
            && !self.is_pinned()
            && !self.is_writeback()
    }

    fn pin(&mut self) -> VfsResult<()> {
        self.pins = self.pins.checked_add(1).ok_or(VfsError::NoMemory)?;
        Ok(())
    }

    fn unpin(&mut self) {
        assert!(self.pins > 0, "unpinning unpinned page cache entry");
        self.pins -= 1;
    }

    fn begin_writeback(&mut self) -> VfsResult<()> {
        self.pin()?;
        match self.writeback.checked_add(1) {
            Some(writeback) => {
                self.writeback = writeback;
                Ok(())
            }
            None => {
                self.unpin();
                Err(VfsError::NoMemory)
            }
        }
    }

    fn end_writeback(&mut self) {
        assert!(self.writeback > 0, "ending inactive page cache writeback");
        self.writeback -= 1;
        self.unpin();
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns a mutable slice over the page data.
    pub fn data(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE) }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        if self.is_writeback() {
            warn!("page cache entry dropped with writeback in flight");
        }
        if self.is_pinned() {
            warn!("pinned page cache entry dropped");
        }
        if self.dirty {
            warn!("dirty page dropped without flushing");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
        if self.shmem {
            let _ = IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |n| n.checked_sub(1),
            );
        }
    }
}

/// A short-lived guard that prevents a cached file page from being evicted.
pub struct CachedFilePagePin {
    cache: CachedFile,
    pn: u32,
    dirty_on_release: bool,
    _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFilePagePin {
    fn drop(&mut self) {
        let mut guard = self.cache.shared.page_cache.lock();
        let Some(page) = guard.get_mut(&self.pn) else {
            warn!(
                "CachedFilePagePin::drop: missing pinned cached page {}",
                self.pn
            );
            return;
        };
        if self.dirty_on_release {
            page.mark_dirty();
        }
        page.unpin();
        drop(guard);
        if self.dirty_on_release {
            retain_cached_file_writeback_anchor_if_dirty(&self.cache.inner, &self.cache.shared);
        }
    }
}

/// A conservative preparation window for file-backed user I/O pins.
pub struct CachedFilePinWindow {
    cache: CachedFile,
    _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFilePinWindow {
    fn drop(&mut self) {
        let mut admission = self.cache.shared.user_io_pin_admission.lock();
        assert!(
            admission.pin_windows != 0,
            "cached-file pin-window underflow"
        );
        admission.pin_windows -= 1;
    }
}

#[derive(Default)]
struct CachedFilePinAdmission {
    cache_users: usize,
    pin_windows: usize,
    invalidating: bool,
}

/// Shared admission for ordinary page-cache users that may need an LRU slot.
struct CachedFileCacheUserGuard {
    shared: Arc<CachedFileShared>,
    _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFileCacheUserGuard {
    fn drop(&mut self) {
        let mut admission = self.shared.user_io_pin_admission.lock();
        assert!(
            admission.cache_users != 0,
            "cached-file user-count underflow"
        );
        admission.cache_users -= 1;
    }
}

/// Serializes a cache-invalidating file mutation with user-I/O pin admission.
///
/// Existing precise page pins are checked separately before the inode mutation
/// is committed. While this guard is alive, new preparation windows and page
/// pins fail without observing a half-published cache transition.
struct CachedFileMutationGuard {
    shared: Arc<CachedFileShared>,
    _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFileMutationGuard {
    fn drop(&mut self) {
        let mut admission = self.shared.user_io_pin_admission.lock();
        debug_assert!(
            admission.invalidating,
            "ending inactive cached-file mutation"
        );
        admission.invalidating = false;
    }
}

/// Stable identity of one address space that consumes cache-eviction notices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachedFileEvictionOwner(NonZeroUsize);

impl CachedFileEvictionOwner {
    /// Creates an owner key from a stable nonzero address-space identity.
    pub const fn new(key: usize) -> Option<Self> {
        match NonZeroUsize::new(key) {
            Some(key) => Some(Self(key)),
            None => None,
        }
    }

    /// Returns the underlying identity value.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

type EvictListenerFn = Arc<dyn Fn(u32, &PageCache) -> bool + Send + Sync>;

#[derive(Clone)]
struct EvictListenerSnapshot {
    owner: CachedFileEvictionOwner,
    listener: EvictListenerFn,
}

fn evict_listeners_snapshot(shared: &CachedFileShared) -> VfsResult<Vec<EvictListenerSnapshot>> {
    let listeners = shared.evict_listeners.lock();
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(listeners.iter().count())
        .map_err(|_| VfsError::NoMemory)?;
    for listener in listeners.iter() {
        snapshot.push(EvictListenerSnapshot {
            owner: listener.owner,
            listener: listener.listener.clone(),
        });
    }
    Ok(snapshot)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EvictionAcknowledgement {
    had_listener: bool,
    deferred: bool,
}

fn acknowledge_cached_page_eviction_with_listeners(
    listeners: &[EvictListenerSnapshot],
    pn: u32,
    page: &PageCache,
    allowed_deferred_owner: Option<CachedFileEvictionOwner>,
) -> VfsResult<EvictionAcknowledgement> {
    let mut acknowledgement = EvictionAcknowledgement {
        had_listener: !listeners.is_empty(),
        deferred: false,
    };
    for listener in listeners {
        if (listener.listener)(pn, page) {
            continue;
        }
        if Some(listener.owner) != allowed_deferred_owner {
            return Err(VfsError::ResourceBusy);
        }
        acknowledgement.deferred = true;
    }
    Ok(acknowledgement)
}

#[cfg(test)]
fn acknowledge_cached_page_eviction(
    shared: &CachedFileShared,
    pn: u32,
    page: &PageCache,
    allowed_deferred_owner: Option<CachedFileEvictionOwner>,
) -> VfsResult<EvictionAcknowledgement> {
    let listeners = evict_listeners_snapshot(shared)?;
    acknowledge_cached_page_eviction_with_listeners(&listeners, pn, page, allowed_deferred_owner)
}

fn writeback_cached_page_data(file: &FileNode, pn: u32, page: &mut PageCache) -> VfsResult<usize> {
    if !page.dirty {
        return Ok(0);
    }
    let page_start = pn as u64 * PAGE_SIZE as u64;
    let len = (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
    if len == 0 {
        return Ok(0);
    }
    let written = file.write_at(&page.data()[..len], page_start)?;
    crate::account_backing_write(written);
    match written {
        written if written == len => Ok(written),
        _ => Err(VfsError::Io),
    }
}

struct DirtyWritebackPage {
    pn: u32,
    data: Vec<u8>,
}

struct DirtyWritebackRun {
    page_start: u64,
    bytes: usize,
    pages: Vec<DirtyWritebackPage>,
}

enum DirtyWritebackCopy {
    Run(DirtyWritebackRun),
    Empty,
    Busy,
    Stale,
}

struct DirtySgWritebackPage {
    pn: u32,
    ptr: *const u8,
    len: usize,
}

struct DirtySgWritebackRun {
    page_start: u64,
    bytes: usize,
    pages: Vec<DirtySgWritebackPage>,
}

enum DirtySgWritebackBegin {
    Run(DirtySgWritebackRun),
    Empty,
    Fallback,
    Busy,
}

fn wait_for_page_writeback_clear(shared: &CachedFileShared, pn: u32) {
    while shared
        .page_cache
        .lock()
        .get(&pn)
        .is_some_and(PageCache::is_writeback)
    {
        spin_loop();
    }
}

fn wait_for_dirty_pages_writeback_clear(shared: &CachedFileShared, pages: &[u32]) {
    while {
        let mut guard = shared.page_cache.lock();
        pages
            .iter()
            .any(|pn| guard.get(pn).is_some_and(PageCache::is_writeback))
    } {
        spin_loop();
    }
}

fn wait_for_all_writeback_clear(shared: &CachedFileShared) {
    while shared
        .page_cache
        .lock()
        .iter()
        .any(|(_pn, page)| page.is_writeback())
    {
        spin_loop();
    }
}

fn cached_dirty_page_numbers(shared: &CachedFileShared) -> Vec<u32> {
    let guard = shared.page_cache.lock();
    guard
        .iter()
        .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
        .collect()
}

fn copy_dirty_writeback_run(
    shared: &CachedFileShared,
    guard: &mut LruCache<u32, PageCache>,
    dirty_pages: &[u32],
    file_len: u64,
) -> VfsResult<DirtyWritebackCopy> {
    let Some(first_pn) = dirty_pages.first().copied() else {
        return Ok(DirtyWritebackCopy::Empty);
    };
    let page_start = first_pn as u64 * PAGE_SIZE as u64;
    let max_len = file_len
        .saturating_sub(page_start)
        .min((dirty_pages.len() * PAGE_SIZE) as u64) as usize;
    if max_len == 0 {
        for pn in dirty_pages {
            if let Some(page) = guard.get_mut(pn) {
                page.clear_dirty();
            }
        }
        return Ok(DirtyWritebackCopy::Empty);
    }

    let listeners = evict_listeners_snapshot(shared)?;
    // The dirty snapshot is advisory. Do not copy a clean/stale page into a
    // run whose byte offsets were derived from the original list; reselect it
    // after releasing any existing writeback instead.
    for pn in dirty_pages {
        match guard.get(pn) {
            Some(page) if page.is_writeback() => return Ok(DirtyWritebackCopy::Busy),
            Some(page) if page.is_dirty() => {}
            _ => return Ok(DirtyWritebackCopy::Stale),
        }
    }
    let mut pages: Vec<DirtyWritebackPage> = Vec::with_capacity(dirty_pages.len());
    for (idx, pn) in dirty_pages.iter().enumerate() {
        let Some(page) = guard.get_mut(pn) else {
            continue;
        };
        for listener in &listeners {
            if !(listener.listener)(*pn, page) {
                return Err(VfsError::ResourceBusy);
            }
        }
        let dst_start = idx * PAGE_SIZE;
        if dst_start >= max_len {
            continue;
        }
        let len = (max_len - dst_start).min(PAGE_SIZE);
        let mut data = vec![0; len];
        data.copy_from_slice(&page.data()[..len]);
        pages.push(DirtyWritebackPage { pn: *pn, data });
    }

    Ok((!pages.is_empty())
        .then_some(DirtyWritebackRun {
            page_start,
            bytes: max_len,
            pages,
        })
        .map_or(DirtyWritebackCopy::Empty, DirtyWritebackCopy::Run))
}

fn begin_sg_dirty_writeback_run(
    shared: &CachedFileShared,
    guard: &mut LruCache<u32, PageCache>,
    dirty_pages: &[u32],
    file_len: u64,
) -> VfsResult<DirtySgWritebackBegin> {
    let Some(first_pn) = dirty_pages.first().copied() else {
        return Ok(DirtySgWritebackBegin::Empty);
    };
    let page_start = first_pn as u64 * PAGE_SIZE as u64;
    let max_len = file_len
        .saturating_sub(page_start)
        .min((dirty_pages.len() * PAGE_SIZE) as u64) as usize;
    if max_len == 0 {
        for pn in dirty_pages {
            if let Some(page) = guard.get_mut(pn) {
                page.clear_dirty();
            }
        }
        return Ok(DirtySgWritebackBegin::Empty);
    }

    if dirty_pages.len() < 2 || max_len != dirty_pages.len() * PAGE_SIZE || max_len % PAGE_SIZE != 0
    {
        return Ok(DirtySgWritebackBegin::Fallback);
    }

    // File-backed mmap pages register evict listeners that may need to unmap
    // writable PTEs before writeback. The owned-buffer path has a snapshot
    // re-check after completion, so keep listener-backed pages on that path
    // until a full write-protect/generation protocol exists.
    if !shared.evict_listeners.lock().is_empty() {
        return Ok(DirtySgWritebackBegin::Fallback);
    }

    for pn in dirty_pages {
        let Some(page) = guard.get_mut(pn) else {
            return Ok(DirtySgWritebackBegin::Fallback);
        };
        if page.is_writeback() {
            return Ok(DirtySgWritebackBegin::Busy);
        }
        if !page.is_dirty() {
            return Ok(DirtySgWritebackBegin::Fallback);
        }
    }

    let mut pages: Vec<DirtySgWritebackPage> = Vec::with_capacity(dirty_pages.len());
    for pn in dirty_pages {
        let Some(page) = guard.get_mut(pn) else {
            for pinned in &pages {
                if let Some(page) = guard.get_mut(&pinned.pn) {
                    page.end_writeback();
                }
            }
            return Ok(DirtySgWritebackBegin::Fallback);
        };
        if let Err(err) = page.begin_writeback() {
            for pinned in &pages {
                if let Some(page) = guard.get_mut(&pinned.pn) {
                    page.end_writeback();
                }
            }
            return Err(err);
        }
        pages.push(DirtySgWritebackPage {
            pn: *pn,
            ptr: page.data().as_ptr(),
            len: PAGE_SIZE,
        });
    }

    Ok(DirtySgWritebackBegin::Run(DirtySgWritebackRun {
        page_start,
        bytes: max_len,
        pages,
    }))
}

fn finish_sg_dirty_writeback_run(
    shared: &CachedFileShared,
    run: &DirtySgWritebackRun,
    success: bool,
) {
    let mut guard = shared.page_cache.lock();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            warn!(
                "missing page-cache page {} while ending SG writeback",
                written.pn
            );
            continue;
        };
        if success {
            page.clear_dirty();
        }
        page.end_writeback();
    }
}

fn begin_dirty_writeback_run(shared: &CachedFileShared, run: &DirtyWritebackRun) -> VfsResult<()> {
    let mut guard = shared.page_cache.lock();
    let mut begun = Vec::new();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(VfsError::ResourceBusy);
        };
        if !page.is_dirty() || page.is_writeback() {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(VfsError::ResourceBusy);
        }
        if let Err(error) = page.begin_writeback() {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(error);
        }
        begun.push(written.pn);
    }
    Ok(())
}

fn finish_dirty_writeback_run(shared: &CachedFileShared, run: &DirtyWritebackRun, success: bool) {
    let mut guard = shared.page_cache.lock();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            continue;
        };
        if success
            && page.is_dirty()
            && page.data().get(..written.data.len()) == Some(written.data.as_slice())
        {
            page.clear_dirty();
        }
        page.end_writeback();
    }
}

fn build_dirty_writeback_segments(run: &DirtyWritebackRun) -> Vec<Vec<u8>> {
    let target_len = DIRTY_WRITEBACK_SEGMENT_PAGES * PAGE_SIZE;
    let mut segments = Vec::with_capacity(run.pages.len().div_ceil(DIRTY_WRITEBACK_SEGMENT_PAGES));
    let mut current = Vec::new();
    for page in &run.pages {
        if !current.is_empty() && current.len() + page.data.len() > target_len {
            segments.push(current);
            current = Vec::new();
        }
        current.extend_from_slice(&page.data);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn record_dirty_writeback(range_flush: bool, pages: usize, bytes: usize, async_enabled: bool) {
    if range_flush {
        record_cached_file_counter(&RANGE_FLUSH_DIRTY_PAGES, pages as u64);
        record_cached_file_counter(&RANGE_FLUSH_BYTES, bytes as u64);
    } else {
        record_cached_file_counter(&FLUSH_DIRTY_PAGES, pages as u64);
        record_cached_file_counter(&FLUSH_BYTES, bytes as u64);
    }
    if async_enabled {
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_HITS, 1);
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_PAGES, pages as u64);
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_BYTES, bytes as u64);
    }
}

fn record_async_dirty_flush_sg(pages: usize) {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_HITS, 1);
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_SEGMENTS, pages as u64);
}

fn record_async_dirty_flush_sg_async_submit(pages: usize) {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS, 1);
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS, pages as u64);
}

fn record_async_dirty_flush_bounce_fallback() {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS, 1);
}

fn record_async_dirty_flush_writeback_restart() {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS, 1);
}

/// Records a completed asynchronous dirty-page writeback failure on the
/// backend inode that owns the cached pages.  Do not use this for synchronous
/// writeback or the subsequent `fsync` metadata flush: those are returned to
/// their immediate caller instead of becoming an errseq event.
fn publish_async_dirty_writeback_completion_error(file: &FileNode, error: VfsError) {
    if let Ok(state) = file.writeback_error_state() {
        state.publish(error);
    }
    // An accepted asynchronous completion also belongs to the filesystem's
    // superblock errseq.  Synchronous writeback and explicit fsync failures
    // never pass through this completion-only hook.
    if let Some(state) = file.syncfs_writeback_error_state() {
        state.publish(error);
    }
}

fn async_dirty_flush_sg_enabled() -> bool {
    ENABLE_ASYNC_DIRTY_FLUSH_SG.load(Ordering::Relaxed)
}

fn cached_readahead_enabled() -> bool {
    ENABLE_CACHED_READAHEAD.load(Ordering::Relaxed)
}

fn record_readahead_miss() {
    record_cached_file_counter(&READAHEAD_MISSES, 1);
}

fn record_readahead_window(pages: usize) {
    if pages == 0 {
        return;
    }
    record_cached_file_counter(&READAHEAD_WINDOWS, 1);
    record_cached_file_counter(&READAHEAD_PAGES_LOADED, pages as u64);
}

fn record_readahead_hit() {
    record_cached_file_counter(&READAHEAD_HITS, 1);
}

fn record_readahead_retired_unused_page() {
    record_cached_file_counter(&READAHEAD_RETIRED_UNUSED_PAGES, 1);
}

fn record_file_sync_request(data_only: bool) {
    if data_only {
        record_cached_file_counter(&SYNC_DATA_ONLY_REQUESTS, 1);
    } else {
        record_cached_file_counter(&SYNC_METADATA_REQUESTS, 1);
    }
}

pub(crate) fn record_file_sync_data_only_metadata_fallback() {
    record_cached_file_counter(&SYNC_DATA_ONLY_METADATA_FALLBACKS, 1);
}

struct DirtyWritebackError {
    error: VfsError,
    errseq_published: bool,
    worker_must_publish: bool,
}

impl From<VfsError> for DirtyWritebackError {
    fn from(error: VfsError) -> Self {
        Self {
            error,
            errseq_published: false,
            worker_must_publish: false,
        }
    }
}

impl DirtyWritebackError {
    /// Converts the lower-level result returned by a real page writeback.
    /// This is the only error class a queued range-writeback worker may turn
    /// into an asynchronous errseq event.  SG completion failures are already
    /// published by the submit/completion path; the synchronous fallback is
    /// deliberately deferred until the range worker owns its completion.
    fn completion(error: VfsError, errseq_published: bool) -> Self {
        Self {
            error,
            errseq_published,
            worker_must_publish: true,
        }
    }
}

/// A range-writeback error that has reached the page writeback layer.  Setup
/// failures (range leases, node lookup, and argument validation) remain plain
/// VFS errors and must not be published as asynchronous completion failures.
enum RangeSyncError {
    Immediate(VfsError),
    Writeback(DirtyWritebackError),
}

fn flush_dirty_page_list_locked(
    shared: &CachedFileShared,
    file: &FileNode,
    mut dirty_pages: Vec<u32>,
    range_flush: bool,
) -> Result<(), DirtyWritebackError> {
    let file_len = file.len()?;
    dirty_pages.sort_unstable();

    let mut start = 0;
    while start < dirty_pages.len() {
        let async_enabled = virtio_async_block_enabled();
        let dirty_run_limit = if async_enabled
            && async_dirty_flush_sg_enabled()
            && virtio_async_block_wait_policy() == AsyncBlockWaitPolicy::InterruptFirst
        {
            IRQ_FIRST_DIRTY_WRITEBACK_PAGES
        } else {
            MAX_DIRTY_WRITEBACK_PAGES
        };
        let end_limit = (start + dirty_run_limit).min(dirty_pages.len());
        let mut end = start + 1;
        while end < end_limit && dirty_pages[end] == dirty_pages[end - 1] + 1 {
            end += 1;
        }

        if async_enabled && async_dirty_flush_sg_enabled() {
            let sg_begin = {
                let mut guard = shared.page_cache.lock();
                begin_sg_dirty_writeback_run(
                    shared,
                    &mut guard,
                    &dirty_pages[start..end],
                    file_len,
                )?
            };
            match sg_begin {
                DirtySgWritebackBegin::Run(run) => {
                    let slices = run
                        .pages
                        .iter()
                        .map(|page| unsafe { core::slice::from_raw_parts(page.ptr, page.len) })
                        .collect::<Vec<_>>();
                    let mut accepted_async_submit = false;
                    let write_result =
                        match file.try_write_at_vectored_async(&slices, run.page_start) {
                            Ok(AsyncVectoredWriteOutcome::Completed(written)) => {
                                accepted_async_submit = true;
                                Ok(written)
                            }
                            Ok(AsyncVectoredWriteOutcome::CompletionError(error)) => {
                                accepted_async_submit = true;
                                Err(error)
                            }
                            Ok(AsyncVectoredWriteOutcome::NotSubmitted) => {
                                file.write_at_vectored(&slices, run.page_start)
                            }
                            Err(error) => Err(error),
                        };
                    if let Ok(written) = write_result.as_ref() {
                        crate::account_backing_write(*written);
                    }
                    match write_result {
                        Ok(written) if written == run.bytes => {
                            record_dirty_writeback(range_flush, run.pages.len(), run.bytes, true);
                            record_async_dirty_flush_sg(run.pages.len());
                            if accepted_async_submit {
                                record_async_dirty_flush_sg_async_submit(run.pages.len());
                            }
                            finish_sg_dirty_writeback_run(shared, &run, true);
                        }
                        Ok(_) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            if accepted_async_submit {
                                publish_async_dirty_writeback_completion_error(file, VfsError::Io);
                            }
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(DirtyWritebackError {
                                error: VfsError::Io,
                                errseq_published: accepted_async_submit,
                                worker_must_publish: false,
                            });
                        }
                        Err(err) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            if accepted_async_submit {
                                publish_async_dirty_writeback_completion_error(file, err);
                            }
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(DirtyWritebackError {
                                error: err,
                                errseq_published: accepted_async_submit,
                                worker_must_publish: false,
                            });
                        }
                    }
                    start = end;
                    continue;
                }
                DirtySgWritebackBegin::Empty => {
                    start = end;
                    continue;
                }
                DirtySgWritebackBegin::Busy => {
                    record_async_dirty_flush_writeback_restart();
                    wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                    dirty_pages = cached_dirty_page_numbers(shared);
                    start = 0;
                    continue;
                }
                DirtySgWritebackBegin::Fallback => {
                    record_async_dirty_flush_bounce_fallback();
                }
            }
        }

        let copy = {
            let mut guard = shared.page_cache.lock();
            copy_dirty_writeback_run(shared, &mut guard, &dirty_pages[start..end], file_len)
        }?;
        let run = match copy {
            DirtyWritebackCopy::Run(run) => run,
            DirtyWritebackCopy::Empty => {
                start = end;
                continue;
            }
            DirtyWritebackCopy::Busy => {
                record_async_dirty_flush_writeback_restart();
                wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
            DirtyWritebackCopy::Stale => {
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
        };

        if let Err(error) = begin_dirty_writeback_run(shared, &run) {
            if error == VfsError::ResourceBusy {
                record_async_dirty_flush_writeback_restart();
                wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
            return Err(error.into());
        }

        let segments = build_dirty_writeback_segments(&run);
        let slices = segments.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let write_result = file.write_at_vectored(&slices, run.page_start);
        if let Ok(written) = write_result.as_ref() {
            crate::account_backing_write(*written);
        }
        match write_result {
            Ok(written) if written == run.bytes => {
                record_dirty_writeback(range_flush, run.pages.len(), run.bytes, async_enabled);
                finish_dirty_writeback_run(shared, &run, true);
            }
            Ok(_) => {
                finish_dirty_writeback_run(shared, &run, false);
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(DirtyWritebackError::completion(VfsError::Io, false));
            }
            Err(err) => {
                finish_dirty_writeback_run(shared, &run, false);
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(DirtyWritebackError::completion(err, false));
            }
        }
        start = end;
    }
    release_cached_file_writeback_anchor_if_clean(shared);
    Ok(())
}

fn flush_dirty_page_list(
    shared: &Arc<CachedFileShared>,
    file: &FileNode,
    dirty_pages: Vec<u32>,
    range_flush: bool,
) -> VfsResult<()> {
    let _range_lease = CachedFileShared::try_range_cache_lease(
        shared,
        0..u64::MAX,
        RangeCacheLeaseKind::CachedWrite,
    )?;
    let _writeback_guard = shared.writeback_lock.read();
    flush_dirty_page_list_locked(shared, file, dirty_pages, range_flush)
        .map_err(|error| error.error)
}

fn flush_dirty_cache_shared_locked(shared: &CachedFileShared, file: &FileNode) -> VfsResult<()> {
    let dirty_pages = {
        let guard = shared.page_cache.lock();
        guard
            .iter()
            .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
            .collect::<Vec<_>>()
    };
    flush_dirty_page_list_locked(shared, file, dirty_pages, false).map_err(|error| error.error)
}

fn flush_dirty_cache_shared(shared: &Arc<CachedFileShared>, file: &FileNode) -> VfsResult<()> {
    let _range_lease = CachedFileShared::try_range_cache_lease(
        shared,
        0..u64::MAX,
        RangeCacheLeaseKind::CachedWrite,
    )?;
    let _writeback_guard = shared.writeback_lock.read();
    flush_dirty_cache_shared_locked(shared, file)
}

/// A page evicted while inserting a new cached file page.
#[must_use = "deferred eviction pages must remain owned until PTE detachment"]
pub struct EvictedPage {
    pn: u32,
    deferred_owner: Option<CachedFileEvictionOwner>,
    _page: Option<PageCache>,
}

impl EvictedPage {
    /// Returns the file page number that was evicted.
    pub fn page_number(&self) -> u32 {
        self.pn
    }

    /// Returns the address-space owner whose listener deferred detachment.
    pub fn deferred_owner(&self) -> Option<CachedFileEvictionOwner> {
        self.deferred_owner
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
    owner: CachedFileEvictionOwner,
    listener: EvictListenerFn,
    link: LinkedListAtomicLink,
}

intrusive_adapter!(EvictListenerAdapter = Box<EvictListener>: EvictListener { link: LinkedListAtomicLink });

const RANGE_CACHE_LEASE_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RangeCacheLeaseKind {
    DirectRead,
    DirectWrite,
    CachedRead,
    CachedWrite,
    WholeFileMutation,
}

#[inline]
fn range_lease_drop_requests_unlinked_cleanup(kind: RangeCacheLeaseKind) -> bool {
    matches!(
        kind,
        RangeCacheLeaseKind::DirectRead | RangeCacheLeaseKind::DirectWrite
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RangeCacheLeaseRecord {
    start: u64,
    end: u64,
    generation: u64,
    kind: RangeCacheLeaseKind,
}

struct RangeCacheLeaseTable {
    slots: [Option<RangeCacheLeaseRecord>; RANGE_CACHE_LEASE_SLOTS],
    generations: [u64; RANGE_CACHE_LEASE_SLOTS],
}

impl RangeCacheLeaseTable {
    const fn new() -> Self {
        Self {
            slots: [None; RANGE_CACHE_LEASE_SLOTS],
            generations: [0; RANGE_CACHE_LEASE_SLOTS],
        }
    }

    fn conflicts(candidate: RangeCacheLeaseRecord, active: RangeCacheLeaseRecord) -> bool {
        if candidate.end <= active.start || active.end <= candidate.start {
            return false;
        }
        match (candidate.kind, active.kind) {
            (RangeCacheLeaseKind::CachedRead, RangeCacheLeaseKind::CachedRead)
            | (RangeCacheLeaseKind::CachedRead, RangeCacheLeaseKind::CachedWrite)
            | (RangeCacheLeaseKind::CachedWrite, RangeCacheLeaseKind::CachedRead)
            | (RangeCacheLeaseKind::CachedWrite, RangeCacheLeaseKind::CachedWrite) => false,
            _ => true,
        }
    }
}

/// A fixed-capacity, generation-checked lease for one file-cache byte range.
/// The token owns the slot; callers may release all cache locks before a
/// synchronous device wait and retain only this lease while the request is
/// live.
struct RangeCacheLease {
    shared: Arc<CachedFileShared>,
    slot: usize,
    generation: u64,
    record: RangeCacheLeaseRecord,
}

impl RangeCacheLease {
    fn revalidate(&self) -> bool {
        self.shared
            .range_cache_leases
            .lock()
            .slots
            .get(self.slot)
            .and_then(|slot| *slot)
            .is_some_and(|active| active == self.record && active.generation == self.generation)
    }
}

impl Drop for RangeCacheLease {
    fn drop(&mut self) {
        let request_cleanup = range_lease_drop_requests_unlinked_cleanup(self.record.kind);
        let mut table = self.shared.range_cache_leases.lock();
        if table
            .slots
            .get(self.slot)
            .and_then(|slot| *slot)
            .is_some_and(|active| active == self.record && active.generation == self.generation)
        {
            table.slots[self.slot] = None;
        }
        drop(table);
        // An effect/direct-I/O lease can be the only thing keeping an
        // unlinked file from acquiring its whole-file mutation lease. Do the
        // cleanup from this exact last-lease transition instead of panicking
        // in the unlink path or relying on open-handle accounting.
        if request_cleanup {
            request_unlinked_cached_file_cleanup(&self.shared);
        }
    }
}

struct CachedFileShared {
    /// Registry slot owned weakly by this shared state. Final release removes
    /// it only when both this key and this allocation still match.
    registry_key: CachedFileRegistryKey,
    /// Keeps the non-owning registry key unique for this inode generation
    /// while a retained cache or futex lease is still alive.
    identity_lease: Arc<CachedFileIdentityLease>,
    /// tmpfs and ALWAYS_CACHE files have no lower storage from which clean
    /// pages can be faulted back, so global pressure reclaim must skip them.
    in_memory: bool,
    page_cache: Mutex<LruCache<u32, PageCache>>,
    /// Remaining entries in the current bounded pressure-scan cycle. The LRU
    /// rotation is the cursor; this counter prevents an all-ineligible inode
    /// from requesting active retries forever.
    pressure_reclaim_scan_remaining: AtomicUsize,
    pressure_reclaim_scan_epoch: AtomicU64,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    unlinked: AtomicBool,
    /// Set once an unlinked inode has no open handles but a direct/effect
    /// range lease still prevents whole-file cache discard. The last
    /// relevant lease drop synchronously retries the bounded cleanup.
    unlinked_cleanup_pending: AtomicBool,
    /// Serializes cleanup attempts so a lease drop racing an earlier Busy
    /// observation cannot lose the deferred request.
    unlinked_cleanup_lock: Mutex<()>,
    open_handles: AtomicUsize,
    user_io_pin_admission: Mutex<CachedFilePinAdmission>,
    /// Serializes cached page-cache users with direct-I/O cache drains.
    direct_io_lock: RwLock<()>,
    /// Serializes dirty writeback with truncate/cache length transitions.
    writeback_lock: RwLock<()>,
    /// Serializes O_APPEND transaction boundaries across handles for this inode.
    append_lock: RwLock<()>,
    /// Fixed-capacity range ownership used to arbitrate cache aliases and
    /// direct I/O without holding a lock across device completion.
    range_cache_leases: Mutex<RangeCacheLeaseTable>,
    /// Per-inode async range-writeback admission and completion state.  This
    /// is deliberately shared by every CachedFile opened on the inode.
    range_writeback: RangeWritebackState,
    fadvise_readahead: FadviseReadaheadState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FadviseReadaheadRequest {
    offset: u64,
    len: u64,
}
struct FadviseReadaheadQueue {
    worker_running: bool,
    worker_generation: u64,
    head: usize,
    len: usize,
    pending: [Option<FadviseReadaheadRequest>; FADVISE_READAHEAD_QUEUE_CAPACITY],
}

impl FadviseReadaheadQueue {
    const fn new() -> Self {
        Self {
            worker_running: false,
            worker_generation: 0,
            head: 0,
            len: 0,
            pending: [None; FADVISE_READAHEAD_QUEUE_CAPACITY],
        }
    }

    fn contains(&self, request: FadviseReadaheadRequest) -> bool {
        (0..self.len).any(|index| {
            self.pending[(self.head + index) % FADVISE_READAHEAD_QUEUE_CAPACITY]
                .is_some_and(|queued| queued == request)
        })
    }

    fn push(&mut self, request: FadviseReadaheadRequest) -> bool {
        if self.len == FADVISE_READAHEAD_QUEUE_CAPACITY {
            return false;
        }
        let tail = (self.head + self.len) % FADVISE_READAHEAD_QUEUE_CAPACITY;
        self.pending[tail] = Some(request);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<FadviseReadaheadRequest> {
        if self.len == 0 {
            return None;
        }
        let request = self.pending[self.head].take();
        self.head = (self.head + 1) % FADVISE_READAHEAD_QUEUE_CAPACITY;
        self.len -= 1;
        request
    }
}
struct FadviseReadaheadState {
    queue: Mutex<FadviseReadaheadQueue>,
}

struct RangeWritebackRequest {
    generation: u64,
    offset: u64,
    len: u64,
    data_only: bool,
}

struct RangeWritebackCompletion {
    generation: u64,
    offset: u64,
    len: u64,
    result: VfsResult<()>,
}

#[derive(Default)]
struct RangeWritebackQueue {
    next_generation: u64,
    worker_running: bool,
    active: Option<(u64, u64, u64)>,
    pending: VecDeque<RangeWritebackRequest>,
    completed: Vec<RangeWritebackCompletion>,
    interests: BTreeMap<u64, usize>,
}

struct RangeWritebackState {
    queue: Mutex<RangeWritebackQueue>,
    completed: WaitQueue,
}

fn gc_range_writeback_completions(queue: &mut RangeWritebackQueue) {
    if let Some((&last, _)) = queue.interests.last_key_value() {
        queue
            .completed
            .retain(|completion| completion.generation <= last);
    } else {
        queue.completed.clear();
    }
}

/// A generation interest acquired atomically with a range snapshot or write
/// submission.  Its Drop is the sole completion-retention release point.
pub struct RangeWritebackFence {
    shared: Option<Arc<CachedFileShared>>,
    generation: u64,
}

impl RangeWritebackFence {
    fn none() -> Self {
        Self {
            shared: None,
            generation: 0,
        }
    }
}

impl Drop for RangeWritebackFence {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        let mut queue = shared.range_writeback.queue.lock();
        if let Some(count) = queue.interests.get_mut(&self.generation) {
            *count -= 1;
            if *count == 0 {
                queue.interests.remove(&self.generation);
            }
        }
        gc_range_writeback_completions(&mut queue);
    }
}

impl RangeWritebackState {
    fn new() -> Self {
        Self {
            queue: Mutex::new(RangeWritebackQueue::default()),
            completed: WaitQueue::new(),
        }
    }
}

impl CachedFileShared {
    fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        if first_page > last_page {
            return CachedFileCacheStat::default();
        }
        let recent_threshold =
            u64::try_from(FILE_CACHE_ACTIVE_PAGES.load(Ordering::Acquire)).unwrap_or(u64::MAX);
        let cache = self.page_cache.lock();
        let mut stat = CachedFileCacheStat::default();
        for (page_no, page) in cache.iter() {
            if (first_page..=last_page).contains(&u64::from(*page_no)) {
                stat.nr_cache += 1;
                stat.nr_dirty += u64::from(page.is_dirty());
                stat.nr_writeback += u64::from(page.is_writeback());
            }
        }
        let shadows = file_cache_shadows().lock();
        // Sample age only after joining the shadow publication domain, so a
        // reclaimer cannot publish A+1 between the entry observation and the
        // age snapshot used to classify A.
        let age = current_file_cache_nonresident_age();
        for (key, evicted_at) in shadows.iter() {
            if key.identity == self.registry_key
                && (first_page..=last_page).contains(&u64::from(key.page))
                && !cache.contains(&key.page)
            {
                stat.nr_evicted += 1;
                stat.nr_recently_evicted += u64::from(file_cache_shadow_is_recent(
                    age,
                    *evicted_at,
                    recent_threshold,
                ));
            }
        }
        stat
    }

    #[cfg(test)]
    fn new(registry_key: CachedFileRegistryKey, in_memory: bool) -> Self {
        Self::with_identity(
            registry_key,
            Arc::new(CachedFileIdentityLease {
                object: registry_key.object(),
            }),
            in_memory,
        )
    }

    fn with_identity(
        registry_key: CachedFileRegistryKey,
        identity_lease: Arc<CachedFileIdentityLease>,
        in_memory: bool,
    ) -> Self {
        let capacity = if in_memory {
            in_memory_page_cache_capacity()
        } else {
            per_file_page_cache_capacity()
        };
        Self {
            registry_key,
            identity_lease,
            in_memory,
            page_cache: Mutex::new(new_bounded_page_cache_store(capacity)),
            pressure_reclaim_scan_remaining: AtomicUsize::new(0),
            pressure_reclaim_scan_epoch: AtomicU64::new(0),
            evict_listeners: Mutex::new(LinkedList::default()),
            unlinked: AtomicBool::new(false),
            unlinked_cleanup_pending: AtomicBool::new(false),
            unlinked_cleanup_lock: Mutex::new(()),
            open_handles: AtomicUsize::new(0),
            user_io_pin_admission: Mutex::new(CachedFilePinAdmission::default()),
            direct_io_lock: RwLock::new(()),
            writeback_lock: RwLock::new(()),
            append_lock: RwLock::new(()),
            range_cache_leases: Mutex::new(RangeCacheLeaseTable::new()),
            range_writeback: RangeWritebackState::new(),
            fadvise_readahead: FadviseReadaheadState {
                queue: Mutex::new(FadviseReadaheadQueue::new()),
            },
        }
    }

    fn try_range_cache_lease(
        shared: &Arc<Self>,
        range: Range<u64>,
        kind: RangeCacheLeaseKind,
    ) -> VfsResult<RangeCacheLease> {
        if range.start >= range.end {
            return Err(VfsError::InvalidInput);
        }
        let record = RangeCacheLeaseRecord {
            start: range.start,
            end: range.end,
            generation: 0,
            kind,
        };
        let mut table = shared.range_cache_leases.lock();
        if table
            .slots
            .iter()
            .flatten()
            .any(|active| RangeCacheLeaseTable::conflicts(record, *active))
        {
            return Err(VfsError::ResourceBusy);
        }
        let Some(slot) = table.slots.iter().position(Option::is_none) else {
            return Err(VfsError::ResourceBusy);
        };
        let generation = table.generations[slot]
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        table.generations[slot] = generation;
        let record = RangeCacheLeaseRecord {
            generation,
            ..record
        };
        table.slots[slot] = Some(record);
        drop(table);
        Ok(RangeCacheLease {
            shared: shared.clone(),
            slot,
            generation,
            record,
        })
    }
}

#[derive(Clone)]
enum InvalidationShadowDomain {
    All,
    Range(Range<u32>),
    From(u64),
}

impl InvalidationShadowDomain {
    fn contains(&self, page: u32) -> bool {
        match self {
            Self::All => true,
            Self::Range(range) => range.contains(&page),
            Self::From(first) => u64::from(page) >= *first,
        }
    }
}

struct CachedPageInvalidationTransaction {
    shared: Arc<CachedFileShared>,
    pages: Vec<(u32, PageCache)>,
    shadow_domain: Option<InvalidationShadowDomain>,
    committed: bool,
}

impl CachedPageInvalidationTransaction {
    fn new(mutation: &CachedFileMutationGuard) -> Self {
        Self::new_shared(mutation.shared.clone())
    }

    fn new_shared(shared: Arc<CachedFileShared>) -> Self {
        Self {
            shared,
            pages: Vec::new(),
            shadow_domain: None,
            committed: false,
        }
    }

    fn stage_all(&mut self) -> VfsResult<()> {
        self.shadow_domain = Some(InvalidationShadowDomain::All);
        let mut cache = self.shared.page_cache.lock();
        if cache.iter().any(|(_, page)| page.is_pinned()) {
            return Err(VfsError::ResourceBusy);
        }
        let listeners = evict_listeners_snapshot(&self.shared)?;
        self.pages
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        while let Some((pn, page)) = cache.pop_lru() {
            file_cache_remove_page(&page);
            self.pages.push((pn, page));
        }
        drop(cache);
        self.acknowledge_staged_pages(&listeners)
    }

    fn stage_range(&mut self, pages: Range<u32>) -> VfsResult<usize> {
        self.shadow_domain = Some(InvalidationShadowDomain::Range(pages.clone()));
        let mut cache = self.shared.page_cache.lock();
        let listeners = evict_listeners_snapshot(&self.shared)?;
        let mut keys = Vec::new();
        keys.try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        // The advised byte range may cover an enormous sparse file. Walk the
        // bounded resident cache once rather than taking the cache lock for
        // every theoretical page in that range.
        for (pn, _) in cache.iter() {
            if *pn >= pages.start && *pn < pages.end {
                keys.push(*pn);
            }
        }
        if keys
            .iter()
            .any(|pn| cache.get(pn).is_some_and(PageCache::is_pinned))
        {
            return Err(VfsError::ResourceBusy);
        }
        self.pages
            .try_reserve_exact(keys.len())
            .map_err(|_| VfsError::NoMemory)?;
        for pn in keys {
            if let Some(page) = cache.pop(&pn) {
                file_cache_remove_page(&page);
                self.pages.push((pn, page));
            }
        }
        let count = self.pages.len();
        drop(cache);
        self.acknowledge_staged_pages(&listeners)?;
        Ok(count)
    }

    /// Detaches one evictable page without notifying its mappings yet.
    ///
    /// Pageout writes dirty data before its mappings are detached.  Keeping
    /// the page in this transaction until the listener acknowledgement has
    /// succeeded makes a failed acknowledgement lossless: `Drop` restores the
    /// original (still dirty) page to the cache.
    fn stage_page_for_pageout(&mut self, pn: u32) -> VfsResult<bool> {
        let mut cache = self.shared.page_cache.lock();
        let Some(page) = cache.get(&pn) else {
            return Ok(false);
        };
        if page.is_pinned() || page.is_writeback() {
            return Ok(false);
        }
        let page = cache
            .pop(&pn)
            .expect("page cache entry disappeared while holding its lock");
        self.pages.push((pn, page));
        Ok(true)
    }

    fn acknowledge_pageout(&self) -> VfsResult<()> {
        let listeners = evict_listeners_snapshot(&self.shared)?;
        self.acknowledge_staged_pages(&listeners)
    }

    fn stage_from(&mut self, first_page: u64) -> VfsResult<usize> {
        self.shadow_domain = Some(InvalidationShadowDomain::From(first_page));
        let mut cache = self.shared.page_cache.lock();
        let listeners = evict_listeners_snapshot(&self.shared)?;
        let mut keys = Vec::new();
        keys.try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (pn, _) in cache.iter() {
            if u64::from(*pn) >= first_page {
                keys.push(*pn);
            }
        }
        if keys
            .iter()
            .any(|pn| cache.get(pn).is_some_and(PageCache::is_pinned))
        {
            return Err(VfsError::ResourceBusy);
        }
        self.pages
            .try_reserve_exact(keys.len())
            .map_err(|_| VfsError::NoMemory)?;
        for pn in keys {
            if let Some(page) = cache.pop(&pn) {
                file_cache_remove_page(&page);
                self.pages.push((pn, page));
            }
        }
        let count = self.pages.len();
        drop(cache);
        self.acknowledge_staged_pages(&listeners)?;
        Ok(count)
    }

    fn acknowledge_staged_pages(&self, listeners: &[EvictListenerSnapshot]) -> VfsResult<()> {
        for (pn, page) in &self.pages {
            acknowledge_cached_page_eviction_with_listeners(listeners, *pn, page, None)?;
        }
        Ok(())
    }

    fn writeback(&mut self, file: &FileNode, range_flush: bool) -> VfsResult<()> {
        for (pn, page) in &mut self.pages {
            let written = writeback_cached_page_data(file, *pn, page)?;
            if written != 0 {
                record_dirty_writeback(range_flush, 1, written, false);
            }
        }
        Ok(())
    }

    fn restore_staged_page(&mut self, pn: u32, update: impl FnOnce(&mut PageCache)) -> bool {
        let Some(index) = self
            .pages
            .iter()
            .position(|(staged_pn, _)| *staged_pn == pn)
        else {
            return false;
        };
        let (staged_pn, mut page) = self.pages.swap_remove(index);
        update(&mut page);
        let mut cache = self.shared.page_cache.lock();
        file_cache_restore_page(&page);
        assert!(
            cache.put(staged_pn, page).is_none(),
            "restoring a retained truncate page replaced page {staged_pn}"
        );
        cache.demote(&staged_pn); // LRU recency only; preserves PageCache::active.
        true
    }

    fn commit_discard(mut self) {
        if let Some(domain) = &self.shadow_domain {
            clear_file_cache_shadow_domain(&self.shared, domain);
        }
        for (_, page) in &mut self.pages {
            page.clear_dirty();
        }
        self.committed = true;
    }
}

impl Drop for CachedPageInvalidationTransaction {
    fn drop(&mut self) {
        if self.committed || self.pages.is_empty() {
            return;
        }
        let mut cache = self.shared.page_cache.lock();
        for (pn, page) in self.pages.drain(..).rev() {
            file_cache_restore_page(&page);
            assert!(
                cache.put(pn, page).is_none(),
                "cache invalidation rollback replaced page {pn}"
            );
            cache.demote(&pn); // LRU recency only; preserves PageCache::active.
        }
    }
}

/// Owned high-level physical effect for a direct ext4 regular-file request.
///
/// Result of attempting to settle an owned effect.  `Retain` is deliberately
/// not a normal I/O error: the caller must keep the effect (and therefore its
/// range lease, staged cache transaction, and pin owner) until exact device
/// retirement is observed.  `Settled` or an exact reset proof permits the
/// owner to be dropped.
#[cfg(feature = "ext4")]
pub enum PhysicalIoSettleOutcome {
    Settled {
        result: VfsResult<usize>,
    },
    Retain {
        reason: PhysicalIoPendingReason,
    },
    /// Every published device handle has retired, but filesystem
    /// revalidation observed a transient `ResourceBusy`. The effect remains
    /// physically owned and must be retried by the task-context completion
    /// worker without submitting the device request again.
    RetryFinalization,
}

/// Proof that the lower block queue has stopped all access to a published
/// physical effect.  The proof is intentionally not constructible from the
/// public fields of [`BlockResetOutcome`]; callers must obtain it from the
/// lower reset result and a quarantined queue never produces one.
#[cfg(feature = "ext4")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoResetProof {
    kind: PhysicalIoResetProofKind,
}

#[cfg(feature = "ext4")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalIoResetProofKind {
    Quiesced,
    Retired,
}

#[cfg(feature = "ext4")]
impl PhysicalIoResetProof {
    /// Converts an exact lower reset result into the upper-layer quiescence
    /// proof.  `Quarantined` deliberately has no conversion: upper owners
    /// must remain in typed custody until a later reset proves quiescence.
    pub fn from_lower_reset(outcome: BlockResetOutcome) -> Option<Self> {
        let kind = match outcome {
            BlockResetOutcome::Quiesced => PhysicalIoResetProofKind::Quiesced,
            BlockResetOutcome::Retired => PhysicalIoResetProofKind::Retired,
            BlockResetOutcome::Quarantined => return None,
        };
        Some(Self { kind })
    }
}

/// The exact inode reference, lower filesystem effect, range lease, and
/// staged cache pages all move together across a worker boundary.  No user
/// SG borrow or filesystem spin guard is retained.  A published request can
/// never become a fallback: abandoning it before exact retirement is a
/// fail-stop quarantine, and its owner fields are intentionally leaked rather
/// than released while device DMA may still be active.
#[cfg(feature = "ext4")]
pub struct PhysicalIoEffect {
    location: ManuallyDrop<Location>,
    inode: ManuallyDrop<Arc<crate::fs::ext4::Inode>>,
    effect: Ext4PhysicalIoEffect,
    range_lease: Option<RangeCacheLease>,
    invalidation: Option<CachedPageInvalidationTransaction>,
    published: bool,
    quarantined: bool,
    finalized: bool,
}

#[cfg(feature = "ext4")]
pub type PreparedPhysicalIoEffect = PhysicalIoEffect;

#[cfg(feature = "ext4")]
impl PhysicalIoEffect {
    fn new(
        location: Location,
        inode: Arc<crate::fs::ext4::Inode>,
        effect: Ext4PhysicalIoEffect,
        range_lease: RangeCacheLease,
        invalidation: CachedPageInvalidationTransaction,
    ) -> Self {
        Self {
            location: ManuallyDrop::new(location),
            inode: ManuallyDrop::new(inode),
            effect,
            range_lease: Some(range_lease),
            invalidation: Some(invalidation),
            published: false,
            quarantined: false,
            finalized: false,
        }
    }

    pub fn plan(&self) -> PhysicalIoPlan {
        self.effect.plan()
    }

    pub fn state(&self) -> PhysicalIoEffectState {
        // Reset retirement is a high-level terminal transition.  The lower
        // effect has no logical completion to mark, but the reset proof is
        // enough to make all physical access impossible, so expose the same
        // drop-safe terminal state as an exact settled effect.
        if self.finalized {
            PhysicalIoEffectState::Finalized
        } else {
            self.effect.state()
        }
    }

    pub fn publication(&self) -> Option<PhysicalIoPublication> {
        self.effect.publication()
    }

    pub fn is_published(&self) -> bool {
        self.published
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    unsafe fn publish_route(&mut self, kernel_worker: bool) -> VfsResult<PhysicalIoPublishOutcome> {
        let submitted = if kernel_worker {
            unsafe {
                self.inode
                    .publish_owned_physical_effect_kernel(&mut self.effect)
            }
        } else {
            unsafe { self.inode.publish_owned_physical_effect(&mut self.effect) }
        };
        let outcome = match submitted {
            Ok(outcome) => outcome,
            Err(error) => {
                // A lower error is not enough evidence that a driver did not
                // expose a descriptor.  Keep every owner in fail-stop mode;
                // adapters that can prove queue-full/unsupported return the
                // explicit NotSubmitted outcome instead.
                self.published = true;
                self.quarantined = true;
                return Err(error);
            }
        };
        match outcome {
            PhysicalIoPublishOutcome::NotSubmitted(_) => {}
            PhysicalIoPublishOutcome::Published(_) => self.published = true,
            PhysicalIoPublishOutcome::Terminal(_) => {
                self.published = true;
                self.quarantined = true;
            }
        }
        Ok(outcome)
    }

    /// Publishes all mapped extents in one atomic exact-route batch. The
    /// caller remains the exact completion waiter for this compatibility path.
    pub unsafe fn publish(&mut self) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.publish_route(false) }
    }

    /// Publishes an io_uring-owned effect to the device-global task-context
    /// completion worker. The lower broker still authenticates completion by
    /// raw handle and cookie; this route is distinct from synchronous exact
    /// waiters so the worker cannot steal their mailbox records.
    pub unsafe fn publish_kernel(&mut self) -> VfsResult<PhysicalIoPublishOutcome> {
        unsafe { self.publish_route(true) }
    }

    /// Settles after observing exact handle/cookie completions.  Device
    /// failures and terminal partial publications are settled logical
    /// failures once every accepted handle has retired; malformed/missing
    /// observations return `Retain` and keep all owners live.
    pub fn settle(&mut self, completions: &[PhysicalIoCompletion]) -> PhysicalIoSettleOutcome {
        if self.finalized {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        if !self.published {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        if self.quarantined && self.effect.publication().is_none() {
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::MalformedPublication,
            };
        }
        match self
            .inode
            .settle_owned_physical_effect(&mut self.effect, completions)
        {
            PhysicalIoSettlement::Retain(reason) => PhysicalIoSettleOutcome::Retain { reason },
            PhysicalIoSettlement::Settled { plan, success } => self.finalize_settled(plan, success),
        }
    }

    /// Retries only the filesystem finalization phase after all exact device
    /// handles have already retired. This path never records another device
    /// completion and never republishes the effect.
    pub fn retry_finalization(&mut self) -> PhysicalIoSettleOutcome {
        let (plan, success) = match self.effect.state() {
            PhysicalIoEffectState::Completed => (self.effect.plan(), true),
            PhysicalIoEffectState::SettledFailure => (self.effect.plan(), false),
            _ => {
                return PhysicalIoSettleOutcome::Retain {
                    reason: PhysicalIoPendingReason::NotPublished,
                };
            }
        };
        self.finalize_settled(plan, success)
    }

    fn finalize_settled(&mut self, plan: PhysicalIoPlan, success: bool) -> PhysicalIoSettleOutcome {
        let result = self
            .inode
            .finalize_settled_physical_effect(&mut self.effect, plan, success);
        if matches!(result, Err(VfsError::ResourceBusy)) {
            // The lower effect remains Completed/SettledFailure until the
            // filesystem can revalidate it. Keep every owner, including the
            // issued request token in io_uring, live for a bounded retry.
            // The range lease excludes cache-side mutation, but it cannot
            // replace this extent rewalk: mapping_seq is local to an
            // InodeRef, and hard-link aliases may carry distinct shared
            // leases. Keep this typed retry until the filesystem proves the
            // mapping terminally.
            return PhysicalIoSettleOutcome::RetryFinalization;
        }
        if self.effect.state() != PhysicalIoEffectState::Finalized {
            // A settlement proof without the lower finalization transition
            // is an internal protocol failure. Keep all owners quarantined
            // instead of allowing Drop to infer that physical retirement was
            // complete.
            return PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::NotPublished,
            };
        }
        // The lower effect has proved exact physical retirement and the
        // filesystem finalization is now terminal. It is therefore safe to
        // release the range/pin owners even when the logical result is EIO.
        self.finalized = true;
        if success {
            // A completed physical write makes the old cache copy stale even
            // when mapping revalidation fails. Never restore it after this
            // point.
            if let Some(invalidation) = self.invalidation.take() {
                invalidation.commit_discard();
            }
        } else {
            // A failed device request is logically unsuccessful; its exact
            // retirement is proven, so restoring the staged cache transaction
            // is safe for reads. A failed write may have reached the medium
            // before reporting an error, so retaining the old cache would
            // expose stale data.
            self.abort_cache_transaction_after_failure();
        }
        PhysicalIoSettleOutcome::Settled { result }
    }

    fn abort_cache_transaction_after_failure(&mut self) {
        if self.effect.plan().operation() == PhysicalIoOperation::Write {
            if let Some(invalidation) = self.invalidation.take() {
                invalidation.commit_discard();
            }
        } else {
            // Dropping an uncommitted transaction restores the exact staged
            // pages, which is the read-failure behavior.
            let _ = self.invalidation.take();
        }
    }

    /// Aborts a published effect after the lower device has provided an
    /// exact reset/quiescence proof.  This is the only reset path that may
    /// turn a published-but-unsettled effect into a releasable terminal
    /// state.  The logical result remains EIO at the io_uring layer; this
    /// method only closes the high-level ownership transition.
    pub fn abort_after_reset(&mut self, proof: PhysicalIoResetProof) {
        let _reset_kind = proof.kind;
        if self.finalized || !self.published {
            return;
        }
        self.finalized = true;
        self.abort_cache_transaction_after_failure();
    }

    /// Compatibility spelling for callers that used the old finalization
    /// verb.  The return type is intentionally typed so a caller cannot
    /// mistake a retained/quarantined effect for a finalized I/O error.
    pub fn finalize(&mut self, completions: &[PhysicalIoCompletion]) -> PhysicalIoSettleOutcome {
        self.settle(completions)
    }
}

#[cfg(feature = "ext4")]
fn drop_prepared_physical_effect_owners(
    invalidation: &mut Option<CachedPageInvalidationTransaction>,
    range_lease: &mut Option<RangeCacheLease>,
) {
    // An exact direct lease drop can synchronously discard an unlinked file's
    // cache. Roll back staged pages first so that cleanup cannot run before
    // the invalidation transaction restores them.
    let _ = invalidation.take();
    let _ = range_lease.take();
}

#[cfg(feature = "ext4")]
impl Drop for PhysicalIoEffect {
    fn drop(&mut self) {
        if (self.published || self.quarantined) && !self.finalized {
            // Published-but-unretired effects are fail-stop.  In particular,
            // do not drop the range lease or let the staged invalidation
            // transaction restore cache pages while DMA may still run.  The
            // owner must be transferred to a reset/quarantine supervisor;
            // this Drop path intentionally leaks it if no supervisor exists.
            let _ = self.range_lease.take().map(core::mem::forget);
            let _ = self.invalidation.take().map(core::mem::forget);
            return;
        }
        // ManuallyDrop makes the fail-stop branch above explicit.  In the
        // ordinary prepared or settled path these two owners still need a
        // normal destructor call.
        drop_prepared_physical_effect_owners(&mut self.invalidation, &mut self.range_lease);
        unsafe {
            ManuallyDrop::drop(&mut self.inode);
            ManuallyDrop::drop(&mut self.location);
        }
    }
}

impl Drop for CachedFileShared {
    fn drop(&mut self) {
        let cache = self.page_cache.lock();
        let remaining = cache.len();
        let active = cache.iter().filter(|(_, page)| page.is_active()).count();
        drop(cache);
        let observed = FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
        debug_assert!(
            observed >= remaining,
            "file-cache resident accounting underflow on final shared drop"
        );
        file_cache_resident_sub(remaining);
        file_cache_active_sub(active);
        // Registry identities are never reused, but retaining their shadows
        // after the final cache owner is gone only wastes the global budget.
        clear_all_file_cache_shadows(self);
        // Final Arc release makes the registered Weak impossible to upgrade.
        // The pointer check prevents a stale release from deleting a newer
        // shared state installed for the same inode-generation identity.
        remove_released_cached_file_registry_entry(self.registry_key, core::ptr::from_ref(self));
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
        self.shared.open_handles.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
            in_memory: self.in_memory,
        }
    }
}

struct FileUserData {
    registry_key: CachedFileRegistryKey,
    identity_lease: Arc<CachedFileIdentityLease>,
    shared: Weak<CachedFileShared>,
    retained: Option<Arc<CachedFileShared>>,
    writeback_anchor: Option<WritebackAnchor>,
    retained_pages: usize,
    retained_epoch: u64,
    mountpoint: Weak<Mountpoint>,
    entry: WeakDirEntry,
}

impl FileUserData {
    fn new_identity(location: &Location) -> Self {
        let object = NEXT_CACHED_FILE_IDENTITY
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("cached file identity generation exhausted");
        let identity_lease = Arc::new(CachedFileIdentityLease { object });
        let registry_key = CachedFileIdentity {
            device: location.mountpoint().device(),
            inode: location.inode(),
            object: identity_lease.object(),
        };
        Self {
            registry_key,
            identity_lease,
            shared: Weak::new(),
            retained: None,
            writeback_anchor: None,
            retained_pages: 0,
            retained_epoch: 0,
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
    }

    fn new(location: &Location, shared: &Arc<CachedFileShared>) -> Self {
        Self {
            registry_key: shared.registry_key,
            identity_lease: shared.identity_lease.clone(),
            shared: Arc::downgrade(shared),
            retained: None,
            writeback_anchor: None,
            retained_pages: 0,
            retained_epoch: 0,
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
    }

    fn identity(&self) -> CachedFileRegistryKey {
        self.registry_key
    }

    pub fn shared(&self) -> Option<Arc<CachedFileShared>> {
        self.retained.clone().or_else(|| self.shared.upgrade())
    }

    fn references_shared(&self, shared: &Arc<CachedFileShared>) -> bool {
        self.retained.as_ref().map_or_else(
            || core::ptr::eq(self.shared.as_ptr(), Arc::as_ptr(shared)),
            |retained| Arc::ptr_eq(retained, shared),
        )
    }

    fn has_live_shared(&self) -> bool {
        self.retained.is_some() || self.shared.strong_count() != 0
    }

    pub fn writeback_anchor(&self) -> Option<WritebackAnchor> {
        if let Some(anchor) = &self.writeback_anchor {
            return Some(anchor.clone());
        }
        Some(
            self.mountpoint
                .upgrade()?
                .writeback_anchor(self.entry.upgrade()?),
        )
    }

    fn update_location(&mut self, location: &Location) {
        self.mountpoint = Arc::downgrade(location.mountpoint());
        self.entry = location.entry().downgrade();
    }

    fn retain_closed(
        &mut self,
        location: &Location,
        shared: &Arc<CachedFileShared>,
        pages: usize,
    ) -> Option<Arc<CachedFileShared>> {
        self.update_location(location);
        let old_pages = self.retained_pages;
        if pages > old_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_add(pages - old_pages, Ordering::AcqRel);
        } else if old_pages > pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_sub(old_pages - pages, Ordering::AcqRel);
        }
        let retired = self.retained.replace(shared.clone());
        self.retained_pages = pages;
        self.retained_epoch = CLOSED_FILE_CACHE_RETAIN_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
        retired
    }

    fn release_retained(&mut self) -> Option<Arc<CachedFileShared>> {
        let retained = self.retained.take()?;
        let pages = self.retained_pages;
        self.retained_pages = 0;
        self.retained_epoch = 0;
        if pages != 0 {
            CLOSED_FILE_CACHE_RETAINED_PAGES.fetch_sub(pages, Ordering::AcqRel);
        }
        record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_RELEASES, 1);
        Some(retained)
    }
}

impl Drop for FileUserData {
    fn drop(&mut self) {
        let _ = self.release_retained();
    }
}

impl CachedFile {
    /// Snapshot the cache state in the inclusive page interval used by
    /// Linux's cachestat(2).  The snapshot is intentionally advisory: page
    /// state may change immediately after the locks are released.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        self.shared.cachestat(first_page, last_page)
    }

    fn record_eviction(&self, page_no: u32) {
        record_file_cache_shadow(&self.shared, page_no);
    }
    /// Returns an existing cached file for `location`, or creates a new one.
    pub fn get_or_create(location: Location) -> Self {
        let in_memory = cached_file_is_in_memory(&location);
        let shared = cached_file_shared_for_location_or_create(&location);
        shared.open_handles.fetch_add(1, Ordering::AcqRel);

        Self {
            inner: location,
            shared,
            in_memory,
        }
    }

    /// Returns a cache handle only when this inode already owns cached state.
    /// Unlike `get_or_create`, this performs no registry, identity, or Arc
    /// allocation and is therefore safe for best-effort advisory paths.
    fn get_existing(location: Location) -> Option<Self> {
        let shared = cached_file_shared_for_location(&location)?;
        shared.open_handles.fetch_add(1, Ordering::AcqRel);
        Some(Self {
            in_memory: cached_file_is_in_memory(&location),
            inner: location,
            shared,
        })
    }

    /// Returns `true` if both handles refer to the same shared state.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Returns the stable identity shared by the page cache and external
    /// users (for example shared-futex wait queues).
    pub fn identity(&self) -> CachedFileIdentity {
        self.shared.registry_key
    }

    /// Faults a bounded advised range into the coherent page cache. Page
    /// allocation and lower I/O happen before taking the cache lock; the
    /// range lease serializes this two-phase publication with DONTNEED.
    fn fadvise_willneed_now(&self, offset: u64, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let max_len = FADVISE_WILLNEED_MAX_PAGES.saturating_mul(PAGE_SIZE as u64);
        let end = offset.saturating_add(len.min(max_len)).min(file.len()?);
        if end <= offset {
            return Ok(());
        }
        let first = offset / PAGE_SIZE as u64;
        let last = end.saturating_sub(1) / PAGE_SIZE as u64;
        for page in first..=last {
            let pn = u32::try_from(page).map_err(|_| VfsError::InvalidInput)?;
            let lease = CachedFileShared::try_range_cache_lease(
                &self.shared,
                page_range(page, 1),
                RangeCacheLeaseKind::CachedRead,
            )?;
            if self.shared.page_cache.lock().contains(&pn) {
                continue;
            }
            let mut prepared = PageCache::new(self.shared.in_memory)?;
            prepared.data().fill(0);
            let read = file.read_at(prepared.data(), page * PAGE_SIZE as u64)?;
            if !self.shared.in_memory {
                crate::account_backing_read(read);
            }
            // The lease stayed live through the lock-free read. Rechecking
            // its slot makes publication conditional on that exact lease.
            if !lease.revalidate() {
                continue;
            }
            let mut cache = self.shared.page_cache.lock();
            if cache.contains(&pn) || cache.len() == cache.cap().get() {
                continue;
            }
            prepared.mark_prefetched();
            cache.put(pn, prepared);
            cache.demote(&pn);
        }
        Ok(())
    }

    /// Queue bounded best-effort prefetch.  The worker owns a `CachedFile`
    /// clone, so close/unlink cannot leave a dangling cache reference.
    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        // This is a syscall path: construct its task name fallibly before
        // publishing a request, rather than relying on String's infallible
        // growth after a successful syscall return.
        let mut name = alloc::string::String::new();
        if name.try_reserve_exact("fadvise-ra".len()).is_err() {
            return Ok(());
        }
        name.push_str("fadvise-ra");
        let request = FadviseReadaheadRequest { offset, len };
        let generation = {
            let mut q = self.shared.fadvise_readahead.queue.lock();
            let _ = !q.contains(request) && q.push(request);
            if q.worker_running {
                return Ok(());
            }
            // Publish the generation before task construction. A worker can
            // therefore never finish and clear a bit that this caller writes
            // after spawning it; failure clears only this exact generation.
            q.worker_generation = q.worker_generation.wrapping_add(1);
            q.worker_running = true;
            q.worker_generation
        };
        let worker = self.clone();
        if axtask::try_spawn_with_name(move || worker.fadvise_readahead_worker(generation), name)
            .is_err()
        {
            let mut q = self.shared.fadvise_readahead.queue.lock();
            if q.worker_running && q.worker_generation == generation {
                q.worker_running = false;
            }
            // WILLNEED is explicitly best-effort: retain/defer queued work
            // for a later advisory call without exposing scheduler ENOMEM.
        }
        Ok(())
    }

    fn fadvise_readahead_worker(&self, generation: u64) {
        loop {
            let request = {
                let mut q = self.shared.fadvise_readahead.queue.lock();
                if !q.worker_running || q.worker_generation != generation {
                    return;
                }
                match q.pop() {
                    Some(r) => r,
                    None => {
                        if q.worker_generation == generation {
                            q.worker_running = false;
                        }
                        return;
                    }
                }
            };
            let _ = self.fadvise_willneed_now(request.offset, request.len);
        }
    }

    /// Marks already resident clean pages as low-reuse candidates.  It never
    /// faults data in, which is the important distinction from WILLNEED.
    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.saturating_add(len);
        if end <= offset {
            return Ok(());
        }
        let mut cache = self.shared.page_cache.lock();
        // Like DONTNEED, NOREUSE is range-local but must be O(resident), not
        // O(advised pages), for sparse or deliberately huge ranges. Gather
        // resident matches in LRU order with fallible storage, mark them
        // without promoting them, then stable-splice them to the cold end.
        let reserve = cache.len();
        let Some(keys) = try_collect_noreuse_keys(&cache, offset, end, reserve) else {
            // This path is a cache-only optimization. The OFD policy remains
            // active, so a later read still marks its consumed pages NOREUSE.
            return Ok(());
        };
        for pn in &keys {
            if let Some(entry) = cache.peek_mut(pn) {
                entry.mark_noreuse();
            }
        }
        stable_demote_lru_keys(&mut cache, &keys);
        Ok(())
    }

    /// Writes back and invalidates only whole pages fully covered by the
    /// range.  On a writeback or eviction failure the transaction drops and
    /// restores every staged page, retaining dirty data and its error state.
    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let file_len = file.len()?;
        let end = offset.saturating_add(len).min(file_len);
        let first = offset.saturating_add(PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64;
        // A partial page in the middle of a file can contain bytes outside
        // the advised range.  The final EOF page has no such live suffix and
        // Linux may invalidate it as part of the through-EOF form.
        let last_exclusive = if end == file_len {
            end.div_ceil(PAGE_SIZE as u64)
        } else {
            end / PAGE_SIZE as u64
        };
        if first >= last_exclusive {
            return Ok(());
        }
        let pages = u32::try_from(first).map_err(|_| VfsError::InvalidInput)?
            ..u32::try_from(last_exclusive).map_err(|_| VfsError::InvalidInput)?;
        // This is a range-local hint, not an inode-wide direct-I/O
        // transition.  The lease excludes only aliases of these pages; pins
        // and eviction listeners are checked by the staged transaction.
        let byte_end = last_exclusive.saturating_mul(PAGE_SIZE as u64);
        let _lease = CachedFileShared::try_range_cache_lease(
            &self.shared,
            first.saturating_mul(PAGE_SIZE as u64)..byte_end,
            RangeCacheLeaseKind::DirectWrite,
        )?;
        let _writeback_guard = self.shared.writeback_lock.write();
        let mut invalidation = CachedPageInvalidationTransaction::new_shared(self.shared.clone());
        invalidation.stage_range(pages)?;
        invalidation.writeback(file, true)?;
        invalidation.commit_discard();
        Ok(())
    }

    /// Opens a short preparation window for pinning file-backed user I/O pages.
    ///
    /// While this window is active, direct cache-draining I/O and LRU evictions
    /// are conservatively rejected for this cached file. Precise page pins take
    /// over once the caller has identified the exact cached pages.
    pub fn begin_user_io_pin_window(&self) -> VfsResult<CachedFilePinWindow> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            0..u64::MAX,
            RangeCacheLeaseKind::CachedWrite,
        )?);
        let mut admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.pin_windows = admission
            .pin_windows
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        drop(admission);
        Ok(CachedFilePinWindow {
            cache: self.clone(),
            _range_lease: range_lease,
        })
    }

    /// Pins an already cached page if it still maps to `paddr`.
    pub fn pin_cached_page_by_paddr(
        &self,
        pn: u32,
        paddr: PhysAddr,
        dirty_on_release: bool,
    ) -> VfsResult<CachedFilePagePin> {
        let admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 {
            return Err(VfsError::ResourceBusy);
        }
        let Some(mut guard) = self.shared.page_cache.try_lock() else {
            return Err(VfsError::ResourceBusy);
        };
        let Some(page) = guard.get_mut(&pn) else {
            return Err(VfsError::BadAddress);
        };
        if page.paddr() != paddr {
            return Err(VfsError::BadAddress);
        }
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            page_range(u64::from(pn), 1),
            if dirty_on_release {
                RangeCacheLeaseKind::CachedWrite
            } else {
                RangeCacheLeaseKind::CachedRead
            },
        )?);
        page.pin()?;
        Ok(CachedFilePagePin {
            cache: self.clone(),
            pn,
            dirty_on_release,
            _range_lease: range_lease,
        })
    }

    fn begin_cache_invalidating_mutation(&self) -> VfsResult<CachedFileMutationGuard> {
        Self::begin_shared_cache_invalidating_mutation(&self.shared)
    }

    fn begin_cache_user(&self) -> VfsResult<CachedFileCacheUserGuard> {
        self.begin_cache_user_range(0..u64::MAX, RangeCacheLeaseKind::CachedRead)
    }

    fn begin_cache_user_range(
        &self,
        range: Range<u64>,
        kind: RangeCacheLeaseKind,
    ) -> VfsResult<CachedFileCacheUserGuard> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            range,
            kind,
        )?);
        let mut admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.cache_users = admission
            .cache_users
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        drop(admission);
        Ok(CachedFileCacheUserGuard {
            shared: self.shared.clone(),
            _range_lease: range_lease,
        })
    }

    fn begin_shared_cache_invalidating_mutation(
        shared: &Arc<CachedFileShared>,
    ) -> VfsResult<CachedFileMutationGuard> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            shared,
            0..u64::MAX,
            RangeCacheLeaseKind::WholeFileMutation,
        )?);
        let mut admission = shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.invalidating = true;
        drop(admission);
        Ok(CachedFileMutationGuard {
            shared: shared.clone(),
            _range_lease: range_lease,
        })
    }

    fn try_begin_shared_cache_invalidating_mutation(
        shared: &Arc<CachedFileShared>,
    ) -> VfsResult<CachedFileMutationGuard> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            shared,
            0..u64::MAX,
            RangeCacheLeaseKind::WholeFileMutation,
        )?);
        let Some(mut admission) = shared.user_io_pin_admission.try_lock() else {
            return Err(VfsError::ResourceBusy);
        };
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.invalidating = true;
        drop(admission);
        Ok(CachedFileMutationGuard {
            shared: shared.clone(),
            _range_lease: range_lease,
        })
    }

    fn admit_truncate(&self, old_len: u64, new_len: u64) -> VfsResult<()> {
        if new_len >= old_len {
            return Ok(());
        }
        let guard = self.shared.page_cache.lock();
        let overlaps_pinned_page = guard.iter().any(|(pn, page)| {
            let page_end = u64::from(*pn)
                .saturating_add(1)
                .saturating_mul(PAGE_SIZE as u64);
            page_end > new_len && page.is_pinned()
        });
        if overlaps_pinned_page {
            Err(VfsError::ResourceBusy)
        } else {
            Ok(())
        }
    }

    /// Returns `true` if this file is backed by an in-memory filesystem (e.g. tmpfs).
    pub fn in_memory(&self) -> bool {
        self.in_memory
    }

    /// Registers a listener that is called when a page is evicted from cache.
    ///
    /// Returns a handle that can later be passed to
    /// [`remove_evict_listener`](Self::remove_evict_listener).
    pub fn add_evict_listener<F>(&self, owner: CachedFileEvictionOwner, listener: F) -> usize
    where
        F: Fn(u32, &PageCache) -> bool + Send + Sync + 'static,
    {
        let pointer = Box::new(EvictListener {
            owner,
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

    fn evict_cache(
        &self,
        file: &FileNode,
        listeners: &[EvictListenerSnapshot],
        pn: u32,
        page: &mut PageCache,
        allowed_deferred_owner: Option<CachedFileEvictionOwner>,
    ) -> VfsResult<EvictionAcknowledgement> {
        if page.is_pinned() {
            return Err(VfsError::ResourceBusy);
        }
        let acknowledgement = acknowledge_cached_page_eviction_with_listeners(
            listeners,
            pn,
            page,
            allowed_deferred_owner,
        )?;
        let _ = writeback_cached_page_data(file, pn, page)?;
        page.clear_dirty();
        // Retiring an untouched readahead page is not a workingset eviction:
        // it has never been consumed by the caller.  In particular, do not
        // let an older shadow survive this explicit retirement.
        if page.is_unused_prefetched() {
            clear_file_cache_shadows(&self.shared, [pn]);
        } else {
            self.record_eviction(pn);
        }
        Ok(acknowledgement)
    }

    fn drain_cache(&self, file: &FileNode) -> VfsResult<()> {
        let _ = file;
        let _direct_guard = self.shared.direct_io_lock.write();
        let mutation = self.begin_cache_invalidating_mutation()?;
        sync_and_invalidate_cached_file_pages_locked(&self.inner, &self.shared, &mutation)
    }

    fn discard_cache(&self) -> VfsResult<()> {
        discard_cached_pages(&self.shared)
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
        flush_dirty_cache_shared(&self.shared, file)
    }

    /// Writes dirty cached pages intersecting one byte range. `len == 0`
    /// means through EOF. The shared cache/writeback locks serialize this
    /// selection with concurrent writers and make completion observable to a
    /// later range wait.
    fn sync_range_marked(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> Result<(), RangeSyncError> {
        let _direct_guard = self.shared.direct_io_lock.read();
        // In-memory nodes have no backing device; range writeback is a no-op.
        if self.in_memory {
            return Ok(());
        }
        let file = self
            .inner
            .entry()
            .as_file()
            .map_err(RangeSyncError::Immediate)?;
        let end = if len == 0 {
            u64::MAX
        } else {
            offset
                .checked_add(len)
                .ok_or(RangeSyncError::Immediate(VfsError::InvalidInput))?
        };
        let first = offset / PAGE_SIZE as u64;
        let last = if end == u64::MAX {
            u64::MAX
        } else {
            end.saturating_sub(1) / PAGE_SIZE as u64
        };
        let dirty_pages = {
            let guard = self.shared.page_cache.lock();
            guard
                .iter()
                .filter_map(|(pn, page)| {
                    (page.is_dirty() && (*pn as u64 >= first) && (*pn as u64 <= last))
                        .then_some(*pn)
                })
                .collect::<Vec<_>>()
        };
        let _range_lease = CachedFileShared::try_range_cache_lease(
            &self.shared,
            offset..end,
            RangeCacheLeaseKind::CachedWrite,
        )
        .map_err(RangeSyncError::Immediate)?;
        let _writeback_guard = self.shared.writeback_lock.read();
        flush_dirty_page_list_locked(&self.shared, file, dirty_pages, true)
            .map_err(RangeSyncError::Writeback)?;
        // sync_file_range writes selected dirty cache pages only.  In
        // particular it must not turn range writeback into fsync-like
        // metadata or device-cache persistence.
        let _ = data_only;
        Ok(())
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        self.sync_range_marked(offset, len, data_only)
            .map_err(|error| match error {
                RangeSyncError::Immediate(error)
                | RangeSyncError::Writeback(DirtyWritebackError { error, .. }) => error,
            })
    }

    fn complete_range_writeback(&self, result: Result<(), RangeSyncError>) -> VfsResult<()> {
        match result {
            Ok(()) => Ok(()),
            Err(RangeSyncError::Immediate(error)) => Err(error),
            Err(RangeSyncError::Writeback(error)) => {
                if error.worker_must_publish && !error.errseq_published {
                    publish_async_dirty_writeback_completion_error(
                        self.inner
                            .entry()
                            .as_file()
                            .expect("range writeback file changed type"),
                        error.error,
                    );
                }
                Err(error.error)
            }
        }
    }

    fn range_writeback_snapshot(&self) -> RangeWritebackFence {
        let generation = {
            let mut queue = self.shared.range_writeback.queue.lock();
            let generation = queue.next_generation;
            *queue.interests.entry(generation).or_default() += 1;
            generation
        };
        RangeWritebackFence {
            shared: Some(self.shared.clone()),
            generation,
        }
    }

    fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        let (generation, start_worker) = {
            let mut queue = self.shared.range_writeback.queue.lock();
            queue.next_generation = queue
                .next_generation
                .checked_add(1)
                .ok_or(VfsError::NoMemory)?;
            let generation = queue.next_generation;
            queue.pending.push_back(RangeWritebackRequest {
                generation,
                offset,
                len,
                data_only,
            });
            *queue.interests.entry(generation).or_default() += 1;
            let start = !queue.worker_running;
            if start {
                queue.worker_running = true;
            }
            (generation, start)
        };
        if start_worker {
            let worker = self.clone();
            if axtask::try_spawn_with_name(
                move || worker.range_writeback_worker(),
                alloc::string::String::from("range-writeback"),
            )
            .is_err()
            {
                let mut queue = self.shared.range_writeback.queue.lock();
                // Other submitters may have observed worker_running while the
                // task allocation was in progress. Complete every request in
                // that admission epoch with a definite error rather than
                // leaving their WAIT_* callers behind an orphaned FIFO.
                while let Some(request) = queue.pending.pop_front() {
                    queue.completed.push(RangeWritebackCompletion {
                        generation: request.generation,
                        offset: request.offset,
                        len: request.len,
                        result: Err(VfsError::NoMemory),
                    });
                }
                if let Some(count) = queue.interests.get_mut(&generation) {
                    *count -= 1;
                    if *count == 0 {
                        queue.interests.remove(&generation);
                    }
                }
                gc_range_writeback_completions(&mut queue);
                queue.worker_running = false;
                drop(queue);
                self.shared.range_writeback.completed.notify_all(false);
                return Err(VfsError::NoMemory);
            }
        }
        Ok(RangeWritebackFence {
            shared: Some(self.shared.clone()),
            generation,
        })
    }

    fn range_writeback_worker(&self) {
        loop {
            let request = {
                let mut queue = self.shared.range_writeback.queue.lock();
                match queue.pending.pop_front() {
                    Some(request) => {
                        queue.active = Some((request.generation, request.offset, request.len));
                        request
                    }
                    None => {
                        queue.worker_running = false;
                        return;
                    }
                }
            };
            // The worker, rather than submitter, owns the synchronous lower
            // writeback.  The range/direct-I/O locks are taken inside this
            // call and are never held while the request waits in the FIFO.
            let result = self.complete_range_writeback(self.sync_range_marked(
                request.offset,
                request.len,
                request.data_only,
            ));
            let mut queue = self.shared.range_writeback.queue.lock();
            queue.active = None;
            queue.completed.push(RangeWritebackCompletion {
                generation: request.generation,
                offset: request.offset,
                len: request.len,
                result,
            });
            gc_range_writeback_completions(&mut queue);
            drop(queue);
            self.shared.range_writeback.completed.notify_all(false);
        }
    }

    fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        let generation = fence.generation;
        let overlaps = |start: u64, length: u64| {
            let end = if len == 0 {
                u64::MAX
            } else {
                offset.saturating_add(len)
            };
            let other_end = if length == 0 {
                u64::MAX
            } else {
                start.saturating_add(length)
            };
            start < end && offset < other_end
        };
        self.shared
            .range_writeback
            .completed
            .wait_until_interruptible(|| {
                let queue = self.shared.range_writeback.queue.lock();
                let active = queue.active.is_some_and(
                    |(request_generation, request_offset, request_len)| {
                        request_generation <= generation && overlaps(request_offset, request_len)
                    },
                );
                !active
                    && !queue.pending.iter().any(|request| {
                        request.generation <= generation && overlaps(request.offset, request.len)
                    })
            })
            .map_err(|_| VfsError::Interrupted)?;
        let queue = self.shared.range_writeback.queue.lock();
        let mut result = Ok(());
        for completion in &queue.completed {
            if completion.generation <= generation && overlaps(completion.offset, completion.len) {
                if let Err(error) = completion.result {
                    result = Err(error);
                    break;
                }
            }
        }
        drop(queue);
        result
    }

    fn aligned_page_range(offset: u64, len: usize) -> Option<Range<u32>> {
        if len == 0 || offset % PAGE_SIZE as u64 != 0 || len % PAGE_SIZE != 0 {
            return None;
        }
        let start = (offset / PAGE_SIZE as u64) as u32;
        let end = ((offset + len as u64) / PAGE_SIZE as u64) as u32;
        Some(start..end)
    }

    fn range_has_cached_page(&self, pages: &Range<u32>) -> bool {
        let guard = self.shared.page_cache.lock();
        pages.clone().any(|pn| guard.contains(&pn))
    }

    /// Demotes resident cache pages in `pages` to the cold end of the LRU.
    /// Missing pages are deliberately left untouched: MADV_COLD must not
    /// fault or allocate file-cache entries.
    pub fn cold_pages(&self, pages: Range<u32>) -> VfsResult<usize> {
        let mut cache = self.shared.page_cache.lock();
        let mut demoted = 0usize;
        for pn in pages {
            if cache.contains(&pn) {
                cache.demote(&pn);
                demoted = demoted.checked_add(1).ok_or(VfsError::NoMemory)?;
            }
        }
        Ok(demoted)
    }

    /// Writes back and evicts resident file-cache pages in `pages`.
    ///
    /// This never creates pages.  In-memory files and pages that are pinned,
    /// under writeback, or blocked by a concurrent cache user are skipped;
    /// pageout is advisory and must not discard data merely to satisfy a
    /// reclaim hint.  Each page owns an invalidation transaction so earlier
    /// successful evictions remain committed if a later page fails.
    pub fn pageout_pages(&self, pages: Range<u32>) -> VfsResult<usize> {
        if self.in_memory {
            return Ok(0);
        }
        let _direct_guard = self.shared.direct_io_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(0),
            Err(error) => return Err(error),
        };
        let file = self.inner.entry().as_file()?;
        let mut evicted = 0usize;
        for pn in pages {
            let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
            if !invalidation.stage_page_for_pageout(pn)? {
                continue;
            }
            invalidation.writeback(file, true)?;
            invalidation.acknowledge_pageout()?;
            invalidation.commit_discard();
            evicted = evicted.checked_add(1).ok_or(VfsError::NoMemory)?;
        }
        release_cached_file_writeback_anchor_if_clean(&self.shared);
        Ok(evicted)
    }

    fn invalidate_cached_range(
        &self,
        file: &FileNode,
        pages: Range<u32>,
        mutation: &CachedFileMutationGuard,
    ) -> VfsResult<usize> {
        let mut invalidation = CachedPageInvalidationTransaction::new(mutation);
        let count = invalidation.stage_range(pages)?;
        invalidation.writeback(file, true)?;
        invalidation.commit_discard();
        record_cached_file_counter(&RANGE_INVALIDATE_PAGES, count as u64);
        Ok(count)
    }

    fn try_read_aligned_bypass(
        &self,
        dst: &mut (impl Write + IoBufMut),
        offset: u64,
        len: usize,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let mut total = 0;
        let mut current = offset;
        let mut chunk = vec![0_u8; ALIGNED_BYPASS_CHUNK.min(len).max(PAGE_SIZE)];
        while total < len && dst.remaining_mut() > 0 {
            let limit = (len - total).min(chunk.len()).min(dst.remaining_mut());
            let async_read = {
                let mut bufs = [&mut chunk[..limit]];
                match file.try_read_at_vectored_async(&mut bufs, current) {
                    Ok(read) => read,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                }
            };
            let read = match async_read {
                Some(read) => read,
                None => match file.read_at(&mut chunk[..limit], current) {
                    Ok(read) => read,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                },
            };
            crate::account_backing_read(read);
            if read == 0 {
                break;
            }
            let written = match dst.write(&chunk[..read]) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if written == 0 {
                break;
            }
            total += written;
            current += written as u64;
            if written < read || read < limit {
                break;
            }
        }
        if total > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, total as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(total))
    }

    fn try_read_aligned_slice_bypass(
        &self,
        dst: &mut [u8],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, dst.len()) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let async_read = {
            let mut bufs = [&mut *dst];
            file.try_read_at_vectored_async(&mut bufs, offset)?
        };
        let read = match async_read {
            Some(read) => read,
            None => file.read_at(dst, offset)?,
        };
        crate::account_backing_read(read);
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    fn try_read_aligned_vectored_bypass(
        &self,
        dst: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let len = dst.iter().map(|buf| buf.len()).sum();
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let read = match file.try_read_at_vectored_async(dst, offset)? {
            Some(read) => read,
            None => file.read_at_vectored(dst, offset)?,
        };
        crate::account_backing_read(read);
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    fn try_read_aligned_pinned_bypass(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        len: usize,
        _try_async: bool,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        // Cache invalidation and pin admission must exclude cached aliases
        // before the raw copy into caller-pinned memory starts.
        let _direct_guard = self.shared.direct_io_lock.write();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        // Pinned user pages remain concurrently accessible from userspace.
        // Keep Rust references confined to kernel-owned bounce storage.
        let read = unsafe { read_file_into_pinned_bounce(file, dst, offset, len) }?;
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    fn try_write_aligned_bypass(
        &self,
        src: &mut (impl Read + IoBuf),
        offset: u64,
        len: usize,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        match self.invalidate_cached_range(file, pages.clone(), &mutation) {
            Ok(_) => {}
            // A source backed by one of the target cache pages carries a
            // precise page pin. Keep that page resident and let the cached
            // overlap-aware path below move it through a bounce buffer.
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        }

        let mut total = 0;
        let mut current = offset;
        let mut chunk = vec![0_u8; ALIGNED_BYPASS_CHUNK.min(len).max(PAGE_SIZE)];
        while total < len && src.remaining() > 0 {
            let limit = (len - total).min(chunk.len()).min(src.remaining());
            let read = match src.read(&mut chunk[..limit]) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if read == 0 {
                break;
            }
            let written = match file.write_at(&chunk[..read], current) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            crate::account_backing_write(written);
            if written == 0 {
                break;
            }
            total += written;
            current += written as u64;
            if written < read {
                break;
            }
        }

        self.invalidate_cached_range(file, pages, &mutation)?;
        if total > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, total as u64);
        }
        Ok(Some(total))
    }

    fn try_write_aligned_slice_bypass(&self, src: &[u8], offset: u64) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, src.len()) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.invalidate_cached_range(file, pages.clone(), &mutation)?;

        let written = file.write_at(src, offset)?;
        crate::account_backing_write(written);

        self.invalidate_cached_range(file, pages, &mutation)?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    fn try_write_aligned_vectored_bypass(
        &self,
        src: &[&[u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let len = src.iter().map(|buf| buf.len()).sum();
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.write();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.invalidate_cached_range(file, pages.clone(), &mutation)?;

        let written = match file.try_write_at_vectored_async(src, offset)? {
            AsyncVectoredWriteOutcome::Completed(written) => written,
            AsyncVectoredWriteOutcome::NotSubmitted => file.write_at_vectored(src, offset)?,
            AsyncVectoredWriteOutcome::CompletionError(error) => return Err(error),
        };
        crate::account_backing_write(written);

        self.invalidate_cached_range(file, pages, &mutation)?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    fn try_write_aligned_pinned_bypass(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        len: usize,
        _try_async: bool,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        // Own the complete invalidation domain before copying from the pinned
        // source through kernel-owned bounce storage.
        let _direct_guard = self.shared.direct_io_lock.write();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        match self.invalidate_cached_range(file, pages.clone(), &mutation) {
            Ok(_) => {}
            // The physical source may itself be a precisely pinned target
            // cache page. Preserve it and use the overlap-aware cached path.
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        }

        // Pinned user pages are stable but not Rust-shared or exclusive.
        let written = unsafe { write_file_from_pinned_bounce(file, src, offset, len) }?;

        self.invalidate_cached_range(file, pages, &mutation)?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    fn ensure_page_cached(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
    ) -> VfsResult<Option<EvictedPage>> {
        self.ensure_page_cached_with(file, cache, pn, true, true, true)
    }

    fn ensure_page_cached_for_owner(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
        owner: CachedFileEvictionOwner,
    ) -> VfsResult<Option<EvictedPage>> {
        if let Some(page) = cache.get_mut(&pn) {
            page.clear_prefetched();
            file_cache_record_page_reference(page);
            return Ok(None);
        }
        if cache.len() < cache.cap().get() {
            // The owner-aware path is called while an address space owns its
            // mapping transaction. Keep population synchronous until MM can
            // drop that lock and range-revalidate after I/O.
            return self.ensure_page_cached_with(file, cache, pn, true, false, true);
        }

        // Load the replacement before touching the resident cache. Once an
        // owner defers PTE detachment, no fallible work remains between removal
        // of the old page and returning its ownership to the caller.
        let mut replacement = PageCache::new(self.shared.in_memory)?;
        replacement.data().fill(0);
        let offset = u64::from(pn) * PAGE_SIZE as u64;
        let read = file.read_at(replacement.data(), offset)?;
        if !self.shared.in_memory {
            crate::account_backing_read(read);
        }

        let listeners = evict_listeners_snapshot(&self.shared)?;
        let Some((evicted_pn, mut evicted_page)) = pop_unpinned_lru_page(cache)? else {
            return Err(VfsError::ResourceBusy);
        };
        let acknowledgement =
            match self.evict_cache(file, &listeners, evicted_pn, &mut evicted_page, Some(owner)) {
                Ok(acknowledgement) => acknowledgement,
                Err(error) => {
                    restore_popped_cache_page(cache, evicted_pn, evicted_page);
                    return Err(error);
                }
            };
        let retained_page = acknowledgement.deferred.then_some(evicted_page);
        consume_file_cache_shadow(&self.shared, pn);
        replacement.record_reference();
        cache.put(pn, replacement);
        file_cache_resident_add(1);
        Ok(Some(EvictedPage {
            pn: evicted_pn,
            deferred_owner: acknowledgement.deferred.then_some(owner),
            _page: retained_page,
        }))
    }

    fn ensure_page_cached_with(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
        readahead: bool,
    ) -> VfsResult<Option<EvictedPage>> {
        if let Some(page) = cache.get_mut(&pn) {
            if load_from_file && page.clear_prefetched() {
                record_readahead_hit();
            } else if !load_from_file {
                page.clear_prefetched();
            }
            file_cache_record_page_reference(page);
            return Ok(None);
        }
        let readahead_enabled = load_from_file && readahead && cached_readahead_enabled();
        if readahead_enabled {
            record_readahead_miss();
        }
        let cap = cache.cap().get();
        let mut evicted = None;
        // Make room for the requested page. The caller may receive this
        // EvictedPage; any further evictions done for readahead below are
        // written back and dropped.
        if cache.len() >= cap {
            let listeners = evict_listeners_snapshot(&self.shared)?;
            if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                if let Err(error) = self.evict_cache(file, &listeners, epn, &mut epage, None) {
                    restore_popped_cache_page(cache, epn, epage);
                    return Err(error);
                }
            } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                let acknowledgement =
                    match self.evict_cache(file, &listeners, epn, &mut epage, None) {
                        Ok(acknowledgement) => acknowledgement,
                        Err(error) => {
                            restore_popped_cache_page(cache, epn, epage);
                            return Err(error);
                        }
                    };
                debug_assert!(!acknowledgement.deferred);
                evicted = Some(EvictedPage {
                    pn: epn,
                    deferred_owner: None,
                    _page: None,
                });
            }
        }

        if !load_from_file {
            let mut page = PageCache::new(self.shared.in_memory)?;
            page.data().fill(0);
            page.record_reference();
            consume_file_cache_shadow(&self.shared, pn);
            cache.put(pn, page);
            file_cache_resident_add(1);
            record_cached_file_counter(&WRITE_NO_READ_INSERT_PAGES, 1);
            record_cached_file_counter(&WRITE_NO_READ_INSERT_BYTES, PAGE_SIZE as u64);
            return Ok(evicted);
        }

        // Readahead: allocate private page-cache pages, read into those pages
        // while they are still unpublished, then insert only completed data.
        // This keeps readers from observing partial page-fill state.
        let avail = cap.saturating_sub(cache.len());
        let ra = if readahead_enabled {
            READAHEAD_PAGES.min(avail).max(1)
        } else {
            1
        };
        let base = pn as u64 * PAGE_SIZE as u64;
        let async_page_fill = allow_async_page_fill && {
            #[cfg(feature = "ext4")]
            {
                lwext4_rust::async_mapped_read_enabled()
            }
            #[cfg(not(feature = "ext4"))]
            {
                false
            }
        };
        if !async_page_fill {
            let mut buf = vec![0u8; ra * PAGE_SIZE];
            let read = file.read_at(&mut buf, base)?;
            if !self.shared.in_memory {
                crate::account_backing_read(read);
            }

            let mut page = PageCache::new(self.shared.in_memory)?;
            let data = page.data();
            data.fill(0);
            let n0 = read.min(PAGE_SIZE);
            data[..n0].copy_from_slice(&buf[..n0]);
            page.record_reference();
            consume_file_cache_shadow(&self.shared, pn);
            cache.put(pn, page);
            file_cache_resident_add(1);

            let mut loaded_readahead_pages = 0usize;
            for i in 1..ra {
                let off = i * PAGE_SIZE;
                if off >= read {
                    break;
                }
                let next_pn = pn + i as u32;
                if cache.contains(&next_pn) {
                    continue;
                }
                if cache.len() >= cap {
                    let listeners = evict_listeners_snapshot(&self.shared)?;
                    if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                        if let Err(error) =
                            self.evict_cache(file, &listeners, epn, &mut epage, None)
                        {
                            restore_popped_cache_page(cache, epn, epage);
                            return Err(error);
                        }
                    } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                        if let Err(error) =
                            self.evict_cache(file, &listeners, epn, &mut epage, None)
                        {
                            restore_popped_cache_page(cache, epn, epage);
                            return Err(error);
                        }
                    }
                }
                let mut np = PageCache::new(self.shared.in_memory)?;
                let nd = np.data();
                nd.fill(0);
                let chunk_end = (off + PAGE_SIZE).min(read);
                nd[..chunk_end - off].copy_from_slice(&buf[off..chunk_end]);
                np.mark_prefetched();
                consume_file_cache_shadow(&self.shared, next_pn);
                cache.put(next_pn, np);
                file_cache_resident_add(1);
                cache.demote(&next_pn);
                loaded_readahead_pages += 1;
            }
            if readahead_enabled {
                record_readahead_window(loaded_readahead_pages);
            }

            return Ok(evicted);
        }

        let mut pages = Vec::with_capacity(ra);
        for _ in 0..ra {
            let mut page = PageCache::new(self.shared.in_memory)?;
            page.data().fill(0);
            pages.push(page);
        }
        let read = {
            let mut bufs = pages.iter_mut().map(|page| page.data()).collect::<Vec<_>>();
            match file.try_read_at_vectored_async(&mut bufs, base)? {
                Some(read) => read,
                None => file.read_at_vectored(&mut bufs, base)?,
            }
        };
        if !self.shared.in_memory {
            crate::account_backing_read(read);
        }

        #[cfg(feature = "ext4")]
        let mut async_filled_pages = 0usize;
        let mut loaded_readahead_pages = 0usize;
        for (i, page) in pages.into_iter().enumerate() {
            let off = i * PAGE_SIZE;
            if i > 0 && off >= read {
                break; // reached EOF
            }
            let target_pn = pn + i as u32;
            if i > 0 && cache.contains(&target_pn) {
                continue;
            }
            if i > 0 && cache.len() >= cap {
                let listeners = evict_listeners_snapshot(&self.shared)?;
                if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                    if let Err(error) = self.evict_cache(file, &listeners, epn, &mut epage, None) {
                        restore_popped_cache_page(cache, epn, epage);
                        return Err(error);
                    }
                } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                    if let Err(error) = self.evict_cache(file, &listeners, epn, &mut epage, None) {
                        restore_popped_cache_page(cache, epn, epage);
                        return Err(error);
                    }
                }
            }
            #[cfg(feature = "ext4")]
            if off < read {
                async_filled_pages += 1;
            }
            let mut page = page;
            if i > 0 {
                page.mark_prefetched();
            } else {
                page.record_reference();
            }
            consume_file_cache_shadow(&self.shared, target_pn);
            cache.put(target_pn, page);
            file_cache_resident_add(1);
            if i > 0 {
                cache.demote(&target_pn);
                loaded_readahead_pages += 1;
            }
        }

        #[cfg(feature = "ext4")]
        {
            lwext4_rust::record_readahead_async_pages(async_filled_pages);
        }
        if readahead_enabled {
            record_readahead_window(loaded_readahead_pages);
        }

        Ok(evicted)
    }

    /// Invokes `f` with the cached page at `pn`, or `None` if it is not cached.
    pub fn with_page<R>(&self, pn: u32, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        let mut f = Some(f);
        let _range_lease = match CachedFileShared::try_range_cache_lease(
            &self.shared,
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        ) {
            Ok(lease) => Some(lease),
            Err(_) => return f.take().unwrap()(None),
        };
        loop {
            let mut guard = self.shared.page_cache.lock();
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            if let Some(page) = guard.get_mut(&pn) {
                file_cache_record_page_reference(page);
            }
            let result = f.take().unwrap()(guard.get_mut(&pn));
            let dirty = guard.get(&pn).is_some_and(PageCache::is_dirty);
            drop(guard);
            if dirty {
                retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
            }
            return result;
        }
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
        let _cache_user = self.begin_cache_user_range(
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        )?;
        let mut f = Some(f);
        loop {
            let mut guard = self.shared.page_cache.lock();
            let evicted = self.ensure_page_cached(self.inner.entry().as_file()?, &mut guard, pn)?;
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(evicted);
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let page = guard.get_mut(&pn).unwrap();
            let result = f.take().unwrap()(page, evicted);
            let dirty = guard.get(&pn).is_some_and(PageCache::is_dirty);
            drop(guard);
            if dirty {
                retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
            }
            return result;
        }
    }

    /// Owner-aware variant used while one address space already owns its lock.
    ///
    /// Only listeners registered for `owner` may defer detachment. The returned
    /// [`EvictedPage`] keeps such a page alive until the caller drains all
    /// aliases for that same owner.
    pub fn with_page_or_insert_for_owner<R>(
        &self,
        pn: u32,
        owner: CachedFileEvictionOwner,
        f: impl FnOnce(&mut PageCache, Option<EvictedPage>) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let _cache_user = self.begin_cache_user_range(
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        )?;
        let mut f = Some(f);
        loop {
            let mut guard = self.shared.page_cache.lock();
            let evicted = self.ensure_page_cached_for_owner(
                self.inner.entry().as_file()?,
                &mut guard,
                pn,
                owner,
            )?;
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(evicted);
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let page = guard.get_mut(&pn).unwrap();
            let result = f.take().unwrap()(page, evicted);
            let dirty = guard.get(&pn).is_some_and(PageCache::is_dirty);
            drop(guard);
            if dirty {
                retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
            }
            return result;
        }
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
        wait_writeback: bool,
        allow_async_page_fill: bool,
        readahead: bool,
    ) -> VfsResult<T> {
        let _cache_user =
            self.begin_cache_user_range(range.clone(), RangeCacheLeaseKind::CachedWrite)?;
        let file = self.inner.entry().as_file()?;
        let mut initial = page_initial(file)?;
        let start_page = (range.start / PAGE_SIZE as u64) as u32;
        let end_page = range.end.div_ceil(PAGE_SIZE as u64) as u32;
        let mut page_offset = (range.start % PAGE_SIZE as u64) as usize;
        for pn in start_page..end_page {
            let page_start = pn as u64 * PAGE_SIZE as u64;
            loop {
                let mut guard = self.shared.page_cache.lock();
                let page_range =
                    page_offset..(range.end - page_start).min(PAGE_SIZE as u64) as usize;
                let load_from_file = load_page(page_start, &page_range);
                self.ensure_page_cached_with(
                    file,
                    &mut guard,
                    pn,
                    load_from_file,
                    allow_async_page_fill,
                    readahead,
                )?;
                if wait_writeback && guard.get(&pn).is_some_and(PageCache::is_writeback) {
                    drop(guard);
                    wait_for_page_writeback_clear(&self.shared, pn);
                    continue;
                }
                let page = guard.get_mut(&pn).unwrap();
                initial = page_each(initial, page, page_start, page_range)?;
                break;
            }
            page_offset = 0;
        }

        Ok(initial)
    }

    fn prepare_write_page(
        &self,
        file: &FileNode,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
    ) -> VfsResult<CachedFilePagePin> {
        let _cache_user = self.begin_cache_user()?;
        loop {
            let mut guard = self.shared.page_cache.lock();
            let evicted = self.ensure_page_cached_with(
                file,
                &mut guard,
                pn,
                load_from_file,
                allow_async_page_fill,
                true,
            )?;
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(evicted);
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let range_lease = Some(CachedFileShared::try_range_cache_lease(
                &self.shared,
                page_range(u64::from(pn), 1),
                RangeCacheLeaseKind::CachedWrite,
            )?);
            guard
                .get_mut(&pn)
                .expect("prepared cache page disappeared while locked")
                .pin()?;
            drop(guard);
            drop(evicted);
            return Ok(CachedFilePagePin {
                cache: self.clone(),
                pn,
                dirty_on_release: false,
                _range_lease: range_lease,
            });
        }
    }

    fn commit_prepared_write(&self, pn: u32, range: Range<usize>, src: &[u8]) {
        loop {
            let mut guard = self.shared.page_cache.lock();
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let page = guard
                .get_mut(&pn)
                .expect("pinned cache page disappeared before write commit");
            page.data()[range].copy_from_slice(src);
            page.mark_dirty();
            return;
        }
    }

    fn read_at_with_async_policy(
        &self,
        mut dst: impl Write + IoBufMut,
        offset: u64,
        allow_async: bool,
        readahead: bool,
    ) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let requested = u64::try_from(dst.remaining_mut()).map_err(|_| VfsError::InvalidInput)?;
        let end = offset
            .checked_add(requested)
            .ok_or(VfsError::InvalidInput)?
            .min(len);
        if end <= offset {
            return Ok(0);
        }
        if allow_async {
            if let Some(read) =
                self.try_read_aligned_bypass(&mut dst, offset, (end - offset) as usize)?
            {
                return Ok(read);
            }
        }
        let _direct_guard = self.shared.direct_io_lock.read();
        let mut total = 0usize;
        let mut current = offset;
        let mut bounce = vec![0_u8; PAGE_SIZE];
        while current < end {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = usize::try_from(end - current)
                .map_err(|_| VfsError::InvalidInput)?
                .min(PAGE_SIZE - page_offset);
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let copied = match self.with_pages(
                current..next,
                |_| Ok(0usize),
                |_, _| true,
                |_, page, _page_start, range| {
                    let copied = range.end - range.start;
                    bounce[..copied].copy_from_slice(&page.data()[range]);
                    Ok(copied)
                },
                false,
                allow_async,
                readahead,
            ) {
                Ok(copied) => copied,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            // The page-cache borrow and its lock have ended before an IoBufMut
            // such as VmBytesMut performs a raw userspace copy.
            let written = match dst.write(&bounce[..copied]) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if written > copied {
                return Err(VfsError::InvalidInput);
            }
            total += written;
            current += written as u64;
            if written < copied || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, true, true)
    }

    /// Reads with caller-selected automatic read-ahead.  The policy belongs
    /// to the open file description, never to this inode-shared cache.
    pub fn read_at_with_readahead(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        readahead: bool,
    ) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, true, readahead)
    }

    /// Reads through the coherent page cache without invoking the lower
    /// split-submit asynchronous hook. A synchronous lower call may still
    /// submit a device request, but it completes that request before returning.
    ///
    /// This is intended for callers that already own a larger transaction and
    /// therefore cannot release its transaction and suspend after publishing a
    /// request. Ordinary reads should use [`read_at`](Self::read_at), which may
    /// use the explicit split-submit/wait path when the filesystem supports it.
    pub fn read_at_sync(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, false, true)
    }

    /// Reads into caller-pinned physical memory without exposing an aliasing
    /// Rust slice when the destination is one of this inode's cached pages.
    ///
    /// # Safety
    ///
    /// Every nonempty destination range must remain pinned, mapped, writable,
    /// and accessible for the complete call. Destination ranges must not
    /// overlap each other. Concurrent userspace access is permitted; this path
    /// creates no Rust reference to the physical ranges and copies through
    /// kernel-owned storage. `try_async` is a hint and may be ignored.
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        let requested = validate_pinned_physical_segments(dst, true)?;
        let file_len = self.inner.len()?;
        let end = offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?
            .min(file_len);
        if end <= offset {
            return Ok(0);
        }
        let len = usize::try_from(end - offset).map_err(|_| VfsError::InvalidInput)?;
        if let Some(read) = self.try_read_aligned_pinned_bypass(dst, offset, len, try_async)? {
            return Ok(read);
        }

        let _direct_guard = self.shared.direct_io_lock.read();
        let mut cursor = PinnedPhysicalCursor::new(dst);
        let mut bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE)?;
        let mut total = 0usize;
        let mut current = offset;
        while current < end {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = usize::try_from(end - current)
                .map_err(|_| VfsError::InvalidInput)?
                .min(PAGE_SIZE - page_offset);
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let copied = match self.with_pages(
                current..next,
                |_| Ok(0usize),
                |_, _| true,
                |_, page, _page_start, range| {
                    let copied = range.end - range.start;
                    bounce[..copied].copy_from_slice(&page.data()[range]);
                    Ok(copied)
                },
                false,
                try_async,
                true,
            ) {
                Ok(copied) => copied,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            match unsafe { copy_to_pinned_physical_segments(&mut cursor, &bounce[..copied]) } {
                Ok(()) => {}
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            }
            total += copied;
            current = next;
        }
        Ok(total)
    }

    pub fn read_at_slice(&self, mut dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let end = (offset + dst.len() as u64).min(len);
        if end <= offset {
            return Ok(0);
        }
        dst = &mut dst[..(end - offset) as usize];
        if let Some(read) = self.try_read_aligned_slice_bypass(dst, offset)? {
            return Ok(read);
        }
        self.read_at(&mut dst, offset)
    }

    pub fn read_at_vectored(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        if let Some(read) = self.try_read_aligned_vectored_bypass(dst, offset)? {
            return Ok(read);
        }
        let mut total = 0usize;
        let mut current = offset;
        for buf in dst.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = match self.read_at_slice(buf, current) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            current += read as u64;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn write_at_locked(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let requested = buf.remaining();
        offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        let file = self.inner.entry().as_file()?;
        let mut committed_len = file.len()?;

        let mut written = 0usize;
        let mut current = offset;
        let mut bounce = vec![0_u8; PAGE_SIZE];
        while written < requested {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = (requested - written).min(PAGE_SIZE - page_offset);
            // Finish a generic source read before acquiring a cache page and
            // creating its mutable data reference. VmBytes may point at that
            // exact cached page through a MAP_SHARED mapping.
            let read = match buf.read(&mut bounce[..chunk]) {
                Ok(read) => read,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if read > chunk {
                if written != 0 {
                    break;
                }
                return Err(VfsError::InvalidInput);
            }
            if read == 0 {
                break;
            }
            let next = current
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            let pn =
                u32::try_from(current / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
            let range = page_offset..page_offset + read;
            let load_from_file = !(range.start == 0 && range.end == PAGE_SIZE)
                && u64::from(pn) * (PAGE_SIZE as u64) < committed_len;
            let page_pin = match self.prepare_write_page(file, pn, load_from_file, true) {
                Ok(page_pin) => page_pin,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if next > committed_len {
                match file.set_len(next) {
                    Ok(()) => committed_len = next,
                    Err(_) if written != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            self.commit_prepared_write(pn, range, &bounce[..read]);
            drop(page_pin);
            written += read;
            current = next;
            if read < chunk {
                break;
            }
        }
        if written != 0 {
            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        }
        Ok(written)
    }

    /// Writes from caller-pinned physical memory without exposing an aliasing
    /// Rust slice when a source range is one of this inode's cached pages.
    ///
    /// # Safety
    ///
    /// Every nonempty source range must remain pinned, mapped, readable, and
    /// accessible for the complete call. Concurrent userspace writes may race
    /// with the raw copy, but no Rust reference is created for the physical
    /// range. `try_async` is a hint and may be ignored.
    pub unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        let requested = validate_pinned_physical_segments(src, false)?;
        if requested == 0 {
            return Ok(0);
        }
        offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        if let Some(written) =
            self.try_write_aligned_pinned_bypass(src, offset, requested, try_async)?
        {
            return Ok(written);
        }

        let _direct_guard = self.shared.direct_io_lock.read();
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        let mut committed_len = file.len()?;

        let mut cursor = PinnedPhysicalCursor::new(src);
        let mut bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE)?;
        let mut written = 0usize;
        let mut current = offset;
        while written < requested {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = (requested - written).min(PAGE_SIZE - page_offset);
            match unsafe { copy_from_pinned_physical_segments(&mut cursor, &mut bounce[..chunk]) } {
                Ok(()) => {}
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            }
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let pn =
                u32::try_from(current / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
            let range = page_offset..page_offset + chunk;
            let load_from_file = !(range.start == 0 && range.end == PAGE_SIZE)
                && u64::from(pn) * (PAGE_SIZE as u64) < committed_len;
            let page_pin = match self.prepare_write_page(file, pn, load_from_file, try_async) {
                Ok(page_pin) => page_pin,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if next > committed_len {
                match file.set_len(next) {
                    Ok(()) => committed_len = next,
                    Err(_) if written != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            self.commit_prepared_write(pn, range, &bounce[..chunk]);
            drop(page_pin);
            written += chunk;
            current = next;
        }
        if written != 0 {
            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        }
        Ok(written)
    }

    /// Writes `buf` to the file at `offset`.
    pub fn write_at(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let len = buf.remaining();
        if let Some(written) = self.try_write_aligned_bypass(&mut buf, offset, len)? {
            return Ok(written);
        }
        let _direct_guard = self.shared.direct_io_lock.read();
        let _guard = self.shared.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        if let Some(written) = self.try_write_aligned_slice_bypass(src, offset)? {
            return Ok(written);
        }
        self.write_at(src, offset)
    }

    pub fn write_at_vectored(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        if let Some(written) = self.try_write_aligned_vectored_bypass(src, offset)? {
            return Ok(written);
        }
        let mut total = 0usize;
        let mut current = offset;
        for buf in src.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at_slice(buf, current) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            current += written as u64;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Appends an admitted prefix of `buf` under one inode append transaction.
    ///
    /// `admit` observes the exact end offset protected by the same append
    /// serialization domain as the lower write. It may shorten the operation,
    /// but cannot extend it beyond the caller's remaining input.
    pub fn append_with_admission(
        &self,
        mut buf: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let _guard = self.shared.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        let requested = buf.remaining();
        let allowed = admit(len, requested)?;
        if allowed > requested {
            return Err(VfsError::InvalidInput);
        }
        let mut admitted = (&mut buf).take(allowed as u64);
        let written = self.write_at_locked(&mut admitted, len)?;
        let new_end = len
            .checked_add(written as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok((written, new_end))
    }

    /// Appends `buf` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, buf: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.append_with_admission(buf, |_offset, requested| Ok(requested))
    }

    /// Appends one scatter list as a single inode append transaction.
    ///
    /// Empty slices are ignored. A short element stops the transaction, and
    /// an error keeps the same propagation semantics as repeated scalar
    /// appends, but no other cached or direct writer can enter between two
    /// nonempty elements.
    pub fn append_vectored(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        let _direct_guard = self.shared.direct_io_lock.read();
        let _append_guard = self.shared.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let mut total = 0usize;
        let mut end = file.len()?;

        for buf in src.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at_locked(buf, end) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            end += written as u64;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok((total, end))
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let _direct_guard = self.shared.direct_io_lock.write();
        let _writeback_guard = self.shared.writeback_lock.write();
        wait_for_all_writeback_clear(&self.shared);
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
        // Length changes can relocate, allocate, or release extents even when
        // no cached page is currently discarded. Keep the whole-file mutation
        // token through the lower operation in both directions.
        let mutation = self.begin_cache_invalidating_mutation()?;
        self.admit_truncate(old_len, len)?;
        let partial_page = (old_len > len && len % PAGE_SIZE as u64 != 0)
            .then_some((len / PAGE_SIZE as u64) as u32);
        let mut discarded = if old_len > len {
            let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
            // Include the boundary page. A lower filesystem that reports a
            // non-atomic failure may already have changed its tail; on success
            // the retained prefix is restored below with a zeroed suffix.
            invalidation.stage_from(len / PAGE_SIZE as u64)?;
            invalidation.writeback(file, true)?;
            Some(invalidation)
        } else {
            None
        };
        if let Err(error) = file.set_len(len) {
            if let Some(invalidation) = discarded.take()
                && !file.set_len_failure_is_atomic()
            {
                invalidation.commit_discard();
                release_cached_file_writeback_anchor_if_clean(&self.shared);
            }
            return Err(error);
        }

        let old_last_page = (old_len / PAGE_SIZE as u64) as u32;
        if old_len < len {
            let mut guard = self.shared.page_cache.lock();
            if let Some(page) = guard.get_mut(&old_last_page) {
                let page_start = old_last_page as u64 * PAGE_SIZE as u64;
                let old_page_offset = (old_len - page_start) as usize;
                let new_page_offset = (len - page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[old_page_offset..new_page_offset].fill(0);
            }
        } else if let (Some(partial_page), Some(invalidation)) = (partial_page, discarded.as_mut())
        {
            invalidation.restore_staged_page(partial_page, |page| {
                let page_start = partial_page as u64 * PAGE_SIZE as u64;
                let new_page_offset = len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[new_page_offset..].fill(0);
                // Preserve dirty state for retained bytes; writeback clamps
                // the final partial page to the new inode length.
                page.mark_dirty();
            });
        }
        if let Some(invalidation) = discarded.take() {
            invalidation.commit_discard();
        }
        release_cached_file_writeback_anchor_if_clean(&self.shared);
        retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
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
        let open_handles = self.shared.open_handles.fetch_sub(1, Ordering::AcqRel);
        if open_handles == 0 {
            warn!("CachedFile dropped with no open handle reference");
            return;
        }
        if open_handles > 1 {
            return;
        }
        if self.shared.unlinked.load(Ordering::Acquire) {
            // The last open handle is not necessarily the last physical
            // owner. Keep registry/cache ownership deferred while an exact
            // direct/effect range lease is live; that lease's Drop performs
            // the final synchronous cleanup.
            request_unlinked_cached_file_cleanup(&self.shared);
            return;
        }
        if try_retain_closed_cached_file(&self.inner, &self.shared) {
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

/// Per-open-file-description advice bits selected by POSIX_FADV_*.
///
/// These are deliberately independent: RANDOM suppresses automatic
/// readahead, SEQUENTIAL extends its window, and NOREUSE changes reclamation
/// treatment for pages actually consumed by later reads. NORMAL clears all
/// three, matching Linux's reset behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FadviseReadahead {
    Normal     = 0,
    Random     = 1,
    Sequential = 2,
    NoReuse    = 3,
}

const FADVISE_RANDOM: u8 = 1 << 0;
const FADVISE_SEQUENTIAL: u8 = 1 << 1;
const FADVISE_NOREUSE: u8 = 1 << 2;

#[inline]
const fn fadvise_next_bits(previous: u8, set: u8, clear: u8) -> u8 {
    (previous | set) & !clear
}

impl FileBackend {
    /// Returns the page-cache snapshot for an inclusive page interval.
    /// Direct handles intentionally have no resident high-level cache.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        match self {
            Self::Cached(cached) => cached.cachestat(first_page, last_page),
            Self::Direct(location) => cached_file_shared_for_location(location)
                .map(|shared| shared.cachestat(first_page, last_page))
                .unwrap_or_default(),
        }
    }

    const DIRECT_IO_CHUNK: usize = ALIGNED_BYPASS_CHUNK;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    /// Clones this backend while selecting whether I/O bypasses the page cache.
    ///
    /// Both modes retain the same file location. Direct operations synchronize
    /// and invalidate cached pages before accessing the VFS node.
    pub fn with_direct_io(&self, enabled: bool) -> Self {
        let location = self.location().clone();
        if enabled {
            Self::new_direct(location)
        } else {
            Self::new_cached(location)
        }
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while dst.remaining_mut() > 0 {
                    let limit = dst.remaining_mut().min(chunk.len());
                    let read = match file.read_at(&mut chunk[..limit], offset) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    crate::account_backing_read(read);
                    if read == 0 {
                        break;
                    }
                    let written = match dst.write(&chunk[..read]) {
                        Ok(written) => written,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
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
            }),
        }
    }

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_slice(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let async_read = {
                    let mut bufs = [&mut *dst];
                    file.try_read_at_vectored_async(&mut bufs, offset)?
                };
                let read = match async_read {
                    Some(read) => read,
                    None => file.read_at(dst, offset)?,
                };
                crate::account_backing_read(read);
                Ok(read)
            }),
        }
    }

    pub fn read_at_vectored(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_vectored(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let read = match file.try_read_at_vectored_async(dst, offset)? {
                    Some(read) => read,
                    None => file.read_at_vectored(dst, offset)?,
                };
                crate::account_backing_read(read);
                Ok(read)
            }),
        }
    }

    /// Prepares an owned ext4 physical effect.  Filesystem mapping admission
    /// runs before cache staging, so an unsupported/hole/unwritten/EOF or
    /// non-regular request returns without device or cache side effects.
    #[cfg(feature = "ext4")]
    pub fn prepare_physical_io_effect(
        &self,
        operation: PhysicalIoOperation,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<PhysicalIoEffect>> {
        let total = validate_physical_io_segments(segments, offset)?;
        let Self::Direct(location) = self else {
            return Ok(None);
        };
        let end = offset
            .checked_add(u64::try_from(total).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        let file = location.entry().as_file()?;
        let inode = match file.downcast_owned::<crate::fs::ext4::Inode>() {
            Ok(inode) => inode,
            Err(_) => return Ok(None),
        };
        let Some(effect) = inode.prepare_owned_physical_effect(operation, segments, offset)? else {
            return Ok(None);
        };

        // Do not create a cached-file registry entry or a range lease until
        // filesystem admission has succeeded.  The lower planner uses its
        // non-caching mapping view, so rejected eligibility remains
        // side-effect free and can synchronously select the fallback.
        let shared = cached_file_shared_for_location_or_create(location);
        let range_lease = match CachedFileShared::try_range_cache_lease(
            &shared,
            offset..end,
            if operation == PhysicalIoOperation::Write {
                RangeCacheLeaseKind::DirectWrite
            } else {
                RangeCacheLeaseKind::DirectRead
            },
        ) {
            Ok(lease) => lease,
            Err(VfsError::ResourceBusy | VfsError::Unsupported) => return Ok(None),
            Err(error) => return Err(error),
        };

        // Only the short cache staging window holds the direct/writeback
        // locks.  The returned transaction and range lease are owned by the
        // effect and survive device submission/completion waits.
        let invalidation_result = (|| -> VfsResult<CachedPageInvalidationTransaction> {
            let _direct_guard = shared.direct_io_lock.write();
            let _writeback_guard = shared.writeback_lock.write();
            wait_for_all_writeback_clear(&shared);
            let first_page = offset / PAGE_SIZE as u64;
            let last_page = end.div_ceil(PAGE_SIZE as u64);
            let first_page = u32::try_from(first_page).map_err(|_| VfsError::InvalidInput)?;
            let last_page = u32::try_from(last_page).map_err(|_| VfsError::InvalidInput)?;
            let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
            invalidation.stage_range(first_page..last_page)?;
            invalidation.writeback(file, true)?;
            Ok(invalidation)
        })();
        let invalidation = match invalidation_result {
            Ok(invalidation) => invalidation,
            Err(VfsError::ResourceBusy | VfsError::Unsupported) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(PhysicalIoEffect::new(
            location.clone(),
            inode,
            effect,
            range_lease,
            invalidation,
        )))
    }

    /// Attempts direct I/O into caller-pinned physical SG memory.
    ///
    /// `Ok(None)` is the capability/fallback result.  Validation failures and
    /// lower filesystem errors are returned as errors and are never bounced.
    ///
    /// # Safety
    ///
    /// The caller must keep all physical ranges pinned, DMA-accessible,
    /// writable, and disjoint for the complete call. Concurrent CPU/device
    /// access may race on contents and is the caller's responsibility; this
    /// path does not construct Rust references from physical addresses.
    pub unsafe fn try_read_at_dma_segments_with_reason(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let total = validate_physical_io_segments(dst, offset)?;
        let result = match self {
            Self::Cached(_) => {
                PhysicalIoAttempt::NotSubmitted(PhysicalIoAttemptNotSubmittedReason::Extent)
            }
            Self::Direct(loc) => with_direct_range_operation_after_preflight(
                loc,
                offset,
                total,
                RangeCacheLeaseKind::DirectRead,
                |_, file| file.physical_read_eligible(dst, offset),
                |_, file| unsafe { file.try_read_at_physical_with_reason(dst, offset) },
            )?
            .unwrap_or(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            )),
        };
        if let PhysicalIoAttempt::Completed(bytes) = result {
            if bytes == 0 || bytes > total {
                return Err(VfsError::Io);
            }
            crate::account_backing_read(bytes);
        }
        Ok(result)
    }

    pub unsafe fn try_read_at_dma_segments(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_read_at_dma_segments_with_reason(dst, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    /// Performs positioned I/O into pinned physical destination segments.
    ///
    /// # Safety
    ///
    /// The caller must uphold the pin, mapping, and access contract of
    /// [`CachedFile::read_at_pinned_segments`].
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        _try_async: bool,
    ) -> VfsResult<usize> {
        validate_pinned_physical_segments(dst, true)?;
        match self {
            Self::Cached(cached) => unsafe { cached.read_at_pinned_segments(dst, offset, false) },
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| unsafe {
                read_file_into_pinned_bounce(
                    file,
                    dst,
                    offset,
                    validate_pinned_physical_segments(dst, true)?,
                )
            }),
        }
    }

    /// Writes `src` to the file at `offset`.
    pub fn write_at(&self, mut src: impl Read + IoBuf, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while src.remaining() > 0 {
                    let limit = src.remaining().min(chunk.len());
                    let read = match src.read(&mut chunk[..limit]) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if read == 0 {
                        break;
                    }
                    let written = match file.write_at(&chunk[..read], offset) {
                        Ok(written) => written,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    crate::account_backing_write(written);
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
            }),
        }
    }

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_slice(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let written = file.write_at(src, offset)?;
                crate::account_backing_write(written);
                Ok(written)
            }),
        }
    }

    pub fn write_at_vectored(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_vectored(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let written = match file.try_write_at_vectored_async(src, offset)? {
                    AsyncVectoredWriteOutcome::Completed(written) => written,
                    AsyncVectoredWriteOutcome::NotSubmitted => {
                        file.write_at_vectored(src, offset)?
                    }
                    AsyncVectoredWriteOutcome::CompletionError(error) => return Err(error),
                };
                crate::account_backing_write(written);
                Ok(written)
            }),
        }
    }

    /// Attempts direct overwrite I/O from caller-pinned physical SG memory.
    /// The request is never allowed to extend the file.
    ///
    /// # Safety
    ///
    /// The caller must keep all physical ranges pinned, DMA-accessible,
    /// readable, and disjoint for the complete call. Concurrent CPU/device
    /// access may race on contents and is the caller's responsibility; this
    /// path does not construct Rust references from physical addresses.
    pub unsafe fn try_write_at_dma_segments_with_reason(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let total = validate_physical_io_segments(src, offset)?;
        let result = match self {
            Self::Cached(_) => {
                PhysicalIoAttempt::NotSubmitted(PhysicalIoAttemptNotSubmittedReason::Extent)
            }
            Self::Direct(loc) => with_direct_range_operation_after_preflight(
                loc,
                offset,
                total,
                RangeCacheLeaseKind::DirectWrite,
                |_, file| file.physical_write_eligible(src, offset),
                |_, file| unsafe { file.try_write_at_physical_with_reason(src, offset) },
            )?
            .unwrap_or(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            )),
        };
        if let PhysicalIoAttempt::Completed(bytes) = result {
            if bytes != total {
                return Err(VfsError::Io);
            }
            crate::account_backing_write(bytes);
        }
        Ok(result)
    }

    pub unsafe fn try_write_at_dma_segments(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_write_at_dma_segments_with_reason(src, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    /// Performs positioned I/O from pinned physical source segments.
    ///
    /// # Safety
    ///
    /// The caller must uphold the pin and access contract of
    /// [`CachedFile::write_at_pinned_segments`].
    pub unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        _try_async: bool,
    ) -> VfsResult<usize> {
        validate_pinned_physical_segments(src, false)?;
        match self {
            Self::Cached(cached) => unsafe { cached.write_at_pinned_segments(src, offset, false) },
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |shared, file| {
                let _append_guard = shared.append_lock.read();
                unsafe {
                    write_file_from_pinned_bounce(
                        file,
                        src,
                        offset,
                        validate_pinned_physical_segments(src, false)?,
                    )
                }
            }),
        }
    }

    /// Appends an admitted prefix of `src` under one inode append transaction.
    pub fn append_with_admission(
        &self,
        mut src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append_with_admission(src, admit),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |shared, file| {
                let _append_guard = shared.append_lock.write();
                let mut total = 0;
                let mut end = file.len()?;
                let requested = src.remaining();
                let allowed = admit(end, requested)?;
                if allowed > requested {
                    return Err(VfsError::InvalidInput);
                }
                let mut admitted = (&mut src).take(allowed as u64);
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while admitted.remaining() > 0 {
                    let limit = admitted.remaining().min(chunk.len());
                    let read = match admitted.read(&mut chunk[..limit]) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if read == 0 {
                        break;
                    }
                    let (written, new_end) = match file.append(&chunk[..read]) {
                        Ok(result) => result,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
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
            }),
        }
    }

    /// Appends `src` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.append_with_admission(src, |_offset, requested| Ok(requested))
    }

    /// Appends one scatter list without releasing the inode append domain
    /// between nonempty elements.
    pub fn append_vectored(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append_vectored(src),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |shared, file| {
                let _append_guard = shared.append_lock.write();
                let mut total = 0usize;
                let mut end = file.len()?;

                // The lower async vectored API is positioned I/O, not an
                // append operation. Keep using FileNodeOps::append so a
                // filesystem's own EOF/inode rules remain in force; the
                // shared high-level guards make all chunks and iovecs one
                // transaction relative to every other axfs writer.
                for buf in src.iter().copied() {
                    if buf.is_empty() {
                        continue;
                    }
                    let requested = buf.len();
                    let mut element_written = 0usize;
                    while element_written < requested {
                        let limit = (requested - element_written).min(Self::DIRECT_IO_CHUNK);
                        let chunk = &buf[element_written..element_written + limit];
                        let (written, new_end) = match file.append(chunk) {
                            Ok(result) => result,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        total += written;
                        element_written += written;
                        end = new_end;
                        if written < limit || written == 0 {
                            break;
                        }
                    }
                    if element_written < requested || element_written == 0 {
                        break;
                    }
                }

                Ok((total, end))
            }),
        }
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        match self {
            Self::Cached(cached) => cached.location(),
            Self::Direct(loc) => loc,
        }
    }

    fn fadvise_cache(&self) -> Option<CachedFile> {
        match self {
            Self::Cached(cache) => Some(cache.clone()),
            // An O_DIRECT description has no private buffered cache, but the
            // inode can still have one through another OFD/mapping. Advice
            // must never instantiate that cache through an infallible Arc or
            // registry allocation, so a cache-less direct inode is a no-op.
            Self::Direct(location) => CachedFile::get_existing(location.clone()),
        }
    }

    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_willneed(offset, len),
            None => Ok(()),
        }
    }

    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_noreuse(offset, len),
            None => Ok(()),
        }
    }

    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_dontneed(offset, len),
            None => Ok(()),
        }
    }

    pub fn read_at_with_readahead(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        readahead: bool,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cache) => cache.read_at_with_readahead(dst, offset, readahead),
            Self::Direct(_) => self.read_at(dst, offset),
        }
    }

    /// Flushes cached data (and optionally metadata) to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        record_file_sync_request(data_only);
        match self {
            Self::Cached(cached) => cached.sync(data_only),
            Self::Direct(loc) => {
                with_cache_invalidating_file_operation(loc, |_, file| file.sync(data_only))
            }
        }
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.sync_range(offset, len, data_only),
            Self::Direct(loc) => {
                with_cache_invalidating_file_operation(loc, |_, file| file.sync(data_only))
            }
        }
    }

    pub fn range_writeback_snapshot(&self) -> RangeWritebackFence {
        match self {
            Self::Cached(cached) => cached.range_writeback_snapshot(),
            Self::Direct(_) => RangeWritebackFence::none(),
        }
    }

    pub fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        match self {
            Self::Cached(cached) => cached.submit_range_writeback(offset, len, data_only),
            // There is no page-cache writeback domain for direct handles.
            // Do not substitute a whole-file sync; Linux treats this as an
            // implementation-supported no-op when there is nothing queued.
            Self::Direct(_) => Ok(RangeWritebackFence::none()),
        }
    }

    pub fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.wait_range_writeback_through(fence, offset, len),
            Self::Direct(_) => Ok(()),
        }
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.set_len(len),
            Self::Direct(loc) => with_cache_invalidating_truncate(loc, len),
        }
    }
}

/// Provides `std::fs::File`-like interface.
pub struct File {
    inner: FileBackend,
    /// Mutable open-file-description append status. Access admission remains
    /// in `flags`; toggling append must never manufacture write authority.
    append: AtomicBool,
    flags: FileFlags,
    /// Serializes operations which observe and later commit the current
    /// position without requiring the position spin lock to remain held while
    /// an external transfer consumer runs.
    position_transaction: Mutex<()>,
    position: Option<Mutex<u64>>,
    readahead: AtomicU8,
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

        // `wall_time` is the Unix-epoch wall clock used for inode metadata;
        // convert its unsigned legacy representation into a VFS timestamp.
        let now: Timestamp = axhal::time::wall_time().into();
        let mut update = MetadataUpdate::default();
        if flags & 1 != 0 {
            update.atime = Some(now);
        }
        if flags & 2 != 0 {
            update.mtime = Some(now);
            update.ctime = Some(now);
        }
        if let Err(err) = self.inner.location().update_supported_metadata(update) {
            warn!("Failed to update file times: {err:?}");
            self.access_flags.fetch_or(flags, Ordering::AcqRel);
        }
    }

    /// Creates a new [`File`] from a [`FileBackend`] and access flags.
    pub fn new(inner: FileBackend, flags: FileFlags) -> Self {
        let position = if inner.location().flags().contains(NodeFlags::STREAM) {
            None
        } else {
            // O_APPEND changes where each write commits; it does not seek the
            // newly opened description to EOF. Clearing append before the
            // first write must therefore expose the initial offset zero.
            Some(Mutex::new(0))
        };
        Self {
            inner,
            append: AtomicBool::new(flags.contains(FileFlags::APPEND)),
            flags: flags & !FileFlags::APPEND,
            position_transaction: Mutex::new(()),
            position,
            readahead: AtomicU8::new(FadviseReadahead::Normal as u8),
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
        let requires_append = flags.contains(FileFlags::APPEND);
        let required_access = flags & !FileFlags::APPEND;
        if self.flags.contains(required_access)
            && (!requires_append
                || (self.flags.contains(FileFlags::WRITE) && self.append_enabled()))
            && !self.is_path()
        {
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
        let mut flags = self.flags;
        flags.set(FileFlags::APPEND, self.append_enabled());
        flags
    }

    /// Whether ordinary I/O on this open file description owns a cursor.
    pub fn has_current_position(&self) -> bool {
        self.position.is_some()
    }

    /// Whether the node accepts explicit-offset reads.
    pub fn supports_positioned_read(&self) -> bool {
        !self
            .location()
            .flags()
            .contains(NodeFlags::NO_POSITIONED_READ)
    }

    /// Whether the node accepts explicit-offset writes.
    pub fn supports_positioned_write(&self) -> bool {
        !self
            .location()
            .flags()
            .contains(NodeFlags::NO_POSITIONED_WRITE)
    }

    /// Whether the node accepts seek operations.
    pub fn supports_seek(&self) -> bool {
        !self.location().flags().contains(NodeFlags::NO_SEEK)
    }

    /// Updates append status for this open file description without changing
    /// its immutable read/write access mode.
    pub fn set_append(&self, append: bool) {
        self.append.store(append, Ordering::Release);
    }

    fn append_enabled(&self) -> bool {
        self.append.load(Ordering::Acquire)
    }

    /// Returns a reference to the underlying [`FileBackend`].
    pub fn backend(&self) -> VfsResult<&FileBackend> {
        self.access(FileFlags::empty())?;
        Ok(&self.inner)
    }

    /// Page-cache statistics do not require data-I/O access, so O_PATH file
    /// descriptors may query their regular file's mapping as on Linux.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        self.inner.cachestat(first_page, last_page)
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        self.inner.location()
    }

    pub fn set_fadvise_readahead(&self, policy: FadviseReadahead) {
        match policy {
            FadviseReadahead::Normal => self.readahead.store(0, Ordering::Release),
            FadviseReadahead::Random => {
                self.update_fadvise_bits(FADVISE_RANDOM, FADVISE_SEQUENTIAL);
            }
            FadviseReadahead::Sequential => {
                self.update_fadvise_bits(FADVISE_SEQUENTIAL, FADVISE_RANDOM);
            }
            FadviseReadahead::NoReuse => {
                self.readahead.fetch_or(FADVISE_NOREUSE, Ordering::AcqRel);
            }
        }
    }

    fn fadvise_has(&self, flag: u8) -> bool {
        self.readahead.load(Ordering::Acquire) & flag != 0
    }

    fn update_fadvise_bits(&self, set: u8, clear: u8) {
        let mut previous = self.readahead.load(Ordering::Acquire);
        loop {
            let next = fadvise_next_bits(previous, set, clear);
            match self.readahead.compare_exchange_weak(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => previous = observed,
            }
        }
    }

    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_willneed(offset, len)
    }

    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_noreuse(offset, len)
    }

    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .fadvise_dontneed(offset, len)
    }

    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.remaining_mut();
        let read = self.access(FileFlags::READ)?.read_at_with_readahead(
            dst,
            offset,
            !self.fadvise_has(FADVISE_RANDOM),
        )?;
        if read != 0 && self.fadvise_has(FADVISE_NOREUSE) {
            // Persist across future reads of this OFD, while marking only
            // pages actually consumed by this read.
            let _ = self.inner.fadvise_noreuse(offset, read as u64);
        }
        if read != 0
            && self.fadvise_has(FADVISE_SEQUENTIAL)
            && !self.fadvise_has(FADVISE_RANDOM)
        {
            let next = offset.saturating_add(read as u64);
            let _ = self
                .inner
                .fadvise_willneed(next, (READAHEAD_PAGES * 2 * PAGE_SIZE) as u64);
        }
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

    /// Attempts a positioned direct read into caller-pinned physical memory.
    /// `Ok(None)` permits the caller to use its ordinary fallback path.
    ///
    /// # Safety
    ///
    /// All segments must remain pinned, DMA-accessible, writable, and disjoint
    /// until the call returns. Concurrent CPU/device content races are the
    /// caller's responsibility and do not make physical addresses into Rust
    /// references.
    pub unsafe fn try_read_at_dma_segments_with_reason(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        if !self.supports_positioned_read() {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            ));
        }
        let result = unsafe {
            self.access(FileFlags::READ)?
                .try_read_at_dma_segments_with_reason(dst, offset)
        }?;
        #[cfg(feature = "times")]
        if matches!(result, PhysicalIoAttempt::Completed(bytes) if bytes != 0)
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(result)
    }

    pub unsafe fn try_read_at_dma_segments(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_read_at_dma_segments_with_reason(dst, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.len();
        let read = self.access(FileFlags::READ)?.read_at_slice(dst, offset)?;
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

    pub fn read_at_vectored_slice(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.iter().map(|buf| buf.len()).sum::<usize>();
        let read = self
            .access(FileFlags::READ)?
            .read_at_vectored(dst, offset)?;
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

    /// Reads at `offset` into caller-pinned physical segments.
    ///
    /// # Safety
    ///
    /// The caller must keep every destination pinned, mapped, writable, and
    /// accessible through the call. Ranges must be mutually disjoint, but may
    /// remain concurrently accessible to userspace because no Rust reference
    /// is created for them.
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = validate_pinned_physical_segments(dst, true)?;
        let read = unsafe {
            self.access(FileFlags::READ)?
                .read_at_pinned_segments(dst, offset, try_async)?
        };
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

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        let written = self.access(FileFlags::WRITE)?.write_at_slice(src, offset)?;
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    pub fn write_at_vectored_slice(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        let written = self
            .access(FileFlags::WRITE)?
            .write_at_vectored(src, offset)?;
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    /// Writes at `offset` from caller-pinned physical segments.
    ///
    /// # Safety
    ///
    /// The caller must keep every source pinned, mapped, readable, and
    /// accessible through the call. Concurrent userspace access is permitted;
    /// the implementation creates no Rust reference to the physical ranges.
    pub unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        let written = unsafe {
            self.access(FileFlags::WRITE)?
                .write_at_pinned_segments(src, offset, try_async)?
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    /// Attempts a positioned direct overwrite from caller-pinned physical
    /// memory. O_APPEND and nodes without positioned writes intentionally
    /// return `Ok(None)` without entering the backend.
    ///
    /// # Safety
    ///
    /// All segments must remain pinned, DMA-accessible, readable, and disjoint
    /// until the call returns. Concurrent CPU/device content races are the
    /// caller's responsibility and do not make physical addresses into Rust
    /// references.
    pub unsafe fn try_write_at_dma_segments_with_reason(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        if self.append_enabled() || !self.supports_positioned_write() {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::DeviceAdmission,
            ));
        }
        let result = unsafe {
            self.access(FileFlags::WRITE)?
                .try_write_at_dma_segments_with_reason(src, offset)
        }?;
        #[cfg(feature = "times")]
        if matches!(result, PhysicalIoAttempt::Completed(bytes) if bytes != 0) {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(result)
    }

    pub unsafe fn try_write_at_dma_segments(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_write_at_dma_segments_with_reason(src, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    fn write_at_end_with_admission_and_new_end(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        let result = self
            .access(FileFlags::WRITE)?
            .append_with_admission(src, admit)?;
        #[cfg(feature = "times")]
        if result.0 > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(result)
    }

    fn write_at_end_with_new_end(&self, src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.write_at_end_with_admission_and_new_end(src, |_offset, requested| Ok(requested))
    }

    fn write_vectored_at_end_with_new_end(&self, src: &[&[u8]]) -> VfsResult<(usize, Option<u64>)> {
        let backend = self.access(FileFlags::WRITE)?;
        if !src.iter().any(|buf| !buf.is_empty()) {
            return Ok((0, None));
        }
        let (total, new_end) = backend.append_vectored(src)?;
        #[cfg(feature = "times")]
        if total > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok((total, Some(new_end)))
    }

    /// Atomically appends data without reading or changing this [`File`]'s
    /// current position.
    ///
    /// This is the positioned counterpart of
    /// [`write_with_placement`](Self::write_with_placement) with
    /// [`WritePlacement::End`].
    pub fn write_at_end(&self, src: impl Read + IoBuf) -> VfsResult<usize> {
        self.write_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Atomically appends an admitted prefix without changing this [`File`]'s
    /// current position.
    ///
    /// The admission callback observes the exact inode end protected by the
    /// append serialization domain. It must be short and must not call another
    /// append or current-position operation on this file.
    pub fn write_at_end_with_admission(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<usize> {
        self.write_at_end_with_admission_and_new_end(src, admit)
            .map(|(written, _)| written)
    }

    /// Atomically appends a byte slice without changing this [`File`]'s
    /// current position.
    pub fn write_at_end_slice(&self, src: &[u8]) -> VfsResult<usize> {
        self.write_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Atomically appends a vectored input without changing this [`File`]'s
    /// current position.
    pub fn write_at_end_vectored_slice(&self, src: &[&[u8]]) -> VfsResult<usize> {
        self.write_vectored_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Attempts to sync OS-internal file content and metadata to disk.
    ///
    /// If `data_only` is `true`, only the file data is synced, not the
    /// metadata.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        self.inner.sync(data_only)
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        self.inner.sync_range(offset, len, data_only)
    }

    pub fn range_writeback_snapshot(&self) -> VfsResult<RangeWritebackFence> {
        Ok(self.access(FileFlags::empty())?.range_writeback_snapshot())
    }

    pub fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        self.access(FileFlags::empty())?
            .submit_range_writeback(offset, len, data_only)
    }

    pub fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .wait_range_writeback_through(fence, offset, len)
    }

    /// Returns a typed map of allocated extents for this open file.  Access is
    /// checked before an optional synchronous flush; the flush completes and
    /// releases all cached-file/filesystem locks before the inode query starts.
    pub fn map_extents(
        &self,
        start: u64,
        length: u64,
        max_extents: usize,
        sync: bool,
    ) -> VfsResult<FileExtentMap> {
        self.access(FileFlags::empty())?;
        if sync {
            self.sync(false)?;
        }
        self.location()
            .entry()
            .as_file()?
            .map_extents(start, length, max_extents)
    }

    /// Reads data from the current position, advancing the cursor.
    pub fn read(&self, dst: impl Write + IoBufMut) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at(dst, 0)
        }
    }

    pub fn read_slice(&self, dst: &mut [u8]) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_slice(dst, 0)
        }
    }

    /// Reads from the current position and advances it by exactly the prefix
    /// accepted by `consume`.
    ///
    /// The position lock remains held across the callback. This is intended
    /// for transfer operations such as sendfile which must not consume source
    /// bytes before the destination accepts them. If the callback fails, the
    /// position is unchanged; a short accepted prefix advances by only that
    /// prefix. Stream nodes without an open-file-description position are not
    /// representable by this transaction and return `InvalidInput`.
    pub fn read_slice_then(
        &self,
        dst: &mut [u8],
        consume: impl FnOnce(&[u8]) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        self.read_slice_at_current_then(dst, |data, _offset| consume(data))
    }

    /// Reads at one frozen current position and commits exactly the prefix
    /// accepted by `consume`.
    ///
    /// The current-position transaction remains owned across `consume`, but
    /// the small position lock is released first. The callback receives the
    /// frozen offset so a higher layer can implement a same-description
    /// transfer through positioned backend I/O without recursively acquiring
    /// this transaction.
    pub fn read_slice_at_current_then(
        &self,
        dst: &mut [u8],
        consume: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        self.read_slice_at_current_checked_then(dst, |_| Ok(()), consume)
    }

    /// Runs a short, nonblocking callback against one frozen current position.
    ///
    /// This is a generic admission primitive for higher layers which need the
    /// exact current offset without performing backend I/O or advancing it.
    pub fn with_current_position<T>(
        &self,
        inspect: impl FnOnce(u64) -> axio::Result<T>,
    ) -> axio::Result<T> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        inspect(offset)
    }

    /// Runs one complete operation against a frozen current position and
    /// commits its accepted prefix exactly once.
    ///
    /// `operation` receives the initial position and must use positioned I/O;
    /// recursively calling a current-position method on this file would try to
    /// acquire the same transaction again. `max_advance` is validated before
    /// the callback can mutate external state, while the returned advance is
    /// checked against that bound before the cursor is committed. An error
    /// leaves the cursor unchanged.
    pub fn with_current_position_transaction<T>(
        &self,
        max_advance: usize,
        operation: impl FnOnce(u64) -> axio::Result<(T, usize)>,
    ) -> axio::Result<T> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        let max_advance = u64::try_from(max_advance).map_err(|_| VfsError::InvalidInput)?;
        offset
            .checked_add(max_advance)
            .ok_or(VfsError::InvalidInput)?;

        let (value, advance) = operation(offset)?;
        let advance = u64::try_from(advance).map_err(|_| VfsError::InvalidInput)?;
        if advance > max_advance {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset.checked_add(advance).ok_or(VfsError::InvalidInput)?;
        Ok(value)
    }

    /// Reads at one frozen current position after a caller-supplied admission
    /// check, then commits exactly the prefix accepted by `consume`.
    ///
    /// `admit` runs while the current-position transaction is held but before
    /// backend read side effects. It must be short and nonblocking. This keeps
    /// higher-layer range policy outside axfs while giving that policy the same
    /// position snapshot used by the eventual read.
    pub fn read_slice_at_current_checked_then(
        &self,
        dst: &mut [u8],
        admit: impl FnOnce(u64) -> axio::Result<()>,
        consume: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let mut state = ();
        self.read_slice_at_current_checked_with(
            dst,
            &mut state,
            |_state, offset| admit(offset),
            |_state, data, offset| consume(data, offset),
        )
    }

    /// Stateful form of [`read_slice_at_current_checked_then`](Self::read_slice_at_current_checked_then).
    ///
    /// Both phases receive the same caller-owned state sequentially. This is
    /// useful when admission and consumption operate on one destination
    /// transaction which cannot be mutably captured by two closures at once.
    pub fn read_slice_at_current_checked_with<S>(
        &self,
        dst: &mut [u8],
        state: &mut S,
        admit: impl FnOnce(&mut S, u64) -> axio::Result<()>,
        consume: impl FnOnce(&mut S, &[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        admit(state, offset)?;
        let read = self.read_at_slice(dst, offset)?;
        // Reject an impossible position before the destination callback can
        // mutate externally visible state.
        offset
            .checked_add(read as u64)
            .ok_or(VfsError::InvalidInput)?;
        if read == 0 {
            return Ok(0);
        }
        let consumed = consume(state, &dst[..read], offset)?;
        if consumed > read {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset
            .checked_add(consumed as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(consumed)
    }

    /// Runs one positioned write callback at a frozen current position and
    /// advances the cursor by exactly the prefix it reports as committed.
    ///
    /// The callback owns the actual write so an embedding layer can perform
    /// policy admission and positioned backend I/O without recursively taking
    /// this transaction. It must not block while holding unrelated endpoint
    /// transactions.
    pub fn write_slice_at_current_then(
        &self,
        src: &[u8],
        write: impl FnOnce(&[u8], u64) -> axio::Result<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let offset = *pos.lock();
        offset
            .checked_add(src.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        if src.is_empty() {
            return Ok(0);
        }
        let written = write(src, offset)?;
        if written > src.len() {
            return Err(VfsError::InvalidInput);
        }
        *pos.lock() = offset
            .checked_add(written as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(written)
    }

    pub fn read_vectored_slice(&self, dst: &mut [&mut [u8]]) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_vectored_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_vectored_slice(dst, 0)
        }
    }

    /// Writes data using an explicit placement decision.
    ///
    /// `placement` is consumed as the decision for this operation and does
    /// not consult the mutable default append status. Nodes marked
    /// [`NodeFlags::POSITIONED_APPEND`] retain their special ordinary-write
    /// behavior: [`WritePlacement::End`] uses and advances their current
    /// position instead of invoking the inode append operation.
    pub fn write_with_placement_and_admission(
        &self,
        mut src: impl Read + IoBuf,
        placement: WritePlacement,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                let offset = *pos.lock();
                let requested = src.remaining();
                offset
                    .checked_add(requested as u64)
                    .ok_or(VfsError::InvalidInput)?;
                let allowed = admit(offset, requested)?;
                if allowed > requested {
                    return Err(VfsError::InvalidInput);
                }
                let mut admitted = (&mut src).take(allowed as u64);
                let written = self.write_at(&mut admitted, offset)?;
                if written > allowed {
                    return Err(VfsError::InvalidInput);
                }
                *pos.lock() = offset
                    .checked_add(written as u64)
                    .ok_or(VfsError::InvalidInput)?;
                Ok(written)
            } else {
                self.write_at_end_with_admission_and_new_end(src, admit)
                    .map(|(written, new_end)| {
                        *pos.lock() = new_end;
                        written
                    })
            }
        } else {
            let requested = src.remaining();
            let allowed = admit(0, requested)?;
            if allowed > requested {
                return Err(VfsError::InvalidInput);
            }
            let mut admitted = (&mut src).take(allowed as u64);
            self.write_at(&mut admitted, 0)
        }
    }

    /// Writes data using an explicit placement decision.
    pub fn write_with_placement(
        &self,
        src: impl Read + IoBuf,
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        self.write_with_placement_and_admission(src, placement, |_offset, requested| Ok(requested))
    }

    /// Writes a byte slice using an explicit placement decision.
    pub fn write_slice_with_placement(
        &self,
        src: &[u8],
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                self.write_at_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            } else {
                self.write_at_end_with_new_end(src)
                    .map(|(written, new_end)| {
                        *pos = new_end;
                        written
                    })
            }
        } else {
            self.write_at_slice(src, 0)
        }
    }

    /// Writes vectored input using an explicit placement decision.
    pub fn write_vectored_slice_with_placement(
        &self,
        src: &[&[u8]],
        placement: WritePlacement,
    ) -> axio::Result<usize> {
        let _transaction = self.position_transaction.lock();
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if placement == WritePlacement::Current
                || self
                    .location()
                    .flags()
                    .contains(NodeFlags::POSITIONED_APPEND)
            {
                self.write_at_vectored_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            } else {
                self.write_vectored_at_end_with_new_end(src)
                    .map(|(written, new_end)| {
                        if let Some(new_end) = new_end {
                            *pos = new_end;
                        }
                        written
                    })
            }
        } else {
            self.write_at_vectored_slice(src, 0)
        }
    }

    fn default_write_placement(&self) -> WritePlacement {
        if self.append_enabled() {
            WritePlacement::End
        } else {
            WritePlacement::Current
        }
    }

    /// Writes data using the file's mutable default append status.
    pub fn write(&self, src: impl Read + IoBuf) -> axio::Result<usize> {
        self.write_with_placement(src, self.default_write_placement())
    }

    /// Writes a byte slice using the file's mutable default append status.
    pub fn write_slice(&self, src: &[u8]) -> axio::Result<usize> {
        self.write_slice_with_placement(src, self.default_write_placement())
    }

    /// Writes vectored input using the file's mutable default append status.
    pub fn write_vectored_slice(&self, src: &[&[u8]]) -> axio::Result<usize> {
        self.write_vectored_slice_with_placement(src, self.default_write_placement())
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
        let _transaction = self.position_transaction.lock();

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

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.inner.location().register(context, events)
    }
}

#[cfg(feature = "times")]
impl Drop for File {
    fn drop(&mut self) {
        self.flush_times();
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        sync::{Arc, Weak},
        vec::Vec,
    };
    use core::{
        any::Any,
        num::NonZeroUsize,
        sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        task::Context,
        time::Duration,
    };
    use std::{
        sync::{Barrier, Mutex as StdMutex, Once as StdOnce, mpsc},
        thread,
    };

    use axfs_ng_vfs::{
        AsyncVectoredWriteOutcome, DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps,
        Location, Metadata, MetadataUpdate, Mountpoint, NodeFlags, NodeOps, NodePermission,
        NodeType, NodeUserData, Reference, StatFs, VfsError, VfsResult,
    };
    use axio::{Cursor, IoBuf, IoBufMut, Read, Seek, SeekFrom, Write};
    use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
    use axsync::Mutex;
    use lru::LruCache;

    use super::{
        ALIGNED_BYPASS_CHUNK, CLOSED_FILE_CACHE_RETAINED_PAGES, CachedFile,
        CachedFileEvictionOwner, CachedFileReclaimStats, CachedFileShared,
        CachedPageInvalidationTransaction, File, FileBackend, FileFlags, FileUserData,
        FADVISE_READAHEAD_QUEUE_CAPACITY, FadviseReadaheadQueue,
        FadviseReadaheadRequest, FADVISE_NOREUSE, FADVISE_RANDOM, FADVISE_SEQUENTIAL,
        MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS, OpenOptions, PAGE_SIZE, PageCache,
        PhysicalIoEffect, fadvise_next_bits,
        PhysicalIoResetProof, PhysicalIoSegment, PinnedPhysicalSegment, RANGE_CACHE_LEASE_SLOTS,
        RangeCacheLease, RangeCacheLeaseKind, WritePlacement, acknowledge_cached_page_eviction,
        advance_clean_cached_file_reclaim_scan_epoch, begin_dirty_writeback_run,
        cached_file_registry_key, cached_file_shared_for_location,
        cached_file_shared_for_location_or_create, discard_cached_pages, file_cache_registry,
        finish_dirty_writeback_run, mark_cached_file_unlinked, physical_to_virtual,
        reclaim_clean_pages_from_shared,
        reclaim_clean_pages_from_shared_with_scan_budget,
        release_unlinked_cached_file_registry_ownership, remove_cached_file_registry_entry,
        synchronize_retained_page_count, try_zeroed_pinned_io_bounce,
        stable_demote_lru_keys, try_collect_noreuse_keys,
        validate_physical_io_segments, validate_pinned_physical_segments,
        with_cache_invalidating_file_operation_after_preflight,
        with_sync_and_invalidate_cached_file_pages,
    };

    #[cfg(feature = "ext4")]
    #[test]
    fn prepared_physical_effect_is_worker_send() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<PhysicalIoEffect>();
    }

    static PRESSURE_RECLAIM_EPOCH_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn fadvise_readahead_ring_is_fixed_capacity_and_preserves_fifo_across_restart() {
        let mut queue = FadviseReadaheadQueue::new();
        for offset in 0..FADVISE_READAHEAD_QUEUE_CAPACITY {
            assert!(queue.push(FadviseReadaheadRequest {
                offset: offset as u64,
                len: PAGE_SIZE as u64,
            }));
        }
        assert!(!queue.push(FadviseReadaheadRequest {
            offset: u64::MAX,
            len: PAGE_SIZE as u64,
        }));
        assert!(queue.contains(FadviseReadaheadRequest {
            offset: 0,
            len: PAGE_SIZE as u64,
        }));

        // A failed spawn clears its published running state; pending work
        // stays in the fixed ring for a later caller to restart.
        queue.worker_running = true;
        queue.worker_running = false;
        for offset in 0..FADVISE_READAHEAD_QUEUE_CAPACITY {
            assert_eq!(
                queue.pop(),
                Some(FadviseReadaheadRequest {
                    offset: offset as u64,
                    len: PAGE_SIZE as u64,
                })
            );
        }
        assert_eq!(queue.pop(), None);

        // Exercise the wrapped tail too, without allocating a replacement
        // backing store.
        assert!(queue.push(FadviseReadaheadRequest { offset: 7, len: 1 }));
        assert_eq!(queue.pop(), Some(FadviseReadaheadRequest { offset: 7, len: 1 }));
    }

    #[test]
    fn fadvise_random_and_sequential_are_exclusive_while_noreuse_persists() {
        let sequential = fadvise_next_bits(FADVISE_NOREUSE, FADVISE_SEQUENTIAL, FADVISE_RANDOM);
        assert_eq!(sequential, FADVISE_SEQUENTIAL | FADVISE_NOREUSE);
        let random = fadvise_next_bits(sequential, FADVISE_RANDOM, FADVISE_SEQUENTIAL);
        assert_eq!(random, FADVISE_RANDOM | FADVISE_NOREUSE);
        assert_eq!(fadvise_next_bits(random, 0, 0), random);
    }

    #[test]
    fn direct_fadvise_without_cached_inode_is_a_nonallocating_noop() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        assert!(cached_file_shared_for_location(&location).is_none());

        let direct = FileBackend::new_direct(location.clone());
        direct.fadvise_willneed(0, PAGE_SIZE as u64).unwrap();
        direct.fadvise_noreuse(0, PAGE_SIZE as u64).unwrap();
        direct.fadvise_dontneed(0, PAGE_SIZE as u64).unwrap();

        assert!(cached_file_shared_for_location(&location).is_none());
    }

    #[test]
    fn noreuse_cold_splice_is_a_stable_lru_partition() {
        let mut cache = LruCache::new(NonZeroUsize::new(4).unwrap());
        for pn in [1, 2, 3, 4] {
            cache.put(pn, pn);
        }
        // LRU order before the splice is 1, 2, 3, 4.
        stable_demote_lru_keys(&mut cache, &[1, 3]);
        let lru_order: Vec<_> = cache.iter().rev().map(|(pn, _)| *pn).collect();
        assert_eq!(lru_order, [1, 3, 2, 4]);
    }

    #[test]
    fn noreuse_reprioritization_oom_is_best_effort() {
        let cache = LruCache::<u32, u32>::new(NonZeroUsize::new(1).unwrap());
        // This deterministically exercises Vec's fallible reservation rather
        // than relying on ambient allocator pressure.
        assert!(try_collect_noreuse_keys(&cache, 0, PAGE_SIZE as u64, usize::MAX).is_none());
    }

    #[test]
    fn range_cache_leases_enforce_overlap_modes_and_allow_disjoint_direct_io() {
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 1,
                inode: 2,
                object: 3,
            },
            false,
        ));
        let cached = CachedFileShared::try_range_cache_lease(
            &shared,
            0..PAGE_SIZE as u64,
            RangeCacheLeaseKind::CachedWrite,
        )
        .unwrap();
        assert!(matches!(
            CachedFileShared::try_range_cache_lease(
                &shared,
                PAGE_SIZE as u64 / 2..PAGE_SIZE as u64 * 2,
                RangeCacheLeaseKind::DirectWrite,
            ),
            Err(VfsError::ResourceBusy)
        ));
        let disjoint = CachedFileShared::try_range_cache_lease(
            &shared,
            PAGE_SIZE as u64 * 2..PAGE_SIZE as u64 * 3,
            RangeCacheLeaseKind::DirectRead,
        )
        .unwrap();
        assert!(cached.revalidate());
        assert!(disjoint.revalidate());
        drop(disjoint);
        drop(cached);
        assert!(
            CachedFileShared::try_range_cache_lease(
                &shared,
                0..PAGE_SIZE as u64,
                RangeCacheLeaseKind::DirectRead,
            )
            .is_ok()
        );
    }

    #[test]
    fn only_direct_range_lease_drop_requests_unlinked_cleanup() {
        assert!(super::range_lease_drop_requests_unlinked_cleanup(
            RangeCacheLeaseKind::DirectRead
        ));
        assert!(super::range_lease_drop_requests_unlinked_cleanup(
            RangeCacheLeaseKind::DirectWrite
        ));
        assert!(!super::range_lease_drop_requests_unlinked_cleanup(
            RangeCacheLeaseKind::CachedRead
        ));
        assert!(!super::range_lease_drop_requests_unlinked_cleanup(
            RangeCacheLeaseKind::CachedWrite
        ));
        assert!(!super::range_lease_drop_requests_unlinked_cleanup(
            RangeCacheLeaseKind::WholeFileMutation
        ));
    }

    #[test]
    fn range_cache_lease_capacity_is_bounded_and_stale_generation_cannot_clear_reuse() {
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 3,
                inode: 4,
                object: 5,
            },
            false,
        ));
        let mut leases = Vec::new();
        for index in 0..RANGE_CACHE_LEASE_SLOTS {
            leases.push(
                CachedFileShared::try_range_cache_lease(
                    &shared,
                    index as u64 * PAGE_SIZE as u64..(index as u64 + 1) * PAGE_SIZE as u64,
                    RangeCacheLeaseKind::DirectRead,
                )
                .unwrap(),
            );
        }
        assert!(matches!(
            CachedFileShared::try_range_cache_lease(
                &shared,
                128 * PAGE_SIZE as u64..129 * PAGE_SIZE as u64,
                RangeCacheLeaseKind::DirectRead,
            ),
            Err(VfsError::ResourceBusy)
        ));
        let stale = RangeCacheLease {
            shared: shared.clone(),
            slot: leases[0].slot,
            generation: leases[0].generation,
            record: leases[0].record,
        };
        drop(leases.remove(0));
        let replacement = CachedFileShared::try_range_cache_lease(
            &shared,
            0..PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectWrite,
        )
        .unwrap();
        drop(stale);
        assert!(replacement.revalidate());
    }

    #[cfg(feature = "ext4")]
    #[test]
    fn physical_reset_proof_rejects_unproven_quarantine() {
        use axdriver::prelude::BlockResetOutcome;

        assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Quiesced).is_some());
        assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Retired).is_some());
        assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Quarantined).is_none());
    }

    #[cfg(feature = "ext4")]
    #[test]
    fn prepared_physical_effect_rolls_back_cache_before_direct_lease_cleanup() {
        init_test_page_allocator();
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 9,
                inode: 10,
                object: 11,
            },
            false,
        ));
        let mut page = PageCache::new(false).unwrap();
        page.data().fill(0x5a);
        assert!(shared.page_cache.lock().put(0, page).is_none());
        shared.unlinked.store(true, Ordering::Release);

        let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
        assert_eq!(invalidation.stage_all(), Ok(()));
        let mut range_lease = Some(
            CachedFileShared::try_range_cache_lease(
                &shared,
                0..PAGE_SIZE as u64,
                RangeCacheLeaseKind::DirectWrite,
            )
            .unwrap(),
        );
        let mut invalidation = Some(invalidation);

        super::drop_prepared_physical_effect_owners(&mut invalidation, &mut range_lease);

        assert!(invalidation.is_none());
        assert!(range_lease.is_none());
        assert!(shared.page_cache.lock().is_empty());
    }

    #[test]
    fn range_cache_lease_slots_recycle_across_repeated_reset_cycles() {
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 6,
                inode: 7,
                object: 8,
            },
            false,
        ));
        for cycle in 0..(RANGE_CACHE_LEASE_SLOTS * 4) {
            let start = (cycle % RANGE_CACHE_LEASE_SLOTS) as u64 * PAGE_SIZE as u64;
            let lease = CachedFileShared::try_range_cache_lease(
                &shared,
                start..start + PAGE_SIZE as u64,
                RangeCacheLeaseKind::DirectRead,
            )
            .unwrap();
            assert!(lease.revalidate());
            drop(lease);
        }
        assert!(
            shared
                .range_cache_leases
                .lock()
                .slots
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn physical_sg_validation_requires_nonempty_aligned_disjoint_ranges() {
        let valid = [
            PhysicalIoSegment::new(512, 512),
            PhysicalIoSegment::new(4096, 1024),
        ];
        assert_eq!(validate_physical_io_segments(&valid, 0), Ok(1536));
        assert_eq!(
            validate_physical_io_segments(&[], 0),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_physical_io_segments(&[PhysicalIoSegment::new(512, 0)], 0),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_physical_io_segments(
                &[
                    PhysicalIoSegment::new(512, 1024),
                    PhysicalIoSegment::new(1024, 512),
                ],
                0,
            ),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_physical_io_segments(&[PhysicalIoSegment::new(512, 512)], 1),
            Err(VfsError::InvalidInput)
        );
    }

    #[test]
    fn default_physical_hook_returns_fallback_without_file_io() {
        let (file, state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE,
            4096,
            FileFlags::READ | FileFlags::WRITE,
        );
        let segments = [PhysicalIoSegment::new(512, 512)];
        assert_eq!(
            unsafe { file.try_read_at_dma_segments(&segments, 0) },
            Ok(None)
        );
        assert_eq!(
            unsafe { file.try_write_at_dma_segments(&segments, 0) },
            Ok(None)
        );
        assert_eq!(state.read_calls.load(Ordering::Acquire), 0);
        assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn physical_preflight_rejects_before_cache_mutation_or_operation() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x5a, true);
        let preflight_calls = AtomicUsize::new(0);
        let operation_calls = AtomicUsize::new(0);
        let result = with_cache_invalidating_file_operation_after_preflight(
            &location,
            |_, _| {
                preflight_calls.fetch_add(1, Ordering::AcqRel);
                Ok(false)
            },
            |_, _| {
                operation_calls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(None)));
        assert_eq!(preflight_calls.load(Ordering::Acquire), 1);
        assert_eq!(operation_calls.load(Ordering::Acquire), 0);
        assert_eq!(state.read_calls.load(Ordering::Acquire), 0);
        assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
        cached.with_page(0, |page| {
            let page = page.expect("preflight rejection discarded a cached page");
            assert!(page.is_dirty());
        });
    }

    #[test]
    fn no_data_open_options_emit_no_data_access_flags() {
        let mut options = OpenOptions::new();
        options.no_data(true).direct(true).no_atime(true);
        let flags = options.to_flags().unwrap();
        assert!(!flags.intersects(FileFlags::READ | FileFlags::WRITE | FileFlags::PATH));
        assert!(flags.contains(FileFlags::DIRECT | FileFlags::NOATIME));

        options.read(true);
        assert!(!options.is_valid());
    }

    struct AppendTestState {
        read_offsets: Mutex<Vec<u64>>,
        write_offsets: Mutex<Vec<u64>>,
        read_calls: AtomicUsize,
        async_read_calls: AtomicUsize,
        async_write_mode: AtomicU8,
        write_calls: AtomicUsize,
        append_calls: AtomicUsize,
        open_calls: AtomicUsize,
        set_len_calls: AtomicUsize,
        last_read_buf: AtomicUsize,
        last_write_buf: AtomicUsize,
        inode_len: AtomicU64,
        append_limit: AtomicUsize,
        fail_read_call: AtomicUsize,
        fail_write_call: AtomicUsize,
        fail_append_call: AtomicUsize,
        fail_set_len_call: AtomicUsize,
        yield_after_append: AtomicBool,
        fail_set_len: AtomicBool,
        set_len_failure_atomic: AtomicBool,
        full_page_io: AtomicBool,
        stored_first_byte: AtomicU8,
        append_markers: StdMutex<Vec<u8>>,
        user_data: NodeUserData,
    }

    impl AppendTestState {
        fn new(inode_len: u64) -> Arc<Self> {
            Arc::new(Self {
                read_offsets: Mutex::new(Vec::new()),
                write_offsets: Mutex::new(Vec::new()),
                read_calls: AtomicUsize::new(0),
                async_read_calls: AtomicUsize::new(0),
                async_write_mode: AtomicU8::new(0),
                write_calls: AtomicUsize::new(0),
                append_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
                set_len_calls: AtomicUsize::new(0),
                last_read_buf: AtomicUsize::new(0),
                last_write_buf: AtomicUsize::new(0),
                inode_len: AtomicU64::new(inode_len),
                append_limit: AtomicUsize::new(usize::MAX),
                fail_read_call: AtomicUsize::new(usize::MAX),
                fail_write_call: AtomicUsize::new(usize::MAX),
                fail_append_call: AtomicUsize::new(usize::MAX),
                fail_set_len_call: AtomicUsize::new(usize::MAX),
                yield_after_append: AtomicBool::new(false),
                fail_set_len: AtomicBool::new(false),
                set_len_failure_atomic: AtomicBool::new(true),
                full_page_io: AtomicBool::new(false),
                stored_first_byte: AtomicU8::new(0),
                append_markers: StdMutex::new(Vec::new()),
                user_data: NodeUserData::new(),
            })
        }
    }

    struct AppendTestFile {
        flags: NodeFlags,
        state: Arc<AppendTestState>,
        fs: Arc<RegistryTestFs>,
    }

    impl NodeOps for AppendTestFile {
        fn inode(&self) -> u64 {
            1
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 1,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o600),
                node_type: NodeType::RegularFile,
                uid: 0,
                gid: 0,
                size: self.state.inode_len.load(Ordering::Acquire),
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: axfs_ng_vfs::Timestamp::ZERO,
                btime: axfs_ng_vfs::Timestamp::ZERO,
                mtime: axfs_ng_vfs::Timestamp::ZERO,
                ctime: axfs_ng_vfs::Timestamp::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            &*self.fs
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn open(&self, _read: bool, _write: bool) -> VfsResult<()> {
            self.state.open_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }

        fn flags(&self) -> NodeFlags {
            self.flags
        }

        fn persistent_user_data(&self) -> Option<&NodeUserData> {
            Some(&self.state.user_data)
        }
    }

    impl Pollable for AppendTestFile {
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

    impl FileNodeOps for AppendTestFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            self.state
                .last_read_buf
                .store(buf.as_ptr() as usize, Ordering::Release);
            self.state.read_offsets.lock().push(offset);
            let call = self.state.read_calls.fetch_add(1, Ordering::AcqRel);
            if call == self.state.fail_read_call.load(Ordering::Acquire) {
                return Err(VfsError::InvalidInput);
            }
            if self.state.full_page_io.load(Ordering::Acquire) {
                let len = self
                    .state
                    .inode_len
                    .load(Ordering::Acquire)
                    .saturating_sub(offset)
                    .min(buf.len() as u64) as usize;
                buf[..len].fill(0);
                if offset == 0 && len != 0 {
                    buf[0] = self.state.stored_first_byte.load(Ordering::Acquire);
                }
                return Ok(len);
            }
            let data = b"abcdefgh";
            let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidInput)?;
            if offset >= data.len() {
                return Ok(0);
            }
            let read = buf.len().min(data.len() - offset);
            buf[..read].copy_from_slice(&data[offset..offset + read]);
            Ok(read)
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
            self.state
                .last_write_buf
                .store(buf.as_ptr() as usize, Ordering::Release);
            self.state.write_offsets.lock().push(offset);
            let call = self.state.write_calls.fetch_add(1, Ordering::AcqRel);
            if call == self.state.fail_write_call.load(Ordering::Acquire) {
                return Err(VfsError::InvalidInput);
            }
            if self.state.full_page_io.load(Ordering::Acquire) {
                if offset == 0
                    && let Some(first) = buf.first()
                {
                    self.state
                        .stored_first_byte
                        .store(*first, Ordering::Release);
                }
                return Ok(buf.len());
            }
            if offset != 0 {
                return Err(VfsError::InvalidInput);
            }
            Ok(buf.len().min(2))
        }

        fn try_read_at_vectored_async(
            &self,
            _bufs: &mut [&mut [u8]],
            _offset: u64,
        ) -> VfsResult<Option<usize>> {
            self.state.async_read_calls.fetch_add(1, Ordering::AcqRel);
            Ok(None)
        }

        fn try_write_at_vectored_async(
            &self,
            _bufs: &[&[u8]],
            _offset: u64,
        ) -> VfsResult<AsyncVectoredWriteOutcome> {
            match self.state.async_write_mode.load(Ordering::Acquire) {
                1 => Err(VfsError::Io),
                2 => Ok(AsyncVectoredWriteOutcome::CompletionError(VfsError::Io)),
                3 => Ok(AsyncVectoredWriteOutcome::Completed(PAGE_SIZE)),
                _ => Ok(AsyncVectoredWriteOutcome::NotSubmitted),
            }
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            let call = self.state.append_calls.fetch_add(1, Ordering::AcqRel);
            if call == self.state.fail_append_call.load(Ordering::Acquire) {
                return Err(VfsError::InvalidInput);
            }
            if let Some(marker) = buf.first().copied() {
                self.state.append_markers.lock().unwrap().push(marker);
            }
            let written = buf
                .len()
                .min(self.state.append_limit.load(Ordering::Acquire));
            let old_len = self
                .state
                .inode_len
                .fetch_add(written as u64, Ordering::AcqRel);
            if self.state.yield_after_append.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok((written, old_len + written as u64))
        }

        fn set_len(&self, len: u64) -> VfsResult<()> {
            let call = self.state.set_len_calls.fetch_add(1, Ordering::AcqRel);
            if self.state.fail_set_len.load(Ordering::Acquire)
                || call == self.state.fail_set_len_call.load(Ordering::Acquire)
            {
                if !self.state.set_len_failure_atomic.load(Ordering::Acquire) {
                    self.state.inode_len.store(len, Ordering::Release);
                }
                return Err(VfsError::InvalidInput);
            }
            self.state.inode_len.store(len, Ordering::Release);
            Ok(())
        }

        fn set_len_failure_is_atomic(&self) -> bool {
            self.state.set_len_failure_atomic.load(Ordering::Acquire)
        }

        fn set_symlink(&self, _target: &str) -> VfsResult<()> {
            Err(VfsError::InvalidInput)
        }
    }

    fn append_test_file_with_access(
        flags: NodeFlags,
        inode_len: u64,
        access: FileFlags,
    ) -> (File, Arc<AppendTestState>) {
        let state = AppendTestState::new(inode_len);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(flags, state.clone()));
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let file = File::new(FileBackend::Direct(location), access);
        (file, state)
    }

    fn append_test_file(flags: NodeFlags, inode_len: u64) -> (File, Arc<AppendTestState>) {
        append_test_file_with_access(flags, inode_len, FileFlags::WRITE | FileFlags::APPEND)
    }

    fn init_test_page_allocator() {
        static PAGE_ALLOCATOR_INIT: StdOnce = StdOnce::new();
        PAGE_ALLOCATOR_INIT.call_once(|| {
            const TEST_PAGE_MEMORY: usize = 16 * 1024 * 1024;
            const PAGE_ALLOCATOR_BASE_ALIGN: usize = 1024 * 1024 * 1024;
            let layout =
                std::alloc::Layout::from_size_align(TEST_PAGE_MEMORY, PAGE_ALLOCATOR_BASE_ALIGN)
                    .unwrap();
            let memory = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!memory.is_null());
            axalloc::global_init(memory as usize, TEST_PAGE_MEMORY);
        });
    }

    fn cached_append_test_file(inode_len: u64) -> (CachedFile, Location, Arc<AppendTestState>) {
        init_test_page_allocator();

        let state = AppendTestState::new(inode_len);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::empty(),
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let cached = CachedFile::get_or_create(location.clone());
        (cached, location, state)
    }

    fn seed_cached_page(
        cached: &CachedFile,
        pn: u32,
        byte: u8,
        dirty: bool,
    ) -> axhal::mem::PhysAddr {
        let mut paddr = None;
        cached
            .with_page_or_insert(pn, |page, evicted| {
                assert!(evicted.is_none());
                page.data().fill(byte);
                if dirty {
                    page.mark_dirty();
                }
                paddr = Some(page.paddr());
                Ok(())
            })
            .unwrap();
        paddr.unwrap()
    }

    #[test]
    fn cold_pages_demotes_only_resident_cache_entries() {
        let (cached, _location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x11, false);
        seed_cached_page(&cached, 1, 0x22, false);
        cached.with_page(0, |_| {});
        assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &1);

        assert_eq!(cached.cold_pages(0..1).unwrap(), 1);
        assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &0);
        assert_eq!(cached.cold_pages(2..3).unwrap(), 0);
    }

    #[test]
    fn pageout_writes_back_then_evicts_each_resident_page() {
        let (cached, _location, state) = cached_append_test_file(2 * PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x31, true);
        seed_cached_page(&cached, 1, 0x32, false);
        let notifications = Arc::new(AtomicUsize::new(0));
        let notified = notifications.clone();
        let handle = cached.add_evict_listener(CachedFileEvictionOwner::new(7).unwrap(), move |_, _| {
            notified.fetch_add(1, Ordering::AcqRel);
            true
        });

        assert_eq!(cached.pageout_pages(0..3).unwrap(), 2);
        assert_eq!(state.write_calls.load(Ordering::Acquire), 1);
        assert_eq!(notifications.load(Ordering::Acquire), 2);
        assert!(!cached.shared.page_cache.lock().contains(&0));
        assert!(!cached.shared.page_cache.lock().contains(&1));

        unsafe { cached.remove_evict_listener(handle) };
    }

    #[test]
    fn async_dirty_writeback_completion_error_is_published_on_the_backend_inode() {
        let (_cached, location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let writeback_errors = location.writeback_error_state().unwrap();
        let mut cursor = writeback_errors.sample();
        let file = location.entry().as_file().unwrap();

        super::publish_async_dirty_writeback_completion_error(file, VfsError::Io);

        assert_eq!(
            writeback_errors.check_and_advance(&mut cursor),
            Some(VfsError::Io)
        );
    }

    #[test]
    fn async_dirty_writeback_presubmit_error_is_not_published() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x5a, true);
        let writeback_errors = location.writeback_error_state().unwrap();
        let mut cursor = writeback_errors.sample();
        let file = location.entry().as_file().unwrap();

        state.async_write_mode.store(1, Ordering::Release);
        axdriver::set_virtio_async_block_enabled(true);
        super::set_async_dirty_flush_sg_enabled(true);
        let result = cached.flush_dirty_cache(file);
        super::set_async_dirty_flush_sg_enabled(false);
        axdriver::set_virtio_async_block_enabled(false);

        assert_eq!(result, Err(VfsError::Io));
        assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
    }

    #[test]
    fn range_sg_completion_error_arrives_with_its_errseq_already_published() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x5a, true);
        let writeback_errors = location.writeback_error_state().unwrap();
        let mut cursor = writeback_errors.sample();

        state.async_write_mode.store(2, Ordering::Release);
        axdriver::set_virtio_async_block_enabled(true);
        super::set_async_dirty_flush_sg_enabled(true);
        let result = cached.sync_range_marked(0, PAGE_SIZE as u64, true);
        super::set_async_dirty_flush_sg_enabled(false);
        axdriver::set_virtio_async_block_enabled(false);

        assert!(matches!(
            &result,
            Err(super::RangeSyncError::Writeback(
                super::DirtyWritebackError {
                    error: VfsError::Io,
                    errseq_published: true,
                    worker_must_publish: false,
                }
            ))
        ));
        assert_eq!(
            writeback_errors.check_and_advance(&mut cursor),
            Some(VfsError::Io)
        );
        assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
    }

    #[test]
    fn range_fallback_write_error_is_deferred_to_its_worker_completion() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x5a, true);
        let writeback_errors = location.writeback_error_state().unwrap();
        let mut cursor = writeback_errors.sample();

        // No SG submission is accepted; the ordinary vectored write is a
        // real range-worker completion and must be published by that worker.
        state.async_write_mode.store(0, Ordering::Release);
        axdriver::set_virtio_async_block_enabled(true);
        super::set_async_dirty_flush_sg_enabled(true);
        let result = cached.sync_range_marked(0, PAGE_SIZE as u64, true);
        super::set_async_dirty_flush_sg_enabled(false);
        axdriver::set_virtio_async_block_enabled(false);

        assert!(matches!(
            &result,
            Err(super::RangeSyncError::Writeback(
                super::DirtyWritebackError {
                    error: VfsError::Io,
                    errseq_published: false,
                    worker_must_publish: true,
                }
            ))
        ));
        assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);

        assert_eq!(cached.complete_range_writeback(result), Err(VfsError::Io));
        assert_eq!(
            writeback_errors.check_and_advance(&mut cursor),
            Some(VfsError::Io)
        );
        assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
    }

    #[test]
    fn pressure_reclaim_only_drops_clean_unpinned_pages() {
        let (cached, _location, _state) = cached_append_test_file(3 * PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x10, false);
        seed_cached_page(&cached, 1, 0x20, true);
        let pinned_paddr = seed_cached_page(&cached, 2, 0x30, false);
        let pin = cached
            .pin_cached_page_by_paddr(2, pinned_paddr, false)
            .unwrap();

        let mut stats = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 16, &mut stats),
            1
        );
        assert!(stats.dirty_pages >= 1);
        assert!(stats.pinned_pages >= 1);
        cached.with_page(0, |page| assert!(page.is_none()));
        cached.with_page(1, |page| assert!(page.is_some()));
        cached.with_page(2, |page| assert!(page.is_some()));
        drop(pin);
    }

    #[test]
    fn pressure_reclaim_records_a_shadow_and_refault_consumes_it() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x33, false);

        let mut stats = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut stats),
            1
        );
        assert_eq!(cached.cachestat(0, 0).nr_evicted, 1);
        assert_eq!(cached.cachestat(0, 0).nr_recently_evicted, 0);

        seed_cached_page(&cached, 0, 0x34, false);
        assert_eq!(cached.cachestat(0, 0).nr_evicted, 0);
    }

    #[test]
    fn nonresident_age_is_shared_across_inode_generations() {
        let (first, _first_location, _first_state) = cached_append_test_file(PAGE_SIZE as u64);
        let (second, _second_location, _second_state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&first, 0, 0x35, false);
        seed_cached_page(&second, 0, 0x36, false);

        let mut first_stats = CachedFileReclaimStats::default();
        let mut second_stats = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&first.shared, 1, &mut first_stats),
            1
        );
        let first_age = super::current_file_cache_nonresident_age();
        assert_eq!(
            reclaim_clean_pages_from_shared(&second.shared, 1, &mut second_stats),
            1
        );
        assert!(super::current_file_cache_nonresident_age() > first_age);
        assert_eq!(first.cachestat(0, 0).nr_recently_evicted, 1);
    }

    #[test]
    fn cachestat_zero_resident_window_only_marks_same_age_shadow_recent() {
        assert!(super::file_cache_shadow_is_recent(7, 7, 0));
        assert!(!super::file_cache_shadow_is_recent(8, 7, 0));
    }

    #[test]
    fn cachestat_active_window_excludes_single_touch_cache_pages() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        cached
            .with_page_or_insert(0, |_, evicted| {
                assert!(evicted.is_none());
                Ok(())
            })
            .unwrap();
        {
            let mut cache = cached.shared.page_cache.lock();
            let page = cache.get(&0).unwrap();
            assert!(page.is_referenced());
            assert!(!page.is_active());
        }

        cached.with_page(0, |page| assert!(page.is_some()));
        assert!(cached.shared.page_cache.lock().get(&0).unwrap().is_active());

        // A just-faulted page has no active-window allowance; the second
        // reference promotes it and allows a one-generation refault.
        assert!(!super::file_cache_shadow_is_recent(2, 1, 0));
        assert!(super::file_cache_shadow_is_recent(2, 1, 1));
    }

    #[test]
    fn cachestat_first_write_marks_referenced_without_promotion() {
        init_test_page_allocator();
        let mut page = PageCache::new(false).unwrap();
        page.mark_dirty();
        assert!(page.is_referenced());
        assert!(!page.is_active());

        assert!(page.record_reference());
        page.demote_active();
        assert!(!page.record_reference());
        assert!(page.record_reference());
    }

    #[test]
    fn final_shared_drop_releases_nonempty_resident_cache() {
        init_test_page_allocator();
        let before = super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 91,
                inode: 92,
                object: 93,
            },
            false,
        ));
        let page = PageCache::new(false).unwrap();
        assert!(shared.page_cache.lock().put(0, page).is_none());
        super::file_cache_resident_add(1);

        drop(shared);
        assert_eq!(
            super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
            before
        );
    }

    #[test]
    fn invalidation_rollback_restores_resident_accounting() {
        init_test_page_allocator();
        let before = super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
        let shared = Arc::new(CachedFileShared::new(
            super::CachedFileIdentity {
                device: 94,
                inode: 95,
                object: 96,
            },
            false,
        ));
        let page = PageCache::new(false).unwrap();
        assert!(shared.page_cache.lock().put(0, page).is_none());
        super::file_cache_resident_add(1);

        let mut transaction = CachedPageInvalidationTransaction::new_shared(shared.clone());
        transaction.stage_all().unwrap();
        assert_eq!(
            super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
            before
        );
        drop(transaction);
        assert_eq!(shared.page_cache.lock().len(), 1);
        assert_eq!(
            super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
            before + 1
        );

        drop(shared);
        assert_eq!(
            super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
            before
        );
    }

    #[test]
    fn managed_page_baseline_is_stable_during_concurrent_page_allocation() {
        init_test_page_allocator();
        let budget = super::file_cache_shadow_budget();
        let managed = super::FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire);
        let workers = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..32 {
                        drop(PageCache::new(false).unwrap());
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(
            super::FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire),
            managed
        );
        assert_eq!(super::file_cache_shadow_budget(), budget);
    }

    #[test]
    fn fallback_writeback_has_the_same_begin_end_page_state_as_sg() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x37, true);
        let run = super::DirtyWritebackRun {
            page_start: 0,
            bytes: PAGE_SIZE,
            pages: vec![super::DirtyWritebackPage {
                pn: 0,
                data: vec![0x37; PAGE_SIZE],
            }],
        };

        begin_dirty_writeback_run(&cached.shared, &run).unwrap();
        assert!(
            cached
                .shared
                .page_cache
                .lock()
                .get(&0)
                .unwrap()
                .is_writeback()
        );
        finish_dirty_writeback_run(&cached.shared, &run, false);
        cached.with_page(0, |page| {
            let page = page.unwrap();
            assert!(page.is_dirty());
            assert!(!page.is_writeback());
        });

        begin_dirty_writeback_run(&cached.shared, &run).unwrap();
        finish_dirty_writeback_run(&cached.shared, &run, true);
        cached.with_page(0, |page| {
            let page = page.unwrap();
            assert!(!page.is_dirty());
            assert!(!page.is_writeback());
        });
    }

    #[test]
    fn pressure_reclaim_skips_mapping_listeners() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x41, false);
        let owner = CachedFileEvictionOwner::new(77).unwrap();
        let handle = cached.add_evict_listener(owner, |_pn, _page| false);

        let mut stats = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut stats),
            0
        );
        assert_eq!(stats.mapped_files, 1);
        cached.with_page(0, |page| assert!(page.is_some()));
        unsafe { cached.remove_evict_listener(handle) };
    }

    #[test]
    fn pressure_reclaim_continues_a_bounded_inode_scan_across_passes() {
        let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
        const TEST_SCAN_BUDGET: usize = 2;
        let pages = TEST_SCAN_BUDGET + 1;
        let (cached, _location, _state) = cached_append_test_file((pages * PAGE_SIZE) as u64);
        for page in 0..TEST_SCAN_BUDGET {
            seed_cached_page(&cached, page as u32, 0x61, true);
        }
        seed_cached_page(&cached, TEST_SCAN_BUDGET as u32, 0x62, false);

        let mut first = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared_with_scan_budget(
                &cached.shared,
                1,
                TEST_SCAN_BUDGET,
                &mut first,
            ),
            0
        );
        assert_eq!(first.scanned_pages, TEST_SCAN_BUDGET);
        assert_eq!(first.scan_budget_exhausted_files, 1);
        assert_eq!(
            cached
                .shared
                .pressure_reclaim_scan_remaining
                .load(Ordering::Acquire),
            1
        );

        let mut second = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared_with_scan_budget(
                &cached.shared,
                1,
                TEST_SCAN_BUDGET,
                &mut second,
            ),
            1
        );
        assert_eq!(second.scanned_pages, 1);
        assert_eq!(second.scan_budget_exhausted_files, 0);
        assert_eq!(
            cached
                .shared
                .pressure_reclaim_scan_remaining
                .load(Ordering::Acquire),
            0
        );
        cached.with_page(TEST_SCAN_BUDGET as u32, |page| assert!(page.is_none()));
    }

    #[test]
    fn pressure_reclaim_does_not_restart_a_completed_inode_scan_epoch() {
        let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x63, true);

        let mut first = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut first),
            0
        );
        assert_eq!(first.scanned_pages, 1);
        assert_eq!(first.scan_budget_exhausted_files, 0);

        let mut repeated = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut repeated),
            0
        );
        assert_eq!(repeated.scanned_pages, 0);
        assert_eq!(repeated.scan_budget_exhausted_files, 0);
    }

    #[test]
    fn pressure_reclaim_new_epoch_reenables_a_completed_inode_scan() {
        let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x64, true);

        let mut completed = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut completed),
            0
        );
        assert_eq!(completed.scanned_pages, 1);

        advance_clean_cached_file_reclaim_scan_epoch();
        let mut next_epoch = CachedFileReclaimStats::default();
        assert_eq!(
            reclaim_clean_pages_from_shared(&cached.shared, 1, &mut next_epoch),
            0
        );
        assert_eq!(next_epoch.scanned_pages, 1);
    }

    #[test]
    fn pressure_reconcile_does_not_apply_an_old_delta_to_new_retention() {
        let (cached, location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x51, false);
        seed_cached_page(&cached, 1, 0x52, true);

        // Model the critical old interleaving directly: reclaim has already
        // removed one clean page, then a last-close transaction publishes a
        // retention count based on the one dirty page that remains.  The
        // reconciliation must preserve that actual count rather than subtract
        // the earlier reclaim delta from it.
        let clean = cached.shared.page_cache.lock().pop(&0).unwrap();
        drop(clean);
        let key = cached_file_registry_key(&location);
        {
            let mut registry = file_cache_registry().lock();
            let entry = registry
                .entry(key)
                .or_insert_with(|| FileUserData::new(&location, &cached.shared));
            let retired = entry.retain_closed(&location, &cached.shared, 1);
            drop(retired);
        }

        synchronize_retained_page_count(&cached.shared, 1);
        {
            let registry = file_cache_registry().lock();
            let entry = registry.get(&key).unwrap();
            assert_eq!(entry.retained_pages, 1);
            assert!(
                entry
                    .retained
                    .as_ref()
                    .is_some_and(|retained| Arc::ptr_eq(retained, &cached.shared))
            );
        }
        cached.with_page(1, |page| assert!(page.is_some_and(|page| page.is_dirty())));

        let retired = file_cache_registry().lock().remove(&key);
        drop(retired);
        cached.shared.unlinked.store(true, Ordering::Release);
    }

    struct RawPhysicalReader {
        paddr: usize,
        remaining: usize,
    }

    impl RawPhysicalReader {
        fn new(paddr: usize, len: usize) -> Self {
            Self {
                paddr,
                remaining: len,
            }
        }
    }

    impl Read for RawPhysicalReader {
        fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
            let len = self.remaining.min(buf.len());
            let src = physical_to_virtual(axhal::mem::PhysAddr::from(self.paddr)).as_ptr();
            unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), len) };
            self.paddr += len;
            self.remaining -= len;
            Ok(len)
        }
    }

    impl IoBuf for RawPhysicalReader {
        fn remaining(&self) -> usize {
            self.remaining
        }
    }

    struct RawPhysicalWriter {
        paddr: usize,
        remaining: usize,
    }

    impl RawPhysicalWriter {
        fn new(paddr: usize, len: usize) -> Self {
            Self {
                paddr,
                remaining: len,
            }
        }
    }

    impl Write for RawPhysicalWriter {
        fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
            let len = self.remaining.min(buf.len());
            let dst = physical_to_virtual(axhal::mem::PhysAddr::from(self.paddr)).as_mut_ptr();
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, len) };
            self.paddr += len;
            self.remaining -= len;
            Ok(len)
        }

        fn flush(&mut self) -> axio::Result<()> {
            Ok(())
        }
    }

    impl IoBufMut for RawPhysicalWriter {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    struct FaultingReader {
        bytes: Vec<u8>,
        position: usize,
        calls: usize,
        fail_call: usize,
    }

    impl FaultingReader {
        fn new(len: usize, fail_call: usize) -> Self {
            Self {
                bytes: vec![0x5a; len],
                position: 0,
                calls: 0,
                fail_call,
            }
        }
    }

    impl Read for FaultingReader {
        fn read(&mut self, dst: &mut [u8]) -> axio::Result<usize> {
            let call = self.calls;
            self.calls += 1;
            if call == self.fail_call {
                return Err(axio::Error::BadAddress);
            }
            let len = dst.len().min(self.remaining());
            dst[..len].copy_from_slice(&self.bytes[self.position..self.position + len]);
            self.position += len;
            Ok(len)
        }
    }

    impl IoBuf for FaultingReader {
        fn remaining(&self) -> usize {
            self.bytes.len() - self.position
        }
    }

    #[test]
    fn pinned_same_cache_page_read_and_scatter_read_use_overlap_bounce() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
        let expected = (0..96).map(|value| value as u8).collect::<Vec<_>>();
        cached.with_page(0, |page| {
            page.unwrap().data()[..expected.len()].copy_from_slice(&expected);
        });

        let scalar = [PinnedPhysicalSegment::new(paddr + 512, 32)];
        assert_eq!(
            unsafe { cached.read_at_pinned_segments(&scalar, 0, false) },
            Ok(32)
        );
        let scatter = [
            PinnedPhysicalSegment::new(paddr + 1024, 32),
            PinnedPhysicalSegment::new(paddr + 1536, 32),
        ];
        assert_eq!(
            unsafe { cached.read_at_pinned_segments(&scatter, 32, false) },
            Ok(64)
        );

        cached.with_page(0, |page| {
            let data = page.unwrap().data();
            assert_eq!(&data[512..544], &expected[..32]);
            assert_eq!(&data[1024..1056], &expected[32..64]);
            assert_eq!(&data[1536..1568], &expected[64..96]);
        });
    }

    #[test]
    fn mutable_pinned_segment_validation_is_fixed_and_bounded() {
        let segments: [PinnedPhysicalSegment; MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS] =
            core::array::from_fn(|index| {
                PinnedPhysicalSegment::new(0x1000 + index * 0x1000, 0x1000)
            });
        assert_eq!(
            validate_pinned_physical_segments(&segments, true),
            Ok(MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS * 0x1000)
        );

        let overflow: [PinnedPhysicalSegment; MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS + 1] =
            core::array::from_fn(|index| {
                PinnedPhysicalSegment::new(0x1000 + index * 0x1000, 0x1000)
            });
        assert_eq!(
            validate_pinned_physical_segments(&overflow, true),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_pinned_physical_segments(
                &[
                    PinnedPhysicalSegment::new(0x1000, 0x1000),
                    PinnedPhysicalSegment::new(0x1800, 0x1000),
                ],
                true,
            ),
            Err(VfsError::InvalidInput)
        );

        let bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE).unwrap();
        assert_eq!(bounce.len(), PAGE_SIZE);
        assert!(bounce.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn pinned_same_cache_page_write_and_scatter_write_use_overlap_bounce() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
        cached.with_page(0, |page| {
            let data = page.unwrap().data();
            data[512..544].fill(0x51);
            data[1024..1040].fill(0x61);
            data[1536..1552].fill(0x71);
        });

        let scalar = [PinnedPhysicalSegment::new(paddr + 512, 32)];
        assert_eq!(
            unsafe { cached.write_at_pinned_segments(&scalar, 0, false) },
            Ok(32)
        );
        let scatter = [
            PinnedPhysicalSegment::new(paddr + 1024, 16),
            PinnedPhysicalSegment::new(paddr + 1536, 16),
        ];
        assert_eq!(
            unsafe { cached.write_at_pinned_segments(&scatter, 128, false) },
            Ok(32)
        );

        cached.with_page(0, |page| {
            let data = page.unwrap().data();
            assert!(data[..32].iter().all(|byte| *byte == 0x51));
            assert!(data[128..144].iter().all(|byte| *byte == 0x61));
            assert!(data[144..160].iter().all(|byte| *byte == 0x71));
        });
    }

    #[test]
    fn aligned_same_cache_page_pin_falls_back_from_direct_bypass() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let paddr = seed_cached_page(&cached, 0, 0x5a, false);
        let segment = [PinnedPhysicalSegment::new(paddr.into(), PAGE_SIZE)];
        let pin = cached.pin_cached_page_by_paddr(0, paddr, true).unwrap();

        assert_eq!(
            unsafe { cached.read_at_pinned_segments(&segment, 0, true) },
            Ok(PAGE_SIZE)
        );
        assert_eq!(
            unsafe { cached.write_at_pinned_segments(&segment, 0, true) },
            Ok(PAGE_SIZE)
        );
        drop(pin);
        cached.with_page(0, |page| {
            let page = page.unwrap();
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x5a));
        });
    }

    #[test]
    fn filesystem_inode_aliases_share_pinned_overlap_policy() {
        init_test_page_allocator();
        let state = AppendTestState::new(PAGE_SIZE as u64);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(NodeFlags::empty(), state));
        // Distinct locations for the same filesystem/inode model two hardlink
        // dentries and must resolve to one cache registry owner.
        let first = CachedFile::get_or_create(Mountpoint::new_root(&fs).root_location());
        let alias = CachedFile::get_or_create(Mountpoint::new_root(&fs).root_location());
        assert!(first.ptr_eq(&alias));

        let paddr: usize = seed_cached_page(&first, 0, 0x2a, false).into();
        let destination = [PinnedPhysicalSegment::new(paddr + 512, 32)];
        assert_eq!(
            unsafe { alias.read_at_pinned_segments(&destination, 0, false) },
            Ok(32)
        );
        first.with_page(0, |page| {
            assert!(
                page.unwrap().data()[512..544]
                    .iter()
                    .all(|byte| *byte == 0x2a)
            );
        });
    }

    #[test]
    fn generic_raw_physical_io_bounces_outside_page_data_borrow() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
        cached.with_page(0, |page| {
            let data = page.unwrap().data();
            data[..32].fill(0x31);
            data[1024..1056].fill(0x41);
        });

        assert_eq!(
            cached.read_at(RawPhysicalWriter::new(paddr + 512, 32), 0),
            Ok(32)
        );
        assert_eq!(
            cached.write_at(RawPhysicalReader::new(paddr + 1024, 32), 128),
            Ok(32)
        );
        cached.with_page(0, |page| {
            let data = page.unwrap().data();
            assert!(data[512..544].iter().all(|byte| *byte == 0x31));
            assert!(data[128..160].iter().all(|byte| *byte == 0x41));
        });
    }

    #[test]
    fn transactional_sync_read_never_calls_async_lower_hook() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        state.full_page_io.store(true, Ordering::Release);
        let mut output = vec![0xa5; PAGE_SIZE];

        assert_eq!(cached.read_at_sync(&mut &mut output[..], 0), Ok(PAGE_SIZE));
        assert_eq!(state.async_read_calls.load(Ordering::Acquire), 0);
        assert_eq!(state.read_calls.load(Ordering::Acquire), 1);
        assert!(output.iter().all(|byte| *byte == 0));
    }

    #[cfg(feature = "ext4")]
    #[test]
    fn pinned_fallback_policy_never_calls_async_lower_hook() {
        struct AsyncMappedReadReset;

        impl Drop for AsyncMappedReadReset {
            fn drop(&mut self) {
                lwext4_rust::set_async_mapped_read_enabled(false);
            }
        }

        lwext4_rust::set_async_mapped_read_enabled(true);
        let _reset = AsyncMappedReadReset;

        let (reader, _location, read_state) = cached_append_test_file(PAGE_SIZE as u64);
        read_state.full_page_io.store(true, Ordering::Release);
        let reader = FileBackend::Cached(reader);
        let (destination_cache, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let destination_paddr = seed_cached_page(&destination_cache, 0, 0xa5, false);
        let destination_pin = destination_cache
            .pin_cached_page_by_paddr(0, destination_paddr, true)
            .unwrap();
        let destination = [PinnedPhysicalSegment::new(
            usize::from(destination_paddr) + 128,
            8,
        )];
        assert_eq!(
            unsafe { reader.read_at_pinned_segments(&destination, 1, true) },
            Ok(8)
        );
        assert_eq!(read_state.async_read_calls.load(Ordering::Acquire), 0);
        assert_eq!(read_state.read_calls.load(Ordering::Acquire), 1);
        destination_cache.with_page(0, |page| {
            assert!(page.unwrap().data()[128..136].iter().all(|byte| *byte == 0));
        });
        drop(destination_pin);

        let (writer, _location, write_state) = cached_append_test_file(PAGE_SIZE as u64);
        write_state.full_page_io.store(true, Ordering::Release);
        let cached_writer = writer.clone();
        let writer = FileBackend::Cached(writer);
        let (source_cache, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let source_paddr = seed_cached_page(&source_cache, 0, 0x5a, false);
        let source_pin = source_cache
            .pin_cached_page_by_paddr(0, source_paddr, false)
            .unwrap();
        let source = [PinnedPhysicalSegment::new(
            usize::from(source_paddr) + 128,
            8,
        )];
        assert_eq!(
            unsafe { writer.write_at_pinned_segments(&source, 1, true) },
            Ok(8)
        );
        assert_eq!(write_state.async_read_calls.load(Ordering::Acquire), 0);
        assert_eq!(write_state.read_calls.load(Ordering::Acquire), 1);
        cached_writer.with_page(0, |page| {
            assert!(page.unwrap().data()[1..9].iter().all(|byte| *byte == 0x5a));
        });
        drop(source_pin);
    }

    #[test]
    fn deferred_open_leaves_truncate_for_the_prepared_consumer_to_commit() {
        let state = AppendTestState::new(41);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::NON_CACHEABLE,
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let mut options = OpenOptions::new();
        options.write(true).truncate(true).direct(true);

        let file = options
            .open_loc_deferred_truncate(location)
            .unwrap()
            .into_file()
            .unwrap();
        assert_eq!(state.inode_len.load(Ordering::Acquire), 41);

        file.backend().unwrap().set_len(0).unwrap();
        assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cached_truncate_rejects_pin_preparation_before_inode_mutation() {
        let state = AppendTestState::new(41);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::empty(),
            state.clone(),
        ));
        let mountpoint = Mountpoint::new_root(&fs);
        let cached = CachedFile::get_or_create(mountpoint.root_location());
        let window = cached.begin_user_io_pin_window().unwrap();

        assert!(matches!(cached.set_len(0), Err(VfsError::ResourceBusy)));
        assert_eq!(state.inode_len.load(Ordering::Acquire), 41);

        drop(window);
        cached.set_len(0).unwrap();
        assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concurrent_buffered_cache_users_share_admission() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let cached = Arc::new(cached);
        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let first_cached = cached.clone();
        let first = thread::spawn(move || {
            first_cached.with_page_or_insert(0, |_, _| {
                holding_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        holding_rx.recv().unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let second_cached = cached.clone();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            second_cached.with_page_or_insert(0, |_, _| Ok(()))
        });
        started_rx.recv().unwrap();

        let mut admitted = false;
        for _ in 0..10_000 {
            if cached.shared.user_io_pin_admission.lock().cache_users == 2 {
                admitted = true;
                break;
            }
            thread::yield_now();
        }
        assert!(admitted, "second buffered cache user was not admitted");

        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(second.join().unwrap(), Ok(()));
        assert_eq!(cached.shared.user_io_pin_admission.lock().cache_users, 0);
    }

    #[test]
    fn same_owner_may_defer_cache_eviction_acknowledgement() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x41, false);
        let owner = CachedFileEvictionOwner::new(1).unwrap();
        let handle = cached.add_evict_listener(owner, |_, _| false);

        let acknowledgement = {
            let mut cache = cached.shared.page_cache.lock();
            let page = cache.get_mut(&0).unwrap();
            acknowledge_cached_page_eviction(&cached.shared, 0, page, Some(owner)).unwrap()
        };
        assert!(acknowledgement.had_listener);
        assert!(acknowledgement.deferred);

        unsafe { cached.remove_evict_listener(handle) };
    }

    #[test]
    fn repeated_shared_page_writes_rearm_listener_on_each_sync() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        state.full_page_io.store(true, Ordering::Release);
        seed_cached_page(&cached, 0, b'A', true);
        let listener_calls = Arc::new(AtomicUsize::new(0));
        let calls = listener_calls.clone();
        let owner = CachedFileEvictionOwner::new(3).unwrap();
        let handle = cached.add_evict_listener(owner, move |_, _| {
            calls.fetch_add(1, Ordering::AcqRel);
            true
        });

        cached.sync(false).unwrap();
        assert_eq!(listener_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.stored_first_byte.load(Ordering::Acquire), b'A');
        cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));

        cached.with_page(0, |page| {
            let page = page.unwrap();
            page.data()[0] = b'B';
            page.mark_dirty();
        });
        cached.sync(false).unwrap();
        assert_eq!(listener_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.stored_first_byte.load(Ordering::Acquire), b'B');
        cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));

        unsafe { cached.remove_evict_listener(handle) };
    }

    #[test]
    fn foreign_deferred_listener_rolls_back_dirty_invalidation() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        let original_paddr = seed_cached_page(&cached, 0, 0x5a, true);
        let foreign = CachedFileEvictionOwner::new(2).unwrap();
        let handle = cached.add_evict_listener(foreign, |_, _| false);

        let mutation = cached.begin_cache_invalidating_mutation().unwrap();
        let result = {
            let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
            invalidation.stage_all()
        };
        assert_eq!(result, Err(VfsError::ResourceBusy));
        cached.with_page(0, |page| {
            let page = page.expect("listener contention must restore the staged page");
            assert_eq!(page.paddr(), original_paddr);
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x5a));
        });
        assert!(state.write_offsets.lock().is_empty());
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
        drop(mutation);

        unsafe { cached.remove_evict_listener(handle) };
    }

    #[test]
    fn short_invalidation_writeback_restores_the_dirty_page() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        let original_paddr = seed_cached_page(&cached, 0, 0x5a, true);

        let mutation = cached.begin_cache_invalidating_mutation().unwrap();
        let result = {
            let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
            invalidation.stage_all().unwrap();
            invalidation.writeback(location.entry().as_file().unwrap(), false)
        };
        assert_eq!(result, Err(VfsError::Io));
        cached.with_page(0, |page| {
            let page = page.expect("short writeback must restore the staged page");
            assert_eq!(page.paddr(), original_paddr);
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x5a));
        });
        assert_eq!(state.write_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn precise_pin_blocks_cached_and_direct_truncate_before_raw_set_len() {
        for direct in [false, true] {
            let (cached, location, state) = cached_append_test_file((PAGE_SIZE * 2) as u64);
            let paddr = seed_cached_page(&cached, 1, 0x33, false);
            let pin = cached.pin_cached_page_by_paddr(1, paddr, false).unwrap();

            let result = if direct {
                FileBackend::Direct(location).set_len(0)
            } else {
                cached.set_len(0)
            };
            assert_eq!(result, Err(VfsError::ResourceBusy));
            assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
            assert_eq!(
                state.inode_len.load(Ordering::Acquire),
                (PAGE_SIZE * 2) as u64
            );
            drop(pin);
        }
    }

    #[test]
    fn non_overlapping_precise_pin_allows_cached_shrink() {
        let (cached, _location, state) = cached_append_test_file((PAGE_SIZE * 3) as u64);
        let paddr = seed_cached_page(&cached, 0, 0x11, false);
        seed_cached_page(&cached, 2, 0x22, false);
        let pin = cached.pin_cached_page_by_paddr(0, paddr, false).unwrap();

        cached.set_len((PAGE_SIZE * 2) as u64).unwrap();
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            state.inode_len.load(Ordering::Acquire),
            (PAGE_SIZE * 2) as u64
        );
        cached.with_page(0, |page| assert!(page.is_some()));
        cached.with_page(2, |page| assert!(page.is_none()));
        drop(pin);
    }

    #[test]
    fn write_pin_marks_page_dirty_and_retains_writeback_ownership_on_release() {
        let (cached, location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        let paddr = seed_cached_page(&cached, 0, 0x44, false);
        let window = cached.begin_user_io_pin_window().unwrap();
        let pin = cached.pin_cached_page_by_paddr(0, paddr, true).unwrap();
        drop(window);

        cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));
        drop(pin);
        cached.with_page(0, |page| assert!(page.unwrap().is_dirty()));
        let key = cached_file_registry_key(&location);
        assert!(
            file_cache_registry()
                .lock()
                .get(&key)
                .is_some_and(|entry| entry.writeback_anchor.is_some())
        );
    }

    #[test]
    fn direct_truncate_failure_restores_staged_dirty_page() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        let original_paddr = seed_cached_page(&cached, 0, 0x6b, true);
        state.full_page_io.store(true, Ordering::Release);
        state.fail_set_len.store(true, Ordering::Release);

        assert_eq!(
            FileBackend::Direct(location).set_len(0),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        cached.with_page(0, |page| {
            let page = page.expect("failed truncate must restore the staged page");
            assert_eq!(page.paddr(), original_paddr);
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x6b));
        });
    }

    #[test]
    fn cached_atomic_truncate_failure_restores_written_back_dirty_page() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        let original_paddr = seed_cached_page(&cached, 0, 0x6d, true);
        state.full_page_io.store(true, Ordering::Release);
        state.fail_set_len.store(true, Ordering::Release);

        assert_eq!(cached.set_len(0), Err(VfsError::InvalidInput));
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        cached.with_page(0, |page| {
            let page = page.expect("atomic truncate failure must restore the staged page");
            assert_eq!(page.paddr(), original_paddr);
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x6d));
        });
    }

    #[test]
    fn non_atomic_truncate_failure_discards_potentially_stale_cache() {
        let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x71, true);
        state.full_page_io.store(true, Ordering::Release);
        state.fail_set_len.store(true, Ordering::Release);
        state.set_len_failure_atomic.store(false, Ordering::Release);

        assert_eq!(
            FileBackend::Direct(location).set_len(0),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
        cached.with_page(0, |page| assert!(page.is_none()));
    }

    #[test]
    fn cached_non_atomic_truncate_failure_writes_back_before_discard() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x73, true);
        state.full_page_io.store(true, Ordering::Release);
        state.fail_set_len.store(true, Ordering::Release);
        state.set_len_failure_atomic.store(false, Ordering::Release);

        assert_eq!(cached.set_len(0), Err(VfsError::InvalidInput));
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        cached.with_page(0, |page| assert!(page.is_none()));
    }

    #[test]
    fn partial_page_truncate_zeroes_tail_and_discards_full_pages() {
        let (cached, _location, state) = cached_append_test_file((PAGE_SIZE * 3) as u64);
        seed_cached_page(&cached, 1, 0x7c, false);
        seed_cached_page(&cached, 2, 0x2c, false);
        let new_len = (PAGE_SIZE + 17) as u64;

        cached.set_len(new_len).unwrap();
        assert_eq!(state.inode_len.load(Ordering::Acquire), new_len);
        cached.with_page(1, |page| {
            let page = page.unwrap();
            assert!(page.data()[..17].iter().all(|byte| *byte == 0x7c));
            assert!(page.data()[17..].iter().all(|byte| *byte == 0));
            assert!(page.is_dirty());
        });
        cached.with_page(2, |page| assert!(page.is_none()));
    }

    #[test]
    fn direct_invalidation_holds_pin_admission_through_lower_operation() {
        let (cached, location, _state) = cached_append_test_file(0);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let operation = thread::spawn(move || {
            with_sync_and_invalidate_cached_file_pages(&location, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv().unwrap();
        assert!(matches!(
            cached.begin_user_io_pin_window(),
            Err(VfsError::ResourceBusy)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(operation.join().unwrap(), Ok(()));
        drop(cached.begin_user_io_pin_window().unwrap());
    }

    #[test]
    fn aligned_write_bypass_waits_for_existing_writeback_reader() {
        let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x52, true);
        state.full_page_io.store(true, Ordering::Release);

        let observer = cached.clone();
        let writeback_reader = observer.shared.writeback_lock.read();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let new_page = vec![0x6a; PAGE_SIZE];
        let writer = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(cached.write_at_slice(&new_page, 0)).unwrap();
        });
        started_rx.recv().unwrap();

        let mut owns_direct_io = false;
        for _ in 0..10_000 {
            if observer.shared.direct_io_lock.try_read().is_none() {
                owns_direct_io = true;
                break;
            }
            thread::yield_now();
        }
        assert!(
            owns_direct_io,
            "aligned bypass did not enter its direct-I/O domain"
        );
        assert_eq!(
            result_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
        observer.with_page(0, |page| {
            let page = page.expect("blocked bypass must not stage the dirty cache page");
            assert!(page.is_dirty());
            assert!(page.data().iter().all(|byte| *byte == 0x52));
        });

        drop(writeback_reader);
        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Ok(PAGE_SIZE))
        );
        writer.join().unwrap();
        assert_eq!(state.write_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.stored_first_byte.load(Ordering::Acquire), 0x6a);
        observer.with_page(0, |page| assert!(page.is_none()));
    }

    #[test]
    fn discard_waits_for_in_flight_writeback_before_staging_pages() {
        let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
        seed_cached_page(&cached, 0, 0x52, true);
        let shared = cached.shared.clone();
        let (writeback_started_tx, writeback_started_rx) = mpsc::channel();
        let (release_writeback_tx, release_writeback_rx) = mpsc::channel();

        let writeback_shared = shared.clone();
        let writeback = thread::spawn(move || {
            let _writeback_guard = writeback_shared.writeback_lock.read();
            writeback_shared
                .page_cache
                .lock()
                .get_mut(&0)
                .unwrap()
                .begin_writeback()
                .unwrap();
            writeback_started_tx.send(()).unwrap();
            release_writeback_rx.recv().unwrap();
            writeback_shared
                .page_cache
                .lock()
                .get_mut(&0)
                .unwrap()
                .end_writeback();
        });
        writeback_started_rx.recv().unwrap();

        let (discard_result_tx, discard_result_rx) = mpsc::channel();
        let discard = thread::spawn(move || {
            discard_result_tx
                .send(discard_cached_pages(&shared))
                .unwrap();
        });
        assert_eq!(
            discard_result_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_writeback_tx.send(()).unwrap();
        writeback.join().unwrap();
        assert_eq!(
            discard_result_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Ok(()))
        );
        discard.join().unwrap();
        cached.with_page(0, |page| assert!(page.is_none()));
    }

    #[test]
    fn path_only_open_skips_the_filesystem_open_callback() {
        let state = AppendTestState::new(0);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::NON_CACHEABLE,
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let mut options = OpenOptions::new();
        options.path(true);

        let result = options.open_loc(location).unwrap().into_file().unwrap();
        assert!(result.is_path());
        assert_eq!(state.open_calls.load(Ordering::Acquire), 0);
    }

    fn assert_positioned_append_offsets(state: &AppendTestState) {
        assert_eq!(&*state.write_offsets.lock(), &[0, 2]);
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn explicit_current_ignores_default_append_for_all_write_forms() {
        let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
        let mut src = Cursor::new(&b"abcd"[..]);
        assert_eq!(
            file.write_with_placement(&mut src, WritePlacement::Current),
            Ok(2)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

        let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
        assert_eq!(
            file.write_slice_with_placement(b"abcd", WritePlacement::Current),
            Ok(2)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

        let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
        assert_eq!(
            file.write_vectored_slice_with_placement(&[b"abcd", b"ef"], WritePlacement::Current,),
            Ok(2)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn current_position_transfer_commits_only_destination_prefix() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::READ);
        let mut buf = [0u8; 4];

        assert_eq!(
            file.read_slice_then(&mut buf, |data| {
                assert_eq!(data, b"abcd");
                Ok(2)
            }),
            Ok(2)
        );
        assert_eq!(
            file.read_slice_then(&mut buf, |data| {
                assert_eq!(data, b"cdef");
                Err(VfsError::InvalidInput)
            }),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            file.read_slice_then(&mut buf, |data| {
                assert_eq!(data, b"cdef");
                Ok(data.len())
            }),
            Ok(4)
        );
        assert_eq!(file.read_slice(&mut buf), Ok(2));
        assert_eq!(&buf[..2], b"gh");
        assert_eq!(&*state.read_offsets.lock(), &[0, 2, 2, 6]);
    }

    #[test]
    fn checked_current_read_rejects_before_backend_io_and_cursor_commit() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::READ);
        let mut buf = [0u8; 4];

        assert_eq!(
            file.read_slice_at_current_checked_then(
                &mut buf,
                |offset| {
                    assert_eq!(offset, 0);
                    Err(VfsError::PermissionDenied)
                },
                |_data, _offset| unreachable!(),
            ),
            Err(VfsError::PermissionDenied)
        );
        assert!(state.read_offsets.lock().is_empty());
        assert_eq!(file.read_slice(&mut buf), Ok(4));
        assert_eq!(&buf, b"abcd");
        assert_eq!(&*state.read_offsets.lock(), &[0]);
    }

    #[test]
    fn current_write_callback_uses_frozen_offset_and_commits_only_its_prefix() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::WRITE);

        assert_eq!(
            file.with_current_position(|offset| {
                assert_eq!(offset, 0);
                Ok(offset)
            }),
            Ok(0)
        );

        assert_eq!(
            file.write_slice_at_current_then(b"abcd", |data, offset| {
                assert_eq!(offset, 0);
                file.write_at_slice(data, offset)
            }),
            Ok(2)
        );
        assert_eq!(
            file.write_slice_at_current_then(b"ef", |_data, offset| {
                assert_eq!(offset, 2);
                Err(VfsError::PermissionDenied)
            }),
            Err(VfsError::PermissionDenied)
        );
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
        assert_eq!(&*state.write_offsets.lock(), &[0]);
    }

    #[test]
    fn operation_position_transaction_commits_once_and_rolls_back_errors() {
        let (file, _state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE,
            16,
            FileFlags::READ | FileFlags::WRITE,
        );

        assert_eq!(
            file.with_current_position_transaction(8, |offset| {
                assert_eq!(offset, 0);
                // Model two internal chunks without publishing the intermediate
                // cursor to lseek/read/write users of this description.
                Ok((17, 6))
            }),
            Ok(17)
        );
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));

        assert_eq!(
            file.with_current_position_transaction(4, |offset| {
                assert_eq!(offset, 6);
                Err::<((), usize), _>(VfsError::PermissionDenied)
            }),
            Err(VfsError::PermissionDenied)
        );
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));
        assert_eq!(
            file.with_current_position_transaction(4, |_offset| Ok(((), 5))),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));
    }

    #[test]
    fn operation_position_transaction_hides_intermediate_cursor_from_concurrent_read() {
        let (file, _state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE,
            16,
            FileFlags::READ | FileFlags::WRITE,
        );
        let file = Arc::new(file);
        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let transfer_file = file.clone();
        let transfer = thread::spawn(move || {
            transfer_file
                .with_current_position_transaction(8, |offset| {
                    assert_eq!(offset, 0);
                    holding_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    // Model multiple positioned chunks whose aggregate cursor
                    // update must not become visible until this callback ends.
                    Ok(((), 6))
                })
                .unwrap();
        });
        holding_rx.recv().unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();
        let observer_file = file.clone();
        let observer = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let mut buf = [0u8; 2];
            let read = observer_file.read_slice(&mut buf).unwrap();
            let mut handle = observer_file.as_ref();
            let position = handle.seek(SeekFrom::Current(0)).unwrap();
            observed_tx.send((read, buf, position)).unwrap();
        });
        started_rx.recv().unwrap();
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_tx.send(()).unwrap();
        transfer.join().unwrap();
        let (read, buf, position) = observed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        observer.join().unwrap();
        assert_eq!(read, 2);
        assert_eq!(&buf, b"gh");
        assert_eq!(position, 8);
    }

    #[test]
    fn same_description_transfer_can_write_at_its_frozen_position() {
        let (file, state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE,
            8,
            FileFlags::READ | FileFlags::WRITE,
        );
        let mut buf = [0u8; 4];

        assert_eq!(
            file.read_slice_at_current_then(&mut buf, |data, offset| {
                assert_eq!(data, b"abcd");
                assert_eq!(offset, 0);
                file.write_at_slice(data, offset)
            }),
            Ok(2)
        );
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
        assert_eq!(&*state.read_offsets.lock(), &[0]);
        assert_eq!(&*state.write_offsets.lock(), &[0]);
    }

    #[test]
    fn stream_read_and_append_status_write_use_zero_without_an_ofd_position() {
        let (file, state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE | NodeFlags::STREAM,
            8,
            FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
        );

        let mut bytes = [0u8; 2];
        let mut dst = Cursor::new(&mut bytes[..]);
        assert_eq!(file.read(&mut dst), Ok(2));
        assert_eq!(&bytes, b"ab");

        let mut src = Cursor::new(&b"xy"[..]);
        assert_eq!(file.write(&mut src), Ok(2));
        assert_eq!(&*state.read_offsets.lock(), &[0]);
        assert_eq!(&*state.write_offsets.lock(), &[0]);
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
        assert!(!file.has_current_position());
        assert!(file.supports_positioned_read());
        assert!(file.supports_positioned_write());
        assert!(file.supports_seek());
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(0));
    }

    #[test]
    fn positioned_io_and_seek_capabilities_are_independent_of_stream_cursor() {
        let flags = NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK;
        let (file, _state) =
            append_test_file_with_access(flags, 8, FileFlags::READ | FileFlags::WRITE);

        assert!(!file.has_current_position());
        assert!(!file.supports_positioned_read());
        assert!(!file.supports_positioned_write());
        assert!(!file.supports_seek());
    }

    #[test]
    fn direct_multichunk_errors_return_and_publish_committed_prefixes() {
        let chunk = FileBackend::DIRECT_IO_CHUNK;

        let (reader, read_state) = append_test_file_with_access(
            NodeFlags::NON_CACHEABLE,
            (chunk * 2) as u64,
            FileFlags::READ,
        );
        read_state.full_page_io.store(true, Ordering::Release);
        read_state.fail_read_call.store(1, Ordering::Release);
        let mut output = vec![0u8; chunk * 2];
        let mut dst = Cursor::new(output.as_mut_slice());
        assert_eq!(reader.read(&mut dst), Ok(chunk));
        let mut reader_handle = &reader;
        assert_eq!(reader_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
        assert_eq!(&*read_state.read_offsets.lock(), &[0, chunk as u64]);

        let (writer, write_state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
        write_state.full_page_io.store(true, Ordering::Release);
        write_state.fail_write_call.store(1, Ordering::Release);
        let input = vec![0x5a; chunk * 2];
        let mut src = Cursor::new(input.as_slice());
        assert_eq!(
            writer.write_with_placement(&mut src, WritePlacement::Current),
            Ok(chunk)
        );
        let mut writer_handle = &writer;
        assert_eq!(writer_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
        assert_eq!(&*write_state.write_offsets.lock(), &[0, chunk as u64]);
    }

    #[test]
    fn cached_aligned_bypass_multichunk_errors_preserve_completed_prefixes() {
        let chunk = ALIGNED_BYPASS_CHUNK;

        let (cached, _location, read_state) = cached_append_test_file((chunk * 2) as u64);
        read_state.full_page_io.store(true, Ordering::Release);
        read_state.fail_read_call.store(1, Ordering::Release);
        let reader = File::new(FileBackend::Cached(cached), FileFlags::READ);
        let mut output = vec![0u8; chunk * 2];
        let mut dst = Cursor::new(output.as_mut_slice());
        assert_eq!(reader.read(&mut dst), Ok(chunk));
        let mut reader_handle = &reader;
        assert_eq!(reader_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
        assert_eq!(&*read_state.read_offsets.lock(), &[0, chunk as u64]);

        let (cached, _location, source_state) = cached_append_test_file(0);
        source_state.full_page_io.store(true, Ordering::Release);
        let source_writer = File::new(FileBackend::Cached(cached), FileFlags::WRITE);
        assert_eq!(
            source_writer
                .write_with_placement(FaultingReader::new(chunk * 2, 1), WritePlacement::Current,),
            Ok(chunk)
        );
        let mut source_writer_handle = &source_writer;
        assert_eq!(
            source_writer_handle.seek(SeekFrom::Current(0)),
            Ok(chunk as u64)
        );
        assert_eq!(&*source_state.write_offsets.lock(), &[0]);

        let (cached, _location, write_state) = cached_append_test_file(0);
        write_state.full_page_io.store(true, Ordering::Release);
        write_state.fail_write_call.store(1, Ordering::Release);
        let writer = File::new(FileBackend::Cached(cached), FileFlags::WRITE);
        let input = vec![0x5a; chunk * 2];
        let mut src = Cursor::new(input.as_slice());
        assert_eq!(
            writer.write_with_placement(&mut src, WritePlacement::Current),
            Ok(chunk)
        );
        let mut writer_handle = &writer;
        assert_eq!(writer_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
        assert_eq!(&*write_state.write_offsets.lock(), &[0, chunk as u64]);
    }

    #[test]
    fn cached_write_fault_before_first_byte_does_not_extend_inode() {
        let (cached, _location, state) = cached_append_test_file(0);
        let file = File::new(FileBackend::Cached(cached), FileFlags::WRITE);

        assert_eq!(
            file.write_with_placement(
                FaultingReader::new(PAGE_SIZE + 1, 0),
                WritePlacement::Current,
            ),
            Err(axio::Error::BadAddress)
        );
        assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(0));
    }

    #[test]
    fn cached_generic_second_page_fault_returns_and_publishes_first_page() {
        let (cached, _location, state) = cached_append_test_file(0);
        let file = File::new(FileBackend::Cached(cached), FileFlags::WRITE);

        assert_eq!(
            file.write_with_placement(
                FaultingReader::new(PAGE_SIZE + 1, 1),
                WritePlacement::Current,
            ),
            Ok(PAGE_SIZE)
        );
        assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(PAGE_SIZE as u64));
    }

    #[test]
    fn cached_read_second_page_error_returns_and_publishes_first_page() {
        let (cached, _location, state) = cached_append_test_file((PAGE_SIZE + 1) as u64);
        state.full_page_io.store(true, Ordering::Release);
        state.fail_read_call.store(1, Ordering::Release);
        let file = File::new(FileBackend::Cached(cached), FileFlags::READ);
        let mut output = vec![0u8; PAGE_SIZE + 1];

        assert_eq!(file.read_slice(&mut output), Ok(PAGE_SIZE));
        assert_eq!(&*state.read_offsets.lock(), &[0, PAGE_SIZE as u64]);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(PAGE_SIZE as u64));
    }

    #[test]
    fn cached_pinned_second_page_errors_preserve_completed_prefixes() {
        let (reader, _location, read_state) = cached_append_test_file((PAGE_SIZE + 1) as u64);
        read_state.full_page_io.store(true, Ordering::Release);
        read_state.fail_read_call.store(1, Ordering::Release);
        let mut destination = vec![0u8; PAGE_SIZE + 1];
        let destination_segment = [PinnedPhysicalSegment::new(
            destination.as_mut_ptr() as usize,
            destination.len(),
        )];
        assert_eq!(
            unsafe { reader.read_at_pinned_segments(&destination_segment, 0, false) },
            Ok(PAGE_SIZE)
        );

        let (writer, _location, write_state) = cached_append_test_file(0);
        write_state.fail_set_len_call.store(1, Ordering::Release);
        let source = vec![0x6b; PAGE_SIZE + 1];
        let source_segment = [PinnedPhysicalSegment::new(
            source.as_ptr() as usize,
            source.len(),
        )];
        assert_eq!(
            unsafe { writer.write_at_pinned_segments(&source_segment, 0, false) },
            Ok(PAGE_SIZE)
        );
        assert_eq!(
            write_state.inode_len.load(Ordering::Acquire),
            PAGE_SIZE as u64
        );
        assert_eq!(write_state.set_len_calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn direct_pinned_io_never_passes_user_physical_ranges_as_lower_slices() {
        let (_cached, location, state) = cached_append_test_file(8);
        let backend = FileBackend::Direct(location);

        let mut destination = vec![0u8; 8];
        let destination_start = destination.as_mut_ptr() as usize;
        let destination_end = destination_start + destination.len();
        let destination_segment = [PinnedPhysicalSegment::new(
            destination_start,
            destination.len(),
        )];
        assert_eq!(
            unsafe { backend.read_at_pinned_segments(&destination_segment, 0, true) },
            Ok(8)
        );
        let lower_read = state.last_read_buf.load(Ordering::Acquire);
        assert!(!(destination_start..destination_end).contains(&lower_read));

        let source = vec![0x4d; 8];
        let source_start = source.as_ptr() as usize;
        let source_end = source_start + source.len();
        let source_segment = [PinnedPhysicalSegment::new(source_start, source.len())];
        assert_eq!(
            unsafe { backend.write_at_pinned_segments(&source_segment, 0, true) },
            Ok(2)
        );
        let lower_write = state.last_write_buf.load(Ordering::Acquire);
        assert!(!(source_start..source_end).contains(&lower_write));
    }

    #[test]
    fn default_vectored_loops_preserve_progress_before_later_errors() {
        let (reader, read_state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 2, FileFlags::READ);
        read_state.full_page_io.store(true, Ordering::Release);
        read_state.fail_read_call.store(1, Ordering::Release);
        let mut left = [0u8; 1];
        let mut right = [0u8; 1];
        let mut dst: [&mut [u8]; 2] = [&mut left, &mut right];
        assert_eq!(
            reader
                .location()
                .entry()
                .as_file()
                .unwrap()
                .read_at_vectored(&mut dst, 0),
            Ok(1)
        );

        let (writer, write_state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
        write_state.full_page_io.store(true, Ordering::Release);
        write_state.fail_write_call.store(1, Ordering::Release);
        let src: [&[u8]; 2] = [b"a", b"b"];
        assert_eq!(
            writer
                .location()
                .entry()
                .as_file()
                .unwrap()
                .write_at_vectored(&src, 0),
            Ok(1)
        );
    }

    #[test]
    fn scalar_append_error_after_one_direct_chunk_publishes_the_prefix() {
        let chunk = FileBackend::DIRECT_IO_CHUNK;
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
        state.fail_append_call.store(1, Ordering::Release);
        let input = vec![0x41; chunk * 2];
        let mut src = Cursor::new(input.as_slice());

        assert_eq!(
            file.write_with_placement(&mut src, WritePlacement::End),
            Ok(chunk)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.inode_len.load(Ordering::Acquire), chunk as u64);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
    }

    #[test]
    fn different_ofds_cannot_admit_append_against_the_same_stale_eof() {
        let state = AppendTestState::new(0);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::NON_CACHEABLE,
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let left = File::new(FileBackend::Direct(location.clone()), FileFlags::WRITE);
        let right = File::new(FileBackend::Direct(location), FileFlags::WRITE);
        let start = Arc::new(Barrier::new(3));

        let spawn = |file: File, marker: u8, start: Arc<Barrier>| {
            thread::spawn(move || {
                start.wait();
                let bytes = [marker];
                let mut src = Cursor::new(&bytes[..]);
                file.write_with_placement_and_admission(
                    &mut src,
                    WritePlacement::End,
                    |offset, requested| {
                        if offset >= 1 {
                            Err(VfsError::OutOfRange)
                        } else {
                            Ok(requested)
                        }
                    },
                )
            })
        };
        let left = spawn(left, b'L', start.clone());
        let right = spawn(right, b'R', start.clone());
        start.wait();
        let results = [left.join().unwrap(), right.join().unwrap()];

        assert_eq!(results.iter().filter(|result| **result == Ok(1)).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(VfsError::OutOfRange))
                .count(),
            1
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 1);
    }

    #[test]
    fn same_ofd_append_admission_keeps_operation_order_and_cursor() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
        let file = Arc::new(file);
        let offsets = Arc::new(StdMutex::new(Vec::new()));
        let start = Arc::new(Barrier::new(3));

        let spawn =
            |file: Arc<File>, marker: u8, start: Arc<Barrier>, offsets: Arc<StdMutex<Vec<u64>>>| {
                thread::spawn(move || {
                    start.wait();
                    let bytes = [marker];
                    let mut src = Cursor::new(&bytes[..]);
                    file.write_with_placement_and_admission(
                        &mut src,
                        WritePlacement::End,
                        |offset, requested| {
                            offsets.lock().unwrap().push(offset);
                            Ok(requested)
                        },
                    )
                })
            };
        let left = spawn(file.clone(), b'A', start.clone(), offsets.clone());
        let right = spawn(file.clone(), b'B', start.clone(), offsets.clone());
        start.wait();
        assert_eq!(left.join().unwrap(), Ok(1));
        assert_eq!(right.join().unwrap(), Ok(1));

        assert_eq!(&*offsets.lock().unwrap(), &[0, 1]);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 2);
        let mut handle = file.as_ref();
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
    }

    #[test]
    fn competing_unaligned_append_cannot_move_eof_after_admission() {
        let state = AppendTestState::new(512);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::NON_CACHEABLE,
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let aligned = File::new(FileBackend::Direct(location.clone()), FileFlags::WRITE);
        let unaligned = File::new(FileBackend::Direct(location), FileFlags::WRITE);
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let first = thread::spawn(move || {
            let bytes = vec![b'A'; 512];
            let mut src = Cursor::new(bytes.as_slice());
            aligned.write_at_end_with_admission(&mut src, |offset, requested| {
                assert_eq!(offset, 512);
                admitted_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(requested)
            })
        });
        admitted_rx.recv().unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = unaligned.write_at_end_slice(b"u");
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(512));
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(1));
        second.join().unwrap();
        assert_eq!(&*state.append_markers.lock().unwrap(), b"Au");
        assert_eq!(state.inode_len.load(Ordering::Acquire), 1025);
    }

    #[test]
    fn explicit_end_ignores_non_append_default_for_all_write_forms() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        let mut src = Cursor::new(&b"ab"[..]);
        assert_eq!(
            file.write_with_placement_and_admission(
                &mut src,
                WritePlacement::End,
                |offset, requested| {
                    assert_eq!(offset, 41);
                    Ok(requested)
                },
            ),
            Ok(2)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 43);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(43));

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        assert_eq!(
            file.write_slice_with_placement(b"abc", WritePlacement::End),
            Ok(3)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 44);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        assert_eq!(
            file.write_vectored_slice_with_placement(&[b"ab", b"c"], WritePlacement::End),
            Ok(3)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 44);
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(&*state.write_offsets.lock(), &[44]);
    }

    #[test]
    fn vectored_append_preserves_partial_and_error_position_semantics() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        state.append_limit.store(1, Ordering::Release);
        assert_eq!(
            file.write_vectored_slice_with_placement(&[b"ab", b"cd"], WritePlacement::End),
            Ok(1)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 42);
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(&*state.write_offsets.lock(), &[42]);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        state.fail_append_call.store(1, Ordering::Release);
        assert_eq!(
            file.write_vectored_slice_with_placement(&[b"ab", b"cd"], WritePlacement::End),
            Ok(2)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 43);
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(&*state.write_offsets.lock(), &[43]);
        let mut handle = &file;
        assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(43));
    }

    #[test]
    fn concurrent_vectored_appends_keep_each_scatter_list_contiguous() {
        const WRITES: usize = 512;

        let state = AppendTestState::new(0);
        state.yield_after_append.store(true, Ordering::Release);
        let fs = Filesystem::new(RegistryTestFs::new_for_append(
            NodeFlags::NON_CACHEABLE,
            state.clone(),
        ));
        let location = Mountpoint::new_root(&fs).root_location();
        let left = Arc::new(File::new(
            FileBackend::Direct(location.clone()),
            FileFlags::WRITE,
        ));
        let right = Arc::new(File::new(FileBackend::Direct(location), FileFlags::WRITE));
        let start = Arc::new(Barrier::new(3));

        let left_thread = {
            let file = left.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for _ in 0..WRITES {
                    file.write_at_end_vectored_slice(&[b"A", b"a"]).unwrap();
                }
            })
        };
        let right_thread = {
            let file = right.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for _ in 0..WRITES {
                    file.write_at_end_vectored_slice(&[b"B", b"b"]).unwrap();
                }
            })
        };
        start.wait();
        left_thread.join().unwrap();
        right_thread.join().unwrap();

        let markers = state.append_markers.lock().unwrap();
        assert_eq!(markers.len(), WRITES * 4);
        assert!(
            markers
                .chunks_exact(2)
                .all(|pair| pair == b"Aa" || pair == b"Bb")
        );
        assert_eq!(state.inode_len.load(Ordering::Acquire), (WRITES * 4) as u64);
    }

    #[test]
    fn positioned_end_writes_do_not_change_ofd_position() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        let mut src = Cursor::new(&b"ab"[..]);
        assert_eq!(
            file.write_at_end_with_admission(&mut src, |offset, requested| {
                assert_eq!(offset, 41);
                Ok(requested)
            }),
            Ok(2)
        );
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Ok(1)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        assert_eq!(file.write_at_end_slice(b"ab"), Ok(2));
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Ok(1)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        assert_eq!(file.write_at_end_vectored_slice(&[b"a", b"b"]), Ok(2));
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Ok(1)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);
    }

    #[test]
    fn positioned_append_write_forms_use_and_advance_ofd_offset() {
        let flags = NodeFlags::NON_CACHEABLE | NodeFlags::POSITIONED_APPEND;

        let (file, state) = append_test_file(flags, 97);
        let mut src = Cursor::new(&b"abcd"[..]);
        assert_eq!(file.write(&mut src), Ok(2));
        let mut second = Cursor::new(&b"z"[..]);
        assert_eq!(file.write(&mut second), Err(VfsError::InvalidInput));
        assert_positioned_append_offsets(&state);

        let (file, state) = append_test_file(flags, 97);
        assert_eq!(file.write_slice(b"abcd"), Ok(2));
        assert_eq!(file.write_slice(b"z"), Err(VfsError::InvalidInput));
        assert_positioned_append_offsets(&state);

        let (file, state) = append_test_file(flags, 97);
        assert_eq!(file.write_vectored_slice(&[b"abcd", b"ef"]), Ok(2));
        assert_eq!(
            file.write_vectored_slice(&[b"z"]),
            Err(VfsError::InvalidInput)
        );
        assert_positioned_append_offsets(&state);
    }

    #[test]
    fn ordinary_append_still_uses_inode_append() {
        let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);

        assert_eq!(file.write_slice(b"abc"), Ok(3));
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert!(state.write_offsets.lock().is_empty());
        assert_eq!(state.inode_len.load(Ordering::Acquire), 44);
    }

    #[test]
    fn append_status_can_change_without_changing_write_authority() {
        let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
        file.set_append(false);
        assert_eq!(file.write_slice(b"abc"), Ok(2));
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
        assert_eq!(&*state.write_offsets.lock(), &[0]);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        file.set_append(true);
        assert!(file.flags().contains(FileFlags::APPEND));
        assert_eq!(file.write_slice(b"abc"), Ok(3));
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);

        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        file.set_append(true);
        file.set_append(false);
        assert!(!file.flags().contains(FileFlags::APPEND));
        assert_eq!(file.write_slice(b"abc"), Ok(2));
        assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
        assert_eq!(&*state.write_offsets.lock(), &[0]);

        let (file, _state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::READ);
        file.set_append(true);
        assert!(file.flags().contains(FileFlags::APPEND));
        assert!(matches!(
            file.access(FileFlags::APPEND),
            Err(VfsError::BadFileDescriptor)
        ));
        assert_eq!(file.write_slice(b"x"), Err(VfsError::BadFileDescriptor));
    }

    struct RegistryTestFs {
        this: Weak<Self>,
        append: Option<(NodeFlags, Arc<AppendTestState>)>,
    }

    impl RegistryTestFs {
        fn new() -> Arc<Self> {
            Arc::new_cyclic(|this| Self {
                this: this.clone(),
                append: None,
            })
        }

        fn new_for_append(flags: NodeFlags, state: Arc<AppendTestState>) -> Arc<Self> {
            Arc::new_cyclic(|this| Self {
                this: this.clone(),
                append: Some((flags, state)),
            })
        }
    }

    impl FilesystemOps for RegistryTestFs {
        fn name(&self) -> &str {
            "registry-test"
        }

        fn root_dir(&self) -> DirEntry {
            let fs = self.this.upgrade().expect("test filesystem is live");
            let node: Arc<dyn FileNodeOps> = if let Some((flags, state)) = &self.append {
                Arc::new(AppendTestFile {
                    flags: *flags,
                    state: state.clone(),
                    fs,
                })
            } else {
                Arc::new(RegistryTestFile { fs })
            };
            DirEntry::new_file(
                FileNode::new(node),
                NodeType::RegularFile,
                Reference::root(),
            )
        }

        fn stat(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 1,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
                mount_flags: 0,
            })
        }
    }

    struct RegistryTestFile {
        fs: Arc<RegistryTestFs>,
    }

    impl NodeOps for RegistryTestFile {
        fn inode(&self) -> u64 {
            1
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 1,
                nlink: 0,
                mode: NodePermission::from_bits_truncate(0o600),
                node_type: NodeType::RegularFile,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: axfs_ng_vfs::Timestamp::ZERO,
                btime: axfs_ng_vfs::Timestamp::ZERO,
                mtime: axfs_ng_vfs::Timestamp::ZERO,
                ctime: axfs_ng_vfs::Timestamp::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            &*self.fs
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl Pollable for RegistryTestFile {
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

    impl FileNodeOps for RegistryTestFile {
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Ok(0)
        }

        fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
            Ok(buf.len())
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            Ok((buf.len(), buf.len() as u64))
        }

        fn set_len(&self, _len: u64) -> VfsResult<()> {
            Ok(())
        }

        fn set_symlink(&self, _target: &str) -> VfsResult<()> {
            Ok(())
        }
    }

    #[test]
    fn unlinked_last_close_releases_registry_retention_and_anchor() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let identity = fs.identity_weak();
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let shared = Arc::new(CachedFileShared::new(key, true));
        shared.unlinked.store(true, Ordering::Release);
        shared.open_handles.store(1, Ordering::Release);

        let retained_pages_before = CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire);
        let mut entry = FileUserData::new(&location, &shared);
        assert!(entry.retain_closed(&location, &shared, 3).is_none());
        entry.writeback_anchor = Some(location.writeback_anchor());
        let retired = file_cache_registry().lock().insert(key, entry);
        assert!(retired.is_none());
        assert_eq!(Arc::strong_count(&shared), 2);
        assert_eq!(
            CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire),
            retained_pages_before + 3
        );

        let cached = CachedFile {
            inner: location,
            shared: shared.clone(),
            in_memory: true,
        };
        drop(mountpoint);
        drop(fs);
        drop(cached);

        assert!(!file_cache_registry().lock().contains_key(&key));
        assert_eq!(Arc::strong_count(&shared), 1);
        assert_eq!(
            CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire),
            retained_pages_before
        );
        assert!(identity.upgrade().is_none());
    }

    #[test]
    fn unlinked_cleanup_waits_for_live_direct_range_lease() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let shared = Arc::new(CachedFileShared::new(key, true));
        let retired = file_cache_registry()
            .lock()
            .insert(key, FileUserData::new(&location, &shared));
        assert!(retired.is_none());

        let lease = CachedFileShared::try_range_cache_lease(
            &shared,
            0..PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectWrite,
        )
        .unwrap();
        mark_cached_file_unlinked(&location);
        assert!(shared.unlinked_cleanup_pending.load(Ordering::Acquire));
        assert!(file_cache_registry().lock().contains_key(&key));

        // The unlink path must not panic or discard around a live direct
        // effect. The exact lease drop is the synchronous cleanup trigger.
        drop(lease);
        assert!(!shared.unlinked_cleanup_pending.load(Ordering::Acquire));
        assert!(!file_cache_registry().lock().contains_key(&key));

        drop(mountpoint);
        drop(fs);
    }

    #[test]
    fn unlinked_registry_release_does_not_remove_replacement_shared() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let replacement = Arc::new(CachedFileShared::new(key, true));
        let stale = Arc::new(CachedFileShared::new(key, true));
        let mut entry = FileUserData::new(&location, &replacement);
        entry.writeback_anchor = Some(location.writeback_anchor());
        let retired = file_cache_registry().lock().insert(key, entry);
        assert!(retired.is_none());

        release_unlinked_cached_file_registry_ownership(&location, &stale);

        {
            let registry = file_cache_registry().lock();
            let entry = registry.get(&key).expect("replacement entry must remain");
            assert!(entry.references_shared(&replacement));
            assert!(entry.writeback_anchor.is_some());
        }
        let retired = file_cache_registry().lock().remove(&key);
        drop(retired);
    }

    #[test]
    fn released_shared_does_not_remove_replacement_registry_entry() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let stale = Arc::new(CachedFileShared::new(key, true));
        let replacement = Arc::new(CachedFileShared::new(key, true));
        let retired = file_cache_registry()
            .lock()
            .insert(key, FileUserData::new(&location, &replacement));
        assert!(retired.is_none());

        drop(stale);

        {
            let registry = file_cache_registry().lock();
            let entry = registry.get(&key).expect("replacement entry must remain");
            assert!(entry.references_shared(&replacement));
        }
        let retired = file_cache_registry().lock().remove(&key);
        drop(retired);
    }

    #[test]
    fn ordinary_linked_cached_file_close_removes_dead_registry_entry() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let cached = CachedFile::get_or_create(location);
        assert!(file_cache_registry().lock().contains_key(&key));

        drop(cached);

        assert!(!file_cache_registry().lock().contains_key(&key));
    }

    #[test]
    fn inode_generation_replacement_gets_distinct_cache_identity() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();

        let first = CachedFile::get_or_create(location.clone());
        let first_identity = first.identity();

        // Model the backend publishing a new inode-generation attachment in
        // the same device/inode slot after unlink.  The old cache remains
        // live, so a raw pair key would incorrectly merge the two states.
        let replacement = FileUserData::new_identity(&location);
        assert_eq!(replacement.registry_key.device(), first_identity.device());
        assert_eq!(replacement.registry_key.inode(), first_identity.inode());
        assert_ne!(replacement.registry_key.object(), first_identity.object());
        location.user_data().insert(replacement);

        let second = CachedFile::get_or_create(location);
        assert_ne!(first.identity(), second.identity());
        assert!(!first.ptr_eq(&second));
        {
            let registry = file_cache_registry().lock();
            assert!(registry.contains_key(&first_identity));
            assert!(registry.contains_key(&second.identity()));
        }
        remove_cached_file_registry_entry(first_identity.device(), first_identity.inode());
        assert!(
            file_cache_registry()
                .lock()
                .contains_key(&second.identity())
        );

        drop(first);
        drop(second);
        assert!(!file_cache_registry().lock().contains_key(&first_identity));
    }

    #[test]
    fn direct_only_shared_release_removes_dead_registry_entry() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let shared = cached_file_shared_for_location_or_create(&location);
        assert!(file_cache_registry().lock().contains_key(&key));

        drop(shared);

        assert!(!file_cache_registry().lock().contains_key(&key));
    }

    #[test]
    fn final_shared_release_preserves_writeback_anchor() {
        let fs = Filesystem::new(RegistryTestFs::new());
        let mountpoint = Mountpoint::new_root(&fs);
        let location = mountpoint.root_location();
        let key = cached_file_registry_key(&location);
        let shared = Arc::new(CachedFileShared::new(key, true));
        let mut entry = FileUserData::new(&location, &shared);
        entry.writeback_anchor = Some(location.writeback_anchor());
        let retired = file_cache_registry().lock().insert(key, entry);
        assert!(retired.is_none());

        drop(shared);

        {
            let registry = file_cache_registry().lock();
            let entry = registry.get(&key).expect("writeback anchor must remain");
            assert!(!entry.has_live_shared());
            assert!(entry.writeback_anchor.is_some());
        }
        let retired = file_cache_registry().lock().remove(&key);
        drop(retired);
    }
}
