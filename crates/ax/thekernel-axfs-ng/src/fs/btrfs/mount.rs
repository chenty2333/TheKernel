use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec,
    vec::Vec,
};

use axdriver::SharedBlockDevice;
use axerrno::{AxError, AxResult};

use super::{
    BtrfsCore, BtrfsDeviceItem, BtrfsDeviceTopologyChange, BtrfsDirItem, BtrfsDirLogRange,
    BtrfsFileExtent, BtrfsInodeItem, BtrfsInodeRef, BtrfsLogicalAllocator, BtrfsRootItem,
    BtrfsSuperblock, BtrfsTopologyStage, BtrfsTreeBlock, BtrfsVolume, CSUM_ITEM, Checksum,
    Compression, DEV_EXTENT, DEV_ITEM, DIR_ITEM, DIR_LOG_INDEX, DIR_LOG_ITEM, EXTENT_DATA,
    EXTENT_DATA_REF, EXTENT_ITEM, FREE_SPACE_BITMAP, FREE_SPACE_EXTENT, FREE_SPACE_INFO,
    INODE_ITEM, INODE_REF, LogicalReservation, ORPHAN_ITEM, QGROUP_INFO, QGROUP_LIMIT,
    QGROUP_RELATION, ROOT_ITEM, TREE_BLOCK_REF, TreeChild, TreeId, TreeItemKey, TreeLeafItem,
    TreeWriteItem, XATTR_ITEM, btrfs_extref_hash, crc32c, decode_dir_items, decode_inode_extrefs,
    decode_inode_refs, decode_tree_block_ref, decode_tree_extent_item, encode_dir_items,
    encode_inode_extrefs, encode_inode_refs,
};

/// A mounted, checksum-verified Btrfs metadata view.  `BtrfsVolume` is
/// deliberately supplied by the caller: decoding the system chunk array and
/// assembling multi-device members is a separate admission step, and this
/// object must never guess a mapping for a missing device.
pub struct BtrfsMount {
    volume: BtrfsVolume,
    superblock: BtrfsSuperblock,
    core: Arc<BtrfsCore>,
    // Balance worker state for the in-progress relocation path.
    #[allow(dead_code)]
    balance: BtrfsBalanceState,
}

/// Mount-persistent balance state.  Admin/control code supplies work one
/// inode at a time, allowing cancellation and a crash to leave only fully
/// published relocation transactions behind; no in-memory cursor is ever
/// mistaken for on-media completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BtrfsBalanceState {
    pub evacuating_devid: Option<u64>,
    pub relocated_inodes: u64,
    /// Inodes admitted into an as-yet-unpublished batch.  This is useful to
    /// the control plane when a caller deliberately builds bounded batches;
    /// it is never treated as durable completion.
    pub planned_inodes: u64,
    /// Number of root-switch generations published by the balance worker.
    pub committed_batches: u64,
    pub paused: bool,
}

/// A relocation batch has one filesystem-tree owner so that all changed
/// inode records, extent refs, checksums, free-space records and qgroup
/// counters can be represented by one `BtrfsMutationPlanner`.  Higher-level
/// scheduling creates one batch per subvolume and commits each batch through
/// one root switch; it must not fall back to an inode-at-a-time transaction.
// In-progress relocation/balance batch planner.
#[allow(dead_code)]
pub struct BtrfsRelocationPlan {
    core: Arc<BtrfsCore>,
    fs_root: u64,
    tree_owner: u64,
    source_member: usize,
    /// Reservations are valid only for this exact mounted root generation.
    /// A normal writer has no shared allocator lock with balance, so a stale
    /// plan must be discarded rather than replayed against a newer tree.
    base_generation: u64,
    base_fs_root: u64,
    targets: Vec<(u64, u64)>,
    allocator: BtrfsLogicalAllocator,
    jobs: Vec<BtrfsRelocationJob>,
}

// In-progress relocation/balance batch planner.
#[allow(dead_code)]
struct BtrfsRelocationJob {
    inode: u64,
    old_extents: Vec<(u64, BtrfsFileExtent)>,
    stored: Vec<u8>,
    logical_len: u64,
    reservation: LogicalReservation,
}

impl Drop for BtrfsRelocationPlan {
    fn drop(&mut self) {
        for job in &self.jobs {
            // A committed/sealed reservation is intentionally not returned:
            // after data I/O it remains leased until remount/recovery.
            let _ = self.allocator.release(job.reservation);
            self.core.remove_planned_balance_inode();
        }
    }
}

// In-progress relocation/balance batch planner.
#[allow(dead_code)]
impl BtrfsRelocationPlan {
    fn discard_unwritten(&mut self) -> AxResult<()> {
        for job in self.jobs.drain(..) {
            self.allocator.release(job.reservation)?;
            self.core.remove_planned_balance_inode();
        }
        Ok(())
    }
}

// Device add/remove/replace requests for the in-progress topology path.
#[allow(dead_code)]
pub enum BtrfsMountDeviceChange {
    Add {
        item: BtrfsDeviceItem,
        device: SharedBlockDevice,
    },
    Remove {
        devid: u64,
    },
    Replace {
        item: BtrfsDeviceItem,
        device: SharedBlockDevice,
    },
}

/// One verified raw log-tree item.  Log-tree semantics depend on the item
/// type and are applied by the inode/dir replay layer; this transport keeps
/// the original key and payload intact instead of turning an unknown record
/// into an empty update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryLogItem {
    pub key: TreeItemKey,
    pub value: Vec<u8>,
}

/// One checked native entry of the superblock log-root tree.  The entry names
/// a per-subvolume log tree; it is intentionally separate from
/// [`RecoveryLogItem`], whose keys are records inside one such tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryLogRoot {
    pub subvolume: u64,
    pub generation: u64,
    pub root_dirid: u64,
    pub bytenr: u64,
    pub level: u8,
}

/// One decoded home-tree to log-tree file-extent replacement.  The key is
/// carried separately because the on-media extent payload does not contain
/// its owning inode or file offset.  Keeping both native payloads here makes
/// replay account for physical references rather than treating EXTENT_DATA as
/// an opaque filesystem-tree byte string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedExtentTransition {
    pub root: u64,
    pub inode: u64,
    pub file_offset: u64,
    pub old: Option<BtrfsFileExtent>,
    pub new: BtrfsFileExtent,
}

impl LoggedExtentTransition {
    pub fn decode(root: u64, key: TreeItemKey, old: Option<&[u8]>, new: &[u8]) -> AxResult<Self> {
        if root == 0 || key.objectid == 0 || key.item_type != EXTENT_DATA {
            return Err(AxError::InvalidInput);
        }
        let new = BtrfsFileExtent::decode(new)?;
        let old = match old {
            Some(bytes) => Some(BtrfsFileExtent::decode(bytes)?),
            None => None,
        };
        Ok(Self {
            root,
            inode: key.objectid,
            file_offset: key.offset,
            old,
            new,
        })
    }

    fn same_physical_mapping(&self) -> bool {
        self.old.as_ref().is_some_and(|old| {
            old.kind == self.new.kind
                && old.disk_bytenr == self.new.disk_bytenr
                && old.disk_num_bytes == self.new.disk_num_bytes
        })
    }

    fn requires_physical_accounting(extent: &BtrfsFileExtent) -> bool {
        extent.owns_physical_storage()
    }

    fn supports_accounting(extent: &BtrfsFileExtent) -> bool {
        // The replay transition preserves all native header fields in the
        // home-tree item.  Accounting can only create a new physical extent
        // once we have a native checksum representation for it; the current
        // checksum-tree writer is CRC32C/uncompressed only.
        extent.compression == 0 && extent.encryption == 0 && extent.other_encoding == 0
    }
}

/// Typed removal of one home-tree data mapping.  Truncate and orphan cleanup
/// use this rather than deleting an EXTENT_DATA key first and trying to
/// reconstruct its physical ownership afterwards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedExtentRetirement {
    pub root: u64,
    pub inode: u64,
    pub file_offset: u64,
    pub old: BtrfsFileExtent,
}

/// Planner-generated orphan retirement.  This is deliberately not decoded
/// from a log-tree ORPHAN_ITEM: only the final replayed namespace may decide
/// that an inode has lost every link.  The marker key is the native
/// `(-5, ORPHAN_ITEM, inode)` form and is staged only after recursive
/// directory unlink planning succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrphanRetirement {
    pub root: u64,
    pub inode: u64,
}

/// Provenance for replay-created orphan work.  This is populated only when
/// replay removes a concrete namespace edge; scanning all unlinked inode
/// items would incorrectly collect internal/cache/special inodes which have
/// no user namespace lifetime.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReplayOrphanCandidate {
    inode: u64,
}

impl ReplayOrphanCandidate {
    fn new(inode: u64) -> AxResult<Self> {
        if inode == 0 {
            return Err(AxError::Io);
        }
        Ok(Self { inode })
    }
}

impl OrphanRetirement {
    pub const OBJECTID: u64 = u64::MAX - 4;
    pub fn marker_key(self) -> TreeItemKey {
        TreeItemKey {
            objectid: Self::OBJECTID,
            item_type: ORPHAN_ITEM,
            offset: self.inode,
        }
    }
    pub fn new(root: u64, inode: u64) -> AxResult<Self> {
        if root == 0 || inode == 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self { root, inode })
    }
}

impl LoggedExtentRetirement {
    pub fn decode(root: u64, key: TreeItemKey, old: &[u8]) -> AxResult<Self> {
        if root == 0 || key.objectid == 0 || key.item_type != EXTENT_DATA {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            root,
            inode: key.objectid,
            file_offset: key.offset,
            old: BtrfsFileExtent::decode(old)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawTreeItem {
    pub key: TreeItemKey,
    pub value: Vec<u8>,
}

/// One physical metadata node together with the owning tree and its level.
/// Keeping the level in the COW plan is essential: an EXTENT_ITEM for a tree
/// block is not recoverable from the leaf/root pointer alone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TreeBlockRecord {
    bytenr: u64,
    /// Immutable owner encoded in the tree-block header.
    header_owner: u64,
    /// Root relation encoded by TREE_BLOCK_REF and delayed refs.
    relation_root: u64,
    level: u8,
}

/// A range-mutation output is intentionally explicit about provenance.  A
/// byte range which happens to compare equal to the old contents must not be
/// mistaken for a retained extent: `CowData` always receives new storage,
/// while `Retain` moves an existing on-media relation without touching its
/// sectors or checksums.  `Hole` has no EXTENT_DATA item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeSegment {
    Retain {
        source_inode: u64,
        source_offset: u64,
        destination_offset: u64,
        length: u64,
    },
    CowData {
        offset: u64,
        bytes: Vec<u8>,
    },
    /// Reserved but unwritten fallocate storage.  This intentionally has no
    /// checksum/data write and is encoded as FILE_EXTENT_PREALLOC.
    Prealloc {
        offset: u64,
        length: u64,
    },
    Hole {
        offset: u64,
        length: u64,
    },
}

impl RangeSegment {
    pub fn offset(&self) -> u64 {
        match self {
            Self::Retain {
                destination_offset, ..
            } => *destination_offset,
            Self::CowData { offset, .. }
            | Self::Prealloc { offset, .. }
            | Self::Hole { offset, .. } => *offset,
        }
    }

    fn len(&self) -> u64 {
        match self {
            Self::Retain { length, .. }
            | Self::Prealloc { length, .. }
            | Self::Hole { length, .. } => *length,
            Self::CowData { bytes, .. } => bytes.len() as u64,
        }
    }
}

/// Complete replacement image for one non-root Btrfs tree.  The writer does
/// not synthesize accounting records: callers provide the final extent,
/// checksum, free-space, quota and filesystem-tree images assembled by their
/// transaction planner.  This makes a missing accounting update impossible
/// to hide behind a successful root switch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtrfsTreeRewrite {
    /// Object ID of this tree's ROOT_ITEM in the root tree (for a subvolume,
    /// this is its subvolume ID; for fixed trees it is the tree ID).
    pub root_objectid: u64,
    /// Owner stored in each COW node header.
    pub tree_owner: u64,
    /// Header owner of the currently reachable tree being replaced.  This
    /// differs from `tree_owner` for the first COW of a shared snapshot: the
    /// old nodes still identify the source subvolume, while new nodes must
    /// identify the destination.
    pub old_tree_owner: u64,
    pub items: Vec<RawTreeItem>,
}

/// Caller-supplied final images for the two trees which host metadata COW
/// itself.  A normal file mutation has already changed data extent/free-space
/// accounting before it reaches the metadata writer; the common engine then
/// appends its own tree-block relations and removes its own reservations from
/// these exact images.
struct MetadataCowAccounting<'a> {
    extent_items: &'a [RawTreeItem],
    free_space_items: &'a [RawTreeItem],
}

/// One mount-owned mutation plan.  The plan always carries all accounting
/// trees whose roots are part of a normal writable Btrfs volume; a VFS
/// operation cannot publish a namespace/inode edit in isolation and leave a
/// stale checksum, free-space, extent-reference or quota tree behind.
pub struct BtrfsMutationPlanner {
    fs_root_objectid: u64,
    fs_tree_owner: u64,
    trees: BTreeMap<u64, Vec<RawTreeItem>>,
    transaction: super::BtrfsTransaction,
}

/// Fully admitted native tree-log recovery transaction.  It owns one shared
/// accounting image and one independent filesystem-tree image per logged
/// subvolume; nothing in this plan has reached media until `commit` consumes
/// it through the ordinary fixed-point COW writer.
pub struct BtrfsMultiRootReplayPlanner {
    transaction: super::BtrfsTransaction,
    rewrites: Vec<BtrfsTreeRewrite>,
    log_roots: Vec<RecoveryLogRoot>,
    freed_bytes: u64,
}

impl BtrfsMultiRootReplayPlanner {
    /// Publishes all replay targets and clears the native log only through one
    /// common COW transaction.  Recheck the immutable on-media recovery set
    /// before reservation so a future caller cannot accidentally pair a plan
    /// with a different log-root topology.
    fn commit(self, mount: &mut BtrfsMount) -> AxResult<()> {
        if (mount.superblock.log_root == 0 && !self.log_roots.is_empty())
            || (mount.superblock.log_root != 0 && mount.recovery_log_roots()? != self.log_roots)
        {
            return Err(AxError::ResourceBusy);
        }
        let allocator = BtrfsLogicalAllocator::new();
        mount.commit_tree_rewrites(
            self.transaction,
            &allocator,
            &self.rewrites,
            mount.superblock.chunk_root,
            0,
            mount
                .superblock
                .bytes_used
                .checked_sub(self.freed_bytes)
                .ok_or(AxError::Io)?,
        )?;
        Ok(())
    }
}

/// Exact, pre-reserved bytenr sequence for a COW generation.  Tree writers
/// consume this in the same bottom-up order used by the layout preflight;
/// they never touch the allocator while an on-disk free-space image is being
/// assembled.
struct CowReservationPlan {
    nodes: Vec<LogicalReservation>,
    cursor: usize,
}

impl CowReservationPlan {
    fn reserve(
        mount: &BtrfsMount,
        allocator: &BtrfsLogicalAllocator,
        target: Option<(u64, u64)>,
        count: usize,
    ) -> AxResult<Self> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..count {
            nodes.push(mount.reserve_metadata_node_in_chunk(allocator, target)?);
        }
        Ok(Self { nodes, cursor: 0 })
    }
    fn next(&mut self) -> AxResult<u64> {
        let node = *self.nodes.get(self.cursor).ok_or(AxError::Io)?;
        self.cursor = self.cursor.checked_add(1).ok_or(AxError::Io)?;
        Ok(node.logical)
    }
    fn all_consumed(&self) -> bool {
        self.cursor == self.nodes.len()
    }
    fn len(&self) -> usize {
        self.nodes.len()
    }
    fn append(
        &mut self,
        mount: &BtrfsMount,
        allocator: &BtrfsLogicalAllocator,
        target: Option<(u64, u64)>,
        count: usize,
    ) -> AxResult<()> {
        self.nodes
            .try_reserve(count)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..count {
            self.nodes
                .push(mount.reserve_metadata_node_in_chunk(allocator, target)?);
        }
        Ok(())
    }
    fn release_tail(&mut self, allocator: &BtrfsLogicalAllocator, count: usize) -> AxResult<()> {
        if self.cursor != 0 || count > self.nodes.len() {
            return Err(AxError::BadState);
        }
        let split = self.nodes.len() - count;
        for node in self.nodes.drain(split..) {
            allocator.release(node)?;
        }
        Ok(())
    }
    #[allow(dead_code)]
    fn move_tail_before(&mut self, prefix: usize, middle: usize) -> AxResult<()> {
        if self.cursor != 0
            || prefix
                .checked_add(middle)
                .map_or(true, |end| end > self.nodes.len())
        {
            return Err(AxError::BadState);
        }
        self.nodes[prefix..].rotate_left(middle);
        Ok(())
    }
    #[allow(dead_code)]
    fn order_topology_rewrites(
        &mut self,
        chunk: usize,
        root: usize,
        extra: usize,
        free: usize,
    ) -> AxResult<()> {
        if self.cursor != 0
            || chunk
                .checked_add(root)
                .and_then(|end| end.checked_add(extra))
                .and_then(|end| end.checked_add(free))
                != Some(self.nodes.len())
        {
            return Err(AxError::BadState);
        }
        // Initial plan: CHUNK, ROOT, extra roots, FreeSpace.  Writer order:
        // CHUNK, FreeSpace, extra roots, ROOT.
        self.nodes[chunk..].rotate_left(root.checked_add(extra).ok_or(AxError::NoMemory)?);
        self.nodes[chunk + free..].rotate_left(root);
        Ok(())
    }
    fn order_topology_with_extent(
        &mut self,
        chunk: usize,
        root: usize,
        extra: usize,
        free: usize,
        extent: usize,
    ) -> AxResult<()> {
        if self.cursor != 0
            || chunk
                .checked_add(root)
                .and_then(|n| n.checked_add(extra))
                .and_then(|n| n.checked_add(free))
                .and_then(|n| n.checked_add(extent))
                != Some(self.nodes.len())
        {
            return Err(AxError::BadState);
        }
        let mut ordered = Vec::new();
        ordered
            .try_reserve_exact(self.nodes.len())
            .map_err(|_| AxError::NoMemory)?;
        // Reservation planning is CHUNK, ROOT, extra, FreeSpace, EXTENT;
        // durable COW order is CHUNK, FreeSpace, extra, EXTENT, ROOT.
        ordered.extend_from_slice(&self.nodes[..chunk]);
        ordered.extend_from_slice(&self.nodes[chunk + root + extra..chunk + root + extra + free]);
        ordered.extend_from_slice(&self.nodes[chunk + root..chunk + root + extra]);
        ordered.extend_from_slice(&self.nodes[chunk + root + extra + free..]);
        ordered.extend_from_slice(&self.nodes[chunk..chunk + root]);
        self.nodes = ordered;
        Ok(())
    }
    fn commit_all(&mut self, allocator: &BtrfsLogicalAllocator) -> AxResult<()> {
        for node in &mut self.nodes {
            allocator.commit(node)?;
        }
        Ok(())
    }
    fn release_all(self, allocator: &BtrfsLogicalAllocator) -> AxResult<()> {
        for node in self.nodes {
            allocator.release(node)?;
        }
        Ok(())
    }
}

impl BtrfsMount {
    pub fn open_single(volume: axdriver::BlockVolume) -> AxResult<Self> {
        let superblock = BtrfsVolume::discover_superblock(&volume)?;
        let bootstrap = BtrfsVolume::bootstrap_single(volume, &superblock)?;
        Self::open_from_bootstrap(bootstrap, superblock)
    }

    /// Complete multi-member admission.  Every supplied member is identified
    /// by its own checked superblock before a system/chunk-tree stripe can
    /// address it, which prevents both missing-device aliasing and accidental
    /// assembly of two different filesystems.
    pub fn open_multi(volume: axdriver::BlockVolume) -> AxResult<Self> {
        let superblock = BtrfsVolume::discover_superblock(&volume)?;
        let bootstrap = BtrfsVolume::bootstrap_multi(volume, &superblock)?;
        Self::open_from_bootstrap(bootstrap, superblock)
    }

    fn open_from_bootstrap(bootstrap: BtrfsVolume, superblock: BtrfsSuperblock) -> AxResult<Self> {
        // The system chunk array is only a bootstrap map.  Walk the checked
        // chunk tree through that map and replace it before attempting to
        // reach ordinary fs-tree/data extents.
        let mut chunk_items = Vec::new();
        Self::collect_tree_items_from(
            &bootstrap,
            &superblock,
            superblock.chunk_root,
            TreeId::Chunk as u64,
            &mut BTreeSet::new(),
            &mut chunk_items,
        )?;
        let mut chunks = Vec::new();
        for item in chunk_items {
            if item.key.item_type != BtrfsVolume::CHUNK_ITEM_TYPE {
                continue;
            }
            chunks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            chunks.push(super::Chunk::decode_item(
                item.key.objectid,
                &item.value,
                |devid| bootstrap.member_index(devid),
            )?);
        }
        if chunks.is_empty() {
            return Err(AxError::Io);
        }
        let volume = bootstrap.with_chunk_tree(chunks)?;
        Self::open(volume, superblock)
    }

    /// Opens only an already identified filesystem.  Every root pointer is
    /// read through the chunk map and checksum-validated before it becomes
    /// visible to a tree traversal.  A non-zero log root is retained for
    /// explicit replay by the filesystem adapter; it is never discarded at
    /// mount time.
    pub fn open(volume: BtrfsVolume, superblock: BtrfsSuperblock) -> AxResult<Self> {
        let nodesize = usize::try_from(superblock.nodesize).map_err(|_| AxError::Io)?;
        let checksum = superblock.csum_type;
        for (logical, owner) in [
            (superblock.root, TreeId::Root as u64),
            (superblock.chunk_root, TreeId::Chunk as u64),
        ] {
            if logical == 0 {
                return Err(AxError::Io);
            }
            let image =
                volume.read_checked_tree_block(logical, nodesize, &superblock.fsid, checksum)?;
            if BtrfsTreeBlock::decode(
                &image,
                &superblock.fsid,
                Checksum::from_disk(checksum, &image[..32])?,
                logical,
            )?
            .owner()
                != owner
            {
                return Err(AxError::Io);
            }
        }
        if superblock.log_root != 0 {
            let image = volume.read_checked_tree_block(
                superblock.log_root,
                nodesize,
                &superblock.fsid,
                checksum,
            )?;
            let block = BtrfsTreeBlock::decode(
                &image,
                &superblock.fsid,
                Checksum::from_disk(checksum, &image[..32])?,
                superblock.log_root,
            )?;
            if block.owner() != TreeId::Log as u64
                || block.generation() != superblock.log_root_transid
                || block.level() != superblock.log_root_level
            {
                return Err(AxError::Io);
            }
        }
        let core = BtrfsCore::new(superblock.generation)?;
        // Seed the transaction coordinator from the checked root tree before
        // publishing the mount.  Snapshot/reflink operations must start from
        // actual on-media roots, never an empty in-memory fiction.
        let mut root_items = Vec::new();
        Self::collect_tree_items_from(
            &volume,
            &superblock,
            superblock.root,
            TreeId::Root as u64,
            &mut BTreeSet::new(),
            &mut root_items,
        )?;
        for item in &root_items {
            if item.key.item_type != ROOT_ITEM {
                continue;
            }
            if let Ok(root) = BtrfsRootItem::decode(&item.value) {
                // Root-tree entries include internal trees as well as user
                // subvolumes.  Registering all valid object IDs is required
                // for the default fs-tree (5) and for snapshot sources.
                core.register_subvolume(item.key.objectid, root.bytenr)?;
            }
        }
        // Seed delayed-ref accounting from validated extent-tree data refs.
        // This makes later overwrites/unlinks able to retire on-media extents
        // instead of treating the mount-time image as an unaccounted base.
        let extent_root = core
            .subvolume_root(TreeId::Extent as u64)
            .ok_or(AxError::Io)?;
        let mut extent_items = Vec::new();
        Self::collect_tree_items_from(
            &volume,
            &superblock,
            extent_root,
            TreeId::Extent as u64,
            &mut BTreeSet::new(),
            &mut extent_items,
        )?;
        let mut lengths = BTreeMap::new();
        for item in &extent_items {
            if item.key.item_type == super::EXTENT_ITEM
                && item.key.objectid != 0
                && item.key.offset != 0
            {
                lengths.insert(item.key.objectid, item.key.offset);
            }
        }
        for item in &extent_items {
            if item.key.item_type != super::EXTENT_DATA_REF {
                continue;
            }
            let (root, owner, file_offset, count) = super::decode_extent_data_ref(&item.value)?;
            let len = lengths
                .get(&item.key.objectid)
                .copied()
                .ok_or(AxError::Io)?;
            let count = i64::try_from(u64::from(count)).map_err(|_| AxError::Io)?;
            core.register_existing_ref(super::DelayedRef {
                bytenr: item.key.objectid,
                len,
                root,
                owner,
                identity: super::DelayedRefIdentity::Data { file_offset },
                delta: count,
            })?;
        }
        // Metadata COW nodes use TREE_BLOCK_REF rather than EXTENT_DATA_REF.
        // Seed them as well, otherwise the first topology replacement would
        // stage valid negative refs which the in-memory transaction gate sees
        // as subtracting from an empty ledger.
        let mut tree_relation_counts = BTreeMap::new();
        for item in &extent_items {
            if item.key.item_type != TREE_BLOCK_REF {
                continue;
            }
            decode_tree_block_ref(&item.value)?;
            let owner = item.key.offset;
            let len = lengths
                .get(&item.key.objectid)
                .copied()
                .ok_or(AxError::Io)?;
            let extent_key = TreeItemKey {
                objectid: item.key.objectid,
                item_type: EXTENT_ITEM,
                offset: len,
            };
            let extent_index = extent_items
                .binary_search_by_key(&extent_key, |entry| entry.key)
                .map_err(|_| AxError::Io)?;
            // A TREE_BLOCK_REF is never permitted to target a data extent or
            // a malformed metadata header.
            let _ = decode_tree_extent_item(&extent_items[extent_index].value)?;
            let count = tree_relation_counts
                .entry(item.key.objectid)
                .or_insert(0u64);
            *count = count.checked_add(1).ok_or(AxError::NoMemory)?;
            core.register_existing_ref(super::DelayedRef {
                bytenr: item.key.objectid,
                len,
                root: owner,
                owner,
                identity: super::DelayedRefIdentity::TreeBlock,
                delta: 1,
            })?;
        }
        for (&bytenr, &relations) in &tree_relation_counts {
            let len = lengths.get(&bytenr).copied().ok_or(AxError::Io)?;
            let key = TreeItemKey {
                objectid: bytenr,
                item_type: EXTENT_ITEM,
                offset: len,
            };
            let index = extent_items
                .binary_search_by_key(&key, |item| item.key)
                .map_err(|_| AxError::Io)?;
            let (references, _, _) = decode_tree_extent_item(&extent_items[index].value)?;
            if references != relations {
                return Err(AxError::Io);
            }
        }
        // Check the inverse direction too: a tree-format EXTENT_ITEM with
        // no matching TREE_BLOCK_REF is an orphaned metadata allocation, not
        // an admissible delayed-ref baseline.
        for item in &extent_items {
            if item.key.item_type != EXTENT_ITEM {
                continue;
            }
            if tree_relation_counts.contains_key(&item.key.objectid) {
                let (references, _, _) = decode_tree_extent_item(&item.value)?;
                if references == 0
                    || tree_relation_counts.get(&item.key.objectid).copied() != Some(references)
                {
                    return Err(AxError::Io);
                }
            }
        }
        if let Some(quota_root) = core.subvolume_root(TreeId::Quota as u64) {
            let mut quota_items = Vec::new();
            Self::collect_tree_items_from(
                &volume,
                &superblock,
                quota_root,
                TreeId::Quota as u64,
                &mut BTreeSet::new(),
                &mut quota_items,
            )?;
            for item in quota_items
                .iter()
                .filter(|item| item.key.item_type == QGROUP_INFO && item.key.objectid == 0)
            {
                if item.value.len() != 40 {
                    return Err(AxError::Io);
                }
                let level = u16::try_from(item.key.offset >> 48).map_err(|_| AxError::Io)?;
                let id = item.key.offset & ((1u64 << 48) - 1);
                let referenced =
                    u64::from_le_bytes(item.value[8..16].try_into().map_err(|_| AxError::Io)?);
                let exclusive =
                    u64::from_le_bytes(item.value[24..32].try_into().map_err(|_| AxError::Io)?);
                core.register_existing_qgroup(
                    super::QgroupId { level, id },
                    referenced,
                    exclusive,
                )?;
                let limit_key = TreeItemKey {
                    objectid: 0,
                    item_type: QGROUP_LIMIT,
                    offset: item.key.offset,
                };
                if let Ok(limit_index) =
                    quota_items.binary_search_by_key(&limit_key, |entry| entry.key)
                {
                    let limit = &quota_items[limit_index].value;
                    if limit.len() != 40 {
                        return Err(AxError::Io);
                    }
                    let flags = u64::from_le_bytes(limit[..8].try_into().map_err(|_| AxError::Io)?);
                    if flags & !3 != 0 {
                        return Err(AxError::OperationNotSupported);
                    }
                    let max_referenced = (flags & 1 != 0)
                        .then(|| u64::from_le_bytes(limit[8..16].try_into().unwrap()));
                    let max_exclusive = (flags & 2 != 0)
                        .then(|| u64::from_le_bytes(limit[16..24].try_into().unwrap()));
                    core.set_qgroup_limit(
                        super::QgroupId { level, id },
                        super::QgroupLimit {
                            max_referenced,
                            max_exclusive,
                        },
                    );
                }
            }
        }
        Ok(Self {
            core,
            volume,
            superblock,
            balance: BtrfsBalanceState::default(),
        })
    }

