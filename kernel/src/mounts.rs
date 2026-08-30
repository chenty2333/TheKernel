use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    DeviceId, Filesystem, FilesystemIdentity, Location, Mountpoint, NodeType, TypeMap, VfsResult,
    WeakFilesystemIdentity,
};
use axsync::Mutex as BlockingMutex;
#[cfg(not(test))]
use axsync::MutexGuard as BlockingMutexGuard;
use hashbrown::{HashMap, HashSet};
use spin::{Lazy, Mutex};

use crate::time::wall_time;

pub struct MountRecord {
    pub mount_id: u64,
    pub parent_id: u64,
    pub root: String,
    pub source: String,
    pub target: String,
    pub fs_type: String,
    pub data: String,
    pub dev: u64,
    pub flags: u32,
    pub expire_epoch: Option<u64>,
    mountpoint: Weak<Mountpoint>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct MountMetadata {
    pub source: String,
    pub fs_type: String,
    pub root: String,
    pub data: String,
}

pub struct BindSubmount {
    pub source: Location,
    pub relative_path: String,
    pub metadata: MountMetadata,
    pub flags: u32,
}

struct MountRecordIndex<'a> {
    records: &'a [MountRecord],
    by_id: HashMap<u64, usize>,
    children: HashMap<u64, Vec<u64>>,
}

impl<'a> MountRecordIndex<'a> {
    fn new(records: &'a [MountRecord]) -> AxResult<Self> {
        let mut by_id = HashMap::new();
        by_id
            .try_reserve(records.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut children = HashMap::new();
        children
            .try_reserve(records.len())
            .map_err(|_| AxError::NoMemory)?;
        for (index, record) in records.iter().enumerate() {
            if by_id.insert(record.mount_id, index).is_some() {
                return Err(AxError::Io);
            }
            let child_ids = children.entry(record.parent_id).or_insert_with(Vec::new);
            child_ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            child_ids.push(record.mount_id);
        }
        Ok(Self {
            records,
            by_id,
            children,
        })
    }

    fn record(&self, mount_id: u64) -> AxResult<&'a MountRecord> {
        self.by_id
            .get(&mount_id)
            .map(|index| &self.records[*index])
            .ok_or(AxError::Io)
    }

    fn subtree_ids(&self, root_mount_id: u64) -> AxResult<HashSet<u64>> {
        self.record(root_mount_id)?;
        let mut ids = HashSet::new();
        ids.try_reserve(self.records.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut pending = Vec::new();
        pending
            .try_reserve(self.records.len())
            .map_err(|_| AxError::NoMemory)?;
        pending.push(root_mount_id);
        while let Some(mount_id) = pending.pop() {
            if !ids.insert(mount_id) {
                return Err(AxError::Io);
            }
            if let Some(children) = self.children.get(&mount_id) {
                pending.extend(children.iter().copied());
            }
        }
        Ok(ids)
    }
}

static MOUNT_RECORDS: BlockingMutex<Vec<MountRecord>> = BlockingMutex::new(Vec::new());
// Detached mount FDs and lazily unmounted-but-referenced mounts remain live
// superblocks even though they have no namespace record. Keep weak mount roots
// so legacy device-number queries can follow Linux's user_get_super lifetime.
static LIVE_SUPERBLOCK_MOUNTS: Lazy<BlockingMutex<HashMap<u64, Weak<Mountpoint>>>> =
    Lazy::new(|| BlockingMutex::new(HashMap::new()));
/// Linux defaults `/proc/sys/fs/mount-max` to 100,000 mounts per namespace.
/// TheKernel currently has one global namespace, so use the same hard ceiling
/// until mount namespace accounting is extracted into the ABI layer.
const MAX_MOUNT_RECORDS: usize = 100_000;
// Keep room for the namespace's attached mountpoints plus an equally large set
// of distinct detached or lazily unmounted mountpoints retained by file
// descriptors. Entries are deduplicated by stable mount ID.
const MAX_LIVE_SUPERBLOCK_MOUNTS: usize = MAX_MOUNT_RECORDS * 2;
static LINUX_DEVICE_IDS: Lazy<BlockingMutex<HashMap<u64, (DeviceId, WeakFilesystemIdentity)>>> =
    Lazy::new(|| BlockingMutex::new(HashMap::new()));

// Host tests do not initialize the scheduler/current-task slot required by the
// sleepable production mutex, even when that mutex is uncontended.
#[cfg(not(test))]
type NamespaceOperationMutex = BlockingMutex<()>;
#[cfg(test)]
type NamespaceOperationMutex = Mutex<()>;
#[cfg(not(test))]
type NamespaceOperationMutexGuard = BlockingMutexGuard<'static, ()>;
#[cfg(test)]
type NamespaceOperationMutexGuard = spin::MutexGuard<'static, ()>;

static MOUNT_NAMESPACE_OPERATION: NamespaceOperationMutex = NamespaceOperationMutex::new(());

/// Capability proving that one pathname, mount, or creation-relevant metadata
/// mutation owns the shared Linux namespace serialization domain.
pub struct NamespaceOperationGuard {
    _guard: NamespaceOperationMutexGuard,
}

impl fmt::Debug for NamespaceOperationGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NamespaceOperationGuard")
    }
}

