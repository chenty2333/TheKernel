use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axsync::Mutex;

/// Native `BTRFS_TREE_LOG_OBJECTID` (`-6LL`) represented in unsigned tree
/// keys and tree-block headers.
pub const TREE_LOG_OBJECTID: u64 = (-6_i64) as u64;

/// Btrfs tree identity.  The numeric representation follows the on-media
/// object ID domain but is deliberately typed so a caller cannot accidentally
/// write an extent item into a root tree.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u64)]
// Complete on-media tree-id table; the writer-side trees are admitted with
// the gated Btrfs COW writer.
#[allow(dead_code)]
pub enum TreeId {
    Root = 1,
    Extent = 2,
    Chunk = 3,
    Device = 4,
    Fs = 5,
    Csum = 7,
    Quota = 8,
    Uuid = 9,
    FreeSpace = 10,
    // BTRFS_TREE_LOG_OBJECTID is the signed on-media value -6, represented
    // in native keys/tree headers as this raw u64.  11 is FreeSpace + 1, not
    // a log-tree owner.
    Log = TREE_LOG_OBJECTID,
    Relocation = 12,
    Subvolume = 256,
}

/// Native B-tree key ordering.  Values are opaque here because each tree has
/// a different binary payload; this core preserves their bytes and never
/// performs a lossy reinterpretation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TreeItemKey {
    pub objectid: u64,
    pub item_type: u8,
    pub offset: u64,
}

