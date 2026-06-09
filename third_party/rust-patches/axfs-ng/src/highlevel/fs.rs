use alloc::{
    borrow::{Cow, ToOwned},
    collections::vec_deque::VecDeque,
    string::String,
    sync::Arc,
    vec::Vec,
};

use axfs_ng_vfs::{
    Location, Metadata, NodePermission, NodeType, VfsError, VfsResult,
    path::{Component, Components, Path, PathBuf},
};
use axio::{Read, Write};
use axsync::Mutex;
use spin::Once;

use super::File;

/// Maximum number of symlinks that will be followed during path resolution.
pub const SYMLINKS_MAX: usize = 40;

/// Global root filesystem context, initialized once during [`init_filesystems`](crate::init_filesystems).
pub static ROOT_FS_CONTEXT: Once<FsContext> = Once::new();
static SYMLINK_FOLLOW_POLICY: Once<fn(&Location) -> bool> = Once::new();
static ATIME_UPDATE_POLICY: Once<fn(&Location) -> bool> = Once::new();

scope_local::scope_local! {
    /// Task-local filesystem context, defaulting to a clone of [`ROOT_FS_CONTEXT`].
    pub static FS_CONTEXT: Arc<Mutex<FsContext>> =
        Arc::new(Mutex::new(
            ROOT_FS_CONTEXT
                .get()
                .expect("Root FS context not initialized")
                .clone(),
        ));
}

/// A single entry returned by [`FsContext::read_dir`].
pub struct ReadDirEntry {
    /// Entry name (file or directory name, not the full path).
    pub name: String,
    /// Inode number.
    pub ino: u64,
    /// Type of the node (file, directory, symlink, etc.).
    pub node_type: NodeType,
    /// Byte offset inside the directory (used for seeking).
    pub offset: u64,
}

/// Provides `std::fs`-like interface.
#[derive(Debug, Clone)]
pub struct FsContext {
    root_dir: Location,
    current_dir: Location,
}

impl FsContext {
    fn may_follow_symlink(loc: &Location) -> bool {
        SYMLINK_FOLLOW_POLICY.get().is_none_or(|policy| policy(loc))
    }

    pub(crate) fn should_update_atime(loc: &Location) -> bool {
        ATIME_UPDATE_POLICY.get().is_none_or(|policy| policy(loc))
    }

    /// Returns whether an absolute path resolves to this context's root entry.
    pub fn path_refers_to_root(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        path.is_absolute()
            && self
                .resolve_no_follow(path)
                .is_ok_and(|entry| entry.is_root())
    }

    /// Creates a new context with `root_dir` as both root and current directory.
    pub fn new(root_dir: Location) -> Self {
        Self {
            root_dir: root_dir.clone(),
            current_dir: root_dir,
        }
    }

    /// Returns a reference to the root directory.
    pub fn root_dir(&self) -> &Location {
        &self.root_dir
    }

    /// Returns a reference to the current working directory.
    pub fn current_dir(&self) -> &Location {
        &self.current_dir
    }

    /// Changes the current working directory to `current_dir`.
    pub fn set_current_dir(&mut self, current_dir: Location) -> VfsResult<()> {
        current_dir.check_is_dir()?;
        self.current_dir = current_dir;
        Ok(())
    }

    /// Returns a new context that shares the same root but uses `current_dir` as
    /// the working directory.
    pub fn with_current_dir(&self, current_dir: Location) -> VfsResult<Self> {
        current_dir.check_is_dir()?;
        Ok(Self {
            root_dir: self.root_dir.clone(),
            current_dir,
        })
    }

    /// Changes the root directory while preserving the current working
    /// directory, matching Linux `chroot(2)` semantics.
    pub fn set_root_dir(&mut self, root_dir: Location) -> VfsResult<()> {
        root_dir.check_is_dir()?;
        self.root_dir = root_dir;
        Ok(())
    }