struct LinuxMountState {
    flags: AtomicU32,
    activity_epoch: AtomicU64,
    readonly_floor: bool,
    metadata: Mutex<MountMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpireProbe {
    Retry,
    Ready,
}

/// Serializes Linux mount/pathname mutations, constrained pathname walks, and
/// mode/owner writes which must not race named-create admission.
pub fn namespace_operation() -> NamespaceOperationGuard {
    NamespaceOperationGuard {
        _guard: MOUNT_NAMESPACE_OPERATION.lock(),
    }
}

pub const ROOT_BLOCK_SOURCE: &str = "/dev/vda";
pub const ROOT_BLOCK_DEVICE_ID: DeviceId = DeviceId::new(8, 0);

const MS_RDONLY: u32 = 0x1;
const MS_NOSUID: u32 = 0x2;
const MS_NODEV: u32 = 0x4;
const MS_NOEXEC: u32 = 0x8;
const MS_REMOUNT: u32 = 0x20;
const MS_MANDLOCK: u32 = 0x40;
const MS_NOSYMFOLLOW: u32 = 0x100;
const MS_NOATIME: u32 = 0x400;
const MS_NODIRATIME: u32 = 0x800;
const MS_STRICTATIME: u32 = 0x100_0000;
const MS_UNBINDABLE: u32 = 1 << 17;
const MS_PRIVATE: u32 = 1 << 18;
const MS_SLAVE: u32 = 1 << 19;
const MS_SHARED: u32 = 1 << 20;
const ST_RELATIME: u32 = 0x1000;
const ST_NOSYMFOLLOW: u32 = 0x2000;
const RELATIME_MAX_AGE_SECS: u64 = 24 * 60 * 60;

impl MountMetadata {
    pub fn new(source: String, fs_type: String, root: String, data: String) -> Self {
        Self {
            source,
            fs_type,
            root,
            data,
        }
    }

    pub fn try_from_strs(source: &str, fs_type: &str, root: &str, data: &str) -> AxResult<Self> {
        Ok(Self {
            source: try_string(source)?,
            fs_type: try_string(fs_type)?,
            root: try_string(root)?,
            data: try_string(data)?,
        })
    }

