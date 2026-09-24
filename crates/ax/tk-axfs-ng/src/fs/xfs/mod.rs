//! Clean-room XFS on-disk metadata access.
//!
//! This module deliberately stops short of registering an XFS mount with the
//! VFS.  A filesystem may only become visible once all namespace mutations,
//! write ordering, recovery, and permission operations are backed by the
//! corresponding on-disk transactions.  What is here is the shared,
//! read-only foundation for that provider: it reads real XFS v4/v5
//! superblocks, allocation-group headers, inode cores, and extent records
//! from a [`BlockVolume`].  No Linux source is included or translated.

use alloc::{sync::Arc, vec, vec::Vec};
use core::cmp;

use axdriver::{BlockVolume, prelude::DevError};
use axhal::time::wall_time;
use kspin::SpinNoPreempt as SpinMutex;

use crate::MountedBlockDevice;

const XFS_SB_MAGIC: u32 = 0x5846_5342; // "XFSB"
const XFS_AGF_MAGIC: u32 = 0x5841_4746; // "XAGF"
const XFS_AGI_MAGIC: u32 = 0x5841_4749; // "XAGI"
const XFS_AGFL_MAGIC: u32 = 0x5841_464c; // "XAFL"
const XFS_DINODE_MAGIC: u16 = 0x494e; // "IN"
const XFS_LOG_RECORD_MAGIC: u32 = 0xfeed_babe;
const XFS_BMAP_MAGIC: u32 = 0x424d_4150;
const XFS_BMAP_CRC_MAGIC: u32 = 0x424d_4133;
// CRC-enabled AG btrees.  rmapbt/refcountbt only exist on v5 media, so
// accepting their non-CRC predecessors would turn an unsupported layout into
// an unauthenticated recovery target.
const XFS_RMAP_CRC_MAGIC: u32 = 0x524d_4233; // "RMB3"
const XFS_REFCOUNT_CRC_MAGIC: u32 = 0x5243_4633; // "RCF3"
// rmap record offset flag for an inode's external bmapbt blocks.  The owner
// remains the inode number; only the high offset bit distinguishes metadata
// fork blocks from ordinary file data ownership.
const XFS_RMAP_OFF_BMBT: u64 = 1u64 << 63;
const XFS_DIR2_BLOCK_MAGIC: u32 = 0x5844_3242;
const XFS_DIR2_DATA_MAGIC: u32 = 0x5844_3244;
const XFS_DIR3_BLOCK_MAGIC: u32 = 0x5844_4233;
const XFS_DIR3_DATA_MAGIC: u32 = 0x5844_4433;
const XFS_DIR2_FREE_MAGIC: u32 = 0x5844_3246;
const XFS_DIR3_FREE_MAGIC: u32 = 0x5844_4633;
const XFS_DA_NODE_MAGIC: u16 = 0xfebe;
const XFS_DA3_NODE_MAGIC: u16 = 0x3ebe;
const XFS_DIR_DATA_FREE_TAG: u16 = 0xffff;
const XFS_DIR2_LEAF1_MAGIC: u16 = 0xd2f1;
const XFS_DIR2_LEAFN_MAGIC: u16 = 0xd2ff;
const XFS_DIR3_LEAF1_MAGIC: u16 = 0x3df1;
const XFS_DIR3_LEAFN_MAGIC: u16 = 0x3dff;
const XFS_ATTR_LEAF_MAGIC: u16 = 0xfbee;
const XFS_ATTR3_LEAF_MAGIC: u16 = 0x3bee;
const XFS_DIR_LEAF_SPACE_BYTES: u64 = 1 << 35;
// The dir2 free-space address space follows the leaf address space.  These
// are byte offsets in the directory's sparse data fork, not disk addresses.
const XFS_DIR_FREE_SPACE_BYTES: u64 = 1 << 36;
pub const XFS_ATTR_LOCAL: u8 = 0x01;
pub const XFS_ATTR_ROOT: u8 = 0x02;
pub const XFS_ATTR_SECURE: u8 = 0x08;
// Native dinode flag layout.  Keep these in the media module so every VFS
// projection and inode writer interprets the same on-disk bits.
pub(crate) const XFS_DIFLAG_REALTIME: u16 = 1 << 0;
pub(crate) const XFS_DIFLAG_PREALLOC: u16 = 1 << 1;
pub(crate) const XFS_DIFLAG_IMMUTABLE: u16 = 1 << 3;
pub(crate) const XFS_DIFLAG_APPEND: u16 = 1 << 4;
pub(crate) const XFS_DIFLAG_SYNC: u16 = 1 << 5;
pub(crate) const XFS_DIFLAG_NOATIME: u16 = 1 << 6;
pub(crate) const XFS_DIFLAG_NODUMP: u16 = 1 << 7;
pub(crate) const XFS_DIFLAG_RTINHERIT: u16 = 1 << 8;
pub(crate) const XFS_DIFLAG_PROJINHERIT: u16 = 1 << 9;
pub(crate) const XFS_DIFLAG_NOSYMLINKS: u16 = 1 << 10;
pub(crate) const XFS_DIFLAG_EXTSIZE: u16 = 1 << 11;
pub(crate) const XFS_DIFLAG_EXTSZINHERIT: u16 = 1 << 12;
pub(crate) const XFS_DIFLAG_NODEFRAG: u16 = 1 << 13;
pub(crate) const XFS_DIFLAG_FILESTREAM: u16 = 1 << 14;
pub(crate) const XFS_DIFLAG2_DAX: u64 = 1 << 0;
pub(crate) const XFS_DIFLAG2_COWEXTSIZE: u64 = 1 << 2;
const XLOG_START_TRANS: u8 = 0x01;
const XLOG_COMMIT_TRANS: u8 = 0x02;
const XLOG_CONTINUE_TRANS: u8 = 0x04;
const XLOG_WAS_CONT_TRANS: u8 = 0x08;
const XLOG_END_TRANS: u8 = 0x10;
const XLOG_UNMOUNT_TRANS: u8 = 0x20;

