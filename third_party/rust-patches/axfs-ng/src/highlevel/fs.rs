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
    use core::{
        any::Any,
        mem::size_of,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };
    use std::{
        sync::{Barrier, Condvar, Mutex as StdMutex},
        thread,
    };

    use axfs_ng_vfs::{
        DirEntry, DirEntrySink, DirNode, DirNodeOps, Filesystem, FilesystemOps, Location, Metadata,
        MetadataUpdate, Mountpoint, NodeOps, NodePermission, NodeType, Reference, StatFs, VfsError,
        VfsResult, WeakDirEntry,
    };
    use spin::Once;

    use super::FsContext;

    // Keep normal unmount as a consuming capability operation. In particular,
    // this must not silently regress to `fn(&Location)` where `Arc<Location>`
    // aliases would be invisible to the mount-handle strong count.
    const _: fn(Location) -> VfsResult<()> = Location::unmount;

    struct TestFs {
        root: Once<DirEntry>,
        flushes: AtomicUsize,
        unmounts: AtomicUsize,
        fail_flush: AtomicBool,
        flush_gate: StdMutex<Option<Arc<FlushGate>>>,
    }

    #[derive(Default)]
    struct FlushGate {
        state: StdMutex<(bool, bool)>,
        changed: Condvar,
    }

    impl FlushGate {
        fn block_flush(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
        }

        fn wait_until_started(&self) {
            let mut state = self.state.lock().unwrap();
            while !state.0 {
                state = self.changed.wait(state).unwrap();
            }
        }

        fn release(&self) {
            let mut state = self.state.lock().unwrap();
            state.1 = true;
            self.changed.notify_all();
        }
    }

    impl TestFs {
        fn new() -> Arc<Self> {
            let fs = Arc::new(Self {
                root: Once::new(),
                flushes: AtomicUsize::new(0),
                unmounts: AtomicUsize::new(0),
                fail_flush: AtomicBool::new(false),
                flush_gate: StdMutex::new(None),
            });
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

        fn install_flush_gate(&self) -> Arc<FlushGate> {
            let gate = Arc::new(FlushGate::default());
            *self.flush_gate.lock().unwrap() = Some(gate.clone());
            gate
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

        fn flush(&self) -> VfsResult<()> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            let gate = { self.flush_gate.lock().unwrap().clone() };
            if let Some(gate) = gate {
                gate.block_flush();
            }
            if self.fail_flush.load(Ordering::Relaxed) {
                Err(VfsError::Io)
            } else {
                Ok(())
            }
        }

        fn unmount(&self) {
            self.unmounts.fetch_add(1, Ordering::Relaxed);
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
            if matches!(name, "child" | "other") {
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
    fn filesystem_device_is_stable_across_mounts() {
        let fs = Filesystem::new(TestFs::new());
        let first = Mountpoint::new_root(&fs);
        let second = Mountpoint::new_root(&fs);

        assert_eq!(first.device(), second.device());
        assert_ne!(first.mount_id(), second.mount_id());
    }

    #[test]
    fn location_stays_two_pointer_words() {
        assert_eq!(size_of::<Location>(), size_of::<usize>() * 2);
    }

    #[test]
    fn path_in_mount_stops_at_the_current_mount_root() {
        let context = TestFs::context();
        let nested = context.resolve("/child/child").unwrap();
        assert_eq!(nested.path_in_mount().unwrap().as_str(), "/child/child");

        let target = context.resolve("/child").unwrap();
        let mounted_fs = Filesystem::new(TestFs::new());
        let mountpoint = target.mount(&mounted_fs).unwrap();
        let mounted_root = context.resolve("/child").unwrap();

        assert_eq!(mounted_root.path_in_mount().unwrap().as_str(), "/");
        assert_eq!(mounted_root.absolute_path().unwrap().as_str(), "/child");
        assert_eq!(mounted_root.mountpoint().mount_id(), mountpoint.mount_id());
    }

    #[test]
    fn unmount_flushes_before_releasing_the_filesystem() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve("/child").unwrap();
        mounted_root.unmount().unwrap();

        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        drop(raw_mountpoint);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
        assert!(!target.is_mountpoint());
    }

    #[test]
    fn normal_unmount_rejects_live_locations_without_flushing() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve("/child").unwrap();
        let open_location = mounted_root.clone();
        assert_eq!(mounted_root.unmount(), Err(VfsError::ResourceBusy));
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 0);
        assert!(target.is_mountpoint());

        drop(open_location);
        context.resolve("/child").unwrap().unmount().unwrap();
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn normal_unmount_ignores_a_raw_mountpoint_metadata_owner() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve("/child").unwrap();
        mounted_root.unmount().unwrap();
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        drop(raw_mountpoint);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn normal_unmount_ignores_internal_writeback_anchors() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve("/child").unwrap();
        let internal_writeback = mounted_root.writeback_anchor();
        mounted_root.unmount().unwrap();

        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        drop(internal_writeback);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn normal_unmount_blocks_new_path_admission_while_flushing() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let gate = mounted.install_flush_gate();
        let mounted_root = context.resolve("/child").unwrap();

        let unmount = thread::spawn(move || mounted_root.unmount());
        gate.wait_until_started();

        assert_eq!(
            context.resolve("/child/child").unwrap_err(),
            VfsError::ResourceBusy
        );

        gate.release();
        unmount.join().unwrap().unwrap();
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn normal_unmount_revalidates_transient_location_admission_during_flush() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let gate = mounted.install_flush_gate();
        let mounted_root = context.resolve("/child").unwrap();

        let unmount = thread::spawn(move || mounted_root.unmount());
        gate.wait_until_started();

        // A raw Mountpoint is metadata rather than a busy lease. If internal
        // code turns it into a Location during the lock-free flush window,
        // phase two still has to observe that admission even after the lease
        // itself has already been dropped.
        let late_location = raw_mountpoint.root_location();
        drop(late_location);
        gate.release();
        assert_eq!(unmount.join().unwrap(), Err(VfsError::ResourceBusy));
        assert!(target.is_mountpoint());

        context.resolve("/child").unwrap().unmount().unwrap();
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn normal_unmount_cannot_treat_an_arc_shared_location_as_exclusive() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let shared_location = Arc::new(context.resolve("/child").unwrap());

        // `unmount` consumes a Location value. Cloning that value out of a
        // shared Arc creates a second handle lease, so phase one rejects it
        // before any filesystem callback runs.
        assert_eq!(
            shared_location.as_ref().clone().unmount(),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 0);
        assert!(target.is_mountpoint());

        drop(shared_location);
        context.resolve("/child").unwrap().unmount().unwrap();
    }

    #[test]
    fn recursive_unmount_reserves_the_subtree_while_flushing() {
        let root = TestFs::new();
        let root_mount = Mountpoint::new_root(&Filesystem::new(root.clone()));
        let context = FsContext::new(root_mount.root_location());
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let existing_target = context.resolve("/child/child").unwrap();
        let gate = mounted.install_flush_gate();

        let namespace_root = context.root_dir().clone();
        let unmount = thread::spawn(move || namespace_root.unmount_all());
        gate.wait_until_started();

        assert_eq!(
            existing_target
                .mount(&Filesystem::new(TestFs::new()))
                .unwrap_err(),
            VfsError::ResourceBusy
        );
        assert_eq!(
            context.resolve("/child/other").unwrap_err(),
            VfsError::ResourceBusy
        );

        gate.release();
        unmount.join().unwrap().unwrap();
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(root.flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lazy_unmount_keeps_an_open_location_alive() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve("/child").unwrap();
        let mount_id = mounted_root.mountpoint().mount_id();
        let open_location = mounted_root.clone();
        mounted_root.lazy_unmount().unwrap();

        assert!(!target.is_mountpoint());
        assert_ne!(
            context.resolve("/child").unwrap().mountpoint().mount_id(),
            mount_id
        );
        assert!(open_location.lookup_no_follow("child").is_ok());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 0);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);

        drop(mounted_root);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        drop(open_location);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lazy_unmount_detaches_a_nested_mount_tree() {
        let context = TestFs::context();
        let outer_target = context.resolve("/child").unwrap();
        let outer = TestFs::new();
        outer_target.mount(&Filesystem::new(outer.clone())).unwrap();

        let outer_root = context.resolve("/child").unwrap();
        let inner_target = outer_root.lookup_no_follow("child").unwrap();
        let inner = TestFs::new();
        let inner_mount = inner_target.mount(&Filesystem::new(inner.clone())).unwrap();
        let inner_mount_id = inner_mount.mount_id();
        drop(inner_mount);
        drop(inner_target);

        assert_eq!(outer_root.mountpoint().subtree_devices().unwrap().len(), 2);
        outer_root.lazy_unmount().unwrap();
        assert!(!outer_target.is_mountpoint());

        let detached_inner = outer_root.lookup_no_follow("child").unwrap();
        assert_eq!(detached_inner.mountpoint().mount_id(), inner_mount_id);
        drop(detached_inner);
        drop(outer_root);
        assert_eq!(outer.unmounts.load(Ordering::Relaxed), 1);
        assert_eq!(inner.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn detached_descendant_location_keeps_ancestor_topology_alive() {
        let context = TestFs::context();
        let outer_target = context.resolve("/child").unwrap();
        let outer = TestFs::new();
        outer_target.mount(&Filesystem::new(outer.clone())).unwrap();

        let outer_root = context.resolve("/child").unwrap();
        let outer_mount_id = outer_root.mountpoint().mount_id();
        let inner_target = outer_root.lookup_no_follow("child").unwrap();
        let inner = TestFs::new();
        inner_target.mount(&Filesystem::new(inner.clone())).unwrap();
        let inner_root = outer_root.lookup_no_follow("child").unwrap();
        drop(inner_target);

        outer_root.lazy_unmount().unwrap();
        drop(outer_root);

        let detached_parent = inner_root
            .parent()
            .expect("detached nested mount must retain its parent topology");
        assert_eq!(detached_parent.mountpoint().mount_id(), outer_mount_id);
        assert!(detached_parent.is_root_of_mount());

        drop(detached_parent);
        drop(inner_root);
        assert_eq!(outer.unmounts.load(Ordering::Relaxed), 1);
        assert_eq!(inner.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_cross_moves_cannot_create_a_mount_cycle() {
        let context = TestFs::context();
        let first_target = context.resolve("/child").unwrap();
        let second_target = context.resolve("/other").unwrap();
        first_target.mount(&Filesystem::new(TestFs::new())).unwrap();
        second_target
            .mount(&Filesystem::new(TestFs::new()))
            .unwrap();

        let first_root = context.resolve("/child").unwrap();
        let second_root = context.resolve("/other").unwrap();
        let target_in_first = first_root.lookup_no_follow("child").unwrap();
        let target_in_second = second_root.lookup_no_follow("child").unwrap();
        let start = Arc::new(Barrier::new(3));

        let first_start = start.clone();
        let first_move = thread::spawn(move || {
            first_start.wait();
            first_root.move_mount_to(&target_in_second)
        });
        let second_start = start.clone();
        let second_move = thread::spawn(move || {
            second_start.wait();
            second_root.move_mount_to(&target_in_first)
        });
        start.wait();

        let first_result = first_move.join().unwrap();
        let second_result = second_move.join().unwrap();
        assert_ne!(first_result.is_ok(), second_result.is_ok());
        assert_ne!(first_target.is_mountpoint(), second_target.is_mountpoint());
    }

    #[test]
    fn lazy_unmount_of_a_stacked_mount_reveals_the_previous_mount() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        target.mount(&Filesystem::new(TestFs::new())).unwrap();
        let lower = context.resolve("/child").unwrap();
        let lower_mount_id = lower.mountpoint().mount_id();

        lower.mount(&Filesystem::new(TestFs::new())).unwrap();
        let upper = context.resolve("/child").unwrap();
        assert_ne!(upper.mountpoint().mount_id(), lower_mount_id);
        upper.lazy_unmount().unwrap();

        assert_eq!(
            context.resolve("/child").unwrap().mountpoint().mount_id(),
            lower_mount_id
        );
    }

    #[test]
    fn namespace_root_cannot_be_unmounted() {
        let context = TestFs::context();
        let root = context.root_dir();

        assert_eq!(root.clone().unmount(), Err(VfsError::ResourceBusy));
        assert_eq!(root.lazy_unmount(), Err(VfsError::ResourceBusy));
    }

    #[test]
    fn failed_unmount_flush_leaves_the_mount_attached() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        mounted.fail_flush.store(true, Ordering::Relaxed);

        assert_eq!(
            context.resolve("/child").unwrap().unmount().unwrap_err(),
            VfsError::Io
        );
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        assert!(target.is_mountpoint());

        mounted.fail_flush.store(false, Ordering::Relaxed);
        context.resolve("/child").unwrap().unmount().unwrap();
    }

    #[test]
    fn failed_recursive_unmount_keeps_the_child_attached() {
        let context = TestFs::context();
        let target = context.resolve("/child").unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        mounted.fail_flush.store(true, Ordering::Relaxed);

        assert_eq!(context.root_dir().unmount_all(), Err(VfsError::Io));
        assert!(target.is_mountpoint());
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);

        mounted.fail_flush.store(false, Ordering::Relaxed);
        context.resolve("/child").unwrap().unmount().unwrap();
    }

    #[test]
    fn flush_all_filesystems_visits_each_stable_device_once() {
        let root = TestFs::new();
        let root_fs = Filesystem::new(root.clone());
        let root_mount = Mountpoint::new_root(&root_fs);
        let context = FsContext::new(root_mount.root_location());

        let mounted = TestFs::new();
        let mounted_fs = Filesystem::new(mounted.clone());
        context
            .resolve("/child")
            .unwrap()
            .mount(&mounted_fs)
            .unwrap();

        let alias = TestFs::new();
        let alias_fs = Filesystem::new_with_device(alias.clone(), mounted_fs.device());
        context
            .resolve("/child/child")
            .unwrap()
            .mount(&alias_fs)
            .unwrap();

        root_mount.flush_all_filesystems().unwrap();

        assert_eq!(root.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(alias.flushes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn flush_all_filesystems_continues_after_an_error() {
        let root = TestFs::new();
        root.fail_flush.store(true, Ordering::Relaxed);
        let root_fs = Filesystem::new(root.clone());
        let root_mount = Mountpoint::new_root(&root_fs);
        let context = FsContext::new(root_mount.root_location());

        let mounted = TestFs::new();
        context
            .resolve("/child")
            .unwrap()
            .mount(&Filesystem::new(mounted.clone()))
            .unwrap();

        assert_eq!(root_mount.flush_all_filesystems(), Err(VfsError::Io));
        assert_eq!(root.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
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
