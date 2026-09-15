use alloc::{
    borrow::{Cow, ToOwned},
    collections::vec_deque::VecDeque,
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};

use axfs_ng_vfs::{
    Location, Metadata, NodeFlags, NodePermission, NodeType, OpenOptions as VfsOpenOptions,
    VfsError, VfsResult,
    path::{
        Component, Components, DOT, FinalComponent, FinalComponentKind, FsName, FsNameBuf, FsPath,
        FsPathBuf,
    },
};
use axio::{Read, Write};
use axsync::Mutex;
use spin::Once;

use super::File;

fn admit_native_namespace_mutation(location: &Location) -> VfsResult<()> {
    const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
    const FS_XFLAG_APPEND: u64 = 0x0000_0010;
    let attr = match location.get_file_attr() {
        Ok(attr) => attr,
        Err(VfsError::OperationNotSupported) => return Ok(()),
        Err(error) => return Err(error),
    };
    if attr.xflags & (FS_XFLAG_IMMUTABLE | FS_XFLAG_APPEND) != 0 {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(())
}

/// Maximum number of symlinks that will be followed during path resolution.
pub const SYMLINKS_MAX: usize = 40;

/// One pathname component observed by a per-walk policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathwalkComponent<'a> {
    /// Filesystem root separator.
    Root,
    /// Current-directory component (`.`).
    Current,
    /// Parent-directory component (`..`).
    Parent,
    /// Named component.
    Normal(&'a FsName),
}

/// Per-walk policy hooks for topology-sensitive path resolution.
///
/// The VFS reports generic traversal events; callers decide whether a
/// symlink, mount edge, absolute restart, or attempted root escape is allowed.
/// This keeps ABI-specific policies such as `openat2(2)` out of the generic
/// resolver while ensuring policy and lookup share one walk.
pub trait PathwalkPolicy {
    fn component(
        &mut self,
        _directory: &Location,
        _component: PathwalkComponent<'_>,
    ) -> VfsResult<()> {
        Ok(())
    }

    fn observe_mount_access(&self) -> bool {
        true
    }

    fn follow_magic_link(&mut self, _link: &Location, _final_component: bool) -> VfsResult<()> {
        Ok(())
    }

    fn follow_symlink(&mut self, _link: &Location, _final_component: bool) -> VfsResult<()> {
        Ok(())
    }

    fn cross_mount(&mut self, _from: &Location, _to: &Location) -> VfsResult<()> {
        Ok(())
    }

    fn absolute_root(&mut self, _from: &Location, _root: &Location) -> VfsResult<()> {
        Ok(())
    }

    fn escape_root(&mut self, _root: &Location) -> VfsResult<()> {
        Ok(())
    }
}

struct AllowPathwalkPolicy;

impl PathwalkPolicy for AllowPathwalkPolicy {}

struct UnobservedPathwalkPolicy;

impl PathwalkPolicy for UnobservedPathwalkPolicy {
    fn observe_mount_access(&self) -> bool {
        false
    }
}

fn allow_pathwalk(_dir: &Location) -> VfsResult<()> {
    Ok(())
}

pub(crate) fn path_requires_directory(path: &FsPath) -> bool {
    let raw = path.as_bytes();
    if raw.ends_with(b"/") {
        return true;
    }
    let last = raw
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(raw, |index| &raw[index + 1..]);
    last == b"."
}

/// Global root filesystem context, initialized once during [`init_filesystems`](crate::init_filesystems).
pub static ROOT_FS_CONTEXT: Once<FsContext> = Once::new();
pub(crate) static ROOT_FS_SCOPE_CONTEXT: Once<Arc<Mutex<FsContext>>> = Once::new();
static SYMLINK_FOLLOW_POLICY: Once<fn(&Location) -> bool> = Once::new();
static ATIME_UPDATE_POLICY: Once<fn(&Location) -> bool> = Once::new();
static MOUNT_ACCESS_POLICY: Once<fn(&Location)> = Once::new();
static AUTOMOUNT_TRIGGER_POLICY: Once<fn(&Location) -> VfsResult<bool>> = Once::new();

scope_local::scope_local! {
    /// Task-local filesystem context, defaulting to a clone of [`ROOT_FS_CONTEXT`].
    pub static FS_CONTEXT: Arc<Mutex<FsContext>> =
        ROOT_FS_SCOPE_CONTEXT
            .get()
            .expect("Root FS scope context not initialized")
            .clone();
}

/// A single entry returned by [`FsContext::read_dir`].
pub struct ReadDirEntry {
    /// Entry name (file or directory name, not the full path).
    pub name: FsNameBuf,
    /// Inode number.
    pub ino: u64,
    /// Type of the node (file, directory, symlink, etc.).
    pub node_type: NodeType,
    /// Byte offset inside the directory (used for seeking).
    pub offset: u64,
}

/// Provides `std::fs`-like interface.
#[derive(Debug)]
pub struct FsContext {
    root_dir: Location,
    current_dir: Location,
    umask: AtomicU32,
}

impl Clone for FsContext {
    fn clone(&self) -> Self {
        Self {
            root_dir: self.root_dir.clone(),
            current_dir: self.current_dir.clone(),
            umask: AtomicU32::new(self.umask.load(Ordering::SeqCst)),
        }
    }
}

impl FsContext {
    fn may_follow_symlink(loc: &Location) -> bool {
        SYMLINK_FOLLOW_POLICY.get().is_none_or(|policy| policy(loc))
    }

    pub(crate) fn should_update_atime(loc: &Location) -> bool {
        ATIME_UPDATE_POLICY.get().is_none_or(|policy| policy(loc))
    }

    fn note_mount_access(loc: &Location) {
        if let Some(policy) = MOUNT_ACCESS_POLICY.get() {
            policy(loc);
        }
    }

    fn note_mount_access_with_policy<P: PathwalkPolicy + ?Sized>(loc: &Location, policy: &P) {
        if policy.observe_mount_access() {
            Self::note_mount_access(loc);
        }
    }

    fn trigger_automount(loc: &Location) -> VfsResult<bool> {
        AUTOMOUNT_TRIGGER_POLICY
            .get()
            .map_or(Ok(false), |trigger| trigger(loc))
    }

    fn lookup_no_follow_with_policy<P: PathwalkPolicy + ?Sized>(
        dir: &Location,
        name: &FsName,
        policy: &mut P,
    ) -> VfsResult<Location> {
        let loc = match dir.lookup_no_follow(name) {
            Ok(loc) => loc,
            Err(err) => {
                Self::note_mount_access_with_policy(dir, policy);
                return Err(err);
            }
        };
        if !Arc::ptr_eq(dir.mountpoint(), loc.mountpoint()) {
            policy.cross_mount(dir, &loc)?;
            Self::note_mount_access_with_policy(&loc, policy);
        }
        Ok(loc)
    }

    /// Returns whether an absolute path resolves to this context's root entry.
    pub fn path_refers_to_root(&self, path: impl AsRef<FsPath>) -> bool {
        let path = path.as_ref();
        path.is_absolute()
            && self
                .resolve_no_follow(path)
                .is_ok_and(|entry| entry.ptr_eq(&self.root_dir))
    }

    /// Creates a new context with `root_dir` as both root and current directory.
    pub fn new(root_dir: Location) -> Self {
        Self {
            root_dir: root_dir.clone(),
            current_dir: root_dir,
            umask: AtomicU32::new(0o022),
        }
    }

    /// Returns this filesystem context's file creation mask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Replaces this filesystem context's file creation mask.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
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
            umask: AtomicU32::new(self.umask.load(Ordering::SeqCst)),
        })
    }

    /// Changes the root directory while preserving the current working
    /// directory, matching Linux `chroot(2)` semantics.
    pub fn set_root_dir(&mut self, root_dir: Location) -> VfsResult<()> {
        root_dir.check_is_dir()?;
        self.root_dir = root_dir;
        Ok(())
    }

    /// Applies Linux `pivot_root(2)`'s fs-structure reference update.
    /// Existing private chroots and working directories remain untouched;
    /// only references exactly at the former namespace root move to `new`.
    /// The caller has already validated both directories, so publication is
    /// allocation-free and cannot fail after mount topology has changed.
    pub fn pivot_root_refs(&mut self, old: &Location, new: &Location) {
        if self.root_dir.ptr_eq(old) {
            self.root_dir = new.clone();
        }
        if self.current_dir.ptr_eq(old) {
            self.current_dir = new.clone();
        }
    }

    /// Attempts to resolve a possible symlink, at the current location (this
    /// assumes that `loc` is a child of current directory).
    pub fn try_resolve_symlink(
        &self,
        loc: Location,
        follow_count: &mut usize,
    ) -> VfsResult<Location> {
        self.try_resolve_symlink_with_admission(loc, follow_count, &mut allow_pathwalk)
    }

    /// Resolves a possible symlink while admitting each traversed directory.
    pub fn try_resolve_symlink_with_admission<F>(
        &self,
        loc: Location,
        follow_count: &mut usize,
        admission: &mut F,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.try_resolve_symlink_with_policy(loc, follow_count, admission, &mut AllowPathwalkPolicy)
    }

    pub fn try_resolve_symlink_with_policy<F, P>(
        &self,
        loc: Location,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        self.try_resolve_symlink_with_policy_at(loc, follow_count, admission, policy, true)
    }

    fn try_resolve_symlink_with_policy_at<F, P>(
        &self,
        loc: Location,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
        final_component: bool,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        if loc.node_type() != NodeType::Symlink {
            return Ok(loc);
        }
        if !Self::may_follow_symlink(&loc) || *follow_count >= SYMLINKS_MAX {
            return Err(VfsError::FilesystemLoop);
        }
        if loc.flags().contains(NodeFlags::MAGIC_LINK) {
            policy.follow_magic_link(&loc, final_component)?;
        }
        policy.follow_symlink(&loc, final_component)?;
        *follow_count += 1;
        let target = loc.read_link()?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }
        let target: &FsPath = &target;
        self.resolve_path_with_policy(target, follow_count, admission, policy)
    }

    fn lookup_with_policy<F, P>(
        &self,
        dir: &Location,
        name: &FsName,
        final_component: bool,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        policy.component(dir, PathwalkComponent::Normal(name))?;
        admission(dir)?;
        let loc = Self::lookup_no_follow_with_policy(dir, name, policy)?;
        if loc.node_type() != NodeType::Symlink && loc.flags().contains(NodeFlags::MAGIC_LINK) {
            policy.follow_magic_link(&loc, final_component)?;
        }
        self.with_current_dir(dir.clone())?
            .try_resolve_symlink_with_policy_at(
                loc,
                follow_count,
                admission,
                policy,
                final_component,
            )
    }

    fn resolve_components_with_policy<F, P>(
        &self,
        components: Components,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let mut dir = self.current_dir.clone();
        Self::note_mount_access_with_policy(&dir, policy);
        for comp in components {
            match comp {
                Component::CurDir => {
                    policy.component(&dir, PathwalkComponent::Current)?;
                    dir.check_is_dir()?;
                    admission(&dir)?;
                }
                Component::ParentDir => {
                    policy.component(&dir, PathwalkComponent::Parent)?;
                    dir.check_is_dir()?;
                    admission(&dir)?;
                    if dir.ptr_eq(&self.root_dir) {
                        policy.escape_root(&dir)?;
                    } else {
                        let parent = dir.parent().unwrap_or_else(|| self.root_dir.clone());
                        let crossed_mount = !Arc::ptr_eq(dir.mountpoint(), parent.mountpoint());
                        if crossed_mount {
                            policy.cross_mount(&dir, &parent)?;
                        }
                        dir = parent;
                        if crossed_mount {
                            Self::note_mount_access_with_policy(&dir, policy);
                        }
                    }
                }
                Component::RootDir => {
                    policy.component(&dir, PathwalkComponent::Root)?;
                    policy.absolute_root(&dir, &self.root_dir)?;
                    let crossed_mount = !Arc::ptr_eq(dir.mountpoint(), self.root_dir.mountpoint());
                    if crossed_mount {
                        policy.cross_mount(&dir, &self.root_dir)?;
                    }
                    dir = self.root_dir.clone();
                    if crossed_mount {
                        Self::note_mount_access_with_policy(&dir, policy);
                    }
                }
                Component::Normal(name) => {
                    dir = self.lookup_with_policy(
                        &dir,
                        name,
                        false,
                        follow_count,
                        admission,
                        policy,
                    )?;
                }
            }
        }
        Ok(dir)
    }

    fn resolve_inner_with_policy<'a, F, P>(
        &self,
        path: &'a FsPath,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<(Location, Option<&'a FsName>)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let entry_name = path.file_name();
        let mut components = path.components();
        if entry_name.is_some() {
            components.next_back();
        }
        let dir =
            self.resolve_components_with_policy(components, follow_count, admission, policy)?;
        dir.check_is_dir()?;
        Ok((dir, entry_name))
    }

    /// Resolves a path starting from `current_dir`.
    pub fn resolve(&self, path: impl AsRef<FsPath>) -> VfsResult<Location> {
        self.resolve_with_admission(path, &mut allow_pathwalk)
    }

    /// Resolves a path after admitting every directory used for lookup.
    ///
    /// The callback is also applied while following relative or absolute
    /// symlink targets. It lets callers enforce pathname-search policy without
    /// embedding an ABI policy in the generic VFS.
    pub fn resolve_with_admission<F>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.resolve_with_policy(path, admission, &mut AllowPathwalkPolicy)
    }

    /// Resolves a path without publishing mount-activity observations.
    ///
    /// This is reserved for control operations, such as an expiration probe,
    /// whose own target lookup must not count as user activity.
    pub fn resolve_with_admission_unobserved<F>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.resolve_with_policy(path, admission, &mut UnobservedPathwalkPolicy)
    }

    pub fn resolve_with_policy<F, P>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let mut follow_count = 0;
        self.resolve_path_with_policy(path.as_ref(), &mut follow_count, admission, policy)
    }

    /// Resolves with Linux LOOKUP_AUTOMOUNT-style final-component triggering.
    /// A trigger may publish a mount and request a retry; retries retain the
    /// same caller policy and admission context rather than opening a second
    /// unchecked pathwalk.
    pub fn resolve_with_automount_policy<F, P>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        for _ in 0..=SYMLINKS_MAX {
            let location = self.resolve_with_policy(path.as_ref(), admission, policy)?;
            if !Self::trigger_automount(&location)? {
                return Ok(location);
            }
        }
        Err(VfsError::FilesystemLoop)
    }

    fn resolve_path_with_policy<F, P>(
        &self,
        path: &FsPath,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let (dir, name) = self.resolve_inner_with_policy(path, follow_count, admission, policy)?;
        let loc = match name {
            Some(name) => {
                self.lookup_with_policy(&dir, name, true, follow_count, admission, policy)
            }
            None => Ok(dir),
        }?;
        if path_requires_directory(path) {
            loc.check_is_dir()?;
            let final_component_is_dot = path.file_name() == Some(DOT);
            if final_component_is_dot && path.file_name().is_some() {
                admission(&loc)?;
            }
        }
        Ok(loc)
    }

    /// Resolves a path starting from `current_dir` not following symlinks.
    pub fn resolve_no_follow(&self, path: impl AsRef<FsPath>) -> VfsResult<Location> {
        self.resolve_no_follow_with_admission(path, &mut allow_pathwalk)
    }

    /// Resolves a path without following the final symlink, admitting every
    /// directory traversed through parent components and their symlink targets.
    pub fn resolve_no_follow_with_admission<F>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.resolve_no_follow_with_policy(path, admission, &mut AllowPathwalkPolicy)
    }

    /// Resolves a path without following its final symlink or publishing
    /// mount-activity observations.
    pub fn resolve_no_follow_with_admission_unobserved<F>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.resolve_no_follow_with_policy(path, admission, &mut UnobservedPathwalkPolicy)
    }

    pub fn resolve_no_follow_with_policy<F, P>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let path = path.as_ref();
        if path_requires_directory(path) {
            let mut follow_count = 0;
            return self.resolve_path_with_policy(path, &mut follow_count, admission, policy);
        }
        let (dir, name) = self.resolve_inner_with_policy(path, &mut 0, admission, policy)?;
        let loc = match name {
            Some(name) => {
                admission(&dir)?;
                Self::lookup_no_follow_with_policy(&dir, name, policy)
            }
            None => Ok(dir),
        }?;
        Ok(loc)
    }

    /// `resolve_with_automount_policy` counterpart that preserves the final
    /// symlink's no-follow treatment while allowing an automount trigger to
    /// publish and retry the terminal mountpoint.
    pub fn resolve_no_follow_with_automount_policy<F, P>(
        &self,
        path: impl AsRef<FsPath>,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<Location>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        for _ in 0..=SYMLINKS_MAX {
            let location = self.resolve_no_follow_with_policy(path.as_ref(), admission, policy)?;
            if !Self::trigger_automount(&location)? {
                return Ok(location);
            }
        }
        Err(VfsError::FilesystemLoop)
    }

    pub(crate) fn resolve_open_with_admission<F, C>(
        &self,
        path: &FsPath,
        options: &VfsOpenOptions,
        follow_final_symlink: bool,
        admission: &mut F,
        create_admission: &mut C,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &FsName, &mut VfsOpenOptions) -> VfsResult<()> + ?Sized,
    {
        self.resolve_open_with_policy(
            path,
            options,
            follow_final_symlink,
            admission,
            create_admission,
            &mut AllowPathwalkPolicy,
        )
    }

    pub(crate) fn resolve_open_with_policy<F, C, P>(
        &self,
        path: &FsPath,
        options: &VfsOpenOptions,
        follow_final_symlink: bool,
        admission: &mut F,
        create_admission: &mut C,
        policy: &mut P,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &FsName, &mut VfsOpenOptions) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let mut follow_count = 0;
        let result = self.resolve_open_inner(
            path,
            options,
            follow_final_symlink,
            &mut follow_count,
            admission,
            create_admission,
            policy,
        )?;
        Ok(result)
    }

    fn resolve_open_inner<F, C, P>(
        &self,
        path: &FsPath,
        options: &VfsOpenOptions,
        follow_final_symlink: bool,
        follow_count: &mut usize,
        admission: &mut F,
        create_admission: &mut C,
        policy: &mut P,
    ) -> VfsResult<(Location, bool)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        C: FnMut(&Location, &FsName, &mut VfsOpenOptions) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        if path.as_bytes().is_empty() {
            return Err(VfsError::NotFound);
        }

        if path.file_name().is_none() {
            let loc = self.resolve_path_with_policy(path, follow_count, admission, policy)?;
            if options.create_new {
                return Err(VfsError::AlreadyExists);
            }
            return Ok((loc, false));
        }

        let (parent, name) =
            match self.resolve_parent_with_policy_at_count(path, follow_count, admission, policy) {
                Ok(parent_and_name) => parent_and_name,
                Err(VfsError::InvalidInput) => {
                    let loc =
                        self.resolve_path_with_policy(path, follow_count, admission, policy)?;
                    if options.create_new {
                        return Err(VfsError::AlreadyExists);
                    }
                    return Ok((loc, false));
                }
                Err(err) => return Err(err),
            };
        admission(&parent)?;
        policy.component(&parent, PathwalkComponent::Normal(&name))?;

        let loc = match Self::lookup_no_follow_with_policy(&parent, &name, policy) {
            Ok(loc) => {
                if options.create_new {
                    return Err(VfsError::AlreadyExists);
                }
                loc
            }
            Err(VfsError::NotFound) if options.create => {
                if path_requires_directory(path) {
                    return Err(VfsError::IsADirectory);
                }
                let mut create_options = options.clone();
                create_admission(&parent, &name, &mut create_options)?;
                let (loc, created) = parent.open_file_with_status(&name, &create_options)?;
                if !Arc::ptr_eq(parent.mountpoint(), loc.mountpoint()) {
                    policy.cross_mount(&parent, &loc)?;
                    Self::note_mount_access_with_policy(&loc, policy);
                }
                if created {
                    return Ok((loc, true));
                }
                loc
            }
            Err(err) => return Err(err),
        };

        let requires_directory = path_requires_directory(path);
        if follow_final_symlink
            && loc.node_type() != NodeType::Symlink
            && loc.flags().contains(NodeFlags::MAGIC_LINK)
        {
            policy.follow_magic_link(&loc, true)?;
        }
        if (follow_final_symlink || requires_directory) && loc.node_type() == NodeType::Symlink {
            if !Self::may_follow_symlink(&loc) || *follow_count >= SYMLINKS_MAX {
                return Err(VfsError::FilesystemLoop);
            }
            if loc.flags().contains(NodeFlags::MAGIC_LINK) {
                policy.follow_magic_link(&loc, true)?;
            }
            policy.follow_symlink(&loc, true)?;
            *follow_count += 1;
            let target = loc.read_link()?;
            if target.is_empty() {
                return Err(VfsError::NotFound);
            }
            let target: &FsPath = &target;
            let result = self.with_current_dir(parent)?.resolve_open_inner(
                target,
                options,
                follow_final_symlink,
                follow_count,
                admission,
                create_admission,
                policy,
            )?;
            if requires_directory {
                result.0.check_is_dir()?;
                let final_component_is_dot = path.file_name() == Some(DOT);
                if final_component_is_dot && path.file_name().is_some() {
                    admission(&result.0)?;
                }
            }
            return Ok(result);
        }

        if requires_directory {
            loc.check_is_dir()?;
            let final_component_is_dot = path.file_name() == Some(DOT);
            if final_component_is_dot && path.file_name().is_some() {
                admission(&loc)?;
            }
        }

        Ok((loc, false))
    }

    /// Resolves every true parent component while preserving the final syntax.
    ///
    /// Unlike create-facing parent helpers, this method never normalizes a
    /// terminal `.` or `..` into an earlier named component. It also retains
    /// trailing-separator directory intent for a normal name. The caller owns
    /// operation-specific policy and error mapping for the returned
    /// [`FinalComponent`].
    pub fn resolve_parent_preserving_final<'a>(
        &self,
        path: &'a FsPath,
    ) -> VfsResult<(Location, FinalComponent<'a>)> {
        self.resolve_parent_preserving_final_with_admission(path, &mut allow_pathwalk)
    }

    /// Resolves every true parent component with per-directory admission while
    /// preserving the borrowed final-component classification.
    pub fn resolve_parent_preserving_final_with_admission<'a, F>(
        &self,
        path: &'a FsPath,
        admission: &mut F,
    ) -> VfsResult<(Location, FinalComponent<'a>)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        let (parent_path, final_component) =
            path.split_final_component().ok_or(VfsError::NotFound)?;
        let parent = self.resolve_components_with_policy(
            parent_path.components(),
            &mut 0,
            admission,
            &mut AllowPathwalkPolicy,
        )?;
        parent.check_is_dir()?;
        admission(&parent)?;
        Ok((parent, final_component))
    }

    /// Taking current node as root directory, resolves a path starting from
    /// `current_dir`.
    ///
    /// Returns `(parent_dir, entry_name)`, where `entry_name` is the name of
    /// the entry.
    pub fn resolve_parent<'a>(&self, path: &'a FsPath) -> VfsResult<(Location, Cow<'a, FsName>)> {
        self.resolve_parent_with_admission(path, &mut allow_pathwalk)
    }

    /// Resolves a parent directory while admitting every directory traversed.
    pub fn resolve_parent_with_admission<'a, F>(
        &self,
        path: &'a FsPath,
        admission: &mut F,
    ) -> VfsResult<(Location, Cow<'a, FsName>)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        self.resolve_parent_with_policy(path, admission, &mut AllowPathwalkPolicy)
    }

    pub fn resolve_parent_with_policy<'a, F, P>(
        &self,
        path: &'a FsPath,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<(Location, Cow<'a, FsName>)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        self.resolve_parent_with_policy_at_count(path, &mut 0, admission, policy)
    }

    fn resolve_parent_with_policy_at_count<'a, F, P>(
        &self,
        path: &'a FsPath,
        follow_count: &mut usize,
        admission: &mut F,
        policy: &mut P,
    ) -> VfsResult<(Location, Cow<'a, FsName>)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
        P: PathwalkPolicy + ?Sized,
    {
        let (dir, name) = self.resolve_inner_with_policy(path, follow_count, admission, policy)?;
        if let Some(name) = name {
            Ok((dir, Cow::Borrowed(name)))
        } else if dir.ptr_eq(&self.root_dir) {
            Err(VfsError::InvalidInput)
        } else if let Some(parent) = dir.parent() {
            if !Arc::ptr_eq(dir.mountpoint(), parent.mountpoint()) {
                policy.cross_mount(&dir, &parent)?;
                Self::note_mount_access_with_policy(&parent, policy);
            }
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
    pub fn resolve_nonexistent<'a>(&self, path: &'a FsPath) -> VfsResult<(Location, &'a FsName)> {
        self.resolve_nonexistent_with_admission(path, &mut allow_pathwalk)
    }

    /// Resolves a nonexistent final component while admitting its parent walk.
    pub fn resolve_nonexistent_with_admission<'a, F>(
        &self,
        path: &'a FsPath,
        admission: &mut F,
    ) -> VfsResult<(Location, &'a FsName)>
    where
        F: FnMut(&Location) -> VfsResult<()> + ?Sized,
    {
        let (dir, name) =
            self.resolve_inner_with_policy(path, &mut 0, admission, &mut AllowPathwalkPolicy)?;
        if let Some(name) = name {
            Ok((dir, name))
        } else if path.is_absolute() && dir.ptr_eq(&self.root_dir) {
            Err(VfsError::AlreadyExists)
        } else {
            Err(VfsError::InvalidInput)
        }
    }

    /// Retrieves metadata for the file.
    pub fn metadata(&self, path: impl AsRef<FsPath>) -> VfsResult<Metadata> {
        self.resolve(path)?.metadata()
    }

    /// Reads the entire contents of a file into a bytes vector.
    pub fn read(&self, path: impl AsRef<FsPath>) -> VfsResult<Vec<u8>> {
        let mut buf = Vec::new();
        let file = File::open(self, path.as_ref())?;
        (&file).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Reads the entire contents of a file into a string.
    /// Writes a slice as the entire contents of a file.
    ///
    /// This function will create a file if it does not exist, and will entirely
    /// replace its contents if it does.
    pub fn write(&self, path: impl AsRef<FsPath>, buf: impl AsRef<[u8]>) -> VfsResult<()> {
        let file = File::create(self, path.as_ref())?;
        (&file).write_all(buf.as_ref())?;
        Ok(())
    }

    /// Returns an iterator over the entries in a directory.
    pub fn read_dir(&self, path: impl AsRef<FsPath>) -> VfsResult<ReadDir> {
        let dir = self.resolve(path)?;
        Ok(ReadDir {
            dir,
            buf: VecDeque::new(),
            offset: 0,
            ended: false,
        })
    }

    /// Removes a file from the filesystem.
    pub fn remove_file(&self, path: impl AsRef<FsPath>) -> VfsResult<()> {
        let path = path.as_ref();
        let (parent, final_component) = self.resolve_parent_preserving_final(path)?;
        if final_component.requires_directory() {
            self.resolve(path)?;
            return Err(VfsError::IsADirectory);
        }
        let FinalComponentKind::Normal(name) = final_component.kind() else {
            return Err(VfsError::IsADirectory);
        };
        admit_native_namespace_mutation(&parent)?;
        if let Ok(victim) = parent.lookup_no_follow_in_mount(name) {
            admit_native_namespace_mutation(&victim)?;
        }
        parent.unlink(name, false)
    }

    /// Removes a directory from the filesystem.
    pub fn remove_dir(&self, path: impl AsRef<FsPath>) -> VfsResult<()> {
        let (parent, final_component) = self.resolve_parent_preserving_final(path.as_ref())?;
        let FinalComponentKind::Normal(name) = final_component.kind() else {
            return Err(VfsError::InvalidInput);
        };
        admit_native_namespace_mutation(&parent)?;
        if let Ok(victim) = parent.lookup_no_follow_in_mount(name) {
            admit_native_namespace_mutation(&victim)?;
        }
        parent.unlink(name, true)
    }

    /// Renames a file or directory to a new name, replacing the original file
    /// if `to` already exists.
    pub fn rename(&self, from: impl AsRef<FsPath>, to: impl AsRef<FsPath>) -> VfsResult<()> {
        let from = from.as_ref();
        let to = to.as_ref();
        let (src_dir, src_final) = self.resolve_parent_preserving_final(from)?;
        let (dst_dir, dst_final) = self.resolve_parent_preserving_final(to)?;
        if !src_dir.same_mount(&dst_dir) {
            return Err(VfsError::CrossesDevices);
        }
        let FinalComponentKind::Normal(src_name) = src_final.kind() else {
            return Err(VfsError::ResourceBusy);
        };
        let FinalComponentKind::Normal(dst_name) = dst_final.kind() else {
            return Err(VfsError::ResourceBusy);
        };

        let src = src_dir.lookup_no_follow_in_mount(src_name)?;
        let dst = match dst_dir.lookup_no_follow_in_mount(dst_name) {
            Ok(dst) => Some(dst),
            Err(VfsError::NotFound) => None,
            Err(error) => return Err(error),
        };
        src_dir.validate_rename_ancestry_checked(&src, &dst_dir, dst.as_ref())?;
        if (src_final.requires_directory() || dst_final.requires_directory()) && !src.is_dir() {
            return Err(VfsError::NotADirectory);
        }
        if dst_final.requires_directory()
            && dst
                .as_ref()
                .is_some_and(|destination| !destination.is_dir())
        {
            return Err(VfsError::NotADirectory);
        }
        if dst.as_ref().is_some_and(|dst| dst.same_node(&src)) {
            return Ok(());
        }
        admit_native_namespace_mutation(&src_dir)?;
        admit_native_namespace_mutation(&dst_dir)?;
        admit_native_namespace_mutation(&src)?;
        if let Some(dst) = dst.as_ref() {
            admit_native_namespace_mutation(dst)?;
        }
        if !src_dir.supports_rename() {
            return Err(VfsError::OperationNotPermitted);
        }
        src_dir.rename_checked(src_name, &src, &dst_dir, dst_name, dst.as_ref())
    }

    /// Creates a new, empty directory at the provided path.
    pub fn create_dir(
        &self,
        path: impl AsRef<FsPath>,
        mode: NodePermission,
    ) -> VfsResult<Location> {
        let (dir, name) = self.resolve_nonexistent(path.as_ref())?;
        admit_native_namespace_mutation(&dir)?;
        dir.create(name, NodeType::Directory, mode)
    }

    /// Creates a new hard link on the filesystem.
    pub fn link(
        &self,
        old_path: impl AsRef<FsPath>,
        new_path: impl AsRef<FsPath>,
    ) -> VfsResult<Location> {
        let old = self.resolve(old_path.as_ref())?;
        let (new_dir, new_name) = self.resolve_nonexistent(new_path.as_ref())?;
        admit_native_namespace_mutation(&old)?;
        admit_native_namespace_mutation(&new_dir)?;
        new_dir.link(new_name, &old)
    }

    /// Creates a new symbolic link on the filesystem.
    pub fn symlink(
        &self,
        target: impl AsRef<FsPath>,
        link_path: impl AsRef<FsPath>,
    ) -> VfsResult<Location> {
        let (dir, name) = self.resolve_nonexistent(link_path.as_ref())?;
        admit_native_namespace_mutation(&dir)?;
        match dir.lookup_no_follow(name) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(err) if err.canonicalize() == VfsError::NotFound => {}
            Err(err) => return Err(err),
        }
        dir.create_symlink(
            name,
            target.as_ref(),
            NodePermission::from_bits_truncate(0o777),
            None,
        )
    }

    /// Returns the canonical, absolute form of a path.
    pub fn canonicalize(&self, path: impl AsRef<FsPath>) -> VfsResult<FsPathBuf> {
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

/// Installs an observer for mount activity discovered by high-level path walks.
///
/// Observations are emitted at walk start, after a mount crossing, and when a
/// component lookup fails in a mount.
pub fn set_mount_access_policy(policy: fn(&Location)) {
    MOUNT_ACCESS_POLICY.call_once(|| policy);
}

/// Installs the typed automount dispatcher.  The dispatcher returns `true`
/// only after it has published a mount and requires the current lookup to be
/// retried; provider-specific registration belongs behind this one global
/// VFS hook.
pub fn set_automount_trigger_policy(policy: fn(&Location) -> VfsResult<bool>) {
    AUTOMOUNT_TRIGGER_POLICY.call_once(|| policy);
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
                &mut |name: &FsName, ino: u64, node_type: NodeType, offset: u64| {
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
    use alloc::{sync::Arc, vec::Vec};
    use core::{
        any::Any,
        mem::size_of,
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        task::Context,
        time::Duration,
    };
    use std::{
        sync::{Barrier, Condvar, Mutex as StdMutex},
        thread,
    };

    use axfs_ng_vfs::{
        CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode,
        FileNodeOps, Filesystem, FilesystemOps, Location, Metadata, MetadataUpdate,
        MetadataUpdateCapabilities, Mountpoint, NamedCreateOptions, NodeOps, NodePermission,
        NodeType, Reference, RenameRequest, StatFs, UnlinkRequest, VfsError, VfsResult,
        WeakDirEntry, drain_deferred_dentry_cache_cleanup,
        path::{FinalComponentKind, FsName, FsPath},
    };
    use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
    use spin::Once;

    use super::{FsContext, PathwalkComponent, PathwalkPolicy, set_mount_access_policy};
    use crate::OpenOptions;

    // Keep normal unmount as a consuming capability operation. In particular,
    // this must not silently regress to `fn(&Location)` where `Arc<Location>`
    // aliases would be invisible to the mount-handle strong count.
    const _: fn(Location) -> VfsResult<()> = Location::unmount;

    static OBSERVED_MOUNT_ID: AtomicU64 = AtomicU64::new(0);
    static OBSERVED_MOUNT_ACCESSES: AtomicUsize = AtomicUsize::new(0);
    static DENTRY_CLEANUP_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn drain_all_dentry_cache_cleanup() {
        for _ in 0..1_000_000 {
            if !drain_deferred_dentry_cache_cleanup() {
                return;
            }
        }
        panic!("deferred dentry cleanup did not converge");
    }

    fn observe_selected_mount(loc: &Location) {
        if loc.mountpoint().mount_id() == OBSERVED_MOUNT_ID.load(Ordering::Relaxed) {
            OBSERVED_MOUNT_ACCESSES.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn select_observed_mount(loc: &Location) {
        OBSERVED_MOUNT_ID.store(loc.mountpoint().mount_id(), Ordering::Relaxed);
        OBSERVED_MOUNT_ACCESSES.store(0, Ordering::Relaxed);
    }

    struct TestFs {
        root: Once<DirEntry>,
        flushes: AtomicUsize,
        unmounts: AtomicUsize,
        fail_flush: AtomicBool,
        rename_supported: AtomicBool,
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
                rename_supported: AtomicBool::new(true),
                flush_gate: StdMutex::new(None),
            });
            let root = DirEntry::new_dir(
                {
                    let fs = fs.clone();
                    move |this| {
                        DirNode::new(Arc::new(TestDir {
                            fs: fs.clone(),
                            this,
                            ino: 1,
                            rename_supported: true,
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

        fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
            MetadataUpdateCapabilities::empty()
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
        ino: u64,
        rename_supported: bool,
    }

    impl TestDir {
        fn child_inode(&self, name: &FsName) -> u64 {
            let mut hash = self.ino ^ 0xcbf2_9ce4_8422_2325;
            for byte in name.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash.max(2)
        }

        fn child(&self, name: &FsName) -> DirEntry {
            let parent = self.this.upgrade();
            let ino = self.child_inode(name);
            let rename_supported = name.as_bytes() != b"rename-disabled";
            DirEntry::new_dir(
                {
                    let fs = self.fs.clone();
                    move |this| {
                        DirNode::new(Arc::new(TestDir {
                            fs: fs.clone(),
                            this,
                            ino,
                            rename_supported,
                        }))
                    }
                },
                Reference::new(parent, name.to_owned()),
            )
        }

        fn file(&self, name: &FsName, node_type: NodeType, contents: &[u8]) -> DirEntry {
            DirEntry::new_file(
                FileNode::new(Arc::new(TestFile {
                    fs: self.fs.clone(),
                    ino: self.child_inode(name),
                    contents: contents.to_vec(),
                })),
                node_type,
                Reference::new(self.this.upgrade(), name.to_owned()),
            )
        }
    }

    struct TestFile {
        fs: Arc<TestFs>,
        ino: u64,
        contents: Vec<u8>,
    }

    impl NodeOps for TestFile {
        fn inode(&self) -> u64 {
            self.ino
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.ino,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o755),
                node_type: NodeType::RegularFile,
                uid: 0,
                gid: 0,
                project_id: 0,
                size: self.contents.len() as u64,
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

    impl Pollable for TestFile {
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

    impl FileNodeOps for TestFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidInput)?;
            let Some(contents) = self.contents.get(offset..) else {
                return Ok(0);
            };
            let len = buf.len().min(contents.len());
            buf[..len].copy_from_slice(&contents[..len]);
            Ok(len)
        }

        fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
            Err(VfsError::Unsupported)
        }

        fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
            Err(VfsError::Unsupported)
        }

        fn set_len(&self, _len: u64) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }

        fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }
    }

    impl NodeOps for TestDir {
        fn inode(&self) -> u64 {
            self.ino
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.ino,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o755),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                project_id: 0,
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

    impl DirNodeOps for TestDir {
        fn supports_named_create(&self, node_type: NodeType) -> bool {
            matches!(node_type, NodeType::Directory | NodeType::RegularFile)
        }

        fn supports_rename(&self) -> bool {
            self.rename_supported && self.fs.rename_supported.load(Ordering::Relaxed)
        }

        fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            let parent_ino = self
                .this
                .upgrade()
                .and_then(|entry| entry.parent())
                .map_or(self.ino, |parent| parent.inode());
            let mut read = 0;
            if offset == 0 && sink.accept(FsName::new(b"."), self.ino, NodeType::Directory, 1) {
                read += 1;
            }
            if offset <= 1 && sink.accept(FsName::new(b".."), parent_ino, NodeType::Directory, 2) {
                read += 1;
            }
            Ok(read)
        }

        fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
            match name.as_bytes() {
                b"child" | b"other" | b"rename-disabled" => Ok(self.child(name)),
                b"file" => Ok(self.file(name, NodeType::RegularFile, b"")),
                b"jump" => Ok(self.file(name, NodeType::Symlink, b"/child/child")),
                b"jump-create" => Ok(self.file(name, NodeType::Symlink, b"/child/new")),
                b"bad-jump" => Ok(self.file(name, NodeType::Symlink, b"/file/")),
                _ => Err(VfsError::NotFound),
            }
        }

        fn create_named(
            &self,
            name: &FsName,
            options: &NamedCreateOptions,
            disposition: CreateDisposition,
        ) -> VfsResult<CreateOutcome<DirEntry>> {
            if disposition == CreateDisposition::OpenOrCreate {
                match self.lookup(name) {
                    Ok(entry) => {
                        return Ok(CreateOutcome {
                            entry,
                            created: false,
                        });
                    }
                    Err(VfsError::NotFound) => {}
                    Err(err) => return Err(err),
                }
            }
            let entry = match options.node_type {
                NodeType::Directory => self.child(name),
                node_type => self.file(name, node_type, b""),
            };
            options.install_initial_data(&entry)?;
            Ok(CreateOutcome {
                entry,
                created: true,
            })
        }

        fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::Unsupported)
        }

        fn unlink(&self, _request: UnlinkRequest<'_>) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }

        fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
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
    fn filesystems_with_shared_identity_share_identity_lifetime() {
        let original = Filesystem::new(TestFs::new());
        let identity = original.identity_weak();
        let view = Filesystem::new_with_identity(TestFs::new(), original.identity());

        assert_eq!(original.device(), view.device());
        drop(original);
        assert!(identity.upgrade().is_some());
        drop(view);
        assert!(identity.upgrade().is_none());
    }

    #[test]
    fn root_cache_cleanup_waits_for_final_filesystem_view() {
        let _cleanup_guard = DENTRY_CLEANUP_TEST_LOCK.lock().unwrap();
        drain_all_dentry_cache_cleanup();
        let backend = TestFs::new();
        let filesystem = Filesystem::new(backend.clone());
        let root = filesystem.root_dir();
        let child_before = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        let view = Filesystem::new_view(backend, &filesystem);
        let mount = Mountpoint::new_root(&filesystem);
        let view_mount = Mountpoint::new_root(&view);

        drop(mount);
        drop(view_mount);
        let child_after_mounts = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        assert!(child_after_mounts.ptr_eq(&child_before));

        drop(view);
        let child_after_view = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        assert!(child_after_view.ptr_eq(&child_before));

        drop(filesystem);
        let child_before_deferred_drain = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        assert!(child_before_deferred_drain.ptr_eq(&child_before));
        drain_all_dentry_cache_cleanup();
        let child_after_final_owner = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        assert!(!child_after_final_owner.ptr_eq(&child_before));
        let uncached_child = root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        assert!(!uncached_child.ptr_eq(&child_after_final_owner));
        drain_all_dentry_cache_cleanup();
    }

    #[test]
    fn shared_identity_keeps_root_cache_lifetimes_independent() {
        let _cleanup_guard = DENTRY_CLEANUP_TEST_LOCK.lock().unwrap();
        drain_all_dentry_cache_cleanup();
        let first = Filesystem::new(TestFs::new());
        let second = Filesystem::new_with_identity(TestFs::new(), first.identity());
        assert_eq!(first.device(), second.device());

        let first_root = first.root_dir();
        let second_root = second.root_dir();
        let first_child = first_root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        let second_child = second_root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"child"))
            .unwrap();
        let first_mount = Mountpoint::new_root(&first);
        let second_mount = Mountpoint::new_root(&second);

        drop(first_mount);
        drop(second_mount);
        assert!(
            first_root
                .as_dir()
                .unwrap()
                .lookup(FsName::new(b"child"))
                .unwrap()
                .ptr_eq(&first_child)
        );
        assert!(
            second_root
                .as_dir()
                .unwrap()
                .lookup(FsName::new(b"child"))
                .unwrap()
                .ptr_eq(&second_child)
        );

        drop(first);
        drain_all_dentry_cache_cleanup();
        assert!(
            !first_root
                .as_dir()
                .unwrap()
                .lookup(FsName::new(b"child"))
                .unwrap()
                .ptr_eq(&first_child)
        );
        assert!(
            second_root
                .as_dir()
                .unwrap()
                .lookup(FsName::new(b"child"))
                .unwrap()
                .ptr_eq(&second_child)
        );

        drop(second);
        drain_all_dentry_cache_cleanup();
        assert!(
            !second_root
                .as_dir()
                .unwrap()
                .lookup(FsName::new(b"child"))
                .unwrap()
                .ptr_eq(&second_child)
        );
    }

    #[test]
    fn deferred_root_cache_cleanup_is_bounded_and_clears_a_deep_wide_tree() {
        let _cleanup_guard = DENTRY_CLEANUP_TEST_LOCK.lock().unwrap();
        drain_all_dentry_cache_cleanup();
        let filesystem = Filesystem::new(TestFs::new());
        let root = filesystem.root_dir();
        let mount = Mountpoint::new_root(&filesystem);
        let mut frontier = vec![root];
        let mut cached = Vec::new();

        for _ in 0..9 {
            let mut next = Vec::new();
            for parent in frontier {
                for name in ["child", "other"] {
                    let child = parent
                        .as_dir()
                        .unwrap()
                        .lookup(FsName::new(name.as_bytes()))
                        .unwrap();
                    cached.push(child.downgrade());
                    next.push(child);
                }
            }
            frontier = next;
        }
        drop(frontier);
        drop(mount);
        drop(filesystem);

        assert!(cached.iter().all(|entry| entry.upgrade().is_some()));
        assert!(drain_deferred_dentry_cache_cleanup());
        assert!(cached.iter().any(|entry| entry.upgrade().is_some()));
        drain_all_dentry_cache_cleanup();
        assert!(cached.iter().all(|entry| entry.upgrade().is_none()));
    }

    #[test]
    fn concurrent_root_cache_cleanup_does_not_lose_intrusive_work_items() {
        const FILESYSTEMS: usize = 64;

        let _cleanup_guard = DENTRY_CLEANUP_TEST_LOCK.lock().unwrap();
        drain_all_dentry_cache_cleanup();
        let start = Arc::new(Barrier::new(FILESYSTEMS + 1));
        let finished = Arc::new(AtomicUsize::new(0));
        let mut producers = Vec::new();

        for _ in 0..FILESYSTEMS {
            let start = start.clone();
            let finished = finished.clone();
            producers.push(thread::spawn(move || {
                let filesystem = Filesystem::new(TestFs::new());
                let root = filesystem.root_dir();
                let child = root
                    .as_dir()
                    .unwrap()
                    .lookup(FsName::new(b"child"))
                    .unwrap();
                let child_weak = child.downgrade();
                let mount = Mountpoint::new_root(&filesystem);
                start.wait();
                drop(mount);
                drop(filesystem);
                drop(child);
                drop(root);
                finished.fetch_add(1, Ordering::Release);
                child_weak
            }));
        }

        start.wait();
        while finished.load(Ordering::Acquire) != FILESYSTEMS {
            drain_deferred_dentry_cache_cleanup();
            thread::yield_now();
        }
        drain_all_dentry_cache_cleanup();

        let cached = producers
            .into_iter()
            .map(|producer| producer.join().unwrap())
            .collect::<Vec<_>>();
        assert!(cached.iter().all(|entry| entry.upgrade().is_none()));
    }

    #[test]
    fn detached_mount_tree_becomes_visible_in_one_root_attachment() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let detached = Mountpoint::new_detached(&Filesystem::new(TestFs::new())).unwrap();
        let detached_context = FsContext::new(detached.root_location());
        let nested_target = detached_context.resolve(FsPath::new(b"/child")).unwrap();
        let nested = nested_target
            .mount(&Filesystem::new(TestFs::new()))
            .unwrap();

        assert!(!target.is_mountpoint());
        assert!(!Arc::ptr_eq(
            context
                .resolve(FsPath::new(b"/child"))
                .unwrap()
                .mountpoint(),
            &detached
        ));

        detached.attach_to(&target).unwrap();

        assert!(Arc::ptr_eq(
            context
                .resolve(FsPath::new(b"/child"))
                .unwrap()
                .mountpoint(),
            &detached
        ));
        assert!(Arc::ptr_eq(
            context
                .resolve(FsPath::new(b"/child/child"))
                .unwrap()
                .mountpoint(),
            &nested
        ));
    }

    #[test]
    fn mounted_dentry_cannot_be_unlinked_renamed_or_replaced() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        target.mount(&Filesystem::new(TestFs::new())).unwrap();
        context
            .create_dir(
                FsPath::new(b"/replacement"),
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();

        assert_eq!(
            context.remove_dir(FsPath::new(b"/child")),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(
            context.remove_dir(FsPath::new(b"/child/")),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(
            context.rename(FsPath::new(b"/child"), FsPath::new(b"/renamed")),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(
            context.rename(FsPath::new(b"/replacement"), FsPath::new(b"/child")),
            Err(VfsError::ResourceBusy)
        );
    }

    #[test]
    fn remove_dir_rejects_non_normal_final_components() {
        let context = TestFs::context();

        for path in [
            FsPath::new(b"/child/."),
            FsPath::new(b"/child/.."),
            FsPath::new(b"/"),
        ] {
            assert_eq!(context.remove_dir(path), Err(VfsError::InvalidInput));
        }
        assert_eq!(
            context.remove_dir(FsPath::new(b"")),
            Err(VfsError::NotFound)
        );
    }

    #[test]
    fn remove_file_preserves_mount_and_directory_intent_errors() {
        let mounted_context = TestFs::context();
        let target = mounted_context.resolve(FsPath::new(b"/child")).unwrap();
        target.mount(&Filesystem::new(TestFs::new())).unwrap();
        let context = TestFs::context();

        assert_eq!(
            (
                mounted_context.remove_file(FsPath::new(b"/child")),
                context.remove_file(FsPath::new(b"/file/")),
                context.remove_file(FsPath::new(b"/child/.")),
                context.remove_file(FsPath::new(b"/")),
            ),
            (
                Err(VfsError::ResourceBusy),
                Err(VfsError::NotADirectory),
                Err(VfsError::IsADirectory),
                Err(VfsError::IsADirectory),
            )
        );
    }

    #[test]
    fn location_stays_three_pointer_words() {
        // Mount lifecycle ownership deliberately split the directly owned
        // mountpoint from its shared admission/accounting handle. Together
        // with the dentry, Location therefore contains three pointer words.
        assert_eq!(size_of::<Location>(), size_of::<usize>() * 3);
    }

    #[test]
    fn pathwalk_admission_observes_each_lookup_directory() {
        let context = TestFs::context();
        let mut visited = Vec::new();

        context
            .resolve_with_admission(FsPath::new(b"/child/child"), &mut |dir| {
                visited.push(dir.absolute_path()?.as_bytes().to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(visited, [b"/".to_vec(), b"/child".to_vec()]);
    }

    #[test]
    fn mount_activity_observes_walk_starts_crossings_and_failed_lookups() {
        set_mount_access_policy(observe_selected_mount);
        let context = TestFs::context();

        select_observed_mount(context.root_dir());
        context.resolve(FsPath::new(b"/")).unwrap();
        assert_eq!(OBSERVED_MOUNT_ACCESSES.load(Ordering::Relaxed), 1);

        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = target.mount(&Filesystem::new(TestFs::new())).unwrap();
        let mounted_root = mounted.root_location();

        select_observed_mount(&mounted_root);
        context.resolve(FsPath::new(b"/child/child")).unwrap();
        assert_eq!(OBSERVED_MOUNT_ACCESSES.load(Ordering::Relaxed), 1);

        let mounted_context = FsContext::new(mounted_root.clone());
        select_observed_mount(&mounted_root);
        assert_eq!(
            mounted_context
                .resolve(FsPath::new(b"missing"))
                .unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(OBSERVED_MOUNT_ACCESSES.load(Ordering::Relaxed), 2);

        select_observed_mount(&mounted_root);
        context
            .resolve_with_admission_unobserved(FsPath::new(b"/child/child"), &mut |_| Ok(()))
            .unwrap();
        assert_eq!(
            context
                .resolve_with_admission_unobserved(FsPath::new(b"/child/child"), &mut |dir| {
                    if Arc::ptr_eq(dir.mountpoint(), &mounted) {
                        Err(VfsError::PermissionDenied)
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err(),
            VfsError::PermissionDenied
        );
        context
            .resolve_no_follow_with_admission_unobserved(FsPath::new(b"/child"), &mut |_| Ok(()))
            .unwrap();
        assert_eq!(OBSERVED_MOUNT_ACCESSES.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pathwalk_admission_can_reject_an_intermediate_directory() {
        let context = TestFs::context();

        let result = context.resolve_with_admission(FsPath::new(b"/child/child"), &mut |dir| {
            if dir.name() == FsName::new(b"child") {
                Err(VfsError::PermissionDenied)
            } else {
                Ok(())
            }
        });

        assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
    }

    #[test]
    fn pathwalk_admission_covers_symlink_targets() {
        let context = TestFs::context();

        let result = context.resolve_with_admission(FsPath::new(b"/jump"), &mut |dir| {
            if dir.name() == FsName::new(b"child") {
                Err(VfsError::PermissionDenied)
            } else {
                Ok(())
            }
        });

        assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
    }

    struct RejectSymlink;

    impl PathwalkPolicy for RejectSymlink {
        fn follow_symlink(&mut self, _link: &Location, _final_component: bool) -> VfsResult<()> {
            Err(VfsError::FilesystemLoop)
        }
    }

    #[test]
    fn pathwalk_policy_observes_intermediate_and_final_symlinks() {
        let context = TestFs::context();
        let result =
            context.resolve_with_policy(FsPath::new(b"/jump"), &mut |_| Ok(()), &mut RejectSymlink);
        assert_eq!(result.unwrap_err(), VfsError::FilesystemLoop);
    }

    #[derive(Default)]
    struct ObserveWalk {
        components: Vec<Vec<u8>>,
        final_symlinks: Vec<bool>,
    }

    impl PathwalkPolicy for ObserveWalk {
        fn component(
            &mut self,
            _directory: &Location,
            component: PathwalkComponent<'_>,
        ) -> VfsResult<()> {
            self.components.push(match component {
                PathwalkComponent::Root => b"/".to_vec(),
                PathwalkComponent::Current => b".".to_vec(),
                PathwalkComponent::Parent => b"..".to_vec(),
                PathwalkComponent::Normal(name) => name.as_bytes().to_vec(),
            });
            Ok(())
        }

        fn follow_symlink(&mut self, _link: &Location, final_component: bool) -> VfsResult<()> {
            self.final_symlinks.push(final_component);
            Ok(())
        }
    }

    #[test]
    fn pathwalk_policy_receives_real_components_and_final_position() {
        let context = TestFs::context();
        let mut final_walk = ObserveWalk::default();
        context
            .resolve_with_policy(FsPath::new(b"/jump"), &mut |_| Ok(()), &mut final_walk)
            .unwrap();
        assert_eq!(final_walk.final_symlinks, [true]);
        assert!(final_walk.components.iter().any(|name| name == b"jump"));
        assert!(final_walk.components.iter().any(|name| name == b"child"));

        let mut intermediate_walk = ObserveWalk::default();
        let _ = context.resolve_with_policy(
            FsPath::new(b"/jump/missing"),
            &mut |_| Ok(()),
            &mut intermediate_walk,
        );
        assert_eq!(intermediate_walk.final_symlinks, [false]);
    }

    struct RejectTopologyEdges;

    impl PathwalkPolicy for RejectTopologyEdges {
        fn cross_mount(&mut self, _from: &Location, _to: &Location) -> VfsResult<()> {
            Err(VfsError::CrossesDevices)
        }

        fn absolute_root(&mut self, _from: &Location, _root: &Location) -> VfsResult<()> {
            Err(VfsError::CrossesDevices)
        }

        fn escape_root(&mut self, _root: &Location) -> VfsResult<()> {
            Err(VfsError::CrossesDevices)
        }
    }

    #[test]
    fn pathwalk_policy_observes_mount_crossings() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        target.mount(&Filesystem::new(TestFs::new())).unwrap();

        let result = context.resolve_with_policy(
            FsPath::new(b"child"),
            &mut |_| Ok(()),
            &mut RejectTopologyEdges,
        );
        assert_eq!(result.unwrap_err(), VfsError::CrossesDevices);
    }

    #[test]
    fn pathwalk_policy_observes_absolute_restart_and_root_escape() {
        let outer = TestFs::context();
        let jail = FsContext::new(outer.resolve(FsPath::new(b"/child")).unwrap());

        assert_eq!(
            jail.resolve_with_policy(
                FsPath::new(b"/child"),
                &mut |_| Ok(()),
                &mut RejectTopologyEdges,
            )
            .unwrap_err(),
            VfsError::CrossesDevices
        );
        assert_eq!(
            jail.resolve_with_policy(
                FsPath::new(b".."),
                &mut |_| Ok(()),
                &mut RejectTopologyEdges
            )
            .unwrap_err(),
            VfsError::CrossesDevices
        );
    }

    #[test]
    fn pathwalk_rejects_parent_traversal_through_a_regular_file() {
        let context = TestFs::context();
        assert_eq!(
            context.resolve(FsPath::new(b"/file/../child")).unwrap_err(),
            VfsError::NotADirectory
        );
    }

    #[test]
    fn terminal_dot_is_a_directory_search_component() {
        let context = TestFs::context();
        let result = context.resolve_with_admission(FsPath::new(b"/child/."), &mut |dir| {
            if dir.name() == FsName::new(b"child") {
                Err(VfsError::PermissionDenied)
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
    }

    #[test]
    fn preserving_final_parent_resolution_keeps_normal_and_special_components() {
        let context = TestFs::context();

        let (parent, final_component) = context
            .resolve_parent_preserving_final(FsPath::new(b"/file"))
            .unwrap();
        assert_eq!(parent.absolute_path().unwrap().as_bytes(), b"/");
        assert_eq!(
            final_component.kind(),
            FinalComponentKind::Normal(FsName::new(b"file"))
        );
        assert!(!final_component.requires_directory());

        let (parent, final_component) = context
            .resolve_parent_preserving_final(FsPath::new(b"/file/"))
            .unwrap();
        assert_eq!(parent.absolute_path().unwrap().as_bytes(), b"/");
        assert_eq!(
            final_component.kind(),
            FinalComponentKind::Normal(FsName::new(b"file"))
        );
        assert!(final_component.requires_directory());

        for (path, kind) in [
            (".", FinalComponentKind::Dot),
            ("..", FinalComponentKind::DotDot),
            ("/", FinalComponentKind::Root),
        ] {
            let (parent, final_component) = context
                .resolve_parent_preserving_final(FsPath::new(path.as_bytes()))
                .unwrap();
            assert_eq!(parent.absolute_path().unwrap().as_bytes(), b"/");
            assert_eq!(final_component.kind(), kind);
            assert!(final_component.requires_directory());
        }
    }

    #[test]
    fn preserving_final_parent_resolution_walks_nested_directory_before_dot() {
        let context = TestFs::context();
        let mut visited = Vec::new();

        let (parent, final_component) = context
            .resolve_parent_preserving_final_with_admission(
                FsPath::new(b"/child/child/."),
                &mut |dir| {
                    visited.push(dir.absolute_path()?.as_bytes().to_vec());
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(parent.absolute_path().unwrap().as_bytes(), b"/child/child");
        assert_eq!(final_component.kind(), FinalComponentKind::Dot);
        assert!(final_component.requires_directory());
        assert_eq!(
            visited,
            [b"/".to_vec(), b"/child".to_vec(), b"/child/child".to_vec()]
        );
    }

    #[test]
    fn preserving_final_parent_resolution_propagates_exact_parent_denial_once() {
        let context = TestFs::context();
        let mut visited = Vec::new();
        let mut exact_parent_admissions = 0;

        let result = context.resolve_parent_preserving_final_with_admission(
            FsPath::new(b"/child/child/."),
            &mut |dir| {
                let path = dir.absolute_path()?.as_bytes().to_vec();
                visited.push(path.clone());
                if path == b"/child/child" {
                    exact_parent_admissions += 1;
                    Err(VfsError::PermissionDenied)
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
        assert_eq!(
            visited,
            [b"/".to_vec(), b"/child".to_vec(), b"/child/child".to_vec()]
        );
        assert_eq!(exact_parent_admissions, 1);
    }

    #[test]
    fn preserving_final_parent_resolution_rejects_file_before_dot() {
        let context = TestFs::context();
        assert_eq!(
            context
                .resolve_parent_preserving_final(FsPath::new(b"/file/."))
                .unwrap_err(),
            VfsError::NotADirectory
        );
    }

    #[test]
    fn no_follow_still_follows_a_symlink_before_a_trailing_slash() {
        let context = TestFs::context();
        assert!(
            context
                .resolve_no_follow(FsPath::new(b"/jump/"))
                .unwrap()
                .is_dir()
        );
    }

    #[test]
    fn context_root_clamps_parent_traversal_and_parent_resolution() {
        let outer = TestFs::context();
        let jail_root = outer.resolve(FsPath::new(b"/child")).unwrap();
        let context = FsContext::new(jail_root);

        assert!(
            context
                .resolve(FsPath::new(b".."))
                .unwrap()
                .ptr_eq(context.root_dir())
        );
        assert_eq!(
            context.resolve_parent(FsPath::new(b"/")).unwrap_err(),
            VfsError::InvalidInput
        );
    }

    #[test]
    fn create_open_follows_a_dangling_symlink_with_one_admission_chain() {
        let context = TestFs::context();
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .mode(0o600)
            .user(1000, 1000);
        let mut create_targets = Vec::new();

        let (loc, created) = options
            .resolve_location_with_admission(
                &context,
                FsPath::new(b"/jump-create"),
                &mut |_| Ok(()),
                &mut |parent, name, _options| {
                    create_targets.push((
                        parent.absolute_path()?.as_bytes().to_vec(),
                        name.as_bytes().to_vec(),
                    ));
                    Ok(())
                },
            )
            .unwrap();

        assert!(created);
        assert_eq!(loc.absolute_path().unwrap().as_bytes(), b"/child/new");
        assert_eq!(create_targets, [(b"/child".to_vec(), b"new".to_vec())]);
    }

    #[test]
    fn create_open_does_not_admit_an_existing_final_component() {
        let context = TestFs::context();
        let mut options = OpenOptions::new();
        options.write(true).create(true).mode(0o600);
        let mut create_admissions = 0;

        let (loc, created) = options
            .resolve_location_with_admission(
                &context,
                FsPath::new(b"/file"),
                &mut |_| Ok(()),
                &mut |_, _, _| {
                    create_admissions += 1;
                    Ok(())
                },
            )
            .unwrap();

        assert!(!created);
        assert_eq!(loc.absolute_path().unwrap().as_bytes(), b"/file");
        assert_eq!(create_admissions, 0);
    }

    #[test]
    fn create_open_denial_publishes_no_name() {
        let context = TestFs::context();
        let mut options = OpenOptions::new();
        options.write(true).create(true).mode(0o600);
        let mut create_admissions = 0;
        let generation = context.root_dir().namespace_generation().unwrap();

        let result = options.resolve_location_with_admission(
            &context,
            FsPath::new(b"/denied-create"),
            &mut |_| Ok(()),
            &mut |parent, name, _| {
                create_admissions += 1;
                assert_eq!(parent.absolute_path()?.as_bytes(), b"/");
                assert_eq!(name, FsName::new(b"denied-create"));
                Err(VfsError::PermissionDenied)
            },
        );

        assert_eq!(result.unwrap_err(), VfsError::PermissionDenied);
        assert_eq!(create_admissions, 1);
        assert!(
            context
                .root_dir()
                .namespace_generation_is_current(generation)
                .unwrap()
        );
        assert_eq!(
            context
                .resolve_no_follow(FsPath::new(b"/denied-create"))
                .unwrap_err(),
            VfsError::NotFound
        );
    }

    #[test]
    fn exclusive_create_rejects_a_dangling_symlink_itself() {
        let context = TestFs::context();
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .create_new(true)
            .mode(0o600);

        let result = options.resolve_location_with_admission(
            &context,
            FsPath::new(b"/jump-create"),
            &mut |_| Ok(()),
            &mut |_, _, _| Ok(()),
        );
        assert_eq!(result.unwrap_err(), VfsError::AlreadyExists);
    }

    #[test]
    fn open_honors_a_trailing_slash_inside_a_symlink_target() {
        let context = TestFs::context();
        let mut options = OpenOptions::new();
        options.read(true);

        let result = options.resolve_location_with_admission(
            &context,
            FsPath::new(b"/bad-jump"),
            &mut |_| Ok(()),
            &mut |_, _, _| Ok(()),
        );
        assert_eq!(result.unwrap_err(), VfsError::NotADirectory);
    }

    #[test]
    fn path_in_mount_stops_at_the_current_mount_root() {
        let context = TestFs::context();
        let nested = context.resolve(FsPath::new(b"/child/child")).unwrap();
        assert_eq!(nested.path_in_mount().unwrap().as_bytes(), b"/child/child");

        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted_fs = Filesystem::new(TestFs::new());
        let mountpoint = target.mount(&mounted_fs).unwrap();
        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();

        assert_eq!(mounted_root.path_in_mount().unwrap().as_bytes(), b"/");
        assert_eq!(mounted_root.absolute_path().unwrap().as_bytes(), b"/child");
        assert_eq!(mounted_root.mountpoint().mount_id(), mountpoint.mount_id());
    }

    #[test]
    fn unmount_flushes_before_releasing_the_filesystem() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();
        let open_location = mounted_root.clone();
        assert_eq!(mounted_root.unmount(), Err(VfsError::ResourceBusy));
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 0);
        assert!(target.is_mountpoint());

        drop(open_location);
        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_prepared_or_flushed_unmount_releases_the_reservation() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let prepared = context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .prepare_unmount()
            .unwrap();
        drop(prepared);
        context.resolve(FsPath::new(b"/child/child")).unwrap();

        let flushed = context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .prepare_unmount()
            .unwrap()
            .flush()
            .unwrap();
        drop(flushed);
        context.resolve(FsPath::new(b"/child/child")).unwrap();
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);

        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
    }

    #[test]
    fn normal_unmount_ignores_a_raw_mountpoint_metadata_owner() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let gate = mounted.install_flush_gate();
        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();

        let unmount = thread::spawn(move || mounted_root.unmount());
        gate.wait_until_started();

        assert_eq!(
            context.resolve(FsPath::new(b"/child/child")).unwrap_err(),
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        let raw_mountpoint = target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let gate = mounted.install_flush_gate();
        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();

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

        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
        assert!(!target.is_mountpoint());
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn normal_unmount_cannot_treat_an_arc_shared_location_as_exclusive() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let shared_location = Arc::new(context.resolve(FsPath::new(b"/child")).unwrap());

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
        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
    }

    #[test]
    fn recursive_unmount_reserves_the_subtree_while_flushing() {
        let root = TestFs::new();
        let root_mount = Mountpoint::new_root(&Filesystem::new(root.clone()));
        let context = FsContext::new(root_mount.root_location());
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        let existing_target = context.resolve(FsPath::new(b"/child/child")).unwrap();
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
            context.resolve(FsPath::new(b"/child/other")).unwrap_err(),
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();

        let mounted_root = context.resolve(FsPath::new(b"/child")).unwrap();
        let mount_id = mounted_root.mountpoint().mount_id();
        let open_location = mounted_root.clone();
        mounted_root.lazy_unmount().unwrap();

        assert!(!target.is_mountpoint());
        assert_ne!(
            context
                .resolve(FsPath::new(b"/child"))
                .unwrap()
                .mountpoint()
                .mount_id(),
            mount_id
        );
        assert!(
            open_location
                .lookup_no_follow(FsName::new(b"child"))
                .is_ok()
        );
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
        let outer_target = context.resolve(FsPath::new(b"/child")).unwrap();
        let outer = TestFs::new();
        outer_target.mount(&Filesystem::new(outer.clone())).unwrap();

        let outer_root = context.resolve(FsPath::new(b"/child")).unwrap();
        let inner_target = outer_root.lookup_no_follow(FsName::new(b"child")).unwrap();
        let inner = TestFs::new();
        let inner_mount = inner_target.mount(&Filesystem::new(inner.clone())).unwrap();
        let inner_mount_id = inner_mount.mount_id();
        drop(inner_mount);
        drop(inner_target);

        assert_eq!(outer_root.mountpoint().subtree_devices().unwrap().len(), 2);
        outer_root.lazy_unmount().unwrap();
        assert!(!outer_target.is_mountpoint());

        let detached_inner = outer_root.lookup_no_follow(FsName::new(b"child")).unwrap();
        assert_eq!(detached_inner.mountpoint().mount_id(), inner_mount_id);
        drop(detached_inner);
        drop(outer_root);
        assert_eq!(outer.unmounts.load(Ordering::Relaxed), 1);
        assert_eq!(inner.unmounts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn detached_descendant_location_keeps_ancestor_topology_alive() {
        let context = TestFs::context();
        let outer_target = context.resolve(FsPath::new(b"/child")).unwrap();
        let outer = TestFs::new();
        outer_target.mount(&Filesystem::new(outer.clone())).unwrap();

        let outer_root = context.resolve(FsPath::new(b"/child")).unwrap();
        let outer_mount_id = outer_root.mountpoint().mount_id();
        let inner_target = outer_root.lookup_no_follow(FsName::new(b"child")).unwrap();
        let inner = TestFs::new();
        inner_target.mount(&Filesystem::new(inner.clone())).unwrap();
        let inner_root = outer_root.lookup_no_follow(FsName::new(b"child")).unwrap();
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
        let first_target = context.resolve(FsPath::new(b"/child")).unwrap();
        let second_target = context.resolve(FsPath::new(b"/other")).unwrap();
        first_target.mount(&Filesystem::new(TestFs::new())).unwrap();
        second_target
            .mount(&Filesystem::new(TestFs::new()))
            .unwrap();

        let first_root = context.resolve(FsPath::new(b"/child")).unwrap();
        let second_root = context.resolve(FsPath::new(b"/other")).unwrap();
        let target_in_first = first_root.lookup_no_follow(FsName::new(b"child")).unwrap();
        let target_in_second = second_root.lookup_no_follow(FsName::new(b"child")).unwrap();
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        target.mount(&Filesystem::new(TestFs::new())).unwrap();
        let lower = context.resolve(FsPath::new(b"/child")).unwrap();
        let lower_mount_id = lower.mountpoint().mount_id();

        lower.mount(&Filesystem::new(TestFs::new())).unwrap();
        let upper = context.resolve(FsPath::new(b"/child")).unwrap();
        assert_ne!(upper.mountpoint().mount_id(), lower_mount_id);
        upper.lazy_unmount().unwrap();

        assert_eq!(
            context
                .resolve(FsPath::new(b"/child"))
                .unwrap()
                .mountpoint()
                .mount_id(),
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
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        mounted.fail_flush.store(true, Ordering::Relaxed);

        assert_eq!(
            context
                .resolve(FsPath::new(b"/child"))
                .unwrap()
                .unmount()
                .unwrap_err(),
            VfsError::Io
        );
        assert_eq!(mounted.flushes.load(Ordering::Relaxed), 1);
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);
        assert!(target.is_mountpoint());

        mounted.fail_flush.store(false, Ordering::Relaxed);
        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
    }

    #[test]
    fn failed_recursive_unmount_keeps_the_child_attached() {
        let context = TestFs::context();
        let target = context.resolve(FsPath::new(b"/child")).unwrap();
        let mounted = TestFs::new();
        target.mount(&Filesystem::new(mounted.clone())).unwrap();
        mounted.fail_flush.store(true, Ordering::Relaxed);

        assert_eq!(context.root_dir().unmount_all(), Err(VfsError::Io));
        assert!(target.is_mountpoint());
        assert_eq!(mounted.unmounts.load(Ordering::Relaxed), 0);

        mounted.fail_flush.store(false, Ordering::Relaxed);
        context
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .unmount()
            .unwrap();
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
            .resolve(FsPath::new(b"/child"))
            .unwrap()
            .mount(&mounted_fs)
            .unwrap();

        let alias = TestFs::new();
        let alias_fs = Filesystem::new_view(alias.clone(), &mounted_fs);
        context
            .resolve(FsPath::new(b"/child/child"))
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
            .resolve(FsPath::new(b"/child"))
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
            .create_dir(FsPath::new(b"/"), NodePermission::from_bits_truncate(0o755))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn symlink_on_root_reports_already_exists() {
        let fs = TestFs::context();
        let err = fs
            .symlink(FsPath::new(b"target"), FsPath::new(b"/"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn rename_root_reports_resource_busy() {
        let fs = TestFs::context();
        let err = fs
            .rename(FsPath::new(b"/"), FsPath::new(b"/root"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
    }

    #[test]
    fn link_to_root_reports_already_exists() {
        let fs = TestFs::context();
        let err = fs
            .link(FsPath::new(b"/child"), FsPath::new(b"/"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::AlreadyExists);
    }

    #[test]
    fn anonymous_create_is_honestly_unsupported_by_default() {
        let fs = TestFs::context();
        let mut options = OpenOptions::new();
        options.write(true).mode(0o600);

        let err = options
            .create_anonymous_location(fs.root_dir(), true)
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::OperationNotSupported);
    }

    #[test]
    fn rename_is_fail_closed_at_location_and_context_seams() {
        let backend = TestFs::new();
        backend.rename_supported.store(false, Ordering::Relaxed);
        let mount = Mountpoint::new_root(&Filesystem::new(backend));
        let fs = FsContext::new(mount.root_location());
        let root = fs.root_dir();

        assert!(!root.supports_rename());
        assert_eq!(
            root.rename(FsName::new(b"file"), root, FsName::new(b"renamed"))
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
        assert_eq!(
            fs.rename(FsPath::new(b"/file"), FsPath::new(b"/renamed"))
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
    }

    #[test]
    fn rename_capability_is_owned_by_the_source_directory() {
        let fs = TestFs::context();
        let root = fs.root_dir();
        let destination = fs.resolve(FsPath::new(b"/rename-disabled")).unwrap();

        assert!(root.supports_rename());
        assert!(!destination.supports_rename());
        assert_eq!(
            fs.rename(
                FsPath::new(b"/file"),
                FsPath::new(b"/rename-disabled/renamed")
            )
            .unwrap_err(),
            VfsError::Unsupported
        );
    }

    #[test]
    fn checked_same_inode_mountpoint_rename_is_a_generation_stable_noop() {
        let fs = TestFs::context();
        let root = fs.root_dir();
        let covered = root
            .lookup_no_follow_in_mount(FsName::new(b"child"))
            .unwrap();
        covered.mount(&Filesystem::new(TestFs::new())).unwrap();
        let generation = root.namespace_generation().unwrap();

        root.rename_checked(
            FsName::new(b"child"),
            &covered,
            root,
            FsName::new(b"child"),
            Some(&covered),
        )
        .unwrap();

        assert!(root.namespace_generation_is_current(generation).unwrap());
        assert!(covered.is_mountpoint());
    }

    #[test]
    fn rename_to_root_reports_resource_busy() {
        let fs = TestFs::context();
        let err = fs
            .rename(FsPath::new(b"/child"), FsPath::new(b"/"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
    }

    #[test]
    fn rename_special_destination_precedes_missing_final_source_lookup() {
        let fs = TestFs::context();
        let err = fs
            .rename(FsPath::new(b"/missing"), FsPath::new(b"/"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::ResourceBusy);

        for destination in ["/child/.", "/child/.."] {
            let err = fs
                .rename(
                    FsPath::new(b"/missing"),
                    FsPath::new(destination.as_bytes()),
                )
                .unwrap_err();
            assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
        }
    }

    #[test]
    fn rename_special_source_never_aliases_an_earlier_component() {
        let fs = TestFs::context();
        for source in ["/child/.", "/child/.."] {
            let err = fs
                .rename(FsPath::new(source.as_bytes()), FsPath::new(b"/other"))
                .unwrap_err();
            assert_eq!(err.canonicalize(), VfsError::ResourceBusy);
        }
    }

    #[test]
    fn rename_root_to_missing_parent_reports_not_found() {
        let fs = TestFs::context();
        let err = fs
            .rename(FsPath::new(b"/"), FsPath::new(b"/missing/child"))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::NotFound);
    }

    #[test]
    fn create_dir_empty_path_stays_invalid_input() {
        let fs = TestFs::context();
        let err = fs
            .create_dir(FsPath::new(b""), NodePermission::from_bits_truncate(0o755))
            .unwrap_err();
        assert_eq!(err.canonicalize(), VfsError::InvalidInput);
    }
}
