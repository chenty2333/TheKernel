use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use hashbrown::{HashMap, HashSet};
use inherit_methods_macro::inherit_methods;
use spin::{Once, RwLock};

use crate::{
    AnonymousOptions, CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, ExportHandle,
    Filesystem, FilesystemIdentity, FilesystemOps, Metadata, MetadataUpdate, Mutex, MutexGuard,
    NamedCreateOptions, NamespaceGeneration, NodeFlags, NodePermission, NodeType, OpenOptions,
    ReferenceKey, TypeMap, VfsError, VfsResult, WeakFilesystemIdentity, XattrProvider,
    XattrSetMode,
    path::{DOT, DOTDOT, PathBuf, try_build_absolute_path},
    unsupported_xattr,
};

static MOUNT_TREE_LOCK: RwLock<()> = RwLock::new(());
// Keep the mount identity in the Linux ``unique`` ID domain.  In particular,
// do not let it be mistaken for the recyclable 32-bit mountinfo ID.
static MOUNT_ID_COUNTER: AtomicU64 = AtomicU64::new((1u64 << 32) + 1);
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

fn optional_lookup<T>(result: VfsResult<T>) -> VfsResult<Option<T>> {
    match result {
        Ok(entry) => Ok(Some(entry)),
        Err(err) if err.canonicalize() == VfsError::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn try_push_path_component(components: &mut Vec<DirEntry>, entry: &DirEntry) -> VfsResult<()> {
    components.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    components.push(entry.clone());
    Ok(())
}

/// Checks directory ancestry by stable backend inode identity instead of
/// dentry allocation identity.
///
/// A cache may replace a dentry wrapper while an open directory handle keeps
/// the old wrapper. Directory hard links are forbidden, so inode identity is
/// unambiguous within one exact mount. Backends exposing only synthetic inode
/// values must keep rename disabled until they provide a stable identity.
fn entry_is_same_or_ancestor_by_inode(ancestor: &DirEntry, descendant: &DirEntry) -> bool {
    let ancestor_inode = ancestor.inode();
    let mut current = descendant.clone();
    loop {
        if current.inode() == ancestor_inode {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        current = parent;
    }
}

fn reserve_non_root_mount() -> VfsResult<()> {
    ACTIVE_NON_ROOT_MOUNTS
        .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_ACTIVE_NON_ROOT_MOUNTS).then_some(active + 1)
        })
        .map(|_| ())
        .map_err(|_| VfsError::NoMemory)
}

fn release_non_root_mount_reservation() {
    let old = ACTIVE_NON_ROOT_MOUNTS.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(old != 0, "active mount count underflow");
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
    namespace_root: AtomicBool,
    /// Preallocated shared location lease. Keeping one baseline reference in
    /// the mount avoids allocating from pathname traversal or under the mount
    /// tree lock; `MountHandle` itself owns no reference back to this mount.
    handle: Arc<MountHandle>,
    /// Set while a normal unmount flushes without the topology writer lock.
    unmounting: AtomicBool,
    /// Personality-specific immutable extension set. Mutable values inside it
    /// use their own synchronization, keeping mount-policy reads lock-free.
    extensions: MountExtensions,
    /// Whether this mount consumed one slot in [`ACTIVE_NON_ROOT_MOUNTS`].
    counted_non_root: bool,
}

struct MountExtensions {
    values: Once<TypeMap>,
}

impl MountExtensions {
    fn new(initial: Option<TypeMap>) -> Self {
        let extensions = Self {
            values: Once::new(),
        };
        if let Some(initial) = initial {
            extensions.values.call_once(|| initial);
        }
        extensions
    }

    fn initialize(&self, extensions: TypeMap) -> VfsResult<()> {
        let mut installed = false;
        self.values.call_once(|| {
            installed = true;
            extensions
        });
        if installed {
            Ok(())
        } else {
            Err(VfsError::AlreadyExists)
        }
    }

    fn get_ref<T: core::any::Any + Send + Sync>(&self) -> Option<&T> {
        self.values.get()?.get_ref::<T>()
    }

    fn get<T: core::any::Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.values.get()?.get::<T>()
    }
}

impl fmt::Debug for MountExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountExtensions")
            .field("initialized", &self.values.get().is_some())
            .finish()
    }
}

#[derive(Debug)]
struct MountHandle {
    ancestors: Mutex<Vec<Arc<Mountpoint>>>,
    users: AtomicUsize,
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
    fn new(
        fs: &Filesystem,
        location_in_parent: Option<&Location>,
        extensions: Option<TypeMap>,
    ) -> Arc<Self> {
        Self::new_with_root(
            fs,
            fs.root_dir(),
            location_in_parent,
            extensions,
            location_in_parent.is_none(),
            location_in_parent.is_some(),
        )
    }

    fn new_with_root(
        fs: &Filesystem,
        root: DirEntry,
        location_in_parent: Option<&Location>,
        extensions: Option<TypeMap>,
        namespace_root: bool,
        counted_non_root: bool,
    ) -> Arc<Self> {
        let handle = Arc::new(MountHandle {
            ancestors: Mutex::new(Vec::with_capacity(MAX_MOUNT_TREE_DEPTH)),
            users: AtomicUsize::new(0),
            admission: RwLock::new(()),
            admitted_during_unmount: AtomicBool::new(false),
        });
        let mountpoint = Arc::new(Self {
            root,
            location: Mutex::new(location_in_parent.map(MountLocation::new)),
            children: Mutex::default(),
            filesystem: fs.clone(),
            device: fs.device(),
            mount_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            namespace_root: AtomicBool::new(namespace_root),
            handle,
            unmounting: AtomicBool::new(false),
            extensions: MountExtensions::new(extensions),
            counted_non_root,
        });
        fs.retain_mount_root(&mountpoint.root);
        mountpoint
    }