/// A delayed extent-reference update.  Reference accounting is committed in
/// the same generation as the tree edits that introduce/remove the extent.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DelayedRefIdentity {
    Data { file_offset: u64 },
    TreeBlock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelayedRef {
    pub bytenr: u64,
    pub len: u64,
    pub root: u64,
    pub owner: u64,
    pub identity: DelayedRefIdentity,
    pub delta: i64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QgroupId {
    pub level: u16,
    pub id: u64,
}

/// A qgroup's exclusive/referenced byte limits.  `None` means unlimited;
/// both counters are maintained separately because reflinks change only the
/// referenced relation in several cases.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QgroupLimit {
    pub max_referenced: Option<u64>,
    pub max_exclusive: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct QgroupUsage {
    referenced: u64,
    exclusive: u64,
    limit: QgroupLimit,
}

#[derive(Clone, Debug)]
// Transaction-core machinery for the gated Btrfs COW writer.
#[allow(dead_code)]
enum TreeChange {
    Set(Arc<[u8]>),
    Delete,
}

#[derive(Clone, Debug)]
// Transaction-core machinery for the gated Btrfs COW writer.
#[allow(dead_code)]
struct StagedLog {
    tree: TreeId,
    key: TreeItemKey,
    value: Option<Arc<[u8]>>,
}

/// Mount-owned Btrfs metadata state.  This is the transaction and recovery
/// nucleus used by the eventual on-disk tree writer: it guarantees atomic
/// generation publication, delayed-ref validation, qgroup admission, and a
/// replayable tree-log boundary before any VFS namespace entry is exposed.
pub struct BtrfsCore {
    state: Mutex<State>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalLease {
    id: u64,
    generation: u64,
}

struct State {
    generation: u64,
    trees: BTreeMap<(TreeId, TreeItemKey), Arc<[u8]>>,
    refs: BTreeMap<(u64, u64, u64, u64, DelayedRefIdentity), u64>,
    qgroups: BTreeMap<QgroupId, QgroupUsage>,
    subvolumes: BTreeMap<u64, u64>,
    next_subvolume: u64,
    committed_log: Vec<StagedLog>,
    next_logical_lease: u64,
    logical_leases: BTreeMap<u64, (u64, Vec<(u64, u64)>)>,
    // Balance progress counter for the gated Btrfs COW writer.
    #[allow(dead_code)]
    planned_balance_inodes: u64,
}

impl BtrfsCore {
    pub fn new(generation: u64) -> AxResult<Arc<Self>> {
        if generation == 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Arc::try_new(Self {
            state: Mutex::new(State {
                generation,
                trees: BTreeMap::new(),
                refs: BTreeMap::new(),
                qgroups: BTreeMap::new(),
                subvolumes: BTreeMap::new(),
                next_subvolume: 256,
                committed_log: Vec::new(),
                next_logical_lease: 1,
                logical_leases: BTreeMap::new(),
                planned_balance_inodes: 0,
            }),
        })
        .map_err(|_| AxError::NoMemory)?)
    }

    // Transaction-core API for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    /// Creates a generation-bound logical-address lease.  Every mutable
    /// allocator created by a mount owns one lease; no two live allocators
    /// can reserve overlapping bytes even though each starts from an
    /// immutable FreeSpace-tree snapshot.
    pub fn begin_logical_lease(&self) -> AxResult<LogicalLease> {
        let mut state = self.state.lock();
        let id = state.next_logical_lease;
        state.next_logical_lease = id.checked_add(1).ok_or(AxError::NoMemory)?;
        let generation = state.generation;
        state.logical_leases.insert(id, (generation, Vec::new()));
        Ok(LogicalLease { id, generation })
    }

    pub fn claim_logical_range(&self, lease: LogicalLease, logical: u64, len: u64) -> AxResult<()> {
        let end = logical.checked_add(len).ok_or(AxError::InvalidInput)?;
        let mut state = self.state.lock();
        if state.generation != lease.generation {
            return Err(AxError::ResourceBusy);
        }
        for (&id, (_, ranges)) in &state.logical_leases {
            if id == lease.id {
                continue;
            }
            if ranges.iter().any(|&(start, span)| {
                start
                    .checked_add(span)
                    .is_some_and(|other_end| logical < other_end && start < end)
            }) {
                return Err(AxError::ResourceBusy);
            }
        }
        let (_, ranges) = state
            .logical_leases
            .get_mut(&lease.id)
            .ok_or(AxError::BadState)?;
        if ranges.iter().any(|&(start, span)| {
            start
                .checked_add(span)
                .is_some_and(|own_end| logical < own_end && start < end)
        }) {
            return Err(AxError::InvalidInput);
        }
        ranges.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        ranges.push((logical, len));
        Ok(())
    }

    pub fn release_logical_range(
        &self,
        lease: LogicalLease,
        logical: u64,
        len: u64,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let (_, ranges) = state
            .logical_leases
            .get_mut(&lease.id)
            .ok_or(AxError::BadState)?;
        let index = ranges
            .iter()
            .position(|&range| range == (logical, len))
            .ok_or(AxError::BadState)?;
        ranges.remove(index);
        Ok(())
    }

    pub fn end_logical_lease(&self, lease: LogicalLease) -> AxResult<()> {
        let mut state = self.state.lock();
        let (_, ranges) = state
            .logical_leases
            .get(&lease.id)
            .ok_or(AxError::BadState)?;
        if !ranges.is_empty() {
            return Err(AxError::ResourceBusy);
        }
        state.logical_leases.remove(&lease.id);
        Ok(())
    }

    // Balance machinery for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn add_planned_balance_inode(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        state.planned_balance_inodes = state
            .planned_balance_inodes
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        Ok(())
    }
    // Balance machinery for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn remove_planned_balance_inode(&self) {
        let mut state = self.state.lock();
        state.planned_balance_inodes = state.planned_balance_inodes.saturating_sub(1);
    }
    // Balance machinery for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn planned_balance_inodes(&self) -> u64 {
        self.state.lock().planned_balance_inodes
    }

    pub fn begin(self: &Arc<Self>) -> BtrfsTransaction {
        let generation = self.state.lock().generation;
        BtrfsTransaction {
            core: self.clone(),
            base_generation: generation,
            changes: BTreeMap::new(),
            delayed_refs: Vec::new(),
            log: Vec::new(),
            qgroup_deltas: BTreeMap::new(),
            new_subvolumes: Vec::new(),
            closed: false,
        }
    }

    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn item(&self, tree: TreeId, key: TreeItemKey) -> Option<Arc<[u8]>> {
        self.state.lock().trees.get(&(tree, key)).cloned()
    }

    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn qgroup_usage(&self, qgroup: QgroupId) -> Option<(u64, u64)> {
        self.state
            .lock()
            .qgroups
            .get(&qgroup)
            .map(|usage| (usage.referenced, usage.exclusive))
    }

    /// Installs a root discovered from the validated root tree.  It is an
    /// initialization operation, not a synthetic subvolume creator: callers
    /// must supply an on-media root object ID and may not overwrite one.
    pub fn register_subvolume(&self, subvolume: u64, root_bytenr: u64) -> AxResult<()> {
        // Tree ID 5 is the default filesystem tree.  It is a perfectly valid
        // snapshot source even though newly allocated subvolume IDs begin at
        // 256.  Rejecting it used to make the normal Btrfs root impossible to
        // snapshot and left the in-memory transaction model disconnected from
        // every real volume.
        if subvolume == 0 || root_bytenr == 0 {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        if state.subvolumes.contains_key(&subvolume) {
            return Err(AxError::AlreadyExists);
        }
        state.subvolumes.insert(subvolume, root_bytenr);
        if subvolume >= 256 {
            state.next_subvolume = state
                .next_subvolume
                .max(subvolume.checked_add(1).ok_or(AxError::NoMemory)?);
        }
        Ok(())
    }

    pub fn subvolume_root(&self, subvolume: u64) -> Option<u64> {
        self.state.lock().subvolumes.get(&subvolume).copied()
    }

    pub fn set_qgroup_limit(&self, qgroup: QgroupId, limit: QgroupLimit) {
        self.state.lock().qgroups.entry(qgroup).or_default().limit = limit;
    }

    /// Imports checked `QGROUP_INFO` counters during mount.  Persistent
    /// writers must seed this before charging a COW generation, otherwise a
    /// negative topology delta would be validated against a fictional zero
    /// baseline.
    pub fn register_existing_qgroup(
        &self,
        qgroup: QgroupId,
        referenced: u64,
        exclusive: u64,
    ) -> AxResult<()> {
        if qgroup.id >> 48 != 0 || exclusive > referenced {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        if state.qgroups.contains_key(&qgroup) {
            return Err(AxError::AlreadyExists);
        }
        state.qgroups.insert(
            qgroup,
            QgroupUsage {
                referenced,
                exclusive,
                limit: QgroupLimit::default(),
            },
        );
        Ok(())
    }

    /// Imports a validated on-media data-reference relation while mounting.
    /// Transactions may then emit a negative delayed ref for that extent;
    /// without this seed a real unlink/overwrite would be rejected as though
    /// the existing extent had never been allocated.
    pub fn register_existing_ref(&self, reference: DelayedRef) -> AxResult<()> {
        if reference.bytenr == 0
            || reference.len == 0
            || reference.root == 0
            || reference.owner == 0
            || reference.delta <= 0
        {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        let key = (
            reference.bytenr,
            reference.len,
            reference.root,
            reference.owner,
            reference.identity,
        );
        let current = state.refs.get(&key).copied().unwrap_or(0);
        let next = current
            .checked_add(reference.delta as u64)
            .ok_or(AxError::NoMemory)?;
        state.refs.insert(key, next);
        Ok(())
    }

    /// Returns a stable snapshot of a registered root map.  Mount code uses
    /// this only to construct on-media ROOT_ITEM updates in the same COW
    /// transaction; callers never receive mutable access to transaction
    /// state.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn subvolumes(&self) -> Vec<(u64, u64)> {
        self.state
            .lock()
            .subvolumes
            .iter()
            .map(|(&id, &root)| (id, root))
            .collect()
    }

    /// Refreshes mount-resident root pointers after their ROOT_ITEM COW
    /// image has passed the superblock publication point.  This cannot create
    /// an identity: snapshot creation still goes through `snapshot`, which
    /// reserves the object ID under the generation gate.  It only prevents a
    /// later operation from resolving an already committed tree through a
    /// retired pre-COW bytenr.
    pub fn refresh_subvolume_roots(&self, roots: &[(u64, u64)]) {
        let mut state = self.state.lock();
        for &(subvolume, root) in roots {
            if root != 0 && state.subvolumes.contains_key(&subvolume) {
                state.subvolumes.insert(subvolume, root);
            }
        }
    }

    /// Applies a committed tree log after a crash boundary.  Normal commits
    /// clear the log only after their durable root publication; callers that
    /// recover an interrupted generation can replay this exact ordered list.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn replay_log(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        let entries = state.committed_log.clone();
        for entry in entries {
            match entry.value {
                Some(value) => {
                    state.trees.insert((entry.tree, entry.key), value);
                }
                None => {
                    state.trees.remove(&(entry.tree, entry.key));
                }
            }
        }
        state.committed_log.clear();
        Ok(())
    }
}

