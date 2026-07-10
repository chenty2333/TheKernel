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
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axpoll::{IoEvents, Pollable};
use hashbrown::HashMap;
use inherit_methods_macro::inherit_methods;
use spin::RwLock;

use crate::{
    DirEntry, DirEntrySink, Filesystem, FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard,
    NodeFlags, NodePermission, NodeType, OpenOptions, ReferenceKey, TypeMap, VfsError, VfsResult,
    path::{DOT, DOTDOT, PathBuf},
};

static MOUNT_TREE_LOCK: RwLock<()> = RwLock::new(());
static MOUNT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static ACTIVE_NON_ROOT_MOUNTS: AtomicUsize = AtomicUsize::new(0);

/// A hard safety bound for mount-tree traversal and nesting.
///
/// The node limit also bounds lazily detached mounts that still have live
/// locations. The depth limit keeps ancestry snapshots and path crossing
/// predictably small.
const MAX_ACTIVE_NON_ROOT_MOUNTS: usize = 65_536;
const MAX_MOUNT_TREE_DEPTH: usize = 256;

fn mount_tree_write() -> spin::RwLockWriteGuard<'static, ()> {
    // An upgradeable reader prevents new readers from entering while the
    // existing readers drain, avoiding the unbounded writer starvation of
    // spin::RwLock::write under pathname-heavy workloads.
    MOUNT_TREE_LOCK.upgradeable_read().upgrade()
}

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
    /// Shared location handle held weakly to avoid a mount/handle cycle.
    ///
    /// A live handle keeps this mount and its current ancestors alive. This
    /// preserves `..` traversal inside a lazily detached nested tree without
    /// adding another Arc to every Location.
    handle: Mutex<Weak<MountHandle>>,
    /// Set while a normal unmount flushes without the topology writer lock.
    unmounting: AtomicBool,
    /// Whether this mount consumed one slot in [`ACTIVE_NON_ROOT_MOUNTS`].
    counted_non_root: bool,
}

#[derive(Debug)]
struct MountHandle {
    mountpoint: Arc<Mountpoint>,
    ancestors: Mutex<Vec<Arc<Mountpoint>>>,
    /// Serializes creation of another Location with normal-unmount commit.
    admission: RwLock<()>,
    /// Records a Location admitted during the lock-free unmount flush window.
    ///
    /// A sticky bit avoids an unconditional shared-cache-line RMW on normal
    /// pathname operations while still letting phase two detect a transient
    /// Location that was created and dropped before revalidation.
    admitted_during_unmount: AtomicBool,
}

/// Filesystem and entry ownership retained for delayed cached writeback.
///
/// Unlike [`Location`], this anchor deliberately owns no mountpoint or mount
/// handle, so cleanly flushing internal cache state cannot make a normal
/// unmount spuriously busy. The filesystem clone delays backend teardown and
/// the entry keeps the inode available for writeback.
#[derive(Debug, Clone)]
pub struct WritebackAnchor {
    filesystem: Filesystem,
    entry: DirEntry,
}

impl WritebackAnchor {
    pub fn device(&self) -> u64 {
        self.filesystem.device()
    }

    pub fn inode(&self) -> u64 {
        self.entry.inode()
    }

    pub fn filesystem(&self) -> &dyn FilesystemOps {
        self.entry.filesystem()
    }

    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }
}

#[derive(Debug, Clone)]
struct MountLocation {
    mountpoint: Weak<Mountpoint>,
    entry: DirEntry,
}

impl MountLocation {
    fn new(location: &Location) -> Self {
        Self {
            mountpoint: Arc::downgrade(location.mountpoint()),
            entry: location.entry.clone(),
        }
    }

    fn upgrade_locked(&self) -> Option<Location> {
        Some(Location::new_locked(
            self.mountpoint.upgrade()?,
            self.entry.clone(),
        ))
    }
}

