//! Open options, open results and write placement.

use super::*;

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
    Dir(OpenedDirectory),
}

/// One opened directory, retaining a provider-created OFD-private directory
/// operation object when the backend needs one (FUSE/NFS).
pub struct OpenedDirectory {
    pub(super) location: Location,
    pub(super) handle: Option<Arc<dyn DirNodeOps>>,
}

impl OpenedDirectory {
    pub fn location(&self) -> &Location {
        &self.location
    }
    pub fn into_parts(self) -> (Location, Option<Arc<dyn DirNodeOps>>) {
        (self.location, self.handle)
    }
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
            Self::Dir(dir) => Ok(dir.location),
            Self::File(_) => Err(VfsError::NotADirectory),
        }
    }

    /// Extracts the underlying [`Location`] regardless of variant.
    pub fn into_location(self) -> Location {
        match self {
            Self::File(file) => file.location().clone(),
            Self::Dir(dir) => dir.location,
        }
    }
}

/// Options and flags which can be used to configure how a file is opened.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    // generic
    pub(super) read: bool,
    pub(super) write: bool,
    pub(super) append: bool,
    pub(super) truncate: bool,
    pub(super) create: bool,
    pub(super) create_new: bool,
    pub(super) directory: bool,
    pub(super) no_follow: bool,
    pub(super) direct: bool,
    pub(super) no_atime: bool,
    pub(super) user: Option<(u32, u32)>,
    pub(super) path: bool,
    pub(super) no_data: bool,
    pub(super) node_type: NodeType,
    // system-specific
    pub(super) mode: u32,
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

    pub(super) fn _open(&self, loc: Location, apply_truncate: bool) -> VfsResult<OpenResult> {
        let flags = self.to_flags()?;
        let mut open_handle: Option<Arc<dyn FileNodeOps>> = None;
        let mut open_dir_handle: Option<Arc<dyn DirNodeOps>> = None;

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
            if flags.contains(FileFlags::WRITE) || self.truncate {
                const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
                const FS_XFLAG_APPEND: u64 = 0x0000_0010;
                match loc.get_file_attr() {
                    Ok(attr) => {
                        if attr.xflags & FS_XFLAG_IMMUTABLE != 0 {
                            return Err(VfsError::OperationNotPermitted);
                        }
                        if attr.xflags & FS_XFLAG_APPEND != 0
                            && (self.truncate || (flags.contains(FileFlags::WRITE) && !self.append))
                        {
                            return Err(VfsError::OperationNotPermitted);
                        }
                    }
                    Err(VfsError::OperationNotSupported) => {}
                    Err(error) => return Err(error),
                }
            }
            loc.open(
                flags.contains(FileFlags::READ),
                flags.contains(FileFlags::WRITE),
            )?;
            // Stateful providers return an OFD-private operation object here.
            // Keep page-cache identity on `loc`; only protocol operations use
            // this handle, so two opens can never exchange remote file handles.
            if loc.is_dir() {
                open_dir_handle = loc.entry().as_dir()?.open_handle(flags.bits() as u32)?;
            } else {
                open_handle = loc.entry().as_file()?.open_handle(
                    flags.contains(FileFlags::READ),
                    flags.contains(FileFlags::WRITE),
                    flags.bits() as u32,
                )?;
            }
        }
        Ok(if loc.is_dir() {
            OpenResult::Dir(OpenedDirectory {
                location: loc,
                handle: open_dir_handle,
            })
        } else {
            // TODO(mivik): is this correct?
            let non_cacheable_type = matches!(
                loc.metadata()?.node_type,
                NodeType::CharacterDevice | NodeType::Fifo | NodeType::Socket
            );

            let direct = non_cacheable_type
                || self.path
                || self.direct
                || open_handle.is_some()
                || loc.flags().contains(NodeFlags::NON_CACHEABLE);
            // Public `OpenOptions::truncate` reaches backend `set_len`
            // before there is a File facade.  Take the same stable native
            // mutation token here rather than leaving this route outside the
            // fileattr setter exclusion domain.
            let native_mutation = if self.truncate && apply_truncate {
                begin_native_location_mutation(&loc, false)?
            } else {
                None
            };
            let backend = if !direct || loc.flags().contains(NodeFlags::ALWAYS_CACHE) {
                FileBackend::new_cached(loc)
            } else {
                FileBackend::new_direct(loc)
            };
            if self.truncate && apply_truncate {
                backend.set_len_with_held_native_mutation(
                    0,
                    held_native_writeback_gate(&native_mutation),
                )?;
            }
            OpenResult::File(File::new_with_open_handle_and_pending_open_truncate(
                backend,
                flags,
                open_handle,
                self.truncate && !apply_truncate,
            ))
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
    /// explicitly commit it through [`File::commit_deferred_open_truncate`].
    pub fn open_loc_deferred_truncate(&self, loc: Location) -> VfsResult<OpenResult> {
        if !self.is_valid() {
            return Err(VfsError::InvalidInput);
        }
        self._open(loc, false)
    }

    /// Opens a file at the given path relative to the provided [`FsContext`].
    pub fn open(&self, context: &FsContext, path: impl AsRef<FsPath>) -> VfsResult<OpenResult> {
        self.open_with_admission(context, path, &mut |_| Ok(()))
    }

    /// Opens a file while admitting every directory traversed by path lookup.
    pub fn open_with_admission<F>(
        &self,
        context: &FsContext,
        path: impl AsRef<FsPath>,
        admission: &mut F,
    ) -> VfsResult<OpenResult>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        let mut allow_create =
            |_dir: &Location, _name: &FsName, _options: &mut axfs_ng_vfs::OpenOptions| Ok(());
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
        path: impl AsRef<FsPath>,
        admission: &mut F,
        create_admission: &mut C,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &FsName, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
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
                initial_attributes: Default::default(),
            },
            !self.no_follow,
            admission,
            create_admission,
        )
    }

    pub fn resolve_location_with_policy<F, C, P>(
        &self,
        context: &FsContext,
        path: impl AsRef<FsPath>,
        admission: &mut F,
        create_admission: &mut C,
        policy: &mut P,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &FsName, &mut axfs_ng_vfs::OpenOptions) -> VfsResult<()> + ?Sized,
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
                initial_attributes: Default::default(),
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