/// Failure while decoding or accessing an XFS volume.  Corrupt media is kept
/// distinct from an unsupported feature so callers never mistake one for a
/// mountable filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsError {
    Io,
    InvalidSuperblock,
    CorruptMetadata,
    UnsupportedFeature,
    AddressOutOfRange,
    NotEmpty,
    QuotaExceeded,
    NoMemory,
}

pub type XfsResult<T> = Result<T, XfsError>;

impl From<DevError> for XfsError {
    fn from(error: DevError) -> Self {
        match error {
            DevError::NoMemory => Self::NoMemory,
            DevError::InvalidParam => Self::AddressOutOfRange,
            _ => Self::Io,
        }
    }
}

mod dir;
mod helpers;
mod inode;
mod log_items;
mod mount;
mod ondisk;
mod recovery;
mod transaction;
mod volume;
mod volume_ag;
mod volume_alloc;
mod volume_dir;
mod volume_inode;
mod volume_io;
mod volume_lookup;
mod volume_open;
mod volume_replay;
mod volume_xattr;

use self::helpers::*;
pub use self::{
    dir::*, inode::*, log_items::*, mount::*, ondisk::*, recovery::*, transaction::*, volume::*,
};

#[cfg(test)]
mod tests {
    #[cfg(feature = "test-ramdisk")]
    use axdriver::{
        AxBlockDevice, BlockFaultLifetime, BlockFaultOperation, BlockFaultRule, SharedBlockDevice,
    };
    #[cfg(feature = "test-ramdisk")]
    use axdriver_block::ramdisk::RamDisk;

    use super::*;

    fn dquot_image(id: u32, quota_type: u8, blocks: u64, inodes: u64, uuid: XfsUuid) -> Vec<u8> {
        let mut image = vec![0; 136];
        put_be16(&mut image, 0, 0x4451).unwrap();
        image[2] = 1;
        image[3] = quota_type;
        put_be32(&mut image, 4, id).unwrap();
        put_be64(&mut image, 40, blocks).unwrap();
        put_be64(&mut image, 48, inodes).unwrap();
        image[120..136].copy_from_slice(&uuid.0);
        rewrite_crc32c(&mut image, 108).unwrap();
        image
    }

