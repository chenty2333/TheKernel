use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
#[cfg(feature = "times")]
use core::sync::atomic::AtomicU8;
use core::{
    hint::spin_loop,
    num::NonZeroUsize,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axalloc::{UsageKind, global_allocator};
use axdriver::{AsyncBlockWaitPolicy, virtio_async_block_enabled, virtio_async_block_wait_policy};
#[cfg(feature = "times")]
use axfs_ng_vfs::MetadataUpdate;
use axfs_ng_vfs::{
    FileNode, FilesystemOps, Location, Mountpoint, NodeFlags, NodePermission, NodeType, VfsError,
    VfsResult, WeakDirEntry, WritebackAnchor, path::Path,
};
use axhal::mem::{PhysAddr, VirtAddr, total_ram_size, virt_to_phys};
use axio::{SeekFrom, prelude::*};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
#[cfg(target_os = "none")]
use axsync::Mutex;
use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use lru::LruCache;
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
        let mut allow_create = |_dir: &Location, _options: &mut axfs_ng_vfs::OpenOptions| Ok(());
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
    /// for the whole operation. `create_admission` is invoked on the actual
    /// directory immediately before a missing final component may be created.
    pub fn resolve_location_with_admission<F, C>(
        &self,
        context: &FsContext,
        path: impl AsRef<Path>,
        admission: &mut F,
        create_admission: &mut C,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
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
        C: FnMut(&Location, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
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
/// Maximum sequential-read readahead window in pages.
const READAHEAD_PAGES: usize = 64;
const MAX_DIRTY_WRITEBACK_PAGES: usize = 64;
const IRQ_FIRST_DIRTY_WRITEBACK_PAGES: usize = 8;
const DIRTY_WRITEBACK_SEGMENT_PAGES: usize = 16;
const IN_MEMORY_PAGE_CACHE_PAGES: usize = 1024;
const ALIGNED_BYPASS_CHUNK: usize = 64 * 1024;
const CLOSED_FILE_CACHE_RETAIN_MAX_PAGES: usize = 1024;
type CachedFileRegistryKey = (u64, u64);
static FILE_CACHE_REGISTRY: Once<Mutex<BTreeMap<CachedFileRegistryKey, FileUserData>>> =
    Once::new();
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
    (location.mountpoint().device(), location.inode())
}

fn cached_file_is_in_memory(location: &Location) -> bool {
    location.flags().contains(NodeFlags::ALWAYS_CACHE) || location.filesystem().name() == "tmpfs"
}

fn cached_file_shared_for_location(location: &Location) -> Option<Arc<CachedFileShared>> {
    let key = cached_file_registry_key(location);
    let registry_shared = {
        let registry = file_cache_registry().lock();
        registry.get(&key).and_then(FileUserData::shared)
    };
    registry_shared.or_else(|| {
        location
            .user_data()
            .get::<FileUserData>()
            .and_then(|it| it.shared())
    })
}

fn cached_file_shared_for_location_or_create(location: &Location) -> Arc<CachedFileShared> {
    let key = cached_file_registry_key(location);
    let user_data_shared = location
        .user_data()
        .get::<FileUserData>()
        .and_then(|it| it.shared());
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

        if let Some(shared) = user_data_shared {
            let retired_entry = registry.insert(key, FileUserData::new(location, &shared));
            break 'registry (shared, retired_entry, released_retained, false);
        }

        let shared = Arc::new(CachedFileShared::new(
            key,
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
        registry.remove(&(device, inode))
    };
    drop(retired);
}

/// Prunes dead cache registry entries for a released inode.
pub fn prune_dead_cached_file_registry_entries_for_inode(inode: u64) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let dead_keys = registry
            .iter()
            .filter_map(|(key @ (_, entry_inode), entry)| {
                (*entry_inode == inode && !entry.has_live_shared()).then_some(*key)
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

    let pages = cached_file_page_count(shared);
    if pages == 0 {
        return false;
    }
    release_cached_file_writeback_anchor_if_clean(shared);

    let key = cached_file_registry_key(location);
    loop {
        let decision = {
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
                ClosedFileCacheRetentionDecision::Retained(retired)
            } else {
                ClosedFileCacheRetentionDecision::Trim(closed_file_cache_trim_candidates(
                    &registry,
                    key,
                    current_without_entry,
                    pages,
                ))
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

/// Flushes and drops cached pages before backend storage is changed out-of-band.
pub fn sync_and_invalidate_cached_file_pages(location: &Location) -> VfsResult<()> {
    let shared = cached_file_shared_for_location(location);
    if let Some(shared) = shared {
        let _writeback_guard = shared.writeback_lock.write();
        wait_for_all_writeback_clear(&shared);
        let file = location.entry().as_file()?;
        loop {
            let Some((pn, mut page)) = ({
                let mut cache = shared.page_cache.lock();
                pop_unpinned_lru_page(&mut cache)?
            }) else {
                break;
            };
            let _ = writeback_cached_page(&shared, file, pn, &mut page)?;
        }
        release_cached_file_writeback_anchor_if_clean(&shared);
        release_closed_cached_file_retention(location);
    }
    Ok(())
}

fn discard_cached_pages(shared: &CachedFileShared) {
    let mut guard = shared.page_cache.lock();
    while let Some((_pn, mut page)) = pop_unpinned_lru_page(&mut guard).unwrap_or(None) {
        page.clear_dirty();
    }
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
        return Ok(cache.pop_lru());
    }
    if limit == 0 {
        Ok(None)
    } else {
        Err(VfsError::ResourceBusy)
    }
}

fn pop_unused_readahead_lru_page(cache: &mut LruCache<u32, PageCache>) -> Option<(u32, PageCache)> {
    let Some((_pn, page)) = cache.peek_lru() else {
        return None;
    };
    if !page.is_unused_prefetched() {
        return None;
    }
    let popped = cache.pop_lru();
    if popped.is_some() {
        record_readahead_retired_unused_page();
    }
    popped
}

/// Marks cached pages for an inode whose final directory entry is being removed.
pub fn mark_cached_file_unlinked(location: &Location) {
    if let Some(shared) = cached_file_shared_for_location(location) {
        shared.unlinked.store(true, Ordering::Release);
        if shared.open_handles.load(Ordering::Acquire) == 0 {
            discard_cached_pages(&shared);
            release_cached_file_writeback_anchor_if_clean(&shared);
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

/// A single page-sized cache entry backed by a physical page.
#[derive(Debug)]
pub struct PageCache {
    addr: VirtAddr,
    dirty: bool,
    prefetched: bool,
    pins: u32,
    writeback: u32,
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
            prefetched: false,
            pins: 0,
            writeback: 0,
        })
    }

    /// Returns the physical address of this page.
    pub fn paddr(&self) -> PhysAddr {
        virt_to_phys(self.addr)
    }

    /// Marks this page as dirty so it will be flushed on eviction.
    pub fn mark_dirty(&mut self) {
        self.prefetched = false;
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

    fn is_unused_prefetched(&self) -> bool {
        self.prefetched && !self.dirty && !self.is_pinned() && !self.is_writeback()
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
    }
}

/// A short-lived guard that prevents a cached file page from being evicted.
pub struct CachedFilePagePin {
    cache: CachedFile,
    pn: u32,
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
        page.unpin();
    }
}

/// A conservative preparation window for file-backed user I/O pins.
pub struct CachedFilePinWindow {
    shared: Arc<CachedFileShared>,
}

impl Drop for CachedFilePinWindow {
    fn drop(&mut self) {
        self.shared
            .user_io_pin_windows
            .fetch_sub(1, Ordering::AcqRel);
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

struct DirtyWritebackPage {
    pn: u32,
    data: Vec<u8>,
}

struct DirtyWritebackRun {
    page_start: u64,
    bytes: usize,
    pages: Vec<DirtyWritebackPage>,
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
) -> Option<DirtyWritebackRun> {
    let first_pn = *dirty_pages.first()?;
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
        return None;
    }

    let listeners = evict_listeners_snapshot(shared);
    let mut pages: Vec<DirtyWritebackPage> = Vec::with_capacity(dirty_pages.len());
    for (idx, pn) in dirty_pages.iter().enumerate() {
        let Some(page) = guard.get_mut(pn) else {
            continue;
        };
        for listener in &listeners {
            listener(*pn, page);
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

    (!pages.is_empty()).then_some(DirtyWritebackRun {
        page_start,
        bytes: max_len,
        pages,
    })
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

fn clear_flushed_dirty_run(shared: &CachedFileShared, run: &DirtyWritebackRun) {
    let mut guard = shared.page_cache.lock();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            continue;
        };
        if page.is_dirty() && page.data().get(..written.data.len()) == Some(written.data.as_slice())
        {
            page.clear_dirty();
        }
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

fn record_readahead_pressure_skip() {
    record_cached_file_counter(&READAHEAD_PRESSURE_SKIPS, 1);
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

fn flush_dirty_page_list_locked(
    shared: &CachedFileShared,
    file: &FileNode,
    mut dirty_pages: Vec<u32>,
    range_flush: bool,
) -> VfsResult<()> {
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
                    let mut used_async_submit = false;
                    let write_result =
                        match file.try_write_at_vectored_async(&slices, run.page_start) {
                            Ok(Some(written)) => {
                                used_async_submit = true;
                                Ok(written)
                            }
                            Ok(None) => file.write_at_vectored(&slices, run.page_start),
                            Err(err) => Err(err),
                        };
                    match write_result {
                        Ok(written) if written == run.bytes => {
                            record_dirty_writeback(range_flush, run.pages.len(), run.bytes, true);
                            record_async_dirty_flush_sg(run.pages.len());
                            if used_async_submit {
                                record_async_dirty_flush_sg_async_submit(run.pages.len());
                            }
                            finish_sg_dirty_writeback_run(shared, &run, true);
                        }
                        Ok(_) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(VfsError::Io);
                        }
                        Err(err) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(err);
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
                    continue;
                }
                DirtySgWritebackBegin::Fallback => {
                    record_async_dirty_flush_bounce_fallback();
                }
            }
        }

        let run = {
            let mut guard = shared.page_cache.lock();
            copy_dirty_writeback_run(shared, &mut guard, &dirty_pages[start..end], file_len)
        };
        let Some(run) = run else {
            start = end;
            continue;
        };

        let segments = build_dirty_writeback_segments(&run);
        let slices = segments.iter().map(Vec::as_slice).collect::<Vec<_>>();
        match file.write_at_vectored(&slices, run.page_start) {
            Ok(written) if written == run.bytes => {
                record_dirty_writeback(range_flush, run.pages.len(), run.bytes, async_enabled);
                clear_flushed_dirty_run(shared, &run);
            }
            Ok(_) => {
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(VfsError::Io);
            }
            Err(err) => {
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(err);
            }
        }
        start = end;
    }
    release_cached_file_writeback_anchor_if_clean(shared);
    Ok(())
}

fn flush_dirty_page_list(
    shared: &CachedFileShared,
    file: &FileNode,
    dirty_pages: Vec<u32>,
    range_flush: bool,
) -> VfsResult<()> {
    let _writeback_guard = shared.writeback_lock.read();
    flush_dirty_page_list_locked(shared, file, dirty_pages, range_flush)
}

fn flush_dirty_cache_shared_locked(shared: &CachedFileShared, file: &FileNode) -> VfsResult<()> {
    let dirty_pages = {
        let guard = shared.page_cache.lock();
        guard
            .iter()
            .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
            .collect::<Vec<_>>()
    };
    flush_dirty_page_list_locked(shared, file, dirty_pages, false)
}

fn flush_dirty_cache_shared(shared: &CachedFileShared, file: &FileNode) -> VfsResult<()> {
    let _writeback_guard = shared.writeback_lock.read();
    flush_dirty_cache_shared_locked(shared, file)
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
    /// Registry slot owned weakly by this shared state. Final release removes
    /// it only when both this key and this allocation still match.
    registry_key: CachedFileRegistryKey,
    page_cache: Mutex<LruCache<u32, PageCache>>,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    unlinked: AtomicBool,
    open_handles: AtomicUsize,
    user_io_pin_windows: AtomicUsize,
    /// Serializes cached page-cache users with direct-I/O cache drains.
    direct_io_lock: RwLock<()>,
    /// Serializes dirty writeback with truncate/cache length transitions.
    writeback_lock: RwLock<()>,
    /// Serializes O_APPEND transaction boundaries across handles for this inode.
    append_lock: RwLock<()>,
}

impl CachedFileShared {
    pub fn new(registry_key: CachedFileRegistryKey, in_memory: bool) -> Self {
        let capacity = if in_memory {
            in_memory_page_cache_capacity()
        } else {
            per_file_page_cache_capacity()
        };
        Self {
            registry_key,
            page_cache: Mutex::new(new_bounded_page_cache_store(capacity)),
            evict_listeners: Mutex::new(LinkedList::default()),
            unlinked: AtomicBool::new(false),
            open_handles: AtomicUsize::new(0),
            user_io_pin_windows: AtomicUsize::new(0),
            direct_io_lock: RwLock::new(()),
            writeback_lock: RwLock::new(()),
            append_lock: RwLock::new(()),
        }
    }
}

impl Drop for CachedFileShared {
    fn drop(&mut self) {
        // Final Arc release makes the registered Weak impossible to upgrade.
        // The pointer check prevents a stale release from deleting a newer
        // shared state installed for the same filesystem/inode key.
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
    shared: Weak<CachedFileShared>,
    retained: Option<Arc<CachedFileShared>>,
    writeback_anchor: Option<WritebackAnchor>,
    retained_pages: usize,
    retained_epoch: u64,
    mountpoint: Weak<Mountpoint>,
    entry: WeakDirEntry,
}

impl FileUserData {
    fn new(location: &Location, shared: &Arc<CachedFileShared>) -> Self {
        Self {
            shared: Arc::downgrade(shared),
            retained: None,
            writeback_anchor: None,
            retained_pages: 0,
            retained_epoch: 0,
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry().downgrade(),
        }
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

    /// Returns `true` if both handles refer to the same shared state.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Opens a short preparation window for pinning file-backed user I/O pages.
    ///
    /// While this window is active, direct cache-draining I/O and LRU evictions
    /// are conservatively rejected for this cached file. Precise page pins take
    /// over once the caller has identified the exact cached pages.
    pub fn begin_user_io_pin_window(&self) -> CachedFilePinWindow {
        self.shared
            .user_io_pin_windows
            .fetch_add(1, Ordering::AcqRel);
        CachedFilePinWindow {
            shared: self.shared.clone(),
        }
    }

    /// Pins an already cached page if it still maps to `paddr`.
    pub fn pin_cached_page_by_paddr(
        &self,
        pn: u32,
        paddr: PhysAddr,
    ) -> VfsResult<CachedFilePagePin> {
        let mut guard = self.shared.page_cache.lock();
        let Some(page) = guard.get_mut(&pn) else {
            return Err(VfsError::BadAddress);
        };
        if page.paddr() != paddr {
            return Err(VfsError::BadAddress);
        }
        page.pin()?;
        Ok(CachedFilePagePin {
            cache: self.clone(),
            pn,
        })
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
        if page.is_pinned() {
            return Err(VfsError::ResourceBusy);
        }
        writeback_cached_page(&self.shared, file, pn, page)
    }

    fn pop_lru_page(&self) -> VfsResult<Option<(u32, PageCache)>> {
        pop_unpinned_lru_page(&mut self.shared.page_cache.lock())
    }

    fn pop_cached_page(&self, pn: u32) -> VfsResult<Option<PageCache>> {
        let mut guard = self.shared.page_cache.lock();
        if guard.get(&pn).is_some_and(PageCache::is_pinned) {
            return Err(VfsError::ResourceBusy);
        }
        Ok(guard.pop(&pn))
    }

    fn drain_cache(&self, file: &FileNode) -> VfsResult<()> {
        self.flush_dirty_cache(file)?;
        while let Some((pn, mut page)) = self.pop_lru_page()? {
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
        flush_dirty_cache_shared(&self.shared, file)
    }

    fn flush_dirty_cache_range(&self, file: &FileNode, pages: Range<u32>) -> VfsResult<()> {
        let dirty_pages = {
            let mut guard = self.shared.page_cache.lock();
            pages
                .filter(|pn| guard.get(pn).is_some_and(PageCache::is_dirty))
                .collect::<Vec<_>>()
        };
        flush_dirty_page_list(&self.shared, file, dirty_pages, true)
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

    fn invalidate_cached_range(&self, file: &FileNode, pages: Range<u32>) -> VfsResult<usize> {
        let keys = {
            let guard = self.shared.page_cache.lock();
            pages.filter(|pn| guard.contains(pn)).collect::<Vec<_>>()
        };
        let count = keys.len();
        for pn in keys {
            if let Some(mut page) = self.pop_cached_page(pn)? {
                let _ = self.evict_cache(file, pn, &mut page)?;
            }
        }
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
                file.try_read_at_vectored_async(&mut bufs, current)?
            };
            let read = match async_read {
                Some(read) => read,
                None => file.read_at(&mut chunk[..limit], current)?,
            };
            if read == 0 {
                break;
            }
            let written = dst.write(&chunk[..read])?;
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
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let read = match file.try_read_at_vectored_async(dst, offset)? {
            Some(read) => read,
            None => file.read_at_vectored(dst, offset)?,
        };
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
        if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.flush_dirty_cache_range(file, pages.clone())?;
        self.invalidate_cached_range(file, pages.clone())?;

        let mut total = 0;
        let mut current = offset;
        let mut chunk = vec![0_u8; ALIGNED_BYPASS_CHUNK.min(len).max(PAGE_SIZE)];
        while total < len && src.remaining() > 0 {
            let limit = (len - total).min(chunk.len()).min(src.remaining());
            let read = src.read(&mut chunk[..limit])?;
            if read == 0 {
                break;
            }
            let written = file.write_at(&chunk[..read], current)?;
            if written == 0 {
                break;
            }
            total += written;
            current += written as u64;
            if written < read {
                break;
            }
        }

        self.invalidate_cached_range(file, pages)?;
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
        if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.flush_dirty_cache_range(file, pages.clone())?;
        self.invalidate_cached_range(file, pages.clone())?;

        let written = file.write_at(src, offset)?;

        self.invalidate_cached_range(file, pages)?;
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
        if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.flush_dirty_cache_range(file, pages.clone())?;
        self.invalidate_cached_range(file, pages.clone())?;

        let written = match file.try_write_at_vectored_async(src, offset)? {
            Some(written) => written,
            None => file.write_at_vectored(src, offset)?,
        };

        self.invalidate_cached_range(file, pages)?;
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
        self.ensure_page_cached_with(file, cache, pn, true)
    }

    fn ensure_page_cached_with(
        &self,
        file: &FileNode,
        cache: &mut LruCache<u32, PageCache>,
        pn: u32,
        load_from_file: bool,
    ) -> VfsResult<Option<EvictedPage>> {
        if let Some(page) = cache.get_mut(&pn) {
            if load_from_file && page.clear_prefetched() {
                record_readahead_hit();
            } else if !load_from_file {
                page.clear_prefetched();
            }
            return Ok(None);
        }
        let readahead_enabled = load_from_file && cached_readahead_enabled();
        if readahead_enabled {
            record_readahead_miss();
        }
        let cap = cache.cap().get();
        let mut evicted = None;
        // Make room for the requested page. The caller may receive this
        // EvictedPage; any further evictions done for readahead below are
        // written back and dropped.
        if cache.len() >= cap {
            if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
                return Err(VfsError::ResourceBusy);
            }
            if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                let _ = self.evict_cache(file, epn, &mut epage)?;
            } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                let retain_page = self.evict_cache(file, epn, &mut epage)?;
                let page = retain_page.then_some(epage);
                evicted = Some(EvictedPage {
                    pn: epn,
                    _page: page,
                });
            }
        }

        if !load_from_file {
            let mut page = PageCache::new()?;
            page.data().fill(0);
            cache.put(pn, page);
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
        let async_page_fill = {
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

            let mut page = PageCache::new()?;
            let data = page.data();
            data.fill(0);
            let n0 = read.min(PAGE_SIZE);
            data[..n0].copy_from_slice(&buf[..n0]);
            cache.put(pn, page);

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
                    if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
                        record_readahead_pressure_skip();
                        break;
                    }
                    if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                        let _ = self.evict_cache(file, epn, &mut epage)?;
                    } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                        let _ = self.evict_cache(file, epn, &mut epage)?;
                    }
                }
                let mut np = PageCache::new()?;
                let nd = np.data();
                nd.fill(0);
                let chunk_end = (off + PAGE_SIZE).min(read);
                nd[..chunk_end - off].copy_from_slice(&buf[off..chunk_end]);
                np.mark_prefetched();
                cache.put(next_pn, np);
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
            let mut page = PageCache::new()?;
            page.data().fill(0);
            pages.push(page);
        }
        let read = {
            let mut bufs = pages.iter_mut().map(|page| page.data()).collect::<Vec<_>>();
            file.read_at_vectored(&mut bufs, base)?
        };

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
                if self.shared.user_io_pin_windows.load(Ordering::Acquire) != 0 {
                    record_readahead_pressure_skip();
                    break;
                }
                if let Some((epn, mut epage)) = pop_unused_readahead_lru_page(cache) {
                    let _ = self.evict_cache(file, epn, &mut epage)?;
                } else if let Some((epn, mut epage)) = pop_unpinned_lru_page(cache)? {
                    let _ = self.evict_cache(file, epn, &mut epage)?;
                }
            }
            if off < read {
                async_filled_pages += 1;
            }
            let mut page = page;
            if i > 0 {
                page.mark_prefetched();
            }
            cache.put(target_pn, page);
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
        loop {
            let mut guard = self.shared.page_cache.lock();
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
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
    ) -> VfsResult<T> {
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
                self.ensure_page_cached_with(file, &mut guard, pn, load_from_file)?;
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

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let end = (offset + dst.remaining_mut() as u64).min(len);
        if end <= offset {
            return Ok(0);
        }
        if let Some(read) =
            self.try_read_aligned_bypass(&mut dst, offset, (end - offset) as usize)?
        {
            return Ok(read);
        }
        let _direct_guard = self.shared.direct_io_lock.read();
        self.with_pages(
            offset..end,
            |_| Ok(0),
            |_, _| true,
            |read, page, _page_start, range| {
                let len = range.end - range.start;
                dst.write(&page.data()[range.start..range.end])?;
                Ok(read + len)
            },
            false,
        )
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
            let read = self.read_at_slice(buf, current)?;
            total += read;
            current += read as u64;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn write_at_locked(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let end = offset + buf.remaining() as u64;
        let old_len = self.inner.entry().as_file()?.len()?;
        let written = self.with_pages(
            offset..end,
            |file| {
                if end > old_len {
                    file.set_len(end)?;
                }
                Ok(0)
            },
            |page_start, range| {
                !(range.start == 0 && range.end == PAGE_SIZE) && page_start < old_len
            },
            |written, page, _page_start, range| {
                let len = range.end - range.start;
                buf.read(&mut page.data()[range.start..range.end])?;
                page.mark_dirty();
                Ok(written + len)
            },
            true,
        )?;
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
            let written = self.write_at_slice(buf, current)?;
            total += written;
            current += written as u64;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
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
            let written = self.write_at_locked(buf, end)?;
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
        let _direct_guard = self.shared.direct_io_lock.read();
        let _writeback_guard = self.shared.writeback_lock.write();
        wait_for_all_writeback_clear(&self.shared);
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
        if self.in_memory && old_len > len {
            flush_dirty_cache_shared_locked(&self.shared, file)?;
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
                if let Some(mut page) = self.pop_cached_page(pn)? {
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
            self.discard_cache();
            release_unlinked_cached_file_registry_ownership(&self.inner, &self.shared);
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

impl FileBackend {
    const DIRECT_IO_CHUNK: usize = ALIGNED_BYPASS_CHUNK;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    fn direct_io_shared(location: &Location) -> Arc<CachedFileShared> {
        cached_file_shared_for_location_or_create(location)
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

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_slice(dst, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let async_read = {
                    let mut bufs = [&mut *dst];
                    file.try_read_at_vectored_async(&mut bufs, offset)?
                };
                match async_read {
                    Some(read) => Ok(read),
                    None => file.read_at(dst, offset),
                }
            }
        }
    }

    pub fn read_at_vectored(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_vectored(dst, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                match file.try_read_at_vectored_async(dst, offset)? {
                    Some(read) => Ok(read),
                    None => file.read_at_vectored(dst, offset),
                }
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

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_slice(src, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let written = file.write_at(src, offset)?;
                if written > 0 {
                    Self::sync_direct_cache(loc)?;
                }
                Ok(written)
            }
        }
    }

    pub fn write_at_vectored(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_vectored(src, offset),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _guard = shared.direct_io_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
                let written = match file.try_write_at_vectored_async(src, offset)? {
                    Some(written) => written,
                    None => file.write_at_vectored(src, offset)?,
                };
                if written > 0 {
                    Self::sync_direct_cache(loc)?;
                }
                Ok(written)
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

    /// Appends one scatter list without releasing the inode append domain
    /// between nonempty elements.
    pub fn append_vectored(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append_vectored(src),
            Self::Direct(loc) => {
                let shared = Self::direct_io_shared(loc);
                let _direct_guard = shared.direct_io_lock.write();
                let _append_guard = shared.append_lock.write();
                Self::sync_direct_cache(loc)?;
                let file = loc.entry().as_file()?;
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
                        let (written, new_end) = file.append(chunk)?;
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
        record_file_sync_request(data_only);
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
    /// Mutable open-file-description append status. Access admission remains
    /// in `flags`; toggling append must never manufacture write authority.
    append: AtomicBool,
    flags: FileFlags,
    /// Serializes operations which observe and later commit the current
    /// position without requiring the position spin lock to remain held while
    /// an external transfer consumer runs.
    position_transaction: Mutex<()>,
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

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        self.inner.location()
    }

    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
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

    fn write_at_end_with_new_end(&self, src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        let result = self.access(FileFlags::WRITE)?.append(src)?;
        #[cfg(feature = "times")]
        if result.0 > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(result)
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
    pub fn write_with_placement(
        &self,
        src: impl Read + IoBuf,
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
                self.write_at(src, *pos).inspect(|n| {
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
            self.write_at(src, 0)
        }
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
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        task::Context,
        time::Duration,
    };
    use std::{
        sync::{Barrier, Mutex as StdMutex, mpsc},
        thread,
    };

    use axfs_ng_vfs::{
        DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate,
        Mountpoint, NodeFlags, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError,
        VfsResult,
    };
    use axio::{Cursor, Seek, SeekFrom};
    use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
    use axsync::Mutex;

    use super::{
        CLOSED_FILE_CACHE_RETAINED_PAGES, CachedFile, CachedFileShared, File, FileBackend,
        FileFlags, FileUserData, OpenOptions, WritePlacement, cached_file_registry_key,
        cached_file_shared_for_location_or_create, file_cache_registry,
        release_unlinked_cached_file_registry_ownership,
    };

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
        append_calls: AtomicUsize,
        open_calls: AtomicUsize,
        inode_len: AtomicU64,
        append_limit: AtomicUsize,
        fail_append_call: AtomicUsize,
        yield_after_append: AtomicBool,
        append_markers: StdMutex<Vec<u8>>,
    }

    impl AppendTestState {
        fn new(inode_len: u64) -> Arc<Self> {
            Arc::new(Self {
                read_offsets: Mutex::new(Vec::new()),
                write_offsets: Mutex::new(Vec::new()),
                append_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
                inode_len: AtomicU64::new(inode_len),
                append_limit: AtomicUsize::new(usize::MAX),
                fail_append_call: AtomicUsize::new(usize::MAX),
                yield_after_append: AtomicBool::new(false),
                append_markers: StdMutex::new(Vec::new()),
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
                atime: Duration::ZERO,
                btime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
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
            self.state.read_offsets.lock().push(offset);
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
            self.state.write_offsets.lock().push(offset);
            if offset != 0 {
                return Err(VfsError::InvalidInput);
            }
            Ok(buf.len().min(2))
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
            self.state.inode_len.store(len, Ordering::Release);
            Ok(())
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
    fn explicit_end_ignores_non_append_default_for_all_write_forms() {
        let (file, state) =
            append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
        let mut src = Cursor::new(&b"ab"[..]);
        assert_eq!(
            file.write_with_placement(&mut src, WritePlacement::End),
            Ok(2)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 43);

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
            Err(VfsError::InvalidInput)
        );
        assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
        assert_eq!(state.inode_len.load(Ordering::Acquire), 43);
        assert_eq!(
            file.write_slice_with_placement(b"x", WritePlacement::Current),
            Ok(1)
        );
        assert_eq!(&*state.write_offsets.lock(), &[0]);
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
        assert_eq!(file.write_at_end(&mut src), Ok(2));
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
                atime: Duration::ZERO,
                btime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
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
