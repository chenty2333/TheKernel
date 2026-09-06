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
    DeviceId, Filesystem, FilesystemIdentity, FsPath, FsPathBuf, Location, Mountpoint, NodeType,
    TypeMap, VfsResult, WeakFilesystemIdentity,
};
use axsync::Mutex as BlockingMutex;
#[cfg(not(test))]
use axsync::MutexGuard as BlockingMutexGuard;
use hashbrown::{HashMap, HashSet};
use spin::{Lazy, Mutex};

use crate::{
    task::{AsThread, UserNamespace},
    time::wall_time,
};

pub struct MountRecord {
    pub mount_id: u64,
    pub mount_id_old: u32,
    pub parent_id: u64,
    pub root: FsPathBuf,
    pub source: FsPathBuf,
    pub target: FsPathBuf,
    pub fs_type: String,
    pub data: String,
    pub dev: u64,
    pub flags: u32,
    pub expire_epoch: Option<u64>,
    mountpoint: Weak<Mountpoint>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct MountMetadata {
    pub source: FsPathBuf,
    pub fs_type: String,
    pub root: FsPathBuf,
    pub data: String,
    /// Resolved block-device members retained at mount publication.  This is
    /// intentionally device identity, not a pathname: detach/unmount and
    /// cloned mount metadata never re-walk a mutable `/dev` namespace.
    pub block_members: Vec<DeviceId>,
}

pub struct BindSubmount {
    pub source: Location,
    pub relative_path: FsPathBuf,
    pub metadata: MountMetadata,
    pub flags: u32,
}

/// Propagation state frozen with a detached tree.  It is applied during the
/// same topology publication that makes the tree visible, so an attached
/// recursive open_tree clone never exposes private placeholder mounts.
#[derive(Clone, Copy, Debug)]
pub struct DetachedMountPropagation {
    pub mount_id: u64,
    pub peer_group: Option<PeerGroup>,
    pub unbindable: bool,
}

/// The syscall adapter supplies the Linux attach grammar selected before VFS
/// mutation, so the ABI plan does not have to infer a bind from storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachKind {
    Attach,
    Bind { source_mount_id: u64 },
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

/// Pre-scheduler bootstrap seed. Once a `MountNamespace` exists, every live
/// lookup and mutation uses its `MountTopology`; this is not namespace state.
static BOOTSTRAP_MOUNT_RECORDS: BlockingMutex<Vec<MountRecord>> = BlockingMutex::new(Vec::new());
static BOOTSTRAP_MOUNT_RECORDS_GENERATION: AtomicU64 = AtomicU64::new(0);
/// The ABI planner's namespace generation.  It advances only after the VFS
/// mutation and record-ledger publication have both committed under the
/// namespace operation lock.
static MOUNTINFO_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
/// Propagation peer IDs span mount namespaces.  Unlike mount IDs they are
/// graph identities, so a replica materialized in a cloned namespace must not
/// allocate from that namespace's local mount-id sequence.
static PROPAGATION_PEER_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
// Detached mount FDs and lazily unmounted-but-referenced mounts remain live
// superblocks even though they have no namespace record. Keep weak mount roots
// so legacy device-number queries can follow Linux's user_get_super lifetime.
static LIVE_SUPERBLOCK_MOUNTS: Lazy<BlockingMutex<HashMap<u64, Weak<Mountpoint>>>> =
    Lazy::new(|| BlockingMutex::new(HashMap::new()));
// `Mountpoint::mount_id()` is the 64-bit identity used by statmount and mount
// topology internals.  The old name_to_handle_at ABI has a distinct, positive
// signed mount-ID namespace.
static NEXT_LEGACY_MOUNT_ID: AtomicU32 = AtomicU32::new(1);
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

/// Typed final-component automount dispatch.  Providers register a trigger
/// that publishes a mount and returns `true` only when the VFS must retry the
/// lookup against the newly visible tree.
type AutomountTrigger = fn(&Location) -> VfsResult<bool>;
static AUTOMOUNT_TRIGGERS: BlockingMutex<Vec<AutomountTrigger>> = BlockingMutex::new(Vec::new());

pub fn register_automount_trigger(trigger: AutomountTrigger) -> AxResult<()> {
    let mut triggers = AUTOMOUNT_TRIGGERS.lock();
    triggers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    triggers.push(trigger);
    Ok(())
}

pub fn trigger_automount(location: &Location) -> VfsResult<bool> {
    let triggers = AUTOMOUNT_TRIGGERS.lock();
    for trigger in triggers.iter().copied() {
        if trigger(location)? {
            return Ok(true);
        }
    }
    Ok(false)
}

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

/// Shared superblock identity retained by namespace-local mount instances.
///
/// The VFS mountpoint remains the storage authority.  This ledger object is
/// deliberately separate: it is what lets a future `MountNamespace` own a
/// copy-on-write topology without duplicating a filesystem or its block
/// device.
#[derive(Debug)]
pub struct LinuxSuperblock {
    pub identity: u64,
    pub fs_type: String,
}

impl LinuxSuperblock {
    pub fn try_new(identity: u64, fs_type: &str) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            identity,
            fs_type: try_string(fs_type)?,
        })
        .map_err(|_| AxError::NoMemory)
    }
}

/// One user-namespace mapping range, expressed in the namespace which owns
/// the mapping.  Translation itself belongs to VFS credential resolution;
/// retaining the immutable rows here makes mount topology snapshots and
/// statmount reporting race-free.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MountIdmapRange {
    pub inside: u32,
    pub outside: u32,
    pub length: u32,
}

/// Immutable credential view installed on an idmapped mount.
pub struct MountIdmap {
    /// The user namespace which owns this mount idmap.  Mounts pin the
    /// namespace for exactly as long as the idmap is reachable, matching the
    /// lifetime of Linux's `mnt_idmap`.  A scalar proc inode is not sufficient
    /// here: capability checks need the namespace ancestry graph.
    user_namespace: Arc<UserNamespace>,
    pub uid: Vec<MountIdmapRange>,
    pub gid: Vec<MountIdmapRange>,
}

impl fmt::Debug for MountIdmap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountIdmap")
            .field("user_namespace", &self.user_namespace.proc_inode())
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .finish()
    }
}

impl MountIdmap {
    pub(crate) fn try_new(
        user_namespace: Arc<UserNamespace>,
        uid: &[MountIdmapRange],
        gid: &[MountIdmapRange],
    ) -> AxResult<Arc<Self>> {
        let mut uid_rows = Vec::new();
        uid_rows
            .try_reserve_exact(uid.len())
            .map_err(|_| AxError::NoMemory)?;
        uid_rows.extend_from_slice(uid);
        let mut gid_rows = Vec::new();
        gid_rows
            .try_reserve_exact(gid.len())
            .map_err(|_| AxError::NoMemory)?;
        gid_rows.extend_from_slice(gid);
        Arc::try_new(Self {
            user_namespace,
            uid: uid_rows,
            gid: gid_rows,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) const fn user_namespace(&self) -> &Arc<UserNamespace> {
        &self.user_namespace
    }
}

/// A shared-propagation peer group.  `master` is the peer group from which a
/// slave receives propagation, never a mount ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerGroup {
    pub id: u64,
    pub master: Option<u64>,
}

/// Namespace-local mount instance.  Several instances may reference one
/// [`LinuxSuperblock`], but attributes, idmapping and propagation are per
/// mount as on Linux.
#[derive(Clone, Debug)]
pub struct Mount {
    pub id: u64,
    pub parent: Option<u64>,
    pub superblock: Arc<LinuxSuperblock>,
    /// The VFS mount instance belonging to this namespace graph.  The VFS
    /// still owns its lifetime; a namespace snapshot never revives a detached
    /// mount by itself.
    mountpoint: Weak<Mountpoint>,
    pub mount_id_old: u32,
    pub root: FsPathBuf,
    pub source: FsPathBuf,
    pub target: FsPathBuf,
    pub fs_type: String,
    pub data: String,
    pub dev: u64,
    /// `MNT_EXPIRE`'s two-step activity probe is namespace-local state.  It
    /// must survive ledger snapshots/replacements, but never be shared by
    /// another mount namespace that happens to reference the same superblock.
    pub expire_epoch: Option<u64>,
    pub flags: u32,
    pub idmap: Option<Arc<MountIdmap>>,
    pub peer_group: Option<PeerGroup>,
    pub unbindable: bool,
    /// Linux `MNT_LOCKED`: inherited mount trees made less privileged by a
    /// user-namespace transition cannot be moved beneath another mount.
    /// This is namespace-local state and is monotonic once installed.
    pub locked: bool,
    /// Detached mounts may receive an idmap before first exposure.  Once
    /// attached, Linux rejects replacement through mount_setattr.
    pub attached: bool,
}

impl Mount {
    fn try_record(&self) -> AxResult<MountRecord> {
        Ok(MountRecord {
            mount_id: self.id,
            mount_id_old: self.mount_id_old,
            parent_id: self.parent.unwrap_or(0),
            root: try_path(&self.root)?,
            source: try_path(&self.source)?,
            target: try_path(&self.target)?,
            fs_type: try_string(&self.fs_type)?,
            data: try_string(&self.data)?,
            dev: self.dev,
            flags: self.flags,
            expire_epoch: self.expire_epoch,
            mountpoint: self.mountpoint.clone(),
        })
    }

    fn try_from_record(record: &MountRecord, previous: Option<&Mount>) -> AxResult<Self> {
        let id = record.mount_id;
        let parent = (record.parent_id != 0).then_some(record.parent_id);
        if id == 0 || parent == Some(id) {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            id,
            parent,
            superblock: previous
                .map(|mount| mount.superblock.clone())
                .unwrap_or(LinuxSuperblock::try_new(record.dev, &record.fs_type)?),
            mountpoint: record.mountpoint.clone(),
            mount_id_old: record.mount_id_old,
            root: try_path(&record.root)?,
            source: try_path(&record.source)?,
            target: try_path(&record.target)?,
            fs_type: try_string(&record.fs_type)?,
            data: try_string(&record.data)?,
            dev: record.dev,
            expire_epoch: record.expire_epoch,
            flags: previous.map_or(record.flags, |mount| mount.flags),
            idmap: previous.and_then(|mount| mount.idmap.clone()),
            peer_group: previous.and_then(|mount| mount.peer_group),
            unbindable: previous.is_some_and(|mount| mount.unbindable),
            locked: previous.is_some_and(|mount| mount.locked),
            attached: true,
        })
    }

    pub fn mountpoint(&self) -> AxResult<Arc<Mountpoint>> {
        self.mountpoint.upgrade().ok_or(AxError::NotFound)
    }

    pub fn propagation(&self) -> u64 {
        if self.unbindable {
            thekernel_linux_mount::MS_UNBINDABLE as u64
        } else if let Some(peer) = self.peer_group {
            if peer.master.is_some() {
                thekernel_linux_mount::MS_SLAVE as u64
            } else {
                thekernel_linux_mount::MS_SHARED as u64
            }
        } else {
            thekernel_linux_mount::MS_PRIVATE as u64
        }
    }
}

#[derive(Clone, Debug)]
pub struct MountSetattrRequest {
    pub attr_set: u64,
    pub attr_clr: u64,
    pub propagation: u64,
    /// `Some(None)` removes an idmap during a replace-capable detached-mount
    /// operation; `Some(Some(_))` installs one; `None` preserves it.
    pub idmap: Option<Option<Arc<MountIdmap>>>,
    pub idmap_replace: bool,
}

#[derive(Clone, Debug)]
struct MountTopologyState {
    generation: u64,
    next_peer_group: u64,
    mounts: Vec<Mount>,
}

/// Copy-on-write, namespace-local mount topology ledger.
///
/// It has no global fallback: callers retain an `Arc<MountTopology>` in their
/// `MountNamespace`.  Existing global mount state remains a temporary owner
/// only until the NamespaceProxy binding slice migrates its call sites.
pub struct MountTopology {
    namespace_id: u64,
    // Own the namespace root physically. Child mounts are retained by the
    // VFS tree below it; record snapshots intentionally carry weak handles.
    root_mount: BlockingMutex<Arc<Mountpoint>>,
    state: BlockingMutex<MountTopologyState>,
}

#[derive(Clone, Debug)]
pub struct MountTopologySnapshot {
    pub namespace_id: u64,
    pub generation: u64,
    pub mounts: Vec<Mount>,
}

pub struct PreparedMountTopologyMutation {
    topology: Arc<MountTopology>,
    expected_generation: u64,
    next_root_mount: Arc<Mountpoint>,
    next: MountTopologyState,
}

/// Provider registrations for a topology clone are deliberately separate
/// from VFS mount construction.  A namespace is not externally reachable
/// until its Arc allocation and nsfs registry admission both succeed, so
/// FUSE/NFS state must not be published before those fallible steps.
#[derive(Clone, Copy)]
pub(crate) enum ClonedProviderMount {
    Fuse { source_id: u64, clone_id: u64 },
    Nfs { source_id: u64, clone_id: u64 },
}

/// A private mount graph plus its uncommitted provider registrations.
/// `activate_provider_mounts` is reversible until the owning
/// `MountNamespace` has passed registry admission.
pub(crate) struct PreparedMountTopologyClone {
    topology: Arc<MountTopology>,
    providers: Vec<ClonedProviderMount>,
    providers_active: bool,
}

impl PreparedMountTopologyClone {
    pub(crate) fn topology(&self) -> Arc<MountTopology> {
        self.topology.clone()
    }

