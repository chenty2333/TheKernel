//! Clean-room Btrfs storage core.
//!
//! The code in this module is based on the published on-disk format and on
//! independently derived invariants.  It intentionally contains no Linux
//! source, translated implementation, or Linux-derived data structure code.
//!
//! Its VFS adapter is built on the checked reader below.  Registration remains
//! gated on the durable COW namespace/data writer; exposing a read/write mount
//! before that boundary exists would be an on-media corruption risk.

mod allocator;
mod compression;
mod format;
mod inode;
mod item;
mod mount;
mod transaction;
mod tree;
mod vfs;
mod volume;

pub use allocator::{BtrfsAllocator, BtrfsLogicalAllocator, LogicalReservation};
pub use compression::Compression;
pub use format::{
    BTRFS_SUPERBLOCK_SIZE, BtrfsSuperblock, Checksum, ChecksumType, crc32c, crc32c_seed,
};
pub use inode::BtrfsInodeState;
pub use item::{
    BtrfsDevExtent, BtrfsDeviceItem, BtrfsDirItem, BtrfsDirLogRange, BtrfsExtentKind,
    BtrfsFileExtent, BtrfsInodeItem, BtrfsInodeRef, BtrfsRootItem, CSUM_ITEM,
    DEV_EXTENT, DEV_ITEM, DIR_INDEX, DIR_ITEM, DIR_LOG_INDEX, DIR_LOG_ITEM, EXTENT_DATA,
    EXTENT_DATA_REF, EXTENT_ITEM, FREE_SPACE_BITMAP, FREE_SPACE_EXTENT, FREE_SPACE_INFO,
    INODE_EXTREF, INODE_ITEM, INODE_REF, ORPHAN_ITEM, QGROUP_INFO, QGROUP_LIMIT, QGROUP_RELATION,
    ROOT_ITEM, TREE_BLOCK_REF, XATTR_ITEM, btrfs_extref_hash, decode_dir_items,
    decode_extent_data_ref, decode_inode_extrefs, decode_inode_refs, decode_tree_block_ref,
    decode_tree_extent_item, encode_data_extent_item, encode_dir_items, encode_extent_data_ref,
    encode_inline_extent, encode_inode_extrefs, encode_inode_refs, encode_prealloc_extent,
    encode_regular_extent, encode_tree_block_ref, encode_tree_extent_item,
};
pub use mount::{
    BtrfsMount, BtrfsMutationPlanner, LoggedExtentRetirement, OrphanRetirement, RangeSegment,
};
pub use transaction::{
    BtrfsCore, BtrfsTransaction, DelayedRef, DelayedRefIdentity, QgroupId, QgroupLimit, TreeId,
    TreeItemKey,
};
pub use tree::{BtrfsTreeBlock, TreeChild, TreeLeafItem, TreeWriteItem};
pub use vfs::BtrfsFilesystem;
pub use volume::{
    BtrfsDeviceTopologyChange, BtrfsTopologyStage, BtrfsVolume, Chunk, ChunkProfile, ScrubReport,
    Stripe,
};

pub(crate) fn set_deferred_orphan_finalizer_waker(waker: fn()) -> bool {
    vfs::set_deferred_orphan_finalizer_waker(waker)
}
pub(crate) fn has_deferred_orphan_finalizer_work() -> bool {
    vfs::has_deferred_orphan_finalizer_work()
}
pub(crate) fn drain_deferred_orphan_finalizers(between: impl FnMut()) -> usize {
    vfs::drain_deferred_orphan_finalizers(between)
}