    /// Attempts to resolve a possible symlink, at the current location (this
    /// assumes that `loc` is a child of current directory).
    pub fn try_resolve_symlink(
        &self,
        loc: Location,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        if loc.node_type() != NodeType::Symlink {
            return Ok(loc);
        }
        if !Self::may_follow_symlink(&loc) {
            return Err(VfsError::FilesystemLoop);
        }
        if *follow_count >= SYMLINKS_MAX {
            return Err(VfsError::FilesystemLoop);
        }
        *follow_count += 1;
        let target = loc.read_link()?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }
        self.resolve_components(PathBuf::from(target).components(), follow_count)
    }

    fn lookup(&self, dir: &Location, name: &str, follow_count: &mut usize) -> VfsResult<Location> {
        let loc = dir.lookup_no_follow(name)?;
        self.with_current_dir(dir.clone())?
            .try_resolve_symlink(loc, follow_count)
    }

    fn resolve_components(
        &self,
        components: Components,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        let mut dir = self.current_dir.clone();
        for comp in components {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    dir = dir.parent().unwrap_or_else(|| self.root_dir.clone());
                }
                Component::RootDir => {
                    dir = self.root_dir.clone();
                }
                Component::Normal(name) => {
                    dir = self.lookup(&dir, name, follow_count)?;
                }
            }
        }
        Ok(dir)
    }

    fn resolve_inner<'a>(
        &self,
        path: &'a Path,
        follow_count: &mut usize,
    ) -> VfsResult<(Location, Option<&'a str>)> {
        let entry_name = path.file_name();
        let mut components = path.components();
        if entry_name.is_some() {
            components.next_back();
        }
        let dir = self.resolve_components(components, follow_count)?;
        dir.check_is_dir()?;
        Ok((dir, entry_name))
    }

    /// Resolves a path starting from `current_dir`.
    pub fn resolve(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        let mut follow_count = 0;
        let (dir, name) = self.resolve_inner(path.as_ref(), &mut follow_count)?;
        match name {
            Some(name) => self.lookup(&dir, name, &mut follow_count),
            None => Ok(dir),
        }
    }

    /// Resolves a path starting from `current_dir` not following symlinks.
    pub fn resolve_no_follow(&self, path: impl AsRef<Path>) -> VfsResult<Location> {
        let (dir, name) = self.resolve_inner(path.as_ref(), &mut 0)?;
        match name {
            Some(name) => dir.lookup_no_follow(name),
            None => Ok(dir),
        }
    }

    /// Taking current node as root directory, resolves a path starting from
    /// `current_dir`.
    ///
    /// Returns `(parent_dir, entry_name)`, where `entry_name` is the name of
    /// the entry.
    pub fn resolve_parent<'a>(&self, path: &'a Path) -> VfsResult<(Location, Cow<'a, str>)> {
        let (dir, name) = self.resolve_inner(path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, Cow::Borrowed(name)))
        } else if let Some(parent) = dir.parent() {
            Ok((parent, Cow::Owned(dir.name().to_owned())))
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Resolves a path starting from `current_dir`, returning the parent
    /// directory and the name of the entry.
    ///
    /// This function requires that the entry does not exist and the parent
    /// exists. Note that, it does not perform an actual check to ensure the
    /// entry's non-existence. It simply raises an error if the entry name is
    /// not present in the path.
    pub fn resolve_nonexistent<'a>(&self, path: &'a Path) -> VfsResult<(Location, &'a str)> {
        let (dir, name) = self.resolve_inner(path, &mut 0)?;
        if let Some(name) = name {
            Ok((dir, name))
        } else if self.path_refers_to_root(path) {
            Err(VfsError::AlreadyExists)
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Retrieves metadata for the file.
    pub fn metadata(&self, path: impl AsRef<Path>) -> VfsResult<Metadata> {
        self.resolve(path)?.metadata()
    }

    /// Reads the entire contents of a file into a bytes vector.
    pub fn read(&self, path: impl AsRef<Path>) -> VfsResult<Vec<u8>> {
        let mut buf = Vec::new();
        let file = File::open(self, path.as_ref())?;
        (&file).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Reads the entire contents of a file into a string.
    pub fn read_to_string(&self, path: impl AsRef<Path>) -> VfsResult<String> {
        String::from_utf8(self.read(path)?).map_err(|_| VfsError::InvalidData)
    }

    /// Writes a slice as the entire contents of a file.
    ///
    /// This function will create a file if it does not exist, and will entirely
    /// replace its contents if it does.
    pub fn write(&self, path: impl AsRef<Path>, buf: impl AsRef<[u8]>) -> VfsResult<()> {
        let file = File::create(self, path.as_ref())?;
        (&file).write_all(buf.as_ref())?;
        Ok(())
    }

    /// Returns an iterator over the entries in a directory.
    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDir> {
        let dir = self.resolve(path)?;
        Ok(ReadDir {
            dir,
            buf: VecDeque::new(),
            offset: 0,
            ended: false,
        })
    }

    /// Removes a file from the filesystem.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(VfsError::IsADirectory)?
            .unlink(entry.name(), false)
    }

    /// Removes a directory from the filesystem.
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> VfsResult<()> {
        let entry = self.resolve_no_follow(path.as_ref())?;
        entry
            .parent()
            .ok_or(VfsError::ResourceBusy)?
            .unlink(entry.name(), true)
    }

    /// Renames a file or directory to a new name, replacing the original file
    /// if `to` already exists.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfsResult<()> {
        let from = from.as_ref();
        let to = to.as_ref();
        let src = self.resolve_no_follow(from);

        if self.path_refers_to_root(from) {
            if self.path_refers_to_root(to) {
                return Err(VfsError::ResourceBusy);
            }
            self.resolve_parent(to)?;
            return Err(VfsError::ResourceBusy);
        }

        if self.path_refers_to_root(to) {
            src?;
            return Err(VfsError::ResourceBusy);
        }

        let (src_dir, src_name) = self.resolve_parent(from)?;
        let (dst_dir, dst_name) = self.resolve_parent(to)?;
        src_dir.rename(&src_name, &dst_dir, &dst_name)
    }

    /// Creates a new, empty directory at the provided path.
    pub fn create_dir(&self, path: impl AsRef<Path>, mode: NodePermission) -> VfsResult<Location> {
        let (dir, name) = self.resolve_nonexistent(path.as_ref())?;
        dir.create(name, NodeType::Directory, mode)
    }

    /// Creates a new hard link on the filesystem.
    pub fn link(
        &self,
        old_path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let old = self.resolve(old_path.as_ref())?;
        let (new_dir, new_name) = self.resolve_nonexistent(new_path.as_ref())?;
        new_dir.link(new_name, &old)
    }

    /// Creates a new symbolic link on the filesystem.
    pub fn symlink(
        &self,
        target: impl AsRef<str>,
        link_path: impl AsRef<Path>,
    ) -> VfsResult<Location> {
        let (dir, name) = self.resolve_nonexistent(link_path.as_ref())?;
        if dir.lookup_no_follow(name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let symlink = dir.create(name, NodeType::Symlink, NodePermission::default())?;
        symlink.entry().as_file()?.set_symlink(target.as_ref())?;
        Ok(symlink)
    }

    /// Returns the canonical, absolute form of a path.
    pub fn canonicalize(&self, path: impl AsRef<Path>) -> VfsResult<PathBuf> {
        self.resolve(path.as_ref())?.absolute_path()
    }
}

/// Installs an optional symlink-follow policy used by path resolution.
///
/// The policy is evaluated only when the resolver is about to follow a
/// symlink. Returning `false` turns that lookup into `ELOOP`.
pub fn set_symlink_follow_policy(policy: fn(&Location) -> bool) {
    SYMLINK_FOLLOW_POLICY.call_once(|| policy);
}

/// Installs an optional access-time update policy used by high-level file I/O.
pub fn set_atime_update_policy(policy: fn(&Location) -> bool) {
    ATIME_UPDATE_POLICY.call_once(|| policy);
}

/// Iterator returned by [`FsContext::read_dir`].
pub struct ReadDir {
    dir: Location,
    buf: VecDeque<ReadDirEntry>,
    offset: u64,
    ended: bool,
}

impl ReadDir {
    /// Maximum number of entries to buffer per `read_dir` syscall.
    // TODO: tune this
    pub const BUF_SIZE: usize = 128;
}

impl Iterator for ReadDir {
    type Item = VfsResult<ReadDirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }

        if self.buf.is_empty() {
            self.buf.clear();
            let result = self.dir.read_dir(
                self.offset,
                &mut |name: &str, ino: u64, node_type: NodeType, offset: u64| {
                    self.buf.push_back(ReadDirEntry {
                        name: name.to_owned(),
                        ino,
                        node_type,
                        offset,
                    });
                    self.offset = offset;
                    self.buf.len() < Self::BUF_SIZE
                },
            );

            // We handle errors only if we didn't get any entries
            if self.buf.is_empty() {
                if let Err(err) = result {
                    return Some(Err(err));
                }
                self.ended = true;
                return None;
            }
        }

        self.buf.pop_front().map(Ok)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{any::Any, time::Duration};

    use axfs_ng_vfs::{
        DirEntry, DirEntrySink, DirNode, DirNodeOps, Filesystem, FilesystemOps, Metadata,
        MetadataUpdate, Mountpoint, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError,
        VfsResult, WeakDirEntry,
    };
    use spin::Once;

    use super::FsContext;

    struct TestFs {
        root: Once<DirEntry>,
    }

    impl TestFs {
        fn new() -> Arc<Self> {
            let fs = Arc::new(Self { root: Once::new() });
            let root = DirEntry::new_dir(
                {
                    let fs = fs.clone();
                    move |this| {
                        DirNode::new(Arc::new(TestDir {
                            fs: fs.clone(),
                            this,
                        }))
                    }
                },
                Reference::root(),
            );
            fs.root.call_once(|| root);
            fs
        }

        fn context() -> FsContext {
            let fs = Self::new();
            let mount = Mountpoint::new_root(&Filesystem::new(fs));
            FsContext::new(mount.root_location())
        }
    }

    impl FilesystemOps for TestFs {
        fn name(&self) -> &str {
            "testfs"
        }

        fn root_dir(&self) -> DirEntry {
            self.root.get().unwrap().clone()
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

    struct TestDir {
        fs: Arc<TestFs>,
        this: WeakDirEntry,
    }

    impl TestDir {
        fn child(&self, name: &str) -> DirEntry {
            let parent = self.this.upgrade();
            DirEntry::new_dir(
                {
                    let fs = self.fs.clone();
                    move |this| {
                        DirNode::new(Arc::new(TestDir {
                            fs: fs.clone(),
                            this,
                        }))
                    }
                },
                Reference::new(parent, name.to_owned()),
            )
        }
    }

    impl NodeOps for TestDir {
        fn inode(&self) -> u64 {
            1
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 1,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o755),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: Duration::ZERO,
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

    impl DirNodeOps for TestDir {
        fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            let mut read = 0;
            if offset == 0 && sink.accept(".", 1, NodeType::Directory, 1) {
                read += 1;
            }
            if offset <= 1 && sink.accept("..", 1, NodeType::Directory, 2) {
                read += 1;
            }
            Ok(read)
        }

        fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
            if name == "child" {
                Ok(self.child(name))
            } else {
                Err(VfsError::NotFound)
            }
        }

        fn create(
            &self,
            _name: &str,
            _node_type: NodeType,
            _permission: NodePermission,
        ) -> VfsResult<DirEntry> {
            Err(VfsError::Unsupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::Unsupported)
        }

        fn unlink(&self, _name: &str) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }

        fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }
    }

    #[test]
    fn create_dir_on_root_reports_already_exists() {
        let fs = TestFs::context();
        let err = fs
            .create_dir("/", NodePermission::from_bits_truncate(0o755))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn symlink_on_root_reports_already_exists() {
        let fs = TestFs::context();
        let err = fs.symlink("target", "/").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn rename_root_reports_resource_busy() {
        let fs = TestFs::context();
        let err = fs.rename("/", "/root").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
    }

    #[test]
    fn link_to_root_reports_already_exists() {
        let fs = TestFs::context();
        let err = fs.link("/child", "/").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn rename_to_root_reports_resource_busy() {
        let fs = TestFs::context();
        let err = fs.rename("/child", "/").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
    }

    #[test]
    fn rename_missing_source_to_root_reports_not_found() {
        let fs = TestFs::context();
        let err = fs.rename("/missing", "/").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::NotFound);
    }

    #[test]
    fn rename_root_to_missing_parent_reports_not_found() {
        let fs = TestFs::context();
        let err = fs.rename("/", "/missing/child").unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::NotFound);
    }

    #[test]
    fn create_dir_empty_path_stays_invalid_input() {
        let fs = TestFs::context();
        let err = fs
            .create_dir("", NodePermission::from_bits_truncate(0o755))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::InvalidInput);
    }
}