    pub(crate) fn activate_provider_mounts(&mut self) -> AxResult<()> {
        if self.providers_active {
            return Err(AxError::BadState);
        }
        // Reserve before the first externally visible registration, so an
        // allocator failure cannot leave a partial activation behind.
        let mut active = Vec::new();
        active
            .try_reserve_exact(self.providers.len())
            .map_err(|_| AxError::NoMemory)?;
        for provider in self.providers.iter().copied() {
            let result = match provider {
                ClonedProviderMount::Fuse {
                    source_id,
                    clone_id,
                } => match crate::pseudofs::dev::fuse::mount_connection(source_id) {
                    Some(connection) => {
                        crate::pseudofs::dev::fuse::register_mount_connection(clone_id, &connection)
                    }
                    None => Err(AxError::NoSuchDevice),
                },
                ClonedProviderMount::Nfs {
                    source_id,
                    clone_id,
                } => crate::syscall::fs::clone_nfs_mount_registration(source_id, clone_id),
            };
            if let Err(error) = result {
                for provider in active.into_iter().rev() {
                    match provider {
                        ClonedProviderMount::Fuse { clone_id, .. } => {
                            crate::pseudofs::dev::fuse::unregister_mount_connection(clone_id);
                        }
                        ClonedProviderMount::Nfs { clone_id, .. } => {
                            crate::syscall::fs::unregister_nfs_mount(clone_id);
                        }
                    }
                }
                return Err(error);
            }
            active.push(provider);
        }
        self.providers_active = true;
        Ok(())
    }

    pub(crate) fn rollback_provider_mounts(&mut self) {
        if !self.providers_active {
            return;
        }
        for provider in self.providers.iter().copied().rev() {
            match provider {
                ClonedProviderMount::Fuse { clone_id, .. } => {
                    crate::pseudofs::dev::fuse::unregister_mount_connection(clone_id);
                }
                ClonedProviderMount::Nfs { clone_id, .. } => {
                    crate::syscall::fs::unregister_nfs_mount(clone_id);
                }
            }
        }
        self.providers_active = false;
    }

    /// The namespace registry and its provider registrations become visible
    /// together.  Once registry admission has succeeded, ownership moves to
    /// normal mount teardown; until then Drop is the failure receipt.
    pub(crate) fn take_active_provider_mounts(&mut self) -> Vec<ClonedProviderMount> {
        debug_assert!(self.providers_active);
        self.providers_active = false;
        core::mem::take(&mut self.providers)
    }
}

impl Drop for PreparedMountTopologyClone {
    fn drop(&mut self) {
        self.rollback_provider_mounts();
    }
}

/// Retires the provider registrations owned by a cloned mount namespace.
/// Normal mount teardown may already have removed individual IDs; both
/// provider unregister operations are idempotent for absent registrations.
pub(crate) fn unregister_cloned_provider_mounts(registrations: &mut Vec<ClonedProviderMount>) {
    for provider in registrations.drain(..).rev() {
        match provider {
            ClonedProviderMount::Fuse { clone_id, .. } => {
                crate::pseudofs::dev::fuse::unregister_mount_connection(clone_id);
            }
            ClonedProviderMount::Nfs { clone_id, .. } => {
                crate::syscall::fs::unregister_nfs_mount(clone_id);
            }
        }
    }
}

impl MountTopology {
    /// Materialize the bootstrap namespace from the only topology that exists
    /// before the first task is created.  This is deliberately a one-way
    /// bootstrap operation: all task-facing mutation goes through the
    /// namespace-local ledger thereafter.
    pub fn try_bootstrap(namespace_id: u64) -> AxResult<Arc<Self>> {
        let records = bootstrap_snapshot()?;
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(records.len())
            .map_err(|_| AxError::NoMemory)?;
        for record in records {
            mounts.push(Mount::try_from_record(&record, None)?);
        }
        Self::try_new(namespace_id, mounts)
    }