pub(super) const PAGE_SIZE: usize = 4096;

pub(super) fn page_range(page: u64, count: u64) -> Range<u64> {
    let start = page.saturating_mul(PAGE_SIZE as u64);
    let end = start.saturating_add(count.saturating_mul(PAGE_SIZE as u64));
    start..end.max(start.saturating_add(1))
}
/// Maximum sequential-read readahead window in pages.
pub(super) const READAHEAD_PAGES: usize = 64;
/// Default page-cache window used by one file-backed mmap fault.
pub const MMAP_READAHEAD_PAGES: usize = READAHEAD_PAGES;
/// VM_SEQ_READ keeps a larger forward window than ordinary mmap read-around.
pub const MMAP_SEQUENTIAL_READAHEAD_PAGES: usize = READAHEAD_PAGES * 2;
pub(super) const FADVISE_READAHEAD_QUEUE_CAPACITY: usize = 16;
/// A WILLNEED worker must not retain an unbounded file/cache lifetime after
/// the syscall returned.  The advice is best effort, so service a bounded
/// prefix of each request and let later advice/read traffic extend it.
pub(super) const FADVISE_WILLNEED_MAX_PAGES: u64 = 64;
pub(super) const MAX_DIRTY_WRITEBACK_PAGES: usize = 64;
pub(super) const IRQ_FIRST_DIRTY_WRITEBACK_PAGES: usize = 8;
pub(super) const DIRTY_WRITEBACK_SEGMENT_PAGES: usize = 16;
pub(super) const ALIGNED_BYPASS_CHUNK: usize = 64 * 1024;
pub(super) const CLOSED_FILE_CACHE_RETAIN_MAX_PAGES: usize = 1024;
/// Bound every system-wide cache walk so memory reporting and pressure work
/// cannot turn one very large inode registry into an unbounded critical path.
pub(super) const GLOBAL_FILE_CACHE_SCAN_LIMIT: usize = 64;
/// Share one reclaim pass across active inodes instead of draining the first
/// cache found in registry order.
pub(super) const GLOBAL_FILE_CACHE_RECLAIM_PER_FILE: usize = 16;
/// Total page inspections allowed for one inode in one reclaim pass.  This is
/// shared by every successful removal so an ineligible LRU cannot be rescanned
/// once per target page.
pub(super) const GLOBAL_FILE_CACHE_RECLAIM_SCAN_PER_FILE: usize = 128;
/// A lock-external cache insertion may race a competing filler after every
/// successful reclaim. Bound that contention loop so an advisory caller never
/// turns repeated real reclaim progress into an unbounded eviction storm.
pub(super) const LOCK_EXTERNAL_INSERT_RECLAIM_RETRY_LIMIT: usize = 64;
/// Bound the number of pages inspected per inode while estimating
/// MemAvailable.  Truncation only under-estimates reclaimable memory.
pub(super) const GLOBAL_FILE_CACHE_ESTIMATE_PER_FILE: usize = 128;
/// One shared nonresident domain is used when no memcg hierarchy exists.
/// Keeping this budget global prevents a many-inode workload from retaining an
/// arbitrary fixed number of shadows per inode.
pub(super) const MIN_FILE_CACHE_SHADOW_PAGES: usize = 64;