/// One in-flight copy-on-write generation.  The caller writes leaf/node/data
/// blocks through the volume, then invokes `commit_after_persist` only after
/// those writes completed; a later adapter will tie its final superblock
/// write and flush to that method's commit point.
pub struct BtrfsTransaction {
    core: Arc<BtrfsCore>,
    base_generation: u64,
    changes: BTreeMap<(TreeId, TreeItemKey), TreeChange>,
    delayed_refs: Vec<DelayedRef>,
    log: Vec<StagedLog>,
    qgroup_deltas: BTreeMap<QgroupId, (i128, i128)>,
    new_subvolumes: Vec<(u64, u64)>,
    closed: bool,
}

impl BtrfsTransaction {
    /// A topology-only COW commit owns only the chunk/root/free-space trees.
    /// It must reject core-only namespace, delayed-ref, qgroup, log, and
    /// subvolume changes, which otherwise would be committed in memory
    /// without a matching on-media tree rewrite.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.delayed_refs.is_empty()
            && self.log.is_empty()
            && self.qgroup_deltas.is_empty()
            && self.new_subvolumes.is_empty()
    }

    // Transaction-core API for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn set_item(&mut self, tree: TreeId, key: TreeItemKey, value: &[u8]) -> AxResult<()> {
        self.ensure_open()?;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(value);
        let value: Arc<[u8]> = Arc::from(owned.into_boxed_slice());
        self.changes.insert((tree, key), TreeChange::Set(value));
        Ok(())
    }

    // Transaction-core API for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn delete_item(&mut self, tree: TreeId, key: TreeItemKey) -> AxResult<()> {
        self.ensure_open()?;
        self.changes.insert((tree, key), TreeChange::Delete);
        Ok(())
    }

    pub fn add_delayed_ref(&mut self, reference: DelayedRef) -> AxResult<()> {
        self.ensure_open()?;
        if reference.bytenr == 0 || reference.len == 0 || reference.delta == 0 {
            return Err(AxError::InvalidInput);
        }
        self.delayed_refs
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.delayed_refs.push(reference);
        Ok(())
    }

    /// Adds an item to the fsync tree log.  The operation must also be staged
    /// in `changes`; storing only a log record is intentionally rejected.
    #[allow(dead_code)]
    pub fn log_item(&mut self, tree: TreeId, key: TreeItemKey) -> AxResult<()> {
        self.ensure_open()?;
        let value = match self.changes.get(&(tree, key)) {
            Some(TreeChange::Set(value)) => Some(value.clone()),
            Some(TreeChange::Delete) => None,
            None => return Err(AxError::InvalidInput),
        };
        self.log.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        self.log.push(StagedLog { tree, key, value });
        Ok(())
    }

    pub fn charge_qgroup(
        &mut self,
        qgroup: QgroupId,
        referenced_delta: i64,
        exclusive_delta: i64,
    ) -> AxResult<()> {
        self.ensure_open()?;
        let entry = self.qgroup_deltas.entry(qgroup).or_insert((0, 0));
        entry.0 = entry
            .0
            .checked_add(i128::from(referenced_delta))
            .ok_or(AxError::InvalidInput)?;
        entry.1 = entry
            .1
            .checked_add(i128::from(exclusive_delta))
            .ok_or(AxError::InvalidInput)?;
        Ok(())
    }

    /// Creates a snapshot root by COW-sharing the source subvolume's tree
    /// items.  The next writer replaces only its own keys; extent references
    /// are explicitly supplied by the tree builder as delayed refs.
    pub fn snapshot(&mut self, source_subvolume: u64) -> AxResult<u64> {
        self.ensure_open()?;
        let state = self.core.state.lock();
        let source = state
            .subvolumes
            .get(&source_subvolume)
            .copied()
            .ok_or(AxError::NotFound)?;
        let destination = state
            .next_subvolume
            .checked_add(self.new_subvolumes.len() as u64)
            .ok_or(AxError::NoMemory)?;
        drop(state);
        self.new_subvolumes
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.new_subvolumes.push((destination, source));
        Ok(destination)
    }

    /// Records a reflink relation.  The destination item must be written by
    /// the caller in this transaction, and the extent gets a real refcount;
    /// no data copy or synthetic success is used.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn reflink(
        &mut self,
        source: DelayedRef,
        destination_root: u64,
        destination_owner: u64,
    ) -> AxResult<()> {
        self.ensure_open()?;
        if source.delta <= 0 {
            return Err(AxError::InvalidInput);
        }
        self.add_delayed_ref(DelayedRef {
            root: destination_root,
            owner: destination_owner,
            delta: 1,
            ..source
        })
    }

    /// Dedupe is a verified reflink, not a name-based optimisation.  The
    /// caller supplies the two complete logical extent images after normal
    /// permission/locking checks; unequal bytes leave both mappings alone.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn dedupe(
        &mut self,
        source: DelayedRef,
        destination_root: u64,
        destination_owner: u64,
        source_data: &[u8],
        destination_data: &[u8],
    ) -> AxResult<bool> {
        self.ensure_open()?;
        if source_data.len() as u64 != source.len || destination_data.len() as u64 != source.len {
            return Err(AxError::InvalidInput);
        }
        if source_data != destination_data {
            return Ok(false);
        }
        self.reflink(source, destination_root, destination_owner)?;
        Ok(true)
    }

    /// Returns the exact staged item image for one tree.  A persistent tree
    /// writer consumes this image while it still owns the generation gate;
    /// exposing an immutable copy avoids a second, lossy mutation journal.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn staged_item(&self, tree: TreeId, key: TreeItemKey) -> Option<Option<Arc<[u8]>>> {
        self.changes.get(&(tree, key)).map(|change| match change {
            TreeChange::Set(value) => Some(value.clone()),
            TreeChange::Delete => None,
        })
    }

    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn staged_tree_ids(&self) -> Vec<TreeId> {
        let mut trees = Vec::new();
        for ((tree, _), _) in &self.changes {
            if !trees.contains(tree) {
                trees.push(*tree);
            }
        }
        trees
    }

    /// Persistent quota-tree COW is mandatory whenever the transaction has
    /// qgroup accounting.  Mount code uses this admission bit before it
    /// starts reserving metadata nodes, so a successful superblock switch
    /// cannot advance only the in-memory qgroup counters.
    pub fn has_qgroup_deltas(&self) -> bool {
        !self.qgroup_deltas.is_empty()
    }

    /// Immutable projection used by the on-media quota-tree writer.  The
    /// transaction gate remains the authority for limits, but a durable COW
    /// commit must prove that its QGROUP_INFO image expresses precisely these
    /// same deltas before it makes the new root reachable.
    pub fn qgroup_deltas(&self) -> Vec<(QgroupId, i128, i128)> {
        self.qgroup_deltas
            .iter()
            .map(|(&id, &(referenced, exclusive))| (id, referenced, exclusive))
            .collect()
    }

    /// Read-only transaction journal projection for the mount's on-media
    /// extent-image admission check.  Callers cannot mutate the journal via
    /// this view; it exists solely so the writer can reject a supplied
    /// EXTENT_DATA_REF image that does not exactly encode the delayed refs it
    /// is about to publish.
    pub fn delayed_refs(&self) -> &[DelayedRef] {
        &self.delayed_refs
    }

    /// True only for an object ID allocated by `snapshot` in this exact
    /// generation and still pointing at the checked source root it shares.
    /// Root-tree writers use this to reject a direct ROOT_ITEM insertion that
    /// would become reachable on disk without a corresponding core identity.
    pub fn stages_snapshot_root(&self, subvolume: u64, root: u64) -> bool {
        self.new_subvolumes
            .iter()
            .any(|&(destination, source)| destination == subvolume && source == root)
    }

    pub fn staged_snapshot_count(&self) -> usize {
        self.new_subvolumes.len()
    }

    /// Checks every non-I/O admission condition of a transaction before a
    /// caller starts an irreversible data-write phase.  The final commit
    /// repeats these checks under the generation lock; this early projection
    /// exists for batch relocation, where discovering an invalid delayed ref
    /// or qgroup limit after copying data would otherwise turn an ordinary
    /// admission failure into leaked on-media sectors.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn preflight_persist(&self) -> AxResult<()> {
        self.ensure_open()?;
        let state = self.core.state.lock();
        if state.generation != self.base_generation {
            return Err(AxError::ResourceBusy);
        }
        validate_refs(&state.refs, &self.delayed_refs)?;
        validate_qgroups(&state.qgroups, &self.qgroup_deltas)?;
        if self
            .new_subvolumes
            .iter()
            .any(|(destination, _)| state.subvolumes.contains_key(destination))
        {
            return Err(AxError::AlreadyExists);
        }
        let _ = state
            .next_subvolume
            .checked_add(u64::try_from(self.new_subvolumes.len()).map_err(|_| AxError::NoMemory)?)
            .ok_or(AxError::NoMemory)?;
        Ok(())
    }

    /// Atomically publishes metadata after the caller has made all staged
    /// data/node/log blocks durable.  The generation check prevents an older
    /// transaction from overwriting a newer root.
    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn commit_after_persist(mut self) -> AxResult<u64> {
        self.ensure_open()?;
        let mut state = self.core.state.lock();
        let generation = self.commit_locked(&mut state)?;
        self.closed = true;
        Ok(generation)
    }

    /// Executes the caller's durable COW write sequence before publishing the
    /// in-memory generation.  The callback receives the exact next generation
    /// and must return only after data, delayed-ref tree, log tree, roots, and
    /// final superblock barrier completed.  A concurrent winner leaves these
    /// writes unreachable rather than allowing an old transaction to replace
    /// a newer root.
    pub fn commit_with_persist(
        mut self,
        persist: impl FnOnce(u64) -> AxResult<()>,
    ) -> AxResult<u64> {
        self.ensure_open()?;
        let next = self
            .base_generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        // Hold the generation lock across the durable root publication.  The
        // former implementation performed I/O first and only then acquired
        // this lock, allowing a concurrent transaction to win and leaving a
        // perfectly durable but unreachable (or worse, stale-published)
        // generation behind.  Btrfs commits are deliberately serialized at
        // this root-switch boundary.
        let mut state = self.core.state.lock();
        if state.generation != self.base_generation {
            return Err(AxError::ResourceBusy);
        }
        validate_refs(&state.refs, &self.delayed_refs)?;
        validate_qgroups(&state.qgroups, &self.qgroup_deltas)?;
        if self
            .new_subvolumes
            .iter()
            .any(|(destination, _)| state.subvolumes.contains_key(destination))
        {
            return Err(AxError::AlreadyExists);
        }
        // `persist` may make a new root generation reachable.  Prove every
        // remaining scalar commit precondition before that visibility point;
        // in particular a topology COW writer must never release planned
        // nodes because the post-publication bookkeeping counter overflowed.
        let _next_subvolume = state
            .next_subvolume
            .checked_add(u64::try_from(self.new_subvolumes.len()).map_err(|_| AxError::NoMemory)?)
            .ok_or(AxError::NoMemory)?;
        persist(next)?;
        let generation = self.commit_locked(&mut state)?;
        self.closed = true;
        Ok(generation)
    }

    // Transaction writer API in progress.
    #[allow(dead_code)]
    pub fn abort(mut self) {
        self.closed = true;
    }
    fn ensure_open(&self) -> AxResult<()> {
        if self.closed {
            Err(AxError::BadState)
        } else {
            Ok(())
        }
    }

    fn commit_locked(&self, state: &mut State) -> AxResult<u64> {
        if state.generation != self.base_generation {
            return Err(AxError::ResourceBusy);
        }
        validate_refs(&state.refs, &self.delayed_refs)?;
        validate_qgroups(&state.qgroups, &self.qgroup_deltas)?;
        state.committed_log = self.log.clone();
        for (key, change) in &self.changes {
            match change {
                TreeChange::Set(value) => {
                    state.trees.insert(*key, value.clone());
                }
                TreeChange::Delete => {
                    state.trees.remove(key);
                }
            }
        }
        apply_refs(&mut state.refs, &self.delayed_refs)?;
        apply_qgroups(&mut state.qgroups, &self.qgroup_deltas)?;
        for (destination, source) in &self.new_subvolumes {
            state.subvolumes.insert(*destination, *source);
        }
        state.next_subvolume = state
            .next_subvolume
            .checked_add(self.new_subvolumes.len() as u64)
            .ok_or(AxError::NoMemory)?;
        state.generation = state.generation.checked_add(1).ok_or(AxError::NoMemory)?;
        Ok(state.generation)
    }
}