    pub fn try_new(namespace_id: u64, mounts: Vec<Mount>) -> AxResult<Arc<Self>> {
        if namespace_id == 0 {
            return Err(AxError::InvalidInput);
        }
        validate_topology_mounts(&mounts)?;
        let root_mount = mounts
            .iter()
            .find(|mount| mount.parent.is_none())
            .ok_or(AxError::InvalidInput)?
            .mountpoint()?;
        Arc::try_new(Self {
            namespace_id,
            root_mount: BlockingMutex::new(root_mount),
            state: BlockingMutex::new(MountTopologyState {
                generation: 1,
                next_peer_group: 1,
                mounts,
            }),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub const fn namespace_id(&self) -> u64 {
        self.namespace_id
    }

    pub fn try_snapshot(&self) -> AxResult<MountTopologySnapshot> {
        let state = self.state.lock();
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(state.mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        mounts.extend(state.mounts.iter().cloned());
        Ok(MountTopologySnapshot {
            namespace_id: self.namespace_id,
            generation: state.generation,
            mounts,
        })
    }

    /// Returns the immutable idmap selected for one live mount.  Credential
    /// consumers deliberately obtain this through their task's namespace
    /// topology rather than treating a VFS `Location` uid as globally scoped.
    pub fn idmap_for_mount(&self, mount_id: u64) -> AxResult<Option<Arc<MountIdmap>>> {
        let state = self.state.lock();
        state
            .mounts
            .iter()
            .find(|mount| mount.id == mount_id)
            .map(|mount| mount.idmap.clone())
            .ok_or(AxError::NotFound)
    }

    pub fn root_location(&self) -> AxResult<Location> {
        Ok(self.root_mount.lock().root_location())
    }

    /// CLONE_NEWNS copies the mount graph while sharing superblocks and
    /// immutable idmap objects.  Subsequent prepare/commit operations mutate
    /// only the returned namespace's ledger.
    pub(crate) fn try_prepare_clone_namespace(
        &self,
        namespace_id: u64,
        lock_mounts: bool,
    ) -> AxResult<PreparedMountTopologyClone> {
        // Materialize both the ledger and its per-mount authority from one
        // topology generation.  A separate `try_records()` lock acquisition
        // could otherwise pair old idmaps/peer state with a newer record
        // graph while a namespace mutation is in flight.
        let snapshot = self.try_snapshot()?;
        // A mount namespace must own a distinct VFS mount tree as well as a
        // distinct ledger.  Sharing Mountpoint objects would let an unmount
        // or move in one namespace mutate every clone's pathwalk graph.
        let mut source = Vec::new();
        source
            .try_reserve_exact(snapshot.mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        for mount in &snapshot.mounts {
            source.push(mount.try_record()?);
        }
        source.sort_by_key(|record| record.parent_id != 0);
        let root = source
            .iter()
            .find(|record| record.parent_id == 0)
            .ok_or(AxError::NotFound)?;
        let root_source = validate_record_state(root)?;
        let root_old = next_mountinfo_id()?;
        let root_clone = Mountpoint::new_root_at_with_extensions(
            &root_source.filesystem_handle(),
            root_source.root_location().entry().clone(),
            mount_extensions(
                root.flags,
                MountMetadata::try_from_parts(&root.source, &root.fs_type, &root.root, &root.data)?,
                root_old,
            )?,
        )?;
        register_live_superblock_mount(&root_clone)?;
        let mut cloned = Vec::new();
        let mut source_identity = HashMap::new();
        cloned
            .try_reserve_exact(source.len())
            .map_err(|_| AxError::NoMemory)?;
        source_identity
            .try_reserve(source.len())
            .map_err(|_| AxError::NoMemory)?;
        cloned.push(MountRecord {
            mount_id: root_clone.mount_id(),
            mount_id_old: root_old,
            parent_id: 0,
            root: try_path(&root.root)?,
            source: try_path(&root.source)?,
            target: try_path(FsPath::new(b"/"))?,
            fs_type: try_string(&root.fs_type)?,
            data: try_string(&root.data)?,
            dev: root.dev,
            flags: root.flags,
            expire_epoch: None,
            mountpoint: Arc::downgrade(&root_clone),
        });
        source_identity.insert(root_clone.mount_id(), root.mount_id);
        let context = axfs::FsContext::new(root_clone.root_location());
        let mut pending = source
            .into_iter()
            .filter(|record| record.parent_id != 0)
            .collect::<Vec<_>>();
        while !pending.is_empty() {
            let before = pending.len();
            let mut next = Vec::new();
            next.try_reserve_exact(before)
                .map_err(|_| AxError::NoMemory)?;
            for record in pending {
                let Some(parent) = cloned
                    .iter()
                    .find(|candidate| candidate.mount_id == record.parent_id)
                else {
                    next.push(record);
                    continue;
                };
                let original = validate_record_state(&record)?;
                let target = context.resolve(&record.target).map_err(|_| AxError::Io)?;
                if target.mountpoint().mount_id() != parent.mount_id {
                    return Err(AxError::Io);
                }
                let old = next_mountinfo_id()?;
                let clone = Mountpoint::new_detached_at_with_extensions(
                    &original.filesystem_handle(),
                    original.root_location().entry().clone(),
                    mount_extensions(
                        record.flags,
                        MountMetadata::try_from_parts(
                            &record.source,
                            &record.fs_type,
                            &record.root,
                            &record.data,
                        )?,
                        old,
                    )?,
                )?;
                clone.attach_to(&target)?;
                register_live_superblock_mount(&clone)?;
                cloned.push(MountRecord {
                    mount_id: clone.mount_id(),
                    mount_id_old: old,
                    parent_id: parent.mount_id,
                    root: try_path(&record.root)?,
                    source: try_path(&record.source)?,
                    target: try_path(&record.target)?,
                    fs_type: try_string(&record.fs_type)?,
                    data: try_string(&record.data)?,
                    dev: record.dev,
                    flags: record.flags,
                    expire_epoch: None,
                    mountpoint: Arc::downgrade(&clone),
                });
                source_identity.insert(clone.mount_id(), record.mount_id);
            }
            if next.len() == before {
                return Err(AxError::Io);
            }
            pending = next;
        }
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(cloned.len())
            .map_err(|_| AxError::NoMemory)?;
        for record in &cloned {
            let old = source_identity
                .get(&record.mount_id)
                .and_then(|id| snapshot.mounts.iter().find(|mount| mount.id == *id));
            let mut mount = Mount::try_from_record(record, old)?;
            // `lock_mnt_tree()` keeps the copied namespace root usable as
            // that namespace's root; it locks only descendants when ownership
            // crosses into a less-privileged user namespace.  The cloned
            // root is a fresh VFS mount and must not inherit a placement lock.
            mount.locked = if mount.parent.is_none() {
                false
            } else {
                mount.locked || lock_mounts
            };
            if mount.locked {
                mount.mountpoint()?.lock_placement();
            }
            mounts.push(mount);
        }
        let topology = Self::try_new(namespace_id, mounts)?;
        // Mountpoint cloning gives every namespace a new mount ID.  Prepare
        // the provider hand-off now, but do not register it yet: the caller
        // still has fallible namespace Arc and nsfs-registry admission ahead.
        let mut providers = Vec::new();
        providers
            .try_reserve_exact(cloned.len())
            .map_err(|_| AxError::NoMemory)?;
        for record in &cloned {
            let source_id = *source_identity.get(&record.mount_id).ok_or(AxError::Io)?;
            let provider = match record.fs_type.as_str() {
                "fuse" => ClonedProviderMount::Fuse {
                    source_id,
                    clone_id: record.mount_id,
                },
                "nfs4" => ClonedProviderMount::Nfs {
                    source_id,
                    clone_id: record.mount_id,
                },
                _ => continue,
            };
            providers.push(provider);
        }
        Ok(PreparedMountTopologyClone {
            topology,
            providers,
            providers_active: false,
        })
    }

    pub(crate) fn try_records(&self) -> AxResult<Vec<MountRecord>> {
        let state = self.state.lock();
        let mut records = Vec::new();
        records
            .try_reserve_exact(state.mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        for mount in &state.mounts {
            records.push(mount.try_record()?);
        }
        Ok(records)
    }

    pub(crate) fn prepare_replace_records(
        self: &Arc<Self>,
        records: &[MountRecord],
    ) -> AxResult<PreparedMountTopologyMutation> {
        self.prepare_replace_records_with_idmaps(records, &HashMap::new())
    }

    /// Prepare structural publication together with the immutable idmaps of
    /// newly attached mounts.  Supplying them at record materialization time
    /// avoids a window in which pathwalk can observe an attached mount before
    /// its credential view is present in the namespace ledger.
    pub(crate) fn prepare_replace_records_with_idmaps(
        self: &Arc<Self>,
        records: &[MountRecord],
        idmaps: &HashMap<u64, Arc<MountIdmap>>,
    ) -> AxResult<PreparedMountTopologyMutation> {
        self.prepare_replace_records_inner(records, idmaps, false)
    }

    /// Prepares a remount publication. Unlike structural record replacement,
    /// a remount intentionally adopts the record's mutable mount flags and
    /// option data while retaining immutable placement and propagation state.
    pub(crate) fn prepare_remount_records(
        self: &Arc<Self>,
        records: &[MountRecord],
    ) -> AxResult<PreparedMountTopologyMutation> {
        self.prepare_replace_records_inner(records, &HashMap::new(), true)
    }

    fn prepare_replace_records_inner(
        self: &Arc<Self>,
        records: &[MountRecord],
        idmaps: &HashMap<u64, Arc<MountIdmap>>,
        adopt_record_flags: bool,
    ) -> AxResult<PreparedMountTopologyMutation> {
        let state = self.state.lock();
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(records.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut installed_idmaps = 0usize;
        for record in records {
            let previous = state
                .mounts
                .iter()
                .find(|mount| mount.id == record.mount_id);
            let mut mount = Mount::try_from_record(record, previous)?;
            if adopt_record_flags && previous.is_some() {
                mount.flags = record.flags;
            }
            if let Some(idmap) = idmaps.get(&record.mount_id) {
                // This entry point only publishes idmaps owned by a detached
                // tree.  Existing attached mounts must retain their mapping.
                if previous.is_some() {
                    return Err(AxError::InvalidInput);
                }
                mount.idmap = Some(idmap.clone());
                installed_idmaps = installed_idmaps.checked_add(1).ok_or(AxError::NoMemory)?;
            }
            mounts.push(mount);
        }
        if installed_idmaps != idmaps.len() {
            return Err(AxError::NotFound);
        }
        validate_topology_mounts(&mounts)?;
        let next_root_mount = mounts
            .iter()
            .find(|mount| mount.parent.is_none())
            .ok_or(AxError::InvalidInput)?
            .mountpoint()?;
        let next = MountTopologyState {
            generation: state
                .generation
                .checked_add(1)
                .filter(|generation| *generation != 0)
                .ok_or(AxError::OutOfRange)?,
            next_peer_group: state.next_peer_group,
            mounts,
        };
        Ok(PreparedMountTopologyMutation {
            topology: self.clone(),
            expected_generation: state.generation,
            next_root_mount,
            next,
        })
    }

    /// Build a replacement ledger and install propagation identities for
    /// newly materialized replica mounts.  The identities are supplied by a
    /// single propagation transaction, so a mount copied into another
    /// namespace joins the same peer graph instead of becoming a private
    /// look-alike.
    pub(crate) fn prepare_replace_records_with_peers(
        self: &Arc<Self>,
        records: &[MountRecord],
        peers: &[(u64, PeerGroup)],
    ) -> AxResult<PreparedMountTopologyMutation> {
        self.prepare_replace_records_with_peers_and_idmaps(records, peers, &HashMap::new())
    }

    pub(crate) fn prepare_replace_records_with_peers_and_idmaps(
        self: &Arc<Self>,
        records: &[MountRecord],
        peers: &[(u64, PeerGroup)],
        idmaps: &HashMap<u64, Arc<MountIdmap>>,
    ) -> AxResult<PreparedMountTopologyMutation> {
        let mut prepared = self.prepare_replace_records_with_idmaps(records, idmaps)?;
        for (mount_id, peer) in peers {
            let mount = prepared
                .next
                .mounts
                .iter_mut()
                .find(|mount| mount.id == *mount_id)
                .ok_or(AxError::NotFound)?;
            mount.peer_group = Some(*peer);
            mount.unbindable = false;
        }
        validate_topology_mounts(&prepared.next.mounts)?;
        Ok(prepared)
    }

    pub(crate) fn prepare_replace_records_with_detached_propagation_and_idmaps(
        self: &Arc<Self>,
        records: &[MountRecord],
        propagation: &[DetachedMountPropagation],
        idmaps: &HashMap<u64, Arc<MountIdmap>>,
    ) -> AxResult<PreparedMountTopologyMutation> {
        let mut prepared = self.prepare_replace_records_with_idmaps(records, idmaps)?;
        for state in propagation {
            if state.peer_group.is_some() && state.unbindable {
                return Err(AxError::InvalidInput);
            }
            let mount = prepared
                .next
                .mounts
                .iter_mut()
                .find(|mount| mount.id == state.mount_id)
                .ok_or(AxError::NotFound)?;
            mount.peer_group = state.peer_group;
            mount.unbindable = state.unbindable;
        }
        validate_topology_mounts(&prepared.next.mounts)?;
        Ok(prepared)
    }

    /// Imports structural VFS changes made by the legacy mountpoint layer
    /// into one namespace ledger.  Attributes and idmaps already selected in
    /// this namespace win over the bootstrap record flags.  Callers invoke
    /// this only after the VFS operation has committed.
    pub fn reconcile_vfs_records(&self) -> AxResult<()> {
        // Structural publication goes through the same prepare/commit
        // transaction as the VFS change.  Reconciliation therefore verifies
        // the namespace-owned VFS tree rather than importing a process-global
        // record list (which would leak another namespace's mounts).
        let records = self.try_records()?;
        let index = MountRecordIndex::new(&records)?;
        let root = records
            .iter()
            .find(|record| record.parent_id == 0)
            .ok_or(AxError::Io)?;
        let root_mount = validate_record_state(root)?;
        let ids = validate_registered_subtree(&index, &root_mount)?;
        if ids.len() != records.len() {
            return Err(AxError::Io);
        }
        Ok(())
    }

    pub fn prepare_setattr(
        self: &Arc<Self>,
        root_mount_id: u64,
        recursive: bool,
        request: MountSetattrRequest,
    ) -> AxResult<PreparedMountTopologyMutation> {
        let state = self.state.lock();
        let mut next = state.clone();
        let selected = selected_mount_indices(&next.mounts, root_mount_id, recursive)?;
        if selected.is_empty() {
            return Err(AxError::NotFound);
        }

        // First calculate all state, including peer IDs, in the private copy.
        // No live record changes until commit can atomically replace it.
        for index in &selected {
            let mount = &mut next.mounts[*index];
            mount.flags = thekernel_linux_mount::apply_mount_attr_flags(
                mount.flags,
                request.attr_set,
                request.attr_clr,
                0,
                0,
            )
            .map_err(map_topology_uapi_error)?;
            if let Some(idmap) = &request.idmap {
                if mount.attached || (mount.idmap.is_some() && !request.idmap_replace) {
                    return Err(AxError::InvalidInput);
                }
                mount.idmap = idmap.clone();
            }
        }
        apply_propagation_change(&mut next, &selected, request.propagation)?;
        validate_topology_mounts(&next.mounts)?;
        next.generation = next
            .generation
            .checked_add(1)
            .filter(|generation| *generation != 0)
            .ok_or(AxError::OutOfRange)?;
        Ok(PreparedMountTopologyMutation {
            topology: self.clone(),
            expected_generation: state.generation,
            next_root_mount: self.root_mount.lock().clone(),
            next,
        })
    }

    /// Implements move_mount(MOVE_MOUNT_SET_GROUP).  This is deliberately a
    /// topology-only transaction: Linux uses this flag to join propagation
    /// groups, rather than to relocate either mount tree.
    pub fn prepare_join_propagation_group(
        self: &Arc<Self>,
        from_mount_id: u64,
        to_mount_id: u64,
    ) -> AxResult<PreparedMountTopologyMutation> {
        let state = self.state.lock();
        let from = state
            .mounts
            .iter()
            .find(|mount| mount.id == from_mount_id)
            .ok_or(AxError::NotFound)?;
        let to = state
            .mounts
            .iter()
            .find(|mount| mount.id == to_mount_id)
            .ok_or(AxError::NotFound)?;

        // do_set_group() only joins roots of the same superblock, with the
        // source root covering the destination root.  `root` is the
        // namespace record's filesystem-relative root, so component-boundary
        // comparison preserves `/a` versus `/ab` correctly.
        if from.superblock.identity != to.superblock.identity
            || from.superblock.fs_type != to.superblock.fs_type
            || !path_contains(&from.root, &to.root)
            || to.peer_group.is_some()
            || to.unbindable
        {
            return Err(AxError::InvalidInput);
        }
        let group = from.peer_group.ok_or(AxError::InvalidInput)?;

        let mut next = state.clone();
        let target = next
            .mounts
            .iter_mut()
            .find(|mount| mount.id == to_mount_id)
            .ok_or(AxError::NotFound)?;
        target.peer_group = Some(group);
        target.unbindable = false;
        validate_topology_mounts(&next.mounts)?;
        next.generation = next
            .generation
            .checked_add(1)
            .filter(|generation| *generation != 0)
            .ok_or(AxError::OutOfRange)?;
        Ok(PreparedMountTopologyMutation {
            topology: self.clone(),
            expected_generation: state.generation,
            next_root_mount: self.root_mount.lock().clone(),
            next,
        })
    }
}

fn path_contains(ancestor: &FsPath, descendant: &FsPath) -> bool {
    let ancestor = ancestor.as_bytes();
    let descendant = descendant.as_bytes();
    ancestor == descendant
        || ancestor == b"/"
        || descendant
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.first() == Some(&b'/'))
}

impl PreparedMountTopologyMutation {
    fn namespace_id(&self) -> u64 {
        self.topology.namespace_id
    }

    /// Validate while the caller owns the namespace-operation gate.  A
    /// cross-namespace propagation receipt validates every participant before
    /// it changes any VFS tree, then publishes these already-built ledgers in
    /// stable namespace-id order.
    fn validate_epoch(&self) -> AxResult<()> {
        (self.topology.state.lock().generation == self.expected_generation)
            .then_some(())
            .ok_or(AxError::WouldBlock)
    }

    /// The preceding batch epoch validation plus `namespace_operation()` make
    /// this replacement non-fallible: no other mount mutation can interleave.
    fn commit_validated(self) {
        let mut state = self.topology.state.lock();
        debug_assert_eq!(state.generation, self.expected_generation);
        *state = self.next;
        *self.topology.root_mount.lock() = self.next_root_mount;
    }

    pub fn commit(self) -> AxResult<()> {
        self.validate_epoch()?;
        self.commit_validated();
        Ok(())
    }
}

/// Publish independently prepared namespace ledgers after their caller has
/// validated every generation in global namespace-id order.  The enclosing
/// operation gate prevents interleaving writers, making this an infallible
/// no-allocation commit phase for attach/bind/move and unmount propagation.
fn commit_topology_batch_validated(prepared: &mut Vec<PreparedMountTopologyMutation>) {
    debug_assert!(
        prepared
            .windows(2)
            .all(|pair| pair[0].namespace_id() <= pair[1].namespace_id())
    );
    for mutation in core::mem::take(prepared) {
        mutation.commit_validated();
    }
}

fn map_topology_uapi_error(error: thekernel_linux_mount::UapiError) -> AxError {
    match error {
        thekernel_linux_mount::UapiError::Invalid => AxError::InvalidInput,
        thekernel_linux_mount::UapiError::Unsupported => AxError::OperationNotSupported,
        thekernel_linux_mount::UapiError::TooBig => axerrno::LinuxError::E2BIG.into(),
        thekernel_linux_mount::UapiError::NotFound => AxError::NotFound,
    }
}

fn validate_topology_mounts(mounts: &[Mount]) -> AxResult<()> {
    let roots = mounts.iter().filter(|mount| mount.parent.is_none()).count();
    if roots != 1 || mounts.iter().any(|mount| mount.id == 0) {
        return Err(AxError::InvalidInput);
    }
    for (index, mount) in mounts.iter().enumerate() {
        if mounts[..index]
            .iter()
            .any(|candidate| candidate.id == mount.id)
        {
            return Err(AxError::InvalidInput);
        }
        if mount.parent == Some(mount.id)
            || mount
                .parent
                .is_some_and(|parent| !mounts.iter().any(|candidate| candidate.id == parent))
        {
            return Err(AxError::InvalidInput);
        }
        let mut cursor = mount.parent;
        for _ in 0..mounts.len() {
            let Some(parent) = cursor else { break };
            let parent = mounts
                .iter()
                .find(|candidate| candidate.id == parent)
                .ok_or(AxError::InvalidInput)?;
            if parent.id == mount.id {
                return Err(AxError::InvalidInput);
            }
            cursor = parent.parent;
        }
        if cursor.is_some() {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(())
}

fn selected_mount_indices(mounts: &[Mount], root: u64, recursive: bool) -> AxResult<Vec<usize>> {
    let root_index = mounts
        .iter()
        .position(|mount| mount.id == root)
        .ok_or(AxError::NotFound)?;
    let mut selected = Vec::new();
    selected
        .try_reserve(mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    selected.push(root_index);
    if recursive {
        let mut cursor = 0;
        while cursor < selected.len() {
            let parent = mounts[selected[cursor]].id;
            for (index, mount) in mounts.iter().enumerate() {
                if mount.parent == Some(parent) {
                    selected.push(index);
                }
            }
            cursor += 1;
        }
    }
    Ok(selected)
}

fn apply_propagation_change(
    state: &mut MountTopologyState,
    selected: &[usize],
    propagation: u64,
) -> AxResult<()> {
    use thekernel_linux_mount::{MS_PRIVATE, MS_SHARED, MS_SLAVE, MS_UNBINDABLE};

    if propagation == 0 {
        return Ok(());
    }
    if !matches!(
        propagation as u32,
        MS_PRIVATE | MS_SHARED | MS_SLAVE | MS_UNBINDABLE
    ) {
        return Err(AxError::InvalidInput);
    }
    for index in selected {
        let parent_peer = state.mounts[*index]
            .parent
            .and_then(|parent| state.mounts.iter().find(|mount| mount.id == parent))
            .and_then(|parent| parent.peer_group.map(|peer| peer.id));
        match propagation as u32 {
            MS_PRIVATE => {
                let mount = &mut state.mounts[*index];
                mount.peer_group = None;
                mount.unbindable = false;
            }
            MS_UNBINDABLE => {
                let mount = &mut state.mounts[*index];
                mount.peer_group = None;
                mount.unbindable = true;
            }
            MS_SHARED => {
                let peer_group = if state.mounts[*index].peer_group.is_none() {
                    // Peer identities cross namespace boundaries during
                    // attach/move/unmount propagation.  A per-topology
                    // counter made independently-created groups collide at
                    // `1`, causing unrelated namespaces to receive each
                    // other's events.  Allocate from the same global domain
                    // used for propagated child mounts instead.
                    let id = next_propagation_peer_id()?;
                    Some(PeerGroup { id, master: None })
                } else {
                    None
                };
                let mount = &mut state.mounts[*index];
                if let Some(peer_group) = peer_group {
                    mount.peer_group = Some(peer_group);
                } else if let Some(peer_group) = mount.peer_group.as_mut() {
                    // A slave's peer group identifies its fellow slaves, not
                    // its master.  Making this mount shared detaches that
                    // relationship while retaining its locally allocated ID.
                    peer_group.master = None;
                }
                mount.unbindable = false;
            }
            MS_SLAVE => {
                let master = parent_peer.ok_or(AxError::InvalidInput)?;
                // Do not repurpose a shared group as a slave group: its
                // other peers must remain shared.  Every conversion gets a
                // fresh group; recursive conversion subsequently creates a
                // graph of explicit slave groups rather than aliases.
                let id = next_propagation_peer_id()?;
                let mount = &mut state.mounts[*index];
                mount.peer_group = Some(PeerGroup {
                    id,
                    master: Some(master),
                });
                mount.unbindable = false;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

struct LinuxMountState {
    mount_id_old: u32,
    legacy_mount_id: i32,
    flags: AtomicU32,
    remount_epoch: AtomicU64,
    activity_epoch: AtomicU64,
    readonly_floor: bool,
    metadata: Mutex<MountMetadata>,
}

struct RemountVisibilityGuard<'a> {
    state: &'a LinuxMountState,
    stable_epoch: u64,
    committed: bool,
}

impl<'a> RemountVisibilityGuard<'a> {
    fn begin(state: &'a LinuxMountState) -> AxResult<Self> {
        loop {
            let epoch = state.remount_epoch.load(Ordering::Acquire);
            if epoch & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            if epoch > u64::MAX - 3 {
                return Err(AxError::OutOfRange);
            }
            if state
                .remount_epoch
                .compare_exchange(
                    epoch,
                    epoch.wrapping_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(Self {
                    state,
                    stable_epoch: epoch,
                    committed: false,
                });
            }
        }
    }

    fn commit(mut self) {
        self.state
            .remount_epoch
            .store(self.stable_epoch + 2, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for RemountVisibilityGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.state
                .remount_epoch
                .store(self.stable_epoch, Ordering::Release);
        }
    }
}

fn stable_mount_flags(state: &LinuxMountState) -> u32 {
    loop {
        let epoch = state.remount_epoch.load(Ordering::Acquire);
        if epoch & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let flags = state.flags.load(Ordering::Acquire);
        if state.remount_epoch.load(Ordering::Acquire) == epoch {
            return flags;
        }
    }
}

fn stable_mount_snapshot(state: &LinuxMountState) -> AxResult<(u32, MountMetadata)> {
    loop {
        let epoch = state.remount_epoch.load(Ordering::Acquire);
        if epoch & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let flags = state.flags.load(Ordering::Acquire);
        let metadata = state.metadata.lock().try_clone()?;
        if state.remount_epoch.load(Ordering::Acquire) == epoch {
            return Ok((flags, metadata));
        }
    }
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

fn plan_mount_mutation(
    records: &[MountRecord],
    operation: thekernel_linux_mount::MountOperation,
) -> AxResult<thekernel_linux_mount::MountPlan> {
    use thekernel_linux_mount::{
        MountAuthority, MountFlags, MountId, NamespaceGeneration, NamespaceId, TopologyEntry,
        TopologySnapshot,
    };

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in records {
        entries.push(TopologyEntry {
            mount: MountId::new(record.mount_id).map_err(|_| AxError::Io)?,
            parent: (record.parent_id != 0)
                .then(|| MountId::new(record.parent_id).map_err(|_| AxError::Io))
                .transpose()?,
            flags: MountFlags::from_validated_kernel_bits(record.flags.into()),
            // VFS performs the authoritative busy check immediately before
            // unmount; the pure plan only records topology admission.
            detachable: true,
        });
    }
    // The pure ABI planner still validates the requested graph, but current
    // generation/namespace authority belongs to MountTopology's private
    // transaction state, not a process-global counter.
    let generation = NamespaceGeneration::from_raw(1).map_err(|_| AxError::Io)?;
    let snapshot = TopologySnapshot {
        namespace: NamespaceId::new(1).map_err(|_| AxError::Io)?,
        generation,
        entries: &entries,
    };
    thekernel_linux_mount::plan_mount(
        snapshot,
        MountAuthority {
            administer: true,
            pivot_root: true,
            lazy_unmount: true,
        },
        operation,
    )
    .map_err(|_| AxError::Io)
}

fn commit_mount_mutation(_plan: thekernel_linux_mount::MountPlan) -> AxResult<()> {
    // The following `publish_current_records` / PreparedMountTopologyMutation
    // is the sole compare-and-publish point.  Keeping an independent global
    // generation here would make one namespace's change spuriously conflict
    // with another's.
    Ok(())
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
    pub fn new(source: FsPathBuf, fs_type: String, root: FsPathBuf, data: String) -> Self {
        Self {
            source,
            fs_type,
            root,
            data,
            block_members: Vec::new(),
        }
    }

    pub fn with_block_members(mut self, block_members: Vec<DeviceId>) -> Self {
        self.block_members = block_members;
        self
    }

    pub fn try_from_parts(
        source: &FsPath,
        fs_type: &str,
        root: &FsPath,
        data: &str,
    ) -> AxResult<Self> {
        Ok(Self {
            source: try_path(source)?,
            fs_type: try_string(fs_type)?,
            root: try_path(root)?,
            data: try_string(data)?,
            block_members: Vec::new(),
        })
    }

    fn try_clone(&self) -> AxResult<Self> {
        let mut clone = Self::try_from_parts(&self.source, &self.fs_type, &self.root, &self.data)?;
        clone
            .block_members
            .try_reserve_exact(self.block_members.len())
            .map_err(|_| AxError::NoMemory)?;
        clone.block_members.extend_from_slice(&self.block_members);
        Ok(clone)
    }
}

impl MountRecord {
    fn try_clone(&self) -> AxResult<Self> {
        Ok(Self {
            mount_id: self.mount_id,
            mount_id_old: self.mount_id_old,
            parent_id: self.parent_id,
            root: try_path(&self.root)?,
            source: try_path(&self.source)?,
            target: try_path(&self.target)?,
            fs_type: try_string(&self.fs_type)?,
            data: try_string(&self.data)?,
            dev: self.dev,
            flags: self.flags,
            expire_epoch: self.expire_epoch,
            mountpoint: self.mountpoint.clone(),
        })
    }
}

fn next_mountinfo_id() -> AxResult<u32> {
    let id = MOUNTINFO_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    u32::try_from(id).map_err(|_| AxError::OutOfRange)
}

fn next_propagation_peer_id() -> AxResult<u64> {
    let id = PROPAGATION_PEER_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    (id != 0).then_some(id).ok_or(AxError::OutOfRange)
}

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

fn try_path(value: &FsPath) -> AxResult<FsPathBuf> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(value.as_bytes().len())
        .map_err(|_| AxError::NoMemory)?;
    bytes.extend_from_slice(value.as_bytes());
    Ok(FsPathBuf::from_vec(bytes))
}

/// The active thread's mount namespace is the sole current-namespace source
/// for both record reads and writes.  In particular it is not necessarily the
/// process-default namespace after `unshare`/`setns`.
fn current_mount_topology() -> Option<Arc<MountTopology>> {
    let task = axtask::current_may_uninit()?;
    let thread = task.try_as_thread()?;
    Some(thread.mount_ns().topology())
}

pub fn snapshot() -> AxResult<Vec<MountRecord>> {
    if let Some(topology) = current_mount_topology() {
        return topology.try_records();
    }
    bootstrap_snapshot()
}

fn bootstrap_snapshot() -> AxResult<Vec<MountRecord>> {
    loop {
        let (generation, snapshot) = {
            let records = BOOTSTRAP_MOUNT_RECORDS.lock();
            let generation = BOOTSTRAP_MOUNT_RECORDS_GENERATION.load(Ordering::Acquire);
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve(records.len())
                .map_err(|_| AxError::NoMemory)?;
            for record in records.iter() {
                snapshot.push(record.try_clone()?);
            }
            (generation, snapshot)
        };
        let validation = snapshot
            .iter()
            .try_for_each(|record| validate_record_state(record).map(|_| ()));
        if BOOTSTRAP_MOUNT_RECORDS_GENERATION.load(Ordering::Acquire) != generation {
            continue;
        }
        validation?;
        return Ok(snapshot);
    }
}

fn publish_bootstrap_records(records: Vec<MountRecord>) {
    *BOOTSTRAP_MOUNT_RECORDS.lock() = records;
    BOOTSTRAP_MOUNT_RECORDS_GENERATION.fetch_add(1, Ordering::Release);
}

/// Publish a namespace-local record transaction.  The bootstrap record list
/// exists only before a task owns a MountNamespace; once scheduled, the
/// current namespace topology is the sole mount ledger.
fn publish_current_records(records: &[MountRecord]) -> AxResult<()> {
    if let Some(topology) = current_mount_topology() {
        return topology.prepare_replace_records(records)?.commit();
    }
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in records {
        replacement.push(record.try_clone()?);
    }
    publish_bootstrap_records(replacement);
    Ok(())
}

enum PreparedRemountRecordPublication {
    Topology(PreparedMountTopologyMutation),
    Bootstrap(Vec<MountRecord>),
}

impl PreparedRemountRecordPublication {
    fn commit(self) -> AxResult<()> {
        match self {
            Self::Topology(publication) => publication.commit(),
            Self::Bootstrap(records) => {
                publish_bootstrap_records(records);
                Ok(())
            }
        }
    }
}

fn prepare_current_remount_record_publication(
    records: &[MountRecord],
) -> AxResult<PreparedRemountRecordPublication> {
    if let Some(topology) = current_mount_topology() {
        return topology
            .prepare_remount_records(records)
            .map(PreparedRemountRecordPublication::Topology);
    }
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in records {
        replacement.push(record.try_clone()?);
    }
    Ok(PreparedRemountRecordPublication::Bootstrap(replacement))
}

fn prepare_current_record_publication(
    records: &[MountRecord],
) -> AxResult<Option<PreparedMountTopologyMutation>> {
    prepare_current_record_publication_with_idmaps(records, &HashMap::new())
}

fn prepare_current_record_publication_with_idmaps(
    records: &[MountRecord],
    idmaps: &HashMap<u64, Arc<MountIdmap>>,
) -> AxResult<Option<PreparedMountTopologyMutation>> {
    if let Some(topology) = current_mount_topology() {
        return topology
            .prepare_replace_records_with_idmaps(records, idmaps)
            .map(Some);
    }
    if !idmaps.is_empty() {
        return Err(AxError::InvalidInput);
    }
    Ok(None)
}

fn prepare_current_record_publication_with_detached_propagation_and_idmaps(
    records: &[MountRecord],
    idmaps: &HashMap<u64, Arc<MountIdmap>>,
    propagation: &[DetachedMountPropagation],
) -> AxResult<Option<PreparedMountTopologyMutation>> {
    if let Some(topology) = current_mount_topology() {
        return topology
            .prepare_replace_records_with_detached_propagation_and_idmaps(
                records,
                propagation,
                idmaps,
            )
            .map(Some);
    }
    if !idmaps.is_empty() || !propagation.is_empty() {
        return Err(AxError::InvalidInput);
    }
    Ok(None)
}

/// Read the superblock magic for an attached mount record.  The record's weak
/// mountpoint is deliberately revalidated here so statmount never reports a
/// detached or stale ledger entry.
pub fn statmount_sb_magic(record: &MountRecord) -> AxResult<u64> {
    let mountpoint = validate_record_state(record)?;
    Ok(mountpoint.root_location().filesystem().stat()?.fs_type as u64)
}

pub fn statx_mount_id(mount_id: u64) -> Option<u32> {
    if let Some(id) = snapshot()
        .ok()?
        .iter()
        .find_map(|record| (record.mount_id == mount_id).then_some(record.mount_id_old))
    {
        return Some(id);
    }
    LIVE_SUPERBLOCK_MOUNTS
        .lock()
        .get(&mount_id)?
        .upgrade()?
        .extension::<LinuxMountState>()
        .map(|state| state.mount_id_old)
}

/// Returns the root location of the first live mount with this Linux device
/// number. The returned mountpoint keeps the mount alive after the records
/// lock is released, so callers may inspect its filesystem without holding
/// namespace state locked.
pub fn mounted_root_location(device: DeviceId) -> AxResult<Location> {
    let mountpoint = snapshot()?
        .iter()
        .find_map(|record| {
            (record.dev == device.0)
                .then(|| record.mountpoint.upgrade())
                .flatten()
        })
        .or_else(|| {
            // Linux ustat resolves a live superblock by device number even
            // when no attached mount in this namespace names it.
            LIVE_SUPERBLOCK_MOUNTS.lock().values().find_map(|mount| {
                let mount = mount.upgrade()?;
                (linux_device_id(mount.device()) == device).then_some(mount)
            })
        })
        .ok_or(AxError::InvalidInput)?;
    Ok(mountpoint.root_location())
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

fn mount_extensions(flags: u32, metadata: MountMetadata, mount_id_old: u32) -> VfsResult<TypeMap> {
    let legacy_mount_id = NEXT_LEGACY_MOUNT_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            (id < i32::MAX as u32).then_some(id + 1)
        })
        .map_err(|_| AxError::StorageFull)? as i32;
    let mut extensions = TypeMap::new();
    let retired = extensions.try_insert(LinuxMountState {
        mount_id_old,
        legacy_mount_id,
        flags: AtomicU32::new(flags),
        remount_epoch: AtomicU64::new(0),
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
            source: FsPathBuf::new(),
            fs_type: String::new(),
            root: FsPathBuf::new(),
            data: String::new(),
            block_members: Vec::new(),
        },
        1,
    )?)
}

pub fn initialize_root_mount(
    mountpoint: &Arc<Mountpoint>,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<()> {
    let dev = linux_device_id(mountpoint.device()).0;
    let record_metadata = metadata.try_clone()?;
    let target = try_path(FsPath::new(b"/"))?;
    let mount_id_old = next_mountinfo_id()?;
    let extensions = mount_extensions(flags, metadata, mount_id_old)?;
    let mut records = snapshot()?;
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
        mount_id_old,
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
    register_live_superblock_mount(mountpoint)?;
    publish_current_records(&records)?;
    Ok(())
}

pub fn mount_with_flags(
    target: &Location,
    filesystem: &Filesystem,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<Arc<Mountpoint>> {
    target.mount_with_extensions(
        filesystem,
        mount_extensions(flags, metadata, next_mountinfo_id()?)?,
    )
}

pub fn new_detached_with_flags(
    filesystem: &Filesystem,
    flags: u32,
    metadata: MountMetadata,
) -> VfsResult<Arc<Mountpoint>> {
    let mountpoint = Mountpoint::new_detached_with_extensions(
        filesystem,
        mount_extensions(flags, metadata, next_mountinfo_id()?)?,
    )?;
    register_live_superblock_mount(&mountpoint)?;
    Ok(mountpoint)
}

fn mount_state(mountpoint: &Mountpoint) -> AxResult<Arc<LinuxMountState>> {
    mountpoint
        .extension_shared::<LinuxMountState>()
        .ok_or(AxError::Io)
}

/// Legacy, per-mount-namespace ID used by the `name_to_handle_at` ABI.
/// This is intentionally not the mountpoint's stable 64-bit identity.
pub fn legacy_mount_id(mountpoint: &Mountpoint) -> AxResult<i32> {
    Ok(mount_state(mountpoint)?.legacy_mount_id)
}

fn flags_for_mountpoint(mountpoint: &Mountpoint) -> Option<u32> {
    mountpoint
        .extension::<LinuxMountState>()
        .map(stable_mount_flags)
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
    stable_mount_snapshot(&state).map(|(_, metadata)| metadata)
}

fn joined_mount_root(base: &FsPath, path_in_mount: &FsPath) -> AxResult<FsPathBuf> {
    if !base.is_absolute() || !path_in_mount.is_absolute() {
        return Err(AxError::Io);
    }
    if path_in_mount.as_bytes() == b"/" {
        return try_path(base);
    }
    if base.as_bytes() == b"/" {
        return try_path(path_in_mount);
    }
    let mut joined = Vec::new();
    joined
        .try_reserve(
            base.as_bytes()
                .len()
                .saturating_add(path_in_mount.as_bytes().len()),
        )
        .map_err(|_| AxError::NoMemory)?;
    let base = base.as_bytes();
    let end = base
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(1, |index| index + 1);
    joined.extend_from_slice(&base[..end]);
    joined.extend_from_slice(path_in_mount.as_bytes());
    Ok(FsPathBuf::from_vec(joined))
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
        let next = update(stable_mount_flags(&state))?;
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
    attach_tree_and_record_kind(root, target, AttachKind::Attach, &HashMap::new(), &[])?;
    if let Err(error) = propagate_attached_tree(root, target) {
        rollback_attached_tree_record(root)?;
        return Err(error);
    }
    Ok(())
}

/// Publish a detached tree and every retained per-mount idmap through one
/// topology transaction.  The caller keeps ownership of `idmaps`; a failed
/// attach therefore leaves the detached FD unchanged and retryable.
pub fn attach_tree_and_record_with_idmaps(
    root: &Arc<Mountpoint>,
    target: &Location,
    idmaps: &HashMap<u64, Arc<MountIdmap>>,
) -> VfsResult<()> {
    attach_tree_and_record_kind(root, target, AttachKind::Attach, idmaps, &[])?;
    if let Err(error) = propagate_attached_tree(root, target) {
        rollback_attached_tree_record(root)?;
        return Err(error);
    }
    Ok(())
}

/// Attached publication for a detached `open_tree` clone.  Its idmaps and
/// propagation graph are both prepared before the VFS attachment, avoiding
/// a visible tree whose ledger still describes private/default mounts.
pub fn attach_tree_and_record_with_idmaps_and_propagation(
    root: &Arc<Mountpoint>,
    target: &Location,
    idmaps: &HashMap<u64, Arc<MountIdmap>>,
    propagation: &[DetachedMountPropagation],
) -> VfsResult<()> {
    attach_tree_and_record_kind(root, target, AttachKind::Attach, idmaps, propagation)?;
    if let Err(error) = propagate_attached_tree(root, target) {
        rollback_attached_tree_record(root)?;
        return Err(error);
    }
    Ok(())
}

pub fn bind_tree_and_record_from(
    root: &Arc<Mountpoint>,
    target: &Location,
    source_mount_id: u64,
) -> VfsResult<()> {
    attach_tree_and_record_kind(
        root,
        target,
        AttachKind::Bind { source_mount_id },
        &HashMap::new(),
        &[],
    )?;
    if let Err(error) = propagate_attached_tree(root, target) {
        rollback_attached_tree_record(root)?;
        return Err(error);
    }
    Ok(())
}

/// Reverts the initiating attachment when preparing or publishing a peer
/// propagation transaction fails.  All replacement records are prepared
/// before the physical detach, so callers never receive an error while the
/// source namespace retains a newly-visible partial attach.
fn rollback_attached_tree_record(root: &Arc<Mountpoint>) -> AxResult<()> {
    let records = snapshot()?;
    let ids = validate_registered_subtree(&MountRecordIndex::new(&records)?, root)?;
    let mut next = Vec::new();
    next.try_reserve(records.len().saturating_sub(ids.len()))
        .map_err(|_| AxError::NoMemory)?;
    for record in records
        .iter()
        .filter(|record| !ids.contains(&record.mount_id))
    {
        next.push(record.try_clone()?);
    }
    let prepared = prepare_current_record_publication(&next)?;
    root.root_location().lazy_unmount()?;
    if let Some(prepared) = prepared {
        prepared.commit()?;
    } else {
        publish_current_records(&next)?;
    }
    Ok(())
}

/// Replicate a newly attached tree beneath every peer of its parent mount.
///
/// The VFS tree for a mount namespace is deliberately distinct, therefore a
/// propagation event cannot reuse the source `Mountpoint`: it has to create a
/// new mount instance backed by the same filesystem and publish a matching
/// namespace-local record transaction.  All target ledgers are prepared
/// before a replica is attached; failures detach every already-attached
/// replica and leave no published remote topology behind.
fn propagate_attached_tree(root: &Arc<Mountpoint>, target: &Location) -> VfsResult<()> {
    // Bootstrap mounts exist before a task has a MountNamespace. There is no
    // peer namespace to replicate into at that stage.
    let Some(source) = current_mount_topology() else {
        return Ok(());
    };
    let source_snapshot = source.try_snapshot()?;
    let parent_id = target.mountpoint().mount_id();
    let parent_peer = source_snapshot
        .mounts
        .iter()
        .find(|mount| mount.id == parent_id)
        .and_then(|mount| mount.peer_group);
    let Some(parent_peer) = parent_peer else {
        return Ok(());
    };

    // A child below a shared parent forms a fresh shared peer group.  A copy
    // below each slave parent is a distinct slave group whose master is that
    // *child* group, not the parent group's master.  Reusing the latter made
    // future propagation of the child attach to an unrelated parent event.
    let peer = PeerGroup {
        id: next_propagation_peer_id()?,
        master: None,
    };
    let mut destinations: Vec<(
        Arc<MountTopology>,
        MountTopologySnapshot,
        u64,
        Location,
        PeerGroup,
    )> = Vec::new();
    let mut slave_children: Vec<(u64, PeerGroup)> = Vec::new();
    for namespace in crate::task::MountNamespace::live()? {
        let topology = namespace.topology();
        let snapshot = topology.try_snapshot()?;
        for mount in snapshot.mounts.iter().cloned() {
            // A peer receives directly; a slave receives from its master but
            // must not feed the event back upstream.
            if !mount.peer_group.is_some_and(|group| {
                group.id == parent_peer.id || group.master == Some(parent_peer.id)
            }) {
                continue;
            }
            // The source attachment is already visible at this exact parent.
            if topology.namespace_id() == source.namespace_id() && mount.id == parent_id {
                continue;
            }
            let parent = mount.mountpoint()?;
            let target_in_parent = target.path_in_mount().map_err(|_| AxError::Io)?;
            let destination = axfs::FsContext::new(parent.root_location())
                .resolve(target_in_parent.as_ref())
                .map_err(|_| AxError::Io)?;
            let child_peer = if mount
                .peer_group
                .is_some_and(|group| group.id == parent_peer.id)
            {
                peer
            } else {
                let parent_group = mount.peer_group.ok_or(AxError::Io)?;
                if let Some((_, child)) =
                    slave_children.iter().find(|(id, _)| *id == parent_group.id)
                {
                    *child
                } else {
                    slave_children
                        .try_reserve(1)
                        .map_err(|_| AxError::NoMemory)?;
                    let child = PeerGroup {
                        id: next_propagation_peer_id()?,
                        master: Some(peer.id),
                    };
                    slave_children.push((parent_group.id, child));
                    child
                }
            };
            destinations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            destinations.push((
                topology.clone(),
                snapshot.clone(),
                mount.id,
                destination,
                child_peer,
            ));
        }
    }

    struct Replica {
        root: Arc<Mountpoint>,
        destination: Location,
        fuse_mount_ids: Vec<u64>,
        nfs_mount_ids: Vec<u64>,
        records: Vec<MountRecord>,
        idmaps: HashMap<u64, Arc<MountIdmap>>,
        child_peer: PeerGroup,
    }
    struct ReplicaGroup {
        topology: Arc<MountTopology>,
        snapshot: MountTopologySnapshot,
        replicas: Vec<Replica>,
    }
    /// Registrations are installed while a cloned tree is still detached.
    /// Keep their rollback receipt alive through every later allocation,
    /// ledger preparation, epoch validation and physical attach.  Only the
    /// final global publication may disarm it.
    struct PropagationCloneRegistrations {
        fuse: Vec<u64>,
        nfs: Vec<u64>,
        active: bool,
    }
    impl Drop for PropagationCloneRegistrations {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            for mount_id in self.fuse.drain(..) {
                crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
            }
            for mount_id in self.nfs.drain(..) {
                crate::syscall::fs::unregister_nfs_mount(mount_id);
            }
        }
    }
    let mut groups: Vec<ReplicaGroup> = Vec::new();
    groups
        .try_reserve(destinations.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut registrations = PropagationCloneRegistrations {
        fuse: Vec::new(),
        nfs: Vec::new(),
        active: true,
    };
    for (topology, snapshot, parent, destination, child_peer) in destinations {
        let (replica_root, records, nfs_mount_ids, idmaps) =
            clone_tree_for_propagation(root, &destination, parent)?;
        let mut fuse_mount_ids = Vec::new();
        fuse_mount_ids
            .try_reserve(records.len())
            .map_err(|_| AxError::NoMemory)?;
        for record in &records {
            if record.fs_type == "fuse" {
                fuse_mount_ids.push(record.mount_id);
            }
        }
        if registrations
            .fuse
            .try_reserve(fuse_mount_ids.len())
            .is_err()
            || registrations.nfs.try_reserve(nfs_mount_ids.len()).is_err()
        {
            for mount_id in fuse_mount_ids {
                crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
            }
            for mount_id in nfs_mount_ids {
                crate::syscall::fs::unregister_nfs_mount(mount_id);
            }
            return Err(AxError::NoMemory);
        }
        registrations.fuse.extend(fuse_mount_ids.iter().copied());
        registrations.nfs.extend(nfs_mount_ids.iter().copied());
        let replica = Replica {
            root: replica_root,
            destination,
            fuse_mount_ids,
            nfs_mount_ids,
            records,
            idmaps,
            child_peer,
        };
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.topology.namespace_id() == topology.namespace_id())
        {
            group
                .replicas
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            group.replicas.push(replica);
        } else {
            let mut replicas = Vec::new();
            replicas.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            replicas.push(replica);
            groups.push(ReplicaGroup {
                topology,
                snapshot,
                replicas,
            });
        }
    }

    // Commit every namespace ledger through the same ordered receipt used by
    // propagated unmount.  Validate generations before the first replica is
    // attached, so rollback remains purely VFS/registration cleanup.
    let mut publications = Vec::new();
    publications
        .try_reserve(groups.len().saturating_add(1))
        .map_err(|_| AxError::NoMemory)?;
    let mut source_published = false;
    for group in &groups {
        let mut existing = group.topology.try_records()?;
        let total = group
            .replicas
            .iter()
            .try_fold(0usize, |n, replica| n.checked_add(replica.records.len()))
            .ok_or(AxError::NoMemory)?;
        existing.try_reserve(total).map_err(|_| AxError::NoMemory)?;
        let mut peers = Vec::new();
        peers
            .try_reserve(group.replicas.len())
            .map_err(|_| AxError::NoMemory)?;
        let idmap_count = group
            .replicas
            .iter()
            .try_fold(0usize, |count, replica| {
                count.checked_add(replica.idmaps.len())
            })
            .ok_or(AxError::NoMemory)?;
        let mut idmaps = HashMap::new();
        idmaps
            .try_reserve(idmap_count)
            .map_err(|_| AxError::NoMemory)?;
        for replica in &group.replicas {
            for record in &replica.records {
                existing.push(record.try_clone()?);
            }
            for (mount_id, idmap) in &replica.idmaps {
                if idmaps.insert(*mount_id, idmap.clone()).is_some() {
                    return Err(AxError::Io);
                }
            }
            peers.push((replica.root.mount_id(), replica.child_peer));
        }
        if group.topology.namespace_id() == source.namespace_id() {
            peers.push((root.mount_id(), peer));
            source_published = true;
        }
        // Retain the exact one snapshot per namespace until all allocations
        // finish; the prepared mutation is based on that one generation.
        let _ = &group.snapshot;
        publications.push(
            group
                .topology
                .prepare_replace_records_with_peers_and_idmaps(&existing, &peers, &idmaps)?,
        );
    }
    if !source_published {
        // Mark the source instance shared even if it has no other current
        // counterpart; a later namespace clone inherits this graph identity.
        let source_records = source.try_records()?;
        publications.push(
            source
                .prepare_replace_records_with_peers(&source_records, &[(root.mount_id(), peer)])?,
        );
    }
    publications.sort_by_key(PreparedMountTopologyMutation::namespace_id);
    for publication in &publications {
        publication.validate_epoch()?;
    }

    for group in &groups {
        for replica in &group.replicas {
            if let Err(error) = replica.root.attach_to(&replica.destination) {
                for group in &groups {
                    for replica in &group.replicas {
                        let _ = replica.root.root_location().lazy_unmount();
                    }
                }
                return Err(error);
            }
        }
    }
    commit_topology_batch_validated(&mut publications);
    registrations.active = false;
    Ok(())
}

fn attach_tree_and_record_kind(
    root: &Arc<Mountpoint>,
    target: &Location,
    kind: AttachKind,
    idmaps: &HashMap<u64, Arc<MountIdmap>>,
    propagation: &[DetachedMountPropagation],
) -> VfsResult<()> {
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
        let (flags, metadata) = stable_mount_snapshot(&state)?;
        let (parent_id, mount_target) = if Arc::ptr_eq(&mountpoint, root) {
            (
                target.mountpoint().mount_id(),
                try_path(target_path.as_ref())?,
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
            mount_id_old: state.mount_id_old,
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

    let mut records = snapshot()?;
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
    let operation = match kind {
        AttachKind::Attach => thekernel_linux_mount::MountOperation::Attach {
            mount: thekernel_linux_mount::MountId::new(root.mount_id()).map_err(|_| AxError::Io)?,
            parent: thekernel_linux_mount::MountId::new(target.mountpoint().mount_id())
                .map_err(|_| AxError::Io)?,
        },
        AttachKind::Bind { source_mount_id } => thekernel_linux_mount::MountOperation::Bind {
            source: thekernel_linux_mount::MountId::new(source_mount_id)
                .map_err(|_| AxError::Io)?,
            parent: thekernel_linux_mount::MountId::new(target.mountpoint().mount_id())
                .map_err(|_| AxError::Io)?,
        },
    };
    let plan = plan_mount_mutation(&records, operation)?;
    // Register every child before publication. Recursive bind children may
    // refer to different superblocks and remain live through detached-tree or
    // lazy-unmount references after their namespace records disappear.
    register_live_superblock_tree(&committed)?;
    records.extend(committed);
    // Allocate and validate the exact topology state, including idmaps,
    // before the VFS mount can become visible to pathwalk.
    let publication = prepare_current_record_publication_with_detached_propagation_and_idmaps(
        &records,
        idmaps,
        propagation,
    )?;
    root.attach_to(target)?;
    let publication_result = commit_mount_mutation(plan).and_then(|_| {
        if let Some(publication) = publication {
            publication.commit()
        } else {
            publish_current_records(&records)
        }
    });
    if let Err(error) = publication_result {
        // `attach_to` is the only irreversible-looking step here; the new
        // tree is still private to this operation, so detach it before
        // exposing an error to userspace.
        let _ = root.root_location().lazy_unmount();
        return Err(error);
    }
    Ok(())
}

/// Clone an already-attached mount tree for a propagation target without
/// publishing it.  The returned root is detached; callers own the final
/// attach/rollback boundary.  Child mountpoints are rebuilt against their
/// cloned parent mount so no VFS object is shared between namespaces.
fn clone_tree_for_propagation(
    root: &Arc<Mountpoint>,
    destination: &Location,
    destination_parent: u64,
) -> AxResult<(
    Arc<Mountpoint>,
    Vec<MountRecord>,
    Vec<u64>,
    HashMap<u64, Arc<MountIdmap>>,
)> {
    let source_records = snapshot()?;
    let source_topology = axtask::current().as_thread().mount_ns().topology();
    let index = MountRecordIndex::new(&source_records)?;
    let ids = validate_registered_subtree(&index, root)?;
    let root_record = index.record(root.mount_id())?;
    let destination_path = destination.absolute_path().map_err(|_| AxError::Io)?;
    let mountpoints = root.subtree_mountpoints()?;
    let mut clones = HashMap::new();
    clones
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut records = Vec::new();
    records
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut fuse_connections = Vec::new();
    fuse_connections
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut nfs_sources = Vec::new();
    nfs_sources
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut idmaps = HashMap::new();
    idmaps
        .try_reserve(mountpoints.len())
        .map_err(|_| AxError::NoMemory)?;

    for source in mountpoints {
        let source_id = source.mount_id();
        if !ids.contains(&source_id) {
            return Err(AxError::Io);
        }
        let record = index.record(source_id)?;
        let fuse_connection = if record.fs_type == "fuse" {
            Some(
                crate::pseudofs::dev::fuse::mount_connection(source_id)
                    .ok_or(AxError::NoSuchDevice)?,
            )
        } else {
            None
        };
        let metadata = MountMetadata::try_from_parts(
            &record.source,
            &record.fs_type,
            &record.root,
            &record.data,
        )?;
        let old = next_mountinfo_id()?;
        // A bind/subtree mount's root is an inode below the backing
        // filesystem root.  Preserve that exact dentry while cloning the
        // recursive tree; using `Filesystem::root_dir()` here widens the
        // clone and makes the ledger's `root` disagree with VFS pathwalk.
        let clone = Mountpoint::new_detached_at_with_extensions(
            &source.filesystem_handle(),
            source.root_location().entry().clone(),
            mount_extensions(record.flags, metadata, old)?,
        )?;
        let (parent_id, target) = if source_id == root.mount_id() {
            (destination_parent, try_path(destination_path.as_ref())?)
        } else {
            let attachment = source.location().ok_or(AxError::Io)?;
            let source_parent = attachment.mountpoint().mount_id();
            let parent: &Arc<Mountpoint> = clones.get(&source_parent).ok_or(AxError::Io)?;
            let clone_target = Location::new(parent.clone(), attachment.entry().clone());
            clone.attach_to(&clone_target)?;
            let suffix = path_suffix(&root_record.target, &record.target).ok_or(AxError::Io)?;
            (
                parent.mount_id(),
                joined_path(destination_path.as_ref(), suffix)?,
            )
        };
        clones.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        clones.insert(source_id, clone.clone());
        if let Some(idmap) = source_topology.idmap_for_mount(source_id)? {
            idmaps.insert(clone.mount_id(), idmap);
        }
        if let Some(connection) = fuse_connection {
            fuse_connections.push((clone.mount_id(), connection));
        }
        if record.fs_type == "nfs4" {
            nfs_sources.push((source_id, clone.mount_id()));
        }
        records.push(MountRecord {
            mount_id: clone.mount_id(),
            mount_id_old: old,
            parent_id,
            root: try_path(&record.root)?,
            source: try_path(&record.source)?,
            target,
            fs_type: try_string(&record.fs_type)?,
            data: try_string(&record.data)?,
            dev: record.dev,
            flags: record.flags,
            expire_epoch: None,
            mountpoint: Arc::downgrade(&clone),
        });
    }
    register_live_superblock_tree(&records)?;
    let mut registered = Vec::new();
    registered
        .try_reserve(fuse_connections.len())
        .map_err(|_| AxError::NoMemory)?;
    for (mount_id, connection) in fuse_connections {
        if let Err(error) =
            crate::pseudofs::dev::fuse::register_mount_connection(mount_id, &connection)
        {
            for mount_id in registered {
                crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
            }
            return Err(error);
        }
        registered.push(mount_id);
    }
    let mut registered_nfs = Vec::new();
    registered_nfs
        .try_reserve(nfs_sources.len())
        .map_err(|_| AxError::NoMemory)?;
    for (source_id, clone_id) in nfs_sources {
        if let Err(error) = crate::syscall::fs::clone_nfs_mount_registration(source_id, clone_id) {
            for mount_id in registered_nfs {
                crate::syscall::fs::unregister_nfs_mount(mount_id);
            }
            for mount_id in registered {
                crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
            }
            return Err(error);
        }
        registered_nfs.push(clone_id);
    }
    let root = clones.remove(&root.mount_id()).ok_or(AxError::Io)?;
    Ok((root, records, registered_nfs, idmaps))
}

fn validate_record_state(record: &MountRecord) -> AxResult<Arc<Mountpoint>> {
    let mountpoint = record.mountpoint.upgrade().ok_or(AxError::Io)?;
    if mountpoint.mount_id() != record.mount_id {
        return Err(AxError::Io);
    }
    let state = mount_state(&mountpoint)?;
    let (flags, metadata) = stable_mount_snapshot(&state)?;
    if flags != record.flags {
        return Err(AxError::Io);
    }
    if metadata.source != record.source
        || metadata.fs_type != record.fs_type
        || metadata.root != record.root
        || metadata.data != record.data
    {
        return Err(AxError::Io);
    }
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
        if record.parent_id != 0 || record.target.as_bytes() != b"/" {
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
    if actual_target.as_ref() != record.target.as_ref() {
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
    let records = snapshot()?;
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
                .and_then(try_path)?;
            selected.push((
                depth,
                BindSubmount {
                    source: root_location,
                    relative_path,
                    metadata: MountMetadata::try_from_parts(
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
    source: FsPathBuf,
    fs_type: String,
    flags: u32,
    data: String,
) -> AxResult<()> {
    let mut records = snapshot()?;
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&record_index, target.mountpoint())?;
    let index = *record_index
        .by_id
        .get(&target.mountpoint().mount_id())
        .ok_or(AxError::Io)?;
    let mountpoint = validate_record_state(&records[index])?;
    let state = mount_state(&mountpoint)?;
    if (!source.is_empty() && source != records[index].source)
        || (!fs_type.is_empty() && fs_type != records[index].fs_type)
    {
        return Err(AxError::InvalidInput);
    }
    if state.readonly_floor && flags & MS_RDONLY == 0 {
        return Err(AxError::OperationNotSupported);
    }
    let mut remount_metadata = state.metadata.lock().try_clone()?;
    remount_metadata.data = try_string(&data)?;
    let plan = plan_mount_mutation(
        &records,
        thekernel_linux_mount::MountOperation::Remount {
            mount: thekernel_linux_mount::MountId::new(mountpoint.mount_id())
                .map_err(|_| AxError::Io)?,
            flags: thekernel_linux_mount::MountFlags::from_validated_kernel_bits(flags.into()),
        },
    )?;
    let record = &mut records[index];
    record.flags = flags;
    // Provider option parsers receive their configuration at mount creation;
    // the namespace ledger still owns the live remount option view consumed
    // by statmount/fspick and by subsequent reconfigure transactions.
    record.data = data;
    record.expire_epoch = None;
    let publication = prepare_current_remount_record_publication(&records)?;
    // The VFS mutation plan, namespace record publication, and replacement
    // extension metadata are all prepared before the topology commit. The
    // extension stores below are infallible, so no observer can see a partial
    // flags/data transition after publication succeeds.
    let visibility = RemountVisibilityGuard::begin(&state)?;
    commit_mount_mutation(plan)?;
    publication.commit()?;
    *state.metadata.lock() = remount_metadata;
    state.flags.store(flags, Ordering::Release);
    visibility.commit();
    Ok(())
}

pub fn try_update_flags_for_mounts(
    root_mount_id: u64,
    recursive: bool,
    mut update: impl FnMut(u32) -> AxResult<u32>,
) -> AxResult<bool> {
    let mut records = snapshot()?;
    let Some(updates) = prepare_mount_flag_updates(
        &records,
        root_mount_id,
        recursive,
        |record| {
            let mountpoint = validate_record_state(record)?;
            let state = mount_state(&mountpoint)?;
            let current = stable_mount_flags(&state);
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
    let root_flags = updates
        .iter()
        .find(|(index, ..)| records[*index].mount_id == root_mount_id)
        .map(|(_, _, flags)| *flags)
        .ok_or(AxError::Io)?;
    let plan = plan_mount_mutation(
        &records,
        thekernel_linux_mount::MountOperation::Setattr {
            mount: thekernel_linux_mount::MountId::new(root_mount_id).map_err(|_| AxError::Io)?,
            flags: thekernel_linux_mount::MountFlags::from_validated_kernel_bits(root_flags.into()),
        },
    )?;
    for (index, state, flags) in updates {
        state.flags.store(flags, Ordering::Release);
        records[index].flags = flags;
        records[index].expire_epoch = None;
    }
    commit_mount_mutation(plan)?;
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
    if !old.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    let old_target = old.absolute_path().map_err(|_| AxError::Io)?;
    let new_target = target.absolute_path().map_err(|_| AxError::Io)?;
    let new_parent_id = target.mountpoint().mount_id();

    let mut records = snapshot()?;
    let record_index = MountRecordIndex::new(&records)?;
    validate_registered_mount_chain(&record_index, target.mountpoint())?;
    let subtree = validate_registered_subtree(&record_index, &root)?;
    let topology = axtask::current()
        .as_thread()
        .mount_ns()
        .topology()
        .try_snapshot()?;
    let source_mount = topology
        .mounts
        .iter()
        .find(|mount| mount.id == root_mount_id)
        .ok_or(AxError::InvalidInput)?;
    let parent_id = source_mount.parent.ok_or(AxError::InvalidInput)?;
    let source_parent = topology
        .mounts
        .iter()
        .find(|mount| mount.id == parent_id)
        .ok_or(AxError::InvalidInput)?;
    if source_parent
        .peer_group
        .is_some_and(|group| group.master.is_none())
    {
        return Err(AxError::InvalidInput);
    }
    let target_shared = topology
        .mounts
        .iter()
        .find(|mount| mount.id == new_parent_id)
        .ok_or(AxError::InvalidInput)?
        .peer_group
        .is_some_and(|group| group.master.is_none());
    if target_shared
        && topology
            .mounts
            .iter()
            .any(|mount| subtree.contains(&mount.id) && mount.unbindable)
    {
        return Err(AxError::InvalidInput);
    }
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
    let plan = plan_mount_mutation(
        &records,
        thekernel_linux_mount::MountOperation::Move {
            mount: thekernel_linux_mount::MountId::new(root_mount_id).map_err(|_| AxError::Io)?,
            parent: thekernel_linux_mount::MountId::new(new_parent_id).map_err(|_| AxError::Io)?,
        },
    )?;

    let mut original_records = Vec::new();
    original_records
        .try_reserve_exact(records.len())
        .map_err(|_| AxError::NoMemory)?;
    for record in &records {
        original_records.push(record.try_clone()?);
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
    if let Err(error) = commit_mount_mutation(plan).and_then(|_| publish_current_records(&records))
    {
        // Both locations remain pinned by this syscall. Restore the physical
        // tree before returning if namespace-ledger publication lost its race.
        let _ = target.move_mount_to(old);
        return Err(error);
    }
    if let Err(error) = propagate_moved_tree(root_mount_id, target) {
        let _ = target.move_mount_to(old);
        let _ = publish_current_records(&original_records);
        return Err(error);
    }
    Ok(())
}

/// Mirror a move across the peer graph.  Each replica is moved below the
/// corresponding peer of the new parent, and its complete descendant record
/// subtree is prepared before the VFS move is made visible.
fn propagate_moved_tree(root_id: u64, target: &Location) -> AxResult<()> {
    let current = axtask::current().as_thread().mount_ns();
    let source = current.topology().try_snapshot()?;
    let Some(root_peer) = source
        .mounts
        .iter()
        .find(|mount| mount.id == root_id)
        .and_then(|mount| mount.peer_group)
    else {
        return Ok(());
    };
    let Some(parent_peer) = source
        .mounts
        .iter()
        .find(|mount| mount.id == target.mountpoint().mount_id())
        .and_then(|mount| mount.peer_group)
    else {
        return Ok(());
    };
    let target_path = target.path_in_mount().map_err(|_| AxError::Io)?;

    struct MoveEdge {
        old: Location,
        new: Location,
    }
    struct MoveReplica {
        edges: Vec<MoveEdge>,
        prepared: Option<PreparedMountTopologyMutation>,
    }
    let mut replicas = Vec::new();
    for namespace in crate::task::MountNamespace::live()? {
        let topology = namespace.topology();
        let mounts = topology.try_snapshot()?.mounts;
        let mut root_ids = Vec::new();
        let mut parent_ids = Vec::new();
        root_ids
            .try_reserve(mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        parent_ids
            .try_reserve(mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        for mount in &mounts {
            if mount
                .peer_group
                .is_some_and(|peer| peer.id == root_peer.id || peer.master == Some(root_peer.id))
                && !(namespace.id() == current.id() && mount.id == root_id)
            {
                root_ids.push(mount.id);
            }
            if mount.peer_group.is_some_and(|peer| {
                peer.id == parent_peer.id || peer.master == Some(parent_peer.id)
            }) {
                parent_ids.push(mount.id);
            }
        }
        // A peer graph may contain a nested selected mount. Moving its
        // ancestor already carries the complete VFS/ledger subtree, so keep
        // only topmost counterparts for this namespace receipt.
        let mut top_roots = Vec::new();
        top_roots
            .try_reserve(root_ids.len())
            .map_err(|_| AxError::NoMemory)?;
        for mount_id in root_ids.iter().copied() {
            let mut parent = mounts
                .iter()
                .find(|mount| mount.id == mount_id)
                .and_then(|mount| mount.parent);
            while let Some(parent_id) = parent {
                if root_ids.iter().any(|candidate| *candidate == parent_id) {
                    break;
                }
                parent = mounts
                    .iter()
                    .find(|mount| mount.id == parent_id)
                    .and_then(|mount| mount.parent);
            }
            if parent.is_none() {
                top_roots.push(mount_id);
            }
        }
        root_ids = top_roots;
        let mut records = topology.try_records()?;
        let mut edges = Vec::new();
        edges
            .try_reserve(root_ids.len())
            .map_err(|_| AxError::NoMemory)?;
        for root_id in root_ids {
            let root_mount = mounts
                .iter()
                .find(|mount| mount.id == root_id)
                .ok_or(AxError::Io)?;
            let root = root_mount.mountpoint()?;
            let old = root.root_location();
            // A namespace can contain more than one member of either peer
            // group.  Enumerate each root counterpart and choose its unique
            // usable destination; never let `find()` silently leave later
            // counterparts behind.
            let mut chosen = None;
            for parent_id in &parent_ids {
                let parent_mount = mounts
                    .iter()
                    .find(|mount| mount.id == *parent_id)
                    .ok_or(AxError::Io)?;
                let candidate = axfs::FsContext::new(parent_mount.mountpoint()?.root_location())
                    .resolve(target_path.as_ref())
                    .map_err(|_| AxError::Io)?;
                if Arc::ptr_eq(candidate.mountpoint(), &root)
                    || candidate.is_same_or_ancestor_of(&old)
                {
                    continue;
                }
                if chosen.is_some() {
                    return Err(AxError::Io);
                }
                chosen = Some((parent_mount.id, candidate));
            }
            let Some((new_parent_id, new)) = chosen else {
                return Err(AxError::NotFound);
            };
            let ids = subtree_mount_ids(&records, root.mount_id())?;
            let new_target = new.absolute_path().map_err(|_| AxError::Io)?;
            for record in &mut records {
                if !ids.contains(&record.mount_id) {
                    continue;
                }
                let suffix = path_suffix(&root_mount.target, &record.target).ok_or(AxError::Io)?;
                record.target = joined_path(new_target.as_ref(), suffix)?;
                record.expire_epoch = None;
                if record.mount_id == root.mount_id() {
                    record.parent_id = new_parent_id;
                }
            }
            edges.push(MoveEdge { old, new });
        }
        if !edges.is_empty() {
            let prepared = topology.prepare_replace_records(&records)?;
            replicas.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            replicas.push(MoveReplica {
                edges,
                prepared: Some(prepared),
            });
        }
    }
    let mut publications = Vec::new();
    publications
        .try_reserve(replicas.len())
        .map_err(|_| AxError::NoMemory)?;
    for replica in &mut replicas {
        publications.push(replica.prepared.take().ok_or(AxError::Io)?);
    }
    publications.sort_by_key(PreparedMountTopologyMutation::namespace_id);
    for publication in &publications {
        publication.validate_epoch()?;
    }
    let mut moved: Vec<(Location, Location)> = Vec::new();
    let edge_count = replicas
        .iter()
        .try_fold(0usize, |count, replica| {
            count.checked_add(replica.edges.len())
        })
        .ok_or(AxError::NoMemory)?;
    moved
        .try_reserve(edge_count)
        .map_err(|_| AxError::NoMemory)?;
    for replica in &replicas {
        for edge in &replica.edges {
            if let Err(error) = edge.old.move_mount_to(&edge.new) {
                for (old, new) in moved {
                    let _ = new.move_mount_to(&old);
                }
                return Err(error);
            }
            moved.push((edge.old.clone(), edge.new.clone()));
        }
    }
    commit_topology_batch_validated(&mut publications);
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
        .filter(|path| !path.as_bytes().is_empty())
        .ok_or(AxError::InvalidInput)
        .and_then(try_path)?;

    let mut records = snapshot()?;
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
            let target = if suffix.as_bytes().is_empty() {
                try_path(FsPath::new(b"/"))?
            } else {
                try_path(suffix)?
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
    let plan = plan_mount_mutation(
        &records,
        thekernel_linux_mount::MountOperation::PivotRoot {
            new_root: thekernel_linux_mount::MountId::new(new_mount.mount_id())
                .map_err(|_| AxError::Io)?,
            put_old: thekernel_linux_mount::MountId::new(namespace_root.mount_id())
                .map_err(|_| AxError::Io)?,
        },
    )?;

    new_root.pivot_root_to(put_old)?;
    for (record_index, target, parent_id) in updates {
        let record = &mut records[record_index];
        record.target = target;
        if let Some(parent_id) = parent_id {
            record.parent_id = parent_id;
        }
        record.expire_epoch = None;
    }
    if let Err(error) = commit_mount_mutation(plan).and_then(|_| publish_current_records(&records))
    {
        // pivot_root_to has a paired inverse while both prepared locations
        // are pinned: restore the old root under the new root's put_old slot.
        let _ = put_old.pivot_root_to(new_root);
        return Err(error);
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

        if let Some(suffix) = path_suffix(FsPath::new(old_target.as_bytes()), &record.target) {
            record.target = joined_path(FsPath::new(new_target.as_bytes()), suffix).unwrap();
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
    if lazy && expire {
        return Err(AxError::InvalidInput);
    }

    // MNT_EXPIRE's first probe is the sole intentionally visible preparatory
    // state.  A successful terminal probe immediately becomes part of the
    // same receipt as every propagated peer.
    if expire {
        let mut records = snapshot()?;
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
            publish_current_records(&records)?;
            return Err(AxError::WouldBlock);
        }
    }

    PreparedUnmountPropagation::prepare(target, lazy)?.commit();
    Ok(())
}

/// A complete unmount/propagation transaction.  It strongly owns every
/// namespace, location, provider-flush lease and connection-registry receipt
/// selected during prepare.  Thus neither a namespace teardown nor a
/// detach/reattach ABA can turn the later publication into a partial event.
struct PreparedUnmountPropagation {
    lazy: bool,
    participants: Vec<PreparedUnmountParticipant>,
    publications: Vec<PreparedMountTopologyMutation>,
    fuse: crate::pseudofs::dev::fuse::PreparedFuseMountTeardown,
    nfs: Option<crate::syscall::fs::PreparedNfsMountTeardown>,
}

struct PreparedUnmountParticipant {
    namespace: Arc<crate::task::MountNamespace>,
    targets: Vec<PreparedUnmountTarget>,
}

struct PreparedUnmountTarget {
    target: Option<Location>,
    flushed: Option<axfs_ng_vfs::FlushedUnmount>,
}

impl PreparedUnmountPropagation {
    fn prepare(source_target: Location, lazy: bool) -> AxResult<Self> {
        let current = axtask::current().as_thread().mount_ns();
        let source_id = source_target.mountpoint().mount_id();
        let source_snapshot = current.topology().try_snapshot()?;
        let peer = source_snapshot
            .mounts
            .iter()
            .find(|mount| mount.id == source_id)
            .and_then(|mount| mount.peer_group);

        let mut namespaces = crate::task::MountNamespace::live()?;
        namespaces.sort_by_key(|namespace| namespace.id());
        let mut participants = Vec::new();
        participants
            .try_reserve(namespaces.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut publications = Vec::new();
        publications
            .try_reserve(namespaces.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut fuse_ids = Vec::new();
        let mut nfs_ids = Vec::new();

        let mut source_target = Some(source_target);
        for namespace in namespaces {
            let topology = namespace.topology();
            let snapshot = topology.try_snapshot()?;
            let mut selected = Vec::new();
            selected
                .try_reserve(snapshot.mounts.len())
                .map_err(|_| AxError::NoMemory)?;
            for mount in &snapshot.mounts {
                if (namespace.id() == current.id() && mount.id == source_id)
                    || peer.is_some_and(|peer| {
                        mount.id != source_id
                            && mount.peer_group.is_some_and(|candidate| {
                                candidate.id == peer.id || candidate.master == Some(peer.id)
                            })
                    })
                {
                    selected.push(mount.id);
                }
            }
            // An unmount of an ancestor consumes every descendant.  Do not
            // prepare the same VFS subtree twice merely because a nested
            // mount also belongs to the propagation peer graph.
            let mut roots = Vec::new();
            roots
                .try_reserve(selected.len())
                .map_err(|_| AxError::NoMemory)?;
            for mount_id in selected.iter().copied() {
                let mut parent = snapshot
                    .mounts
                    .iter()
                    .find(|mount| mount.id == mount_id)
                    .and_then(|mount| mount.parent);
                while let Some(parent_id) = parent {
                    if selected.iter().any(|candidate| *candidate == parent_id) {
                        break;
                    }
                    parent = snapshot
                        .mounts
                        .iter()
                        .find(|mount| mount.id == parent_id)
                        .and_then(|mount| mount.parent);
                }
                if parent.is_none() {
                    roots.push(mount_id);
                }
            }
            // Every root selected in this namespace is folded into one
            // prospective ledger.  Preparing a replacement per root from
            // the same snapshot makes the second receipt stale as soon as
            // the first commits and can also resurrect a sibling removal.
            let records = topology.try_records()?;
            let record_index = MountRecordIndex::new(&records)?;
            let mut removed = HashSet::new();
            removed
                .try_reserve(records.len())
                .map_err(|_| AxError::NoMemory)?;
            let mut targets = Vec::new();
            targets
                .try_reserve(roots.len())
                .map_err(|_| AxError::NoMemory)?;
            for mount_id in roots {
                let mount = snapshot
                    .mounts
                    .iter()
                    .find(|mount| mount.id == mount_id)
                    .ok_or(AxError::Io)?;
                let mountpoint = mount.mountpoint()?;
                let target = if namespace.id() == current.id() && mount.id == source_id {
                    source_target.take().ok_or(AxError::Io)?
                } else {
                    mountpoint.root_location()
                };
                let ids = validate_registered_subtree(&record_index, &mountpoint)?;
                // Preserve the ABI planner's structural admission for every
                // affected namespace before any VFS participant is marked
                // unmounting.
                let _plan = plan_mount_mutation(
                    &records,
                    thekernel_linux_mount::MountOperation::Unmount {
                        mount: thekernel_linux_mount::MountId::new(mount.id)
                            .map_err(|_| AxError::Io)?,
                        lazy,
                    },
                )?;
                removed.extend(ids);
                targets.push(PreparedUnmountTarget {
                    target: Some(target),
                    flushed: None,
                });
            }
            if targets.is_empty() {
                continue;
            }
            let mut next = Vec::new();
            next.try_reserve(records.len().saturating_sub(removed.len()))
                .map_err(|_| AxError::NoMemory)?;
            for record in records.iter() {
                if removed.contains(&record.mount_id) {
                    match record.fs_type.as_str() {
                        "fuse" => fuse_ids.push(record.mount_id),
                        "nfs4" => nfs_ids.push(record.mount_id),
                        _ => {}
                    }
                } else {
                    next.push(record.try_clone()?);
                }
            }
            let prepared = topology.prepare_replace_records(&next)?;
            participants.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            publications.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            participants.push(PreparedUnmountParticipant {
                namespace: namespace.clone(),
                targets,
            });
            publications.push(prepared);
        }
        if participants.is_empty() {
            return Err(AxError::NotFound);
        }
        // The source must be part of every terminal receipt, even if a stale
        // peer graph caused a replica scan to omit it.
        if !participants.iter().any(|participant| {
            participant.namespace.id() == current.id()
                && participant.targets.iter().any(|target| {
                    target
                        .target
                        .as_ref()
                        .is_some_and(|target| target.mountpoint().mount_id() == source_id)
                })
        }) {
            return Err(AxError::Io);
        }
        // All VFS checks and provider flushes occur while nothing is detached.
        // If one fails, Drop releases the already-marked participants and all
        // namespace ledgers/connection registries remain unchanged.
        if !lazy {
            for participant in &mut participants {
                for target in &mut participant.targets {
                    let location = target.target.take().ok_or(AxError::Io)?;
                    location.check_unmountable()?;
                    // Move the sole prepare Location into the receipt. A
                    // cloned Location would itself make normal unmount busy.
                    target.flushed = Some(location.prepare_unmount()?.flush()?);
                }
            }
        }
        // One FUSE receipt sees the complete removal set, which is necessary
        // to identify the final replica and queue exactly one DESTROY.
        let fuse = crate::pseudofs::dev::fuse::prepare_mount_teardown(fuse_ids)?;
        let nfs = (!nfs_ids.is_empty())
            .then(|| crate::syscall::fs::prepare_nfs_mount_teardown(nfs_ids))
            .transpose()?;
        publications.sort_by_key(PreparedMountTopologyMutation::namespace_id);
        Ok(Self {
            lazy,
            participants,
            publications,
            fuse,
            nfs,
        })
    }

    fn commit(mut self) {
        // Validate every expected generation in globally ordered namespace
        // order before the first physical detach.  The enclosing operation
        // gate makes the following no-allocation swaps non-fallible.
        for publication in &self.publications {
            publication
                .validate_epoch()
                .expect("prepared unmount namespace epoch");
        }
        for participant in &mut self.participants {
            for target in &mut participant.targets {
                if self.lazy {
                    target
                        .target
                        .take()
                        .expect("prepared lazy unmount participant")
                        .lazy_unmount()
                        .expect("validated lazy unmount participant");
                } else {
                    // `prepare_unmount` sealed every participant's Location
                    // admission, so commit cannot fail after batch epoch
                    // validation.
                    target
                        .flushed
                        .take()
                        .expect("prepared unmount participant")
                        .commit()
                        .expect("validated unmount participant");
                }
            }
        }
        // Epochs were checked before physical detach.  Reusing the batch
        // publisher retains the same ordered, no-allocation swap discipline.
        commit_topology_batch_validated(&mut self.publications);
        if self.lazy {
            self.fuse.commit_deferred();
        } else {
            self.fuse.commit();
        }
        if let Some(nfs) = self.nfs.take() {
            nfs.commit();
        }
    }
}

/// Validate every prospective peer removal before the source namespace makes
/// an unmount visible.  The namespace-operation gate held by callers keeps
/// the selected topology stable until the later commit phase.
#[allow(dead_code)]
fn preflight_propagated_unmount(source_root: u64, peer: PeerGroup, lazy: bool) -> AxResult<()> {
    let current = axtask::current().as_thread().mount_ns();
    for namespace in crate::task::MountNamespace::live()? {
        if namespace.id() == current.id() {
            continue;
        }
        let topology = namespace.topology();
        let snapshot = topology.try_snapshot()?;
        let Some(mount) = snapshot.mounts.iter().find(|mount| {
            mount.id != source_root
                && mount
                    .peer_group
                    .is_some_and(|group| group.id == peer.id || group.master == Some(peer.id))
        }) else {
            continue;
        };
        let target = mount.mountpoint()?.root_location();
        let records = topology.try_records()?;
        let ids = subtree_mount_ids(&records, mount.id)?;
        let mut next = Vec::new();
        next.try_reserve(records.len().saturating_sub(ids.len()))
            .map_err(|_| AxError::NoMemory)?;
        for record in records
            .iter()
            .filter(|record| !ids.contains(&record.mount_id))
        {
            next.push(record.try_clone()?);
        }
        let _prepared = topology.prepare_replace_records(&next)?;
        if !lazy {
            target.check_unmountable()?;
            // Provider flush preparation is itself the failure point; dropping
            // this preflight receipt is side-effect free.
            drop(target.prepare_unmount()?.flush()?);
        }
    }
    Ok(())
}

/// Remove every peer replica after the initiating namespace has committed its
/// unmount.  Record replacement is prepared before touching a replica, and
/// non-lazy propagation retains the provider flush/busy checks used by the
/// initiating unmount path.
#[allow(dead_code)]
fn propagate_unmounted_tree(source_root: u64, peer: PeerGroup, lazy: bool) -> AxResult<()> {
    let current = axtask::current().as_thread().mount_ns();
    struct UnmountReplica {
        target: Location,
        prepared: PreparedMountTopologyMutation,
        flushed: Option<axfs_ng_vfs::FlushedUnmount>,
        fuse_mount_ids: Vec<u64>,
    }
    let mut replicas = Vec::new();
    for namespace in crate::task::MountNamespace::live()? {
        if namespace.id() == current.id() {
            continue;
        }
        let topology = namespace.topology();
        let snapshot = topology.try_snapshot()?;
        let Some(mount) = snapshot.mounts.iter().find(|mount| {
            mount.id != source_root
                && mount
                    .peer_group
                    .is_some_and(|group| group.id == peer.id || group.master == Some(peer.id))
        }) else {
            continue;
        };
        let target = mount.mountpoint()?.root_location();
        let records = topology.try_records()?;
        let ids = subtree_mount_ids(&records, mount.id)?;
        let mut fuse_mount_ids = Vec::new();
        fuse_mount_ids
            .try_reserve(ids.len())
            .map_err(|_| AxError::NoMemory)?;
        fuse_mount_ids.extend(ids.iter().copied());
        let mut next = Vec::new();
        next.try_reserve(records.len().saturating_sub(ids.len()))
            .map_err(|_| AxError::NoMemory)?;
        for record in records
            .iter()
            .filter(|record| !ids.contains(&record.mount_id))
        {
            next.push(record.try_clone()?);
        }
        let prepared = topology.prepare_replace_records(&next)?;
        replicas.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        replicas.push(UnmountReplica {
            target,
            prepared,
            flushed: None,
            fuse_mount_ids,
        });
    }
    if !lazy {
        // Prepare and flush every replica before committing any VFS detach.
        // A preparation failure then drops all prior receipts and leaves every
        // peer mount, including its FUSE registry entry, untouched.
        for replica in &mut replicas {
            replica.target.check_unmountable()?;
            replica.flushed = Some(replica.target.clone().prepare_unmount()?.flush()?);
        }
    }
    for replica in replicas {
        let fuse_teardown =
            crate::pseudofs::dev::fuse::prepare_mount_teardown(replica.fuse_mount_ids)?;
        if lazy {
            replica.target.clone().lazy_unmount()?;
        } else {
            replica
                .flushed
                .expect("prepared replica unmount")
                .commit()?;
        }
        if let Err(error) = replica.prepared.commit() {
            // The VFS detach already committed.  Do not leave this removed
            // replica registered merely because a later namespace-ledger
            // generation raced; subsequent replica teardowns must see it as
            // gone when deciding the final connection DESTROY.
            if lazy {
                fuse_teardown.commit_deferred();
            } else {
                fuse_teardown.commit();
            }
            return Err(error);
        }
        if lazy {
            fuse_teardown.commit_deferred();
        } else {
            fuse_teardown.commit();
        }
    }
    Ok(())
}

fn path_suffix<'a>(base: &FsPath, path: &'a FsPath) -> Option<&'a FsPath> {
    let base = base.as_bytes();
    let path_bytes = path.as_bytes();
    if path_bytes == base {
        Some(FsPath::new(b""))
    } else if base == b"/" && path_bytes.starts_with(b"/") {
        Some(path)
    } else {
        path_bytes
            .strip_prefix(base)
            .filter(|suffix| suffix.starts_with(b"/"))
            .map(FsPath::new)
    }
}

fn joined_path(base: &FsPath, suffix: &FsPath) -> AxResult<FsPathBuf> {
    if suffix.as_bytes().is_empty() || suffix.as_bytes() == b"/" {
        return try_path(base);
    }
    if base.as_bytes() == b"/" {
        return try_path(suffix);
    }
    let mut joined = Vec::new();
    joined
        .try_reserve(
            base.as_bytes()
                .len()
                .saturating_add(suffix.as_bytes().len()),
        )
        .map_err(|_| AxError::NoMemory)?;
    joined.extend_from_slice(base.as_bytes());
    joined.extend_from_slice(suffix.as_bytes());
    Ok(FsPathBuf::from_vec(joined))
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
    if crate::file::inode_flags::suppresses_atime(loc) {
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
        || now.seconds().saturating_sub(metadata.atime.seconds()) >= RELATIME_MAX_AGE_SECS as i64
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
            mount_id_old: mount_id as u32,
            parent_id,
            root: FsPathBuf::from_vec(b"/".to_vec()),
            source: FsPathBuf::from_vec(b"none".to_vec()),
            target: FsPathBuf::from_vec(target.as_bytes().to_vec()),
            fs_type: "tmpfs".to_string(),
            data: String::new(),
            dev: mount_id,
            flags: 0,
            expire_epoch: None,
            mountpoint: Weak::new(),
        }
    }

    #[test]
    fn remount_publication_adopts_flags_and_data_without_structural_reconciliation() {
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = MemoryFs::new().unwrap();
        let mountpoint = Mountpoint::new_root(&filesystem);
        let initial_metadata =
            MountMetadata::try_from_parts(FsPath::new(b"none"), "tmpfs", FsPath::new(b"/"), "")
                .unwrap();
        mountpoint
            .initialize_extensions(
                mount_extensions(0, initial_metadata.try_clone().unwrap(), 1).unwrap(),
            )
            .unwrap();
        let record = MountRecord {
            mount_id: mountpoint.mount_id(),
            mount_id_old: 1,
            parent_id: 0,
            root: FsPathBuf::from_vec(b"/".to_vec()),
            source: FsPathBuf::from_vec(b"none".to_vec()),
            target: FsPathBuf::from_vec(b"/".to_vec()),
            fs_type: "tmpfs".to_string(),
            data: String::new(),
            dev: mountpoint.device(),
            flags: 0,
            expire_epoch: None,
            mountpoint: Arc::downgrade(&mountpoint),
        };
        let topology =
            MountTopology::try_new(1, alloc::vec![Mount::try_from_record(&record, None).unwrap()])
                .unwrap();
        let mut records = topology.try_records().unwrap();
        records[0].flags = MS_RDONLY | MS_NOEXEC;
        records[0].data = "size=64M".to_string();
        let publication = topology.prepare_remount_records(&records).unwrap();
        let state = mount_state(&mountpoint).unwrap();
        let mut remount_metadata = initial_metadata;
        remount_metadata.data = "size=64M".to_string();
        let visibility = RemountVisibilityGuard::begin(&state).unwrap();
        publication.commit().unwrap();
        *state.metadata.lock() = remount_metadata;
        state.flags.store(MS_RDONLY | MS_NOEXEC, Ordering::Release);
        visibility.commit();

        let published = topology.try_records().unwrap();
        assert_eq!(published[0].flags, MS_RDONLY | MS_NOEXEC);
        assert_eq!(published[0].data, "size=64M");
        assert!(validate_record_state(&published[0]).is_ok());
        let root = mountpoint.root_location();
        assert_eq!(flags_for_location(&root).unwrap(), MS_RDONLY | MS_NOEXEC);
        assert_eq!(metadata_for_location(&root).unwrap().data, "size=64M");
    }

    #[test]
    fn remount_epoch_exhaustion_rejects_before_entering_visibility_gate() {
        let filesystem = MemoryFs::new().unwrap();
        let mountpoint = Mountpoint::new_root(&filesystem);
        let metadata =
            MountMetadata::try_from_parts(FsPath::new(b"none"), "tmpfs", FsPath::new(b"/"), "")
                .unwrap();
        mountpoint
            .initialize_extensions(mount_extensions(0, metadata, 1).unwrap())
            .unwrap();
        let state = mount_state(&mountpoint).unwrap();
        state.remount_epoch.store(u64::MAX - 1, Ordering::Release);

        assert_eq!(
            RemountVisibilityGuard::begin(&state).err(),
            Some(AxError::OutOfRange)
        );
        assert_eq!(state.remount_epoch.load(Ordering::Acquire), u64::MAX - 1);
    }

    #[test]
    fn device_lookup_keeps_detached_live_superblocks_visible() {
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = MemoryFs::new().unwrap();
        let mountpoint = new_detached_with_flags(
            &filesystem,
            0,
            MountMetadata::try_from_parts(FsPath::new(b"none"), "tmpfs", FsPath::new(b"/"), "")
                .unwrap(),
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
        assert_eq!(lower.target.as_bytes(), b"/mnt");
        assert_eq!(lower.parent_id, 1);
        assert_eq!(moved.target.as_bytes(), b"/moved");
        assert_eq!(moved.parent_id, 1);
        assert_eq!(nested.target.as_bytes(), b"/moved/nested");
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
        assert_eq!(namespace_root.target.as_bytes(), b"/");
        assert_eq!(moved.target.as_bytes(), b"/moved");
        assert_eq!(nested.target.as_bytes(), b"/moved/nested");
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
        assert_eq!(moved.target.as_bytes(), b"/");
        assert_eq!(nested.target.as_bytes(), b"/nested");
    }
}