    fn try_new_with_root(
        fs: &Filesystem,
        root: DirEntry,
        location_in_parent: Option<&Location>,
        extensions: Option<TypeMap>,
        namespace_root: bool,
        counted_non_root: bool,
    ) -> VfsResult<Arc<Self>> {
        let mut ancestors = Vec::new();
        ancestors
            .try_reserve_exact(MAX_MOUNT_TREE_DEPTH)
            .map_err(|_| VfsError::NoMemory)?;
        let handle = Arc::try_new(MountHandle {
            ancestors: Mutex::new(ancestors),
            users: AtomicUsize::new(0),
            admission: RwLock::new(()),
            admitted_during_unmount: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let mountpoint = Arc::try_new(Self {
            root,
            location: Mutex::new(location_in_parent.map(MountLocation::new)),
            children: Mutex::default(),
            filesystem: fs.clone(),
            device: fs.device(),
            mount_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            namespace_root: AtomicBool::new(namespace_root),
            handle,
            unmounting: AtomicBool::new(false),
            extensions: MountExtensions::new(extensions),
            counted_non_root,
        })
        .map_err(|_| VfsError::NoMemory)?;
        fs.retain_mount_root(&mountpoint.root);
        Ok(mountpoint)
    }

    pub fn new_root(fs: &Filesystem) -> Arc<Self> {
        Self::new(fs, None, None)
    }

    pub fn new_detached(fs: &Filesystem) -> VfsResult<Arc<Self>> {
        Self::new_detached_with_extensions(fs, TypeMap::new())
    }

    pub fn new_detached_with_extensions(
        fs: &Filesystem,
        extensions: TypeMap,
    ) -> VfsResult<Arc<Self>> {
        let root = fs.root_dir();
        reserve_non_root_mount()?;
        match Self::try_new_with_root(fs, root, None, Some(extensions), false, true) {
            Ok(mountpoint) => Ok(mountpoint),
            Err(error) => {
                release_non_root_mount_reservation();
                Err(error)
            }
        }
    }

    fn new_mounted(
        fs: &Filesystem,
        root: DirEntry,
        location_in_parent: &Location,
        extensions: Option<TypeMap>,
    ) -> VfsResult<Arc<Self>> {
        reserve_non_root_mount()?;
        match Self::try_new_with_root(fs, root, Some(location_in_parent), extensions, false, true) {
            Ok(mountpoint) => Ok(mountpoint),
            Err(error) => {
                release_non_root_mount_reservation();
                Err(error)
            }
        }
    }

    pub fn root_location(self: &Arc<Self>) -> Location {
        Location::new(self.clone(), self.root.clone())
    }

    pub fn attach_to(self: &Arc<Self>, target: &Location) -> VfsResult<()> {
        self.root_location().move_mount_to(target)
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
        self.namespace_root.load(Ordering::Acquire)
    }

    pub fn is_attached(&self) -> bool {
        let _tree = MOUNT_TREE_LOCK.read();
        self.namespace_root.load(Ordering::Acquire) || self.location.lock().is_some()
    }

    /// Returns the effective mountpoint.
    ///
    /// For example, first `mount /dev/sda1 /mnt` and then `mount /dev/sda2
    /// /mnt`. After the second mount is completed, the content of the first
    /// mount will be overridden (root mount -> mnt1 -> mnt2). We need to
    /// return `mnt2` for `mnt1.effective_mountpoint()`.
    fn effective_mountpoint_locked(self: &Arc<Self>) -> VfsResult<Arc<Mountpoint>> {
        let mut mountpoint = self.clone();
        for _ in 0..MAX_MOUNT_TREE_DEPTH {
            if mountpoint.unmounting.load(Ordering::Acquire) {
                return Err(VfsError::ResourceBusy);
            }
            let next = mountpoint
                .children
                .lock()
                .get(&mountpoint.root.key_ref())
                .cloned();
            if let Some(next) = next {
                mountpoint = next;
            } else {
                return Ok(mountpoint);
            }
        }
        Err(VfsError::FilesystemLoop)
    }

    pub fn device(self: &Arc<Self>) -> u64 {
        self.device
    }

    pub fn mount_id(self: &Arc<Self>) -> u64 {
        self.mount_id
    }

    pub fn filesystem_identity(self: &Arc<Self>) -> FilesystemIdentity {
        self.filesystem.identity()
    }

    pub fn filesystem_handle(self: &Arc<Self>) -> Filesystem {
        self.filesystem.clone()
    }

    pub fn encode_export_handle(self: &Arc<Self>, location: &Location) -> VfsResult<ExportHandle> {
        if !Arc::ptr_eq(self, location.mountpoint()) {
            return Err(VfsError::CrossesDevices);
        }
        self.filesystem.encode_export_handle(location.entry())
    }

    pub fn decode_export_handle(self: &Arc<Self>, handle: ExportHandle) -> VfsResult<Location> {
        // Keep a location admission across backend lookup so a normal unmount
        // cannot pass its no-users phase between decode and publication.
        let anchor = self.root_location();
        let entry = self.filesystem.decode_export_handle(handle)?;
        let result = anchor.wrap(entry);
        drop(anchor);
        Ok(result)
    }

    pub fn export_handle_is_descendant(
        self: &Arc<Self>,
        ancestor: &Location,
        handle: ExportHandle,
    ) -> VfsResult<bool> {
        if !Arc::ptr_eq(self, ancestor.mountpoint()) {
            return Err(VfsError::CrossesDevices);
        }
        self.filesystem
            .export_handle_is_descendant(ancestor.entry(), handle)
    }

    pub fn filesystem_identity_weak(self: &Arc<Self>) -> WeakFilesystemIdentity {
        self.filesystem.identity_weak()
    }

    pub fn initialize_extensions(&self, extensions: TypeMap) -> VfsResult<()> {
        self.extensions.initialize(extensions)
    }

    pub fn extension<T: core::any::Any + Send + Sync>(&self) -> Option<&T> {
        self.extensions.get_ref::<T>()
    }

    pub fn extension_shared<T: core::any::Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.extensions.get::<T>()
    }

    pub fn writeback_anchor(&self, entry: DirEntry) -> WritebackAnchor {
        WritebackAnchor {
            filesystem: self.filesystem.clone(),
            entry,
        }
    }

    /// Returns every stable filesystem identity in this mount subtree.
    pub fn subtree_devices(self: &Arc<Self>) -> VfsResult<Vec<u64>> {
        let _tree = MOUNT_TREE_LOCK.read();
        let nodes = self.subtree_nodes_locked()?;
        let mut seen = HashSet::new();
        seen.try_reserve(nodes.len())
            .map_err(|_| VfsError::NoMemory)?;
        let mut devices = Vec::new();
        devices
            .try_reserve(nodes.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (mountpoint, _) in nodes {
            if seen.insert(mountpoint.device) {
                devices.push(mountpoint.device);
            }
        }
        Ok(devices)
    }

    pub fn subtree_mountpoints(self: &Arc<Self>) -> VfsResult<Vec<Arc<Mountpoint>>> {
        let _tree = MOUNT_TREE_LOCK.read();
        let nodes = self.subtree_nodes_locked()?;
        let mut mountpoints = Vec::new();
        mountpoints
            .try_reserve(nodes.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (mountpoint, _) in nodes {
            mountpoints.push(mountpoint);
        }
        Ok(mountpoints)
    }

    fn subtree_nodes_locked(self: &Arc<Self>) -> VfsResult<Vec<(Arc<Mountpoint>, usize)>> {
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        pending.push((self.clone(), 0));
        let mut visited = HashSet::new();
        let mut nodes = Vec::new();
        while let Some((mountpoint, depth)) = pending.pop() {
            if depth > MAX_MOUNT_TREE_DEPTH {
                return Err(VfsError::ResourceBusy);
            }
            visited.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            if !visited.insert(mountpoint.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if visited.len() > MAX_ACTIVE_NON_ROOT_MOUNTS + 1 {
                return Err(VfsError::NoMemory);
            }
            let child_count = mountpoint.children.lock().len();
            pending
                .try_reserve(child_count)
                .map_err(|_| VfsError::NoMemory)?;
            for child in mountpoint.children.lock().values() {
                pending.push((child.clone(), depth + 1));
            }
            nodes.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            nodes.push((mountpoint, depth));
        }
        Ok(nodes)
    }

    fn parent_mountpoint_locked(&self) -> Option<Arc<Mountpoint>> {
        self.location.lock().as_ref()?.mountpoint.upgrade()
    }

    fn has_mount_at_or_below_locked(&self, entry: &DirEntry) -> VfsResult<bool> {
        let entry_is_dir = entry.is_dir();
        for child in self.children.lock().values() {
            let location = child.location.lock();
            let location = location.as_ref().ok_or(VfsError::Io)?;
            let parent = location.mountpoint.upgrade().ok_or(VfsError::Io)?;
            if parent.mount_id != self.mount_id {
                return Err(VfsError::Io);
            }
            if entry.inode() == location.entry.inode()
                || (entry_is_dir && entry_is_same_or_ancestor_by_inode(entry, &location.entry))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn refresh_handle_ancestors_locked(&self) {
        let mut ancestors = self.handle.ancestors.lock();
        ancestors.clear();
        if self.handle.users.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut current = self.parent_mountpoint_locked();
        for _ in 0..MAX_MOUNT_TREE_DEPTH {
            let Some(mountpoint) = current else {
                break;
            };
            current = mountpoint.parent_mountpoint_locked();
            ancestors.push(mountpoint);
        }
        debug_assert!(current.is_none());
    }

    fn location_handle_locked(self: &Arc<Self>) -> Arc<MountHandle> {
        self.handle.clone()
    }

    fn active_location_count(&self) -> usize {
        self.handle.users.load(Ordering::Acquire)
    }

    fn refresh_subtree_handles_locked(subtree: &[(Arc<Mountpoint>, usize)]) {
        for (mountpoint, _) in subtree {
            mountpoint.refresh_handle_ancestors_locked();
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
        let mut visited = HashSet::new();
        visited
            .try_reserve(MAX_MOUNT_TREE_DEPTH.saturating_add(1))
            .map_err(|_| VfsError::NoMemory)?;
        for depth in 0..=MAX_MOUNT_TREE_DEPTH {
            if !visited.insert(current.mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            if current.unmounting.load(Ordering::Acquire) {
                return Err(VfsError::ResourceBusy);
            }
            if current.namespace_root.load(Ordering::Acquire) {
                return Ok(depth);
            }
            let Some(parent) = current.parent_mountpoint_locked() else {
                return Ok(depth);
            };
            current = parent;
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
        if self.namespace_root.load(Ordering::Acquire) {
            return Err(VfsError::ResourceBusy);
        }

        if require_unused && (self.active_location_count() != 1 || !self.children.lock().is_empty())
        {
            return Err(VfsError::ResourceBusy);
        }

        let location = self.location.lock();
        let parent_location = location.as_ref().ok_or(VfsError::InvalidInput)?;
        let parent = parent_location
            .mountpoint
            .upgrade()
            .ok_or(VfsError::InvalidInput)?;
        let children = parent.children.lock();
        if !children
            .get(&parent_location.entry.key_ref())
            .is_some_and(|mounted| Arc::ptr_eq(mounted, self))
        {
            return Err(VfsError::InvalidInput);
        }

        Ok(())
    }

    fn detach_from_parent_locked(self: &Arc<Self>, require_unused: bool) -> VfsResult<()> {
        self.validate_detach_locked(require_unused)?;
        let subtree = self.subtree_nodes_locked()?;

        self.detach_prevalidated_from_parent_locked()?;
        Self::refresh_subtree_handles_locked(&subtree);
        Ok(())
    }

    /// Commits one already-validated parent edge without allocating.
    fn detach_prevalidated_from_parent_locked(self: &Arc<Self>) -> VfsResult<()> {
        let mut location = self.location.lock();
        let parent_location = location.as_ref().ok_or(VfsError::InvalidInput)?;
        let parent = parent_location
            .mountpoint
            .upgrade()
            .ok_or(VfsError::InvalidInput)?;
        let mut children = parent.children.lock();
        if !children
            .get(&parent_location.entry.key_ref())
            .is_some_and(|mounted| Arc::ptr_eq(mounted, self))
        {
            return Err(VfsError::InvalidInput);
        }

        children.remove(&parent_location.entry.key_ref());
        *location = None;
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
            let nodes = self.subtree_nodes_locked()?;
            let mut seen = HashSet::new();
            seen.try_reserve(nodes.len())
                .map_err(|_| VfsError::NoMemory)?;
            let mut filesystems = Vec::new();
            filesystems
                .try_reserve(nodes.len())
                .map_err(|_| VfsError::NoMemory)?;
            for (mountpoint, _) in nodes {
                if seen.insert(mountpoint.device) {
                    filesystems.push(mountpoint.filesystem.clone());
                }
            }
            filesystems
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
        if self.counted_non_root {
            release_non_root_mount_reservation();
        }
    }
}

#[derive(Debug)]
pub struct Location {
    mountpoint: Arc<Mountpoint>,
    mount: Arc<MountHandle>,
    entry: DirEntry,
}

pub struct PreparedUnmount {
    location: Option<Location>,
}

pub struct FlushedUnmount {
    location: Option<Location>,
}

impl PreparedUnmount {
    pub fn flush(mut self) -> VfsResult<FlushedUnmount> {
        let Some(location) = self.location.as_ref() else {
            return Err(VfsError::Io);
        };
        location.mountpoint().filesystem.flush()?;
        let location = self.location.take().ok_or(VfsError::Io)?;
        Ok(FlushedUnmount {
            location: Some(location),
        })
    }
}

impl Drop for PreparedUnmount {
    fn drop(&mut self) {
        if let Some(location) = self.location.as_ref() {
            location.cancel_prepared_unmount();
        }
    }
}

impl FlushedUnmount {
    pub fn commit(mut self) -> VfsResult<()> {
        let location = self.location.take().ok_or(VfsError::Io)?;
        location.commit_prepared_unmount()
    }
}

impl Drop for FlushedUnmount {
    fn drop(&mut self) {
        if let Some(location) = self.location.as_ref() {
            location.cancel_prepared_unmount();
        }
    }
}

impl Clone for Location {
    fn clone(&self) -> Self {
        let _admission = self.mount.admission.read();
        let previous = self.mount.users.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous != usize::MAX, "mount Location count overflow");
        if self.mountpoint.unmounting.load(Ordering::Acquire) {
            self.mount
                .admitted_during_unmount
                .store(true, Ordering::Release);
        }
        Self {
            mountpoint: self.mountpoint.clone(),
            mount: self.mount.clone(),
            entry: self.entry.clone(),
        }
    }
}

impl Drop for Location {
    fn drop(&mut self) {
        // Serialize the zero-to-one/one-to-zero transitions with Location
        // admission. The last user temporarily takes the preallocated vector,
        // drops ancestor Arcs outside its spin mutex, then returns the same
        // allocation for the next pathwalk.
        let _admission = self.mount.admission.write();
        let Ok(previous) =
            self.mount
                .users
                .try_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                    users.checked_sub(1)
                })
        else {
            debug_assert!(false, "mount Location count underflow");
            return;
        };
        if previous != 1 {
            return;
        }
        let mut retired = {
            let mut ancestors = self.mount.ancestors.lock();
            core::mem::take(&mut *ancestors)
        };
        retired.clear();
        *self.mount.ancestors.lock() = retired;
    }
}

#[inherit_methods(from = "self.entry")]
impl Location {
    /// Returns whether `self` names the same object as, or an ancestor of,
    /// `descendant` in this mount.  This is an object-identity relation, not a
    /// pathname comparison, so rename/reuse cannot retarget policy rules.
    pub fn is_same_or_ancestor_of(&self, descendant: &Location) -> bool {
        Arc::ptr_eq(&self.mountpoint, &descendant.mountpoint)
            && entry_is_same_or_ancestor_by_inode(&self.entry, &descendant.entry)
    }
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

    pub fn flags(&self) -> NodeFlags;

    pub fn open(&self, read: bool, write: bool) -> VfsResult<()>;

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
        let previous = mount.users.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous != usize::MAX, "mount Location count overflow");
        if previous == 0 {
            mountpoint.refresh_handle_ancestors_locked();
        }
        if mountpoint.unmounting.load(Ordering::Acquire) {
            mount.admitted_during_unmount.store(true, Ordering::Release);
        }
        // `mount` already owns the admitted handle reference. Releasing the
        // read guard before moving that reference cannot hide it from phase
        // two: the strong count remains elevated until the Location is built.
        drop(_admission);
        Self {
            mountpoint,
            mount,
            entry,
        }
    }

    fn wrap(&self, entry: DirEntry) -> Self {
        let _admission = self.mount.admission.read();
        let previous = self.mount.users.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous != usize::MAX, "mount Location count overflow");
        if self.mountpoint.unmounting.load(Ordering::Acquire) {
            self.mount
                .admitted_during_unmount
                .store(true, Ordering::Release);
        }
        Self {
            mountpoint: self.mountpoint.clone(),
            mount: self.mount.clone(),
            entry,
        }
    }

    pub fn mountpoint(&self) -> &Arc<Mountpoint> {
        &self.mountpoint
    }

    pub fn entry(&self) -> &DirEntry {
        &self.entry
    }

    pub fn writeback_anchor(&self) -> WritebackAnchor {
        self.mountpoint().writeback_anchor(self.entry.clone())
    }

    pub fn writeback_error_state(&self) -> VfsResult<Arc<crate::WritebackErrorState>> {
        self.entry.writeback_error_state()
    }

    pub fn name(&self) -> &str {
        self.entry.name()
    }

    pub fn parent(&self) -> Option<Self> {
        let _tree = MOUNT_TREE_LOCK.read();
        let mut current = self.clone();
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            if !current.is_root_of_mount() {
                return current.entry.parent().map(|entry| current.wrap(entry));
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

    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        self.entry.xattr_provider()
    }

    pub fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        self.xattr_provider()
            .ok_or_else(unsupported_xattr)?
            .get_xattr(name)
    }

    pub fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        self.xattr_provider()
            .ok_or_else(unsupported_xattr)?
            .list_xattrs()
    }

    pub fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        self.xattr_provider()
            .ok_or_else(unsupported_xattr)?
            .set_xattr(name, value, mode)
    }

    pub fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        self.xattr_provider()
            .ok_or_else(unsupported_xattr)?
            .remove_xattr(name)
    }

