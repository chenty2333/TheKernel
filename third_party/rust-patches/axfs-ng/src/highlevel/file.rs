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
use axfs_ng_vfs::{
    FileNode, FilesystemOps, Location, MetadataUpdate, Mountpoint, NodeFlags, NodePermission,
    NodeType, VfsError, VfsResult, WeakDirEntry, WritebackAnchor, path::Path,
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
        let mut allow_create = |_dir: &Location| Ok(());
        let (loc, _created) = self.resolve_location_with_admission(
            context,
            path,
            admission,
            &mut allow_create,
        )?;
        self._open(loc)
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
        C: FnMut(&Location) -> VfsResult<()> + ?Sized,
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
/// Maximum sequential-read readahead window in pages.
const READAHEAD_PAGES: usize = 64;
const MAX_DIRTY_WRITEBACK_PAGES: usize = 64;
const IRQ_FIRST_DIRTY_WRITEBACK_PAGES: usize = 8;
const DIRTY_WRITEBACK_SEGMENT_PAGES: usize = 16;
const IN_MEMORY_PAGE_CACHE_PAGES: usize = 1024;
const ALIGNED_BYPASS_CHUNK: usize = 64 * 1024;
const CLOSED_FILE_CACHE_RETAIN_MAX_PAGES: usize = 1024;
static FILE_CACHE_REGISTRY: Once<Mutex<BTreeMap<(u64, u64), FileUserData>>> = Once::new();
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

        let shared = Arc::new(CachedFileShared::new(cached_file_is_in_memory(location)));
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

type CachedFileWritebackSnapshotEntry = ((u64, u64), Arc<CachedFileShared>, WritebackAnchor);

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
    key: (u64, u64),
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
    registry: &BTreeMap<(u64, u64), FileUserData>,
    preserve_key: (u64, u64),
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
    page_cache: Mutex<LruCache<u32, PageCache>>,
    evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    unlinked: AtomicBool,
    open_handles: AtomicUsize,
    user_io_pin_windows: AtomicUsize,
    /// Serializes cached page-cache users with direct-I/O cache drains.
    direct_io_lock: RwLock<()>,
    /// Serializes dirty writeback with truncate/cache length transitions.
    writeback_lock: RwLock<()>,
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
            open_handles: AtomicUsize::new(0),
            user_io_pin_windows: AtomicUsize::new(0),
            direct_io_lock: RwLock::new(()),
            writeback_lock: RwLock::new(()),
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
            release_closed_cached_file_retention(&self.inner);
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

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
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

    pub fn read_slice(&self, dst: &mut [u8]) -> axio::Result<usize> {
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_slice(dst, 0)
        }
    }

    pub fn read_vectored_slice(&self, dst: &mut [&mut [u8]]) -> axio::Result<usize> {
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            self.read_at_vectored_slice(dst, *pos).inspect(|n| {
                *pos += *n as u64;
            })
        } else {
            self.read_at_vectored_slice(dst, 0)
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

    pub fn write_slice(&self, src: &[u8]) -> axio::Result<usize> {
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
                self.write_at_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            }
        } else {
            self.write_at_slice(src, 0)
        }
    }

    pub fn write_vectored_slice(&self, src: &[&[u8]]) -> axio::Result<usize> {
        if let Some(pos) = self.position.as_ref() {
            let mut pos = pos.lock();
            if let Ok(f) = self.access(FileFlags::APPEND) {
                let mut total = 0usize;
                for buf in src.iter().copied() {
                    if buf.is_empty() {
                        continue;
                    }
                    let requested = buf.len();
                    let (written, new_size) = f.append(buf)?;
                    #[cfg(feature = "times")]
                    if written > 0 {
                        self.record_time_flags(2);
                        self.flush_times();
                    }
                    *pos = new_size;
                    total += written;
                    if written < requested || written == 0 {
                        break;
                    }
                }
                Ok(total)
            } else {
                self.write_at_vectored_slice(src, *pos).inspect(|n| {
                    *pos += *n as u64;
                })
            }
        } else {
            self.write_at_vectored_slice(src, 0)
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