    fn try_clone(&self) -> AxResult<Self> {
        Self::try_from_strs(&self.source, &self.fs_type, &self.root, &self.data)
    }
}

impl MountRecord {
    fn try_clone(&self) -> AxResult<Self> {
        Ok(Self {
            mount_id: self.mount_id,
            parent_id: self.parent_id,
            root: try_string(&self.root)?,
            source: try_string(&self.source)?,
            target: try_string(&self.target)?,
            fs_type: try_string(&self.fs_type)?,
            data: try_string(&self.data)?,
            dev: self.dev,
            flags: self.flags,
            expire_epoch: self.expire_epoch,
            mountpoint: self.mountpoint.clone(),
        })
    }
}

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

pub fn snapshot() -> AxResult<Vec<MountRecord>> {
    let records = MOUNT_RECORDS.lock();
    for record in records.iter() {
        validate_record_state(record)?;
    }
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in records.iter() {
        snapshot.push(record.try_clone()?);
    }
    Ok(snapshot)
}

/// Returns the root location of the first live mount with this Linux device
/// number. The returned mountpoint keeps the mount alive after the records
/// lock is released, so callers may inspect its filesystem without holding
/// namespace state locked.
pub fn mounted_root_location(device: DeviceId) -> AxResult<Location> {
    let detached_or_retained = {
        let mut mounts = LIVE_SUPERBLOCK_MOUNTS.lock();
        mounts.retain(|_, mountpoint| mountpoint.strong_count() != 0);
        mounts.values().find_map(|mountpoint| {
            let mountpoint = mountpoint.upgrade()?;
            (linux_device_id(mountpoint.device()) == device).then_some(mountpoint)
        })
    };
    if let Some(mountpoint) = detached_or_retained {
        return Ok(mountpoint.root_location());
    }

    let attached = {
        let records = MOUNT_RECORDS.lock();
        records
            .iter()
            .find_map(|record| {
                (record.dev == device.0)
                    .then(|| record.mountpoint.upgrade())
                    .flatten()
            })
            .ok_or(AxError::InvalidInput)?
    };
    Ok(attached.root_location())
}

fn register_live_superblock_mount(mountpoint: &Arc<Mountpoint>) -> VfsResult<()> {
    let mut mounts = LIVE_SUPERBLOCK_MOUNTS.lock();
    mounts.retain(|_, entry| entry.strong_count() != 0);
    let mount_id = mountpoint.mount_id();
    if mounts.contains_key(&mount_id) {
        mounts.insert(mount_id, Arc::downgrade(mountpoint));
        return Ok(());
    }
    if mounts.len() >= MAX_LIVE_SUPERBLOCK_MOUNTS {
        return Err(AxError::StorageFull);
    }
    mounts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    mounts.insert(mount_id, Arc::downgrade(mountpoint));
    Ok(())
}

fn register_live_superblock_tree(records: &[MountRecord]) -> VfsResult<()> {
    let mut mounts = LIVE_SUPERBLOCK_MOUNTS.lock();
    mounts.retain(|_, entry| entry.strong_count() != 0);
    let additional = records
        .iter()
        .filter(|record| !mounts.contains_key(&record.mount_id))
        .count();
    if mounts
        .len()
        .checked_add(additional)
        .is_none_or(|total| total > MAX_LIVE_SUPERBLOCK_MOUNTS)
    {
        return Err(AxError::StorageFull);
    }
    mounts
        .try_reserve(additional)
        .map_err(|_| AxError::NoMemory)?;
    for record in records {
        mounts.insert(record.mount_id, record.mountpoint.clone());
    }
    Ok(())
}

fn mount_extensions(flags: u32, metadata: MountMetadata) -> VfsResult<TypeMap> {
    let mut extensions = TypeMap::new();
    let retired = extensions.try_insert(LinuxMountState {
        flags: AtomicU32::new(flags),
        activity_epoch: AtomicU64::new(0),
        readonly_floor: flags & MS_RDONLY != 0,
        metadata: Mutex::new(metadata),
    })?;
    drop(retired);
    Ok(extensions)
}

#[cfg(test)]
pub(crate) fn initialize_test_mount(mountpoint: &Arc<Mountpoint>, flags: u32) -> VfsResult<()> {
    mountpoint.initialize_extensions(mount_extensions(
        flags,
        MountMetadata {
            source: String::new(),
            fs_type: String::new(),
            root: String::new(),
            data: String::new(),
        },
    )?)
}

pub fn initialize_root_mount(
    mountpoint: &Arc<Mountpoint>,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<()> {
    let dev = linux_device_id(mountpoint.device()).0;
    let record_metadata = metadata.try_clone()?;
    let target = try_string("/")?;
    let extensions = mount_extensions(flags, metadata)?;
    let mut records = MOUNT_RECORDS.lock();
    if records
        .iter()
        .any(|record| record.mount_id == mountpoint.mount_id())
    {
        return Err(AxError::AlreadyExists);
    }
    if records.len() >= MAX_MOUNT_RECORDS {
        return Err(AxError::StorageFull);
    }
    records.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    mountpoint.initialize_extensions(extensions)?;
    records.push(MountRecord {
        mount_id: mountpoint.mount_id(),
        parent_id: 0,
        root: record_metadata.root,
        source: record_metadata.source,
        target,
        fs_type: record_metadata.fs_type,
        data: record_metadata.data,
        dev,
        flags,
        expire_epoch: None,
        mountpoint: Arc::downgrade(mountpoint),
    });
    Ok(())
}

pub fn mount_with_flags(
    target: &Location,
    filesystem: &Filesystem,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<Arc<Mountpoint>> {
    target.mount_with_extensions(filesystem, mount_extensions(flags, metadata)?)
}

pub fn new_detached_with_flags(
    filesystem: &Filesystem,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<Arc<Mountpoint>> {
    let mountpoint =
        Mountpoint::new_detached_with_extensions(filesystem, mount_extensions(flags, metadata)?)?;
    register_live_superblock_mount(&mountpoint)?;
    Ok(mountpoint)
}

fn mount_state(mountpoint: &Mountpoint) -> AxResult<Arc<LinuxMountState>> {
    mountpoint
        .extension_shared::<LinuxMountState>()
        .ok_or(AxError::Io)
}

fn flags_for_mountpoint(mountpoint: &Mountpoint) -> Option<u32> {
    mountpoint
        .extension::<LinuxMountState>()
        .map(|state| state.flags.load(Ordering::Acquire))
}

fn activity_epoch(mountpoint: &Mountpoint) -> AxResult<u64> {
    mountpoint
        .extension::<LinuxMountState>()
        .map(|state| state.activity_epoch.load(Ordering::Relaxed))
        .ok_or(AxError::Io)
}

pub fn note_mount_access(loc: &Location) {
    if let Some(state) = loc.mountpoint().extension::<LinuxMountState>() {
        state.activity_epoch.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn flags_for_location(loc: &Location) -> AxResult<u32> {
    flags_for_mountpoint(loc.mountpoint()).ok_or(AxError::Io)
}

/// Returns whether privilege transitions are disabled for this exact mount.
pub(crate) fn is_nosuid(loc: &Location) -> AxResult<bool> {
    Ok(flags_for_location(loc)? & MS_NOSUID != 0)
}

pub fn metadata_for_location(loc: &Location) -> AxResult<MountMetadata> {
    let state = mount_state(loc.mountpoint())?;
    state.metadata.lock().try_clone()
}

fn joined_mount_root(base: &str, path_in_mount: &str) -> AxResult<String> {
    if !base.starts_with('/') || !path_in_mount.starts_with('/') {
        return Err(AxError::Io);
    }
    if path_in_mount == "/" {
        return try_string(base);
    }
    if base == "/" {
        return try_string(path_in_mount);
    }
    let mut joined = String::new();
    joined
        .try_reserve(base.len().saturating_add(path_in_mount.len()))
        .map_err(|_| AxError::NoMemory)?;
    joined.push_str(base.trim_end_matches('/'));
    joined.push_str(path_in_mount);
    Ok(joined)
}

pub fn clone_metadata_for_bind(loc: &Location) -> AxResult<MountMetadata> {
    let mut metadata = metadata_for_location(loc)?;
    let path_in_mount = loc.path_in_mount().map_err(|_| AxError::Io)?;
    metadata.root = joined_mount_root(&metadata.root, path_in_mount.as_ref())?;
    Ok(metadata)
}

pub fn update_detached_mount_flags(
    root: &Arc<Mountpoint>,
    recursive: bool,
    mut update: impl FnMut(u32) -> AxResult<u32>,
) -> AxResult<()> {
    let mountpoints = if recursive {
        root.subtree_mountpoints()?
    } else {
        let mut mountpoints = Vec::new();
        mountpoints.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        mountpoints.push(root.clone());
        mountpoints
    };
    let mut updates = Vec::new();
    updates
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    for mountpoint in mountpoints {
        let state = mountpoint
            .extension_shared::<LinuxMountState>()
            .ok_or(AxError::Io)?;
        let next = update(state.flags.load(Ordering::Acquire))?;
        if state.readonly_floor && next & MS_RDONLY == 0 {
            return Err(AxError::OperationNotSupported);
        }
        updates.push((state, next));
    }
    for (state, flags) in updates {
        state.flags.store(flags, Ordering::Release);
    }
    Ok(())
}

pub fn register_linux_device(identity: FilesystemIdentity, linux_device: DeviceId) -> AxResult<()> {
    let vfs_device = identity.device();
    if vfs_device == 0 {
        return Ok(());
    }
    let mut devices = LINUX_DEVICE_IDS.lock();
    devices.retain(|_, (_, identity)| identity.upgrade().is_some());
    if let Some(entry) = devices.get_mut(&vfs_device) {
        *entry = (linux_device, identity.downgrade());
        return Ok(());
    }
    devices.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    devices.insert(vfs_device, (linux_device, identity.downgrade()));
    Ok(())
}

pub fn linux_device_id(vfs_device: u64) -> DeviceId {
    if vfs_device == 0 {
        return DeviceId::default();
    }

    let mut devices = LINUX_DEVICE_IDS.lock();
    devices.retain(|_, (_, identity)| identity.upgrade().is_some());
    if let Some((device, _)) = devices.get(&vfs_device) {
        return *device;
    }
    drop(devices);

    let minor = vfs_device as u32;
    DeviceId::new(0, if minor == 0 { u32::MAX } else { minor })
}

pub fn extra_block_device_id(index: usize) -> Option<DeviceId> {
    let minor = index.checked_add(1)?.checked_mul(16)?;
    Some(DeviceId::new(8, u32::try_from(minor).ok()?))
}

pub fn attach_tree_and_record(root: &Arc<Mountpoint>, target: &Location) -> VfsResult<()> {
    if root.is_attached() {
        return Err(AxError::ResourceBusy);
    }
    let target_path = target.absolute_path().map_err(|_| AxError::Io)?;
    let mountpoints = root.subtree_mountpoints()?;
    let mut pending_ids = HashSet::new();
    pending_ids
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut committed = Vec::new();
    committed
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    for mountpoint in mountpoints {
        if !pending_ids.insert(mountpoint.mount_id()) {
            return Err(AxError::Io);
        }
        let state = mount_state(&mountpoint)?;
        let flags = state.flags.load(Ordering::Acquire);
        let metadata = state.metadata.lock().try_clone()?;
        let (parent_id, mount_target) = if Arc::ptr_eq(&mountpoint, root) {
            (
                target.mountpoint().mount_id(),
                try_string(target_path.as_ref())?,
            )
        } else {
            let attachment = mountpoint.location().ok_or(AxError::Io)?;
            let relative = mountpoint
                .root_location()
                .absolute_path()
                .map_err(|_| AxError::Io)?;
            (
                attachment.mountpoint().mount_id(),
                joined_path(target_path.as_ref(), relative.as_ref())?,
            )
        };
        committed.push(MountRecord {
            mount_id: mountpoint.mount_id(),
            parent_id,
            root: metadata.root,
            source: metadata.source,
            target: mount_target,
            fs_type: metadata.fs_type,
            data: metadata.data,
            dev: linux_device_id(mountpoint.device()).0,
            flags,
            expire_epoch: None,
            mountpoint: Arc::downgrade(&mountpoint),
        });
    }

    let mut records = MOUNT_RECORDS.lock();
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&record_index, target.mountpoint())?;
    if records
        .iter()
        .any(|record| pending_ids.contains(&record.mount_id))
    {
        return Err(axfs_ng_vfs::VfsError::AlreadyExists);
    }
    if records
        .len()
        .checked_add(committed.len())
        .is_none_or(|total| total > MAX_MOUNT_RECORDS)
    {
        return Err(axfs_ng_vfs::VfsError::StorageFull);
    }
    records
        .try_reserve(committed.len())
        .map_err(|_| axfs_ng_vfs::VfsError::NoMemory)?;
    // Register every child before publication. Recursive bind children may
    // refer to different superblocks and remain live through detached-tree or
    // lazy-unmount references after their namespace records disappear.
    register_live_superblock_tree(&committed)?;
    root.attach_to(target)?;
    records.extend(committed);
    Ok(())
}

fn validate_record_state(record: &MountRecord) -> AxResult<Arc<Mountpoint>> {
    let mountpoint = record.mountpoint.upgrade().ok_or(AxError::Io)?;
    if mountpoint.mount_id() != record.mount_id {
        return Err(AxError::Io);
    }
    let state = mount_state(&mountpoint)?;
    if state.flags.load(Ordering::Acquire) != record.flags {
        return Err(AxError::Io);
    }
    let metadata = state.metadata.lock();
    if metadata.source != record.source
        || metadata.fs_type != record.fs_type
        || metadata.root != record.root
        || metadata.data != record.data
    {
        return Err(AxError::Io);
    }
    drop(metadata);
    Ok(mountpoint)
}

fn validate_registered_mount(
    index: &MountRecordIndex<'_>,
    mount_id: u64,
) -> AxResult<Arc<Mountpoint>> {
    validate_record_state(index.record(mount_id)?)
}

fn validate_mount_attachment(
    index: &MountRecordIndex<'_>,
    mountpoint: &Arc<Mountpoint>,
) -> AxResult<Option<Arc<Mountpoint>>> {
    let record = index.record(mountpoint.mount_id())?;
    let registered = validate_registered_mount(index, mountpoint.mount_id())?;
    if !Arc::ptr_eq(&registered, mountpoint) {
        return Err(AxError::Io);
    }
    if mountpoint.is_root() {
        if record.parent_id != 0 || record.target != "/" {
            return Err(AxError::Io);
        }
        return Ok(None);
    }

    let attachment = mountpoint.location().ok_or(AxError::Io)?;
    if attachment.mountpoint().mount_id() != record.parent_id {
        return Err(AxError::Io);
    }
    let parent = validate_registered_mount(index, record.parent_id)?;
    if !Arc::ptr_eq(&parent, attachment.mountpoint()) {
        return Err(AxError::Io);
    }
    let actual_target = mountpoint
        .root_location()
        .absolute_path()
        .map_err(|_| AxError::Io)?;
    if actual_target.as_ref() != record.target {
        return Err(AxError::Io);
    }
    Ok(Some(parent))
}

fn validate_registered_mount_chain(
    index: &MountRecordIndex<'_>,
    mountpoint: &Arc<Mountpoint>,
) -> AxResult<()> {
    let mut current = mountpoint.clone();
    for _ in 0..=index.records.len() {
        let Some(parent) = validate_mount_attachment(index, &current)? else {
            return Ok(());
        };
        current = parent;
    }
    Err(AxError::Io)
}

fn validate_registered_subtree(
    index: &MountRecordIndex<'_>,
    root: &Arc<Mountpoint>,
) -> AxResult<HashSet<u64>> {
    let root_id = root.mount_id();
    validate_registered_mount_chain(index, root)?;
    let ledger_ids = index.subtree_ids(root_id)?;
    let mountpoints = root.subtree_mountpoints()?;
    let mut topology_ids = HashSet::new();
    topology_ids
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    for mountpoint in mountpoints {
        if !topology_ids.insert(mountpoint.mount_id()) {
            return Err(AxError::Io);
        }
    }
    if topology_ids != ledger_ids {
        return Err(AxError::Io);
    }

    let mut seen_records = HashSet::new();
    seen_records
        .try_reserve(ledger_ids.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in index
        .records
        .iter()
        .filter(|record| ledger_ids.contains(&record.mount_id))
    {
        if !seen_records.insert(record.mount_id) {
            return Err(AxError::Io);
        }
        let mountpoint = validate_record_state(record)?;
        validate_mount_attachment(index, &mountpoint)?;
    }
    if seen_records != ledger_ids {
        return Err(AxError::Io);
    }
    Ok(ledger_ids)
}

pub fn recursive_bind_submounts(source: &Location) -> AxResult<Vec<BindSubmount>> {
    let source_mount_id = source.mountpoint().mount_id();
    let source_path = source.absolute_path().map_err(|_| AxError::Io)?;
    let records = MOUNT_RECORDS.lock();
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_subtree(&record_index, source.mountpoint())?;
    let mut admitted = HashMap::new();
    admitted
        .try_reserve(records.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut visited = HashSet::new();
    visited
        .try_reserve(records.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut selected = Vec::new();
    selected
        .try_reserve(records.len())
        .map_err(|_| AxError::NoMemory)?;
    admitted.insert(source_mount_id, 0usize);

    loop {
        let old_visited = visited.len();
        for record in records.iter() {
            if record.mount_id == source_mount_id || visited.contains(&record.mount_id) {
                continue;
            }
            let direct_child = record.parent_id == source_mount_id;
            let Some(parent_depth) = admitted.get(&record.parent_id).copied() else {
                continue;
            };

            let mountpoint = record.mountpoint.upgrade().ok_or(AxError::Io)?;
            let attachment = mountpoint.location().ok_or(AxError::Io)?;
            if attachment.mountpoint().mount_id() != record.parent_id {
                return Err(AxError::Io);
            }
            if direct_child && !source.entry().is_ancestor_of(attachment.entry())? {
                visited.insert(record.mount_id);
                continue;
            }

            visited.insert(record.mount_id);
            if record.flags & MS_UNBINDABLE != 0 {
                continue;
            }
            let depth = parent_depth.checked_add(1).ok_or(AxError::Io)?;
            admitted.insert(record.mount_id, depth);
            let root_location = mountpoint.root_location();
            let absolute = root_location.absolute_path().map_err(|_| AxError::Io)?;
            let relative_path = path_suffix(source_path.as_ref(), absolute.as_ref())
                .ok_or(AxError::Io)
                .and_then(try_string)?;
            selected.push((
                depth,
                BindSubmount {
                    source: root_location,
                    relative_path,
                    metadata: MountMetadata::try_from_strs(
                        &record.source,
                        &record.fs_type,
                        &record.root,
                        &record.data,
                    )?,
                    flags: record.flags,
                },
            ));
        }
        if visited.len() == old_visited {
            break;
        }
    }

    selected.sort_by_key(|(depth, _)| *depth);
    let mut mounts = Vec::new();
    mounts
        .try_reserve_exact(selected.len())
        .map_err(|_| AxError::NoMemory)?;
    for (_, mount) in selected {
        mounts.push(mount);
    }
    Ok(mounts)
}

pub fn remount_with_data(
    target: &Location,
    source: String,
    fs_type: String,
    flags: u32,
    data: String,
) -> AxResult<()> {
    let mut records = MOUNT_RECORDS.lock();
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&record_index, target.mountpoint())?;
    let index = *record_index
        .by_id
        .get(&target.mountpoint().mount_id())
        .ok_or(AxError::Io)?;
    let mountpoint = validate_record_state(&records[index])?;
    let state = mount_state(&mountpoint)?;
    let record = &mut records[index];
    if (!source.is_empty() && source != record.source)
        || (!fs_type.is_empty() && fs_type != record.fs_type)
    {
        return Err(AxError::InvalidInput);
    }
    if !data.is_empty() {
        return Err(AxError::OperationNotSupported);
    }
    if state.readonly_floor && flags & MS_RDONLY == 0 {
        return Err(AxError::OperationNotSupported);
    }
    state.flags.store(flags, Ordering::Release);
    record.flags = flags;
    record.expire_epoch = None;
    Ok(())
}

pub fn try_update_flags_for_mounts(
    root_mount_id: u64,
    recursive: bool,
    mut update: impl FnMut(u32) -> AxResult<u32>,
) -> AxResult<bool> {
    let mut records = MOUNT_RECORDS.lock();
    let Some(updates) = prepare_mount_flag_updates(
        &records,
        root_mount_id,
        recursive,
        |record| {
            let mountpoint = validate_record_state(record)?;
            let state = mount_state(&mountpoint)?;
            let current = state.flags.load(Ordering::Acquire);
            Ok((state, current))
        },
        &mut update,
    )?
    else {
        return Ok(false);
    };

    for (_, state, flags) in &updates {
        if state.readonly_floor && *flags & MS_RDONLY == 0 {
            return Err(AxError::OperationNotSupported);
        }
    }
    for (index, state, flags) in updates {
        state.flags.store(flags, Ordering::Release);
        records[index].flags = flags;
        records[index].expire_epoch = None;
    }
    Ok(true)
}

fn prepare_mount_flag_updates<S>(
    records: &[MountRecord],
    root_mount_id: u64,
    recursive: bool,
    mut current: impl FnMut(&MountRecord) -> AxResult<(S, u32)>,
    mut update: impl FnMut(u32) -> AxResult<u32>,
) -> AxResult<Option<Vec<(usize, S, u32)>>> {
    if !records
        .iter()
        .any(|record| record.mount_id == root_mount_id)
    {
        return Ok(None);
    }

    let subtree = if recursive {
        Some(subtree_mount_ids(records, root_mount_id)?)
    } else {
        None
    };
    let mut updates = Vec::new();
    updates
        .try_reserve(if recursive { records.len() } else { 1 })
        .map_err(|_| AxError::NoMemory)?;
    for (index, record) in records.iter().enumerate() {
        let selected = record.mount_id == root_mount_id
            || subtree
                .as_ref()
                .is_some_and(|ids| ids.contains(&record.mount_id));
        if selected {
            let (state, flags) = current(record)?;
            updates.push((index, state, update(flags)?));
        }
    }
    Ok(Some(updates))
}

pub fn move_tree_and_records(old: &Location, target: &Location) -> AxResult<()> {
    let root = old.mountpoint().clone();
    let root_mount_id = root.mount_id();
    let old_target = old.absolute_path().map_err(|_| AxError::Io)?;
    let new_target = target.absolute_path().map_err(|_| AxError::Io)?;
    let new_parent_id = target.mountpoint().mount_id();

    let mut records = MOUNT_RECORDS.lock();
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&record_index, target.mountpoint())?;
    let subtree = validate_registered_subtree(&record_index, &root)?;
    let mut updates = Vec::new();
    updates
        .try_reserve(subtree.len())
        .map_err(|_| AxError::NoMemory)?;
    for (index, record) in records.iter().enumerate() {
        if !subtree.contains(&record.mount_id) {
            continue;
        }
        let suffix = path_suffix(old_target.as_ref(), &record.target).ok_or(AxError::Io)?;
        updates.push((
            index,
            joined_path(new_target.as_ref(), suffix)?,
            record.mount_id == root_mount_id,
        ));
    }
    if updates.len() != subtree.len() {
        return Err(AxError::Io);
    }

    old.move_mount_to(target)?;
    for (index, target, is_root) in updates {
        let record = &mut records[index];
        record.target = target;
        record.expire_epoch = None;
        if is_root {
            record.parent_id = new_parent_id;
        }
    }
    Ok(())
}

/// Performs the mount-tree and mount-record half of `pivot_root(2)` as one
/// namespace operation. Callers must hold [`namespace_operation`].
pub fn pivot_root_and_records(
    old_root: &Location,
    new_root: &Location,
    put_old: &Location,
) -> AxResult<()> {
    let new_mount = new_root.mountpoint();
    if new_mount.is_root() {
        return Err(AxError::ResourceBusy);
    }
    if !old_root.is_root_of_mount()
        || !new_root.is_root_of_mount()
        || !put_old.is_dir()
        || !Arc::ptr_eq(put_old.mountpoint(), new_mount)
    {
        return Err(AxError::InvalidInput);
    }
    let new_root_path = new_root.absolute_path().map_err(|_| AxError::Io)?;
    let put_old_path = put_old.absolute_path().map_err(|_| AxError::Io)?;
    let put_old_new_path = path_suffix(new_root_path.as_ref(), put_old_path.as_ref())
        .filter(|path| !path.is_empty())
        .ok_or(AxError::InvalidInput)
        .and_then(try_string)?;

    let mut records = MOUNT_RECORDS.lock();
    let index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&index, new_mount)?;
    let new_subtree = validate_registered_subtree(&index, new_mount)?;
    let old_root_index = records
        .iter()
        .position(|record| record.parent_id == 0)
        .ok_or(AxError::Io)?;
    let namespace_root = validate_record_state(&records[old_root_index])?;
    if !namespace_root.is_root() {
        return Err(AxError::Io);
    }
    if !Arc::ptr_eq(old_root.mountpoint(), &namespace_root) {
        return Err(AxError::InvalidInput);
    }

    // All allocations and string construction happen before the VFS tree is
    // touched. Once `pivot_root_to` succeeds, publishing this prepared ledger
    // is infallible and preserves a single atomic namespace view.
    let mut updates = Vec::new();
    updates
        .try_reserve_exact(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for (record_index, record) in records.iter().enumerate() {
        let (target, parent_id) = if new_subtree.contains(&record.mount_id) {
            let suffix = path_suffix(new_root_path.as_ref(), &record.target).ok_or(AxError::Io)?;
            let target = if suffix.is_empty() {
                try_string("/")?
            } else {
                try_string(suffix)?
            };
            (
                target,
                (record.mount_id == new_mount.mount_id()).then_some(0),
            )
        } else {
            (
                joined_path(&put_old_new_path, &record.target)?,
                (record_index == old_root_index).then_some(new_mount.mount_id()),
            )
        };
        updates.push((record_index, target, parent_id));
    }

    new_root.pivot_root_to(put_old)?;
    for (record_index, target, parent_id) in updates {
        let record = &mut records[record_index];
        record.target = target;
        if let Some(parent_id) = parent_id {
            record.parent_id = parent_id;
        }
        record.expire_epoch = None;
    }
    Ok(())
}

#[cfg(test)]
fn move_tree_records(
    records: &mut [MountRecord],
    root_mount_id: u64,
    old_target: &str,
    new_target: &str,
    new_parent_id: u64,
) {
    let subtree = subtree_mount_ids(records, root_mount_id).unwrap();
    for record in records.iter_mut() {
        if !subtree.contains(&record.mount_id) {
            continue;
        }

        if let Some(suffix) = path_suffix(old_target, &record.target) {
            record.target = joined_path(new_target, suffix).unwrap();
            record.expire_epoch = None;
        }
        if record.mount_id == root_mount_id {
            record.parent_id = new_parent_id;
        }
    }
}

fn subtree_mount_ids(records: &[MountRecord], root_mount_id: u64) -> AxResult<HashSet<u64>> {
    MountRecordIndex::new(records)?.subtree_ids(root_mount_id)
}

fn advance_expire_probe(
    expire_epoch: &mut Option<u64>,
    check_unmountable: impl FnOnce() -> AxResult<()>,
    current_epoch: impl FnOnce() -> AxResult<u64>,
) -> AxResult<ExpireProbe> {
    check_unmountable()?;
    let current_epoch = current_epoch()?;
    if *expire_epoch == Some(current_epoch) {
        Ok(ExpireProbe::Ready)
    } else {
        *expire_epoch = Some(current_epoch);
        Ok(ExpireProbe::Retry)
    }
}

pub fn unmount_and_remove_records(target: Location, lazy: bool, expire: bool) -> AxResult<()> {
    let root = target.mountpoint().clone();
    let root_mount_id = root.mount_id();

    if lazy {
        if expire {
            return Err(AxError::InvalidInput);
        }
        let mut records = MOUNT_RECORDS.lock();
        let record_index = MountRecordIndex::new(&records)?;
        let ids = validate_registered_subtree(&record_index, &root)?;
        target.lazy_unmount()?;
        records.retain(|record| !ids.contains(&record.mount_id));
        return Ok(());
    }

    let ids = {
        let mut records = MOUNT_RECORDS.lock();
        let record_index = MountRecordIndex::new(&records)?;
        let ids = validate_registered_subtree(&record_index, &root)?;
        if expire {
            let record = records
                .iter_mut()
                .find(|record| record.mount_id == root_mount_id)
                .ok_or(AxError::Io)?;
            if advance_expire_probe(
                &mut record.expire_epoch,
                || target.check_unmountable(),
                || activity_epoch(&root),
            )? == ExpireProbe::Retry
            {
                return Err(AxError::WouldBlock);
            }
        } else {
            target.check_unmountable()?;
        }
        ids
    };

    let flushed = target.prepare_unmount()?.flush()?;
    let mut records = MOUNT_RECORDS.lock();
    if expire {
        let record = records
            .iter()
            .position(|record| record.mount_id == root_mount_id)
            .ok_or(AxError::Io)?;
        if advance_expire_probe(
            &mut records[record].expire_epoch,
            || Ok(()),
            || activity_epoch(&root),
        )? == ExpireProbe::Retry
        {
            drop(records);
            drop(flushed);
            return Err(AxError::WouldBlock);
        }
    }
    flushed.commit()?;
    records.retain(|record| !ids.contains(&record.mount_id));
    Ok(())
}

fn path_suffix<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if path == base {
        Some("")
    } else if base == "/" && path.starts_with('/') {
        Some(path)
    } else {
        path.strip_prefix(base)
            .filter(|suffix| suffix.starts_with('/'))
    }
}

fn joined_path(base: &str, suffix: &str) -> AxResult<String> {
    if suffix.is_empty() || suffix == "/" {
        return try_string(base);
    }
    if base == "/" {
        return try_string(suffix);
    }
    let mut joined = String::new();
    joined
        .try_reserve(base.len().saturating_add(suffix.len()))
        .map_err(|_| AxError::NoMemory)?;
    joined.push_str(base);
    joined.push_str(suffix);
    Ok(joined)
}

pub fn is_readonly(loc: &Location) -> AxResult<bool> {
    Ok(flags_for_location(loc)? & MS_RDONLY != 0)
}

pub fn is_nodev(loc: &Location) -> AxResult<bool> {
    Ok(flags_for_location(loc)? & MS_NODEV != 0)
}

pub fn is_noexec(loc: &Location) -> AxResult<bool> {
    Ok(flags_for_location(loc)? & MS_NOEXEC != 0)
}

pub fn has_mandatory_locking(loc: &Location) -> AxResult<bool> {
    Ok(flags_for_location(loc)? & MS_MANDLOCK != 0)
}

pub fn should_follow_symlink(loc: &Location) -> bool {
    flags_for_mountpoint(loc.mountpoint()).is_some_and(|flags| flags & MS_NOSYMFOLLOW == 0)
}

fn uses_relatime(flags: u32) -> bool {
    flags & (MS_NOATIME | MS_STRICTATIME) == 0
}

pub fn should_update_atime(loc: &Location) -> bool {
    let Some(flags) = flags_for_mountpoint(loc.mountpoint()) else {
        return false;
    };
    if flags & MS_NOATIME != 0 {
        return false;
    }

    let metadata = match loc.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if flags & MS_NODIRATIME != 0 && metadata.node_type == NodeType::Directory {
        return false;
    }
    if flags & MS_STRICTATIME != 0 {
        return true;
    }

    let now: axfs_ng_vfs::Timestamp = wall_time().into();
    metadata.atime <= metadata.mtime
        || metadata.atime <= metadata.ctime
        || now.seconds().saturating_sub(metadata.atime.seconds())
            >= RELATIME_MAX_AGE_SECS as i64
}

pub fn mount_options(flags: u32) -> String {
    let mut options = Vec::new();
    options.push(if flags & MS_RDONLY != 0 { "ro" } else { "rw" });
    if flags & MS_NOSUID != 0 {
        options.push("nosuid");
    }
    if flags & MS_NODEV != 0 {
        options.push("nodev");
    }
    if flags & MS_NOEXEC != 0 {
        options.push("noexec");
    }
    if flags & MS_MANDLOCK != 0 {
        options.push("mand");
    }
    if flags & MS_NOSYMFOLLOW != 0 {
        options.push("nosymfollow");
    }
    if flags & MS_NOATIME != 0 {
        options.push("noatime");
    } else if flags & MS_STRICTATIME != 0 {
        options.push("strictatime");
    } else {
        options.push("relatime");
    }
    if flags & MS_NODIRATIME != 0 {
        options.push("nodiratime");
    }
    if flags & MS_SHARED != 0 {
        options.push("shared");
    } else if flags & MS_SLAVE != 0 {
        options.push("slave");
    } else if flags & MS_PRIVATE != 0 {
        options.push("private");
    } else if flags & MS_UNBINDABLE != 0 {
        options.push("unbindable");
    }
    options.join(",")
}

pub fn statfs_mount_flags(loc: &Location, base_flags: u32) -> AxResult<u32> {
    let mount_flags = flags_for_location(loc)?;
    let mut result = base_flags;
    result |= mount_flags
        & (MS_RDONLY
            | MS_NOSUID
            | MS_NODEV
            | MS_NOEXEC
            | MS_REMOUNT
            | MS_MANDLOCK
            | MS_NOATIME
            | MS_NODIRATIME);
    if uses_relatime(mount_flags) {
        result |= ST_RELATIME;
    }
    if mount_flags & MS_NOSYMFOLLOW != 0 {
        result |= ST_NOSYMFOLLOW;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;
    use crate::pseudofs::MemoryFs;

    fn record(mount_id: u64, parent_id: u64, target: &str) -> MountRecord {
        MountRecord {
            mount_id,
            parent_id,
            root: "/".to_string(),
            source: "none".to_string(),
            target: target.to_string(),
            fs_type: "tmpfs".to_string(),
            data: String::new(),
            dev: mount_id,
            flags: 0,
            expire_epoch: None,
            mountpoint: Weak::new(),
        }
    }

    #[test]
    fn device_lookup_keeps_detached_live_superblocks_visible() {
        let filesystem = MemoryFs::new().unwrap();
        let mountpoint = new_detached_with_flags(
            &filesystem,
            0,
            MountMetadata::try_from_strs("none", "tmpfs", "/", "").unwrap(),
        )
        .unwrap();
        let device = linux_device_id(mountpoint.device());

        let root = mounted_root_location(device).unwrap();
        assert_eq!(root.mountpoint().device(), mountpoint.device());
        assert!(!mountpoint.is_attached());
    }

    #[test]
    fn first_expire_probe_records_the_epoch_and_retries() {
        let mut expire_epoch = None;

        assert_eq!(
            advance_expire_probe(&mut expire_epoch, || Ok(()), || Ok(7)).unwrap(),
            ExpireProbe::Retry
        );
        assert_eq!(expire_epoch, Some(7));
    }

    #[test]
    fn unchanged_second_expire_probe_is_ready_to_commit() {
        let mut expire_epoch = Some(7);

        assert_eq!(
            advance_expire_probe(&mut expire_epoch, || Ok(()), || Ok(7)).unwrap(),
            ExpireProbe::Ready
        );
        assert_eq!(expire_epoch, Some(7));
    }

    #[test]
    fn changed_expire_epoch_rearms_the_probe() {
        let mut expire_epoch = Some(7);

        assert_eq!(
            advance_expire_probe(&mut expire_epoch, || Ok(()), || Ok(8)).unwrap(),
            ExpireProbe::Retry
        );
        assert_eq!(expire_epoch, Some(8));
    }

    #[test]
    fn busy_first_expire_probe_does_not_record_an_epoch() {
        let mut expire_epoch = None;
        let epoch_loaded = core::cell::Cell::new(false);

        assert_eq!(
            advance_expire_probe(
                &mut expire_epoch,
                || Err(AxError::ResourceBusy),
                || {
                    epoch_loaded.set(true);
                    Ok(7)
                },
            ),
            Err(AxError::ResourceBusy)
        );
        assert_eq!(expire_epoch, None);
        assert!(!epoch_loaded.get());
    }

    #[test]
    fn activity_during_the_flush_window_rearms_the_probe() {
        let mut expire_epoch = Some(7);

        assert_eq!(
            advance_expire_probe(&mut expire_epoch, || Ok(()), || Ok(9)).unwrap(),
            ExpireProbe::Retry
        );
        assert_eq!(expire_epoch, Some(9));
    }

    #[test]
    fn mount_subtree_uses_ids_instead_of_stacked_paths() {
        let records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt"),
            record(4, 3, "/mnt/nested"),
            record(5, 1, "/other"),
        ];

        let ids = subtree_mount_ids(&records, 3).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&3));
        assert!(ids.contains(&4));
    }

    #[test]
    fn recursive_flag_update_uses_only_the_selected_mount_subtree() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt/branch/nested"),
            record(4, 2, "/mnt/other"),
            record(5, 1, "/mnt/branch/unrelated-topology"),
        ];

        let updates = prepare_mount_flag_updates(
            &records,
            2,
            true,
            |record| Ok(((), record.flags)),
            |flags| Ok(flags | MS_RDONLY),
        )
        .unwrap()
        .unwrap();
        for (index, (), flags) in updates {
            records[index].flags = flags;
        }

        assert_ne!(records[1].flags & MS_RDONLY, 0);
        assert_ne!(records[2].flags & MS_RDONLY, 0);
        assert_ne!(records[3].flags & MS_RDONLY, 0);
        assert_eq!(records[4].flags & MS_RDONLY, 0);
    }

    #[test]
    fn failed_flag_preflight_does_not_mutate_any_record() {
        let records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt/sub"),
        ];
        let before = records
            .iter()
            .map(|record| record.flags)
            .collect::<Vec<_>>();

        let result = prepare_mount_flag_updates(
            &records,
            2,
            true,
            |record| {
                if record.mount_id == 3 {
                    Err(AxError::Io)
                } else {
                    Ok(((), 0))
                }
            },
            |flags| Ok(flags | MS_RDONLY),
        );

        assert!(matches!(result, Err(AxError::Io)));
        assert_eq!(
            records
                .iter()
                .map(|record| record.flags)
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn unrelated_mount_flags_preserve_default_relatime() {
        assert!(uses_relatime(0));
        assert!(uses_relatime(MS_NODEV | MS_NOEXEC));
        assert!(!uses_relatime(MS_NOATIME));
        assert!(!uses_relatime(MS_STRICTATIME));
    }

    #[test]
    fn move_tree_only_moves_the_selected_stacked_subtree() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt"),
            record(4, 3, "/mnt/nested"),
        ];

        move_tree_records(&mut records, 3, "/mnt", "/moved", 1);

        let lower = records.iter().find(|record| record.mount_id == 2).unwrap();
        let moved = records.iter().find(|record| record.mount_id == 3).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 4).unwrap();
        assert_eq!(lower.target, "/mnt");
        assert_eq!(lower.parent_id, 1);
        assert_eq!(moved.target, "/moved");
        assert_eq!(moved.parent_id, 1);
        assert_eq!(nested.target, "/moved/nested");
        assert_eq!(nested.parent_id, 3);
    }

    #[test]
    fn move_tree_rewrites_descendants_of_a_root_overmount() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/"),
            record(3, 2, "/nested"),
        ];

        move_tree_records(&mut records, 2, "/", "/moved", 1);

        let namespace_root = records.iter().find(|record| record.mount_id == 1).unwrap();
        let moved = records.iter().find(|record| record.mount_id == 2).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 3).unwrap();
        assert_eq!(namespace_root.target, "/");
        assert_eq!(moved.target, "/moved");
        assert_eq!(nested.target, "/moved/nested");
    }

    #[test]
    fn move_tree_to_namespace_root_does_not_add_a_second_separator() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/source"),
            record(3, 2, "/source/nested"),
        ];

        move_tree_records(&mut records, 2, "/source", "/", 1);

        let moved = records.iter().find(|record| record.mount_id == 2).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 3).unwrap();
        assert_eq!(moved.target, "/");
        assert_eq!(nested.target, "/nested");
    }
}