fn validate_refs(
    current: &BTreeMap<(u64, u64, u64, u64, DelayedRefIdentity), u64>,
    updates: &[DelayedRef],
) -> AxResult<()> {
    let mut projected = current.clone();
    apply_refs(&mut projected, updates)
}
fn apply_refs(
    current: &mut BTreeMap<(u64, u64, u64, u64, DelayedRefIdentity), u64>,
    updates: &[DelayedRef],
) -> AxResult<()> {
    for update in updates {
        let key = (
            update.bytenr,
            update.len,
            update.root,
            update.owner,
            update.identity,
        );
        let old = current.get(&key).copied().unwrap_or(0);
        let next = i128::from(old)
            .checked_add(i128::from(update.delta))
            .ok_or(AxError::InvalidInput)?;
        if next < 0 || next > i128::from(u64::MAX) {
            return Err(AxError::InvalidInput);
        }
        if next == 0 {
            current.remove(&key);
        } else {
            current.insert(key, next as u64);
        }
    }
    Ok(())
}
fn validate_qgroups(
    current: &BTreeMap<QgroupId, QgroupUsage>,
    deltas: &BTreeMap<QgroupId, (i128, i128)>,
) -> AxResult<()> {
    for (id, (referenced, exclusive)) in deltas {
        let usage = current.get(id).copied().unwrap_or_default();
        let new_referenced = i128::from(usage.referenced)
            .checked_add(*referenced)
            .ok_or(AxError::InvalidInput)?;
        let new_exclusive = i128::from(usage.exclusive)
            .checked_add(*exclusive)
            .ok_or(AxError::InvalidInput)?;
        if new_referenced < 0
            || new_exclusive < 0
            || new_referenced > i128::from(u64::MAX)
            || new_exclusive > i128::from(u64::MAX)
            || new_exclusive > new_referenced
        {
            return Err(AxError::InvalidInput);
        }
        if usage
            .limit
            .max_referenced
            .map_or(false, |limit| new_referenced > i128::from(limit))
            || usage
                .limit
                .max_exclusive
                .map_or(false, |limit| new_exclusive > i128::from(limit))
        {
            return Err(AxError::StorageFull);
        }
    }
    Ok(())
}
fn apply_qgroups(
    current: &mut BTreeMap<QgroupId, QgroupUsage>,
    deltas: &BTreeMap<QgroupId, (i128, i128)>,
) -> AxResult<()> {
    validate_qgroups(current, deltas)?;
    for (id, (referenced, exclusive)) in deltas {
        let usage = current.entry(*id).or_default();
        usage.referenced = (i128::from(usage.referenced) + *referenced) as u64;
        usage.exclusive = (i128::from(usage.exclusive) + *exclusive) as u64;
    }
    Ok(())
}