    pub fn superblock(&self) -> BtrfsSuperblock {
        self.superblock
    }
    // Writer-side accessor kept for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn core(&self) -> &Arc<BtrfsCore> {
        &self.core
    }
    pub fn volume(&self) -> &BtrfsVolume {
        &self.volume
    }

    pub fn begin_transaction(&self) -> super::BtrfsTransaction {
        self.core.begin()
    }

    /// Starts a complete writable plan for a subvolume.  Every tree is read
    /// through the checked tree walker before it becomes mutable.  The
    /// resulting planner contains the fs, extent, checksum, free-space and
    /// quota images even for a metadata-only change, which is the admission
    /// boundary preventing an incomplete cross-tree commit.
    pub fn mutation_planner(&self, fs_root_objectid: u64) -> AxResult<BtrfsMutationPlanner> {
        let fs_root = self.subvolume_root(fs_root_objectid)?;
        let image = self.volume.read_checked_tree_block(
            fs_root,
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            self.superblock.csum_type,
        )?;
        let fs_tree_owner = BtrfsTreeBlock::decode(
            &image,
            &self.superblock.fsid,
            Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
            fs_root,
        )?
        .owner();
        if fs_tree_owner == 0 {
            return Err(AxError::Io);
        }
        let mut trees = BTreeMap::new();
        for objectid in [
            fs_root_objectid,
            TreeId::Extent as u64,
            TreeId::Csum as u64,
            TreeId::FreeSpace as u64,
            TreeId::Quota as u64,
        ] {
            let root = if objectid == fs_root_objectid {
                fs_root
            } else {
                match self.subvolume_root(objectid) {
                    Ok(root) => root,
                    // Qgroups are an optional on-media feature.  A plan on a
                    // volume without a quota tree has no qgroup state to
                    // update; once present, it is mandatory in every plan.
                    Err(AxError::NotFound) if objectid == TreeId::Quota as u64 => continue,
                    Err(error) => return Err(error),
                }
            };
            let mut items = Vec::new();
            let header_owner = if objectid == fs_root_objectid {
                fs_tree_owner
            } else {
                objectid
            };
            self.collect_tree_items(root, header_owner, &mut BTreeSet::new(), &mut items)?;
            trees.insert(objectid, items);
        }
        Ok(BtrfsMutationPlanner {
            fs_root_objectid,
            fs_tree_owner,
            trees,
            transaction: self.begin_transaction(),
        })
    }

    /// Stages a real ROOT_ITEM for a snapshot.  The source payload is copied
    /// byte-for-byte from the validated root tree so unknown root-item fields
    /// survive; only the key changes.  The caller commits this staged root
    /// tree alongside the delayed extent references produced by its COW
    /// filesystem-tree writer.
    #[allow(dead_code)]
    pub fn stage_snapshot(
        &self,
        transaction: &mut super::BtrfsTransaction,
        source_subvolume: u64,
    ) -> AxResult<u64> {
        let destination = transaction.snapshot(source_subvolume)?;
        let source_key = TreeItemKey {
            objectid: source_subvolume,
            item_type: ROOT_ITEM,
            offset: 0,
        };
        let source = self
            .lookup(self.superblock.root, TreeId::Root as u64, source_key)?
            .ok_or(AxError::NotFound)?;
        transaction.set_item(
            TreeId::Root,
            TreeItemKey {
                objectid: destination,
                item_type: ROOT_ITEM,
                offset: 0,
            },
            &source,
        )?;
        transaction.log_item(
            TreeId::Root,
            TreeItemKey {
                objectid: destination,
                item_type: ROOT_ITEM,
                offset: 0,
            },
        )?;
        Ok(destination)
    }

    /// Creates a writable snapshot root through the same root-tree COW
    /// publication path used by ordinary transactions.  The new ROOT_ITEM is
    /// copied from the validated source payload, so feature/UUID/drop fields
    /// unknown to this implementation survive unchanged; only its object ID
    /// changes.  The transaction coordinator records the shared root before
    /// the redundant superblock makes that root reachable.
    #[allow(dead_code)]
    pub fn snapshot_subvolume(&mut self, source_subvolume: u64) -> AxResult<u64> {
        let mut transaction = self.begin_transaction();
        let destination = self.stage_snapshot(&mut transaction, source_subvolume)?;
        let source_key = TreeItemKey {
            objectid: source_subvolume,
            item_type: ROOT_ITEM,
            offset: 0,
        };
        let mut root_items = self.root_tree_items()?;
        let source_index = root_items
            .binary_search_by_key(&source_key, |item| item.key)
            .map_err(|_| AxError::NotFound)?;
        let mut copied = root_items[source_index].clone();
        copied.key.objectid = destination;
        let insert = root_items
            .binary_search_by_key(&copied.key, |item| item.key)
            .unwrap_or_else(|index| index);
        if root_items
            .get(insert)
            .is_some_and(|item| item.key == copied.key)
        {
            return Err(AxError::AlreadyExists);
        }
        root_items.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        root_items.insert(insert, copied);
        let allocator = self.logical_allocator()?;
        let bytes_used = self.superblock.bytes_used;
        self.commit_root_tree_transaction(
            transaction,
            &allocator,
            &root_items,
            self.superblock.chunk_root,
            0,
            bytes_used,
        )?;
        Ok(destination)
    }

    /// Reads and validates the native superblock log-root tree.  Linux Btrfs
    /// stores one ROOT_ITEM per logged subvolume here; it is not itself the
    /// inode log tree.  This phase is read-only and must finish before any
    /// recovery planner or media write is considered.
    pub fn recovery_log_roots(&self) -> AxResult<Vec<RecoveryLogRoot>> {
        if self.superblock.log_root == 0 {
            return Ok(Vec::new());
        }
        self.validate_recovery_log_tree_root(
            self.superblock.log_root,
            TreeId::Log as u64,
            self.superblock.log_root_transid,
            self.superblock.log_root_level,
        )?;
        let mut items = Vec::new();
        self.collect_leaf_items(
            self.superblock.log_root,
            TreeId::Log as u64,
            &mut BTreeSet::new(),
            &mut items,
        )?;
        let mut roots = Vec::new();
        let mut subvolumes = BTreeSet::new();
        roots
            .try_reserve(items.len())
            .map_err(|_| AxError::NoMemory)?;
        for item in items {
            if item.key.item_type != ROOT_ITEM || item.key.offset != 0 {
                return Err(AxError::Io);
            }
            // Internal tree IDs never name replay targets.
            if item.key.objectid < TreeId::Subvolume as u64 || !subvolumes.insert(item.key.objectid)
            {
                return Err(AxError::Io);
            }
            let root = BtrfsRootItem::decode(&item.value)?;
            if root.generation > self.superblock.log_root_transid {
                return Err(AxError::Io);
            }
            self.validate_recovery_log_tree_root(
                root.bytenr,
                item.key.objectid,
                root.generation,
                root.level,
            )?;
            // Walk the complete per-subvolume tree now, before a later
            // recovery phase can allocate a planner.  Besides checksum and
            // FSID checks, this enforces every child pointer's owner and
            // generation and rejects cycles in the native tree.
            let mut tree_items = Vec::new();
            self.collect_leaf_items(
                root.bytenr,
                item.key.objectid,
                &mut BTreeSet::new(),
                &mut tree_items,
            )?;
            roots.push(RecoveryLogRoot {
                subvolume: item.key.objectid,
                generation: root.generation,
                root_dirid: root.root_dirid,
                bytenr: root.bytenr,
                level: root.level,
            });
        }
        Ok(roots)
    }

    /// Enumerates every filesystem-tree ROOT_ITEM from the root tree.  The
    /// root/chunk/extent/log/relocation trees are below `Subvolume`; the
    /// historical default filesystem tree is objectid 5 and is included
    /// explicitly.  This is used after log replay too, because native orphan
    /// markers are independent recovery state.
    fn recovery_filesystem_roots(&self) -> AxResult<Vec<RecoveryLogRoot>> {
        let mut items = Vec::new();
        self.collect_leaf_items(
            self.superblock.root,
            TreeId::Root as u64,
            &mut BTreeSet::new(),
            &mut items,
        )?;
        let mut roots = Vec::new();
        for item in items {
            if item.key.item_type != ROOT_ITEM || item.key.offset != 0 {
                continue;
            }
            if item.key.objectid != 5 && item.key.objectid < TreeId::Subvolume as u64 {
                continue;
            }
            let root = BtrfsRootItem::decode(&item.value)?;
            self.validate_recovery_log_tree_root(
                root.bytenr,
                item.key.objectid,
                root.generation,
                root.level,
            )?;
            roots.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            roots.push(RecoveryLogRoot {
                subvolume: item.key.objectid,
                generation: root.generation,
                root_dirid: root.root_dirid,
                bytenr: root.bytenr,
                level: root.level,
            });
        }
        roots.sort_by_key(|root| root.subvolume);
        if roots.is_empty() {
            return Err(AxError::Io);
        }
        Ok(roots)
    }

    fn validate_recovery_log_tree_root(
        &self,
        logical: u64,
        owner: u64,
        generation: u64,
        level: u8,
    ) -> AxResult<()> {
        let image = self.volume.read_checked_tree_block(
            logical,
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            self.superblock.csum_type,
        )?;
        let block = BtrfsTreeBlock::decode(
            &image,
            &self.superblock.fsid,
            Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
            logical,
        )?;
        if block.owner() != owner || block.generation() != generation || block.level() != level {
            return Err(AxError::Io);
        }
        Ok(())
    }

    /// Replays every checked native per-subvolume log tree in one COW
    /// publication. The obsolete `subvolume` argument is deliberately
    /// ignored: native media decides the targets through log-root ROOT_ITEMs.
    pub fn replay_inode_log(&mut self, _subvolume: u64) -> AxResult<()> {
        if self.superblock.log_root == 0 && !self.has_native_orphan_work()? {
            return Ok(());
        }
        let planner = self.multi_root_replay_planner()?;
        planner.commit(self)
    }

    pub fn has_native_orphan_work(&self) -> AxResult<bool> {
        for root in self.recovery_filesystem_roots()? {
            let mut items = Vec::new();
            self.collect_tree_items(
                root.bytenr,
                root.subvolume,
                &mut BTreeSet::new(),
                &mut items,
            )?;
            if items.iter().any(|item| {
                item.key.objectid == OrphanRetirement::OBJECTID
                    && item.key.item_type == ORPHAN_ITEM
                    && item.key.offset != 0
                    && item.value.is_empty()
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Native ORPHAN_ITEMs are recovery state in their own right; they are
    /// not contingent on a tree log existing.  Mount scans the exact home
    /// tree marker form and publishes its cleanup through the normal COW
    /// transaction before exposing the filesystem.
    #[allow(dead_code)]
    fn recover_native_orphans(&mut self, subvolume: u64) -> AxResult<()> {
        let _ = self.subvolume_root(subvolume)?;
        let mut planner = self.mutation_planner(subvolume)?;
        let before = planner.tree_items(subvolume)?.to_vec();
        let mut items = before.clone();
        let root_dirid = self.superblock.root_dir_objectid;
        let free_space =
            self.logical_allocator_from_items(planner.tree_items(TreeId::FreeSpace as u64)?)?;
        let generation = self
            .superblock
            .generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let mut candidates = BTreeSet::new();
        let freed = Self::plan_generated_orphans(
            &mut items,
            subvolume,
            root_dirid,
            &mut planner,
            generation,
            u64::from(self.superblock.sectorsize),
            &free_space,
            &mut candidates,
        )?;
        if items == before {
            return Ok(());
        }
        for item in &before {
            if items
                .binary_search_by_key(&item.key, |entry| entry.key)
                .is_err()
            {
                let _ = planner.delete_item(subvolume, item.key)?;
            }
        }
        for item in &items {
            if before
                .binary_search_by_key(&item.key, |entry| entry.key)
                .ok()
                .is_none_or(|old| before[old].value != item.value)
            {
                planner.set_item(subvolume, item.key, item.value.clone())?;
            }
        }
        planner.finish_logged_extent_accounting(&free_space)?;
        self.commit_mutation_planner(
            planner,
            0,
            self.superblock
                .bytes_used
                .checked_sub(freed)
                .ok_or(AxError::Io)?,
        )?;
        Ok(())
    }

    fn recovery_log_items_for_root(&self, root: RecoveryLogRoot) -> AxResult<Vec<RecoveryLogItem>> {
        self.validate_recovery_log_tree_root(
            root.bytenr,
            root.subvolume,
            root.generation,
            root.level,
        )?;
        let mut records = Vec::new();
        self.collect_leaf_items(
            root.bytenr,
            root.subvolume,
            &mut BTreeSet::new(),
            &mut records,
        )?;
        records.sort_by_key(|record| record.key);
        Ok(records)
    }

    fn validate_recovery_log_records(records: &[RecoveryLogItem]) -> AxResult<()> {
        for record in records {
            if record.key.objectid == 0 {
                return Err(AxError::Io);
            }
            match record.key.item_type {
                // Deletion records require coordinated removal of both
                // directory indexes and their inode/data accounting.  Keep
                // them out of the admitted set until that full transition is
                // represented in the shared transaction image.
                DIR_LOG_INDEX => {
                    let _ = BtrfsDirLogRange::decode(record.key.offset, &record.value)?;
                }
                ORPHAN_ITEM => return Err(AxError::Unsupported),
                // Linux reserves this obsolete key number but no longer
                // emits it.  A no-op acceptance would preserve unprocessed
                // deletion state, so fail before any COW reservation.
                DIR_LOG_ITEM => return Err(AxError::Unsupported),
                INODE_ITEM => {
                    if record.key.offset != 0 {
                        return Err(AxError::Io);
                    }
                    let _ = BtrfsInodeItem::decode(&record.value)?;
                }
                // The planner below decodes this again as a
                // LoggedExtentTransition and stages the matching shared
                // extent/csum/free-space/qgroup work before it rewrites the
                // home tree.
                EXTENT_DATA => {
                    let _ = BtrfsFileExtent::decode(&record.value)?;
                }
                XATTR_ITEM => {
                    let entries = decode_dir_items(&record.value)?;
                    if entries.is_empty()
                        || entries
                            .iter()
                            .any(|entry| u64::from(crc32c(&entry.name)) != record.key.offset)
                    {
                        return Err(AxError::Io);
                    }
                }
                DIR_ITEM => {
                    let entries = decode_dir_items(&record.value)?;
                    if entries.is_empty()
                        || entries.iter().any(|entry| {
                            u64::from(crc32c(&entry.name)) != record.key.offset
                                || entry.location_type != INODE_ITEM
                                || entry.location_offset != 0
                        })
                    {
                        return Err(AxError::Io);
                    }
                }
                super::DIR_INDEX => {
                    let entries = decode_dir_items(&record.value)?;
                    if entries.len() != 1
                        || record.key.offset == 0
                        || entries[0].location_type != INODE_ITEM
                        || entries[0].location_offset != 0
                    {
                        return Err(AxError::Io);
                    }
                }
                super::INODE_REF => {
                    if record.key.offset == 0 {
                        return Err(AxError::Io);
                    }
                    let _ = decode_inode_refs(&record.value)?;
                }
                super::INODE_EXTREF => {
                    let records = decode_inode_extrefs(&record.value)?;
                    if records.is_empty()
                        || records.iter().any(|(parent, _, name)| {
                            btrfs_extref_hash(*parent, name) != record.key.offset
                        })
                    {
                        return Err(AxError::Io);
                    }
                }
                _ => return Err(AxError::Unsupported),
            }
        }
        Ok(())
    }

    fn set_recovery_item(
        items: &mut Vec<RawTreeItem>,
        key: TreeItemKey,
        value: Vec<u8>,
    ) -> AxResult<()> {
        match items.binary_search_by_key(&key, |item| item.key) {
            Ok(index) => items[index].value = value,
            Err(index) => {
                items.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                items.insert(index, RawTreeItem { key, value });
            }
        }
        Ok(())
    }

    fn delete_recovery_item(items: &mut Vec<RawTreeItem>, key: TreeItemKey) -> AxResult<Vec<u8>> {
        let index = items
            .binary_search_by_key(&key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        Ok(items.remove(index).value)
    }

    /// Applies the typed DIR_LOG_INDEX delete intervals before the log's
    /// positive DIR_INDEX records.  A range removes only a home index which
    /// is absent from the log at the same parent/index/name; packed hash
    /// buckets and the matching ordinary/extended backref are edited by name
    /// rather than dropped wholesale.
    fn apply_dir_log_ranges(
        items: &mut Vec<RawTreeItem>,
        records: &[RecoveryLogItem],
        orphan_candidates: &mut BTreeSet<ReplayOrphanCandidate>,
    ) -> AxResult<()> {
        let mut ranges: BTreeMap<u64, Vec<BtrfsDirLogRange>> = BTreeMap::new();
        let mut logged = BTreeSet::new();
        for record in records {
            match record.key.item_type {
                DIR_LOG_INDEX => {
                    let range = BtrfsDirLogRange::decode(record.key.offset, &record.value)?;
                    let list = ranges.entry(record.key.objectid).or_default();
                    list.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    list.push(range);
                }
                super::DIR_INDEX => {
                    for entry in decode_dir_items(&record.value)? {
                        logged.insert((
                            record.key.objectid,
                            record.key.offset,
                            entry.name,
                            entry.inode,
                            entry.location_type,
                            entry.location_offset,
                            entry.item_type,
                        ));
                    }
                }
                _ => {}
            }
        }
        for list in ranges.values_mut() {
            list.sort_by_key(|range| range.first);
            let mut merged: Vec<BtrfsDirLogRange> = Vec::new();
            for range in list.iter().copied() {
                if let Some(last) = merged.last_mut() {
                    if range.first <= last.last.saturating_add(1) {
                        last.last = last.last.max(range.last);
                        continue;
                    }
                }
                merged.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                merged.push(range);
            }
            *list = merged;
        }
        let candidates: Vec<_> = items
            .iter()
            .filter(|item| item.key.item_type == super::DIR_INDEX)
            .cloned()
            .collect();
        for index_item in candidates {
            let Some(parent_ranges) = ranges.get(&index_item.key.objectid) else {
                continue;
            };
            if !parent_ranges.iter().any(|range| {
                index_item.key.offset >= range.first && index_item.key.offset <= range.last
            }) {
                continue;
            }
            let entries = decode_dir_items(&index_item.value)?;
            if entries.len() != 1 {
                return Err(AxError::Io);
            }
            let entry = &entries[0];
            if logged.contains(&(
                index_item.key.objectid,
                index_item.key.offset,
                entry.name.clone(),
                entry.inode,
                entry.location_type,
                entry.location_offset,
                entry.item_type,
            )) {
                continue;
            }
            Self::unlink_replayed_namespace_entry(
                items,
                index_item.key.objectid,
                index_item.key.offset,
                &entry.name,
                entry.inode,
                entry.location_type,
                entry.location_offset,
                entry.item_type,
            )?;
            orphan_candidates.insert(ReplayOrphanCandidate::new(entry.inode)?);
        }
        Ok(())
    }

    /// Removes one fully identified namespace edge.  Directory index records
    /// are the authoritative ordered side; the hash bucket and exactly one
    /// ordinary or extended inode backref must follow it.  Keeping this as a
    /// single operation is important for both log-range deletion and orphan
    /// directory teardown: neither caller may leave an unpaired half-edge in
    /// the merged image.
    fn unlink_replayed_namespace_entry(
        items: &mut Vec<RawTreeItem>,
        parent: u64,
        index: u64,
        name: &[u8],
        inode: u64,
        location_type: u8,
        location_offset: u64,
        item_type: u8,
    ) -> AxResult<()> {
        let index_key = TreeItemKey {
            objectid: parent,
            item_type: super::DIR_INDEX,
            offset: index,
        };
        let index_item = Self::delete_recovery_item(items, index_key)?;
        let index_entries = decode_dir_items(&index_item)?;
        if index_entries.len() != 1
            || index_entries[0].inode != inode
            || index_entries[0].location_type != location_type
            || index_entries[0].location_offset != location_offset
            || index_entries[0].item_type != item_type
            || index_entries[0].name.as_slice() != name
        {
            return Err(AxError::Io);
        }

        let hash = u64::from(crc32c(name));
        let bucket_key = TreeItemKey {
            objectid: parent,
            item_type: DIR_ITEM,
            offset: hash,
        };
        let bucket_index = items
            .binary_search_by_key(&bucket_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let mut bucket = decode_dir_items(&items[bucket_index].value)?;
        let before = bucket.len();
        bucket.retain(|entry| {
            !(entry.inode == inode
                && entry.location_type == location_type
                && entry.location_offset == location_offset
                && entry.item_type == item_type
                && entry.name.as_slice() == name)
        });
        if bucket.len().checked_add(1) != Some(before) {
            return Err(AxError::Io);
        }
        if bucket.is_empty() {
            let _ = Self::delete_recovery_item(items, bucket_key)?;
        } else {
            items[bucket_index].value = encode_dir_items(&bucket)?;
        }

        let ordinary = TreeItemKey {
            objectid: inode,
            item_type: INODE_REF,
            offset: parent,
        };
        if let Ok(ref_index) = items.binary_search_by_key(&ordinary, |item| item.key) {
            let mut refs = decode_inode_refs(&items[ref_index].value)?;
            let before = refs.len();
            refs.retain(|reference| {
                !(reference.index == index && reference.name.as_slice() == name)
            });
            if refs.len() + 1 == before {
                if refs.is_empty() {
                    let _ = items.remove(ref_index);
                } else {
                    items[ref_index].value = encode_inode_refs(&refs)?;
                }
                return Ok(());
            }
        }
        let ext_key = TreeItemKey {
            objectid: inode,
            item_type: super::INODE_EXTREF,
            offset: btrfs_extref_hash(parent, name),
        };
        let ext_index = items
            .binary_search_by_key(&ext_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let mut extrefs = decode_inode_extrefs(&items[ext_index].value)?;
        let before = extrefs.len();
        extrefs.retain(|(ref_parent, ref_index, ref_name)| {
            !(*ref_parent == parent && *ref_index == index && ref_name.as_slice() == name)
        });
        if extrefs.len().checked_add(1) != Some(before) {
            return Err(AxError::Io);
        }
        if extrefs.is_empty() {
            let _ = items.remove(ext_index);
        } else {
            items[ext_index].value = encode_inode_extrefs(&extrefs)?;
        }
        Ok(())
    }

    fn assert_replayed_directory_acyclic(items: &[RawTreeItem]) -> AxResult<()> {
        let modes: BTreeMap<u64, u32> = items
            .iter()
            .filter(|item| item.key.item_type == INODE_ITEM && item.key.offset == 0)
            .map(|item| Ok((item.key.objectid, BtrfsInodeItem::decode(&item.value)?.mode)))
            .collect::<AxResult<_>>()?;
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == super::DIR_INDEX)
        {
            let entries = decode_dir_items(&item.value)?;
            if entries.len() != 1 || !modes.contains_key(&item.key.objectid) {
                return Err(AxError::Io);
            }
            let child = entries[0].inode;
            if modes
                .get(&child)
                .is_some_and(|mode| *mode & 0o170000 == 0o040000)
            {
                let list = children.entry(item.key.objectid).or_default();
                list.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                list.push(child);
            }
        }
        fn visit(
            node: u64,
            edges: &BTreeMap<u64, Vec<u64>>,
            active: &mut BTreeSet<u64>,
            done: &mut BTreeSet<u64>,
        ) -> AxResult<()> {
            if done.contains(&node) {
                return Ok(());
            }
            if !active.insert(node) {
                return Err(AxError::Io);
            }
            if let Some(children) = edges.get(&node) {
                for &child in children {
                    visit(child, edges, active, done)?;
                }
            }
            active.remove(&node);
            done.insert(node);
            Ok(())
        }
        let mut active = BTreeSet::new();
        let mut done = BTreeSet::new();
        for &inode in modes.keys() {
            visit(inode, &children, &mut active, &mut done)?;
        }
        Ok(())
    }

    /// Turns the final merged namespace into planner-generated orphan work.
    /// Log ORPHAN_ITEM records never reach here.  Unlinked directories are
    /// first emptied through exact namespace triples, recursively, and only
    /// then only ledger-proven zero-link inodes and empty unlinked
    /// directories are marked and retired.  Every mutation remains in the
    /// replay image and the shared accounting planner until the outer
    /// multi-root commit.
    fn plan_generated_orphans(
        items: &mut Vec<RawTreeItem>,
        root: u64,
        root_dirid: u64,
        accounting: &mut BtrfsMutationPlanner,
        generation: u64,
        sector: u64,
        free_space: &BtrfsLogicalAllocator,
        orphan_candidates: &mut BTreeSet<ReplayOrphanCandidate>,
    ) -> AxResult<u64> {
        let mut freed = 0u64;
        if root_dirid == 0 {
            return Err(AxError::Io);
        }
        Self::assert_replayed_directory_acyclic(items)?;
        // A pre-existing native orphan marker is independently admitted
        // provenance.  It participates in directory teardown just like an
        // edge removed by this replay, but a log-tree ORPHAN_ITEM still never
        // reaches this function.
        for item in items
            .iter()
            .filter(|item| item.key.item_type == ORPHAN_ITEM)
        {
            if item.key.objectid != OrphanRetirement::OBJECTID
                || item.key.offset == 0
                || !item.value.is_empty()
            {
                return Err(AxError::Io);
            }
            orphan_candidates.insert(ReplayOrphanCandidate::new(item.key.offset)?);
        }

        // Empty every directory which has lost its parent.  A child can lose
        // its final parent during this pass, hence the fixed-point loop.
        loop {
            let modes: BTreeMap<u64, u32> = items
                .iter()
                .filter(|item| item.key.item_type == INODE_ITEM && item.key.offset == 0)
                .map(|item| Ok((item.key.objectid, BtrfsInodeItem::decode(&item.value)?.mode)))
                .collect::<AxResult<_>>()?;
            if !modes.contains_key(&root_dirid) {
                return Err(AxError::Io);
            }
            let mut direct: BTreeMap<u64, usize> = BTreeMap::new();
            for item in items
                .iter()
                .filter(|item| item.key.item_type == super::DIR_INDEX)
            {
                let entries = decode_dir_items(&item.value)?;
                if entries.len() != 1
                    || !modes.contains_key(&item.key.objectid)
                    || !modes.contains_key(&entries[0].inode)
                {
                    return Err(AxError::Io);
                }
                let count = direct.entry(entries[0].inode).or_insert(0);
                *count = count.checked_add(1).ok_or(AxError::NoMemory)?;
            }
            let dirs: Vec<u64> = orphan_candidates
                .iter()
                .filter_map(|candidate| {
                    let inode = candidate.inode;
                    modes
                        .get(&inode)
                        .is_some_and(|mode| *mode & 0o170000 == 0o040000)
                        .then_some(inode)
                        .filter(|inode| *inode != root_dirid && !direct.contains_key(inode))
                })
                .collect();
            if dirs.is_empty() {
                break;
            }
            let mut removed = false;
            for directory in dirs {
                let children: Vec<(u64, Vec<u8>, u64, u8, u64, u8)> = items
                    .iter()
                    .filter(|item| {
                        item.key.item_type == super::DIR_INDEX && item.key.objectid == directory
                    })
                    .map(|item| {
                        let entries = decode_dir_items(&item.value)?;
                        if entries.len() != 1 {
                            return Err(AxError::Io);
                        }
                        Ok((
                            item.key.offset,
                            entries[0].name.clone(),
                            entries[0].inode,
                            entries[0].location_type,
                            entries[0].location_offset,
                            entries[0].item_type,
                        ))
                    })
                    .collect::<AxResult<_>>()?;
                for (index, name, inode, location_type, location_offset, item_type) in children {
                    Self::unlink_replayed_namespace_entry(
                        items,
                        directory,
                        index,
                        &name,
                        inode,
                        location_type,
                        location_offset,
                        item_type,
                    )?;
                    orphan_candidates.insert(ReplayOrphanCandidate::new(inode)?);
                    removed = true;
                }
            }
            if !removed {
                break;
            }
        }

        Self::fixup_replayed_link_counts(items)?;
        let modes: BTreeMap<u64, u32> = items
            .iter()
            .filter(|item| item.key.item_type == INODE_ITEM && item.key.offset == 0)
            .map(|item| Ok((item.key.objectid, BtrfsInodeItem::decode(&item.value)?.mode)))
            .collect::<AxResult<_>>()?;
        let mut direct: BTreeMap<u64, usize> = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == super::DIR_INDEX)
        {
            let entries = decode_dir_items(&item.value)?;
            if entries.len() != 1 {
                return Err(AxError::Io);
            }
            let count = direct.entry(entries[0].inode).or_insert(0);
            *count = count.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        // Native markers already present in the home image are not log input:
        // they are a previous transaction's recovery state.  They may only
        // name an inode through the exact (-5, 48, inode) empty form.
        let mut orphan_inodes = BTreeSet::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == ORPHAN_ITEM)
        {
            if item.key.objectid != OrphanRetirement::OBJECTID
                || item.key.offset == 0
                || !item.value.is_empty()
            {
                return Err(AxError::Io);
            }
            orphan_inodes.insert(item.key.offset);
        }
        for candidate in orphan_candidates.iter().copied() {
            let inode = candidate.inode;
            if inode != root_dirid && modes.contains_key(&inode) && !direct.contains_key(&inode) {
                orphan_inodes.insert(inode);
            }
        }

        for inode in orphan_inodes {
            let orphan = OrphanRetirement::new(root, inode)?;
            // This marker is generated from the final namespace only.  It is
            // deliberately staged before cleanup to mirror native orphan
            // ordering, but removed from the final COW image on completion.
            Self::set_recovery_item(items, orphan.marker_key(), Vec::new())?;
            // A stale marker must not delete a relinked inode, and one for an
            // already absent inode has no inode-scoped state left to retire.
            if inode == root_dirid || modes.get(&inode).is_none() || direct.contains_key(&inode) {
                let _ = Self::delete_recovery_item(items, orphan.marker_key())?;
                continue;
            }
            let extents: Vec<(TreeItemKey, Vec<u8>)> = items
                .iter()
                .filter(|item| {
                    item.key.objectid == orphan.inode && item.key.item_type == EXTENT_DATA
                })
                .map(|item| (item.key, item.value.clone()))
                .collect();
            for (key, value) in extents {
                let retirement = LoggedExtentRetirement::decode(orphan.root, key, &value)?;
                let released = accounting.prepare_logged_extent_retirement(
                    &retirement,
                    generation,
                    sector,
                    free_space,
                )?;
                freed = freed.checked_add(released).ok_or(AxError::NoMemory)?;
            }
            // A directory was emptied through unlink_replayed_namespace_entry;
            // any residual name record therefore means an unsafe/corrupt
            // graph rather than a safe inode-scoped cleanup.
            if items.iter().any(|item| {
                item.key.objectid == orphan.inode
                    && matches!(item.key.item_type, DIR_ITEM | super::DIR_INDEX)
            }) {
                return Err(AxError::Io);
            }
            let delete: Vec<TreeItemKey> = items
                .iter()
                .filter_map(|item| {
                    (item.key.objectid == orphan.inode
                        && matches!(
                            item.key.item_type,
                            INODE_ITEM | INODE_REF | super::INODE_EXTREF | XATTR_ITEM | EXTENT_DATA
                        ))
                    .then_some(item.key)
                })
                .collect();
            for key in delete {
                let _ = Self::delete_recovery_item(items, key)?;
            }
            let _ = Self::delete_recovery_item(items, orphan.marker_key())?;
        }
        Ok(freed)
    }

    /// Validates the bidirectional namespace projection after log records
    /// have been merged but before the COW planner reserves a node.  A log
    /// replay must never publish only one half of a name: every DIR_ITEM is
    /// paired with exactly one DIR_INDEX and one inode backref (ordinary or
    /// extended), and every backref resolves back to that same name/index.
    fn validate_replayed_namespace(items: &[RawTreeItem]) -> AxResult<()> {
        fn expected_dir_type(mode: u32) -> AxResult<u8> {
            match mode & 0o170000 {
                0o100000 => Ok(1),
                0o040000 => Ok(2),
                0o020000 => Ok(3),
                0o060000 => Ok(4),
                0o010000 => Ok(5),
                0o140000 => Ok(6),
                0o120000 => Ok(7),
                _ => Err(AxError::Io),
            }
        }
        type Link = (u64, u64, Vec<u8>, u64, u8);
        let mut links: Vec<Link> = Vec::new();
        let mut inode_ids = BTreeSet::new();
        let mut inodes = BTreeMap::new();
        for item in items {
            if item.key.item_type == INODE_ITEM && item.key.offset == 0 {
                let inode = BtrfsInodeItem::decode(&item.value)?;
                if !inode_ids.insert(item.key.objectid) {
                    return Err(AxError::Io);
                }
                inodes.insert(item.key.objectid, inode);
            }
        }
        for item in items
            .iter()
            .filter(|item| item.key.item_type == super::DIR_INDEX)
        {
            let entries = decode_dir_items(&item.value)?;
            if entries.len() != 1 {
                return Err(AxError::Io);
            }
            let entry = &entries[0];
            if entry.location_type != INODE_ITEM
                || entry.location_offset != 0
                || !inode_ids.contains(&entry.inode)
                || entry.item_type
                    != expected_dir_type(inodes.get(&entry.inode).ok_or(AxError::Io)?.mode)?
            {
                return Err(AxError::Io);
            }
            links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            links.push((
                item.key.objectid,
                item.key.offset,
                entry.name.clone(),
                entry.inode,
                entry.item_type,
            ));
        }
        links.sort();
        if links
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1)
        {
            return Err(AxError::Io);
        }
        for item in items.iter().filter(|item| item.key.item_type == DIR_ITEM) {
            for entry in decode_dir_items(&item.value)? {
                if u64::from(crc32c(&entry.name)) != item.key.offset
                    || entry.location_type != INODE_ITEM
                    || entry.location_offset != 0
                {
                    return Err(AxError::Io);
                }
                let matches = links
                    .iter()
                    .filter(|(parent, _, name, inode, kind)| {
                        *parent == item.key.objectid
                            && name.as_slice() == entry.name.as_slice()
                            && *inode == entry.inode
                            && *kind == entry.item_type
                    })
                    .count();
                if matches != 1 {
                    return Err(AxError::Io);
                }
            }
        }
        for (parent, index, name, inode, _) in &links {
            let hash = u64::from(crc32c(name));
            let item = items
                .binary_search_by_key(
                    &TreeItemKey {
                        objectid: *parent,
                        item_type: DIR_ITEM,
                        offset: hash,
                    },
                    |item| item.key,
                )
                .ok()
                .map(|index| &items[index])
                .ok_or(AxError::Io)?;
            if !decode_dir_items(&item.value)?
                .iter()
                .any(|entry| entry.inode == *inode && entry.name.as_slice() == name.as_slice())
            {
                return Err(AxError::Io);
            }
            let ordinary = TreeItemKey {
                objectid: *inode,
                item_type: INODE_REF,
                offset: *parent,
            };
            let mut backrefs = 0usize;
            if let Ok(position) = items.binary_search_by_key(&ordinary, |item| item.key) {
                backrefs += decode_inode_refs(&items[position].value)?
                    .iter()
                    .filter(|reference| {
                        reference.index == *index && reference.name.as_slice() == name.as_slice()
                    })
                    .count();
            }
            let extended = TreeItemKey {
                objectid: *inode,
                item_type: super::INODE_EXTREF,
                offset: btrfs_extref_hash(*parent, name),
            };
            if let Ok(position) = items.binary_search_by_key(&extended, |item| item.key) {
                backrefs += decode_inode_extrefs(&items[position].value)?
                    .iter()
                    .filter(|(ref_parent, ref_index, ref_name)| {
                        *ref_parent == *parent
                            && *ref_index == *index
                            && ref_name.as_slice() == name.as_slice()
                    })
                    .count();
            }
            if backrefs != 1 {
                return Err(AxError::Io);
            }
        }
        for item in items.iter().filter(|item| item.key.item_type == INODE_REF) {
            for reference in decode_inode_refs(&item.value)? {
                if links
                    .iter()
                    .filter(|(parent, entry_index, entry_name, inode, _)| {
                        *parent == item.key.offset
                            && *entry_index == reference.index
                            && entry_name.as_slice() == reference.name.as_slice()
                            && *inode == item.key.objectid
                    })
                    .count()
                    != 1
                {
                    return Err(AxError::Io);
                }
            }
        }
        for item in items
            .iter()
            .filter(|item| item.key.item_type == super::INODE_EXTREF)
        {
            for (parent, index, name) in decode_inode_extrefs(&item.value)? {
                if item.key.offset != btrfs_extref_hash(parent, &name)
                    || links
                        .iter()
                        .filter(|(entry_parent, entry_index, entry_name, inode, _)| {
                            *entry_parent == parent
                                && *entry_index == index
                                && entry_name.as_slice() == name.as_slice()
                                && *inode == item.key.objectid
                        })
                        .count()
                        != 1
                {
                    return Err(AxError::Io);
                }
            }
        }
        // Link counts are part of the same namespace transaction.  A
        // directory has its native baseline two links plus one for each
        // directly contained directory; ordinary inodes count their visible
        // parent/name relations exactly.
        for (&inode_id, inode) in &inodes {
            let direct = links
                .iter()
                .filter(|(_, _, _, child, _)| *child == inode_id)
                .count();
            let expected = if inode.mode & 0o170000 == 0o040000 {
                let children = links
                    .iter()
                    .filter(|(_, _, _, child, _)| {
                        inodes
                            .get(child)
                            .is_some_and(|child_inode| child_inode.mode & 0o170000 == 0o040000)
                    })
                    .count();
                2usize.checked_add(children).ok_or(AxError::NoMemory)?
            } else {
                direct
            };
            if u64::from(inode.nlink) != u64::try_from(expected).map_err(|_| AxError::NoMemory)? {
                return Err(AxError::Io);
            }
        }
        Ok(())
    }

    /// Recomputes native link counts from the merged namespace before the
    /// final graph assertion.  This is deliberately a repair pass after all
    /// range deletes and positive index records, never an incremental counter
    /// update whose ordering could observe a half-replayed rename.
    fn fixup_replayed_link_counts(items: &mut [RawTreeItem]) -> AxResult<()> {
        let mut links: Vec<(u64, u64)> = Vec::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == super::DIR_INDEX)
        {
            let entries = decode_dir_items(&item.value)?;
            if entries.len() != 1 {
                return Err(AxError::Io);
            }
            links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            links.push((item.key.objectid, entries[0].inode));
        }
        let modes: BTreeMap<u64, u32> = items
            .iter()
            .filter(|item| item.key.item_type == INODE_ITEM && item.key.offset == 0)
            .map(|item| Ok((item.key.objectid, BtrfsInodeItem::decode(&item.value)?.mode)))
            .collect::<AxResult<_>>()?;
        for item in items
            .iter_mut()
            .filter(|item| item.key.item_type == INODE_ITEM && item.key.offset == 0)
        {
            let mut inode = BtrfsInodeItem::decode(&item.value)?;
            let direct = links
                .iter()
                .filter(|(_, child)| *child == item.key.objectid)
                .count();
            let count = if inode.mode & 0o170000 == 0o040000 {
                2usize
                    .checked_add(
                        links
                            .iter()
                            .filter(|(_, child)| {
                                modes
                                    .get(child)
                                    .is_some_and(|mode| *mode & 0o170000 == 0o040000)
                            })
                            .count(),
                    )
                    .ok_or(AxError::NoMemory)?
            } else {
                direct
            };
            inode.nlink = u32::try_from(count).map_err(|_| AxError::NoMemory)?;
            item.value = inode.encode();
        }
        Ok(())
    }

    fn apply_recovery_log_records(
        items: &mut Vec<RawTreeItem>,
        records: &[RecoveryLogItem],
        ordinary_limit: usize,
    ) -> AxResult<()> {
        for record in records {
            if record.key.item_type == DIR_LOG_INDEX {
                continue;
            }
            if record.key.item_type == INODE_ITEM {
                let logged = BtrfsInodeItem::decode(&record.value)?;
                match items.binary_search_by_key(&record.key, |item| item.key) {
                    Ok(index) => {
                        let current = BtrfsInodeItem::decode(&items[index].value)?;
                        // A zero-generation log entry is existence-only.  It
                        // must not regress a newer inode already present in
                        // the home tree.  Directory size is rebuilt only by
                        // the directory replay phase, so reject that variant
                        // rather than publishing a guessed size.
                        if logged.generation == 0 {
                            continue;
                        }
                        if current.mode & 0o170000 == 0o040000 || logged.mode & 0o170000 == 0o040000
                        {
                            return Err(AxError::Unsupported);
                        }
                        if logged.size != current.size || logged.nbytes != current.nbytes {
                            return Err(AxError::Unsupported);
                        }
                        items[index].value = record.value.clone();
                    }
                    Err(_) => {
                        if logged.generation == 0 || logged.mode & 0o170000 == 0o040000 {
                            return Err(AxError::Unsupported);
                        }
                        Self::set_recovery_item(items, record.key, record.value.clone())?;
                    }
                }
            } else if record.key.item_type == INODE_REF {
                let logged = decode_inode_refs(&record.value)?;
                let existing = items
                    .binary_search_by_key(&record.key, |item| item.key)
                    .ok();
                let mut ordinary = existing
                    .map(|index| decode_inode_refs(&items[index].value))
                    .transpose()?
                    .unwrap_or_default();
                for reference in logged {
                    if ordinary.iter().any(|current| {
                        current.name == reference.name && current.index != reference.index
                    }) {
                        return Err(AxError::Io);
                    }
                    if ordinary.iter().any(|current| current == &reference) {
                        continue;
                    }
                    let mut candidate = ordinary.clone();
                    candidate.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    candidate.push(reference.clone());
                    if ordinary_limit >= 10
                        && encode_inode_refs(&candidate)?.len() <= ordinary_limit
                    {
                        ordinary = candidate;
                        continue;
                    }
                    let ext_key = TreeItemKey {
                        objectid: record.key.objectid,
                        item_type: super::INODE_EXTREF,
                        offset: btrfs_extref_hash(record.key.offset, &reference.name),
                    };
                    let mut extrefs = match items.binary_search_by_key(&ext_key, |item| item.key) {
                        Ok(index) => decode_inode_extrefs(&items[index].value)?,
                        Err(_) => Vec::new(),
                    };
                    if extrefs.iter().any(|(parent, index, name)| {
                        *parent == record.key.offset
                            && name == &reference.name
                            && *index != reference.index
                    }) {
                        return Err(AxError::Io);
                    }
                    if !extrefs.iter().any(|(parent, index, name)| {
                        *parent == record.key.offset
                            && *index == reference.index
                            && name == &reference.name
                    }) {
                        extrefs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                        extrefs.push((record.key.offset, reference.index, reference.name));
                        Self::set_recovery_item(items, ext_key, encode_inode_extrefs(&extrefs)?)?;
                    }
                }
                if !ordinary.is_empty() {
                    Self::set_recovery_item(items, record.key, encode_inode_refs(&ordinary)?)?;
                }
            } else if record.key.item_type == super::INODE_EXTREF {
                let logged = decode_inode_extrefs(&record.value)?;
                let merged = match items.binary_search_by_key(&record.key, |item| item.key) {
                    Ok(index) => {
                        let mut refs = decode_inode_extrefs(&items[index].value)?;
                        for reference in logged {
                            if refs.iter().any(|existing| {
                                existing.0 == reference.0
                                    && existing.2 == reference.2
                                    && existing.1 != reference.1
                            }) {
                                return Err(AxError::Io);
                            }
                            if !refs.iter().any(|existing| existing == &reference) {
                                refs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                                refs.push(reference);
                            }
                        }
                        encode_inode_extrefs(&refs)?
                    }
                    Err(_) => encode_inode_extrefs(&logged)?,
                };
                Self::set_recovery_item(items, record.key, merged)?;
            } else {
                Self::set_recovery_item(items, record.key, record.value.clone())?;
            }
        }
        Ok(())
    }

    /// A positive DIR_INDEX is a namespace edge, not merely an ordered
    /// directory payload.  Linux's replay path materializes its inode
    /// backref even when the log did not carry a separate INODE_REF item.
    /// Do that while the image is still private to the multi-root planner so
    /// final graph validation checks a completed namespace instead of being
    /// asked to diagnose a derivable omission.
    fn materialize_logged_dir_backrefs(
        items: &mut Vec<RawTreeItem>,
        records: &[RecoveryLogItem],
        ordinary_limit: usize,
    ) -> AxResult<()> {
        if ordinary_limit < 10 {
            return Err(AxError::Io);
        }
        let mut seen = BTreeSet::new();
        for record in records
            .iter()
            .filter(|record| record.key.item_type == super::DIR_INDEX)
        {
            if !seen.insert(record.key) {
                return Err(AxError::Io);
            }
            let logged = decode_dir_items(&record.value)?;
            if logged.len() != 1 {
                return Err(AxError::Io);
            }
            let entry = &logged[0];
            let current_index = items
                .binary_search_by_key(&record.key, |item| item.key)
                .map_err(|_| AxError::Io)?;
            let current = decode_dir_items(&items[current_index].value)?;
            if current.len() != 1 || current[0] != *entry {
                return Err(AxError::Io);
            }

            let regular_key = TreeItemKey {
                objectid: entry.inode,
                item_type: INODE_REF,
                offset: record.key.objectid,
            };
            let regular = match items.binary_search_by_key(&regular_key, |item| item.key) {
                Ok(position) => Some(decode_inode_refs(&items[position].value)?),
                Err(_) => None,
            };
            let ext_key = TreeItemKey {
                objectid: entry.inode,
                item_type: super::INODE_EXTREF,
                offset: btrfs_extref_hash(record.key.objectid, &entry.name),
            };
            let extrefs = match items.binary_search_by_key(&ext_key, |item| item.key) {
                Ok(position) => Some(decode_inode_extrefs(&items[position].value)?),
                Err(_) => None,
            };
            let regular_match_count = regular
                .as_ref()
                .map(|refs| {
                    refs.iter()
                        .filter(|reference| {
                            reference.index == record.key.offset
                                && reference.name.as_slice() == entry.name.as_slice()
                        })
                        .count()
                })
                .unwrap_or(0);
            if regular_match_count > 1 {
                return Err(AxError::Io);
            }
            let regular_matches = regular_match_count == 1;
            let ext_match_count = extrefs
                .as_ref()
                .map(|refs| {
                    refs.iter()
                        .filter(|(parent, index, name)| {
                            *parent == record.key.objectid
                                && *index == record.key.offset
                                && name.as_slice() == entry.name.as_slice()
                        })
                        .count()
                })
                .unwrap_or(0);
            if ext_match_count > 1
                || extrefs.as_ref().is_some_and(|refs| {
                    refs.iter().any(|(parent, index, name)| {
                        *parent == record.key.objectid
                            && name.as_slice() == entry.name.as_slice()
                            && *index != record.key.offset
                    })
                })
            {
                return Err(AxError::Io);
            }
            let ext_matches = ext_match_count == 1;
            if regular_matches && ext_matches {
                return Err(AxError::Io);
            }
            if regular_matches || ext_matches {
                continue;
            }

            // The ordinary key is a packed alias bucket.  Extend it first;
            // only an item too large for one leaf uses EXTREF fallback.
            let mut ordinary = regular.unwrap_or_default();
            ordinary.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            ordinary.push(BtrfsInodeRef {
                index: record.key.offset,
                name: entry.name.clone(),
            });
            let encoded_ordinary = encode_inode_refs(&ordinary)?;
            if encoded_ordinary.len() <= ordinary_limit {
                Self::set_recovery_item(items, regular_key, encoded_ordinary)?;
                continue;
            }
            let mut refs = extrefs.unwrap_or_default();
            if refs.iter().any(|(parent, index, name)| {
                *parent == record.key.objectid
                    && name.as_slice() == entry.name.as_slice()
                    && *index != record.key.offset
            }) {
                return Err(AxError::Io);
            }
            refs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            refs.push((record.key.objectid, record.key.offset, entry.name.clone()));
            Self::set_recovery_item(items, ext_key, encode_inode_extrefs(&refs)?)?;
        }
        Ok(())
    }

    /// Builds all recovery images before media publication.  Every log root
    /// and item is decoded first.  It then builds exactly one transaction and
    /// exactly one image for every shared accounting tree; independently
    /// planned per-subvolume transactions would each start from stale extent
    /// and free-space state and could not be committed together safely.
    pub fn multi_root_replay_planner(&self) -> AxResult<BtrfsMultiRootReplayPlanner> {
        let log_roots = self.recovery_log_roots()?;
        let roots = self.recovery_filesystem_roots()?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(roots.len())
            .map_err(|_| AxError::NoMemory)?;
        for root in roots.iter().copied() {
            let records = match log_roots
                .iter()
                .find(|logged| logged.subvolume == root.subvolume)
                .copied()
            {
                Some(logged) => self.recovery_log_items_for_root(logged)?,
                None => Vec::new(),
            };
            Self::validate_recovery_log_records(&records)?;
            pending.push((root, records));
        }

        let first = pending.first().ok_or(AxError::Io)?.0;
        // This one planner owns the Extent/Csum/FreeSpace/Quota images for
        // every log root.  Per-subvolume images can differ, but they cannot
        // independently publish a physical-accounting snapshot.
        let mut accounting = self.mutation_planner(first.subvolume)?;
        let free_space =
            self.logical_allocator_from_items(accounting.tree_items(TreeId::FreeSpace as u64)?)?;
        let generation = self
            .superblock
            .generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let sector = u64::from(self.superblock.sectorsize);
        let mut freed_bytes = 0u64;
        let mut extra_rewrites = Vec::new();
        extra_rewrites
            .try_reserve_exact(pending.len().saturating_sub(1))
            .map_err(|_| AxError::NoMemory)?;

        for (index, (root, records)) in pending.into_iter().enumerate() {
            let fs_root = self.subvolume_root(root.subvolume)?;
            let image = self.volume.read_checked_tree_block(
                fs_root,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let old_tree_owner = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                fs_root,
            )?
            .owner();
            if old_tree_owner == 0 {
                return Err(AxError::Io);
            }
            let home_items = if index == 0 {
                accounting.tree_items(root.subvolume)?.to_vec()
            } else {
                let mut items = Vec::new();
                self.collect_tree_items(fs_root, old_tree_owner, &mut BTreeSet::new(), &mut items)?;
                items
            };
            if home_items.is_empty() {
                return Err(AxError::Io);
            }
            let mut items = home_items.clone();
            let mut orphan_candidates = BTreeSet::new();
            for record in records
                .iter()
                .filter(|record| record.key.item_type == EXTENT_DATA)
            {
                let old = home_items
                    .binary_search_by_key(&record.key, |item| item.key)
                    .ok()
                    .map(|entry| home_items[entry].value.as_slice());
                let transition =
                    LoggedExtentTransition::decode(root.subvolume, record.key, old, &record.value)?;
                let checksums = if !transition.same_physical_mapping()
                    && LoggedExtentTransition::requires_physical_accounting(&transition.new)
                {
                    self.logged_extent_checksums(&transition.new)?
                } else {
                    None
                };
                accounting.prepare_logged_extent_transition(
                    &transition,
                    generation,
                    sector,
                    &free_space,
                    checksums.as_deref(),
                )?;
            }
            Self::apply_dir_log_ranges(&mut items, &records, &mut orphan_candidates)?;
            let ordinary_limit = usize::try_from(self.superblock.nodesize)
                .map_err(|_| AxError::Io)?
                .checked_sub(0x65 + 25)
                .ok_or(AxError::Io)?;
            Self::apply_recovery_log_records(&mut items, &records, ordinary_limit)?;
            Self::materialize_logged_dir_backrefs(&mut items, &records, ordinary_limit)?;
            Self::fixup_replayed_link_counts(&mut items)?;
            let released = Self::plan_generated_orphans(
                &mut items,
                root.subvolume,
                root.root_dirid,
                &mut accounting,
                generation,
                sector,
                &free_space,
                &mut orphan_candidates,
            )?;
            freed_bytes = freed_bytes.checked_add(released).ok_or(AxError::NoMemory)?;
            Self::validate_replayed_namespace(&items)?;
            if index == 0 {
                // `items` is the complete merged filesystem image, not a
                // write-set.  Range deletion and orphan cleanup remove keys,
                // so publish the negative half before additions/replacements
                // rather than silently retaining retired inode state.
                for item in &home_items {
                    if items
                        .binary_search_by_key(&item.key, |entry| entry.key)
                        .is_err()
                    {
                        let _ = accounting.delete_item(root.subvolume, item.key)?;
                    }
                }
                for item in &items {
                    if home_items
                        .binary_search_by_key(&item.key, |entry| entry.key)
                        .ok()
                        .is_none_or(|old| home_items[old].value != item.value)
                    {
                        accounting.set_item(root.subvolume, item.key, item.value.clone())?;
                    }
                }
            } else {
                extra_rewrites.push(BtrfsTreeRewrite {
                    root_objectid: root.subvolume,
                    tree_owner: root.subvolume,
                    old_tree_owner,
                    items,
                });
            }
        }
        accounting.finish_logged_extent_accounting(&free_space)?;
        let (transaction, mut rewrites) = accounting.into_rewrites()?;
        rewrites.append(&mut extra_rewrites);
        rewrites.sort_by_key(|rewrite| rewrite.root_objectid);
        Ok(BtrfsMultiRootReplayPlanner {
            transaction,
            rewrites,
            log_roots,
            freed_bytes,
        })
    }

    /// Resolves a subvolume's filesystem-tree root through the root tree.
    /// The caller receives a checked logical bytenr, not an in-memory alias.
    pub fn subvolume_root(&self, subvolume: u64) -> AxResult<u64> {
        let key = TreeItemKey {
            objectid: subvolume,
            item_type: ROOT_ITEM,
            offset: 0,
        };
        let value = self
            .lookup(self.superblock.root, TreeId::Root as u64, key)?
            .ok_or(AxError::NotFound)?;
        BtrfsRootItem::decode(&value).map(|item| item.bytenr)
    }

    pub fn root_tree_items(&self) -> AxResult<Vec<RawTreeItem>> {
        let mut items = Vec::new();
        self.collect_tree_items(
            self.superblock.root,
            TreeId::Root as u64,
            &mut BTreeSet::new(),
            &mut items,
        )?;
        Ok(items)
    }

    /// Replaces exactly one root-tree root-item pointer in a complete COW
    /// root-tree image.  It rejects an absent or duplicated target instead of
    /// accidentally creating a second subvolume identity.
    pub fn replace_subvolume_root_item(
        items: &mut [RawTreeItem],
        subvolume: u64,
        new_root: u64,
        generation: u64,
    ) -> AxResult<()> {
        let key = TreeItemKey {
            objectid: subvolume,
            item_type: ROOT_ITEM,
            offset: 0,
        };
        let index = items
            .iter()
            .position(|item| item.key == key)
            .ok_or(AxError::NotFound)?;
        if items[index + 1..].iter().any(|item| item.key == key) {
            return Err(AxError::Io);
        }
        let item = &mut items[index];
        item.value = BtrfsRootItem::replace_root(&item.value, new_root, generation)?;
        Ok(())
    }

    /// Imports the v2 free-space tree into a logical allocator.  A block
    /// group is allowed to use either extent records or the native bitmap
    /// representation; treating the latter as an empty cache turns a normal
    /// fragmented filesystem into an allocator that can overwrite live data.
    pub fn logical_allocator(&self) -> AxResult<BtrfsLogicalAllocator> {
        const FREE_SPACE_TREE: u64 = TreeId::FreeSpace as u64;
        let root = match self.subvolume_root(FREE_SPACE_TREE) {
            Ok(root) => root,
            Err(AxError::NotFound) => return self.legacy_logical_allocator(),
            Err(error) => return Err(error),
        };
        let mut items = Vec::new();
        self.collect_tree_items(root, FREE_SPACE_TREE, &mut BTreeSet::new(), &mut items)?;
        let allocator = BtrfsLogicalAllocator::with_core_lease(self.core.clone())?;
        let count = Self::import_free_space_items(
            &allocator,
            &items,
            u64::from(self.superblock.sectorsize),
        )?;
        if count == 0 {
            return Err(AxError::OperationNotSupported);
        }
        Ok(allocator)
    }

    /// Imports the pre-v2 space-cache files.  The descriptor is an extent
    /// tree-style special item in the default fs tree; its cache inode is
    /// decoded through the ordinary native extent reader so multi-extent
    /// cache files are not truncated to their first page.
    fn legacy_logical_allocator(&self) -> AxResult<BtrfsLogicalAllocator> {
        const FREE_SPACE_OBJECTID: u64 = u64::MAX - 10;
        const HEADER_BYTES: usize = 48;
        const PAGE: usize = 4096;
        let fs_root = self.subvolume_root(TreeId::Fs as u64)?;
        let mut items = Vec::new();
        self.collect_tree_items(fs_root, TreeId::Fs as u64, &mut BTreeSet::new(), &mut items)?;
        let allocator = BtrfsLogicalAllocator::with_core_lease(self.core.clone())?;
        let mut imported = 0usize;
        for header in items
            .iter()
            .filter(|item| item.key.objectid == FREE_SPACE_OBJECTID && item.key.item_type == 0)
        {
            if header.value.len() != HEADER_BYTES {
                return Err(AxError::Io);
            }
            let inode = u64::from_le_bytes(header.value[..8].try_into().map_err(|_| AxError::Io)?);
            let generation =
                u64::from_le_bytes(header.value[24..32].try_into().map_err(|_| AxError::Io)?);
            let entries = usize::try_from(u64::from_le_bytes(
                header.value[32..40].try_into().map_err(|_| AxError::Io)?,
            ))
            .map_err(|_| AxError::Io)?;
            let bitmaps = usize::try_from(u64::from_le_bytes(
                header.value[40..48].try_into().map_err(|_| AxError::Io)?,
            ))
            .map_err(|_| AxError::Io)?;
            if inode == 0 || generation > self.superblock.generation || entries < bitmaps {
                return Err(AxError::Io);
            }
            let size = usize::try_from(self.inode_item(fs_root, TreeId::Fs as u64, inode)?.size)
                .map_err(|_| AxError::Io)?;
            if size < PAGE || size % PAGE != 0 {
                return Err(AxError::Io);
            }
            let mut data = Vec::new();
            data.try_reserve_exact(size)
                .map_err(|_| AxError::NoMemory)?;
            data.resize(size, 0);
            if self.read_file_at(fs_root, TreeId::Fs as u64, inode, 0, &mut data)? != size {
                return Err(AxError::Io);
            }
            let pages = size / PAGE;
            // Page zero stores one little-endian CRC32C per payload page.
            // Do the bound check before trusting either on-disk count.
            let record_bytes = entries.checked_mul(17).ok_or(AxError::Io)?;
            if record_bytes > (pages - 1).checked_mul(PAGE).ok_or(AxError::Io)?
                || pages - 1 > PAGE / 4
            {
                return Err(AxError::Io);
            }
            for page in 1..pages {
                let stored = u32::from_le_bytes(
                    data[(page - 1) * 4..page * 4]
                        .try_into()
                        .map_err(|_| AxError::Io)?,
                );
                if stored != crc32c(&data[page * PAGE..(page + 1) * PAGE]) {
                    return Err(AxError::Io);
                }
            }
            let records = entries
                .checked_mul(17)
                .and_then(|n| PAGE.checked_add(n))
                .ok_or(AxError::Io)?;
            if records > data.len() {
                return Err(AxError::Io);
            }
            let mut bitmap_cursor = records;
            for index in 0..entries {
                let at = PAGE
                    .checked_add(index.checked_mul(17).ok_or(AxError::Io)?)
                    .ok_or(AxError::Io)?;
                let offset =
                    u64::from_le_bytes(data[at..at + 8].try_into().map_err(|_| AxError::Io)?);
                let bytes =
                    u64::from_le_bytes(data[at + 8..at + 16].try_into().map_err(|_| AxError::Io)?);
                match data[at + 16] {
                    1 => {
                        allocator.add_free(offset, bytes)?;
                        imported = imported.checked_add(1).ok_or(AxError::NoMemory)?;
                    }
                    2 => {
                        let sectors = bytes
                            .checked_div(u64::from(self.superblock.sectorsize))
                            .ok_or(AxError::Io)?;
                        let bitmap_bytes =
                            usize::try_from(sectors.div_ceil(8)).map_err(|_| AxError::Io)?;
                        let bitmap = data
                            .get(
                                bitmap_cursor
                                    ..bitmap_cursor.checked_add(bitmap_bytes).ok_or(AxError::Io)?,
                            )
                            .ok_or(AxError::Io)?;
                        bitmap_cursor =
                            bitmap_cursor.checked_add(bitmap_bytes).ok_or(AxError::Io)?;
                        for bit in 0..sectors {
                            if bitmap[usize::try_from(bit / 8).map_err(|_| AxError::Io)?]
                                & (1 << (bit % 8))
                                != 0
                            {
                                allocator.add_free(
                                    offset
                                        .checked_add(
                                            bit.checked_mul(u64::from(self.superblock.sectorsize))
                                                .ok_or(AxError::Io)?,
                                        )
                                        .ok_or(AxError::Io)?,
                                    u64::from(self.superblock.sectorsize),
                                )?;
                                imported = imported.checked_add(1).ok_or(AxError::NoMemory)?;
                            }
                        }
                    }
                    _ => return Err(AxError::Io),
                }
            }
            if bitmap_cursor > data.len() {
                return Err(AxError::Io);
            }
        }
        if imported == 0 {
            return Err(AxError::OperationNotSupported);
        }
        Ok(allocator)
    }

    fn logical_allocator_from_items(
        &self,
        items: &[RawTreeItem],
    ) -> AxResult<BtrfsLogicalAllocator> {
        let allocator = BtrfsLogicalAllocator::with_core_lease(self.core.clone())?;
        let count = Self::import_free_space_items(
            &allocator,
            items,
            u64::from(self.superblock.sectorsize),
        )?;
        if count == 0 {
            return Err(AxError::OperationNotSupported);
        }
        Ok(allocator)
    }

    /// Decodes both legal v2 free-space encodings.  Bitmap keys describe the
    /// logical window directly: `(start, FREE_SPACE_BITMAP, length)`, with
    /// one little-endian bit per sector.  The final partial byte must not
    /// advertise space outside the key window.
    fn import_free_space_items(
        allocator: &BtrfsLogicalAllocator,
        items: &[RawTreeItem],
        sector: u64,
    ) -> AxResult<usize> {
        if sector == 0 || !sector.is_power_of_two() {
            return Err(AxError::Io);
        }
        // `FREE_SPACE_INFO` is the authority for each block group.  Do not
        // accept orphan records or a mixed extent/bitmap representation.
        let mut groups: BTreeMap<u64, (u64, u32, u32)> = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == FREE_SPACE_INFO)
        {
            if item.value.len() != 8
                || item.key.objectid == 0
                || item.key.offset == 0
                || item.key.objectid % sector != 0
                || item.key.offset % sector != 0
            {
                return Err(AxError::Io);
            }
            let expected = u32::from_le_bytes(item.value[..4].try_into().map_err(|_| AxError::Io)?);
            let flags = u32::from_le_bytes(item.value[4..8].try_into().map_err(|_| AxError::Io)?);
            if flags & !1 != 0
                || groups
                    .insert(item.key.objectid, (item.key.offset, flags, expected))
                    .is_some()
            {
                return Err(AxError::Io);
            }
        }
        if groups.is_empty() {
            return Err(AxError::Io);
        }
        let group_for = |start: u64, len: u64| -> AxResult<(u64, u32)> {
            let (&base, &(size, flags, _)) =
                groups.range(..=start).next_back().ok_or(AxError::Io)?;
            if start
                .checked_add(len)
                .is_none_or(|end| end > base.saturating_add(size))
            {
                return Err(AxError::Io);
            }
            Ok((base, flags))
        };
        let mut actual: BTreeMap<u64, (u64, u32)> = BTreeMap::new();
        let mut count = 0usize;
        for item in items {
            match item.key.item_type {
                FREE_SPACE_EXTENT => {
                    if !item.value.is_empty()
                        || item.key.objectid == 0
                        || item.key.offset == 0
                        || item.key.objectid % sector != 0
                        || item.key.offset % sector != 0
                    {
                        return Err(AxError::Io);
                    }
                    let (group, flags) = group_for(item.key.objectid, item.key.offset)?;
                    if flags != 0 {
                        return Err(AxError::Io);
                    }
                    allocator.add_free(item.key.objectid, item.key.offset)?;
                    let runs = actual.entry(group).or_insert((item.key.objectid, 0));
                    runs.1 = runs.1.checked_add(1).ok_or(AxError::NoMemory)?;
                    count = count.checked_add(1).ok_or(AxError::NoMemory)?;
                }
                FREE_SPACE_BITMAP => {
                    let start = item.key.objectid;
                    let len = item.key.offset;
                    if start == 0 || len == 0 || start % sector != 0 || len % sector != 0 {
                        return Err(AxError::Io);
                    }
                    let bits = len / sector;
                    let bytes = usize::try_from(bits.div_ceil(8)).map_err(|_| AxError::Io)?;
                    if item.value.len() != bytes {
                        return Err(AxError::Io);
                    }
                    let (group, flags) = group_for(start, len)?;
                    if flags != 1 {
                        return Err(AxError::Io);
                    }
                    if bits % 8 != 0
                        && item
                            .value
                            .last()
                            .is_some_and(|byte| *byte >> (bits % 8) != 0)
                    {
                        return Err(AxError::Io);
                    }
                    let mut bit = 0u64;
                    while bit < bits {
                        let byte = item.value[usize::try_from(bit / 8).map_err(|_| AxError::Io)?];
                        if byte & (1 << (bit % 8)) == 0 {
                            bit += 1;
                            continue;
                        }
                        let first = bit;
                        loop {
                            bit += 1;
                            if bit == bits {
                                break;
                            }
                            let next =
                                item.value[usize::try_from(bit / 8).map_err(|_| AxError::Io)?];
                            if next & (1 << (bit % 8)) == 0 {
                                break;
                            }
                        }
                        let logical = start
                            .checked_add(first.checked_mul(sector).ok_or(AxError::Io)?)
                            .ok_or(AxError::Io)?;
                        let span = bit
                            .checked_sub(first)
                            .and_then(|n| n.checked_mul(sector))
                            .ok_or(AxError::Io)?;
                        allocator.add_free(logical, span)?;
                        let runs = actual.entry(group).or_insert((logical, 0));
                        if runs.1 == 0 || runs.0 != logical {
                            runs.1 = runs.1.checked_add(1).ok_or(AxError::NoMemory)?;
                        }
                        runs.0 = logical.checked_add(span).ok_or(AxError::Io)?;
                        count = count.checked_add(1).ok_or(AxError::NoMemory)?;
                    }
                }
                FREE_SPACE_INFO => {}
                _ => return Err(AxError::Io),
            }
        }
        for (&base, &(_, _, expected)) in &groups {
            if actual.get(&base).map(|(_, runs)| *runs).unwrap_or(0) != expected {
                return Err(AxError::Io);
            }
        }
        Ok(count)
    }

    fn qgroup_usages_from_items(
        items: &[RawTreeItem],
    ) -> AxResult<BTreeMap<super::QgroupId, (u64, u64)>> {
        let mut usages = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == QGROUP_INFO)
        {
            if item.key.objectid != 0 || item.value.len() != 40 {
                return Err(AxError::Io);
            }
            let level = u16::try_from(item.key.offset >> 48).map_err(|_| AxError::Io)?;
            let id = item.key.offset & ((1u64 << 48) - 1);
            let referenced =
                u64::from_le_bytes(item.value[8..16].try_into().map_err(|_| AxError::Io)?);
            let exclusive =
                u64::from_le_bytes(item.value[24..32].try_into().map_err(|_| AxError::Io)?);
            if exclusive > referenced
                || usages
                    .insert(super::QgroupId { level, id }, (referenced, exclusive))
                    .is_some()
            {
                return Err(AxError::Io);
            }
        }
        Ok(usages)
    }

    fn qgroup_parents_from_items(
        items: &[RawTreeItem],
    ) -> AxResult<BTreeMap<super::QgroupId, Vec<super::QgroupId>>> {
        let mut parents = BTreeMap::new();
        let edges: BTreeSet<(u64, u64)> = items
            .iter()
            .filter(|item| item.key.item_type == QGROUP_RELATION)
            .map(|item| (item.key.objectid, item.key.offset))
            .collect();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == QGROUP_RELATION)
        {
            if !item.value.is_empty()
                || item.key.objectid == 0
                || !edges.contains(&(item.key.offset, item.key.objectid))
            {
                return Err(AxError::Io);
            }
            let decode = |raw: u64| -> AxResult<super::QgroupId> {
                Ok(super::QgroupId {
                    level: u16::try_from(raw >> 48).map_err(|_| AxError::Io)?,
                    id: raw & ((1u64 << 48) - 1),
                })
            };
            let child = decode(item.key.objectid)?;
            let parent = decode(item.key.offset)?;
            if child == parent {
                return Err(AxError::Io);
            }
            // The reciprocal record is visited too; retain only the
            // canonical lower-level → higher-level orientation and reject
            // same-level edges, which can never be a qgroup hierarchy.
            if child.level == parent.level {
                return Err(AxError::Io);
            }
            if child.level > parent.level {
                continue;
            }
            let entry = parents.entry(child).or_insert_with(Vec::new);
            if entry.contains(&parent) {
                return Err(AxError::Io);
            }
            entry.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            entry.push(parent);
        }
        Ok(parents)
    }

    fn data_ref_counts_from_items(
        items: &[RawTreeItem],
    ) -> AxResult<BTreeMap<(u64, u64, u64, u64, u64), u64>> {
        let mut lengths = BTreeMap::new();
        let mut extent_values = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == EXTENT_ITEM)
        {
            if item.key.objectid == 0
                || item.key.offset == 0
                || lengths.insert(item.key.objectid, item.key.offset).is_some()
                || extent_values
                    .insert(item.key.objectid, &item.value)
                    .is_some()
            {
                return Err(AxError::Io);
            }
        }
        let mut refs = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == EXTENT_DATA_REF)
        {
            let (root, owner, offset, count) = super::decode_extent_data_ref(&item.value)?;
            let len = lengths
                .get(&item.key.objectid)
                .copied()
                .ok_or(AxError::Io)?;
            if count == 0 {
                return Err(AxError::Io);
            }
            let identity = (item.key.objectid, len, root, owner, offset);
            let total = refs.entry(identity).or_insert(0u64);
            *total = (*total)
                .checked_add(u64::from(count))
                .ok_or(AxError::NoMemory)?;
        }
        let mut totals = BTreeMap::new();
        for (&(bytenr, _, _, _, _), &count) in &refs {
            let total = totals.entry(bytenr).or_insert(0u64);
            *total = total.checked_add(count).ok_or(AxError::NoMemory)?;
        }
        for (&bytenr, &count) in &totals {
            let value = extent_values.get(&bytenr).copied().ok_or(AxError::Io)?;
            if value.len() < 8
                || u64::from_le_bytes(value[..8].try_into().map_err(|_| AxError::Io)?) != count
            {
                return Err(AxError::Io);
            }
        }
        Ok(refs)
    }

    /// The supplied Extent image is authoritative only if its complete data
    /// relation delta equals the transaction's delayed data-ref journal in
    /// both directions.  This closes the former one-way admission hole where
    /// an extent item could be edited without a matching delayed ref (or a
    /// delayed ref could advance core accounting without on-media evidence).
    fn validate_delayed_data_refs(
        transaction: &super::BtrfsTransaction,
        before: &[RawTreeItem],
        after: &[RawTreeItem],
    ) -> AxResult<()> {
        let before = Self::data_ref_counts_from_items(before)?;
        let after = Self::data_ref_counts_from_items(after)?;
        let mut expected = BTreeMap::new();
        for key in before.keys().chain(after.keys()) {
            let old = before.get(key).copied().unwrap_or(0);
            let new = after.get(key).copied().unwrap_or(0);
            let delta = i128::from(new) - i128::from(old);
            if delta != 0 {
                expected.insert(*key, delta);
            }
        }
        let mut actual = BTreeMap::new();
        for reference in transaction.delayed_refs() {
            let super::DelayedRefIdentity::Data { file_offset } = reference.identity else {
                continue;
            };
            let key = (
                reference.bytenr,
                reference.len,
                reference.root,
                reference.owner,
                file_offset,
            );
            // Only relation tuples represented by an EXTENT_DATA_REF belong
            // here.  TREE_BLOCK_REF relations share the generic delayed-ref
            // ledger but are reconciled by the fixed-point record writer.
            if before.contains_key(&key) || after.contains_key(&key) {
                let entry = actual.entry(key).or_insert(0i128);
                *entry = entry
                    .checked_add(i128::from(reference.delta))
                    .ok_or(AxError::NoMemory)?;
            }
        }
        actual.retain(|_, delta| *delta != 0);
        if actual != expected {
            return Err(AxError::Io);
        }
        Ok(())
    }

    /// Decodes the complete native metadata relation set.  A tree-block
    /// relationship has no payload: its identity is exactly
    /// `(bytenr, EXTENT_ITEM length, TREE_BLOCK_REF key.offset)`.  Keeping
    /// that identity separate from data backrefs is important because a
    /// metadata COW transaction may share a physical node between roots.
    fn tree_block_ref_set_from_items(items: &[RawTreeItem]) -> AxResult<BTreeSet<(u64, u64, u64)>> {
        let mut extents = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == EXTENT_ITEM)
        {
            if item.key.objectid == 0 || item.key.offset == 0 {
                return Err(AxError::Io);
            }
            if let Ok((references, _, _)) = decode_tree_extent_item(&item.value) {
                if extents
                    .insert(item.key.objectid, (item.key.offset, references))
                    .is_some()
                {
                    return Err(AxError::Io);
                }
            }
        }
        let mut relations = BTreeSet::new();
        let mut counts = BTreeMap::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == TREE_BLOCK_REF)
        {
            if item.key.objectid == 0 || item.key.offset == 0 {
                return Err(AxError::Io);
            }
            decode_tree_block_ref(&item.value)?;
            let (len, _) = extents
                .get(&item.key.objectid)
                .copied()
                .ok_or(AxError::Io)?;
            if !relations.insert((item.key.objectid, len, item.key.offset)) {
                return Err(AxError::Io);
            }
            let count = counts.entry(item.key.objectid).or_insert(0u64);
            *count = count.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        for (bytenr, (_, references)) in extents {
            if counts.get(&bytenr).copied().unwrap_or(0) != references {
                return Err(AxError::Io);
            }
        }
        Ok(relations)
    }

    /// The public delayed-ref journal and the final native TREE_BLOCK_REF
    /// key-set are two representations of the same relation mutation.  This
    /// is deliberately a two-way comparison: neither an orphaned on-media
    /// relation nor a ledger-only metadata reference may reach publication.
    fn validate_delayed_tree_block_refs(
        transaction: &super::BtrfsTransaction,
        before: &[RawTreeItem],
        after: &[RawTreeItem],
    ) -> AxResult<()> {
        let before = Self::tree_block_ref_set_from_items(before)?;
        let after = Self::tree_block_ref_set_from_items(after)?;
        let mut expected = BTreeMap::new();
        for relation in before.difference(&after) {
            expected.insert(*relation, -1i128);
        }
        for relation in after.difference(&before) {
            expected.insert(*relation, 1i128);
        }

        let mut actual = BTreeMap::new();
        for reference in transaction.delayed_refs() {
            if reference.identity != super::DelayedRefIdentity::TreeBlock {
                continue;
            }
            // TREE_BLOCK_REF key.offset carries root; there is no separate
            // owner field in its payload, so any noncanonical journal entry
            // cannot be represented faithfully and must fail admission.
            if reference.bytenr == 0
                || reference.len == 0
                || reference.root == 0
                || reference.owner != reference.root
            {
                return Err(AxError::Io);
            }
            let relation = (reference.bytenr, reference.len, reference.root);
            // A key-set delta has one direction per key.  Do not normalize a
            // public +1/-1 pair away: it has no native TREE_BLOCK_REF delta
            // to prove and would otherwise turn a journal-only mutation
            // into an invisible no-op at commit time.
            if actual
                .insert(relation, i128::from(reference.delta))
                .is_some()
            {
                return Err(AxError::Io);
            }
        }
        // A set relation can only be inserted or removed once per root in a
        // generation.  Reject count-like aliases even if an i128 sum happens
        // to fit, rather than treating them as a different tree-block ABI.
        if actual.values().any(|delta| !matches!(*delta, -1 | 1)) || actual != expected {
            return Err(AxError::Io);
        }
        Ok(())
    }

    fn validate_data_refs_not_free(
        extent_items: &[RawTreeItem],
        free_space_items: &[RawTreeItem],
        sector: u64,
    ) -> AxResult<()> {
        let refs = Self::data_ref_counts_from_items(extent_items)?;
        // Validation only enumerates the supplied image; it must not acquire
        // a live writer lease.
        let allocator = BtrfsLogicalAllocator::new();
        Self::import_free_space_items(&allocator, free_space_items, sector)?;
        for ((bytenr, len, _, _, _), count) in refs {
            if count == 0 {
                return Err(AxError::Io);
            }
            let end = bytenr.checked_add(len).ok_or(AxError::Io)?;
            for (free, free_len) in allocator.free_extents() {
                let free_end = free.checked_add(free_len).ok_or(AxError::Io)?;
                if bytenr < free_end && free < end {
                    return Err(AxError::Io);
                }
            }
        }
        Ok(())
    }

    /// Applies an already admitted qgroup delta to a complete quota-tree
    /// image.  Keeping this beside the fixed-point writer avoids advancing
    /// the in-memory transaction counters without making the corresponding
    /// QGROUP_INFO bytes part of the COW generation.
    fn apply_qgroup_delta_to_items(
        items: &mut Vec<RawTreeItem>,
        id: super::QgroupId,
        referenced: i128,
        exclusive: i128,
        generation: u64,
    ) -> AxResult<()> {
        if id.id >> 48 != 0 || generation == 0 {
            return Err(AxError::InvalidInput);
        }
        let objectid = (u64::from(id.level) << 48) | id.id;
        let key = TreeItemKey {
            objectid: 0,
            item_type: QGROUP_INFO,
            offset: objectid,
        };
        let index = items
            .binary_search_by_key(&key, |item| item.key)
            .map_err(|_| AxError::NotFound)?;
        let value = &mut items[index].value;
        if value.len() != 40 {
            return Err(AxError::Io);
        }
        let apply = |value: &mut [u8], offset: usize, delta: i128| -> AxResult<()> {
            let old = u64::from_le_bytes(
                value[offset..offset + 8]
                    .try_into()
                    .map_err(|_| AxError::Io)?,
            );
            let next = i128::from(old)
                .checked_add(delta)
                .ok_or(AxError::NoMemory)?;
            if !(0..=i128::from(u64::MAX)).contains(&next) {
                return Err(AxError::Io);
            }
            value[offset..offset + 8].copy_from_slice(&(next as u64).to_le_bytes());
            Ok(())
        };
        value[..8].copy_from_slice(&generation.to_le_bytes());
        apply(value, 8, referenced)?;
        apply(value, 16, referenced)?;
        apply(value, 24, exclusive)?;
        apply(value, 32, exclusive)
    }

    fn free_space_items_from_allocator(
        allocator: &BtrfsLogicalAllocator,
        mut items: Vec<RawTreeItem>,
        reclaim: &BTreeSet<u64>,
        node_bytes: u64,
        sector: u64,
        chunks: &[super::Chunk],
    ) -> AxResult<Vec<RawTreeItem>> {
        // Preserve the mounted representation for every block group.  Bitmap
        // groups are rebuilt as bounded 4KiB bitmap windows, matching the
        // Linux free-space-tree on-disk maximum and avoiding a giant bitmap
        // item that cannot fit in a metadata leaf.
        for item in &items {
            match item.key.item_type {
                FREE_SPACE_EXTENT => {
                    if !item.value.is_empty() {
                        return Err(AxError::Io);
                    }
                }
                FREE_SPACE_INFO => {
                    if item.value.len() != 8
                        || u32::from_le_bytes(item.value[4..8].try_into().map_err(|_| AxError::Io)?)
                            & !1
                            != 0
                    {
                        return Err(AxError::Io);
                    }
                }
                FREE_SPACE_BITMAP => {
                    if item.key.objectid == 0 || item.key.offset == 0 {
                        return Err(AxError::Io);
                    }
                }
                _ => return Err(AxError::Io),
            }
        }
        if sector == 0 || !sector.is_power_of_two() {
            return Err(AxError::Io);
        }
        let mut extents = allocator.free_extents();
        for &logical in reclaim {
            if logical == 0 || logical % sector != 0 || node_bytes % sector != 0 {
                return Err(AxError::Io);
            }
            extents.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            extents.push((logical, node_bytes));
        }
        extents.sort_by_key(|&(logical, _)| logical);
        for pair in extents.windows(2) {
            if pair[0]
                .0
                .checked_add(pair[0].1)
                .is_none_or(|end| end > pair[1].0)
            {
                return Err(AxError::Io);
            }
        }
        // The generic allocator intentionally coalesces neighbours.  Btrfs
        // free-space records, however, may never cross a block-group key.
        // Split merged ranges back at every chunk boundary before deriving
        // either the persistent records or INFO extent counts.
        let mut split = Vec::new();
        for (logical, len) in extents {
            let end = logical.checked_add(len).ok_or(AxError::Io)?;
            let mut cursor = logical;
            while cursor < end {
                let chunk = chunks
                    .iter()
                    .find(|chunk| {
                        cursor >= chunk.logical
                            && cursor < chunk.logical.saturating_add(chunk.length)
                    })
                    .ok_or(AxError::Io)?;
                let chunk_end = chunk.logical.checked_add(chunk.length).ok_or(AxError::Io)?;
                let span = (end.min(chunk_end))
                    .checked_sub(cursor)
                    .ok_or(AxError::Io)?;
                if span == 0 {
                    return Err(AxError::Io);
                }
                split.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                split.push((cursor, span));
                cursor = cursor.checked_add(span).ok_or(AxError::Io)?;
            }
        }
        let extents = split;
        let final_extents = extents.clone();
        let bitmap_groups: BTreeSet<u64> = items
            .iter()
            .filter_map(|item| {
                (item.key.item_type == FREE_SPACE_INFO
                    && item.value.len() == 8
                    && u32::from_le_bytes(item.value[4..8].try_into().ok()?) & 1 != 0)
                    .then_some(item.key.objectid)
            })
            .collect();
        items.clear();
        items
            .try_reserve(
                extents
                    .len()
                    .checked_add(reclaim.len())
                    .ok_or(AxError::NoMemory)?,
            )
            .map_err(|_| AxError::NoMemory)?;
        for (logical, len) in extents {
            if logical == 0 || len == 0 || logical % sector != 0 || len % sector != 0 {
                return Err(AxError::Io);
            }
            let group = chunks
                .iter()
                .find(|chunk| {
                    logical >= chunk.logical
                        && logical
                            .checked_add(len)
                            .is_some_and(|end| end <= chunk.logical.saturating_add(chunk.length))
                })
                .ok_or(AxError::Io)?;
            if !bitmap_groups.contains(&group.logical) {
                items.push(RawTreeItem {
                    key: TreeItemKey {
                        objectid: logical,
                        item_type: FREE_SPACE_EXTENT,
                        offset: len,
                    },
                    value: Vec::new(),
                });
            }
        }
        if items
            .iter()
            .all(|item| item.key.item_type != FREE_SPACE_EXTENT)
            && bitmap_groups.is_empty()
        {
            return Err(AxError::StorageFull);
        }
        let mut counts: BTreeMap<(u64, u64), (u64, u32)> = BTreeMap::new();
        for &(logical, len) in &final_extents {
            let extent_end = logical.checked_add(len).ok_or(AxError::Io)?;
            let group = chunks
                .iter()
                .find(|chunk| {
                    logical >= chunk.logical
                        && extent_end <= chunk.logical.saturating_add(chunk.length)
                })
                .ok_or(AxError::Io)?;
            let entry = counts
                .entry((group.logical, group.length))
                .or_insert((logical, 0));
            if entry.1 == 0 || entry.0 != logical {
                entry.1 = entry.1.checked_add(1).ok_or(AxError::NoMemory)?;
            }
            entry.0 = extent_end;
        }
        items
            .try_reserve(chunks.len())
            .map_err(|_| AxError::NoMemory)?;
        const BITMAP_BITS: u64 = 4096 * 8;
        for chunk in chunks {
            let bitmap = bitmap_groups.contains(&chunk.logical);
            let count = counts
                .get(&(chunk.logical, chunk.length))
                .map(|(_, count)| *count)
                .unwrap_or(0);
            let mut value = Vec::new();
            value.try_reserve_exact(8).map_err(|_| AxError::NoMemory)?;
            value.extend_from_slice(&count.to_le_bytes());
            value.extend_from_slice(&(if bitmap { 1u32 } else { 0 }).to_le_bytes());
            items.push(RawTreeItem {
                key: TreeItemKey {
                    objectid: chunk.logical,
                    item_type: FREE_SPACE_INFO,
                    offset: chunk.length,
                },
                value,
            });
            if bitmap {
                let bits = chunk.length / sector;
                if chunk.length == 0 || chunk.length % sector != 0 {
                    return Err(AxError::Io);
                }
                let mut bit_start = 0u64;
                while bit_start < bits {
                    let bit_count = (bits - bit_start).min(BITMAP_BITS);
                    let bytes =
                        usize::try_from(bit_count.div_ceil(8)).map_err(|_| AxError::NoMemory)?;
                    let mut value = Vec::new();
                    value
                        .try_reserve_exact(bytes)
                        .map_err(|_| AxError::NoMemory)?;
                    value.resize(bytes, 0);
                    let window_start = chunk
                        .logical
                        .checked_add(bit_start.checked_mul(sector).ok_or(AxError::Io)?)
                        .ok_or(AxError::Io)?;
                    let window_len = bit_count.checked_mul(sector).ok_or(AxError::Io)?;
                    for (free, len) in allocator.free_extents() {
                        let free_end = free.checked_add(len).ok_or(AxError::Io)?;
                        let window_end = window_start.checked_add(window_len).ok_or(AxError::Io)?;
                        let begin = free.max(window_start);
                        let end = free_end.min(window_end);
                        if begin >= end {
                            continue;
                        }
                        if begin % sector != 0 || end % sector != 0 {
                            return Err(AxError::Io);
                        }
                        let first = (begin - window_start) / sector;
                        let last = (end - window_start) / sector;
                        for bit in first..last {
                            let index = usize::try_from(bit / 8).map_err(|_| AxError::NoMemory)?;
                            value[index] |= 1 << (bit % 8);
                        }
                    }
                    for &logical in reclaim {
                        let end = logical.checked_add(node_bytes).ok_or(AxError::Io)?;
                        let window_end = window_start.checked_add(window_len).ok_or(AxError::Io)?;
                        let begin = logical.max(window_start);
                        let end = end.min(window_end);
                        if begin >= end {
                            continue;
                        }
                        if begin % sector != 0 || end % sector != 0 {
                            return Err(AxError::Io);
                        }
                        for bit in (begin - window_start) / sector..(end - window_start) / sector {
                            let index = usize::try_from(bit / 8).map_err(|_| AxError::NoMemory)?;
                            value[index] |= 1 << (bit % 8);
                        }
                    }
                    items.push(RawTreeItem {
                        key: TreeItemKey {
                            objectid: window_start,
                            item_type: FREE_SPACE_BITMAP,
                            offset: window_len,
                        },
                        value,
                    });
                    bit_start = bit_start.checked_add(bit_count).ok_or(AxError::NoMemory)?;
                }
            }
        }
        items.sort_by_key(|item| item.key);
        Ok(items)
    }

    /// Reserves one metadata-tree node from an already imported free-space
    /// allocator.  The selected logical range is checked against the mounted
    /// chunk map while the reservation is made, so a later COW writer cannot
    /// accidentally write B-tree blocks into a data-only block group.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn reserve_metadata_node(
        &self,
        allocator: &BtrfsLogicalAllocator,
    ) -> AxResult<LogicalReservation> {
        self.reserve_metadata_node_in_chunk(allocator, None)
    }

    /// Reserves a COW metadata node from one previously published
    /// metadata/system block group.  Balance uses this to guarantee that a
    /// replacement root never lands back on the member being evacuated.
    pub fn reserve_metadata_node_in_chunk(
        &self,
        allocator: &BtrfsLogicalAllocator,
        target: Option<(u64, u64)>,
    ) -> AxResult<LogicalReservation> {
        let bytes = u64::from(self.superblock.nodesize);
        allocator.reserve_where(bytes, bytes, |logical, len| {
            self.volume.metadata_contains(logical, len)
                && target.is_none_or(|(start, size)| {
                    logical >= start
                        && logical
                            .checked_add(len)
                            .is_some_and(|end| end <= start.saturating_add(size))
                })
        })
    }

    pub fn inode_item(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
    ) -> AxResult<BtrfsInodeItem> {
        let key = TreeItemKey {
            objectid: inode,
            item_type: INODE_ITEM,
            offset: 0,
        };
        let value = self
            .lookup(fs_root, tree_owner, key)?
            .ok_or(AxError::NotFound)?;
        BtrfsInodeItem::decode(&value)
    }

    /// Looks up one raw-byte name in a directory.  Btrfs hashes names into a
    /// DIR_ITEM key and stores collisions as packed items, so a matching hash
    /// alone is never accepted as a lookup result.
    pub fn lookup_dir_item(
        &self,
        fs_root: u64,
        tree_owner: u64,
        parent: u64,
        name: &[u8],
    ) -> AxResult<BtrfsDirItem> {
        if name.is_empty() || name.iter().any(|byte| *byte == b'/') {
            return Err(AxError::InvalidInput);
        }
        let key = TreeItemKey {
            objectid: parent,
            item_type: DIR_ITEM,
            offset: u64::from(crc32c(name)),
        };
        let value = self
            .lookup(fs_root, tree_owner, key)?
            .ok_or(AxError::NotFound)?;
        decode_dir_items(&value)?
            .into_iter()
            .find(|item| item.name == name)
            .ok_or(AxError::NotFound)
    }

    pub fn get_xattr(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        name: &[u8],
    ) -> AxResult<Vec<u8>> {
        if name.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let key = TreeItemKey {
            objectid: inode,
            item_type: XATTR_ITEM,
            offset: u64::from(crc32c(name)),
        };
        let value = self
            .lookup(fs_root, tree_owner, key)?
            .ok_or(AxError::NotFound)?;
        let item = decode_dir_items(&value)?
            .into_iter()
            .find(|item| item.name == name)
            .ok_or(AxError::NotFound)?;
        Ok(item.data)
    }

    pub fn list_xattrs(&self, fs_root: u64, tree_owner: u64, inode: u64) -> AxResult<Vec<u8>> {
        let mut items = Vec::new();
        self.collect_tree_items(fs_root, tree_owner, &mut BTreeSet::new(), &mut items)?;
        let mut result = Vec::new();
        for item in items {
            if item.key.objectid != inode || item.key.item_type != XATTR_ITEM {
                continue;
            }
            for xattr in decode_dir_items(&item.value)? {
                result
                    .try_reserve(xattr.name.len().checked_add(1).ok_or(AxError::NoMemory)?)
                    .map_err(|_| AxError::NoMemory)?;
                result.extend_from_slice(&xattr.name);
                result.push(0);
            }
        }
        Ok(result)
    }

    /// Returns all packed directory records in native key order.  It is used
    /// by getdents and deliberately includes hash collisions exactly once.
    pub fn directory_items(
        &self,
        fs_root: u64,
        tree_owner: u64,
        parent: u64,
    ) -> AxResult<Vec<BtrfsDirItem>> {
        let mut items = Vec::new();
        self.collect_tree_items(fs_root, tree_owner, &mut BTreeSet::new(), &mut items)?;
        let mut entries = Vec::new();
        for item in items {
            if item.key.objectid == parent && item.key.item_type == DIR_ITEM {
                let decoded = decode_dir_items(&item.value)?;
                entries
                    .try_reserve(decoded.len())
                    .map_err(|_| AxError::NoMemory)?;
                entries.extend(decoded);
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Resolves the one namespace parent of a directory from its checked
    /// `INODE_REF`. Directories cannot be hard-linked, so missing, duplicate,
    /// or dangling back-references are corrupt metadata rather than a path
    /// traversal boundary that can safely be guessed.
    pub fn directory_parent(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        root_inode: u64,
    ) -> AxResult<Option<u64>> {
        if inode == root_inode {
            return Ok(None);
        }

        let mut items = Vec::new();
        self.collect_tree_items(fs_root, tree_owner, &mut BTreeSet::new(), &mut items)?;
        let mut parent = None;
        for item in items {
            if item.key.objectid != inode || item.key.item_type != INODE_REF {
                continue;
            }
            for reference in decode_inode_refs(&item.value)? {
                let candidate = item.key.offset;
                if candidate == 0 || parent.replace(candidate).is_some() {
                    return Err(AxError::Io);
                }
                let entry =
                    self.lookup_dir_item(fs_root, tree_owner, candidate, &reference.name)?;
                if entry.inode != inode {
                    return Err(AxError::Io);
                }
            }
        }
        parent.map(Some).ok_or(AxError::Io)
    }

    /// Collects an inode's file-extent records in logical-file order.  Holes
    /// are absent by definition; callers synthesize zeroes for gaps rather
    /// than manufacturing an extent mapping.
    pub fn file_extents(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
    ) -> AxResult<Vec<(u64, BtrfsFileExtent)>> {
        let mut items = Vec::new();
        self.collect_tree_items(fs_root, tree_owner, &mut BTreeSet::new(), &mut items)?;
        let mut extents = Vec::new();
        for item in items {
            if item.key.objectid == inode && item.key.item_type == EXTENT_DATA {
                extents.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                extents.push((item.key.offset, BtrfsFileExtent::decode(&item.value)?));
            }
        }
        extents.sort_by_key(|(offset, _)| *offset);
        let mut end = 0u64;
        for (offset, extent) in &extents {
            if *offset < end || extent.num_bytes == 0 {
                return Err(AxError::Io);
            }
            end = offset.checked_add(extent.num_bytes).ok_or(AxError::Io)?;
        }
        Ok(extents)
    }

    /// Reads a file range from native extent records.  Holes and preallocated
    /// unwritten extents produce zeroes; compressed data is decoded before
    /// applying the extent offset, which is required for shared compressed
    /// extents.  Unsupported compression is surfaced rather than returning
    /// corrupted bytes as if it were uncompressed media.
    pub fn read_file_at(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        offset: u64,
        out: &mut [u8],
    ) -> AxResult<usize> {
        let inode_item = self.inode_item(fs_root, tree_owner, inode)?;
        if offset >= inode_item.size || out.is_empty() {
            return Ok(0);
        }
        let requested = usize::try_from((inode_item.size - offset).min(out.len() as u64))
            .map_err(|_| AxError::Io)?;
        out[..requested].fill(0);
        let end = offset.checked_add(requested as u64).ok_or(AxError::Io)?;
        for (file_offset, extent) in self.file_extents(fs_root, tree_owner, inode)? {
            let extent_end = file_offset
                .checked_add(extent.num_bytes)
                .ok_or(AxError::Io)?;
            let copy_start = file_offset.max(offset);
            let copy_end = extent_end.min(end);
            if copy_start >= copy_end
                || extent.is_explicit_hole()
                || matches!(extent.kind, super::BtrfsExtentKind::Prealloc)
            {
                continue;
            }
            let relative = usize::try_from(copy_start - file_offset).map_err(|_| AxError::Io)?;
            let count = usize::try_from(copy_end - copy_start).map_err(|_| AxError::Io)?;
            let decoded = match extent.kind {
                super::BtrfsExtentKind::Inline => decode_extent(
                    extent.compression,
                    &extent.inline_data,
                    usize::try_from(extent.ram_bytes).map_err(|_| AxError::Io)?,
                )?,
                super::BtrfsExtentKind::Regular => {
                    let mut stored = Vec::new();
                    stored
                        .try_reserve_exact(
                            usize::try_from(extent.disk_num_bytes)
                                .map_err(|_| AxError::NoMemory)?,
                        )
                        .map_err(|_| AxError::NoMemory)?;
                    stored.resize(
                        usize::try_from(extent.disk_num_bytes).map_err(|_| AxError::NoMemory)?,
                        0,
                    );
                    self.read_data_checked(extent.disk_bytenr, &mut stored)?;
                    decode_extent(
                        extent.compression,
                        &stored,
                        usize::try_from(extent.ram_bytes).map_err(|_| AxError::Io)?,
                    )?
                }
                super::BtrfsExtentKind::Prealloc => continue,
            };
            let source_start = usize::try_from(extent.extent_offset)
                .map_err(|_| AxError::Io)?
                .checked_add(relative)
                .ok_or(AxError::Io)?;
            let source_end = source_start.checked_add(count).ok_or(AxError::Io)?;
            let source = decoded.get(source_start..source_end).ok_or(AxError::Io)?;
            let destination = usize::try_from(copy_start - offset).map_err(|_| AxError::Io)?;
            out[destination..destination + count].copy_from_slice(source);
        }
        Ok(requested)
    }

    /// Publishes a freshly allocated, uncompressed regular extent for a
    /// complete file image.  Replacing an allocated extent retires its exact
    /// data-ref, checksum range and free-space relation in this same plan;
    /// shared reflinks retain the backing extent and lose only this relation.
    pub fn replace_file_with_regular(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        image: &[u8],
    ) -> AxResult<()> {
        self.replace_file_with_regular_in_range(fs_root, tree_owner, inode, image, None)
    }

    /// Replaces one file with several independently allocated regular
    /// extents.  Missing logical intervals are deliberate holes: no
    /// `EXTENT_DATA` item or backing reservation is manufactured for them.
    /// All old relations and every new relation are committed in one planner
    /// generation, so a failed range mutation cannot publish a half-punched
    /// file or leak a data extent.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn replace_file_with_regular_segments(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        final_size: u64,
        segments: &[(u64, Vec<u8>)],
    ) -> AxResult<()> {
        let sector = u64::from(self.superblock.sectorsize);
        let old_extents = self.file_extents(fs_root, tree_owner, inode)?;
        let allocator = self.logical_allocator()?;
        let mut prepared: Vec<(u64, Vec<u8>, LogicalReservation)> = Vec::new();
        prepared
            .try_reserve_exact(segments.len())
            .map_err(|_| AxError::NoMemory)?;
        let preparation = (|| {
            let mut previous_end = 0u64;
            for (offset, bytes) in segments {
                if bytes.is_empty() || *offset < previous_end {
                    return Err(AxError::InvalidInput);
                }
                let logical_len = u64::try_from(bytes.len()).map_err(|_| AxError::NoMemory)?;
                let end = offset.checked_add(logical_len).ok_or(AxError::NoMemory)?;
                // KEEP_SIZE allocation may publish an unwritten/zeroed extent
                // beyond i_size.  It is deliberately retained in nbytes while
                // the inode logical size remains `final_size`.
                let allocated_len = logical_len
                    .checked_add(sector - 1)
                    .ok_or(AxError::NoMemory)?
                    / sector
                    * sector;
                let mut stored = Vec::new();
                stored
                    .try_reserve_exact(
                        usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
                    )
                    .map_err(|_| AxError::NoMemory)?;
                stored.resize(
                    usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
                    0,
                );
                stored[..bytes.len()].copy_from_slice(bytes);
                let reservation =
                    allocator.reserve_where(allocated_len, sector, |logical, len| {
                        self.volume.data_contains(logical, len)
                    })?;
                prepared.push((*offset, stored, reservation));
                previous_end = end;
            }
            Ok(())
        })();
        if let Err(error) = preparation {
            for (_, _, reservation) in prepared {
                allocator.release(reservation)?;
            }
            return Err(error);
        }

        let mut retired_bytes = 0u64;
        for (_, extent) in &old_extents {
            if extent.kind == super::BtrfsExtentKind::Regular
                && extent.owns_physical_storage()
                && self.extent_reference_count(extent.disk_bytenr, extent.disk_num_bytes)? == 1
            {
                allocator.add_free(extent.disk_bytenr, extent.disk_num_bytes)?;
                retired_bytes = retired_bytes
                    .checked_add(extent.disk_num_bytes)
                    .ok_or(AxError::NoMemory)?;
            }
        }
        let mut wrote = false;
        let result = (|| {
            for (_, stored, reservation) in &prepared {
                self.volume.write_data_range(reservation.logical, stored)?;
                wrote = true;
            }
            let generation = self
                .superblock
                .generation
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
            let mut planner = self.mutation_planner(tree_owner)?;
            let old_keys: Vec<_> = planner
                .tree_items(tree_owner)?
                .iter()
                .filter(|item| item.key.objectid == inode && item.key.item_type == EXTENT_DATA)
                .map(|item| item.key)
                .collect();
            for key in old_keys {
                let _ = planner.delete_item(tree_owner, key)?;
            }
            let mut freed = 0u64;
            let mut qgroup_referenced = 0i64;
            let mut qgroup_exclusive = 0i64;
            for (file_offset, extent) in &old_extents {
                if extent.kind == super::BtrfsExtentKind::Regular && extent.owns_physical_storage()
                {
                    let became_free = planner.retire_regular_extent(
                        tree_owner,
                        inode,
                        *file_offset,
                        extent.disk_bytenr,
                        extent.disk_num_bytes,
                    )?;
                    planner
                        .transaction_mut()
                        .add_delayed_ref(super::DelayedRef {
                            bytenr: extent.disk_bytenr,
                            len: extent.disk_num_bytes,
                            root: tree_owner,
                            owner: inode,
                            identity: super::DelayedRefIdentity::Data {
                                file_offset: *file_offset,
                            },
                            delta: -1,
                        })?;
                    let bytes =
                        i64::try_from(extent.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
                    qgroup_referenced = qgroup_referenced.checked_sub(bytes).ok_or(AxError::Io)?;
                    if became_free {
                        planner.remove_checksum_range(
                            extent.disk_bytenr,
                            sector,
                            extent.disk_num_bytes,
                        )?;
                        freed = freed
                            .checked_add(extent.disk_num_bytes)
                            .ok_or(AxError::NoMemory)?;
                        qgroup_exclusive =
                            qgroup_exclusive.checked_sub(bytes).ok_or(AxError::Io)?;
                    }
                }
            }
            if freed != retired_bytes {
                return Err(AxError::Io);
            }
            let mut allocated_total = 0u64;
            for (file_offset, stored, reservation) in &prepared {
                let allocated_len = u64::try_from(stored.len()).map_err(|_| AxError::NoMemory)?;
                let logical_len = (*segments)
                    .iter()
                    .find(|(offset, _)| offset == file_offset)
                    .ok_or(AxError::Io)?
                    .1
                    .len() as u64;
                let mut checksums = Vec::new();
                checksums
                    .try_reserve_exact(
                        stored.len() / usize::try_from(sector).map_err(|_| AxError::Io)? * 4,
                    )
                    .map_err(|_| AxError::NoMemory)?;
                for block in stored.chunks_exact(usize::try_from(sector).map_err(|_| AxError::Io)?)
                {
                    checksums.extend_from_slice(&crc32c(block).to_le_bytes());
                }
                planner.set_item(
                    tree_owner,
                    TreeItemKey {
                        objectid: inode,
                        item_type: EXTENT_DATA,
                        offset: *file_offset,
                    },
                    super::encode_regular_extent(
                        generation,
                        reservation.logical,
                        allocated_len,
                        0,
                        logical_len,
                    )?,
                )?;
                planner.set_item(
                    TreeId::Extent as u64,
                    TreeItemKey {
                        objectid: reservation.logical,
                        item_type: super::EXTENT_ITEM,
                        offset: allocated_len,
                    },
                    super::encode_data_extent_item(generation, 1)?,
                )?;
                let mut relation = Vec::new();
                relation
                    .try_reserve_exact(24)
                    .map_err(|_| AxError::NoMemory)?;
                relation.extend_from_slice(&tree_owner.to_le_bytes());
                relation.extend_from_slice(&inode.to_le_bytes());
                relation.extend_from_slice(&file_offset.to_le_bytes());
                planner.set_item(
                    TreeId::Extent as u64,
                    TreeItemKey {
                        objectid: reservation.logical,
                        item_type: EXTENT_DATA_REF,
                        offset: u64::from(crc32c(&relation)),
                    },
                    super::encode_extent_data_ref(tree_owner, inode, *file_offset, 1)?,
                )?;
                planner.set_checksum_run(reservation.logical, sector, &checksums)?;
                planner
                    .transaction_mut()
                    .add_delayed_ref(super::DelayedRef {
                        bytenr: reservation.logical,
                        len: allocated_len,
                        root: tree_owner,
                        owner: inode,
                        identity: super::DelayedRefIdentity::Data {
                            file_offset: *file_offset,
                        },
                        delta: 1,
                    })?;
                let bytes = i64::try_from(allocated_len).map_err(|_| AxError::NoMemory)?;
                qgroup_referenced = qgroup_referenced.checked_add(bytes).ok_or(AxError::Io)?;
                qgroup_exclusive = qgroup_exclusive.checked_add(bytes).ok_or(AxError::Io)?;
                allocated_total = allocated_total
                    .checked_add(allocated_len)
                    .ok_or(AxError::NoMemory)?;
            }
            planner.replace_free_space_extents(&allocator.free_extents())?;
            planner.transaction_mut().charge_qgroup(
                super::QgroupId {
                    level: 0,
                    id: tree_owner,
                },
                qgroup_referenced,
                qgroup_exclusive,
            )?;
            if planner.tree_items(TreeId::Quota as u64).is_ok() {
                planner.charge_qgroup_on_disk(
                    super::QgroupId {
                        level: 0,
                        id: tree_owner,
                    },
                    qgroup_referenced,
                    qgroup_exclusive,
                    generation,
                )?;
            }
            let inode_key = TreeItemKey {
                objectid: inode,
                item_type: INODE_ITEM,
                offset: 0,
            };
            let index = planner
                .tree_items(tree_owner)?
                .binary_search_by_key(&inode_key, |item| item.key)
                .map_err(|_| AxError::NotFound)?;
            let mut inode_item =
                BtrfsInodeItem::decode(&planner.tree_items(tree_owner)?[index].value)?;
            inode_item.transid = generation;
            inode_item.size = final_size;
            inode_item.nbytes = allocated_total;
            planner.set_item(tree_owner, inode_key, inode_item.encode())?;
            let bytes_used = self
                .superblock
                .bytes_used
                .checked_add(allocated_total)
                .ok_or(AxError::NoMemory)?
                .checked_sub(retired_bytes)
                .ok_or(AxError::Io)?;
            self.commit_mutation_planner(planner, 0, bytes_used)
        })();
        match result {
            Ok(_) => {
                for (_, _, reservation) in &mut prepared {
                    allocator.commit(reservation)?;
                }
                Ok(())
            }
            Err(error) => {
                for (_, _, mut reservation) in prepared {
                    if wrote {
                        allocator.seal(&mut reservation)?;
                    } else {
                        allocator.release(reservation)?;
                    }
                }
                Err(error)
            }
        }
    }

    /// Atomically publishes a provenance-aware range mutation.  Retained
    /// ranges remain aliases of their original physical extent; only COW
    /// ranges reserve/write sectors.  The filesystem, extent, checksum,
    /// free-space and quota trees are all changed by the same planner
    /// generation, so neither a moved relation nor a punched extent can be
    /// observed without its accounting counterpart.
    pub fn replace_file_with_range_segments(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        final_size: u64,
        segments: &[RangeSegment],
    ) -> AxResult<()> {
        self.replace_file_with_range_segments_at(
            fs_root, tree_owner, inode, final_size, segments, None,
        )
    }

    /// Placement-constrained form used by balance relocation.  It retains
    /// every RangeSegment/Prealloc relation rule of the public operation and
    /// narrows only where newly-COW storage may be reserved.
    pub fn replace_file_with_range_segments_at(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        final_size: u64,
        segments: &[RangeSegment],
        placement: Option<(u64, u64)>,
    ) -> AxResult<()> {
        enum PreparedKind {
            Retain {
                bytenr: u64,
                disk_len: u64,
                extent_offset: u64,
                logical_len: u64,
                prealloc: bool,
            },
            Cow {
                stored: Vec<u8>,
                reservation: LogicalReservation,
                logical_len: u64,
            },
            Prealloc {
                reservation: LogicalReservation,
                logical_len: u64,
                allocated_len: u64,
            },
            Hole,
        }
        struct Prepared {
            offset: u64,
            kind: PreparedKind,
        }

        let sector = u64::from(self.superblock.sectorsize);
        if let Some((logical, len)) = placement {
            if logical == 0 || len == 0 || !self.volume.data_contains(logical, len) {
                return Err(AxError::InvalidInput);
            }
        }
        let old_extents = self.file_extents(fs_root, tree_owner, inode)?;
        let allocator = self.logical_allocator()?;
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(segments.len())
            .map_err(|_| AxError::NoMemory)?;
        let setup = (|| {
            let mut previous_end = 0u64;
            for segment in segments {
                let offset = segment.offset();
                let len = segment.len();
                let end = offset.checked_add(len).ok_or(AxError::NoMemory)?;
                if len == 0 || offset < previous_end {
                    return Err(AxError::InvalidInput);
                }
                previous_end = end;
                match segment {
                    RangeSegment::Hole { .. } => prepared.push(Prepared {
                        offset,
                        kind: PreparedKind::Hole,
                    }),
                    RangeSegment::Prealloc { length, .. } => {
                        let allocated_len =
                            length.checked_add(sector - 1).ok_or(AxError::NoMemory)? / sector
                                * sector;
                        let reservation =
                            allocator.reserve_where(allocated_len, sector, |logical, len| {
                                self.volume.data_contains(logical, len)
                                    && placement.is_none_or(|(start, span)| {
                                        logical >= start
                                            && logical.checked_add(len).is_some_and(|end| {
                                                end <= start.saturating_add(span)
                                            })
                                    })
                            })?;
                        prepared.push(Prepared {
                            offset,
                            kind: PreparedKind::Prealloc {
                                reservation,
                                logical_len: *length,
                                allocated_len,
                            },
                        });
                    }
                    RangeSegment::CowData { bytes, .. } => {
                        let logical_len =
                            u64::try_from(bytes.len()).map_err(|_| AxError::NoMemory)?;
                        let allocated_len = logical_len
                            .checked_add(sector - 1)
                            .ok_or(AxError::NoMemory)?
                            / sector
                            * sector;
                        let mut stored = Vec::new();
                        stored
                            .try_reserve_exact(
                                usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
                            )
                            .map_err(|_| AxError::NoMemory)?;
                        stored.resize(
                            usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
                            0,
                        );
                        stored[..bytes.len()].copy_from_slice(bytes);
                        let reservation =
                            allocator.reserve_where(allocated_len, sector, |logical, len| {
                                self.volume.data_contains(logical, len)
                                    && placement.is_none_or(|(start, span)| {
                                        logical >= start
                                            && logical.checked_add(len).is_some_and(|end| {
                                                end <= start.saturating_add(span)
                                            })
                                    })
                            })?;
                        prepared.push(Prepared {
                            offset,
                            kind: PreparedKind::Cow {
                                stored,
                                reservation,
                                logical_len,
                            },
                        });
                    }
                    RangeSegment::Retain {
                        source_inode,
                        source_offset,
                        length,
                        ..
                    } => {
                        let source_end = source_offset
                            .checked_add(*length)
                            .ok_or(AxError::NoMemory)?;
                        let source_extents = if *source_inode == inode {
                            old_extents.clone()
                        } else {
                            self.file_extents(fs_root, tree_owner, *source_inode)?
                        };
                        let (old_offset, old) = source_extents
                            .iter()
                            .find(|(old_offset, old)| {
                                old_offset.checked_add(old.num_bytes).is_some_and(|end| {
                                    *source_offset >= *old_offset && source_end <= end
                                })
                            })
                            .ok_or(AxError::InvalidInput)?;
                        if old.is_explicit_hole() {
                            prepared.push(Prepared {
                                offset,
                                kind: PreparedKind::Hole,
                            });
                            continue;
                        }
                        match old.kind {
                            // These encoders deliberately emit the native
                            // uncompressed/unencrypted/other_encoding=0
                            // representation.  Refuse a relation that has
                            // any other header semantics rather than moving
                            // it and silently normalizing those fields.
                            super::BtrfsExtentKind::Regular
                                if old.compression == 0
                                    && old.encryption == 0
                                    && old.other_encoding == 0
                                    && old.ram_bytes == old.disk_num_bytes =>
                            {
                                let relative =
                                    source_offset.checked_sub(*old_offset).ok_or(AxError::Io)?;
                                let extent_offset = old
                                    .extent_offset
                                    .checked_add(relative)
                                    .ok_or(AxError::NoMemory)?;
                                if extent_offset
                                    .checked_add(*length)
                                    .map_or(true, |end| end > old.disk_num_bytes)
                                {
                                    return Err(AxError::Io);
                                }
                                prepared.push(Prepared {
                                    offset,
                                    kind: PreparedKind::Retain {
                                        bytenr: old.disk_bytenr,
                                        disk_len: old.disk_num_bytes,
                                        extent_offset,
                                        logical_len: *length,
                                        prealloc: false,
                                    },
                                });
                            }
                            super::BtrfsExtentKind::Prealloc
                                if old.compression == 0
                                    && old.encryption == 0
                                    && old.other_encoding == 0
                                    && old.ram_bytes == old.disk_num_bytes =>
                            {
                                let relative =
                                    source_offset.checked_sub(*old_offset).ok_or(AxError::Io)?;
                                let extent_offset = old
                                    .extent_offset
                                    .checked_add(relative)
                                    .ok_or(AxError::NoMemory)?;
                                if extent_offset
                                    .checked_add(*length)
                                    .map_or(true, |end| end > old.disk_num_bytes)
                                {
                                    return Err(AxError::Io);
                                }
                                prepared.push(Prepared {
                                    offset,
                                    kind: PreparedKind::Retain {
                                        bytenr: old.disk_bytenr,
                                        disk_len: old.disk_num_bytes,
                                        extent_offset,
                                        logical_len: *length,
                                        prealloc: true,
                                    },
                                });
                            }
                            // A PREALLOC record is semantically distinct
                            // from a hole.  If an unsupported header cannot
                            // be retained verbatim, fail rather than
                            // rewriting it as a zeroed regular extent.
                            super::BtrfsExtentKind::Prealloc => {
                                return Err(AxError::OperationNotSupported);
                            }
                            // Inline and compressed extents cannot be split by merely changing
                            // extent_offset.  Read only the requested source interval and make
                            // its provenance explicit as a COW write instead of manufacturing a
                            // false physical alias.
                            super::BtrfsExtentKind::Inline | super::BtrfsExtentKind::Regular => {
                                let mut bytes = Vec::new();
                                bytes
                                    .try_reserve_exact(
                                        usize::try_from(*length).map_err(|_| AxError::NoMemory)?,
                                    )
                                    .map_err(|_| AxError::NoMemory)?;
                                bytes.resize(
                                    usize::try_from(*length).map_err(|_| AxError::NoMemory)?,
                                    0,
                                );
                                if self.read_file_at(
                                    fs_root,
                                    tree_owner,
                                    *source_inode,
                                    *source_offset,
                                    &mut bytes,
                                )? != bytes.len()
                                {
                                    return Err(AxError::Io);
                                }
                                let allocated_len =
                                    length.checked_add(sector - 1).ok_or(AxError::NoMemory)?
                                        / sector
                                        * sector;
                                let mut stored = Vec::new();
                                stored
                                    .try_reserve_exact(
                                        usize::try_from(allocated_len)
                                            .map_err(|_| AxError::NoMemory)?,
                                    )
                                    .map_err(|_| AxError::NoMemory)?;
                                stored.resize(
                                    usize::try_from(allocated_len)
                                        .map_err(|_| AxError::NoMemory)?,
                                    0,
                                );
                                stored[..bytes.len()].copy_from_slice(&bytes);
                                let reservation = allocator.reserve_where(
                                    allocated_len,
                                    sector,
                                    |logical, len| {
                                        self.volume.data_contains(logical, len)
                                            && placement.is_none_or(|(start, span)| {
                                                logical >= start
                                                    && logical.checked_add(len).is_some_and(|end| {
                                                        end <= start.saturating_add(span)
                                                    })
                                            })
                                    },
                                )?;
                                prepared.push(Prepared {
                                    offset,
                                    kind: PreparedKind::Cow {
                                        stored,
                                        reservation,
                                        logical_len: *length,
                                    },
                                });
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = setup {
            for item in prepared {
                match item.kind {
                    PreparedKind::Cow { reservation, .. }
                    | PreparedKind::Prealloc { reservation, .. } => {
                        allocator.release(reservation)?
                    }
                    _ => {}
                }
            }
            return Err(error);
        }

        let mut newly_free: Vec<(u64, u64)> = Vec::new();
        let mut wrote = false;
        let result = (|| {
            for item in &prepared {
                if let PreparedKind::Cow {
                    stored,
                    reservation,
                    ..
                } = &item.kind
                {
                    self.volume.write_data_range(reservation.logical, stored)?;
                    wrote = true;
                }
            }
            let generation = self
                .superblock
                .generation
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
            let mut planner = self.mutation_planner(tree_owner)?;
            let old_keys: Vec<_> = planner
                .tree_items(tree_owner)?
                .iter()
                .filter(|item| item.key.objectid == inode && item.key.item_type == EXTENT_DATA)
                .map(|item| item.key)
                .collect();
            for key in old_keys {
                let _ = planner.delete_item(tree_owner, key)?;
            }

            let mut qgroup_referenced = 0i64;
            let mut qgroup_exclusive = 0i64;
            let mut allocated_total = 0u64;
            let mut retained_physical = BTreeSet::new();
            for item in &prepared {
                match &item.kind {
                    PreparedKind::Hole => {}
                    PreparedKind::Retain {
                        bytenr,
                        disk_len,
                        extent_offset,
                        logical_len,
                        prealloc,
                    } => {
                        planner.add_regular_extent_ref(
                            tree_owner,
                            inode,
                            item.offset,
                            *bytenr,
                            *disk_len,
                        )?;
                        let encoded = if *prealloc {
                            super::encode_prealloc_extent(
                                generation,
                                *bytenr,
                                *disk_len,
                                *extent_offset,
                                *logical_len,
                            )?
                        } else {
                            super::encode_regular_extent(
                                generation,
                                *bytenr,
                                *disk_len,
                                *extent_offset,
                                *logical_len,
                            )?
                        };
                        planner.set_item(
                            tree_owner,
                            TreeItemKey {
                                objectid: inode,
                                item_type: EXTENT_DATA,
                                offset: item.offset,
                            },
                            encoded,
                        )?;
                        planner
                            .transaction_mut()
                            .add_delayed_ref(super::DelayedRef {
                                bytenr: *bytenr,
                                len: *disk_len,
                                root: tree_owner,
                                owner: inode,
                                identity: super::DelayedRefIdentity::Data {
                                    file_offset: item.offset,
                                },
                                delta: 1,
                            })?;
                        if retained_physical.insert((*bytenr, *disk_len)) {
                            allocated_total = allocated_total
                                .checked_add(*disk_len)
                                .ok_or(AxError::NoMemory)?;
                        }
                        qgroup_referenced = qgroup_referenced
                            .checked_add(i64::try_from(*disk_len).map_err(|_| AxError::NoMemory)?)
                            .ok_or(AxError::Io)?;
                    }
                    PreparedKind::Cow {
                        stored,
                        reservation,
                        logical_len,
                    } => {
                        let allocated_len =
                            u64::try_from(stored.len()).map_err(|_| AxError::NoMemory)?;
                        let mut checksums = Vec::new();
                        checksums
                            .try_reserve_exact(
                                stored.len() / usize::try_from(sector).map_err(|_| AxError::Io)?
                                    * 4,
                            )
                            .map_err(|_| AxError::NoMemory)?;
                        for block in
                            stored.chunks_exact(usize::try_from(sector).map_err(|_| AxError::Io)?)
                        {
                            checksums.extend_from_slice(&crc32c(block).to_le_bytes());
                        }
                        planner.set_item(
                            tree_owner,
                            TreeItemKey {
                                objectid: inode,
                                item_type: EXTENT_DATA,
                                offset: item.offset,
                            },
                            super::encode_regular_extent(
                                generation,
                                reservation.logical,
                                allocated_len,
                                0,
                                *logical_len,
                            )?,
                        )?;
                        planner.set_item(
                            TreeId::Extent as u64,
                            TreeItemKey {
                                objectid: reservation.logical,
                                item_type: super::EXTENT_ITEM,
                                offset: allocated_len,
                            },
                            super::encode_data_extent_item(generation, 1)?,
                        )?;
                        let mut relation = Vec::new();
                        relation
                            .try_reserve_exact(24)
                            .map_err(|_| AxError::NoMemory)?;
                        relation.extend_from_slice(&tree_owner.to_le_bytes());
                        relation.extend_from_slice(&inode.to_le_bytes());
                        relation.extend_from_slice(&item.offset.to_le_bytes());
                        planner.set_item(
                            TreeId::Extent as u64,
                            TreeItemKey {
                                objectid: reservation.logical,
                                item_type: EXTENT_DATA_REF,
                                offset: u64::from(crc32c(&relation)),
                            },
                            super::encode_extent_data_ref(tree_owner, inode, item.offset, 1)?,
                        )?;
                        planner.set_checksum_run(reservation.logical, sector, &checksums)?;
                        planner
                            .transaction_mut()
                            .add_delayed_ref(super::DelayedRef {
                                bytenr: reservation.logical,
                                len: allocated_len,
                                root: tree_owner,
                                owner: inode,
                                identity: super::DelayedRefIdentity::Data {
                                    file_offset: item.offset,
                                },
                                delta: 1,
                            })?;
                        let bytes = i64::try_from(allocated_len).map_err(|_| AxError::NoMemory)?;
                        qgroup_referenced =
                            qgroup_referenced.checked_add(bytes).ok_or(AxError::Io)?;
                        qgroup_exclusive =
                            qgroup_exclusive.checked_add(bytes).ok_or(AxError::Io)?;
                        allocated_total = allocated_total
                            .checked_add(allocated_len)
                            .ok_or(AxError::NoMemory)?;
                    }
                    PreparedKind::Prealloc {
                        reservation,
                        logical_len,
                        allocated_len,
                    } => {
                        planner.set_item(
                            tree_owner,
                            TreeItemKey {
                                objectid: inode,
                                item_type: EXTENT_DATA,
                                offset: item.offset,
                            },
                            super::encode_prealloc_extent(
                                generation,
                                reservation.logical,
                                *allocated_len,
                                0,
                                *logical_len,
                            )?,
                        )?;
                        planner.set_item(
                            TreeId::Extent as u64,
                            TreeItemKey {
                                objectid: reservation.logical,
                                item_type: super::EXTENT_ITEM,
                                offset: *allocated_len,
                            },
                            super::encode_data_extent_item(generation, 1)?,
                        )?;
                        let mut relation = Vec::new();
                        relation
                            .try_reserve_exact(24)
                            .map_err(|_| AxError::NoMemory)?;
                        relation.extend_from_slice(&tree_owner.to_le_bytes());
                        relation.extend_from_slice(&inode.to_le_bytes());
                        relation.extend_from_slice(&item.offset.to_le_bytes());
                        planner.set_item(
                            TreeId::Extent as u64,
                            TreeItemKey {
                                objectid: reservation.logical,
                                item_type: EXTENT_DATA_REF,
                                offset: u64::from(crc32c(&relation)),
                            },
                            super::encode_extent_data_ref(tree_owner, inode, item.offset, 1)?,
                        )?;
                        planner
                            .transaction_mut()
                            .add_delayed_ref(super::DelayedRef {
                                bytenr: reservation.logical,
                                len: *allocated_len,
                                root: tree_owner,
                                owner: inode,
                                identity: super::DelayedRefIdentity::Data {
                                    file_offset: item.offset,
                                },
                                delta: 1,
                            })?;
                        let bytes = i64::try_from(*allocated_len).map_err(|_| AxError::NoMemory)?;
                        qgroup_referenced =
                            qgroup_referenced.checked_add(bytes).ok_or(AxError::Io)?;
                        qgroup_exclusive =
                            qgroup_exclusive.checked_add(bytes).ok_or(AxError::Io)?;
                        allocated_total = allocated_total
                            .checked_add(*allocated_len)
                            .ok_or(AxError::NoMemory)?;
                    }
                }
            }

            let mut freed = 0u64;
            for (file_offset, extent) in &old_extents {
                if !extent.owns_physical_storage() {
                    continue;
                }
                let became_free = planner.retire_regular_extent(
                    tree_owner,
                    inode,
                    *file_offset,
                    extent.disk_bytenr,
                    extent.disk_num_bytes,
                )?;
                planner
                    .transaction_mut()
                    .add_delayed_ref(super::DelayedRef {
                        bytenr: extent.disk_bytenr,
                        len: extent.disk_num_bytes,
                        root: tree_owner,
                        owner: inode,
                        identity: super::DelayedRefIdentity::Data {
                            file_offset: *file_offset,
                        },
                        delta: -1,
                    })?;
                let bytes = i64::try_from(extent.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
                if !retained_physical.contains(&(extent.disk_bytenr, extent.disk_num_bytes)) {
                    qgroup_referenced = qgroup_referenced.checked_sub(bytes).ok_or(AxError::Io)?;
                }
                if became_free {
                    planner.remove_checksum_range(
                        extent.disk_bytenr,
                        sector,
                        extent.disk_num_bytes,
                    )?;
                    freed = freed
                        .checked_add(extent.disk_num_bytes)
                        .ok_or(AxError::NoMemory)?;
                    newly_free.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    newly_free.push((extent.disk_bytenr, extent.disk_num_bytes));
                    qgroup_exclusive = qgroup_exclusive.checked_sub(bytes).ok_or(AxError::Io)?;
                }
            }
            // Do not mutate the live allocator before the root switch: a
            // later metadata failure must not make an on-media extent
            // available for a second allocation.  The planner receives the
            // post-transaction image, and the allocator is updated only once
            // commit succeeds below.
            let mut post_free = allocator.free_extents();
            post_free
                .try_reserve(newly_free.len())
                .map_err(|_| AxError::NoMemory)?;
            post_free.extend_from_slice(&newly_free);
            post_free.sort_unstable_by_key(|(offset, _)| *offset);
            let mut merged_free: Vec<(u64, u64)> = Vec::new();
            merged_free
                .try_reserve_exact(post_free.len())
                .map_err(|_| AxError::NoMemory)?;
            for (offset, len) in post_free {
                let end = offset.checked_add(len).ok_or(AxError::Io)?;
                if let Some((previous_offset, previous_len)) = merged_free.last_mut() {
                    let previous_end = previous_offset
                        .checked_add(*previous_len)
                        .ok_or(AxError::Io)?;
                    if offset < previous_end {
                        return Err(AxError::Io);
                    }
                    if offset == previous_end {
                        *previous_len = previous_len.checked_add(len).ok_or(AxError::NoMemory)?;
                        continue;
                    }
                }
                let _ = end;
                merged_free.push((offset, len));
            }
            planner.replace_free_space_extents(&merged_free)?;
            planner.transaction_mut().charge_qgroup(
                super::QgroupId {
                    level: 0,
                    id: tree_owner,
                },
                qgroup_referenced,
                qgroup_exclusive,
            )?;
            if planner.tree_items(TreeId::Quota as u64).is_ok() {
                planner.charge_qgroup_on_disk(
                    super::QgroupId {
                        level: 0,
                        id: tree_owner,
                    },
                    qgroup_referenced,
                    qgroup_exclusive,
                    generation,
                )?;
            }
            let inode_key = TreeItemKey {
                objectid: inode,
                item_type: INODE_ITEM,
                offset: 0,
            };
            let index = planner
                .tree_items(tree_owner)?
                .binary_search_by_key(&inode_key, |item| item.key)
                .map_err(|_| AxError::NotFound)?;
            let mut inode_item =
                BtrfsInodeItem::decode(&planner.tree_items(tree_owner)?[index].value)?;
            inode_item.transid = generation;
            inode_item.size = final_size;
            inode_item.nbytes = allocated_total;
            planner.set_item(tree_owner, inode_key, inode_item.encode())?;
            let allocated_bytes =
                prepared
                    .iter()
                    .try_fold(0u64, |total, item| match &item.kind {
                        PreparedKind::Cow { stored, .. } => total
                            .checked_add(
                                u64::try_from(stored.len()).map_err(|_| AxError::NoMemory)?,
                            )
                            .ok_or(AxError::NoMemory),
                        PreparedKind::Prealloc { allocated_len, .. } => {
                            total.checked_add(*allocated_len).ok_or(AxError::NoMemory)
                        }
                        _ => Ok(total),
                    })?;
            let bytes_used = self
                .superblock
                .bytes_used
                .checked_add(allocated_bytes)
                .ok_or(AxError::NoMemory)?
                .checked_sub(freed)
                .ok_or(AxError::Io)?;
            self.commit_mutation_planner(planner, 0, bytes_used)
        })();
        match result {
            Ok(_) => {
                for item in &mut prepared {
                    match &mut item.kind {
                        PreparedKind::Cow { reservation, .. }
                        | PreparedKind::Prealloc { reservation, .. } => {
                            allocator.commit(reservation)?
                        }
                        _ => {}
                    }
                }
                // `freed` extents have already been published in the
                // free-space tree; make them available to the in-memory
                // allocator only after that commit boundary.
                for &(bytenr, len) in &newly_free {
                    allocator.add_free(bytenr, len)?;
                }
                Ok(())
            }
            Err(error) => {
                for item in prepared {
                    match item.kind {
                        PreparedKind::Cow {
                            mut reservation, ..
                        }
                        | PreparedKind::Prealloc {
                            mut reservation, ..
                        } => {
                            if wrote {
                                allocator.seal(&mut reservation)?;
                            } else {
                                allocator.release(reservation)?;
                            }
                        }
                        _ => {}
                    }
                }
                Err(error)
            }
        }
    }

    fn replace_file_with_regular_in_range(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        image: &[u8],
        target_chunk: Option<(u64, u64)>,
    ) -> AxResult<()> {
        if image.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let old_extents = self.file_extents(fs_root, tree_owner, inode)?;
        let sector = u64::from(self.superblock.sectorsize);
        let logical_len = u64::try_from(image.len()).map_err(|_| AxError::NoMemory)?;
        let allocated_len = logical_len
            .checked_add(sector - 1)
            .ok_or(AxError::NoMemory)?
            / sector
            * sector;
        let allocator = self.logical_allocator()?;
        let mut reservation = allocator.reserve_where(allocated_len, sector, |logical, len| {
            self.volume.data_contains(logical, len)
                && target_chunk.map_or(true, |(start, size)| {
                    logical >= start
                        && logical
                            .checked_add(len)
                            .is_some_and(|end| end <= start.saturating_add(size))
                })
        })?;
        let mut retired_bytes = 0u64;
        for (_, extent) in &old_extents {
            if extent.kind == super::BtrfsExtentKind::Regular && extent.owns_physical_storage() {
                let refs =
                    self.extent_reference_count(extent.disk_bytenr, extent.disk_num_bytes)?;
                if refs == 1 {
                    allocator.add_free(extent.disk_bytenr, extent.disk_num_bytes)?;
                    retired_bytes = retired_bytes
                        .checked_add(extent.disk_num_bytes)
                        .ok_or(AxError::NoMemory)?;
                }
            }
        }
        let mut stored = Vec::new();
        stored
            .try_reserve_exact(usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        stored.resize(
            usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
            0,
        );
        stored[..image.len()].copy_from_slice(image);
        let mut wrote = false;
        let result = (|| {
            self.volume.write_data_range(reservation.logical, &stored)?;
            wrote = true;
            let generation = self
                .superblock
                .generation
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
            let mut checksums = Vec::new();
            checksums
                .try_reserve_exact(
                    stored.len() / usize::try_from(sector).map_err(|_| AxError::Io)? * 4,
                )
                .map_err(|_| AxError::NoMemory)?;
            for block in stored.chunks_exact(usize::try_from(sector).map_err(|_| AxError::Io)?) {
                checksums.extend_from_slice(&crc32c(block).to_le_bytes());
            }
            let mut planner = self.mutation_planner(tree_owner)?;
            let old_keys: Vec<_> = planner
                .tree_items(tree_owner)?
                .iter()
                .filter(|item| item.key.objectid == inode && item.key.item_type == EXTENT_DATA)
                .map(|item| item.key)
                .collect();
            for key in old_keys {
                let _ = planner.delete_item(tree_owner, key)?;
            }
            let mut freed = 0u64;
            let mut qgroup_referenced =
                i64::try_from(allocated_len).map_err(|_| AxError::NoMemory)?;
            let mut qgroup_exclusive = qgroup_referenced;
            for (file_offset, extent) in &old_extents {
                if extent.kind == super::BtrfsExtentKind::Regular && extent.owns_physical_storage()
                {
                    let became_free = planner.retire_regular_extent(
                        tree_owner,
                        inode,
                        *file_offset,
                        extent.disk_bytenr,
                        extent.disk_num_bytes,
                    )?;
                    planner
                        .transaction_mut()
                        .add_delayed_ref(super::DelayedRef {
                            bytenr: extent.disk_bytenr,
                            len: extent.disk_num_bytes,
                            root: tree_owner,
                            owner: inode,
                            identity: super::DelayedRefIdentity::Data {
                                file_offset: *file_offset,
                            },
                            delta: -1,
                        })?;
                    qgroup_referenced = qgroup_referenced
                        .checked_sub(
                            i64::try_from(extent.disk_num_bytes).map_err(|_| AxError::NoMemory)?,
                        )
                        .ok_or(AxError::Io)?;
                    if became_free {
                        planner.remove_checksum_range(
                            extent.disk_bytenr,
                            sector,
                            extent.disk_num_bytes,
                        )?;
                        freed = freed
                            .checked_add(extent.disk_num_bytes)
                            .ok_or(AxError::NoMemory)?;
                        qgroup_exclusive = qgroup_exclusive
                            .checked_sub(
                                i64::try_from(extent.disk_num_bytes)
                                    .map_err(|_| AxError::NoMemory)?,
                            )
                            .ok_or(AxError::Io)?;
                    }
                }
            }
            if freed != retired_bytes {
                return Err(AxError::Io);
            }
            planner.set_item(
                tree_owner,
                TreeItemKey {
                    objectid: inode,
                    item_type: EXTENT_DATA,
                    offset: 0,
                },
                super::encode_regular_extent(
                    generation,
                    reservation.logical,
                    allocated_len,
                    0,
                    logical_len,
                )?,
            )?;
            planner.set_item(
                TreeId::Extent as u64,
                TreeItemKey {
                    objectid: reservation.logical,
                    item_type: super::EXTENT_ITEM,
                    offset: allocated_len,
                },
                super::encode_data_extent_item(generation, 1)?,
            )?;
            let mut relation = Vec::new();
            relation
                .try_reserve_exact(24)
                .map_err(|_| AxError::NoMemory)?;
            relation.extend_from_slice(&tree_owner.to_le_bytes());
            relation.extend_from_slice(&inode.to_le_bytes());
            relation.extend_from_slice(&0u64.to_le_bytes());
            let ref_offset = u64::from(crc32c(&relation));
            planner.set_item(
                TreeId::Extent as u64,
                TreeItemKey {
                    objectid: reservation.logical,
                    item_type: EXTENT_DATA_REF,
                    offset: ref_offset,
                },
                super::encode_extent_data_ref(tree_owner, inode, 0, 1)?,
            )?;
            planner.set_checksum_run(reservation.logical, sector, &checksums)?;
            planner.replace_free_space_extents(&allocator.free_extents())?;
            planner
                .transaction_mut()
                .add_delayed_ref(super::DelayedRef {
                    bytenr: reservation.logical,
                    len: allocated_len,
                    root: tree_owner,
                    owner: inode,
                    identity: super::DelayedRefIdentity::Data { file_offset: 0 },
                    delta: 1,
                })?;
            planner.transaction_mut().charge_qgroup(
                super::QgroupId {
                    level: 0,
                    id: tree_owner,
                },
                qgroup_referenced,
                qgroup_exclusive,
            )?;
            if planner.tree_items(TreeId::Quota as u64).is_ok() {
                planner.charge_qgroup_on_disk(
                    super::QgroupId {
                        level: 0,
                        id: tree_owner,
                    },
                    qgroup_referenced,
                    qgroup_exclusive,
                    generation,
                )?;
            }
            let inode_key = TreeItemKey {
                objectid: inode,
                item_type: INODE_ITEM,
                offset: 0,
            };
            let index = planner
                .tree_items(tree_owner)?
                .binary_search_by_key(&inode_key, |item| item.key)
                .map_err(|_| AxError::NotFound)?;
            let mut inode_item =
                BtrfsInodeItem::decode(&planner.tree_items(tree_owner)?[index].value)?;
            inode_item.transid = generation;
            inode_item.size = logical_len;
            inode_item.nbytes = allocated_len;
            planner.set_item(tree_owner, inode_key, inode_item.encode())?;
            let bytes_used = self
                .superblock
                .bytes_used
                .checked_add(allocated_len)
                .ok_or(AxError::NoMemory)?
                .checked_sub(retired_bytes)
                .ok_or(AxError::Io)?;
            self.commit_mutation_planner(planner, 0, bytes_used)?;
            Ok(())
        })();
        match result {
            Ok(()) => allocator.commit(&mut reservation),
            Err(error) => {
                if wrote {
                    allocator.seal(&mut reservation)?;
                } else {
                    allocator.release(reservation)?;
                }
                Err(error)
            }
        }
    }

    /// Scrubs every allocated sector of one inode through its checksum-tree
    /// record.  Mirror repair is delegated to the volume only after a good
    /// checksum-verified copy was found, so this never copies an unchecked
    /// primary over a replica.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn scrub_inode(
        &self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        repair: bool,
    ) -> AxResult<super::ScrubReport> {
        let sector = usize::try_from(self.superblock.sectorsize).map_err(|_| AxError::Io)?;
        let mut report = super::ScrubReport::default();
        for (_, extent) in self.file_extents(fs_root, tree_owner, inode)? {
            if extent.kind != super::BtrfsExtentKind::Regular
                || !extent.owns_physical_storage()
                || extent.compression != 0
            {
                continue;
            }
            if extent.disk_num_bytes % sector as u64 != 0 {
                return Err(AxError::Io);
            }
            for offset in (0..extent.disk_num_bytes).step_by(sector) {
                let logical = extent.disk_bytenr.checked_add(offset).ok_or(AxError::Io)?;
                let one = self.data_checksum(logical)?;
                let result = self.volume.scrub_extent(logical, sector, one, repair)?;
                report.checked_mirrors = report
                    .checked_mirrors
                    .saturating_add(result.checked_mirrors);
                report.bad_mirrors = report.bad_mirrors.saturating_add(result.bad_mirrors);
                report.repaired_mirrors = report
                    .repaired_mirrors
                    .saturating_add(result.repaired_mirrors);
            }
        }
        Ok(report)
    }

    /// Shares one complete regular extent between two inodes.  Partial
    /// extent cloning would need split extent items and is intentionally not
    /// disguised as a full-range clone.  Destination allocation must be
    /// empty/inline so this transaction never drops an unrelated data ref.
    pub fn reflink_regular_extent(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        source_inode: u64,
        source_offset: u64,
        destination_inode: u64,
        destination_offset: u64,
        len: u64,
    ) -> AxResult<()> {
        if source_inode == 0 || destination_inode == 0 || len == 0 {
            return Err(AxError::InvalidInput);
        }
        let source_size = self.inode_item(fs_root, tree_owner, source_inode)?.size;
        let source_end = source_offset
            .checked_add(len)
            .ok_or(AxError::InvalidInput)?;
        if source_end > source_size {
            return Err(AxError::InvalidInput);
        }
        let end = destination_offset
            .checked_add(len)
            .ok_or(AxError::InvalidInput)?;
        let mut segments = Vec::new();
        for (offset, extent) in self.file_extents(fs_root, tree_owner, destination_inode)? {
            let extent_end = offset.checked_add(extent.num_bytes).ok_or(AxError::Io)?;
            if extent_end <= destination_offset || offset >= end {
                segments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                segments.push(RangeSegment::Retain {
                    source_inode: destination_inode,
                    source_offset: offset,
                    destination_offset: offset,
                    length: extent.num_bytes,
                });
            } else {
                if offset < destination_offset {
                    segments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    segments.push(RangeSegment::Retain {
                        source_inode: destination_inode,
                        source_offset: offset,
                        destination_offset: offset,
                        length: destination_offset - offset,
                    });
                }
                if extent_end > end {
                    segments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    segments.push(RangeSegment::Retain {
                        source_inode: destination_inode,
                        source_offset: end,
                        destination_offset: end,
                        length: extent_end - end,
                    });
                }
            }
        }
        // Split the source mapping at every extent boundary.  Holes remain
        // holes in the replacement image; an individual compressed/inline
        // segment is materialized by the range engine instead of inventing a
        // physical shared relation for bytes that have no split-safe extent
        // header.
        for (offset, extent) in self.file_extents(fs_root, tree_owner, source_inode)? {
            let extent_end = offset.checked_add(extent.num_bytes).ok_or(AxError::Io)?;
            let begin = offset.max(source_offset);
            let finish = extent_end.min(source_end);
            if begin >= finish {
                continue;
            }
            segments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            segments.push(RangeSegment::Retain {
                source_inode,
                source_offset: begin,
                destination_offset: destination_offset
                    .checked_add(begin - source_offset)
                    .ok_or(AxError::NoMemory)?,
                length: finish - begin,
            });
        }
        segments.sort_by_key(RangeSegment::offset);
        let old_size = self
            .inode_item(fs_root, tree_owner, destination_inode)?
            .size;
        self.replace_file_with_range_segments(
            fs_root,
            tree_owner,
            destination_inode,
            old_size.max(end),
            &segments,
        )
    }

    /// Verifies two complete candidate extent images before sharing the
    /// source extent.  A mismatch has no side effects and reports `false`;
    /// successful dedupe performs the same reference transaction as reflink.
    pub fn dedupe_regular_extent(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        source_inode: u64,
        source_offset: u64,
        destination_inode: u64,
        destination_offset: u64,
        len: u64,
    ) -> AxResult<bool> {
        let size = usize::try_from(len).map_err(|_| AxError::NoMemory)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(size)
            .map_err(|_| AxError::NoMemory)?;
        source.resize(size, 0);
        let mut destination = Vec::new();
        destination
            .try_reserve_exact(size)
            .map_err(|_| AxError::NoMemory)?;
        destination.resize(size, 0);
        if self.read_file_at(
            fs_root,
            tree_owner,
            source_inode,
            source_offset,
            &mut source,
        )? != size
            || self.read_file_at(
                fs_root,
                tree_owner,
                destination_inode,
                destination_offset,
                &mut destination,
            )? != size
            || source != destination
        {
            return Ok(false);
        }
        self.reflink_regular_extent(
            fs_root,
            tree_owner,
            source_inode,
            source_offset,
            destination_inode,
            destination_offset,
            len,
        )?;
        Ok(true)
    }

    /// Rewrites one inode through the regular-extent COW transaction.  This
    /// is the balance worker's relocation primitive: every old mapping is
    /// retired only after its replacement extent, checksum and free-space
    /// image are staged, so an interrupted relocation leaves the old extent
    /// reachable.  Chunk-selection policy lives above this primitive.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn relocate_inode_data(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
    ) -> AxResult<()> {
        let item = self.inode_item(fs_root, tree_owner, inode)?;
        if item.size == 0 {
            return Ok(());
        }
        let mut image = Vec::new();
        image
            .try_reserve_exact(usize::try_from(item.size).map_err(|_| AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        image.resize(
            usize::try_from(item.size).map_err(|_| AxError::NoMemory)?,
            0,
        );
        if self.read_file_at(fs_root, tree_owner, inode, 0, &mut image)? != image.len() {
            return Err(AxError::Io);
        }
        self.replace_file_with_regular(fs_root, tree_owner, inode, &image)
    }

    /// Relocates data into one selected data block group.  The target is
    /// checked at reservation time, before any data write is issued, which
    /// makes a balance scheduler's chunk/device choice an enforceable
    /// placement constraint rather than post-hoc bookkeeping.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn relocate_inode_data_to_chunk(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
        chunk_logical: u64,
        chunk_len: u64,
    ) -> AxResult<()> {
        if chunk_logical == 0
            || chunk_len == 0
            || !self.volume.data_contains(chunk_logical, chunk_len)
        {
            return Err(AxError::InvalidInput);
        }
        let item = self.inode_item(fs_root, tree_owner, inode)?;
        if item.size == 0 {
            return Ok(());
        }
        let mut image = Vec::new();
        image
            .try_reserve_exact(usize::try_from(item.size).map_err(|_| AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        image.resize(
            usize::try_from(item.size).map_err(|_| AxError::NoMemory)?,
            0,
        );
        if self.read_file_at(fs_root, tree_owner, inode, 0, &mut image)? != image.len() {
            return Err(AxError::Io);
        }
        let segment = RangeSegment::CowData {
            offset: 0,
            bytes: image,
        };
        self.replace_file_with_range_segments_at(
            fs_root,
            tree_owner,
            inode,
            item.size,
            core::slice::from_ref(&segment),
            Some((chunk_logical, chunk_len)),
        )
    }

    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn balance_state(&self) -> BtrfsBalanceState {
        let mut state = self.balance;
        state.planned_inodes = self.core.planned_balance_inodes();
        state
    }

    /// Allocates and persists one complete block group.  Physical member
    /// reservations remain private until the CHUNK tree, all DEV_EXTENTs and
    /// every affected native DEV_ITEM have reached the topology generation;
    /// publication failure returns every stripe to the physical allocator.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn allocate_chunk(
        &mut self,
        physical: &super::BtrfsAllocator,
        logical: u64,
        logical_len: u64,
        stripe_len: u64,
        profile: super::ChunkProfile,
        requested_stripes: usize,
        block_group_flags: u64,
        system_chunk_array: &[u8],
    ) -> AxResult<()> {
        if logical == 0 || logical_len == 0 || stripe_len == 0 || block_group_flags & 7 == 0 {
            return Err(AxError::InvalidInput);
        }
        if self.volume.chunks().iter().any(|chunk| {
            logical < chunk.logical.saturating_add(chunk.length)
                && chunk.logical < logical.saturating_add(logical_len)
        }) {
            return Err(AxError::ResourceBusy);
        }
        let (geometry, mut reservation) = physical.reserve_chunk(
            profile,
            requested_stripes,
            logical_len,
            stripe_len,
            u64::from(self.superblock.sectorsize),
        )?;
        let result = (|| {
            let mut stripes = Vec::new();
            stripes
                .try_reserve_exact(reservation.stripes.len())
                .map_err(|_| AxError::NoMemory)?;
            for stripe in &reservation.stripes {
                stripes.push(super::Stripe {
                    device: stripe.device,
                    physical: stripe.physical,
                });
            }
            let sector = u64::from(self.superblock.sectorsize);
            if logical % sector != 0
                || logical_len % sector != 0
                || geometry.stripe_len % sector != 0
            {
                return Err(AxError::InvalidInput);
            }
            let chunk = super::Chunk {
                logical,
                length: logical_len,
                stripe_len: geometry.stripe_len,
                profile,
                sub_stripes: geometry.sub_stripes,
                block_group_flags,
                stripes,
            };
            let mut items = self.chunk_tree_items()?;
            let mut device_items = self.device_items_from_chunk_tree(&items)?;
            let encoded = chunk.encode_item(|index| self.volume.member_devid(index))?;
            Self::set_raw_item(
                &mut items,
                RawTreeItem {
                    key: TreeItemKey {
                        objectid: logical,
                        item_type: BtrfsVolume::CHUNK_ITEM_TYPE,
                        offset: 0,
                    },
                    value: encoded,
                },
            )?;
            for stripe in &reservation.stripes {
                let devid = self
                    .volume
                    .member_devid(stripe.device)
                    .ok_or(AxError::NoSuchDevice)?;
                let device = device_items.get_mut(&devid).ok_or(AxError::NoSuchDevice)?;
                device.bytes_used = device
                    .bytes_used
                    .checked_add(stripe.len)
                    .ok_or(AxError::NoMemory)?;
                if device.bytes_used > device.total_bytes {
                    return Err(AxError::StorageFull);
                }
                let extent = super::BtrfsDevExtent {
                    chunk_tree: TreeId::Chunk as u64,
                    chunk_objectid: logical,
                    chunk_offset: 0,
                    length: stripe.len,
                };
                Self::set_raw_item(
                    &mut items,
                    RawTreeItem {
                        key: TreeItemKey {
                            objectid: devid,
                            item_type: DEV_EXTENT,
                            offset: stripe.physical,
                        },
                        value: extent.encode()?,
                    },
                )?;
            }
            for (&devid, item) in &device_items {
                Self::set_raw_item(
                    &mut items,
                    RawTreeItem {
                        key: TreeItemKey {
                            objectid: devid,
                            item_type: DEV_ITEM,
                            offset: 0,
                        },
                        value: item.encode()?,
                    },
                )?;
            }
            let mut chunks = self.volume.chunks().to_vec();
            chunks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            chunks.push(chunk);
            let mut stage = self
                .volume
                .stage_member_change(BtrfsDeviceTopologyChange::Keep)?;
            BtrfsVolume::stage_chunks(&mut stage, chunks)?;
            BtrfsVolume::validate_staged_system_chunks(&stage, system_chunk_array)?;
            self.commit_staged_topology_transaction(
                self.begin_transaction(),
                &items,
                system_chunk_array,
                device_items,
                stage,
                None,
                None,
                &[],
                Some((logical, logical_len)),
                None,
                self.superblock.log_root,
                self.superblock.bytes_used,
            )
        })();
        match result {
            Ok(_) => physical.commit(&mut reservation),
            Err(error) => {
                physical.release(reservation)?;
                Err(error)
            }
        }
    }

    /// Starts an explicit evacuation.  Work is accumulated in bounded,
    /// explicitly committed relocation batches.  A paused worker therefore
    /// has either no published relocation, or a complete root-switch
    /// generation; an in-memory cursor is never mistaken for completion.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn begin_device_evacuation(&mut self, devid: u64) -> AxResult<()> {
        if self.volume.member_index(devid).is_none() {
            return Err(AxError::NoSuchDevice);
        }
        if self.balance.evacuating_devid.is_some() {
            return Err(AxError::ResourceBusy);
        }
        self.balance.evacuating_devid = Some(devid);
        self.balance.paused = false;
        Ok(())
    }

    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn pause_balance(&mut self) {
        self.balance.paused = true;
    }
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn resume_balance(&mut self) -> AxResult<()> {
        if self.balance.evacuating_devid.is_none() {
            return Err(AxError::BadState);
        }
        self.balance.paused = false;
        Ok(())
    }

    /// Begins a data-profile relocation batch.  Target admission excludes
    /// every data block group with a stripe on the evacuation source; each
    /// subsequent inode reservation is made against the same allocator view
    /// so ENOSPC is discovered before any member receives a data write.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn begin_balance_relocation_plan(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
    ) -> AxResult<BtrfsRelocationPlan> {
        let devid = self.balance.evacuating_devid.ok_or(AxError::BadState)?;
        if self.balance.paused {
            return Err(AxError::ResourceBusy);
        }
        let source = self
            .volume
            .member_index(devid)
            .ok_or(AxError::NoSuchDevice)?;
        let mut targets = Vec::new();
        for chunk in self.volume.chunks() {
            if chunk.block_group_flags & 1 != 0
                && !chunk.stripes.iter().any(|stripe| stripe.device == source)
            {
                targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                targets.push((chunk.logical, chunk.length));
            }
        }
        if targets.is_empty() {
            return Err(AxError::StorageFull);
        }
        Ok(BtrfsRelocationPlan {
            core: self.core.clone(),
            fs_root,
            tree_owner,
            source_member: source,
            base_generation: self.superblock.generation,
            base_fs_root: {
                let root = self.subvolume_root(tree_owner)?;
                if root != fs_root {
                    return Err(AxError::InvalidInput);
                }
                root
            },
            targets,
            allocator: self.logical_allocator()?,
            jobs: Vec::new(),
        })
    }

    /// Appends one inode to a relocation batch without changing any tree or
    /// writing any sector.  The full regular file image is intentionally
    /// materialized now: relocation is a physical rewrite, not a sequence of
    /// partial extent substitutions that could publish a mixed source/target
    /// mapping.  All reservations remain reclaimable until commit starts.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn balance_plan_inode(
        &mut self,
        plan: &mut BtrfsRelocationPlan,
        inode: u64,
    ) -> AxResult<()> {
        if self.balance.paused
            || self
                .volume
                .member_index(self.balance.evacuating_devid.ok_or(AxError::BadState)?)
                != Some(plan.source_member)
        {
            return Err(AxError::ResourceBusy);
        }
        if plan.jobs.iter().any(|job| job.inode == inode) {
            return Err(AxError::AlreadyExists);
        }
        let old_extents = self.file_extents(plan.fs_root, plan.tree_owner, inode)?;
        let mut affected = false;
        for (_, extent) in &old_extents {
            if extent.owns_physical_storage()
                && self.volume.logical_range_uses_member(
                    extent.disk_bytenr,
                    extent.disk_num_bytes,
                    plan.source_member,
                )?
            {
                affected = true;
                break;
            }
        }
        if !affected {
            return Err(AxError::NotFound);
        }
        let item = self.inode_item(plan.fs_root, plan.tree_owner, inode)?;
        if item.size == 0 {
            return Err(AxError::NotFound);
        }
        let sector = u64::from(self.superblock.sectorsize);
        let allocated_len =
            item.size.checked_add(sector - 1).ok_or(AxError::NoMemory)? / sector * sector;
        let reservation = plan
            .allocator
            .reserve_where(allocated_len, sector, |logical, len| {
                self.volume.data_contains(logical, len)
                    && plan.targets.iter().any(|&(start, span)| {
                        logical >= start
                            && logical
                                .checked_add(len)
                                .is_some_and(|end| end <= start.saturating_add(span))
                    })
            })?;
        let result = (|| {
            let mut stored = Vec::new();
            stored
                .try_reserve_exact(usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?)
                .map_err(|_| AxError::NoMemory)?;
            stored.resize(
                usize::try_from(allocated_len).map_err(|_| AxError::NoMemory)?,
                0,
            );
            if self.read_file_at(
                plan.fs_root,
                plan.tree_owner,
                inode,
                0,
                &mut stored[..usize::try_from(item.size).map_err(|_| AxError::NoMemory)?],
            )? != usize::try_from(item.size).map_err(|_| AxError::NoMemory)?
            {
                return Err(AxError::Io);
            }
            plan.jobs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            self.core.add_planned_balance_inode()?;
            plan.jobs.push(BtrfsRelocationJob {
                inode,
                old_extents,
                stored,
                logical_len: item.size,
                reservation,
            });
            Ok(())
        })();
        if let Err(error) = result {
            plan.allocator.release(reservation)?;
            return Err(error);
        }
        Ok(())
    }

    /// Publishes all previously planned inode relocations in one shared COW
    /// transaction.  Planning and all qgroup/delayed-ref/free-space images
    /// happen before the first data write.  Once a write is attempted the
    /// reservations are deliberately committed even on later failure: this
    /// prevents an unreachable but possibly written extent from being handed
    /// out again before recovery/scrub can account for it.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn commit_balance_relocation_plan(
        &mut self,
        mut plan: BtrfsRelocationPlan,
    ) -> AxResult<()> {
        if plan.jobs.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let job_count = u64::try_from(plan.jobs.len()).map_err(|_| AxError::NoMemory)?;
        if self.balance.paused
            || self
                .volume
                .member_index(self.balance.evacuating_devid.ok_or(AxError::BadState)?)
                != Some(plan.source_member)
            || self.superblock.generation != plan.base_generation
            || self.subvolume_root(plan.tree_owner)? != plan.base_fs_root
        {
            plan.discard_unwritten()?;
            return Err(AxError::ResourceBusy);
        }
        // The batch owns the exact extent set observed while its data image
        // was read.  Never erase mappings introduced after that snapshot.
        for job in &plan.jobs {
            if self.file_extents(plan.fs_root, plan.tree_owner, job.inode)? != job.old_extents {
                plan.discard_unwritten()?;
                return Err(AxError::ResourceBusy);
            }
        }
        let sector = u64::from(self.superblock.sectorsize);
        let generation = self
            .superblock
            .generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let mut planner = match self.mutation_planner(plan.tree_owner) {
            Ok(planner) => planner,
            Err(error) => {
                plan.discard_unwritten()?;
                return Err(error);
            }
        };
        let plan_result = (|| {
            let mut newly_free = Vec::new();
            let mut qgroup_referenced = 0i64;
            let mut qgroup_exclusive = 0i64;
            let mut allocated = 0u64;
            let mut freed = 0u64;
            for job in &plan.jobs {
                let old_keys: Vec<_> = planner
                    .tree_items(plan.tree_owner)?
                    .iter()
                    .filter(|item| {
                        item.key.objectid == job.inode && item.key.item_type == EXTENT_DATA
                    })
                    .map(|item| item.key)
                    .collect();
                for key in old_keys {
                    let _ = planner.delete_item(plan.tree_owner, key)?;
                }
                let allocated_len =
                    u64::try_from(job.stored.len()).map_err(|_| AxError::NoMemory)?;
                let mut checksums = Vec::new();
                checksums
                    .try_reserve_exact(
                        job.stored.len() / usize::try_from(sector).map_err(|_| AxError::Io)? * 4,
                    )
                    .map_err(|_| AxError::NoMemory)?;
                for block in job
                    .stored
                    .chunks_exact(usize::try_from(sector).map_err(|_| AxError::Io)?)
                {
                    checksums.extend_from_slice(&crc32c(block).to_le_bytes());
                }
                planner.set_item(
                    plan.tree_owner,
                    TreeItemKey {
                        objectid: job.inode,
                        item_type: EXTENT_DATA,
                        offset: 0,
                    },
                    super::encode_regular_extent(
                        generation,
                        job.reservation.logical,
                        allocated_len,
                        0,
                        job.logical_len,
                    )?,
                )?;
                planner.set_item(
                    TreeId::Extent as u64,
                    TreeItemKey {
                        objectid: job.reservation.logical,
                        item_type: EXTENT_ITEM,
                        offset: allocated_len,
                    },
                    super::encode_data_extent_item(generation, 1)?,
                )?;
                let mut relation = Vec::new();
                relation
                    .try_reserve_exact(24)
                    .map_err(|_| AxError::NoMemory)?;
                relation.extend_from_slice(&plan.tree_owner.to_le_bytes());
                relation.extend_from_slice(&job.inode.to_le_bytes());
                relation.extend_from_slice(&0u64.to_le_bytes());
                planner.set_item(
                    TreeId::Extent as u64,
                    TreeItemKey {
                        objectid: job.reservation.logical,
                        item_type: EXTENT_DATA_REF,
                        offset: u64::from(crc32c(&relation)),
                    },
                    super::encode_extent_data_ref(plan.tree_owner, job.inode, 0, 1)?,
                )?;
                planner.set_checksum_run(job.reservation.logical, sector, &checksums)?;
                planner
                    .transaction_mut()
                    .add_delayed_ref(super::DelayedRef {
                        bytenr: job.reservation.logical,
                        len: allocated_len,
                        root: plan.tree_owner,
                        owner: job.inode,
                        identity: super::DelayedRefIdentity::Data { file_offset: 0 },
                        delta: 1,
                    })?;
                let bytes = i64::try_from(allocated_len).map_err(|_| AxError::NoMemory)?;
                qgroup_referenced = qgroup_referenced.checked_add(bytes).ok_or(AxError::Io)?;
                qgroup_exclusive = qgroup_exclusive.checked_add(bytes).ok_or(AxError::Io)?;
                allocated = allocated
                    .checked_add(allocated_len)
                    .ok_or(AxError::NoMemory)?;
                for (file_offset, extent) in &job.old_extents {
                    if !extent.owns_physical_storage() {
                        continue;
                    }
                    let became_free = planner.retire_regular_extent(
                        plan.tree_owner,
                        job.inode,
                        *file_offset,
                        extent.disk_bytenr,
                        extent.disk_num_bytes,
                    )?;
                    planner
                        .transaction_mut()
                        .add_delayed_ref(super::DelayedRef {
                            bytenr: extent.disk_bytenr,
                            len: extent.disk_num_bytes,
                            root: plan.tree_owner,
                            owner: job.inode,
                            identity: super::DelayedRefIdentity::Data {
                                file_offset: *file_offset,
                            },
                            delta: -1,
                        })?;
                    let old_bytes =
                        i64::try_from(extent.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
                    qgroup_referenced = qgroup_referenced
                        .checked_sub(old_bytes)
                        .ok_or(AxError::Io)?;
                    if became_free {
                        planner.remove_checksum_range(
                            extent.disk_bytenr,
                            sector,
                            extent.disk_num_bytes,
                        )?;
                        newly_free.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                        newly_free.push((extent.disk_bytenr, extent.disk_num_bytes));
                        qgroup_exclusive =
                            qgroup_exclusive.checked_sub(old_bytes).ok_or(AxError::Io)?;
                        freed = freed
                            .checked_add(extent.disk_num_bytes)
                            .ok_or(AxError::NoMemory)?;
                    }
                }
                let inode_key = TreeItemKey {
                    objectid: job.inode,
                    item_type: INODE_ITEM,
                    offset: 0,
                };
                let index = planner
                    .tree_items(plan.tree_owner)?
                    .binary_search_by_key(&inode_key, |item| item.key)
                    .map_err(|_| AxError::NotFound)?;
                let mut inode =
                    BtrfsInodeItem::decode(&planner.tree_items(plan.tree_owner)?[index].value)?;
                inode.transid = generation;
                inode.size = job.logical_len;
                inode.nbytes = allocated_len;
                planner.set_item(plan.tree_owner, inode_key, inode.encode())?;
            }
            let mut post_free = plan.allocator.free_extents();
            post_free
                .try_reserve(newly_free.len())
                .map_err(|_| AxError::NoMemory)?;
            post_free.extend_from_slice(&newly_free);
            post_free.sort_unstable_by_key(|(logical, _)| *logical);
            let mut merged: Vec<(u64, u64)> = Vec::new();
            merged
                .try_reserve_exact(post_free.len())
                .map_err(|_| AxError::NoMemory)?;
            for (logical, len) in post_free {
                if let Some((start, span)) = merged.last_mut() {
                    let end = (*start).checked_add(*span).ok_or(AxError::Io)?;
                    if logical < end {
                        return Err(AxError::Io);
                    }
                    if logical == end {
                        *span = (*span).checked_add(len).ok_or(AxError::NoMemory)?;
                        continue;
                    }
                }
                merged.push((logical, len));
            }
            planner.replace_free_space_extents(&merged)?;
            // A quota tree is optional.  Do not manufacture an in-memory
            // qgroup for a volume that has no durable QGROUP_INFO image;
            // when it is present, the transaction and its on-media counter
            // move together in this same generation.
            if planner.tree_items(TreeId::Quota as u64).is_ok() {
                planner.transaction_mut().charge_qgroup(
                    super::QgroupId {
                        level: 0,
                        id: plan.tree_owner,
                    },
                    qgroup_referenced,
                    qgroup_exclusive,
                )?;
                planner.charge_qgroup_on_disk(
                    super::QgroupId {
                        level: 0,
                        id: plan.tree_owner,
                    },
                    qgroup_referenced,
                    qgroup_exclusive,
                    generation,
                )?;
            }
            Ok((allocated, freed, newly_free))
        })();
        let (allocated, freed, newly_free) = match plan_result {
            Ok(value) => value,
            Err(error) => {
                plan.discard_unwritten()?;
                return Err(error);
            }
        };
        if let Err(error) = planner.transaction_mut().preflight_persist() {
            plan.discard_unwritten()?;
            return Err(error);
        }
        let bytes_used = match self
            .superblock
            .bytes_used
            .checked_add(allocated)
            .ok_or(AxError::NoMemory)?
            .checked_sub(freed)
            .ok_or(AxError::Io)
        {
            Ok(bytes_used) => bytes_used,
            Err(error) => {
                plan.discard_unwritten()?;
                return Err(error);
            }
        };
        // All fallible accounting/preflight work is complete.  This is the
        // first point data reaches a device.
        for job in &plan.jobs {
            if let Err(error) = self
                .volume
                .write_data_range(job.reservation.logical, &job.stored)
            {
                for job in &mut plan.jobs {
                    let _ = plan.allocator.seal(&mut job.reservation);
                }
                return Err(error);
            }
        }
        match self.commit_mutation_planner(planner, 0, bytes_used) {
            Ok(_) => {
                for job in &mut plan.jobs {
                    plan.allocator.commit(&mut job.reservation)?;
                }
                for (logical, len) in newly_free {
                    plan.allocator.add_free(logical, len)?;
                }
                self.balance.relocated_inodes = self
                    .balance
                    .relocated_inodes
                    .checked_add(job_count)
                    .ok_or(AxError::NoMemory)?;
                self.balance.committed_batches = self
                    .balance
                    .committed_batches
                    .checked_add(1)
                    .ok_or(AxError::NoMemory)?;
                Ok(())
            }
            Err(error) => {
                for job in &mut plan.jobs {
                    let _ = plan.allocator.seal(&mut job.reservation);
                }
                Err(error)
            }
        }
    }

    /// Explicitly abandons an unpublished batch.  A plan owns logical
    /// reservations, so dropping it without this call would be ambiguous:
    /// this method is the only pre-write cancellation boundary and restores
    /// both free space and the externally visible resumable-work count.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn discard_balance_relocation_plan(
        &mut self,
        mut plan: BtrfsRelocationPlan,
    ) -> AxResult<()> {
        plan.discard_unwritten()?;
        Ok(())
    }

    /// Compatibility entry point for control planes that submit one inode at
    /// a time.  It now still travels through the plan/commit machinery, so
    /// it cannot regain the old private immediate-COW implementation.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn balance_relocate_inode(
        &mut self,
        fs_root: u64,
        tree_owner: u64,
        inode: u64,
    ) -> AxResult<()> {
        let mut plan = self.begin_balance_relocation_plan(fs_root, tree_owner)?;
        self.balance_plan_inode(&mut plan, inode)?;
        self.commit_balance_relocation_plan(plan)
    }

    /// Checks every currently reachable ROOT_ITEM tree, plus the two roots
    /// named directly by the superblock.  A balance decision cannot infer
    /// that a device is evacuated merely because Chunk/Root/FreeSpace happen
    /// to be elsewhere: Extent, Csum, Quota and arbitrary subvolume roots
    /// are equally live metadata.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    fn any_live_tree_uses_member(&self, member: usize) -> AxResult<bool> {
        let mut roots = Vec::new();
        roots.try_reserve(2).map_err(|_| AxError::NoMemory)?;
        roots.push((self.superblock.chunk_root, TreeId::Chunk as u64));
        roots.push((self.superblock.root, TreeId::Root as u64));
        for item in self
            .root_tree_items()?
            .iter()
            .filter(|item| item.key.item_type == ROOT_ITEM && item.key.offset == 0)
        {
            let root = BtrfsRootItem::decode(&item.value)?;
            if root.bytenr == 0 {
                return Err(AxError::Io);
            }
            let image = self.volume.read_checked_tree_block(
                root.bytenr,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let owner = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                root.bytenr,
            )?
            .owner();
            if owner == 0 {
                return Err(AxError::Io);
            }
            roots.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            roots.push((root.bytenr, owner));
        }
        for (logical, owner) in roots {
            let mut nodes = BTreeSet::new();
            self.collect_tree_nodes(logical, owner, None, &mut nodes)?;
            for node in nodes {
                if self.volume.logical_range_uses_member(
                    node,
                    u64::from(self.superblock.nodesize),
                    member,
                )? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// COW-relocates the topology roots (chunk, root and free-space trees)
    /// into one already published metadata/system block group that has no
    /// stripe on the evacuation source.  This is the metadata/system balance
    /// producer's durable unit: its final free-space image, root switch and
    /// device/chunk accounting are committed together, so a later remove
    /// cannot merely hide live topology blocks behind a changed member map.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn balance_relocate_topology_metadata(
        &mut self,
        target_logical: u64,
        target_len: u64,
        system_chunk_array: &[u8],
    ) -> AxResult<u64> {
        if self.superblock.log_root != 0 {
            return Err(AxError::ResourceBusy);
        }
        let devid = self.balance.evacuating_devid.ok_or(AxError::BadState)?;
        if self.balance.paused || target_logical == 0 || target_len == 0 {
            return Err(AxError::InvalidInput);
        }
        let source = self
            .volume
            .member_index(devid)
            .ok_or(AxError::NoSuchDevice)?;
        let target = self
            .volume
            .chunks()
            .iter()
            .find(|chunk| {
                chunk.logical == target_logical
                    && chunk.length == target_len
                    && chunk.block_group_flags & 6 != 0
                    && !chunk.stripes.iter().any(|stripe| stripe.device == source)
            })
            .map(|chunk| (chunk.logical, chunk.length))
            .ok_or(AxError::StorageFull)?;
        let affected = self.any_live_tree_uses_member(source)?;
        if !affected {
            return Err(AxError::NotFound);
        }
        let items = self.chunk_tree_items()?;
        let device_items = self.device_items_from_chunk_tree(&items)?;
        let mut stage = self
            .volume
            .stage_member_change(BtrfsDeviceTopologyChange::Keep)?;
        BtrfsVolume::stage_chunks(&mut stage, self.volume.chunks().to_vec())?;
        BtrfsVolume::validate_staged_system_chunks(&stage, system_chunk_array)?;
        // Rebuild every reachable tree in this one generation.  Limiting the
        // rewrite list to topology trees made a later device removal capable
        // of stranding a Csum/Quota/subvolume node on the evacuated member.
        let mut rewrites = Vec::new();
        for item in self
            .root_tree_items()?
            .iter()
            .filter(|item| item.key.item_type == ROOT_ITEM && item.key.offset == 0)
        {
            let objectid = item.key.objectid;
            if matches!(objectid, id if id == TreeId::Chunk as u64 || id == TreeId::Root as u64 || id == TreeId::FreeSpace as u64 || id == TreeId::Extent as u64)
            {
                continue;
            }
            let root = BtrfsRootItem::decode(&item.value)?;
            let image = self.volume.read_checked_tree_block(
                root.bytenr,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let old_tree_owner = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                root.bytenr,
            )?
            .owner();
            if old_tree_owner == 0 {
                return Err(AxError::Io);
            }
            let mut tree_items = Vec::new();
            self.collect_tree_items(
                root.bytenr,
                old_tree_owner,
                &mut BTreeSet::new(),
                &mut tree_items,
            )?;
            rewrites.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            rewrites.push(BtrfsTreeRewrite {
                root_objectid: objectid,
                tree_owner: objectid,
                old_tree_owner,
                items: tree_items,
            });
        }
        rewrites.sort_by_key(|rewrite| rewrite.root_objectid);
        self.commit_staged_topology_transaction(
            self.begin_transaction(),
            &items,
            system_chunk_array,
            device_items,
            stage,
            None,
            None,
            &rewrites,
            None,
            Some(target),
            self.superblock.log_root,
            self.superblock.bytes_used,
        )
    }

    /// Applies add/remove/replace only through a staged routing map and a
    /// complete CHUNK-tree rewrite.  The tree and all new-member superblocks
    /// are written through the candidate volume first; the mounted map is
    /// swapped only after that final durability point.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn change_device(
        &mut self,
        change: BtrfsMountDeviceChange,
        system_chunk_array: &[u8],
    ) -> AxResult<u64> {
        let items = self.chunk_tree_items()?;
        self.change_device_with_chunk_tree(change, items, system_chunk_array)
    }

    /// Same device operation with a caller-provided final CHUNK-tree image.
    /// This is the balance scheduler's hand-off: it evacuates/copies a block
    /// group first, then removes or replaces the outgoing member in the very
    /// tree generation that is checked against the staged routing map.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn change_device_with_chunk_tree(
        &mut self,
        change: BtrfsMountDeviceChange,
        mut items: Vec<RawTreeItem>,
        system_chunk_array: &[u8],
    ) -> AxResult<u64> {
        if self.superblock.log_root != 0 {
            return Err(AxError::ResourceBusy);
        }
        items.sort_by_key(|item| item.key);
        if items.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(AxError::Io);
        }
        let mut device_items = self.device_items_from_chunk_tree(&items)?;
        let (stage_change, changed_devid, replacement, clear_balance, require_evacuated) =
            match change {
                BtrfsMountDeviceChange::Add { item, device } => {
                    if item.devid == 0
                        || item.fsid != self.superblock.fsid
                        || device_items.contains_key(&item.devid)
                    {
                        return Err(AxError::InvalidInput);
                    }
                    (
                        BtrfsDeviceTopologyChange::Add {
                            devid: item.devid,
                            device,
                        },
                        item.devid,
                        Some(item),
                        false,
                        false,
                    )
                }
                BtrfsMountDeviceChange::Remove { devid } => {
                    if self.balance.evacuating_devid == Some(devid) && self.balance.paused {
                        return Err(AxError::ResourceBusy);
                    }
                    if items
                        .iter()
                        .any(|item| item.key.objectid == devid && item.key.item_type == DEV_EXTENT)
                    {
                        return Err(AxError::ResourceBusy);
                    }
                    (
                        BtrfsDeviceTopologyChange::Remove { devid },
                        devid,
                        None,
                        true,
                        true,
                    )
                }
                BtrfsMountDeviceChange::Replace { item, device } => {
                    if item.devid == 0 || item.fsid != self.superblock.fsid {
                        return Err(AxError::InvalidInput);
                    }
                    (
                        BtrfsDeviceTopologyChange::Replace {
                            devid: item.devid,
                            device,
                        },
                        item.devid,
                        Some(item),
                        false,
                        true,
                    )
                }
            };
        if require_evacuated
            && items
                .iter()
                .any(|item| item.key.objectid == changed_devid && item.key.item_type == DEV_EXTENT)
        {
            // Replace has no hidden physical-copy shortcut.  The balance
            // transaction must first copy every device extent and present a
            // final tree with no old mappings; otherwise switching routes
            // would make valid logical extents read from an empty disk.
            return Err(AxError::ResourceBusy);
        }
        let replacing = replacement.is_some();
        let mut stage = self.volume.stage_member_change(stage_change)?;
        match replacement {
            Some(item) => {
                device_items.insert(changed_devid, item);
                Self::set_raw_item(
                    &mut items,
                    RawTreeItem {
                        key: TreeItemKey {
                            objectid: changed_devid,
                            item_type: DEV_ITEM,
                            offset: 0,
                        },
                        value: item.encode()?,
                    },
                )?;
            }
            None => {
                device_items
                    .remove(&changed_devid)
                    .ok_or(AxError::NoSuchDevice)?;
                items.retain(|item| {
                    item.key.objectid != changed_devid
                        || !matches!(item.key.item_type, DEV_ITEM | DEV_EXTENT)
                });
            }
        }
        let chunks = Self::decode_topology_chunks(&items, |devid| {
            BtrfsVolume::stage_member_index(&stage, devid)
        })?;
        BtrfsVolume::stage_chunks(&mut stage, chunks)?;
        if replacing && BtrfsVolume::staged_member_has_stripes(&stage, changed_devid)? {
            return Err(AxError::ResourceBusy);
        }
        BtrfsVolume::validate_staged_system_chunks(&stage, system_chunk_array)?;
        let generation = self.commit_staged_topology_transaction(
            self.begin_transaction(),
            &items,
            system_chunk_array,
            device_items,
            stage,
            None,
            None,
            &[],
            None,
            None,
            self.superblock.log_root,
            self.superblock.bytes_used,
        )?;
        if clear_balance {
            self.balance = BtrfsBalanceState::default();
        }
        Ok(generation)
    }

    /// Reads checksum-covered file data a sector at a time.  The checksum
    /// tree stores packed consecutive digests, so the key range—not merely an
    /// exact key comparison—determines which digest covers a sector.
    fn read_data_checked(&self, logical: u64, out: &mut [u8]) -> AxResult<()> {
        let sector = usize::try_from(self.superblock.sectorsize).map_err(|_| AxError::Io)?;
        if out.len() % sector != 0 || logical % sector as u64 != 0 {
            return Err(AxError::Io);
        }
        for (index, block) in out.chunks_exact_mut(sector).enumerate() {
            let address = logical
                .checked_add((index * sector) as u64)
                .ok_or(AxError::Io)?;
            self.volume.read_logical(address, block)?;
            let checksum = self.data_checksum(address)?;
            if !checksum.verify(block) {
                return Err(AxError::Io);
            }
        }
        Ok(())
    }

    fn data_checksum(&self, logical: u64) -> AxResult<Checksum> {
        const CSUM_TREE: u64 = TreeId::Csum as u64;
        let root = self.subvolume_root(CSUM_TREE)?;
        let mut items = Vec::new();
        self.collect_tree_items(root, CSUM_TREE, &mut BTreeSet::new(), &mut items)?;
        let checksum_size = 4usize; // only CRC32C is admitted by BtrfsSuperblock
        for item in items {
            if item.key.item_type != CSUM_ITEM || logical < item.key.objectid {
                continue;
            }
            let delta = logical - item.key.objectid;
            if delta % u64::from(self.superblock.sectorsize) != 0 {
                continue;
            }
            let index = usize::try_from(delta / u64::from(self.superblock.sectorsize))
                .map_err(|_| AxError::Io)?;
            let offset = index.checked_mul(checksum_size).ok_or(AxError::Io)?;
            if let Some(bytes) = item.value.get(offset..offset + checksum_size) {
                return Checksum::from_disk(self.superblock.csum_type, bytes);
            }
        }
        Err(AxError::Io)
    }

    /// Builds the checksum run for a regular extent that exists only in a
    /// native log tree.  The sectors were written before the crash; replay
    /// re-reads them and records exactly the digest bytes that the new Csum
    /// tree image will publish.  Prealloc has no initialized-data checksum.
    fn logged_extent_checksums(&self, extent: &BtrfsFileExtent) -> AxResult<Option<Vec<u8>>> {
        if extent.kind != super::BtrfsExtentKind::Regular {
            return Ok(None);
        }
        if !LoggedExtentTransition::supports_accounting(extent) {
            return Err(AxError::OperationNotSupported);
        }
        let sector = usize::try_from(self.superblock.sectorsize).map_err(|_| AxError::Io)?;
        if extent.disk_num_bytes == 0
            || extent.disk_num_bytes % u64::try_from(sector).map_err(|_| AxError::Io)? != 0
        {
            return Err(AxError::Io);
        }
        let sectors = usize::try_from(
            extent.disk_num_bytes / u64::try_from(sector).map_err(|_| AxError::Io)?,
        )
        .map_err(|_| AxError::NoMemory)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(sectors.checked_mul(4).ok_or(AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        let mut block = Vec::new();
        block
            .try_reserve_exact(sector)
            .map_err(|_| AxError::NoMemory)?;
        block.resize(sector, 0);
        for index in 0..sectors {
            let logical = extent
                .disk_bytenr
                .checked_add(
                    u64::try_from(index)
                        .map_err(|_| AxError::NoMemory)?
                        .checked_mul(u64::try_from(sector).map_err(|_| AxError::Io)?)
                        .ok_or(AxError::Io)?,
                )
                .ok_or(AxError::Io)?;
            self.volume.read_logical(logical, &mut block)?;
            output.extend_from_slice(&crc32c(&block).to_le_bytes());
        }
        Ok(Some(output))
    }

    fn extent_reference_count(&self, bytenr: u64, len: u64) -> AxResult<u64> {
        let root = self.subvolume_root(TreeId::Extent as u64)?;
        let key = TreeItemKey {
            objectid: bytenr,
            item_type: super::EXTENT_ITEM,
            offset: len,
        };
        let value = self
            .lookup(root, TreeId::Extent as u64, key)?
            .ok_or(AxError::Io)?;
        let raw: [u8; 8] = value
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(AxError::Io)?;
        let references = u64::from_le_bytes(raw);
        if references == 0 {
            return Err(AxError::Io);
        }
        Ok(references)
    }

    /// Descends a checked B-tree using the precise internal-node lower-bound
    /// rule.  Each pointer has both its expected logical address and owner
    /// checked on every hop, preventing a corrupted child reference from
    /// being reinterpreted as another metadata tree.
    pub fn lookup(
        &self,
        root_logical: u64,
        owner: u64,
        wanted: TreeItemKey,
    ) -> AxResult<Option<Vec<u8>>> {
        let mut logical = root_logical;
        loop {
            let image = self.volume.read_checked_tree_block(
                logical,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let block = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                logical,
            )?;
            if block.owner() != owner {
                return Err(AxError::Io);
            }
            if block.level() == 0 {
                return match block.find_leaf(wanted)? {
                    None => Ok(None),
                    Some(TreeLeafItem { value, .. }) => {
                        let mut owned = Vec::new();
                        owned
                            .try_reserve_exact(value.len())
                            .map_err(|_| AxError::NoMemory)?;
                        owned.extend_from_slice(value);
                        Ok(Some(owned))
                    }
                };
            }
            let TreeChild {
                bytenr, generation, ..
            } = block.child_for(wanted)?;
            if generation > self.superblock.generation {
                return Err(AxError::Io);
            }
            let image = self.volume.read_checked_tree_block(
                bytenr,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let child = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                bytenr,
            )?;
            if child.owner() != owner || child.generation() != generation {
                return Err(AxError::Io);
            }
            logical = bytenr;
        }
    }

    /// Writes a freshly allocated COW leaf, persists it, and publishes it as
    /// the new superblock root.  It is intentionally a primitive rather than
    /// an advertised filesystem operation: callers must have already updated
    /// the root/chunk/extent trees and supplied their matching new root
    /// addresses.  The method provides the essential ordering boundary:
    /// node writes -> flush -> redundant superblock publication.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn publish_cow_leaf(
        &mut self,
        logical: u64,
        owner: u64,
        items: &[TreeWriteItem<'_>],
        new_root: u64,
        new_chunk_root: u64,
        new_log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        let generation = self
            .superblock
            .generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let image = BtrfsTreeBlock::encode_leaf(
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            logical,
            generation,
            owner as u64,
            items,
        )?;
        self.volume.write_tree_block(logical, &image)?;
        self.volume.flush()?;
        self.volume.publish_superblock(
            &self.superblock,
            generation,
            new_root,
            new_chunk_root,
            new_log_root,
            bytes_used,
        )?;
        self.superblock = BtrfsSuperblock::decode(
            &self.superblock.prepare_commit(
                generation,
                new_root,
                new_chunk_root,
                new_log_root,
                bytes_used,
            )?,
            self.superblock.bytenr,
        )?;
        Ok(generation)
    }

    /// Rebuilds a complete metadata tree into newly allocated COW nodes.
    /// Nodes are written strictly bottom-up; the returned address is not
    /// reachable until the caller publishes the corresponding root item or
    /// superblock in its surrounding transaction.  Allocation is injected so
    /// the extent/chunk-tree transaction remains the sole authority over
    /// logical and physical free-space accounting.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn rewrite_tree_cow(
        &self,
        owner: u64,
        generation: u64,
        items: &[RawTreeItem],
        mut allocate: impl FnMut() -> AxResult<u64>,
    ) -> AxResult<u64> {
        if owner == 0 || generation == 0 || items.is_empty() {
            return Err(AxError::InvalidInput);
        }
        if items.windows(2).any(|pair| pair[0].key >= pair[1].key) {
            return Err(AxError::InvalidInput);
        }
        let nodesize = self.superblock.nodesize as usize;
        // Complete all growth allocations used by the writer before any COW
        // block reaches media.  The exact preflight is also the reservation
        // contract used by topology commits, so its node count is a safe
        // upper bound for the leaf-child vector.
        let node_budget = self.preflight_tree_cow_nodes(owner, generation, items)?;
        let mut children = Vec::new();
        children
            .try_reserve_exact(node_budget)
            .map_err(|_| AxError::NoMemory)?;
        let mut start = 0usize;
        while start < items.len() {
            let mut end = start;
            while end < items.len() {
                let candidate: Vec<TreeWriteItem<'_>> = items[start..=end]
                    .iter()
                    .map(|item| TreeWriteItem {
                        key: item.key,
                        value: &item.value,
                    })
                    .collect();
                match BtrfsTreeBlock::encode_leaf(
                    nodesize,
                    &self.superblock.fsid,
                    1,
                    generation,
                    owner,
                    &candidate,
                ) {
                    Ok(_) => {
                        end += 1;
                    }
                    Err(AxError::StorageFull) if end != start => break,
                    Err(error) => return Err(error),
                }
            }
            let bytenr = allocate()?;
            let final_items: Vec<TreeWriteItem<'_>> = items[start..end]
                .iter()
                .map(|item| TreeWriteItem {
                    key: item.key,
                    value: &item.value,
                })
                .collect();
            let image = BtrfsTreeBlock::encode_leaf(
                nodesize,
                &self.superblock.fsid,
                bytenr,
                generation,
                owner,
                &final_items,
            )?;
            self.volume.write_tree_block(bytenr, &image)?;
            children.push(TreeChild {
                key: items[start].key,
                bytenr,
                generation,
            });
            start = end;
        }
        let mut level = 1u8;
        while children.len() > 1 {
            if level == u8::MAX {
                return Err(AxError::StorageFull);
            }
            let mut next = Vec::new();
            // Each internal node consumes at least one child, so this is an
            // exact safe upper bound and prevents a post-write Vec growth.
            next.try_reserve_exact(children.len())
                .map_err(|_| AxError::NoMemory)?;
            let mut start = 0usize;
            while start < children.len() {
                let mut end = start;
                while end < children.len() {
                    match BtrfsTreeBlock::encode_internal(
                        nodesize,
                        &self.superblock.fsid,
                        1,
                        generation,
                        owner,
                        level,
                        &children[start..=end],
                    ) {
                        Ok(_) => {
                            end += 1;
                        }
                        Err(AxError::StorageFull) if end != start => break,
                        Err(error) => return Err(error),
                    }
                }
                let bytenr = allocate()?;
                let image = BtrfsTreeBlock::encode_internal(
                    nodesize,
                    &self.superblock.fsid,
                    bytenr,
                    generation,
                    owner,
                    level,
                    &children[start..end],
                )?;
                self.volume.write_tree_block(bytenr, &image)?;
                next.push(TreeChild {
                    key: children[start].key,
                    bytenr,
                    generation,
                });
                start = end;
            }
            children = next;
            level = level.checked_add(1).ok_or(AxError::StorageFull)?;
        }
        Ok(children[0].bytenr)
    }

    /// Recorded variant used by the extent-tree writer.  A topology commit
    /// must account for every block it COWs, including the newly written
    /// extent-tree blocks themselves, before its root switch is visible.
    fn rewrite_tree_cow_recorded(
        &self,
        owner: u64,
        relation_root: u64,
        generation: u64,
        items: &[RawTreeItem],
        mut allocate: impl FnMut() -> AxResult<u64>,
        records: &mut Vec<TreeBlockRecord>,
    ) -> AxResult<u64> {
        if owner == 0
            || generation == 0
            || items.is_empty()
            || items.windows(2).any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(AxError::InvalidInput);
        }
        let nodesize = self.superblock.nodesize as usize;
        let node_budget = self.preflight_tree_cow_nodes(owner, generation, items)?;
        let mut children = Vec::new();
        children
            .try_reserve_exact(node_budget)
            .map_err(|_| AxError::NoMemory)?;
        records
            .try_reserve(node_budget)
            .map_err(|_| AxError::NoMemory)?;
        let mut start = 0usize;
        while start < items.len() {
            let mut end = start;
            while end < items.len() {
                let candidate: Vec<TreeWriteItem<'_>> = items[start..=end]
                    .iter()
                    .map(|item| TreeWriteItem {
                        key: item.key,
                        value: &item.value,
                    })
                    .collect();
                match BtrfsTreeBlock::encode_leaf(
                    nodesize,
                    &self.superblock.fsid,
                    1,
                    generation,
                    owner,
                    &candidate,
                ) {
                    Ok(_) => end += 1,
                    Err(AxError::StorageFull) if end != start => break,
                    Err(error) => return Err(error),
                }
            }
            let bytenr = allocate()?;
            let final_items: Vec<TreeWriteItem<'_>> = items[start..end]
                .iter()
                .map(|item| TreeWriteItem {
                    key: item.key,
                    value: &item.value,
                })
                .collect();
            self.volume.write_tree_block(
                bytenr,
                &BtrfsTreeBlock::encode_leaf(
                    nodesize,
                    &self.superblock.fsid,
                    bytenr,
                    generation,
                    owner,
                    &final_items,
                )?,
            )?;
            records.push(TreeBlockRecord {
                bytenr,
                header_owner: owner,
                relation_root,
                level: 0,
            });
            children.push(TreeChild {
                key: items[start].key,
                bytenr,
                generation,
            });
            start = end;
        }
        let mut level = 1u8;
        while children.len() > 1 {
            if level == u8::MAX {
                return Err(AxError::StorageFull);
            }
            let mut next = Vec::new();
            next.try_reserve_exact(children.len())
                .map_err(|_| AxError::NoMemory)?;
            let mut start = 0usize;
            while start < children.len() {
                let mut end = start;
                while end < children.len() {
                    match BtrfsTreeBlock::encode_internal(
                        nodesize,
                        &self.superblock.fsid,
                        1,
                        generation,
                        owner,
                        level,
                        &children[start..=end],
                    ) {
                        Ok(_) => end += 1,
                        Err(AxError::StorageFull) if end != start => break,
                        Err(error) => return Err(error),
                    }
                }
                let bytenr = allocate()?;
                self.volume.write_tree_block(
                    bytenr,
                    &BtrfsTreeBlock::encode_internal(
                        nodesize,
                        &self.superblock.fsid,
                        bytenr,
                        generation,
                        owner,
                        level,
                        &children[start..end],
                    )?,
                )?;
                records.push(TreeBlockRecord {
                    bytenr,
                    header_owner: owner,
                    relation_root,
                    level,
                });
                next.push(TreeChild {
                    key: children[start].key,
                    bytenr,
                    generation,
                });
                start = end;
            }
            children = next;
            level = level.checked_add(1).ok_or(AxError::StorageFull)?;
        }
        Ok(children[0].bytenr)
    }

    /// Exact owner/level layout counterpart to the recorded writer.  Dummy
    /// addresses are sufficient because encoded node size never depends on a
    /// child bytenr width; the returned order is the allocator consumption
    /// order (leaves first, then each internal level).
    fn preflight_tree_cow_layout(
        &self,
        owner: u64,
        generation: u64,
        items: &[RawTreeItem],
    ) -> AxResult<Vec<u8>> {
        let count = self.preflight_tree_cow_nodes(owner, generation, items)?;
        let mut levels = Vec::new();
        levels
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        let nodesize = self.superblock.nodesize as usize;
        let mut children = Vec::new();
        let mut start = 0usize;
        while start < items.len() {
            let mut end = start;
            while end < items.len() {
                let candidate: Vec<TreeWriteItem<'_>> = items[start..=end]
                    .iter()
                    .map(|item| TreeWriteItem {
                        key: item.key,
                        value: &item.value,
                    })
                    .collect();
                match BtrfsTreeBlock::encode_leaf(
                    nodesize,
                    &self.superblock.fsid,
                    1,
                    generation,
                    owner,
                    &candidate,
                ) {
                    Ok(_) => end += 1,
                    Err(AxError::StorageFull) if end != start => break,
                    Err(error) => return Err(error),
                }
            }
            levels.push(0);
            children.push(TreeChild {
                key: items[start].key,
                bytenr: 1,
                generation,
            });
            start = end;
        }
        let mut level = 1u8;
        while children.len() > 1 {
            let mut next = Vec::new();
            let mut start = 0usize;
            while start < children.len() {
                let mut end = start;
                while end < children.len() {
                    match BtrfsTreeBlock::encode_internal(
                        nodesize,
                        &self.superblock.fsid,
                        1,
                        generation,
                        owner,
                        level,
                        &children[start..=end],
                    ) {
                        Ok(_) => end += 1,
                        Err(AxError::StorageFull) if end != start => break,
                        Err(error) => return Err(error),
                    }
                }
                levels.push(level);
                next.push(TreeChild {
                    key: children[start].key,
                    bytenr: 1,
                    generation,
                });
                start = end;
            }
            children = next;
            level = level.checked_add(1).ok_or(AxError::StorageFull)?;
        }
        if levels.len() != count {
            return Err(AxError::BadState);
        }
        Ok(levels)
    }

    /// Pure counterpart to `rewrite_tree_cow`.  It runs the identical leaf
    /// and internal-node packing probes with dummy bytenrs, producing the
    /// exact number of reservations the real bottom-up writer will consume.
    /// No allocator, volume or transaction state is touched.
    pub fn preflight_tree_cow_nodes(
        &self,
        owner: u64,
        generation: u64,
        items: &[RawTreeItem],
    ) -> AxResult<usize> {
        if owner == 0
            || generation == 0
            || items.is_empty()
            || items.windows(2).any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(AxError::InvalidInput);
        }
        let nodesize = self.superblock.nodesize as usize;
        let mut children = Vec::new();
        let mut count = 0usize;
        let mut start = 0usize;
        while start < items.len() {
            let mut end = start;
            while end < items.len() {
                let candidate: Vec<TreeWriteItem<'_>> = items[start..=end]
                    .iter()
                    .map(|item| TreeWriteItem {
                        key: item.key,
                        value: &item.value,
                    })
                    .collect();
                match BtrfsTreeBlock::encode_leaf(
                    nodesize,
                    &self.superblock.fsid,
                    1,
                    generation,
                    owner,
                    &candidate,
                ) {
                    Ok(_) => end += 1,
                    Err(AxError::StorageFull) if end != start => break,
                    Err(error) => return Err(error),
                }
            }
            children.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            children.push(TreeChild {
                key: items[start].key,
                bytenr: 1,
                generation,
            });
            count = count.checked_add(1).ok_or(AxError::NoMemory)?;
            start = end;
        }
        let mut level = 1u8;
        while children.len() > 1 {
            let mut next = Vec::new();
            let mut start = 0usize;
            while start < children.len() {
                let mut end = start;
                while end < children.len() {
                    match BtrfsTreeBlock::encode_internal(
                        nodesize,
                        &self.superblock.fsid,
                        1,
                        generation,
                        owner,
                        level,
                        &children[start..=end],
                    ) {
                        Ok(_) => end += 1,
                        Err(AxError::StorageFull) if end != start => break,
                        Err(error) => return Err(error),
                    }
                }
                next.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                next.push(TreeChild {
                    key: children[start].key,
                    bytenr: 1,
                    generation,
                });
                count = count.checked_add(1).ok_or(AxError::NoMemory)?;
                start = end;
            }
            children = next;
            level = level.checked_add(1).ok_or(AxError::StorageFull)?;
        }
        Ok(count)
    }

    /// Commits a rebuilt root tree through the same generation gate as the
    /// delayed-ref/qgroup transaction.  The supplied item set must already
    /// include the caller's updated filesystem-root, extent, checksum, and
    /// free-space root records; this method owns only the final ordering and
    /// does not manufacture missing accounting records.
    pub fn commit_root_tree_transaction(
        &mut self,
        transaction: super::BtrfsTransaction,
        allocator: &BtrfsLogicalAllocator,
        root_items: &[RawTreeItem],
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        // Root-only callers (notably snapshot creation) used to have a
        // private allocator/write/publish path.  That made their ROOT-tree
        // COW nodes invisible to both the extent tree and the free-space
        // fixed point.  Feed the exact caller image into the same self-hosted
        // engine used by topology transactions instead.
        let _ = allocator;
        if chunk_root != self.superblock.chunk_root
            || log_root != self.superblock.log_root
            || bytes_used != self.superblock.bytes_used
            || root_items.windows(2).any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(AxError::InvalidInput);
        }
        // This entry point is the snapshot-only root editor.  A removal or
        // retarget would need to retire/reconcile the complete old subtree
        // relation and delete its core subvolume identity; do not let such a
        // change masquerade as a root-only COW while that operation has no
        // explicit deletion plan.
        let mounted_root_items = self.root_tree_items()?;
        for old in &mounted_root_items {
            let index = root_items
                .binary_search_by_key(&old.key, |item| item.key)
                .map_err(|_| AxError::InvalidInput)?;
            if root_items[index] != *old {
                return Err(AxError::InvalidInput);
            }
        }
        let mut introduced = 0usize;
        for item in root_items.iter().filter(|item| {
            mounted_root_items
                .binary_search_by_key(&item.key, |old| old.key)
                .is_err()
        }) {
            if item.key.item_type != ROOT_ITEM || item.key.offset != 0 {
                return Err(AxError::InvalidInput);
            }
            let root = BtrfsRootItem::decode(&item.value)?;
            if !transaction.stages_snapshot_root(item.key.objectid, root.bytenr) {
                return Err(AxError::InvalidInput);
            }
            introduced = introduced.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        if introduced != transaction.staged_snapshot_count() {
            return Err(AxError::InvalidInput);
        }
        let chunk_items = self.chunk_tree_items()?;
        let device_items = self.device_items_from_chunk_tree(&chunk_items)?;
        let mut stage = self
            .volume
            .stage_member_change(BtrfsDeviceTopologyChange::Keep)?;
        BtrfsVolume::stage_chunks(&mut stage, self.volume.chunks().to_vec())?;
        let system_chunks = self.superblock.system_chunk_array().to_vec();
        BtrfsVolume::validate_staged_system_chunks(&stage, &system_chunks)?;
        self.commit_staged_topology_transaction(
            transaction,
            &chunk_items,
            &system_chunks,
            device_items,
            stage,
            Some(root_items),
            None,
            &[],
            None,
            None,
            log_root,
            bytes_used,
        )
    }

    /// Atomically persists a complete cross-tree COW transaction.  Every
    /// replacement tree is written bottom-up first, its ROOT_ITEM is changed
    /// in a freshly rebuilt root tree, then the redundant superblocks are
    /// published after a volume flush.  The supplied set deliberately has no
    /// implicit defaults: a data mutation must include its extent/checksum
    /// and free-space tree rewrites, and a qgroup mutation must include its
    /// quota-tree rewrite, before this method can make it reachable.
    pub fn commit_tree_rewrites(
        &mut self,
        transaction: super::BtrfsTransaction,
        allocator: &BtrfsLogicalAllocator,
        rewrites: &[BtrfsTreeRewrite],
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        if rewrites.is_empty()
            || rewrites.iter().any(|rewrite| {
                rewrite.root_objectid == 0
                    || rewrite.tree_owner == 0
                    || rewrite.old_tree_owner == 0
                    || rewrite.tree_owner == TreeId::Root as u64
                    || rewrite.items.is_empty()
            })
        {
            return Err(AxError::InvalidInput);
        }
        if rewrites.iter().enumerate().any(|(index, rewrite)| {
            rewrites[index + 1..]
                .iter()
                .any(|other| other.root_objectid == rewrite.root_objectid)
        }) {
            return Err(AxError::InvalidInput);
        }
        if rewrites.iter().any(|rewrite| {
            rewrite
                .items
                .windows(2)
                .any(|pair| pair[0].key >= pair[1].key)
        }) {
            return Err(AxError::InvalidInput);
        }

        // Extent and FreeSpace are not ordinary payload trees: the COW
        // writer must extend them with its own metadata-node records and use
        // the caller's post-data-allocation free image as its allocator
        // baseline.  Split them out and send every remaining tree through
        // the same self-hosted fixed-point engine as topology commits.
        if chunk_root != self.superblock.chunk_root {
            return Err(AxError::InvalidInput);
        }
        let _ = allocator;
        let extent = rewrites
            .iter()
            .find(|rewrite| rewrite.root_objectid == TreeId::Extent as u64)
            .ok_or(AxError::InvalidInput)?;
        let free_space = rewrites
            .iter()
            .find(|rewrite| rewrite.root_objectid == TreeId::FreeSpace as u64)
            .ok_or(AxError::InvalidInput)?;
        if extent.tree_owner != TreeId::Extent as u64
            || extent.old_tree_owner != TreeId::Extent as u64
            || free_space.tree_owner != TreeId::FreeSpace as u64
            || free_space.old_tree_owner != TreeId::FreeSpace as u64
            || !rewrites
                .iter()
                .any(|rewrite| rewrite.root_objectid == TreeId::Csum as u64)
            || (transaction.has_qgroup_deltas()
                && !rewrites
                    .iter()
                    .any(|rewrite| rewrite.root_objectid == TreeId::Quota as u64))
        {
            return Err(AxError::InvalidInput);
        }
        let mut extra = Vec::new();
        for rewrite in rewrites {
            if rewrite.root_objectid == TreeId::Extent as u64
                || rewrite.root_objectid == TreeId::FreeSpace as u64
            {
                continue;
            }
            extra.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            extra.push(rewrite.clone());
        }
        if extra.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let chunk_items = self.chunk_tree_items()?;
        let device_items = self.device_items_from_chunk_tree(&chunk_items)?;
        let mut stage = self
            .volume
            .stage_member_change(BtrfsDeviceTopologyChange::Keep)?;
        BtrfsVolume::stage_chunks(&mut stage, self.volume.chunks().to_vec())?;
        let system_chunks = self.superblock.system_chunk_array().to_vec();
        BtrfsVolume::validate_staged_system_chunks(&stage, &system_chunks)?;
        self.commit_staged_topology_transaction(
            transaction,
            &chunk_items,
            &system_chunks,
            device_items,
            stage,
            None,
            Some(MetadataCowAccounting {
                extent_items: &extent.items,
                free_space_items: &free_space.items,
            }),
            &extra,
            None,
            None,
            log_root,
            bytes_used,
        )
    }

    /// Commits a planner created by [`mutation_planner`](Self::mutation_planner).
    /// The common engine reconstructs its allocator from the planner's final
    /// FreeSpace image, so no mounted allocator can drift from the data
    /// allocation accounting being published.
    pub fn commit_mutation_planner(
        &mut self,
        planner: BtrfsMutationPlanner,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        // A normal COW mutation cannot make an outstanding fsync log vanish.
        // Only the accounted replay-cleanup transaction may clear this
        // pointer after logged namespace/data records reach their home tree.
        if log_root != self.superblock.log_root {
            return Err(AxError::ResourceBusy);
        }
        let allocator = BtrfsLogicalAllocator::new();
        let (transaction, rewrites) = planner.into_rewrites()?;
        self.commit_tree_rewrites(
            transaction,
            &allocator,
            &rewrites,
            self.superblock.chunk_root,
            log_root,
            bytes_used,
        )
    }

    fn chunk_tree_items(&self) -> AxResult<Vec<RawTreeItem>> {
        let mut items = Vec::new();
        self.collect_tree_items(
            self.superblock.chunk_root,
            TreeId::Chunk as u64,
            &mut BTreeSet::new(),
            &mut items,
        )?;
        Ok(items)
    }

    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    fn collect_tree_nodes(
        &self,
        logical: u64,
        owner: u64,
        expected_generation: Option<u64>,
        seen: &mut BTreeSet<u64>,
    ) -> AxResult<()> {
        if !seen.insert(logical) {
            return Err(AxError::Io);
        }
        let image = self.volume.read_checked_tree_block(
            logical,
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            self.superblock.csum_type,
        )?;
        let block = BtrfsTreeBlock::decode(
            &image,
            &self.superblock.fsid,
            Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
            logical,
        )?;
        if block.owner() != owner
            || block.generation() > self.superblock.generation
            || expected_generation.is_some_and(|generation| block.generation() != generation)
        {
            return Err(AxError::Io);
        }
        if block.level() == 0 {
            return Ok(());
        }
        for index in 0..block.item_count() {
            let child = block.child(index)?;
            self.collect_tree_nodes(child.bytenr, owner, Some(child.generation), seen)?;
        }
        Ok(())
    }

    fn collect_tree_block_records(
        &self,
        logical: u64,
        header_owner: u64,
        relation_root: u64,
        expected_generation: Option<u64>,
        seen: &mut BTreeSet<(u64, u64)>,
        output: &mut Vec<TreeBlockRecord>,
    ) -> AxResult<()> {
        // A physical metadata extent may retain relations for more than one
        // tree root.  Cycle detection is relation-based, not bytenr-based.
        if !seen.insert((logical, relation_root)) {
            return Err(AxError::Io);
        }
        let image = self.volume.read_checked_tree_block(
            logical,
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            self.superblock.csum_type,
        )?;
        let block = BtrfsTreeBlock::decode(
            &image,
            &self.superblock.fsid,
            Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
            logical,
        )?;
        if block.owner() != header_owner
            || block.generation() > self.superblock.generation
            || expected_generation.is_some_and(|generation| block.generation() != generation)
        {
            return Err(AxError::Io);
        }
        output.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        output.push(TreeBlockRecord {
            bytenr: logical,
            header_owner,
            relation_root,
            level: block.level(),
        });
        if block.level() != 0 {
            for index in 0..block.item_count() {
                let child = block.child(index)?;
                self.collect_tree_block_records(
                    child.bytenr,
                    header_owner,
                    relation_root,
                    Some(child.generation),
                    seen,
                    output,
                )?;
            }
        }
        Ok(())
    }

    fn set_extent_tree_block_item(
        &self,
        items: &mut Vec<RawTreeItem>,
        record: TreeBlockRecord,
        generation: u64,
    ) -> AxResult<()> {
        Self::set_raw_item(
            items,
            RawTreeItem {
                key: TreeItemKey {
                    objectid: record.bytenr,
                    item_type: EXTENT_ITEM,
                    offset: u64::from(self.superblock.nodesize),
                },
                value: super::encode_tree_extent_item(generation, 1, record.level)?,
            },
        )?;
        Self::set_raw_item(
            items,
            RawTreeItem {
                key: TreeItemKey {
                    objectid: record.bytenr,
                    item_type: TREE_BLOCK_REF,
                    offset: record.relation_root,
                },
                value: super::encode_tree_block_ref(record.relation_root)?,
            },
        )
    }

    /// Adds one additional root relation to an existing metadata extent.
    /// Snapshot creation shares checked filesystem-tree blocks; it must
    /// increase both the native extent-tree reference count and the in-core
    /// delayed-ref ledger before the new ROOT_ITEM becomes reachable.
    fn add_extent_tree_block_relation(
        &self,
        items: &mut Vec<RawTreeItem>,
        record: TreeBlockRecord,
    ) -> AxResult<()> {
        let extent_key = TreeItemKey {
            objectid: record.bytenr,
            item_type: EXTENT_ITEM,
            offset: u64::from(self.superblock.nodesize),
        };
        let index = items
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let (references, generation, level) = decode_tree_extent_item(&items[index].value)?;
        if references == 0 || level != record.level {
            return Err(AxError::Io);
        }
        let relation_key = TreeItemKey {
            objectid: record.bytenr,
            item_type: TREE_BLOCK_REF,
            offset: record.relation_root,
        };
        if items
            .binary_search_by_key(&relation_key, |item| item.key)
            .is_ok()
        {
            return Err(AxError::AlreadyExists);
        }
        items[index].value = super::encode_tree_extent_item(
            generation,
            references.checked_add(1).ok_or(AxError::NoMemory)?,
            level,
        )?;
        Self::set_raw_item(
            items,
            RawTreeItem {
                key: relation_key,
                value: super::encode_tree_block_ref(record.relation_root)?,
            },
        )
    }

    /// Finds newly introduced ROOT_ITEMs which deliberately share an already
    /// mounted filesystem-tree root (the snapshot operation).  The tree
    /// block header retains the source root as its owner while every block
    /// receives a second relation for the destination root.
    fn snapshot_shared_tree_records(
        &self,
        mounted_root_items: &[RawTreeItem],
        requested_root_items: &[RawTreeItem],
    ) -> AxResult<Vec<TreeBlockRecord>> {
        let mut records = Vec::new();
        let mut seen = BTreeSet::new();
        for item in requested_root_items {
            if item.key.item_type != ROOT_ITEM
                || mounted_root_items
                    .binary_search_by_key(&item.key, |entry| entry.key)
                    .is_ok()
            {
                continue;
            }
            let destination = item.key.objectid;
            let root = BtrfsRootItem::decode(&item.value)?;
            let _source_relation_root = mounted_root_items
                .iter()
                .find_map(|candidate| {
                    (candidate.key.item_type == ROOT_ITEM
                        && candidate.key.objectid != destination
                        && BtrfsRootItem::decode(&candidate.value)
                            .ok()
                            .is_some_and(|old| old.bytenr == root.bytenr))
                    .then_some(candidate.key.objectid)
                })
                .ok_or(AxError::Io)?;
            let image = self.volume.read_checked_tree_block(
                root.bytenr,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let header_owner = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                root.bytenr,
            )?
            .owner();
            if header_owner == 0 {
                return Err(AxError::Io);
            }
            // `_source_relation_root` selects the existing ROOT_ITEM relation;
            // `header_owner` comes from the checked node itself and may name
            // an earlier ancestor when the source is already a snapshot.
            self.collect_tree_block_records(
                root.bytenr,
                header_owner,
                destination,
                None,
                &mut seen,
                &mut records,
            )?;
        }
        Ok(records)
    }

    /// Removes exactly one TREE_BLOCK_REF relation.  The physical
    /// EXTENT_ITEM is retired only when this was its last relation; callers
    /// use that boolean, rather than a tree walk's bytenr set, for free-space
    /// reclamation and bytes_used accounting.
    fn retire_extent_tree_block_relation(
        &self,
        items: &mut Vec<RawTreeItem>,
        record: TreeBlockRecord,
    ) -> AxResult<bool> {
        let extent_key = TreeItemKey {
            objectid: record.bytenr,
            item_type: EXTENT_ITEM,
            offset: u64::from(self.superblock.nodesize),
        };
        let index = items
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let (references, generation, level) = decode_tree_extent_item(&items[index].value)?;
        if level != record.level || references == 0 {
            return Err(AxError::Io);
        }
        let relation_key = TreeItemKey {
            objectid: record.bytenr,
            item_type: TREE_BLOCK_REF,
            offset: record.relation_root,
        };
        let relation = items
            .binary_search_by_key(&relation_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        decode_tree_block_ref(&items[relation].value)?;
        items.remove(relation);
        let index = items
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        if references == 1 {
            items.remove(index);
            Ok(true)
        } else {
            items[index].value = super::encode_tree_extent_item(generation, references - 1, level)?;
            Ok(false)
        }
    }

    fn device_items_from_chunk_tree(
        &self,
        items: &[RawTreeItem],
    ) -> AxResult<BTreeMap<u64, BtrfsDeviceItem>> {
        let mut output = BTreeMap::new();
        for item in items.iter().filter(|item| item.key.item_type == DEV_ITEM) {
            if item.key.offset != 0 {
                return Err(AxError::Io);
            }
            let device = BtrfsDeviceItem::decode(&item.value)?;
            if device.devid != item.key.objectid
                || device.fsid != self.superblock.fsid
                || output.insert(device.devid, device).is_some()
            {
                return Err(AxError::Io);
            }
        }
        if output.len() != usize::try_from(self.superblock.num_devices).map_err(|_| AxError::Io)? {
            return Err(AxError::Io);
        }
        Ok(output)
    }

    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    fn decode_topology_chunks(
        items: &[RawTreeItem],
        mut member_index: impl FnMut(u64) -> Option<usize>,
    ) -> AxResult<Vec<super::Chunk>> {
        let mut chunks = Vec::new();
        for item in items
            .iter()
            .filter(|item| item.key.item_type == BtrfsVolume::CHUNK_ITEM_TYPE)
        {
            chunks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            chunks.push(super::Chunk::decode_item(
                item.key.objectid,
                &item.value,
                |devid| member_index(devid),
            )?);
        }
        if chunks.is_empty() {
            return Err(AxError::Io);
        }
        Ok(chunks)
    }

    fn set_raw_item(items: &mut Vec<RawTreeItem>, item: RawTreeItem) -> AxResult<()> {
        match items.binary_search_by_key(&item.key, |entry| entry.key) {
            Ok(index) => items[index] = item,
            Err(index) => {
                items.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                items.insert(index, item);
            }
        }
        Ok(())
    }

    fn commit_staged_topology_transaction(
        &mut self,
        mut transaction: super::BtrfsTransaction,
        chunk_items: &[RawTreeItem],
        system_chunk_array: &[u8],
        device_items: BTreeMap<u64, BtrfsDeviceItem>,
        stage: BtrfsTopologyStage,
        root_items_override: Option<&[RawTreeItem]>,
        accounting: Option<MetadataCowAccounting<'_>>,
        extra_rewrites: &[BtrfsTreeRewrite],
        newly_allocatable: Option<(u64, u64)>,
        metadata_target: Option<(u64, u64)>,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        // Topology starts from the mounted accounting image; an ordinary
        // mutation supplies its already planned Extent/FreeSpace image.  In
        // both cases this engine alone adds/removes metadata-node accounting
        // and derives the final metadata delta from its reservation/reclaim
        // plan.
        // Quota is a self-hosted accounting tree.  Even topology-only COW
        // creates/retires metadata relations, therefore an existing quota
        // tree must participate in this same fixed point rather than being
        // updated only for VFS data operations.
        let mut extra_rewrites = extra_rewrites.to_vec();
        if !extra_rewrites
            .iter()
            .any(|rewrite| rewrite.root_objectid == TreeId::Quota as u64)
        {
            if let Ok(root) = self.subvolume_root(TreeId::Quota as u64) {
                let mut items = Vec::new();
                self.collect_tree_items(
                    root,
                    TreeId::Quota as u64,
                    &mut BTreeSet::new(),
                    &mut items,
                )?;
                extra_rewrites
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                extra_rewrites.push(BtrfsTreeRewrite {
                    root_objectid: TreeId::Quota as u64,
                    tree_owner: TreeId::Quota as u64,
                    old_tree_owner: TreeId::Quota as u64,
                    items,
                });
            }
        }
        extra_rewrites.sort_by_key(|rewrite| rewrite.root_objectid);
        let quota_base_items = match self.subvolume_root(TreeId::Quota as u64) {
            Ok(root) => {
                let mut items = Vec::new();
                self.collect_tree_items(
                    root,
                    TreeId::Quota as u64,
                    &mut BTreeSet::new(),
                    &mut items,
                )?;
                Some(items)
            }
            Err(AxError::NotFound) => None,
            Err(error) => return Err(error),
        };
        if chunk_items
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
            || device_items.is_empty()
        {
            return Err(AxError::InvalidInput);
        }
        if root_items_override.is_some_and(|items| {
            items.is_empty() || items.windows(2).any(|pair| pair[0].key >= pair[1].key)
        }) {
            return Err(AxError::InvalidInput);
        }
        if extra_rewrites.iter().any(|rewrite| matches!(rewrite.root_objectid, id if id == TreeId::Chunk as u64 || id == TreeId::Root as u64 || id == TreeId::FreeSpace as u64 || id == TreeId::Extent as u64))
            || extra_rewrites.windows(2).any(|pair| pair[0].root_objectid >= pair[1].root_objectid)
        { return Err(AxError::InvalidInput); }
        let writes_log = extra_rewrites
            .iter()
            .any(|rewrite| rewrite.root_objectid == TreeId::Log as u64);
        let clears_log = !writes_log && log_root == 0 && self.superblock.log_root != 0;
        if writes_log && self.superblock.log_root != 0 {
            return Err(AxError::ResourceBusy);
        }
        if !writes_log && !clears_log && log_root != self.superblock.log_root {
            return Err(AxError::InvalidInput);
        }
        let allocator = match accounting.as_ref() {
            Some(images) => self.logical_allocator_from_items(images.free_space_items)?,
            None => self.logical_allocator()?,
        };
        if let Some((logical, len)) = newly_allocatable {
            allocator.add_free(logical, len)?;
        }
        // These complete old COW trees become unreachable only if the new
        // root is published.  They are added solely to this transaction-local
        // allocator image; failure drops that image and preserves the old
        // roots, while success makes the reclaimed records visible together
        // with the new root generation.
        let mut obsolete_records = Vec::new();
        let mut obsolete_seen = BTreeSet::new();
        self.collect_tree_block_records(
            self.superblock.chunk_root,
            TreeId::Chunk as u64,
            TreeId::Chunk as u64,
            None,
            &mut obsolete_seen,
            &mut obsolete_records,
        )?;
        self.collect_tree_block_records(
            self.superblock.root,
            TreeId::Root as u64,
            TreeId::Root as u64,
            None,
            &mut obsolete_seen,
            &mut obsolete_records,
        )?;
        let old_free_root = self.subvolume_root(TreeId::FreeSpace as u64)?;
        self.collect_tree_block_records(
            old_free_root,
            TreeId::FreeSpace as u64,
            TreeId::FreeSpace as u64,
            None,
            &mut obsolete_seen,
            &mut obsolete_records,
        )?;
        let old_extent_root = self.subvolume_root(TreeId::Extent as u64)?;
        self.collect_tree_block_records(
            old_extent_root,
            TreeId::Extent as u64,
            TreeId::Extent as u64,
            None,
            &mut obsolete_seen,
            &mut obsolete_records,
        )?;
        if clears_log {
            // A native log root is a root tree whose ROOT_ITEMs point to one
            // independent log tree per subvolume.  Retire both levels in the
            // same delayed-ref image as the home-tree rewrites; clearing only
            // the top-level tree would leak every per-subvolume log extent.
            // This complete validation still precedes any reservation or
            // media write, so a malformed member cannot produce a partial
            // recovery publication.
            let native_log_roots = self.recovery_log_roots()?;
            self.collect_tree_block_records(
                self.superblock.log_root,
                TreeId::Log as u64,
                TreeId::Log as u64,
                None,
                &mut obsolete_seen,
                &mut obsolete_records,
            )?;
            for root in native_log_roots {
                self.collect_tree_block_records(
                    root.bytenr,
                    root.subvolume,
                    root.subvolume,
                    Some(root.generation),
                    &mut obsolete_seen,
                    &mut obsolete_records,
                )?;
            }
        }
        for rewrite in &extra_rewrites {
            if rewrite.root_objectid == TreeId::Log as u64 {
                if self.superblock.log_root != 0 {
                    self.collect_tree_block_records(
                        self.superblock.log_root,
                        TreeId::Log as u64,
                        TreeId::Log as u64,
                        None,
                        &mut obsolete_seen,
                        &mut obsolete_records,
                    )?;
                }
            } else {
                self.collect_tree_block_records(
                    self.subvolume_root(rewrite.root_objectid)?,
                    rewrite.old_tree_owner,
                    rewrite.root_objectid,
                    None,
                    &mut obsolete_seen,
                    &mut obsolete_records,
                )?;
            }
        }
        let node_bytes = u64::from(self.superblock.nodesize);
        let current = self.superblock;
        let total_bytes = device_items
            .values()
            .try_fold(0u64, |sum, item| sum.checked_add(item.total_bytes))
            .ok_or(AxError::NoMemory)?;
        if bytes_used > total_bytes {
            return Err(AxError::InvalidInput);
        }
        // A root-only operation supplies its already checked/edited ROOT
        // image here.  All other users begin from the mounted image and have
        // their replacement root records inserted below.  In both cases the
        // root itself is rebuilt only after Extent and FreeSpace reach their
        // shared fixed point.
        let mounted_root_items = self.root_tree_items()?;
        let mut root_items = match root_items_override {
            Some(items) => items.to_vec(),
            None => mounted_root_items.clone(),
        };
        let shared_snapshot_records =
            self.snapshot_shared_tree_records(&mounted_root_items, &root_items)?;
        let planned_generation = current.generation.checked_add(1).ok_or(AxError::NoMemory)?;
        // ROOT_ITEM payload widths are fixed, so replacing the chunk-root
        // bytenr does not alter ROOT-tree packing.  This gives an exact plan
        // for the two trees before the first live reservation is consumed.
        let chunk_layout =
            self.preflight_tree_cow_layout(TreeId::Chunk as u64, planned_generation, chunk_items)?;
        let chunk_nodes = chunk_layout.len();
        let root_layout =
            self.preflight_tree_cow_layout(TreeId::Root as u64, planned_generation, &root_items)?;
        let root_nodes = root_layout.len();
        let mut extra_layouts = Vec::new();
        let extra_nodes = extra_rewrites.iter().try_fold(0usize, |count, rewrite| {
            let layout = self.preflight_tree_cow_layout(
                rewrite.tree_owner,
                planned_generation,
                &rewrite.items,
            )?;
            let nodes = layout.len();
            extra_layouts.push(layout);
            count.checked_add(nodes).ok_or(AxError::NoMemory)
        })?;
        // Fixed point: the final FreeSpace image excludes every COW node,
        // including the nodes used to encode that image.  Reserve chunk/root
        // nodes first, then grow or shrink only an unconsumed FreeSpace tail
        // until its real packing count is stable.
        let mut reservations = CowReservationPlan::reserve(
            self,
            &allocator,
            metadata_target,
            chunk_nodes
                .checked_add(root_nodes)
                .and_then(|count| count.checked_add(extra_nodes))
                .ok_or(AxError::NoMemory)?,
        )?;
        let mut free_nodes = 0usize;
        let mut extent_nodes = 0usize;
        let free_root = self.subvolume_root(TreeId::FreeSpace as u64)?;
        let free_base = match accounting.as_ref() {
            Some(images) => images.free_space_items.to_vec(),
            None => {
                let mut items = Vec::new();
                self.collect_tree_items(
                    free_root,
                    TreeId::FreeSpace as u64,
                    &mut BTreeSet::new(),
                    &mut items,
                )?;
                items
            }
        };
        let mut free_space_items = Vec::new();
        // Keep the mounted extent image even when the caller supplies a
        // post-data-mutation accounting image.  It is the only stable
        // baseline for proving the final TREE_BLOCK_REF key-set delta.
        let mut mounted_extent_items = Vec::new();
        self.collect_tree_items(
            old_extent_root,
            TreeId::Extent as u64,
            &mut BTreeSet::new(),
            &mut mounted_extent_items,
        )?;
        let extent_base = match accounting.as_ref() {
            Some(images) => images.extent_items.to_vec(),
            None => mounted_extent_items.clone(),
        };
        if accounting.is_some() {
            Self::validate_delayed_data_refs(&transaction, &mounted_extent_items, &extent_base)?;
            // The supplied FreeSpace image must both be internally valid and
            // have no overlap with any final EXTENT_DATA_REF-backed range.
            Self::validate_data_refs_not_free(
                &extent_base,
                &free_base,
                u64::from(self.superblock.sectorsize),
            )?;
        }
        // Retire only the exact tree-root relation being replaced.  A shared
        // physical tree extent stays allocated (and keeps its EXTENT_ITEM)
        // until the last TREE_BLOCK_REF is gone.
        let mut retired_extent_base = extent_base.clone();
        let mut obsolete = BTreeSet::new();
        for record in &obsolete_records {
            if self.retire_extent_tree_block_relation(&mut retired_extent_base, *record)? {
                obsolete.insert(record.bytenr);
            }
        }
        // The source tree remains live.  These are positive relations for
        // newly introduced snapshot ROOT_ITEMs, not replacements, so they
        // are applied after old-root retirement and survive the fixed-point
        // rebuild of the extent tree.
        for record in &shared_snapshot_records {
            self.add_extent_tree_block_relation(&mut retired_extent_base, *record)?;
        }
        let mut extent_items;
        for _iteration in 0..32 {
            free_space_items = Self::free_space_items_from_allocator(
                &allocator,
                free_base.clone(),
                &obsolete,
                node_bytes,
                u64::from(self.superblock.sectorsize),
                BtrfsVolume::staged_chunks(&stage),
            )?;
            let needed_free = self.preflight_tree_cow_nodes(
                TreeId::FreeSpace as u64,
                planned_generation,
                &free_space_items,
            )?;
            let desired_tail = needed_free
                .checked_add(extent_nodes)
                .ok_or(AxError::NoMemory)?;
            let current_tail = free_nodes
                .checked_add(extent_nodes)
                .ok_or(AxError::NoMemory)?;
            if desired_tail != current_tail {
                // FreeSpace and Extent are one mutually dependent tail.  Do
                // not trim only the physical tail when FreeSpace changes: it
                // could belong to the Extent self-image.  Re-reserve the
                // whole unpublished tail and iterate again instead.
                if current_tail != 0 {
                    reservations.release_tail(&allocator, current_tail)?;
                }
                if desired_tail != 0 {
                    reservations.append(self, &allocator, metadata_target, desired_tail)?;
                }
            }
            free_nodes = needed_free;
            extent_items = retired_extent_base.clone();
            let mut candidates = Vec::new();
            let mut append = |start: usize,
                              header_owner: u64,
                              relation_root: u64,
                              layout: &[u8]|
             -> AxResult<()> {
                for (index, &level) in layout.iter().enumerate() {
                    let reservation = *reservations
                        .nodes
                        .get(start.checked_add(index).ok_or(AxError::NoMemory)?)
                        .ok_or(AxError::BadState)?;
                    candidates.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    candidates.push(TreeBlockRecord {
                        bytenr: reservation.logical,
                        header_owner,
                        relation_root,
                        level,
                    });
                }
                Ok(())
            };
            append(0, TreeId::Chunk as u64, TreeId::Chunk as u64, &chunk_layout)?;
            append(
                chunk_nodes,
                TreeId::Root as u64,
                TreeId::Root as u64,
                &root_layout,
            )?;
            let mut offset = chunk_nodes
                .checked_add(root_nodes)
                .ok_or(AxError::NoMemory)?;
            for (rewrite, layout) in extra_rewrites.iter().zip(extra_layouts.iter()) {
                append(offset, rewrite.tree_owner, rewrite.root_objectid, layout)?;
                offset = offset.checked_add(layout.len()).ok_or(AxError::NoMemory)?;
            }
            let free_layout = self.preflight_tree_cow_layout(
                TreeId::FreeSpace as u64,
                planned_generation,
                &free_space_items,
            )?;
            append(
                offset,
                TreeId::FreeSpace as u64,
                TreeId::FreeSpace as u64,
                &free_layout,
            )?;
            offset = offset.checked_add(free_nodes).ok_or(AxError::NoMemory)?;
            let extent_layout = vec![0u8; extent_nodes];
            append(
                offset,
                TreeId::Extent as u64,
                TreeId::Extent as u64,
                &extent_layout,
            )?;
            for record in candidates {
                self.set_extent_tree_block_item(&mut extent_items, record, planned_generation)?;
            }
            let needed_extent = self.preflight_tree_cow_nodes(
                TreeId::Extent as u64,
                planned_generation,
                &extent_items,
            )?;
            if needed_extent == extent_nodes {
                break;
            }
            let current_tail = free_nodes
                .checked_add(extent_nodes)
                .ok_or(AxError::NoMemory)?;
            let desired_tail = free_nodes
                .checked_add(needed_extent)
                .ok_or(AxError::NoMemory)?;
            if current_tail != 0 {
                reservations.release_tail(&allocator, current_tail)?;
            }
            if desired_tail != 0 {
                reservations.append(self, &allocator, metadata_target, desired_tail)?;
            }
            extent_nodes = needed_extent;
            if _iteration == 31 {
                reservations.release_all(&allocator)?;
                return Err(AxError::ResourceBusy);
            }
        }
        // Reservation order was CHUNK, ROOT, FreeSpace.  The writer is
        // bottom-up CHUNK, FreeSpace, ROOT, so move the exact tail before the
        // root group without changing any reserved address.
        reservations.order_topology_with_extent(
            chunk_nodes,
            root_nodes,
            extra_nodes,
            free_nodes,
            extent_nodes,
        )?;
        // Re-materialise after ordering: extent keys name physical logical
        // addresses, so the final item image must use the writer order rather
        // than the preliminary reservation order used by the convergence
        // probe.
        extent_items = retired_extent_base.clone();
        let mut final_records = Vec::new();
        let mut append_final =
            |start: usize, header_owner: u64, relation_root: u64, layout: &[u8]| -> AxResult<()> {
                for (index, &level) in layout.iter().enumerate() {
                    let reservation = *reservations
                        .nodes
                        .get(start.checked_add(index).ok_or(AxError::NoMemory)?)
                        .ok_or(AxError::BadState)?;
                    final_records
                        .try_reserve(1)
                        .map_err(|_| AxError::NoMemory)?;
                    final_records.push(TreeBlockRecord {
                        bytenr: reservation.logical,
                        header_owner,
                        relation_root,
                        level,
                    });
                }
                Ok(())
            };
        append_final(0, TreeId::Chunk as u64, TreeId::Chunk as u64, &chunk_layout)?;
        let mut final_offset = chunk_nodes;
        let free_layout = self.preflight_tree_cow_layout(
            TreeId::FreeSpace as u64,
            planned_generation,
            &free_space_items,
        )?;
        append_final(
            final_offset,
            TreeId::FreeSpace as u64,
            TreeId::FreeSpace as u64,
            &free_layout,
        )?;
        final_offset = final_offset
            .checked_add(free_nodes)
            .ok_or(AxError::NoMemory)?;
        for (rewrite, layout) in extra_rewrites.iter().zip(extra_layouts.iter()) {
            append_final(
                final_offset,
                rewrite.tree_owner,
                rewrite.root_objectid,
                layout,
            )?;
            final_offset = final_offset
                .checked_add(layout.len())
                .ok_or(AxError::NoMemory)?;
        }
        // The level payload changes no item width, so a first zero-level pass
        // determines the exact self-hosting count and the second pass writes
        // the real leaf/internal levels into the final image.
        append_final(
            final_offset,
            TreeId::Extent as u64,
            TreeId::Extent as u64,
            &vec![0; extent_nodes],
        )?;
        final_offset = final_offset
            .checked_add(extent_nodes)
            .ok_or(AxError::NoMemory)?;
        append_final(
            final_offset,
            TreeId::Root as u64,
            TreeId::Root as u64,
            &root_layout,
        )?;
        for record in &final_records {
            self.set_extent_tree_block_item(&mut extent_items, *record, planned_generation)?;
        }
        let extent_layout = self.preflight_tree_cow_layout(
            TreeId::Extent as u64,
            planned_generation,
            &extent_items,
        )?;
        if extent_layout.len() != extent_nodes {
            reservations.release_all(&allocator)?;
            return Err(AxError::BadState);
        }
        for (record, level) in final_records
            .iter_mut()
            .filter(|record| record.header_owner == TreeId::Extent as u64)
            .zip(extent_layout.iter().copied())
        {
            record.level = level;
        }
        for record in &final_records {
            self.set_extent_tree_block_item(&mut extent_items, *record, planned_generation)?;
        }
        if self.preflight_tree_cow_nodes(
            TreeId::Extent as u64,
            planned_generation,
            &extent_items,
        )? != extent_nodes
        {
            reservations.release_all(&allocator)?;
            return Err(AxError::BadState);
        }
        // Derive metadata qgroup accounting from the exact relation set that
        // will be persisted.  A new tree block is referenced and exclusive;
        // removing a relation drops referenced bytes and drops exclusive
        // bytes only when that physical extent loses its final relation.
        // Snapshot sharing adds only referenced bytes.  We intentionally
        // charge only qgroups that have a native QGROUP_INFO item: internal
        // Btrfs trees do not acquire fictional subvolume qgroups merely
        // because their metadata was COWed.
        if let (Some(base), Some(quota_index)) = (
            quota_base_items.as_ref(),
            extra_rewrites
                .iter()
                .position(|rewrite| rewrite.root_objectid == TreeId::Quota as u64),
        ) {
            let mut metadata_deltas: BTreeMap<super::QgroupId, (i128, i128)> = BTreeMap::new();
            let known = Self::qgroup_usages_from_items(&extra_rewrites[quota_index].items)?;
            let parents = Self::qgroup_parents_from_items(&extra_rewrites[quota_index].items)?;
            let supplied = Self::qgroup_usages_from_items(&extra_rewrites[quota_index].items)?;
            let base_usage = Self::qgroup_usages_from_items(base)?;
            // Caller-managed deltas begin at changed children.  First derive
            // the *complete* projected delta for every ancestor, then compare
            // each supplied QGROUP_INFO image once.  Charging while walking
            // each child used to compare a shared ancestor against only the
            // most recently visited child, so two children below one parent
            // could be rejected or silently under-accounted.
            let caller_deltas = transaction.qgroup_deltas();
            let caller_delta_by_id: BTreeMap<_, _> = caller_deltas
                .iter()
                .map(|(id, referenced, exclusive)| (*id, (*referenced, *exclusive)))
                .collect();
            let mut ancestor_deltas: BTreeMap<super::QgroupId, (i128, i128)> = BTreeMap::new();
            for (child, referenced, exclusive) in caller_deltas {
                let mut stack = parents.get(&child).cloned().unwrap_or_default();
                let mut seen = BTreeSet::new();
                while let Some(parent) = stack.pop() {
                    if !seen.insert(parent) || !known.contains_key(&parent) {
                        return Err(AxError::Io);
                    }
                    let entry = ancestor_deltas.entry(parent).or_insert((0, 0));
                    entry.0 = entry.0.checked_add(referenced).ok_or(AxError::NoMemory)?;
                    entry.1 = entry.1.checked_add(exclusive).ok_or(AxError::NoMemory)?;
                    if let Some(next) = parents.get(&parent) {
                        stack
                            .try_reserve(next.len())
                            .map_err(|_| AxError::NoMemory)?;
                        stack.extend_from_slice(next);
                    }
                }
            }
            for (parent, (referenced, exclusive)) in ancestor_deltas {
                // A caller is allowed to mutate an ancestor directly as well
                // as mutate one or more descendants.  The on-disk image and
                // final ledger then carry their sum, while only the
                // descendant-derived portion is newly charged here (the
                // direct portion is already present in `transaction`).
                let direct = caller_delta_by_id.get(&parent).copied().unwrap_or((0, 0));
                let expected = (
                    direct.0.checked_add(referenced).ok_or(AxError::NoMemory)?,
                    direct.1.checked_add(exclusive).ok_or(AxError::NoMemory)?,
                );
                let old = base_usage.get(&parent).copied().unwrap_or((0, 0));
                let new = supplied.get(&parent).copied().unwrap_or((0, 0));
                let supplied_delta = (
                    i128::from(new.0) - i128::from(old.0),
                    i128::from(new.1) - i128::from(old.1),
                );
                if supplied_delta != (0, 0) && supplied_delta != expected {
                    return Err(AxError::Io);
                }
                transaction.charge_qgroup(
                    parent,
                    i64::try_from(referenced).map_err(|_| AxError::NoMemory)?,
                    i64::try_from(exclusive).map_err(|_| AxError::NoMemory)?,
                )?;
                if supplied_delta == (0, 0) {
                    Self::apply_qgroup_delta_to_items(
                        &mut extra_rewrites[quota_index].items,
                        parent,
                        expected.0,
                        expected.1,
                        planned_generation,
                    )?;
                }
            }
            let mut charge = |root: u64, referenced: i128, exclusive: i128| -> AxResult<()> {
                let id = super::QgroupId { level: 0, id: root };
                if !known.contains_key(&id) {
                    return Ok(());
                }
                let mut stack = Vec::new();
                stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                stack.push(id);
                let mut seen = BTreeSet::new();
                while let Some(id) = stack.pop() {
                    if !seen.insert(id) {
                        return Err(AxError::Io);
                    }
                    if !known.contains_key(&id) {
                        return Err(AxError::Io);
                    }
                    let entry = metadata_deltas.entry(id).or_insert((0, 0));
                    entry.0 = entry.0.checked_add(referenced).ok_or(AxError::NoMemory)?;
                    entry.1 = entry.1.checked_add(exclusive).ok_or(AxError::NoMemory)?;
                    if let Some(next) = parents.get(&id) {
                        stack
                            .try_reserve(next.len())
                            .map_err(|_| AxError::NoMemory)?;
                        stack.extend_from_slice(next);
                    }
                }
                Ok(())
            };
            for record in &obsolete_records {
                charge(
                    record.relation_root,
                    -i128::from(node_bytes),
                    if obsolete.contains(&record.bytenr) {
                        -i128::from(node_bytes)
                    } else {
                        0
                    },
                )?;
            }
            for record in &shared_snapshot_records {
                charge(record.relation_root, i128::from(node_bytes), 0)?;
            }
            for record in &final_records {
                charge(
                    record.relation_root,
                    i128::from(node_bytes),
                    i128::from(node_bytes),
                )?;
            }
            for (id, (referenced, exclusive)) in metadata_deltas {
                transaction.charge_qgroup(
                    id,
                    i64::try_from(referenced).map_err(|_| AxError::NoMemory)?,
                    i64::try_from(exclusive).map_err(|_| AxError::NoMemory)?,
                )?;
                Self::apply_qgroup_delta_to_items(
                    &mut extra_rewrites[quota_index].items,
                    id,
                    referenced,
                    exclusive,
                    planned_generation,
                )?;
            }
            // The caller's data deltas and the writer's metadata deltas must
            // be represented by precisely the final QGROUP_INFO image.
            let before = Self::qgroup_usages_from_items(base)?;
            let after = Self::qgroup_usages_from_items(&extra_rewrites[quota_index].items)?;
            let mut observed = BTreeMap::new();
            for id in before.keys().chain(after.keys()) {
                let old = before.get(id).copied().unwrap_or((0, 0));
                let new = after.get(id).copied().unwrap_or((0, 0));
                let delta = (
                    i128::from(new.0) - i128::from(old.0),
                    i128::from(new.1) - i128::from(old.1),
                );
                if delta != (0, 0) {
                    observed.insert(*id, delta);
                }
            }
            let declared: BTreeMap<_, _> = transaction
                .qgroup_deltas()
                .into_iter()
                .filter(|(_, referenced, exclusive)| *referenced != 0 || *exclusive != 0)
                .map(|(id, referenced, exclusive)| (id, (referenced, exclusive)))
                .collect();
            if observed != declared {
                reservations.release_all(&allocator)?;
                return Err(AxError::Io);
            }
        } else if transaction.has_qgroup_deltas() {
            reservations.release_all(&allocator)?;
            return Err(AxError::InvalidInput);
        }
        // Persisted TREE_BLOCK_REF records and the in-memory delayed-ref
        // ledger are one transaction.  The old records are retained until
        // this point so a failed fixed-point probe cannot consume a live COW
        // node from the mounted generation.
        for record in &obsolete_records {
            transaction.add_delayed_ref(super::DelayedRef {
                bytenr: record.bytenr,
                len: node_bytes,
                root: record.relation_root,
                owner: record.relation_root,
                identity: super::DelayedRefIdentity::TreeBlock,
                delta: -1,
            })?;
        }
        for record in &shared_snapshot_records {
            transaction.add_delayed_ref(super::DelayedRef {
                bytenr: record.bytenr,
                len: node_bytes,
                root: record.relation_root,
                owner: record.relation_root,
                identity: super::DelayedRefIdentity::TreeBlock,
                delta: 1,
            })?;
        }
        for record in &final_records {
            transaction.add_delayed_ref(super::DelayedRef {
                bytenr: record.bytenr,
                len: node_bytes,
                root: record.relation_root,
                owner: record.relation_root,
                identity: super::DelayedRefIdentity::TreeBlock,
                delta: 1,
            })?;
        }
        // This must run after all writer-owned COW records join the journal,
        // but before the first new tree block is made durable.  It proves the
        // entire final TREE_BLOCK_REF key-set delta against every public and
        // writer-owned delayed TreeBlock ref in both directions.
        if let Err(error) = Self::validate_delayed_tree_block_refs(
            &transaction,
            &mounted_extent_items,
            &extent_items,
        ) {
            reservations.release_all(&allocator)?;
            return Err(error);
        }
        // `bytes_used` provided by the caller describes its data/extent
        // mutation.  This COW transaction additionally consumes exactly the
        // planned nodes and retires exactly the unreachable old nodes that
        // are injected into the final FreeSpace image.  Keep the two parts
        // together so the superblock cannot retain stale metadata usage.
        let allocated_metadata = u64::try_from(reservations.len())
            .map_err(|_| AxError::NoMemory)?
            .checked_mul(node_bytes)
            .ok_or(AxError::NoMemory)?;
        let reclaimed_metadata = u64::try_from(obsolete.len())
            .map_err(|_| AxError::NoMemory)?
            .checked_mul(node_bytes)
            .ok_or(AxError::NoMemory)?;
        let final_bytes_used = bytes_used
            .checked_add(allocated_metadata)
            .ok_or(AxError::NoMemory)?
            .checked_sub(reclaimed_metadata)
            .ok_or(AxError::Io)?;
        if final_bytes_used > total_bytes {
            reservations.release_all(&allocator)?;
            return Err(AxError::StorageFull);
        }
        let mut published = None;
        let generation = transaction.commit_with_persist(|generation| {
            if generation != planned_generation {
                return Err(AxError::BadState);
            }
            let mut written = Vec::new();
            let chunk_root = self.rewrite_tree_cow_recorded(
                TreeId::Chunk as u64,
                TreeId::Chunk as u64,
                generation,
                chunk_items,
                || reservations.next(),
                &mut written,
            )?;
            Self::replace_subvolume_root_item(
                &mut root_items,
                TreeId::Chunk as u64,
                chunk_root,
                generation,
            )?;
            let free_root = self.rewrite_tree_cow_recorded(
                TreeId::FreeSpace as u64,
                TreeId::FreeSpace as u64,
                generation,
                &free_space_items,
                || reservations.next(),
                &mut written,
            )?;
            Self::replace_subvolume_root_item(
                &mut root_items,
                TreeId::FreeSpace as u64,
                free_root,
                generation,
            )?;
            let mut published_log_root = log_root;
            let mut published_log_level = if log_root == 0 {
                0
            } else {
                current.log_root_level
            };
            for rewrite in &extra_rewrites {
                let tree_root = self.rewrite_tree_cow_recorded(
                    rewrite.tree_owner,
                    rewrite.root_objectid,
                    generation,
                    &rewrite.items,
                    || reservations.next(),
                    &mut written,
                )?;
                if rewrite.root_objectid == TreeId::Log as u64 {
                    let image = self.volume.read_checked_tree_block(
                        tree_root,
                        self.superblock.nodesize as usize,
                        &self.superblock.fsid,
                        self.superblock.csum_type,
                    )?;
                    published_log_level = BtrfsTreeBlock::decode(
                        &image,
                        &self.superblock.fsid,
                        Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                        tree_root,
                    )?
                    .level();
                    published_log_root = tree_root;
                } else {
                    Self::replace_subvolume_root_item(
                        &mut root_items,
                        rewrite.root_objectid,
                        tree_root,
                        generation,
                    )?;
                }
            }
            let extent_root = self.rewrite_tree_cow_recorded(
                TreeId::Extent as u64,
                TreeId::Extent as u64,
                generation,
                &extent_items,
                || reservations.next(),
                &mut written,
            )?;
            Self::replace_subvolume_root_item(
                &mut root_items,
                TreeId::Extent as u64,
                extent_root,
                generation,
            )?;
            let root = self.rewrite_tree_cow_recorded(
                TreeId::Root as u64,
                TreeId::Root as u64,
                generation,
                &root_items,
                || reservations.next(),
                &mut written,
            )?;
            if written != final_records {
                return Err(AxError::BadState);
            }
            published = Some((root, chunk_root, published_log_root, published_log_level));
            // Tree blocks travel through the old checked map.  Their flush
            // precedes candidate-superblock initialization/publication.
            self.volume.flush()?;
            self.volume.publish_staged_topology_superblocks(
                &stage,
                &current,
                generation,
                root,
                chunk_root,
                published_log_root,
                published_log_level,
                final_bytes_used,
                system_chunk_array,
                &device_items,
            )
        });
        let generation = match generation {
            Ok(generation) => generation,
            Err(error) => {
                reservations.release_all(&allocator)?;
                return Err(error);
            }
        };
        if !reservations.all_consumed() {
            reservations.release_all(&allocator)?;
            return Err(AxError::Io);
        }
        reservations.commit_all(&allocator)?;
        let (root, chunk_root, published_log_root, published_log_level) =
            published.ok_or(AxError::Io)?;
        self.superblock = BtrfsSuperblock::decode(
            &current.prepare_topology_commit_with_total(
                generation,
                root,
                chunk_root,
                published_log_root,
                published_log_level,
                final_bytes_used,
                total_bytes,
                u64::try_from(device_items.len()).map_err(|_| AxError::NoMemory)?,
                system_chunk_array,
            )?,
            current.bytenr,
        )?;
        // The durable ROOT_ITEM image is now the sole authority for future
        // mount-local tree resolution.  Keeping the old bytenrs in `core`
        // would let the next mutation COW a retired tree even though the
        // superblock already names this generation.
        for item in &root_items {
            if item.key.item_type == ROOT_ITEM {
                if let Ok(root_item) = BtrfsRootItem::decode(&item.value) {
                    self.core
                        .refresh_subvolume_roots(&[(item.key.objectid, root_item.bytenr)]);
                }
            }
        }
        self.volume.publish_staged_topology(stage);
        Ok(generation)
    }

    /// Commits a complete device/chunk topology replacement.  All rejection
    /// happens before any COW node write: chunk records must be sorted and
    /// self-consistent, and the caller must retain at least one live replica
    /// for every existing system/metadata range.  The new chunk tree, root
    /// tree pointer and every superblock mirror become visible as one
    /// generation; an interrupted operation leaves the prior topology root.
    // Balance/relocation writer API in progress.
    #[allow(dead_code)]
    pub fn commit_topology_transaction(
        &mut self,
        transaction: super::BtrfsTransaction,
        allocator: &BtrfsLogicalAllocator,
        chunk_items: &[RawTreeItem],
        system_chunk_array: &[u8],
        num_devices: u64,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<u64> {
        // Keep the legacy public entry point on the same fixed-point writer;
        // accepting a caller-provided allocator here used to bypass the
        // persisted FreeSpace update entirely.
        let _ = allocator;
        if log_root != self.superblock.log_root || bytes_used != self.superblock.bytes_used {
            return Err(AxError::InvalidInput);
        }
        let devices = self.device_items_from_chunk_tree(chunk_items)?;
        if u64::try_from(devices.len()).map_err(|_| AxError::NoMemory)? != num_devices {
            return Err(AxError::InvalidInput);
        }
        let mut stage = self
            .volume
            .stage_member_change(BtrfsDeviceTopologyChange::Keep)?;
        let chunks = Self::decode_topology_chunks(chunk_items, |devid| {
            BtrfsVolume::stage_member_index(&stage, devid)
        })?;
        BtrfsVolume::stage_chunks(&mut stage, chunks)?;
        BtrfsVolume::validate_staged_system_chunks(&stage, system_chunk_array)?;
        self.commit_staged_topology_transaction(
            transaction,
            chunk_items,
            system_chunk_array,
            devices,
            stage,
            None,
            None,
            &[],
            None,
            None,
            log_root,
            bytes_used,
        )
    }

    fn collect_leaf_items(
        &self,
        logical: u64,
        owner: u64,
        visited: &mut BTreeSet<u64>,
        output: &mut Vec<RecoveryLogItem>,
    ) -> AxResult<()> {
        // Metadata COW makes a cycle corrupt rather than a valid sharing
        // pattern.  Detect it before recursive descent so malformed media
        // cannot turn recovery into an unbounded walk.
        if !visited.insert(logical) {
            return Err(AxError::Io);
        }
        let image = self.volume.read_checked_tree_block(
            logical,
            self.superblock.nodesize as usize,
            &self.superblock.fsid,
            self.superblock.csum_type,
        )?;
        let block = BtrfsTreeBlock::decode(
            &image,
            &self.superblock.fsid,
            Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
            logical,
        )?;
        if block.owner() != owner || block.generation() > self.superblock.generation {
            return Err(AxError::Io);
        }
        if block.level() == 0 {
            output
                .try_reserve(block.item_count() as usize)
                .map_err(|_| AxError::NoMemory)?;
            for index in 0..block.item_count() {
                let TreeLeafItem { key, value } = block.leaf_item(index)?;
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| AxError::NoMemory)?;
                owned.extend_from_slice(value);
                output.push(RecoveryLogItem { key, value: owned });
            }
            return Ok(());
        }
        for index in 0..block.item_count() {
            let child = block.child(index)?;
            if child.generation > self.superblock.generation {
                return Err(AxError::Io);
            }
            let image = self.volume.read_checked_tree_block(
                child.bytenr,
                self.superblock.nodesize as usize,
                &self.superblock.fsid,
                self.superblock.csum_type,
            )?;
            let decoded = BtrfsTreeBlock::decode(
                &image,
                &self.superblock.fsid,
                Checksum::from_disk(self.superblock.csum_type, &image[..32])?,
                child.bytenr,
            )?;
            if decoded.owner() != owner
                || decoded.generation() != child.generation
                || decoded.generation() > block.generation()
                || decoded.level().checked_add(1) != Some(block.level())
            {
                return Err(AxError::Io);
            }
            self.collect_leaf_items(child.bytenr, owner, visited, output)?;
        }
        Ok(())
    }

    fn collect_tree_items(
        &self,
        logical: u64,
        owner: u64,
        visited: &mut BTreeSet<u64>,
        output: &mut Vec<RawTreeItem>,
    ) -> AxResult<()> {
        Self::collect_tree_items_from(
            &self.volume,
            &self.superblock,
            logical,
            owner,
            visited,
            output,
        )
    }

    fn collect_tree_items_from(
        volume: &BtrfsVolume,
        superblock: &BtrfsSuperblock,
        logical: u64,
        owner: u64,
        visited: &mut BTreeSet<u64>,
        output: &mut Vec<RawTreeItem>,
    ) -> AxResult<()> {
        if !visited.insert(logical) {
            return Err(AxError::Io);
        }
        let image = volume.read_checked_tree_block(
            logical,
            superblock.nodesize as usize,
            &superblock.fsid,
            superblock.csum_type,
        )?;
        let block = BtrfsTreeBlock::decode(
            &image,
            &superblock.fsid,
            Checksum::from_disk(superblock.csum_type, &image[..32])?,
            logical,
        )?;
        if block.owner() != owner || block.generation() > superblock.generation {
            return Err(AxError::Io);
        }
        if block.level() == 0 {
            output
                .try_reserve(block.item_count() as usize)
                .map_err(|_| AxError::NoMemory)?;
            for index in 0..block.item_count() {
                let TreeLeafItem { key, value } = block.leaf_item(index)?;
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| AxError::NoMemory)?;
                owned.extend_from_slice(value);
                output.push(RawTreeItem { key, value: owned });
            }
            return Ok(());
        }
        for index in 0..block.item_count() {
            let child = block.child(index)?;
            let image = volume.read_checked_tree_block(
                child.bytenr,
                superblock.nodesize as usize,
                &superblock.fsid,
                superblock.csum_type,
            )?;
            let decoded = BtrfsTreeBlock::decode(
                &image,
                &superblock.fsid,
                Checksum::from_disk(superblock.csum_type, &image[..32])?,
                child.bytenr,
            )?;
            if decoded.owner() != owner || decoded.generation() != child.generation {
                return Err(AxError::Io);
            }
            Self::collect_tree_items_from(
                volume,
                superblock,
                child.bytenr,
                owner,
                visited,
                output,
            )?;
        }
        Ok(())
    }
}

impl BtrfsMutationPlanner {
    pub fn transaction_mut(&mut self) -> &mut super::BtrfsTransaction {
        &mut self.transaction
    }

    /// Stages one native metadata-node extent and its owning tree relation in
    /// the same planner image.  Callers pair this with a negative delayed
    /// reference when the superseded COW node becomes unreachable.
    #[allow(dead_code)]
    pub fn add_tree_block_ref(
        &mut self,
        bytenr: u64,
        len: u64,
        tree: u64,
        generation: u64,
        level: u8,
    ) -> AxResult<()> {
        if bytenr == 0 || len == 0 || tree == 0 {
            return Err(AxError::InvalidInput);
        }
        let extent_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::EXTENT_ITEM,
            offset: len,
        };
        let relation_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::TREE_BLOCK_REF,
            offset: tree,
        };
        match self
            .tree_items(TreeId::Extent as u64)?
            .binary_search_by_key(&extent_key, |item| item.key)
        {
            Ok(index) => {
                if self
                    .tree_items(TreeId::Extent as u64)?
                    .binary_search_by_key(&relation_key, |item| item.key)
                    .is_ok()
                {
                    return Err(AxError::AlreadyExists);
                }
                let (references, old_generation, old_level) = super::decode_tree_extent_item(
                    &self.tree_items(TreeId::Extent as u64)?[index].value,
                )?;
                if old_level != level || old_generation > generation {
                    return Err(AxError::Io);
                }
                self.set_item(
                    TreeId::Extent as u64,
                    extent_key,
                    super::encode_tree_extent_item(
                        old_generation,
                        references.checked_add(1).ok_or(AxError::NoMemory)?,
                        level,
                    )?,
                )?;
            }
            Err(_) => self.set_item(
                TreeId::Extent as u64,
                extent_key,
                super::encode_tree_extent_item(generation, 1, level)?,
            )?,
        }
        self.set_item(
            TreeId::Extent as u64,
            relation_key,
            super::encode_tree_block_ref(tree)?,
        )?;
        self.transaction.add_delayed_ref(super::DelayedRef {
            bytenr,
            len,
            root: tree,
            owner: tree,
            identity: super::DelayedRefIdentity::TreeBlock,
            delta: 1,
        })
    }

    #[allow(dead_code)]
    pub fn retire_tree_block_ref(&mut self, bytenr: u64, len: u64, tree: u64) -> AxResult<()> {
        if bytenr == 0 || len == 0 || tree == 0 {
            return Err(AxError::InvalidInput);
        }
        let extent_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::EXTENT_ITEM,
            offset: len,
        };
        let relation_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::TREE_BLOCK_REF,
            offset: tree,
        };
        let index = self
            .tree_items(TreeId::Extent as u64)?
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let (references, generation, level) =
            super::decode_tree_extent_item(&self.tree_items(TreeId::Extent as u64)?[index].value)?;
        if references == 0 {
            return Err(AxError::Io);
        }
        let relation = self
            .tree_items(TreeId::Extent as u64)?
            .binary_search_by_key(&relation_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        super::decode_tree_block_ref(&self.tree_items(TreeId::Extent as u64)?[relation].value)?;
        self.delete_item(TreeId::Extent as u64, relation_key)?;
        if references == 1 {
            self.delete_item(TreeId::Extent as u64, extent_key)?;
        } else {
            self.set_item(
                TreeId::Extent as u64,
                extent_key,
                super::encode_tree_extent_item(generation, references - 1, level)?,
            )?;
        }
        self.transaction.add_delayed_ref(super::DelayedRef {
            bytenr,
            len,
            root: tree,
            owner: tree,
            identity: super::DelayedRefIdentity::TreeBlock,
            delta: -1,
        })
    }

    pub fn tree_items(&self, objectid: u64) -> AxResult<&[RawTreeItem]> {
        self.trees
            .get(&objectid)
            .map(Vec::as_slice)
            .ok_or(AxError::InvalidInput)
    }

    /// Replaces one exact native item in a checked tree image.  The B-tree
    /// ordering invariant is re-established immediately, so later tree COW
    /// cannot accidentally encode a malformed leaf.
    pub fn set_item(&mut self, objectid: u64, key: TreeItemKey, value: Vec<u8>) -> AxResult<()> {
        let items = self.trees.get_mut(&objectid).ok_or(AxError::InvalidInput)?;
        match items.binary_search_by_key(&key, |item| item.key) {
            Ok(index) => items[index].value = value,
            Err(index) => {
                items.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                items.insert(index, RawTreeItem { key, value });
            }
        }
        Ok(())
    }

    pub fn delete_item(&mut self, objectid: u64, key: TreeItemKey) -> AxResult<Vec<u8>> {
        let items = self.trees.get_mut(&objectid).ok_or(AxError::InvalidInput)?;
        let index = items
            .binary_search_by_key(&key, |item| item.key)
            .map_err(|_| AxError::NotFound)?;
        Ok(items.remove(index).value)
    }

    #[allow(dead_code)]
    pub fn fs_root_objectid(&self) -> u64 {
        self.fs_root_objectid
    }
    #[allow(dead_code)]
    pub fn fs_tree_owner(&self) -> u64 {
        self.fs_tree_owner
    }

    /// Adds one file relation to an existing regular data extent.  The
    /// relation and the extent-item counter move together; callers use this
    /// only for `RangeSegment::Retain`, never for a newly allocated extent
    /// whose initial reference is already one.
    pub fn add_regular_extent_ref(
        &mut self,
        root: u64,
        inode: u64,
        file_offset: u64,
        bytenr: u64,
        len: u64,
    ) -> AxResult<()> {
        let tree = TreeId::Extent as u64;
        let extent_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::EXTENT_ITEM,
            offset: len,
        };
        let index = self
            .tree_items(tree)?
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let mut extent = self.tree_items(tree)?[index].value.clone();
        if extent.len() < 24
            || u64::from_le_bytes(extent[16..24].try_into().map_err(|_| AxError::Io)?) != 1
        {
            return Err(AxError::Io);
        }
        let references = u64::from_le_bytes(extent[..8].try_into().map_err(|_| AxError::Io)?);
        if references == 0 {
            return Err(AxError::Io);
        }
        extent[..8].copy_from_slice(
            &references
                .checked_add(1)
                .ok_or(AxError::NoMemory)?
                .to_le_bytes(),
        );
        self.set_item(tree, extent_key, extent)?;
        let mut relation = Vec::new();
        relation
            .try_reserve_exact(24)
            .map_err(|_| AxError::NoMemory)?;
        relation.extend_from_slice(&root.to_le_bytes());
        relation.extend_from_slice(&inode.to_le_bytes());
        relation.extend_from_slice(&file_offset.to_le_bytes());
        let relation_key = TreeItemKey {
            objectid: bytenr,
            item_type: EXTENT_DATA_REF,
            offset: u64::from(crc32c(&relation)),
        };
        match self
            .tree_items(tree)?
            .binary_search_by_key(&relation_key, |item| item.key)
        {
            Ok(index) => {
                let (item_root, owner, offset, count) =
                    super::decode_extent_data_ref(&self.tree_items(tree)?[index].value)?;
                if item_root != root || owner != inode || offset != file_offset {
                    return Err(AxError::Io);
                }
                self.set_item(
                    tree,
                    relation_key,
                    super::encode_extent_data_ref(
                        root,
                        inode,
                        file_offset,
                        count.checked_add(1).ok_or(AxError::NoMemory)?,
                    )?,
                )?;
            }
            Err(_) => self.set_item(
                tree,
                relation_key,
                super::encode_extent_data_ref(root, inode, file_offset, 1)?,
            )?,
        }
        Ok(())
    }

    /// Replaces the complete v2 free-space image with canonical extent
    /// records.  This is also the safe conversion path for a bitmap group:
    /// retaining a bitmap while adding extent records double-advertises the
    /// same sectors after remount.
    pub fn replace_free_space_extents(&mut self, extents: &[(u64, u64)]) -> AxResult<()> {
        let tree = TreeId::FreeSpace as u64;
        let mut ordered = extents.to_vec();
        ordered.sort_by_key(|&(logical, _)| logical);
        for &(logical, len) in &ordered {
            if logical == 0 || len == 0 {
                return Err(AxError::Io);
            }
        }
        for pair in ordered.windows(2) {
            if pair[0]
                .0
                .checked_add(pair[0].1)
                .is_none_or(|end| end > pair[1].0)
            {
                return Err(AxError::Io);
            }
        }
        let infos: Vec<_> = self
            .tree_items(tree)?
            .iter()
            .filter(|item| item.key.item_type == FREE_SPACE_INFO)
            .cloned()
            .collect();
        for &(logical, len) in &ordered {
            let end = logical.checked_add(len).ok_or(AxError::Io)?;
            if !infos.iter().any(|info| {
                logical >= info.key.objectid
                    && end <= info.key.objectid.saturating_add(info.key.offset)
            }) {
                return Err(AxError::Io);
            }
        }
        let stale: Vec<_> = self
            .tree_items(tree)?
            .iter()
            .filter(|item| matches!(item.key.item_type, FREE_SPACE_EXTENT | FREE_SPACE_BITMAP))
            .map(|item| item.key)
            .collect();
        for key in stale {
            let _ = self.delete_item(tree, key)?;
        }
        for &(logical, len) in &ordered {
            self.set_item(
                tree,
                TreeItemKey {
                    objectid: logical,
                    item_type: FREE_SPACE_EXTENT,
                    offset: len,
                },
                Vec::new(),
            )?;
        }
        for info in infos {
            if info.value.len() != 8 {
                return Err(AxError::Io);
            }
            let end = info
                .key
                .objectid
                .checked_add(info.key.offset)
                .ok_or(AxError::Io)?;
            let count = ordered
                .iter()
                .filter(|&&(logical, len)| {
                    logical >= info.key.objectid
                        && logical
                            .checked_add(len)
                            .is_some_and(|extent_end| extent_end <= end)
                })
                .count();
            let count = u32::try_from(count).map_err(|_| AxError::NoMemory)?;
            let mut value = info.value;
            value[..4].copy_from_slice(&count.to_le_bytes());
            value[4..8].copy_from_slice(&0u32.to_le_bytes());
            self.set_item(tree, info.key, value)?;
        }
        Ok(())
    }

    /// Installs one consecutive sector checksum run.  Callers must first
    /// prove that the logical range is newly allocated (or remove all old
    /// covering runs), preventing a short replacement from leaving a stale
    /// digest for a neighbour sector.
    pub fn set_checksum_run(
        &mut self,
        logical: u64,
        sector: u64,
        checksums: &[u8],
    ) -> AxResult<()> {
        if logical == 0 || sector == 0 || checksums.is_empty() || checksums.len() % 4 != 0 {
            return Err(AxError::InvalidInput);
        }
        let sectors = u64::try_from(checksums.len() / 4).map_err(|_| AxError::NoMemory)?;
        let len = sectors.checked_mul(sector).ok_or(AxError::NoMemory)?;
        let end = logical.checked_add(len).ok_or(AxError::NoMemory)?;
        let tree = TreeId::Csum as u64;
        for item in self.tree_items(tree)? {
            if item.key.item_type != CSUM_ITEM {
                continue;
            }
            let old_sectors = u64::try_from(item.value.len() / 4).map_err(|_| AxError::Io)?;
            let old_end = item
                .key
                .objectid
                .checked_add(old_sectors.checked_mul(sector).ok_or(AxError::Io)?)
                .ok_or(AxError::Io)?;
            if logical < old_end && item.key.objectid < end {
                return Err(AxError::ResourceBusy);
            }
        }
        let mut value = Vec::new();
        value
            .try_reserve_exact(checksums.len())
            .map_err(|_| AxError::NoMemory)?;
        value.extend_from_slice(checksums);
        self.set_item(
            tree,
            TreeItemKey {
                objectid: logical,
                item_type: CSUM_ITEM,
                offset: 0,
            },
            value,
        )
    }

    /// Removes one complete checksum range, preserving left/right portions
    /// of packed neighbouring runs.  A data extent may start in the middle
    /// of a checksum item, so deleting only an exact-key match would either
    /// leak stale digests or erase a live neighbour.
    pub fn remove_checksum_range(&mut self, logical: u64, sector: u64, len: u64) -> AxResult<()> {
        if logical == 0 || sector == 0 || len == 0 || logical % sector != 0 || len % sector != 0 {
            return Err(AxError::InvalidInput);
        }
        let end = logical.checked_add(len).ok_or(AxError::NoMemory)?;
        let tree = TreeId::Csum as u64;
        let affected: Vec<_> = self
            .tree_items(tree)?
            .iter()
            .filter(|item| {
                if item.key.item_type != CSUM_ITEM || item.value.len() % 4 != 0 {
                    return false;
                }
                let count = u64::try_from(item.value.len() / 4).ok();
                count
                    .and_then(|count| count.checked_mul(sector))
                    .and_then(|span| item.key.objectid.checked_add(span))
                    .is_some_and(|item_end| logical < item_end && item.key.objectid < end)
            })
            .cloned()
            .collect();
        for item in affected {
            let _ = self.delete_item(tree, item.key)?;
            let item_end = item
                .key
                .objectid
                .checked_add(
                    u64::try_from(item.value.len() / 4)
                        .map_err(|_| AxError::Io)?
                        .checked_mul(sector)
                        .ok_or(AxError::Io)?,
                )
                .ok_or(AxError::Io)?;
            if item.key.objectid < logical {
                let sectors = usize::try_from((logical - item.key.objectid) / sector)
                    .map_err(|_| AxError::Io)?;
                self.set_item(
                    tree,
                    TreeItemKey {
                        objectid: item.key.objectid,
                        item_type: CSUM_ITEM,
                        offset: 0,
                    },
                    item.value[..sectors * 4].to_vec(),
                )?;
            }
            if end < item_end {
                let start =
                    usize::try_from((end - item.key.objectid) / sector).map_err(|_| AxError::Io)?;
                self.set_item(
                    tree,
                    TreeItemKey {
                        objectid: end,
                        item_type: CSUM_ITEM,
                        offset: 0,
                    },
                    item.value[start * 4..].to_vec(),
                )?;
            }
        }
        Ok(())
    }

    /// Updates the native qgroup-info counters in the quota tree.  Both
    /// compressed and uncompressed counters follow the same value for an
    /// uncompressed extent; compressed writers pass their physical usage in
    /// a later specialised path rather than guessing it here.
    pub fn charge_qgroup_on_disk(
        &mut self,
        id: super::QgroupId,
        referenced: i64,
        exclusive: i64,
        generation: u64,
    ) -> AxResult<()> {
        let tree = TreeId::Quota as u64;
        if id.id >> 48 != 0 || generation == 0 {
            return Err(AxError::InvalidInput);
        }
        let objectid = (u64::from(id.level) << 48) | id.id;
        let key = TreeItemKey {
            objectid: 0,
            item_type: super::QGROUP_INFO,
            offset: objectid,
        };
        let mut value = match self
            .tree_items(tree)?
            .binary_search_by_key(&key, |item| item.key)
        {
            Ok(index) => self.tree_items(tree)?[index].value.clone(),
            Err(_) => Vec::new(),
        };
        if value.is_empty() {
            value.try_reserve_exact(40).map_err(|_| AxError::NoMemory)?;
            value.resize(40, 0);
        }
        if value.len() != 40 {
            return Err(AxError::Io);
        }
        let add = |bytes: &mut [u8], offset: usize, delta: i64| -> AxResult<()> {
            let old = u64::from_le_bytes(
                bytes[offset..offset + 8]
                    .try_into()
                    .map_err(|_| AxError::Io)?,
            );
            let next = i128::from(old)
                .checked_add(i128::from(delta))
                .ok_or(AxError::NoMemory)?;
            if next < 0 || next > i128::from(u64::MAX) {
                return Err(AxError::Io);
            }
            bytes[offset..offset + 8].copy_from_slice(&(next as u64).to_le_bytes());
            Ok(())
        };
        value[..8].copy_from_slice(&generation.to_le_bytes());
        add(&mut value, 8, referenced)?;
        add(&mut value, 16, referenced)?;
        add(&mut value, 24, exclusive)?;
        add(&mut value, 32, exclusive)?;
        self.set_item(tree, key, value)
    }

    /// Removes this file's data-reference relation and returns whether the
    /// physical extent became exclusively free.  The extent-item payload is
    /// retained byte-for-byte except for its reference counter when another
    /// reflink still owns it.
    pub fn retire_regular_extent(
        &mut self,
        root: u64,
        inode: u64,
        file_offset: u64,
        bytenr: u64,
        len: u64,
    ) -> AxResult<bool> {
        let tree = TreeId::Extent as u64;
        let extent_key = TreeItemKey {
            objectid: bytenr,
            item_type: super::EXTENT_ITEM,
            offset: len,
        };
        let index = self
            .tree_items(tree)?
            .binary_search_by_key(&extent_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let mut extent = self.tree_items(tree)?[index].value.clone();
        if extent.len() < 24
            || u64::from_le_bytes(extent[16..24].try_into().map_err(|_| AxError::Io)?) != 1
        {
            return Err(AxError::Io);
        }
        let references = u64::from_le_bytes(extent[..8].try_into().map_err(|_| AxError::Io)?);
        if references == 0 {
            return Err(AxError::Io);
        }
        let ref_key = self
            .tree_items(tree)?
            .iter()
            .find(|item| {
                item.key.objectid == bytenr
                    && item.key.item_type == EXTENT_DATA_REF
                    && super::decode_extent_data_ref(&item.value).ok().is_some_and(
                        |(item_root, owner, offset, _)| {
                            item_root == root && owner == inode && offset == file_offset
                        },
                    )
            })
            .map(|item| item.key)
            .ok_or(AxError::Io)?;
        let ref_index = self
            .tree_items(tree)?
            .binary_search_by_key(&ref_key, |item| item.key)
            .map_err(|_| AxError::Io)?;
        let (_, _, _, count) =
            super::decode_extent_data_ref(&self.tree_items(tree)?[ref_index].value)?;
        if count == 1 {
            let _ = self.delete_item(tree, ref_key)?;
        } else {
            self.set_item(
                tree,
                ref_key,
                super::encode_extent_data_ref(root, inode, file_offset, count - 1)?,
            )?;
        }
        if references == 1 {
            let _ = self.delete_item(tree, extent_key)?;
            Ok(true)
        } else {
            extent[..8].copy_from_slice(&(references - 1).to_le_bytes());
            self.set_item(tree, extent_key, extent)?;
            Ok(false)
        }
    }

    /// Stages the shared accounting half of one native log EXTENT_DATA
    /// replacement.  The caller supplies the FreeSpace snapshot used by all
    /// logged subvolumes and calls `finish_logged_extent_accounting` once,
    /// after every transition has been admitted.  Thus no per-root replay can
    /// publish a stale free-space or qgroup view.
    pub fn prepare_logged_extent_transition(
        &mut self,
        transition: &LoggedExtentTransition,
        generation: u64,
        sector: u64,
        free_space: &BtrfsLogicalAllocator,
        new_checksums: Option<&[u8]>,
    ) -> AxResult<()> {
        if generation == 0 || sector == 0 || !sector.is_power_of_two() {
            return Err(AxError::InvalidInput);
        }
        let qgroup = super::QgroupId {
            level: 0,
            id: transition.root,
        };
        if transition.same_physical_mapping() {
            return Ok(());
        }

        if let Some(old) = &transition.old {
            if LoggedExtentTransition::requires_physical_accounting(old) {
                let became_free = self.retire_regular_extent(
                    transition.root,
                    transition.inode,
                    transition.file_offset,
                    old.disk_bytenr,
                    old.disk_num_bytes,
                )?;
                self.transaction.add_delayed_ref(super::DelayedRef {
                    bytenr: old.disk_bytenr,
                    len: old.disk_num_bytes,
                    root: transition.root,
                    owner: transition.inode,
                    identity: super::DelayedRefIdentity::Data {
                        file_offset: transition.file_offset,
                    },
                    delta: -1,
                })?;
                let bytes = i64::try_from(old.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
                self.transaction.charge_qgroup(
                    qgroup,
                    -bytes,
                    if became_free { -bytes } else { 0 },
                )?;
                if self.tree_items(TreeId::Quota as u64).is_ok() {
                    self.charge_qgroup_on_disk(
                        qgroup,
                        -bytes,
                        if became_free { -bytes } else { 0 },
                        generation,
                    )?;
                }
                if became_free {
                    if old.kind == super::BtrfsExtentKind::Regular {
                        self.remove_checksum_range(old.disk_bytenr, sector, old.disk_num_bytes)?;
                    }
                    free_space.add_free(old.disk_bytenr, old.disk_num_bytes)?;
                }
            }
        }

        if !LoggedExtentTransition::requires_physical_accounting(&transition.new) {
            return Ok(());
        }
        if !LoggedExtentTransition::supports_accounting(&transition.new) {
            return Err(AxError::OperationNotSupported);
        }
        let new = &transition.new;
        let extent_key = TreeItemKey {
            objectid: new.disk_bytenr,
            item_type: EXTENT_ITEM,
            offset: new.disk_num_bytes,
        };
        let exists = self
            .tree_items(TreeId::Extent as u64)?
            .binary_search_by_key(&extent_key, |item| item.key)
            .is_ok();
        if exists {
            self.add_regular_extent_ref(
                transition.root,
                transition.inode,
                transition.file_offset,
                new.disk_bytenr,
                new.disk_num_bytes,
            )?;
        } else {
            free_space.consume_exact(new.disk_bytenr, new.disk_num_bytes)?;
            self.set_item(
                TreeId::Extent as u64,
                extent_key,
                super::encode_data_extent_item(generation, 1)?,
            )?;
            let mut relation = Vec::new();
            relation
                .try_reserve_exact(24)
                .map_err(|_| AxError::NoMemory)?;
            relation.extend_from_slice(&transition.root.to_le_bytes());
            relation.extend_from_slice(&transition.inode.to_le_bytes());
            relation.extend_from_slice(&transition.file_offset.to_le_bytes());
            self.set_item(
                TreeId::Extent as u64,
                TreeItemKey {
                    objectid: new.disk_bytenr,
                    item_type: EXTENT_DATA_REF,
                    offset: u64::from(crc32c(&relation)),
                },
                super::encode_extent_data_ref(
                    transition.root,
                    transition.inode,
                    transition.file_offset,
                    1,
                )?,
            )?;
            if new.kind == super::BtrfsExtentKind::Regular {
                let checksums = new_checksums.ok_or(AxError::InvalidInput)?;
                let sectors = new.disk_num_bytes.checked_div(sector).ok_or(AxError::Io)?;
                if new.disk_num_bytes % sector != 0
                    || checksums.len()
                        != usize::try_from(sectors.checked_mul(4).ok_or(AxError::NoMemory)?)
                            .map_err(|_| AxError::NoMemory)?
                {
                    return Err(AxError::Io);
                }
                self.set_checksum_run(new.disk_bytenr, sector, checksums)?;
            }
        }
        self.transaction.add_delayed_ref(super::DelayedRef {
            bytenr: new.disk_bytenr,
            len: new.disk_num_bytes,
            root: transition.root,
            owner: transition.inode,
            identity: super::DelayedRefIdentity::Data {
                file_offset: transition.file_offset,
            },
            delta: 1,
        })?;
        let bytes = i64::try_from(new.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
        self.transaction
            .charge_qgroup(qgroup, bytes, if exists { 0 } else { bytes })?;
        if self.tree_items(TreeId::Quota as u64).is_ok() {
            self.charge_qgroup_on_disk(qgroup, bytes, if exists { 0 } else { bytes }, generation)?;
        }
        Ok(())
    }

    /// Commits the single canonical FreeSpace image after all per-root log
    /// transitions have modified the shared allocator snapshot.
    pub fn finish_logged_extent_accounting(
        &mut self,
        free_space: &BtrfsLogicalAllocator,
    ) -> AxResult<()> {
        self.replace_free_space_extents(&free_space.free_extents())
    }

    /// Stages data ownership removal before its filesystem-tree key is
    /// deleted.  This is deliberately public to the truncate/orphan layer:
    /// after the key is removed there is no reliable way to infer whether a
    /// physical extent was shared, checksummed, or preallocated.
    pub fn prepare_logged_extent_retirement(
        &mut self,
        retirement: &LoggedExtentRetirement,
        generation: u64,
        sector: u64,
        free_space: &BtrfsLogicalAllocator,
    ) -> AxResult<u64> {
        let old = &retirement.old;
        if !LoggedExtentTransition::requires_physical_accounting(old) {
            return Ok(0);
        }
        // Teardown is physical metadata work. A compressed mapping still
        // owns its disk_bytenr/disk_num_bytes reference and checksum range;
        // only unsupported encryption encodings remain unsafe to retire.
        if old.encryption != 0
            || old.other_encoding != 0
            || generation == 0
            || sector == 0
            || !sector.is_power_of_two()
        {
            return Err(AxError::OperationNotSupported);
        }
        let became_free = self.retire_regular_extent(
            retirement.root,
            retirement.inode,
            retirement.file_offset,
            old.disk_bytenr,
            old.disk_num_bytes,
        )?;
        self.transaction.add_delayed_ref(super::DelayedRef {
            bytenr: old.disk_bytenr,
            len: old.disk_num_bytes,
            root: retirement.root,
            owner: retirement.inode,
            identity: super::DelayedRefIdentity::Data {
                file_offset: retirement.file_offset,
            },
            delta: -1,
        })?;
        let bytes = i64::try_from(old.disk_num_bytes).map_err(|_| AxError::NoMemory)?;
        let qgroup = super::QgroupId {
            level: 0,
            id: retirement.root,
        };
        self.transaction
            .charge_qgroup(qgroup, -bytes, if became_free { -bytes } else { 0 })?;
        if self.tree_items(TreeId::Quota as u64).is_ok() {
            self.charge_qgroup_on_disk(
                qgroup,
                -bytes,
                if became_free { -bytes } else { 0 },
                generation,
            )?;
        }
        if became_free {
            if old.kind == super::BtrfsExtentKind::Regular {
                self.remove_checksum_range(old.disk_bytenr, sector, old.disk_num_bytes)?;
            }
            free_space.add_free(old.disk_bytenr, old.disk_num_bytes)?;
        }
        Ok(if became_free { old.disk_num_bytes } else { 0 })
    }

    /// Converts the fully populated planner into the exact COW images used by
    /// `commit_tree_rewrites`.  The caller retains responsibility for data
    /// sector writes before invoking the mount commit, while this method makes
    /// omission of an accounting tree an invalid plan rather than a hidden
    /// best-effort update.
    pub fn into_rewrites(self) -> AxResult<(super::BtrfsTransaction, Vec<BtrfsTreeRewrite>)> {
        let mut rewrites = Vec::new();
        for (&objectid, items) in &self.trees {
            if items.is_empty() {
                return Err(AxError::Io);
            }
            rewrites.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let old_tree_owner = if objectid == self.fs_root_objectid {
                self.fs_tree_owner
            } else {
                objectid
            };
            rewrites.push(BtrfsTreeRewrite {
                root_objectid: objectid,
                tree_owner: objectid,
                old_tree_owner,
                items: items.clone(),
            });
        }
        Ok((self.transaction, rewrites))
    }
}

fn decode_extent(compression: u8, bytes: &[u8], logical_len: usize) -> AxResult<Vec<u8>> {
    let compression = match compression {
        0 => Compression::None,
        1 => Compression::Zlib,
        2 => Compression::Lzo,
        3 => Compression::Zstd,
        _ => return Err(AxError::Io),
    };
    compression.decode(bytes, logical_len)
}
