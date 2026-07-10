use alloc::{
    borrow::ToOwned,
    collections::BTreeSet,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    iter,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axpoll::{IoEvents, Pollable};
use hashbrown::HashMap;
use inherit_methods_macro::inherit_methods;

use crate::{
    DirEntry, DirEntrySink, Filesystem, FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard,
    NodeFlags, NodePermission, NodeType, OpenOptions, ReferenceKey, TypeMap, VfsError, VfsResult,
    path::{DOT, DOTDOT, PathBuf},
};

#[derive(Debug)]
pub struct Mountpoint {
    /// Root dir entry in the mountpoint.
    root: DirEntry,
    /// Location in the parent mountpoint.
    location: Mutex<Option<MountLocation>>,
    /// Children of the mountpoint.
    children: Mutex<HashMap<ReferenceKey, Arc<Self>>>,
    /// Filesystem instance owned by this mount.
    filesystem: Filesystem,
    /// Stable identity of the mounted filesystem.
    device: u64,
    /// Unique identity of this mount instance.
    mount_id: u64,
    /// Whether this is the root mount of its namespace.
    namespace_root: bool,
}

#[derive(Debug, Clone)]
struct MountLocation {
    mountpoint: Weak<Mountpoint>,
    entry: DirEntry,
}

impl MountLocation {
    fn new(location: Location) -> Self {
        Self {
            mountpoint: Arc::downgrade(&location.mountpoint),
            entry: location.entry,
        }
    }

    fn upgrade(&self) -> Option<Location> {
        Some(Location::new(self.mountpoint.upgrade()?, self.entry.clone()))
    }
}

impl Mountpoint {
    pub fn new(fs: &Filesystem, location_in_parent: Option<Location>) -> Arc<Self> {
        static MOUNT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

        let root = fs.root_dir();
        let namespace_root = location_in_parent.is_none();
        Arc::new(Self {
            root,
            location: Mutex::new(location_in_parent.map(MountLocation::new)),
            children: Mutex::default(),
            filesystem: fs.clone(),
            device: fs.device(),
            mount_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            namespace_root,
        })
    }

    pub fn new_root(fs: &Filesystem) -> Arc<Self> {
        Self::new(fs, None)
    }

    pub fn root_location(self: &Arc<Self>) -> Location {
        Location::new(self.clone(), self.root.clone())
    }

    /// Returns the location in the parent mountpoint.
    pub fn location(&self) -> Option<Location> {
        self.location.lock().as_ref()?.upgrade()
    }

    pub fn is_root(&self) -> bool {
        self.namespace_root
    }

    /// Returns the effective mountpoint.
    ///
    /// For example, first `mount /dev/sda1 /mnt` and then `mount /dev/sda2
    /// /mnt`. After the second mount is completed, the content of the first
    /// mount will be overridden (root mount -> mnt1 -> mnt2). We need to
    /// return `mnt2` for `mnt1.effective_mountpoint()`.
    pub(crate) fn effective_mountpoint(self: &Arc<Self>) -> Arc<Mountpoint> {
        let mut mountpoint = self.clone();
        loop {
            let next = mountpoint
                .children
                .lock()
                .get(&mountpoint.root.key())
                .cloned();
            if let Some(next) = next {
                mountpoint = next;
            } else {
                return mountpoint;
            }
        }
    }

    pub fn device(self: &Arc<Self>) -> u64 {
        self.device
    }

    pub fn mount_id(self: &Arc<Self>) -> u64 {
        self.mount_id
    }

    pub fn filesystem_lifetime(self: &Arc<Self>) -> Weak<()> {
        self.filesystem.lifetime_handle()
    }

    /// Returns every stable filesystem identity in this mount subtree.
    pub fn subtree_devices(self: &Arc<Self>) -> BTreeSet<u64> {
        let mut devices = BTreeSet::new();
        self.collect_subtree_devices(&mut devices);
        devices
    }

    fn collect_subtree_devices(self: &Arc<Self>, devices: &mut BTreeSet<u64>) {
        devices.insert(self.device);
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        for child in children {
            child.collect_subtree_devices(devices);
        }
    }

    fn parent_mountpoint(&self) -> Option<Arc<Mountpoint>> {
        self.location.lock().as_ref()?.mountpoint.upgrade()
    }

    fn detach_from_parent(self: &Arc<Self>, require_unused: bool) -> VfsResult<()> {
        if self.namespace_root {
            return Err(VfsError::ResourceBusy);
        }

        let mut location = self.location.lock();
        let parent_location = location.as_ref().ok_or(VfsError::InvalidInput)?;
        let parent = parent_location
            .mountpoint
            .upgrade()
            .ok_or(VfsError::InvalidInput)?;
        let key = parent_location.entry.key();
        let mut children = parent.children.lock();
        if !children
            .get(&key)
            .is_some_and(|mounted| Arc::ptr_eq(mounted, self))
        {
            return Err(VfsError::InvalidInput);
        }

        // The attached-tree reference and the caller's Location are the only
        // expected owners of an unused mount.
        if require_unused && Arc::strong_count(self) != 2 {
            return Err(VfsError::ResourceBusy);
        }

        children.remove(&key);
        *location = None;
        Ok(())
    }

    /// Flushes every distinct filesystem reachable from this mount tree.
    ///
    /// Bind mounts share a stable device identity with their source and are
    /// flushed once. Mount-tree locks are released before filesystem code runs.
    pub fn flush_all_filesystems(self: &Arc<Self>) -> VfsResult<()> {
        self.flush_all_filesystems_inner(&mut BTreeSet::new())
    }

    fn flush_all_filesystems_inner(self: &Arc<Self>, flushed: &mut BTreeSet<u64>) -> VfsResult<()> {
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        let mut first_error = None;

        if flushed.insert(self.device)
            && let Err(err) = self.filesystem.flush()
        {
            first_error = Some(err);
        }
        for child in children {
            if let Err(err) = child.flush_all_filesystems_inner(flushed)
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }

        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for Mountpoint {
    fn drop(&mut self) {
        if let Ok(root) = self.root.as_dir() {
            root.forget();
        }
    }
}

#[derive(Debug, Clone)]
pub struct Location {
    mountpoint: Arc<Mountpoint>,
    entry: DirEntry,
}

#[inherit_methods(from = "self.entry")]
impl Location {
    pub fn inode(&self) -> u64;

    pub fn filesystem(&self) -> &dyn FilesystemOps;

    pub fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> VfsResult<u64>;

    pub fn sync(&self, data_only: bool) -> VfsResult<()>;

    pub fn is_file(&self) -> bool;

    pub fn is_dir(&self) -> bool;

    pub fn node_type(&self) -> NodeType;

    pub fn read_link(&self) -> VfsResult<String>;

    pub fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize>;

    pub fn flags(&self) -> NodeFlags;

    pub fn user_data(&self) -> MutexGuard<'_, TypeMap>;
}

impl Location {
    pub fn new(mountpoint: Arc<Mountpoint>, entry: DirEntry) -> Self {
        Self { mountpoint, entry }
    }

    fn wrap(&self, entry: DirEntry) -> Self {
        Self::new(self.mountpoint.clone(), entry)
    }

    pub fn mountpoint(&self) -> &Arc<Mountpoint> {
        &self.mountpoint
    }

    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }

    pub fn name(&self) -> &str {
        self.entry.name()
    }

    pub fn parent(&self) -> Option<Self> {
        if !self.is_root_of_mount() {
            return Some(self.wrap(self.entry.parent().unwrap()));
        }
        self.mountpoint.location()?.parent()
    }

    pub fn is_root(&self) -> bool {
        self.mountpoint.is_root() && self.is_root_of_mount()
    }

    pub fn check_is_dir(&self) -> VfsResult<()> {
        self.entry.as_dir().map(|_| ())
    }

    pub fn check_is_file(&self) -> VfsResult<()> {
        self.entry.as_file().map(|_| ())
    }

    pub fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.entry.metadata()?;
        metadata.device = self.mountpoint.device();
        Ok(metadata)
    }

    /// Applies only fields the backing filesystem can persist.
    ///
    /// This is intended for inode initialization and kernel-maintained
    /// timestamps. Explicit metadata-changing syscalls should use the strict
    /// `update_metadata` operation so unsupported requests remain visible.
    pub fn update_supported_metadata(&self, mut update: MetadataUpdate) -> VfsResult<()> {
        update.retain_supported(self.filesystem().metadata_update_capabilities());
        if update.is_empty() {
            Ok(())
        } else {
            self.entry.update_metadata(update)
        }
    }

    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
        let mut components = vec![];
        let mut cur = self.clone();
        loop {
            let mut entry = cur.entry.clone();
            while !entry.ptr_eq(&cur.mountpoint.root) {
                components.push(entry.name().to_owned());
                entry = entry.parent().ok_or(VfsError::InvalidInput)?;
            }
            cur = match cur.mountpoint.location() {
                Some(loc) => loc,
                None => break,
            }
        }
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    /// Returns this entry's path relative to the root of its filesystem mount.
    pub fn path_in_mount(&self) -> VfsResult<PathBuf> {
        let mut components = vec![];
        let mut entry = self.entry.clone();
        while !entry.ptr_eq(&self.mountpoint.root) {
            components.push(entry.name().to_owned());
            entry = entry.parent().ok_or(VfsError::InvalidInput)?;
        }
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mountpoint, &other.mountpoint) && self.entry.ptr_eq(&other.entry)
    }

    pub fn is_mountpoint(&self) -> bool {
        self.mountpoint
            .children
            .lock()
            .contains_key(&self.entry.key())
    }

    pub fn is_root_of_mount(&self) -> bool {
        self.entry.ptr_eq(&self.mountpoint.root)
    }

    /// See [`Mountpoint::effective_mountpoint`].
    fn resolve_mountpoint(self) -> Self {
        if self.entry.as_dir().is_err() {
            return self;
        }
        let Some(mountpoint) = self
            .mountpoint
            .children
            .lock()
            .get(&self.entry.key())
            .cloned()
        else {
            return self;
        };
        let mountpoint = mountpoint.effective_mountpoint();
        let entry = mountpoint.root.clone();
        Self::new(mountpoint, entry)
    }

    pub fn lookup_no_follow(&self, name: &str) -> VfsResult<Self> {
        Ok(match name {
            DOT => self.clone(),
            DOTDOT => self.parent().unwrap_or_else(|| self.clone()),
            _ => {
                let loc = Self::new(self.mountpoint.clone(), self.entry.as_dir()?.lookup(name)?);
                loc.resolve_mountpoint()
            }
        })
    }

    pub fn create(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
    ) -> VfsResult<Self> {
        self.entry
            .as_dir()?
            .create(name, node_type, permission)
            .map(|entry| self.wrap(entry))
    }

    pub fn link(&self, name: &str, node: &Self) -> VfsResult<Self> {
        if !Arc::ptr_eq(&self.mountpoint, &node.mountpoint) {
            return Err(VfsError::CrossesDevices);
        }
        self.entry
            .as_dir()?
            .link(name, &node.entry)
            .map(|entry| self.wrap(entry))
    }

    pub fn rename(&self, src_name: &str, dst_dir: &Self, dst_name: &str) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mountpoint, &dst_dir.mountpoint) {
            return Err(VfsError::CrossesDevices);
        }
        let src = self.entry.as_dir()?.lookup(src_name)?;
        if src.is_dir() && !self.ptr_eq(dst_dir) && src.is_ancestor_of(&dst_dir.entry)? {
            return Err(VfsError::InvalidInput);
        }
        self.entry
            .as_dir()?
            .rename(src_name, dst_dir.entry.as_dir()?, dst_name)
    }

    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        self.entry.as_dir()?.unlink(name, is_dir)
    }

    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<Location> {
        self.entry
            .as_dir()?
            .open_file(name, options)
            .map(|entry| self.wrap(entry).resolve_mountpoint())
    }

    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.entry.as_dir()?.read_dir(offset, sink)
    }

    pub fn mount(&self, fs: &Filesystem) -> VfsResult<Arc<Mountpoint>> {
        self.check_is_dir()?;
        let result = Mountpoint::new(fs, Some(self.clone()));
        self.mountpoint
            .children
            .lock()
            .insert(self.entry.key(), result.clone());
        Ok(result)
    }

    pub fn move_mount_to(&self, target: &Self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint.namespace_root {
            return Err(VfsError::ResourceBusy);
        }
        target.check_is_dir()?;
        if !self.is_dir() {
            return Err(VfsError::NotADirectory);
        }

        let mut current = Some(target.mountpoint.clone());
        while let Some(mountpoint) = current {
            if Arc::ptr_eq(&mountpoint, &self.mountpoint) {
                return Err(VfsError::InvalidInput);
            }
            current = mountpoint.parent_mountpoint();
        }

        let mut location = self.mountpoint.location.lock();
        if let Some(old_location) = location.as_ref() {
            let old_parent = old_location
                .mountpoint
                .upgrade()
                .ok_or(VfsError::InvalidInput)?;
            let old_key = old_location.entry.key();
            let mut old_children = old_parent.children.lock();
            if !old_children
                .get(&old_key)
                .is_some_and(|mounted| Arc::ptr_eq(mounted, &self.mountpoint))
            {
                return Err(VfsError::InvalidInput);
            }
            old_children.remove(&old_key);
        }

        *location = Some(MountLocation::new(target.clone()));
        target
            .mountpoint
            .children
            .lock()
            .insert(target.entry.key(), self.mountpoint.clone());
        Ok(())
    }

    pub fn unmount(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint.namespace_root {
            return Err(VfsError::ResourceBusy);
        }
        if !self.mountpoint.children.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }
        if Arc::strong_count(&self.mountpoint) != 2 {
            return Err(VfsError::ResourceBusy);
        }
        self.mountpoint.filesystem.flush()?;
        self.mountpoint.detach_from_parent(true)
    }

    /// Lazily detaches this mount and its descendants from the namespace.
    /// Existing Locations keep the detached tree alive and usable.
    pub fn lazy_unmount(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        self.mountpoint.detach_from_parent(false)
    }

    pub fn unmount_all(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        let children = self
            .mountpoint
            .children
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for child in children {
            child.root_location().unmount_all()?;
        }
        self.mountpoint.filesystem.flush()?;
        if self.mountpoint.namespace_root {
            Ok(())
        } else {
            self.mountpoint.detach_from_parent(false)
        }
    }
}

#[inherit_methods(from = "self.entry")]
impl Pollable for Location {
    fn poll(&self) -> IoEvents;

    fn register(&self, context: &mut Context<'_>, events: IoEvents);
}