    #[test]
    fn transaction_composition_conflict_is_a_pure_unit_invariant() {
        let before = vec![0; XFS_LOG_BASIC_BLOCK];
        let mut first_after = before.clone();
        first_after[7] = 1;
        let mut second_after = before.clone();
        second_after[19] = 2;
        let transaction = XfsMetadataTransaction {
            buffers: vec![
                XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Inode,
                    basic_block: 100,
                    before: before.clone(),
                    after: first_after,
                },
                XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Inode,
                    basic_block: 100,
                    before: before.clone(),
                    after: second_after,
                },
            ],
            ..Default::default()
        };
        let composed = transaction.composed_buffers().unwrap();
        assert_eq!(composed.len(), 1);
        assert_eq!(composed[0].after[7], 1);
        assert_eq!(composed[0].after[19], 2);

        let mut conflicting_after = before.clone();
        conflicting_after[7] = 3;
        let conflicting = XfsMetadataTransaction {
            buffers: vec![
                transaction.buffers[0].clone(),
                XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Inode,
                    basic_block: 100,
                    before,
                    after: conflicting_after,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            conflicting.composed_buffers(),
            Err(XfsError::CorruptMetadata)
        );
        // Composition is pure: a rejected transaction cannot expose either
        // partially merged image to a later log/home-write phase.
        assert_eq!(conflicting.buffers[0].after[7], 1);
        assert_eq!(conflicting.buffers[1].after[7], 3);
    }

    #[test]
    fn dquot_delta_image_unit_preserves_debit_credit_and_reapplication() {
        let uuid = XfsUuid([0x5a; 16]);
        let before = dquot_image(41, 1, 9, 2, uuid);
        let current = XfsDquot::parse(&before, 41, 1, uuid, false).unwrap();
        let debit = current.apply_delta(5, 1, true, 10, 0, 0).unwrap();
        assert_eq!((debit.blocks, debit.inodes), (14, 3));
        let credited = XfsDquot {
            blocks: debit.blocks,
            inodes: debit.inodes,
            ..current.clone()
        }
        .apply_delta(-5, -1, true, 10, 0, 0)
        .unwrap();
        assert_eq!(
            (credited.blocks, credited.inodes),
            (current.blocks, current.inodes)
        );

        let mut after = before.clone();
        put_be64(&mut after, 40, debit.blocks).unwrap();
        put_be64(&mut after, 48, debit.inodes).unwrap();
        let delta = XfsDquotDelta {
            id: 41,
            quota_type: 1,
            basic_block: 80,
            block_count: 1,
            byte_offset: 0,
            before: before.clone(),
            after,
        };
        let item = delta.log_item(77, uuid, false).unwrap();
        let once = item
            .materialize_home_dquot(&before, 77, true, Some(uuid), false)
            .unwrap();
        let twice = item
            .materialize_home_dquot(&once, 77, true, Some(uuid), false)
            .unwrap();
        assert_eq!(twice, once);
        let durable = XfsDquot::parse(&once, 41, 1, uuid, false).unwrap();
        assert_eq!((durable.blocks, durable.inodes), (14, 3));
    }

    #[test]
    fn reflink_planner_unit_keeps_refcount_and_rmap_in_lockstep() {
        let mut planner = XfsAgMutationPlanner {
            ag: 0,
            free: vec![],
            rmap: vec![XfsRmapRecord {
                start_block: 12,
                block_count: 1,
                owner: 100,
                offset: 4,
            }],
            refcount: vec![],
        };
        assert_eq!(planner.refcount_at(12).unwrap(), 1);
        planner.add_owner(12, 200, 8).unwrap();
        planner.set_refcount(12, 2).unwrap();
        assert_eq!(planner.refcount_at(12).unwrap(), 2);
        assert_eq!(
            planner
                .rmap
                .iter()
                .filter(|record| record.start_block == 12)
                .count(),
            2
        );

        planner.remove_owner(12, 200, 8).unwrap();
        planner.set_refcount(12, 1).unwrap();
        assert_eq!(planner.refcount_at(12).unwrap(), 1);
        assert!(planner.refcount.is_empty());
        assert_eq!(
            planner.rmap,
            vec![XfsRmapRecord {
                start_block: 12,
                block_count: 1,
                owner: 100,
                offset: 4
            }]
        );
    }

    #[cfg(feature = "test-ramdisk")]
    fn recovery_test_volume() -> XfsVolume {
        let first = SharedBlockDevice::new(AxBlockDevice::Existing(RamDisk::new(512)));
        let second = SharedBlockDevice::new(AxBlockDevice::Existing(RamDisk::new(512)));
        let data = BlockVolume::new(vec![first, second]).unwrap();
        XfsVolume {
            data,
            external_log: None,
            realtime: None,
            rtgroup_inodes: Vec::new(),
            superblock: XfsSuperblock {
                block_size: 512,
                data_blocks: 2,
                realtime_blocks: 0,
                realtime_extents: 0,
                realtime_extent_size: 0,
                log_start: 0,
                root_inode: 1,
                realtime_bitmap_inode: 0,
                realtime_summary_inode: 0,
                realtime_bitmap_blocks: 0,
                ag_blocks: 2,
                ag_count: 1,
                log_blocks: 2,
                quota_flags: 0,
                user_quota_inode: 0,
                group_quota_inode: 0,
                project_quota_inode: 0,
                version: XfsSuperblock::VERSION_5,
                version_features: XfsSuperblock::VERSION_DIRV2,
                sector_size: 512,
                inode_size: 256,
                inodes_per_block: 2,
                block_log: 9,
                sector_log: 9,
                inode_log: 8,
                inodes_per_block_log: 1,
                ag_block_log: 1,
                directory_block_log: 0,
                uuid: XfsUuid([0x11; 16]),
                meta_uuid: XfsUuid([0x22; 16]),
                features: XfsFeatures {
                    compat: 0,
                    ro_compat: 0,
                    incompat: 0,
                    log_incompat: 0,
                },
                metadir_inode: 0,
                rtgroup_count: 0,
                rtgroup_extents: 0,
                rtgroup_block_log: 0,
                realtime_start: 0,
                realtime_reserved: 0,
            },
            replay_lock: SpinMutex::new(()),
            _data_claim: None,
        }
    }

    #[cfg(feature = "test-ramdisk")]
    fn agf_recovery_write(basic_block: u64, lsn: u64) -> XfsHomeWriteDescriptor {
        let mut bytes = vec![0; 512];
        bytes[0..4].copy_from_slice(&XFS_AGF_MAGIC.to_be_bytes());
        bytes[208..216].copy_from_slice(&lsn.to_be_bytes());
        XfsHomeWriteDescriptor {
            basic_block,
            bytes,
            lsn,
            item: XfsBufferReplayItem {
                flags: 5 << 11,
                block_number: basic_block,
                block_count: 1,
                dirty_chunks: Vec::new(),
                chunks: Vec::new(),
            },
        }
    }

    #[cfg(feature = "test-ramdisk")]
    #[test]
    fn recovery_retry_skips_fua_completed_home_and_replays_only_missing_home() {
        let volume = recovery_test_volume();
        let commit = XfsRecoveryCommit {
            lsn: 73,
            writes: vec![agf_recovery_write(0, 73), agf_recovery_write(1, 73)],
        };

        volume.data.set_fault_rules(&[BlockFaultRule {
            operation: BlockFaultOperation::WriteFua,
            device: Some(1),
            successful_matches: 0,
            lifetime: BlockFaultLifetime::Once,
        }]);
        assert_eq!(volume.apply_recovery_commit(&commit), Err(XfsError::Io));

        let mut first = [0; 512];
        let mut second = [0; 512];
        volume.data.read_blocks(0, &mut first).unwrap();
        volume.data.read_blocks(1, &mut second).unwrap();
        assert_eq!(be64(&first, 208), Ok(73));
        assert_eq!(be64(&second, 208), Ok(0));

        volume.data.set_fault_rules(&[BlockFaultRule {
            operation: BlockFaultOperation::WriteFua,
            device: Some(0),
            successful_matches: 0,
            lifetime: BlockFaultLifetime::Persistent,
        }]);
        volume.apply_recovery_commit(&commit).unwrap();
        volume.data.read_blocks(0, &mut first).unwrap();
        volume.data.read_blocks(1, &mut second).unwrap();
        assert_eq!(be64(&first, 208), Ok(73));
        assert_eq!(be64(&second, 208), Ok(73));
    }
}