    /// Captures the namespace generations of this directory.
    pub fn namespace_generation(&self) -> VfsResult<NamespaceGeneration> {
        Ok(self.entry.as_dir()?.namespace_generation())
    }

    /// Returns whether a prepared directory mutation still observes the same
    /// local and backend namespace generations.
    pub fn namespace_generation_is_current(
        &self,
        generation: NamespaceGeneration,
    ) -> VfsResult<bool> {
        Ok(self
            .entry
            .as_dir()?
            .namespace_generation_is_current(generation))
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
        let mut components = Vec::new();
        let mut cur = self.clone();
        let mut visited = HashSet::new();
        visited
            .try_reserve(MAX_MOUNT_TREE_DEPTH.saturating_add(1))
            .map_err(|_| VfsError::NoMemory)?;
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            if !visited.insert(cur.mountpoint().mount_id) {
                return Err(VfsError::FilesystemLoop);
            }
            let mut entry = cur.entry.clone();
            while !entry.ptr_eq(&cur.mountpoint().root) {
                try_push_path_component(&mut components, &entry)?;
                entry = entry.parent().ok_or(VfsError::InvalidInput)?;
            }
            cur = match cur.mountpoint().location_locked() {
                Some(loc) => loc,
                None => {
                    return try_build_absolute_path(&components, DirEntry::name);
                }
            };
        }
        Err(VfsError::ResourceBusy)
    }

    /// Returns this entry's path relative to the root of its filesystem mount.
    pub fn path_in_mount(&self) -> VfsResult<PathBuf> {
        let mut components = Vec::new();
        let mut entry = self.entry.clone();
        while !entry.ptr_eq(&self.mountpoint().root) {
            try_push_path_component(&mut components, &entry)?;
            entry = entry.parent().ok_or(VfsError::InvalidInput)?;
        }
        try_build_absolute_path(&components, DirEntry::name)
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.mountpoint(), other.mountpoint()) && self.entry.ptr_eq(&other.entry)
    }

    /// Returns whether both locations belong to the exact same mount instance.
    ///
    /// Distinct bind or filesystem views may share a stable filesystem identity
    /// while remaining distinct mounts, so filesystem/device identity is not a
    /// substitute for this topology check.
    pub fn same_mount(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.mountpoint(), other.mountpoint())
    }

    /// Returns whether both handles name the same backend inode in this mount.
    ///
    /// Directory cache generations may replace a dentry wrapper after an
    /// unrelated mutation, so pointer equality is too strict for transaction
    /// revalidation. Filesystem backends remain responsible for checking any
    /// stronger generation token under their namespace lock before commit.
    pub fn same_node(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.mountpoint(), other.mountpoint()) && self.inode() == other.inode()
    }

    pub fn is_mountpoint(&self) -> bool {
        let _tree = MOUNT_TREE_LOCK.read();
        self.mountpoint()
            .children
            .lock()
            .contains_key(&self.entry.key_ref())
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
            .get(&self.entry.key_ref())
            .cloned()
        else {
            return Ok(self);
        };
        let mountpoint = mountpoint.effective_mountpoint_locked()?;
        let entry = mountpoint.root.clone();
        Ok(Self::new_locked(mountpoint, entry))
    }

    /// Looks up a final component without crossing a child mount attached to it.
    ///
    /// For an ordinary name, the returned location wraps the covered dentry in
    /// this directory's exact mount. Higher-level namespace-mutation preflight
    /// can therefore inspect the covered inode's type and security state before
    /// separately rejecting mutation of a mountpoint. No operation policy or
    /// ABI-visible error mapping is applied here.
    pub fn lookup_no_follow_in_mount(&self, name: &str) -> VfsResult<Self> {
        match name {
            DOT => Ok(self.clone()),
            DOTDOT => Ok(self.parent().unwrap_or_else(|| self.clone())),
            _ => self
                .entry
                .as_dir()?
                .lookup(name)
                .map(|entry| self.wrap(entry)),
        }
    }

    pub fn lookup_no_follow(&self, name: &str) -> VfsResult<Self> {
        match name {
            // Preserve parent traversal out of a mount root. Resolving the
            // resulting covered mountpoint would immediately cross back in.
            DOT | DOTDOT => self.lookup_no_follow_in_mount(name),
            _ => self.lookup_no_follow_in_mount(name)?.resolve_mountpoint(),
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

    pub fn create_named(
        &self,
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<Self>> {
        self.entry
            .as_dir()?
            .create_named(name, options, disposition)
            .map(|outcome| outcome.map(|entry| self.wrap(entry)))
    }

    pub fn create_anonymous(&self, options: &AnonymousOptions) -> VfsResult<Self> {
        self.entry
            .as_dir()?
            .create_anonymous(options)
            .map(|entry| self.wrap(entry))
    }

    pub fn create_symlink(
        &self,
        name: &str,
        target: &str,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<Self> {
        self.entry
            .as_dir()?
            .create_symlink(name, target, permission, user)
            .map(|entry| self.wrap(entry))
    }

    /// Returns whether this directory backend implements named publication for
    /// `node_type`.
    pub fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.entry
            .as_dir()
            .is_ok_and(|dir| dir.supports_named_create(node_type))
    }

    /// Returns whether this directory backend implements symbolic-link
    /// publication.
    pub fn supports_symlink(&self) -> bool {
        self.entry.as_dir().is_ok_and(|dir| dir.supports_symlink())
    }

    /// Returns whether this directory backend implements hard links.
    pub fn supports_hard_links(&self) -> bool {
        self.entry
            .as_dir()
            .is_ok_and(|dir| dir.supports_hard_links())
    }

    /// Returns whether this directory backend implements non-directory removal.
    pub fn supports_unlink(&self) -> bool {
        self.entry.as_dir().is_ok_and(|dir| dir.supports_unlink())
    }

    /// Returns whether this directory backend implements directory removal.
    pub fn supports_rmdir(&self) -> bool {
        self.entry.as_dir().is_ok_and(|dir| dir.supports_rmdir())
    }

    /// Returns whether this directory backend implements ordinary rename.
    pub fn supports_rename(&self) -> bool {
        self.entry.as_dir().is_ok_and(|dir| dir.supports_rename())
    }

    pub fn link(&self, name: &str, node: &Self) -> VfsResult<Self> {
        if !self.same_mount(node) {
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
        let dst = optional_lookup(dst_dir.entry.as_dir()?.lookup(dst_name))?;
        let src = self.wrap(src);
        let dst = dst.map(|entry| dst_dir.wrap(entry));
        self.rename_checked(src_name, &src, dst_dir, dst_name, dst.as_ref())
    }

    /// Renames only the exact source and destination identities prepared by a
    /// higher-level transaction.
    ///
    /// Mount topology is checked before entering the filesystem backend. A
    /// caller that coordinates mount syscalls separately may retain that
    /// higher-level guard through this operation without carrying the VFS spin
    /// lock across filesystem I/O.
    pub fn rename_checked(
        &self,
        src_name: &str,
        src: &Self,
        dst_dir: &Self,
        dst_name: &str,
        dst: Option<&Self>,
    ) -> VfsResult<()> {
        self.validate_rename_ancestry_checked(src, dst_dir, dst)?;
        if dst.is_some_and(|dst| dst.same_node(src)) {
            return Ok(());
        }
        if !self.supports_rename() {
            return Err(VfsError::OperationNotPermitted);
        }
        {
            let _tree = MOUNT_TREE_LOCK.read();
            if self.mountpoint().has_mount_at_or_below_locked(&src.entry)?
                || match dst {
                    Some(dst) => self.mountpoint().has_mount_at_or_below_locked(&dst.entry)?,
                    None => false,
                }
            {
                return Err(VfsError::ResourceBusy);
            }
        }
        self.entry.as_dir()?.rename(
            src_name,
            &src.entry,
            dst_dir.entry.as_dir()?,
            dst_name,
            dst.map(|dst| &dst.entry),
        )
    }

    /// Validates the exact-mount and directory-ancestry constraints that must
    /// be decided before a higher-level rename policy hook runs.
    ///
    /// The backend repeats these checks during publication because directory
    /// topology may still race after a nonblocking policy hook. Mountpoint
    /// rejection is intentionally not part of this pre-hook seam: Linux's
    /// `vfs_rename()` invokes `security_inode_rename()` before its local
    /// mountpoint check.
    pub fn validate_rename_ancestry_checked(
        &self,
        src: &Self,
        dst_dir: &Self,
        dst: Option<&Self>,
    ) -> VfsResult<()> {
        if !Arc::ptr_eq(self.mountpoint(), src.mountpoint())
            || !Arc::ptr_eq(self.mountpoint(), dst_dir.mountpoint())
            || dst.is_some_and(|dst| !Arc::ptr_eq(dst_dir.mountpoint(), dst.mountpoint()))
        {
            return Err(VfsError::CrossesDevices);
        }
        if src.entry.is_dir()
            && !self.ptr_eq(dst_dir)
            && entry_is_same_or_ancestor_by_inode(&src.entry, &dst_dir.entry)
        {
            return Err(VfsError::InvalidInput);
        }
        if let Some(dst) = dst
            && dst.entry.is_dir()
            && !self.ptr_eq(dst_dir)
            && entry_is_same_or_ancestor_by_inode(&dst.entry, &self.entry)
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        Ok(())
    }

    pub fn unlink(&self, name: &str, is_dir: bool) -> VfsResult<()> {
        let expected = self.entry.as_dir()?.lookup(name)?;
        self.unlink_entry_checked(name, is_dir, &expected)
    }

    pub fn unlink_checked(&self, name: &str, is_dir: bool, expected: &Self) -> VfsResult<()> {
        if !Arc::ptr_eq(self.mountpoint(), expected.mountpoint()) {
            return Err(VfsError::ResourceBusy);
        }
        self.unlink_entry_checked(name, is_dir, &expected.entry)
    }

    fn unlink_entry_checked(&self, name: &str, is_dir: bool, expected: &DirEntry) -> VfsResult<()> {
        {
            let _tree = MOUNT_TREE_LOCK.read();
            if self
                .mountpoint()
                .children
                .lock()
                .contains_key(&expected.key_ref())
            {
                return Err(VfsError::ResourceBusy);
            }
        }
        self.entry.as_dir()?.unlink_checked(name, is_dir, expected)
    }

    pub fn open_file(&self, name: &str, options: &OpenOptions) -> VfsResult<Location> {
        self.entry
            .as_dir()?
            .open_file(name, options)
            .and_then(|entry| self.wrap(entry).resolve_mountpoint())
    }

    pub fn open_file_with_status(
        &self,
        name: &str,
        options: &OpenOptions,
    ) -> VfsResult<(Location, bool)> {
        self.entry
            .as_dir()?
            .open_file_with_status(name, options)
            .and_then(|(entry, created)| Ok((self.wrap(entry).resolve_mountpoint()?, created)))
    }

    pub fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.entry.as_dir()?.read_dir(offset, sink)
    }

    pub fn mount(&self, fs: &Filesystem) -> VfsResult<Arc<Mountpoint>> {
        self.mount_inner(fs, Some(TypeMap::new()))
    }

    pub fn mount_with_extensions(
        &self,
        fs: &Filesystem,
        extensions: TypeMap,
    ) -> VfsResult<Arc<Mountpoint>> {
        self.mount_inner(fs, Some(extensions))
    }

    fn mount_inner(
        &self,
        fs: &Filesystem,
        extensions: Option<TypeMap>,
    ) -> VfsResult<Arc<Mountpoint>> {
        self.check_is_dir()?;
        // `root_dir` is an open filesystem callback. Invoke it before taking
        // the global topology writer so a backend cannot stall pathname
        // readers or re-enter the mount tree while that writer is held.
        let root = fs.root_dir();
        // Admit the mount object and its policy extensions before taking the
        // topology writer. Failure only drops an unpublished detached object.
        let result = Mountpoint::new_mounted(fs, root, self, extensions)?;
        let _tree = mount_tree_write();
        let parent_depth = self.mountpoint().attached_depth_locked()?;
        if parent_depth >= MAX_MOUNT_TREE_DEPTH {
            return Err(VfsError::ResourceBusy);
        }
        let key = self.entry.try_key()?;
        {
            let mut children = self.mountpoint().children.lock();
            if children.contains_key(&key) {
                return Err(VfsError::ResourceBusy);
            }
            children.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        }
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
        if self.mountpoint().namespace_root.load(Ordering::Acquire) {
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
        let mut visited = HashSet::new();
        visited
            .try_reserve(MAX_MOUNT_TREE_DEPTH.saturating_add(1))
            .map_err(|_| VfsError::NoMemory)?;
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

        let target_key = target.entry.try_key()?;
        {
            let mut children = target.mountpoint().children.lock();
            if children.contains_key(&target_key) {
                return Err(VfsError::ResourceBusy);
            }
            children.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        }
        let mut location = self.mountpoint().location.lock();
        if let Some(old_location) = location.as_ref() {
            let old_parent = old_location
                .mountpoint
                .upgrade()
                .ok_or(VfsError::InvalidInput)?;
            let mut old_children = old_parent.children.lock();
            if !old_children
                .get(&old_location.entry.key_ref())
                .is_some_and(|mounted| Arc::ptr_eq(mounted, self.mountpoint()))
            {
                return Err(VfsError::InvalidInput);
            }
            old_children.remove(&old_location.entry.key_ref());
        }

        *location = Some(MountLocation::new(target));
        target
            .mountpoint()
            .children
            .lock()
            .insert(target_key, self.mountpoint().clone());
        drop(location);
        Mountpoint::refresh_subtree_handles_locked(&subtree);
        Ok(())
    }

    /// Atomically promotes this mounted tree to namespace root and mounts the
    /// former root at `put_old`. Both locations must already be stable.
    pub fn pivot_root_to(&self, put_old: &Self) -> VfsResult<()> {
        if !self.is_root_of_mount() || !put_old.is_dir() {
            return Err(VfsError::InvalidInput);
        }
        let new_root = self.mountpoint();
        if new_root.namespace_root.load(Ordering::Acquire)
            || !Arc::ptr_eq(put_old.mountpoint(), new_root)
        {
            return Err(VfsError::InvalidInput);
        }

        let _tree = mount_tree_write();
        if new_root.unmounting.load(Ordering::Acquire) {
            return Err(VfsError::ResourceBusy);
        }
        if !entry_is_same_or_ancestor_by_inode(&new_root.root, put_old.entry()) {
            return Err(VfsError::InvalidInput);
        }
        let mut old_root = new_root.clone();
        for _ in 0..=MAX_MOUNT_TREE_DEPTH {
            if old_root.namespace_root.load(Ordering::Acquire) {
                break;
            }
            old_root = old_root
                .parent_mountpoint_locked()
                .ok_or(VfsError::InvalidInput)?;
        }
        if !old_root.namespace_root.load(Ordering::Acquire)
            || old_root.unmounting.load(Ordering::Acquire)
        {
            return Err(VfsError::ResourceBusy);
        }

        let new_location = new_root.location.lock();
        let old_parent = new_location
            .as_ref()
            .and_then(|location| location.mountpoint.upgrade())
            .ok_or(VfsError::InvalidInput)?;
        let new_key = new_location.as_ref().unwrap().entry.try_key()?;
        let put_old_key = put_old.entry.try_key()?;
        if !old_parent
            .children
            .lock()
            .get(&new_key)
            .is_some_and(|mount| Arc::ptr_eq(mount, new_root))
        {
            return Err(VfsError::InvalidInput);
        }
        {
            let mut children = new_root.children.lock();
            if children.contains_key(&put_old_key) {
                return Err(VfsError::ResourceBusy);
            }
            // `insert` is part of the irreversible edge swap below. Reserve
            // its bucket before the first topology mutation so ENOMEM leaves
            // the tree completely untouched.
            children.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        }
        drop(new_location);

        // Validate before either edge is changed. The commit below allocates
        // nothing, so it cannot leave a partially pivoted namespace.
        let new_subtree = new_root.subtree_nodes_locked()?;
        let old_subtree = old_root.subtree_nodes_locked()?;
        if new_subtree
            .iter()
            .any(|(mount, _)| mount.unmounting.load(Ordering::Acquire))
            || old_subtree
                .iter()
                .any(|(mount, _)| mount.unmounting.load(Ordering::Acquire))
        {
            return Err(VfsError::ResourceBusy);
        }

        old_parent.children.lock().remove(&new_key);
        *new_root.location.lock() = None;
        old_root.namespace_root.store(false, Ordering::Release);
        *old_root.location.lock() = Some(MountLocation::new(put_old));
        new_root
            .children
            .lock()
            .insert(put_old_key, old_root.clone());
        new_root.namespace_root.store(true, Ordering::Release);
        Mountpoint::refresh_subtree_handles_locked(&new_subtree);
        Mountpoint::refresh_subtree_handles_locked(&old_subtree);
        Ok(())
    }

    pub fn check_unmountable(&self) -> VfsResult<()> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint().namespace_root.load(Ordering::Acquire) {
            return Err(VfsError::ResourceBusy);
        }

        let _tree = mount_tree_write();
        let _admission = self.mount.admission.upgradeable_read().upgrade();
        if self.mountpoint().has_unmounting_ancestor_locked() {
            return Err(VfsError::ResourceBusy);
        }
        self.mountpoint().validate_detach_locked(true)
    }

    pub fn prepare_unmount(self) -> VfsResult<PreparedUnmount> {
        if !self.is_root_of_mount() {
            return Err(VfsError::InvalidInput);
        }
        if self.mountpoint().namespace_root.load(Ordering::Acquire) {
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

        Ok(PreparedUnmount {
            location: Some(self),
        })
    }

    fn cancel_prepared_unmount(&self) {
        let _tree = mount_tree_write();
        let _admission = self.mount.admission.upgradeable_read().upgrade();
        self.mountpoint().unmounting.store(false, Ordering::Release);
    }

    fn commit_prepared_unmount(&self) -> VfsResult<()> {
        let _tree = mount_tree_write();
        let _admission = self.mount.admission.upgradeable_read().upgrade();
        let result = if !self.mount.admitted_during_unmount.load(Ordering::Acquire) {
            self.mountpoint().detach_from_parent_locked(true)
        } else {
            Err(VfsError::ResourceBusy)
        };
        self.mountpoint().unmounting.store(false, Ordering::Release);
        result
    }

    /// Flushes and detaches this mount when this is its only live Location.
    ///
    /// Consuming `self` is part of the exclusivity contract. A borrowed
    /// `Location` can itself be shared through `Arc<Location>` without adding
    /// another mount-handle reference, which would make a strong-count-only
    /// busy check unable to prove that no other thread can use the same lease
    /// during the lock-free flush window.
    pub fn unmount(self) -> VfsResult<()> {
        self.prepare_unmount()?.flush()?.commit()
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
                if !mountpoint.namespace_root.load(Ordering::Acquire) {
                    mountpoint.validate_detach_locked(false)?;
                }
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

        // Revalidate every frozen edge before the first topology mutation.
        // The leaf-to-root commit below performs no allocation or backend I/O,
        // so resource pressure after a successful flush cannot leave a partial
        // detach.
        for (mountpoint, _) in &mounts {
            if !mountpoint.namespace_root.load(Ordering::Acquire) {
                if let Err(err) = mountpoint.validate_detach_locked(false) {
                    for (mountpoint, _) in &mounts {
                        mountpoint.unmounting.store(false, Ordering::Release);
                    }
                    return Err(err);
                }
            }
        }
        let mut detach_error = None;
        for (mountpoint, _) in &mounts {
            if !mountpoint.namespace_root.load(Ordering::Acquire) {
                if let Err(err) = mountpoint.detach_prevalidated_from_parent_locked() {
                    detach_error = Some(err);
                    break;
                }
                mountpoint.refresh_handle_ancestors_locked();
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

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError>;
}

#[cfg(test)]
mod tests {
    use core::{any::Any, time::Duration};
    use std::{
        sync::{Arc as StdArc, Barrier},
        thread,
    };

    use axerrno::LinuxError;

    use super::*;
    use crate::{
        DirNode, DirNodeOps, MetadataUpdateCapabilities, NodeOps, Reference, RenameRequest, StatFs,
        UnlinkRequest,
    };

    #[derive(Debug, PartialEq, Eq)]
    struct Marker(u32);

    fn marker_map(value: u32) -> TypeMap {
        let mut extensions = TypeMap::new();
        extensions.insert(Marker(value));
        extensions
    }

    struct LookupTestFs {
        root: Once<DirEntry>,
        root_inode: u64,
    }

    impl LookupTestFs {
        fn new(root_inode: u64) -> Arc<Self> {
            let filesystem = Arc::new(Self {
                root: Once::new(),
                root_inode,
            });
            let root = DirEntry::new_dir(
                {
                    let filesystem = filesystem.clone();
                    move |this| {
                        DirNode::new(Arc::new(LookupTestDir {
                            inode: filesystem.root_inode,
                            filesystem: filesystem.clone(),
                            this,
                        }))
                    }
                },
                Reference::root(),
            );
            filesystem.root.call_once(|| root);
            filesystem
        }
    }

    impl FilesystemOps for LookupTestFs {
        fn name(&self) -> &str {
            "lookup-test"
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
                file_count: 3,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
                mount_flags: 0,
            })
        }

        fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
            MetadataUpdateCapabilities::empty()
        }
    }

    struct LookupTestDir {
        inode: u64,
        filesystem: Arc<LookupTestFs>,
        this: crate::WeakDirEntry,
    }

    impl LookupTestDir {
        fn child(&self, name: &str, inode: u64) -> DirEntry {
            DirEntry::new_dir(
                {
                    let filesystem = self.filesystem.clone();
                    move |this| {
                        DirNode::new(Arc::new(Self {
                            inode,
                            filesystem: filesystem.clone(),
                            this,
                        }))
                    }
                },
                Reference::new(self.this.upgrade(), name.into()),
            )
        }
    }

    impl NodeOps for LookupTestDir {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: self.inode,
                nlink: 1,
                mode: NodePermission::from_bits_truncate(0o755),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 4096,
                blocks: 0,
                rdev: Default::default(),
                atime: crate::Timestamp::ZERO,
                btime: crate::Timestamp::ZERO,
                mtime: crate::Timestamp::ZERO,
                ctime: crate::Timestamp::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            &*self.filesystem
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for LookupTestDir {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            Ok(0)
        }

        fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
            match name {
                "child" => Ok(self.child(name, self.filesystem.root_inode + 1)),
                "other" => Ok(self.child(name, self.filesystem.root_inode + 2)),
                _ => Err(VfsError::NotFound),
            }
        }

        fn create_named(
            &self,
            _name: &str,
            _options: &NamedCreateOptions,
            _disposition: CreateDisposition,
        ) -> VfsResult<CreateOutcome<DirEntry>> {
            Err(VfsError::Unsupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
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
    fn in_mount_lookup_exposes_a_covered_dentry_without_crossing() {
        let parent_filesystem = Filesystem::new(LookupTestFs::new(100));
        let parent_mount = Mountpoint::new_root(&parent_filesystem);
        let parent = parent_mount.root_location();
        let covered_before_mount = parent.lookup_no_follow_in_mount("child").unwrap();
        let child_filesystem = Filesystem::new(LookupTestFs::new(200));
        let child_mount = covered_before_mount.mount(&child_filesystem).unwrap();

        let ordinary = parent.lookup_no_follow("child").unwrap();
        let covered = parent.lookup_no_follow_in_mount("child").unwrap();

        assert!(Arc::ptr_eq(ordinary.mountpoint(), &child_mount));
        assert!(ordinary.is_root_of_mount());
        assert_eq!(ordinary.inode(), 200);
        assert!(covered.same_mount(&parent));
        assert_eq!(covered.inode(), 101);
        assert!(covered.is_mountpoint());
        assert!(!ordinary.same_mount(&covered));

        let ordinary_other = parent.lookup_no_follow("other").unwrap();
        let in_mount_other = parent.lookup_no_follow_in_mount("other").unwrap();
        assert!(ordinary_other.ptr_eq(&in_mount_other));
        assert!(!in_mount_other.is_mountpoint());
    }

    #[test]
    fn pivot_root_replaces_the_namespace_root_without_a_transient_detach() {
        let parent_filesystem = Filesystem::new(LookupTestFs::new(100));
        let old_root_mount = Mountpoint::new_root(&parent_filesystem);
        let old_root = old_root_mount.root_location();
        let mountpoint = old_root.lookup_no_follow_in_mount("child").unwrap();
        let new_filesystem = Filesystem::new(LookupTestFs::new(200));
        let new_mount = mountpoint.mount(&new_filesystem).unwrap();
        let new_root = new_mount.root_location();
        let put_old = new_root.lookup_no_follow_in_mount("child").unwrap();

        new_root.pivot_root_to(&put_old).unwrap();

        assert!(new_mount.is_root());
        assert!(!old_root_mount.is_root());
        assert!(old_root_mount.location().unwrap().ptr_eq(&put_old));
        assert!(
            new_root
                .lookup_no_follow("child")
                .unwrap()
                .same_mount(&old_root)
        );
    }

    #[test]
    fn rename_ancestry_preflight_preserves_both_trap_error_classes() {
        let filesystem = Filesystem::new(LookupTestFs::new(300));
        let mount = Mountpoint::new_root(&filesystem);
        let root = mount.root_location();
        let child = root.lookup_no_follow_in_mount("child").unwrap();
        let grandchild = child.lookup_no_follow_in_mount("other").unwrap();

        assert_eq!(
            root.validate_rename_ancestry_checked(&child, &grandchild, None),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            child.validate_rename_ancestry_checked(&grandchild, &root, Some(&child)),
            Err(VfsError::DirectoryNotEmpty)
        );
    }

    #[test]
    fn extensions_are_initialized_exactly_once() {
        let extensions = MountExtensions::new(None);
        assert!(extensions.get_ref::<Marker>().is_none());

        extensions.initialize(marker_map(7)).unwrap();
        assert_eq!(extensions.get_ref::<Marker>(), Some(&Marker(7)));
        assert_eq!(
            extensions.initialize(marker_map(9)),
            Err(VfsError::AlreadyExists)
        );
        assert_eq!(extensions.get_ref::<Marker>(), Some(&Marker(7)));
    }

    #[test]
    fn ordinary_mount_extensions_cannot_be_late_initialized() {
        let extensions = MountExtensions::new(Some(TypeMap::new()));
        assert_eq!(
            extensions.initialize(marker_map(1)),
            Err(VfsError::AlreadyExists)
        );
        assert!(extensions.get_ref::<Marker>().is_none());
    }

    #[test]
    fn concurrent_extension_loser_observes_completed_publication() {
        let extensions = StdArc::new(MountExtensions::new(None));
        let barrier = StdArc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for value in [7, 9] {
            let extensions = extensions.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                let result = extensions.initialize(marker_map(value));
                let observed = extensions.get_ref::<Marker>().map(|marker| marker.0);
                (result, observed)
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|(result, _)| result.is_ok()).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|(result, _)| *result == Err(VfsError::AlreadyExists))
                .count(),
            1
        );
        assert!(results.iter().all(|(_, observed)| observed.is_some()));
        assert_eq!(results[0].1, results[1].1);
    }

    #[test]
    fn optional_lookup_only_swallows_not_found() {
        assert_eq!(optional_lookup::<()>(Err(VfsError::NotFound)), Ok(None));
        assert_eq!(optional_lookup::<()>(Err(VfsError::Io)), Err(VfsError::Io));
        assert_eq!(optional_lookup(Ok(7_u8)), Ok(Some(7_u8)));
    }

    #[test]
    fn default_node_xattrs_are_honestly_unsupported() {
        let filesystem = Filesystem::new(LookupTestFs::new(100));
        let mount = Mountpoint::new_root(&filesystem);
        let root = mount.root_location();

        assert_eq!(
            LinuxError::from(root.get_xattr(b"user.key").unwrap_err()),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            LinuxError::from(root.list_xattrs().unwrap_err()),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            LinuxError::from(
                root.set_xattr(b"user.key", b"value", XattrSetMode::Upsert)
                    .unwrap_err()
            ),
            LinuxError::EOPNOTSUPP
        );
        assert_eq!(
            LinuxError::from(root.remove_xattr(b"user.key").unwrap_err()),
            LinuxError::EOPNOTSUPP
        );
    }
}