impl Mountpoint {
    fn new(fs: &Filesystem, location_in_parent: Option<&Location>) -> Arc<Self> {
        Self::new_with_root(fs, fs.root_dir(), location_in_parent)
    }

    fn new_with_root(
        fs: &Filesystem,
        root: DirEntry,
        location_in_parent: Option<&Location>,
    ) -> Arc<Self> {
        let namespace_root = location_in_parent.is_none();
        Arc::new(Self {
            root,
            location: Mutex::new(location_in_parent.map(MountLocation::new)),
            children: Mutex::default(),
            filesystem: fs.clone(),
            device: fs.device(),
            mount_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            namespace_root,
            handle: Mutex::new(Weak::new()),
            unmounting: AtomicBool::new(false),
            counted_non_root: !namespace_root,
        })
    }

    pub fn new_root(fs: &Filesystem) -> Arc<Self> {
        Self::new(fs, None)
    }

    fn new_mounted(
        fs: &Filesystem,
        root: DirEntry,
        location_in_parent: &Location,
    ) -> VfsResult<Arc<Self>> {
        ACTIVE_NON_ROOT_MOUNTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_NON_ROOT_MOUNTS).then_some(active + 1)
            })
            .map_err(|_| VfsError::NoMemory)?;
        Ok(Self::new_with_root(fs, root, Some(location_in_parent)))
    }

    pub fn root_location(self: &Arc<Self>) -> Location {
        Location::new(self.clone(), self.root.clone())
    }

    /// Returns the location in the parent mountpoint.
    pub fn location(&self) -> Option<Location> {
        let _tree = MOUNT_TREE_LOCK.read();
        self.location_locked()
    }

    fn location_locked(&self) -> Option<Location> {
        self.location.lock().as_ref()?.upgrade_locked()
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
    fn effective_mountpoint_locked(self: &Arc<Self>) -> VfsResult<Arc<Mountpoint>> {
        let mut mountpoint = self.clone();
        let mut visited = BTreeSet::new();
        for _ in 0..MAX_MOUNT_TREE_DEPTH {
            if !visited.insert(mountpoint.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if mountpoint.unmounting.load(Ordering::Acquire) {
                return Err(VfsError::ResourceBusy);
            }
            let next = mountpoint
                .children
                .lock()
                .get(&mountpoint.root.key())
                .cloned();
            if let Some(next) = next {
                mountpoint = next;
            } else {
                return Ok(mountpoint);
            }
        }
        Err(VfsError::ResourceBusy)
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

    pub fn writeback_anchor(&self, entry: DirEntry) -> WritebackAnchor {
        WritebackAnchor {
            filesystem: self.filesystem.clone(),
            entry,
        }
    }

    /// Returns every stable filesystem identity in this mount subtree.
    pub fn subtree_devices(self: &Arc<Self>) -> VfsResult<BTreeSet<u64>> {
        let _tree = MOUNT_TREE_LOCK.read();
        Ok(self
            .subtree_nodes_locked()?
            .into_iter()
            .map(|(mountpoint, _)| mountpoint.device)
            .collect())
    }

    fn subtree_nodes_locked(self: &Arc<Self>) -> VfsResult<Vec<(Arc<Mountpoint>, usize)>> {
        let mut pending = vec![(self.clone(), 0)];
        let mut visited = BTreeSet::new();
        let mut nodes = Vec::new();
        while let Some((mountpoint, depth)) = pending.pop() {
            if depth > MAX_MOUNT_TREE_DEPTH {
                return Err(VfsError::ResourceBusy);
            }
            if !visited.insert(mountpoint.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if visited.len() > MAX_ACTIVE_NON_ROOT_MOUNTS + 1 {
                return Err(VfsError::NoMemory);
            }
            let children = mountpoint
                .children
                .lock()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            pending.extend(children.into_iter().map(|child| (child, depth + 1)));
            nodes.push((mountpoint, depth));
        }
        Ok(nodes)
    }

    fn parent_mountpoint_locked(&self) -> Option<Arc<Mountpoint>> {
        self.location.lock().as_ref()?.mountpoint.upgrade()
    }

    fn current_ancestors_locked(&self) -> Vec<Arc<Mountpoint>> {
        let mut ancestors = Vec::new();
        let mut visited = BTreeSet::new();
        let mut current = self.parent_mountpoint_locked();
        for _ in 0..MAX_MOUNT_TREE_DEPTH {
            let Some(mountpoint) = current else {
                break;
            };
            if !visited.insert(mountpoint.mount_id) {
                break;
            }
            current = mountpoint.parent_mountpoint_locked();
            ancestors.push(mountpoint);
        }
        ancestors
    }

    fn location_handle_locked(self: &Arc<Self>) -> Arc<MountHandle> {
        if let Some(handle) = self.handle.lock().upgrade() {
            return handle;
        }

        // Compute ancestry without holding the handle slot. Topology is stable
        // under MOUNT_TREE_LOCK, and this lock order avoids handle/location
        // inversion with unmount validation.
        let ancestors = self.current_ancestors_locked();
        let mut slot = self.handle.lock();
        if let Some(handle) = slot.upgrade() {
            return handle;
        }
        let handle = Arc::new(MountHandle {
            mountpoint: self.clone(),
            ancestors: Mutex::new(ancestors),
            admission: RwLock::new(()),
            admitted_during_unmount: AtomicBool::new(false),
        });
        *slot = Arc::downgrade(&handle);
        handle
    }

    fn active_location_count(&self) -> usize {
        self.handle.lock().strong_count()
    }

    fn refresh_active_handles_locked(self: &Arc<Self>) {
        let mut pending = vec![self.clone()];
        let mut visited = BTreeSet::new();
        while let Some(mountpoint) = pending.pop() {
            if visited.len() >= MAX_ACTIVE_NON_ROOT_MOUNTS + 1 {
                break;
            }
            if !visited.insert(mountpoint.mount_id) {
                continue;
            }
            let ancestors = mountpoint.current_ancestors_locked();
            let handle = { mountpoint.handle.lock().upgrade() };
            if let Some(handle) = handle {
                *handle.ancestors.lock() = ancestors;
            }
            pending.extend(mountpoint.children.lock().values().cloned());
        }
    }

    fn has_unmounting_ancestor_locked(self: &Arc<Self>) -> bool {
        let mut current = Some(self.clone());
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            let Some(mountpoint) = current else {
                return false;
            };
            if mountpoint.unmounting.load(Ordering::Acquire) {
                return true;
            }
            current = mountpoint.parent_mountpoint_locked();
        }
        true
    }

    fn attached_depth_locked(self: &Arc<Self>) -> VfsResult<usize> {
        let mut current = self.clone();
        let mut visited = BTreeSet::new();
        for depth in 0..=MAX_MOUNT_TREE_DEPTH {
            if !visited.insert(current.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if current.unmounting.load(Ordering::Acquire) {
                return Err(VfsError::ResourceBusy);
            }
            if current.namespace_root {
                return Ok(depth);
            }
            current = current
                .parent_mountpoint_locked()
                .ok_or(VfsError::InvalidInput)?;
        }
        Err(VfsError::ResourceBusy)
    }

    fn subtree_has_unmounting_locked(self: &Arc<Self>) -> VfsResult<bool> {
        Ok(self
            .subtree_nodes_locked()?
            .iter()
            .any(|(mountpoint, _)| mountpoint.unmounting.load(Ordering::Acquire)))
    }

    fn validate_detach_locked(self: &Arc<Self>, require_unused: bool) -> VfsResult<()> {
        if self.namespace_root {
            return Err(VfsError::ResourceBusy);
        }

        if require_unused && (self.active_location_count() != 1 || !self.children.lock().is_empty())
        {
            return Err(VfsError::ResourceBusy);
        }

        let (parent_mountpoint, key) = {
            let location = self.location.lock();
            let parent_location = location.as_ref().ok_or(VfsError::InvalidInput)?;
            (
                parent_location.mountpoint.clone(),
                parent_location.entry.key(),
            )
        };
        let parent = parent_mountpoint.upgrade().ok_or(VfsError::InvalidInput)?;
        let children = parent.children.lock();
        if !children
            .get(&key)
            .is_some_and(|mounted| Arc::ptr_eq(mounted, self))
        {
            return Err(VfsError::InvalidInput);
        }

        Ok(())
    }

    fn detach_from_parent_locked(self: &Arc<Self>, require_unused: bool) -> VfsResult<()> {
        self.validate_detach_locked(require_unused)?;

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

        children.remove(&key);
        *location = None;
        drop(children);
        drop(location);
        self.refresh_active_handles_locked();
        Ok(())
    }

    fn detach_from_parent(self: &Arc<Self>, require_unused: bool) -> VfsResult<()> {
        let _tree = mount_tree_write();
        if self.has_unmounting_ancestor_locked()
            || (!require_unused && self.subtree_has_unmounting_locked()?)
        {
            return Err(VfsError::ResourceBusy);
        }
        self.detach_from_parent_locked(require_unused)
    }

    /// Flushes every distinct filesystem reachable from this mount tree.
    ///
    /// Bind mounts share a stable device identity with their source and are
    /// flushed once. Mount-tree locks are released before filesystem code runs.
    pub fn flush_all_filesystems(self: &Arc<Self>) -> VfsResult<()> {
        let filesystems = {
            let _tree = MOUNT_TREE_LOCK.read();
            let mut seen = BTreeSet::new();
            self.subtree_nodes_locked()?
                .into_iter()
                .filter_map(|(mountpoint, _)| {
                    seen.insert(mountpoint.device)
                        .then_some(mountpoint.filesystem.clone())
                })
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for filesystem in filesystems {
            if let Err(err) = filesystem.flush()
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
        if self.counted_non_root {
            let old = ACTIVE_NON_ROOT_MOUNTS.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(old != 0, "active mount count underflow");
        }
    }
}

#[derive(Debug)]
pub struct Location {
    mount: Arc<MountHandle>,
    entry: DirEntry,
}

impl Clone for Location {
    fn clone(&self) -> Self {
        let _admission = self.mount.admission.read();
        if self.mount.mountpoint.unmounting.load(Ordering::Acquire) {
            self.mount
                .admitted_during_unmount
                .store(true, Ordering::Release);
        }
        Self {
            mount: self.mount.clone(),
            entry: self.entry.clone(),
        }
    }
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
        let _tree = MOUNT_TREE_LOCK.read();
        Self::new_locked(mountpoint, entry)
    }

    fn new_locked(mountpoint: Arc<Mountpoint>, entry: DirEntry) -> Self {
        let mount = mountpoint.location_handle_locked();
        let _admission = mount.admission.read();
        if mount.mountpoint.unmounting.load(Ordering::Acquire) {
            mount
                .admitted_during_unmount
                .store(true, Ordering::Release);
        }
        // `mount` already owns the admitted handle reference. Releasing the
        // read guard before moving that reference cannot hide it from phase
        // two: the strong count remains elevated until the Location is built.
        drop(_admission);
        Self {
            mount,
            entry,
        }
    }

    fn wrap(&self, entry: DirEntry) -> Self {
        let _admission = self.mount.admission.read();
        if self.mount.mountpoint.unmounting.load(Ordering::Acquire) {
            self.mount
                .admitted_during_unmount
                .store(true, Ordering::Release);
        }
        Self {
            mount: self.mount.clone(),
            entry,
        }
    }

    pub fn mountpoint(&self) -> &Arc<Mountpoint> {
        &self.mount.mountpoint
    }

    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }

    pub fn writeback_anchor(&self) -> WritebackAnchor {
        self.mountpoint().writeback_anchor(self.entry.clone())
    }

    pub fn name(&self) -> &str {
        self.entry.name()
    }

    pub fn parent(&self) -> Option<Self> {
        let _tree = MOUNT_TREE_LOCK.read();
        let mut current = self.clone();
        let mut visited = BTreeSet::new();
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            if !current.is_root_of_mount() {
                return current.entry.parent().map(|entry| current.wrap(entry));
            }
            if !visited.insert(current.mountpoint().mount_id) {
                return None;
            }
            current = current.mountpoint().location_locked()?;
        }
        None
    }

    pub fn is_root(&self) -> bool {
        self.mountpoint().is_root() && self.is_root_of_mount()
    }

    pub fn check_is_dir(&self) -> VfsResult<()> {
        self.entry.as_dir().map(|_| ())
    }

    pub fn check_is_file(&self) -> VfsResult<()> {
        self.entry.as_file().map(|_| ())
    }

    pub fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.entry.metadata()?;
        metadata.device = self.mountpoint().device();
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
        let _tree = MOUNT_TREE_LOCK.read();
        let mut components = vec![];
        let mut cur = self.clone();
        let mut visited = BTreeSet::new();
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            if !visited.insert(cur.mountpoint().mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            let mut entry = cur.entry.clone();
            while !entry.ptr_eq(&cur.mountpoint().root) {
                components.push(entry.name().to_owned());
                entry = entry.parent().ok_or(VfsError::InvalidInput)?;
            }
            cur = match cur.mountpoint().location_locked() {
                Some(loc) => loc,
                None => {
                    return Ok(iter::once("/")
                        .chain(components.iter().map(String::as_str).rev())
                        .collect());
                }
            };
        }
        Err(VfsError::ResourceBusy)
    }

    /// Returns this entry's path relative to the root of its filesystem mount.
    pub fn path_in_mount(&self) -> VfsResult<PathBuf> {
        let mut components = vec![];
        let mut entry = self.entry.clone();
        while !entry.ptr_eq(&self.mountpoint().root) {
            components.push(entry.name().to_owned());
            entry = entry.parent().ok_or(VfsError::InvalidInput)?;
        }
        Ok(iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect())
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.mountpoint(), other.mountpoint()) && self.entry.ptr_eq(&other.entry)
    }

    pub fn is_mountpoint(&self) -> bool {
        let _tree = MOUNT_TREE_LOCK.read();
        self.mountpoint()
            .children
            .lock()
            .contains_key(&self.entry.key())
    }

    pub fn is_root_of_mount(&self) -> bool {
        self.entry.ptr_eq(&self.mountpoint().root)
    }

    /// See [`Mountpoint::effective_mountpoint`].
    fn resolve_mountpoint(self) -> VfsResult<Self> {
        if self.entry.as_dir().is_err() {
            return Ok(self);
        }
        let _tree = MOUNT_TREE_LOCK.read();
        if self.mountpoint().has_unmounting_ancestor_locked() {
            return Err(VfsError::ResourceBusy);
        }
        let Some(mountpoint) = self
            .mountpoint()
            .children
            .lock()
            .get(&self.entry.key())
            .cloned()
        else {
            return Ok(self);
        };
        let mountpoint = mountpoint.effective_mountpoint_locked()?;
        let entry = mountpoint.root.clone();
        Ok(Self::new_locked(mountpoint, entry))
    }

    pub fn lookup_no_follow(&self, name: &str) -> VfsResult<Self> {
        match name {
            DOT => Ok(self.clone()),
            DOTDOT => Ok(self.parent().unwrap_or_else(|| self.clone())),
            _ => {
                let loc = self.wrap(self.entry.as_dir()?.lookup(name)?);
                loc.resolve_mountpoint()
            }
        }
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
        if !Arc::ptr_eq(self.mountpoint(), node.mountpoint()) {
            return Err(VfsError::CrossesDevices);
        }
        self.entry
            .as_dir()?
            .link(name, &node.entry)
            .map(|entry| self.wrap(entry))
    }

    pub fn rename(&self, src_name: &str, dst_dir: &Self, dst_name: &str) -> VfsResult<()> {
        if !Arc::ptr_eq(self.mountpoint(), dst_dir.mountpoint()) {
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
            .and_then(|entry| self.wrap(entry).resolve_mountpoint())
    }

    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.entry.as_dir()?.read_dir(offset, sink)
    }

    pub fn mount(&self, fs: &Filesystem) -> VfsResult<Arc<Mountpoint>> {
        self.check_is_dir()?;
        // `root_dir` is an open filesystem callback. Invoke it before taking
        // the global topology writer so a backend cannot stall pathname
        // readers or re-enter the mount tree while that writer is held.
        let root = fs.root_dir();
        let _tree = mount_tree_write();
        let parent_depth = self.mountpoint().attached_depth_locked()?;
        if parent_depth >= MAX_MOUNT_TREE_DEPTH {
            return Err(VfsError::ResourceBusy);
        }
        let key = self.entry.key();
        if self.mountpoint().children.lock().contains_key(&key) {
            return Err(VfsError::ResourceBusy);
        }
        let result = Mountpoint::new_mounted(fs, root, self)?;
        self.mountpoint()
            .children
            .lock()
            .insert(key, result.clone());
        Ok(result)
    }

    pub fn move_mount_to(&self, target: &Self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint().namespace_root {
            return Err(VfsError::ResourceBusy);
        }
        target.check_is_dir()?;
        if !self.is_dir() {
            return Err(VfsError::NotADirectory);
        }

        let _tree = mount_tree_write();
        self.mountpoint().attached_depth_locked()?;
        let target_depth = target.mountpoint().attached_depth_locked()?;
        let subtree = self.mountpoint().subtree_nodes_locked()?;
        if subtree
            .iter()
            .any(|(mountpoint, _)| mountpoint.unmounting.load(Ordering::Acquire))
        {
            return Err(VfsError::ResourceBusy);
        }
        let subtree_height = subtree.iter().map(|(_, depth)| *depth).max().unwrap_or(0);
        if target_depth
            .checked_add(1)
            .and_then(|depth| depth.checked_add(subtree_height))
            .is_none_or(|depth| depth > MAX_MOUNT_TREE_DEPTH)
        {
            return Err(VfsError::ResourceBusy);
        }

        let mut current = Some(target.mountpoint().clone());
        let mut visited = BTreeSet::new();
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            let Some(mountpoint) = current else {
                break;
            };
            if !visited.insert(mountpoint.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if Arc::ptr_eq(&mountpoint, self.mountpoint()) {
                return Err(VfsError::InvalidInput);
            }
            current = mountpoint.parent_mountpoint_locked();
        }

        let target_key = target.entry.key();
        if target
            .mountpoint()
            .children
            .lock()
            .contains_key(&target_key)
        {
            return Err(VfsError::ResourceBusy);
        }

        let mut location = self.mountpoint().location.lock();
        if let Some(old_location) = location.as_ref() {
            let old_parent = old_location
                .mountpoint
                .upgrade()
                .ok_or(VfsError::InvalidInput)?;
            let old_key = old_location.entry.key();
            let mut old_children = old_parent.children.lock();
            if !old_children
                .get(&old_key)
                .is_some_and(|mounted| Arc::ptr_eq(mounted, self.mountpoint()))
            {
                return Err(VfsError::InvalidInput);
            }
            old_children.remove(&old_key);
        }

        *location = Some(MountLocation::new(target));
        target
            .mountpoint()
            .children
            .lock()
            .insert(target_key, self.mountpoint().clone());
        drop(location);
        self.mountpoint().refresh_active_handles_locked();
        Ok(())
    }

    /// Flushes and detaches this mount when this is its only live Location.
    ///
    /// Consuming `self` is part of the exclusivity contract. A borrowed
    /// `Location` can itself be shared through `Arc<Location>` without adding
    /// another mount-handle reference, which would make a strong-count-only
    /// busy check unable to prove that no other thread can use the same lease
    /// during the lock-free flush window.
    pub fn unmount(self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint().namespace_root {
            return Err(VfsError::ResourceBusy);
        }

        {
            let _tree = mount_tree_write();
            let _admission = self.mount.admission.upgradeable_read().upgrade();
            if self.mountpoint().has_unmounting_ancestor_locked() {
                return Err(VfsError::ResourceBusy);
            }
            self.mountpoint().validate_detach_locked(true)?;
            self.mount
                .admitted_during_unmount
                .store(false, Ordering::Release);
            self.mountpoint().unmounting.store(true, Ordering::Release);
        }

        let flush_result = self.mountpoint().filesystem.flush();
        let _tree = mount_tree_write();
        let _admission = self.mount.admission.upgradeable_read().upgrade();
        let result = match flush_result {
            Ok(())
                if !self
                    .mount
                    .admitted_during_unmount
                    .load(Ordering::Acquire) =>
            {
                self.mountpoint().detach_from_parent_locked(true)
            }
            Ok(()) => Err(VfsError::ResourceBusy),
            Err(err) => Err(err),
        };
        self.mountpoint().unmounting.store(false, Ordering::Release);
        result
    }

    /// Lazily detaches this mount and its descendants from the namespace.
    /// Existing Locations keep the detached tree alive and usable.
    pub fn lazy_unmount(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        self.mountpoint().detach_from_parent(false)
    }

    /// Flushes and detaches this namespace tree after users have been quiesced.
    ///
    /// The reservation below freezes mount topology and new pathname crossing;
    /// it does not freeze I/O through already-open Files, DirEntries, or
    /// Locations. Shutdown callers must stop and drain those operations first.
    pub fn unmount_all(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }

        let mut mounts = {
            let _tree = mount_tree_write();
            if self.mountpoint().has_unmounting_ancestor_locked() {
                return Err(VfsError::ResourceBusy);
            }
            let mounts = self.mountpoint().subtree_nodes_locked()?;
            if mounts
                .iter()
                .any(|(mountpoint, _)| mountpoint.unmounting.load(Ordering::Acquire))
            {
                return Err(VfsError::ResourceBusy);
            }
            for (mountpoint, _) in &mounts {
                mountpoint.unmounting.store(true, Ordering::Release);
            }
            mounts
        };
        mounts.sort_unstable_by_key(|(_, depth)| core::cmp::Reverse(*depth));

        let mut flush_error = None;
        for (mountpoint, _) in &mounts {
            if let Err(err) = mountpoint.filesystem.flush() {
                flush_error = Some(err);
                break;
            }
        }

        let _tree = mount_tree_write();
        if let Some(err) = flush_error {
            for (mountpoint, _) in &mounts {
                mountpoint.unmounting.store(false, Ordering::Release);
            }
            return Err(err);
        }

        let mut detach_error = None;
        for (mountpoint, _) in &mounts {
            if !mountpoint.namespace_root {
                if let Err(err) = mountpoint.detach_from_parent_locked(false) {
                    detach_error = Some(err);
                    break;
                }
            }
        }
        for (mountpoint, _) in &mounts {
            mountpoint.unmounting.store(false, Ordering::Release);
        }
        detach_error.map_or(Ok(()), Err)
    }
}

#[inherit_methods(from = "self.entry")]
impl Pollable for Location {
    fn poll(&self) -> IoEvents;

    fn register(&self, context: &mut Context<'_>, events: IoEvents);
}
