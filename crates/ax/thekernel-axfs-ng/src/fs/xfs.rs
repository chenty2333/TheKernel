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
use axdriver::BlockVolume;
use axdriver::prelude::DevError;
use axhal::time::wall_time;
use core::cmp;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsUuid(pub [u8; 16]);

/// On-disk XFS feature words.  The individual bits intentionally remain raw:
/// they are a persistent format contract, not a policy decision made at
/// mount time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsFeatures {
    pub compat: u32,
    pub ro_compat: u32,
    pub incompat: u32,
    pub log_incompat: u32,
}

impl XfsFeatures {
    /// v5 metadata CRCs live in the incompatible feature word.
    pub const INCOMPAT_FTYPE: u32 = 1 << 0;
    pub const INCOMPAT_SPINODES: u32 = 1 << 1;
    pub const INCOMPAT_META_UUID: u32 = 1 << 2;
    pub const INCOMPAT_BIGTIME: u32 = 1 << 3;
    pub const INCOMPAT_NEEDSREPAIR: u32 = 1 << 4;
    pub const INCOMPAT_METADIR: u32 = 1 << 8;
    pub const RO_COMPAT_RMAPBT: u32 = 1 << 1;
    pub const RO_COMPAT_REFLINK: u32 = 1 << 2;

    pub const fn has_rmapbt(self) -> bool {
        self.ro_compat & Self::RO_COMPAT_RMAPBT != 0
    }

    pub const fn has_reflink(self) -> bool {
        self.ro_compat & Self::RO_COMPAT_REFLINK != 0
    }

    pub const fn needs_repair(self) -> bool {
        self.incompat & Self::INCOMPAT_NEEDSREPAIR != 0
    }
}

/// Immutable XFS geometry decoded from the primary superblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsSuperblock {
    pub block_size: u32,
    pub data_blocks: u64,
    pub realtime_blocks: u64,
    pub realtime_extents: u64,
    pub realtime_extent_size: u32,
    pub log_start: u64,
    pub root_inode: u64,
    pub realtime_bitmap_inode: u64,
    pub realtime_summary_inode: u64,
    /// Number of bitmap file blocks on legacy (non-rtgroup) media.
    pub realtime_bitmap_blocks: u32,
    pub ag_blocks: u32,
    pub ag_count: u32,
    pub log_blocks: u32,
    pub quota_flags: u16,
    pub user_quota_inode: u64,
    pub group_quota_inode: u64,
    pub project_quota_inode: u64,
    pub version: u16,
    /// v4 superblock version feature bits (the upper bits of
    /// `sb_versionnum`).  They remain relevant on legacy-format media.
    pub version_features: u16,
    pub sector_size: u16,
    pub inode_size: u16,
    pub inodes_per_block: u16,
    pub block_log: u8,
    pub sector_log: u8,
    pub inode_log: u8,
    pub inodes_per_block_log: u8,
    pub ag_block_log: u8,
    pub directory_block_log: u8,
    pub uuid: XfsUuid,
    pub meta_uuid: XfsUuid,
    pub features: XfsFeatures,
    pub metadir_inode: u64,
    pub rtgroup_count: u32,
    pub rtgroup_extents: u32,
    pub rtgroup_block_log: u8,
    pub realtime_start: u64,
    pub realtime_reserved: u64,
}

impl XfsSuperblock {
    pub const VERSION_5: u16 = 5;
    pub const VERSION_DIRV2: u16 = 1 << 13;

    pub const fn is_v5(self) -> bool {
        self.version == Self::VERSION_5
    }

    pub const fn has_dirv2(self) -> bool {
        self.is_v5() || self.version_features & Self::VERSION_DIRV2 != 0
    }

    pub const fn bytes_per_ag(self) -> u64 {
        self.ag_blocks as u64 * self.block_size as u64
    }

    pub fn parse(bytes: &[u8]) -> XfsResult<Self> {
        // The v5 fields end at 264 bytes.  Older superblocks are a prefix of
        // this layout; requiring the full sector lets all offsets below be
        // checked uniformly and refuses truncated media rather than applying
        // default values to persistent metadata.
        if bytes.len() < 264 || be32(bytes, 0)? != XFS_SB_MAGIC {
            return Err(XfsError::InvalidSuperblock);
        }
        let block_size = be32(bytes, 4)?;
        let data_blocks = be64(bytes, 8)?;
        let realtime_blocks = be64(bytes, 16)?;
        let realtime_extents = be64(bytes, 24)?;
        let realtime_extent_size = be32(bytes, 80)?;
        let mut uuid = [0; 16];
        uuid.copy_from_slice(slice(bytes, 32, 16)?);
        let log_start = be64(bytes, 48)?;
        let root_inode = be64(bytes, 56)?;
        let realtime_bitmap_inode = be64(bytes, 64)?;
        let realtime_summary_inode = be64(bytes, 72)?;
        let ag_blocks = be32(bytes, 84)?;
        let ag_count = be32(bytes, 88)?;
        let realtime_bitmap_blocks = be32(bytes, 92)?;
        let log_blocks = be32(bytes, 96)?;
        let version_word = be16(bytes, 100)?;
        let version = version_word & 0x000f;
        let sector_size = be16(bytes, 102)?;
        let inode_size = be16(bytes, 104)?;
        let inodes_per_block = be16(bytes, 106)?;
        let block_log = byte(bytes, 120)?;
        let sector_log = byte(bytes, 121)?;
        let inode_log = byte(bytes, 122)?;
        let inodes_per_block_log = byte(bytes, 123)?;
        let ag_block_log = byte(bytes, 124)?;
        let directory_block_log = byte(bytes, 192)?;
        let features = if version == Self::VERSION_5 {
            XfsFeatures {
                compat: be32(bytes, 208)?,
                ro_compat: be32(bytes, 212)?,
                incompat: be32(bytes, 216)?,
                log_incompat: be32(bytes, 220)?,
            }
        } else {
            XfsFeatures {
                compat: 0,
                ro_compat: 0,
                incompat: 0,
                log_incompat: 0,
            }
        };
        let mut meta_uuid = [0; 16];
        meta_uuid.copy_from_slice(slice(bytes, 248, 16)?);
        let metadir =
            version == Self::VERSION_5 && features.incompat & XfsFeatures::INCOMPAT_METADIR != 0;
        if metadir && bytes.len() < 304 {
            return Err(XfsError::InvalidSuperblock);
        }
        if metadir && bytes[281..288].iter().any(|byte| *byte != 0) {
            return Err(XfsError::InvalidSuperblock);
        }

        let sb = Self {
            block_size,
            data_blocks,
            realtime_blocks,
            realtime_extents,
            realtime_extent_size,
            log_start,
            root_inode,
            realtime_bitmap_inode,
            realtime_summary_inode,
            ag_blocks,
            ag_count,
            realtime_bitmap_blocks,
            log_blocks,
            quota_flags: be16(bytes, 176)?,
            user_quota_inode: be64(bytes, 160)?,
            group_quota_inode: be64(bytes, 168)?,
            project_quota_inode: if version == Self::VERSION_5 {
                be64(bytes, 232)?
            } else {
                0
            },
            version,
            version_features: version_word & !0x000f,
            sector_size,
            inode_size,
            inodes_per_block,
            block_log,
            sector_log,
            inode_log,
            inodes_per_block_log,
            ag_block_log,
            directory_block_log,
            uuid: XfsUuid(uuid),
            meta_uuid: XfsUuid(meta_uuid),
            features,
            metadir_inode: if metadir { be64(bytes, 264)? } else { 0 },
            rtgroup_count: if metadir { be32(bytes, 272)? } else { 0 },
            rtgroup_extents: if metadir { be32(bytes, 276)? } else { 0 },
            rtgroup_block_log: if metadir { byte(bytes, 280)? } else { 0 },
            realtime_start: if metadir { be64(bytes, 288)? } else { 0 },
            realtime_reserved: if metadir { be64(bytes, 296)? } else { 0 },
        };
        if byte(bytes, 126)? != 0 {
            return Err(XfsError::InvalidSuperblock);
        }
        sb.validate()?;
        if sb.is_v5() {
            verify_crc32c(slice(bytes, 0, sb.sector_size as usize)?, 224)?;
        }
        Ok(sb)
    }

    fn validate(&self) -> XfsResult<()> {
        if self.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            if self.rtgroup_extents < 2 || self.realtime_extent_size == 0 {
                return Err(XfsError::InvalidSuperblock);
            }
            let group_blocks =
                u64::from(self.rtgroup_extents).checked_mul(u64::from(self.realtime_extent_size));
            let expected_groups = self
                .realtime_extents
                .div_ceil(u64::from(self.rtgroup_extents));
            let declared_realtime = self
                .realtime_extents
                .checked_mul(u64::from(self.realtime_extent_size));
            if !self.is_v5()
                || self.metadir_inode == 0
                || self.rtgroup_count == 0
                || !group_blocks.is_some_and(|blocks| {
                    blocks <= 0x7fff_ffff
                        && self.rtgroup_block_log == (64 - (blocks - 1).leading_zeros()) as u8
                })
                || u64::from(self.rtgroup_count) != expected_groups
                || declared_realtime != Some(self.realtime_blocks)
                || self.realtime_reserved > self.realtime_blocks
                || (self.realtime_start != 0
                    && self
                        .realtime_start
                        .checked_add(self.realtime_blocks)
                        .is_none_or(|end| end > self.data_blocks))
            {
                return Err(XfsError::InvalidSuperblock);
            }
        }
        if self.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0 && self.realtime_extents != 0
        {
            let expected = self.realtime_extents.div_ceil(
                u64::from(self.block_size)
                    .checked_mul(8)
                    .ok_or(XfsError::InvalidSuperblock)?,
            );
            if self.realtime_bitmap_blocks == 0
                || u64::from(self.realtime_bitmap_blocks) != expected
            {
                return Err(XfsError::InvalidSuperblock);
            }
        }
        if !matches!(self.version, 4 | 5)
            || !self.block_size.is_power_of_two()
            || !(512..=65536).contains(&self.block_size)
            || self.block_log != self.block_size.ilog2() as u8
            || !self.sector_size.is_power_of_two()
            || self.sector_size < 512
            || self.sector_size as u32 > self.block_size
            || self.sector_log != self.sector_size.ilog2() as u8
            || !self.inode_size.is_power_of_two()
            || !(256..=2048).contains(&self.inode_size)
            || self.inode_size as u32 > self.block_size
            || self.inode_log != self.inode_size.ilog2() as u8
            || self.inodes_per_block == 0
            || self.inodes_per_block_log != self.inodes_per_block.ilog2() as u8
            || self.inodes_per_block as u32 * self.inode_size as u32 != self.block_size
            || self.ag_blocks == 0
            || self.ag_count == 0
            || self.ag_block_log != (u32::BITS - (self.ag_blocks - 1).leading_zeros()) as u8
            || self.data_blocks == 0
            || self.root_inode == 0
            || self
                .block_log
                .checked_add(self.directory_block_log)
                .is_none()
        {
            return Err(XfsError::InvalidSuperblock);
        }
        let declared = (self.ag_count as u64)
            .checked_mul(self.ag_blocks as u64)
            .ok_or(XfsError::InvalidSuperblock)?;
        if self.data_blocks > declared || self.features.needs_repair() {
            return Err(XfsError::InvalidSuperblock);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsAgf {
    pub sequence: u32,
    pub length: u32,
    pub free_blocks: u32,
    pub longest_free_extent: u32,
    pub bno_root: u32,
    pub cnt_root: u32,
    pub rmap_root: Option<u32>,
    pub refcount_root: Option<u32>,
    pub freelist_first: u32,
    pub freelist_last: u32,
    pub freelist_count: u32,
    pub uuid: XfsUuid,
}

impl XfsAgf {
    fn parse(bytes: &[u8], features: XfsFeatures, crc_enabled: bool) -> XfsResult<Self> {
        if bytes.len() < 92 || be32(bytes, 0)? != XFS_AGF_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        if crc_enabled {
            verify_crc32c(bytes, 216)?;
        }
        let rmap_root = if features.has_rmapbt() {
            Some(be32(bytes, 24)?)
        } else {
            None
        };
        let refcount_root = if features.has_reflink() {
            Some(be32(bytes, 88)?)
        } else {
            None
        };
        let mut uuid = [0; 16];
        uuid.copy_from_slice(slice(bytes, 64, 16)?);
        Ok(Self {
            sequence: be32(bytes, 8)?,
            length: be32(bytes, 12)?,
            bno_root: be32(bytes, 16)?,
            cnt_root: be32(bytes, 20)?,
            free_blocks: be32(bytes, 52)?,
            longest_free_extent: be32(bytes, 56)?,
            freelist_first: be32(bytes, 40)?,
            freelist_last: be32(bytes, 44)?,
            freelist_count: be32(bytes, 48)?,
            rmap_root,
            refcount_root,
            uuid: XfsUuid(uuid),
        })
    }

    fn serialize(self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
        let mut bytes = vec![0; sb.sector_size as usize];
        if bytes.len() < if sb.is_v5() { 224 } else { 92 } {
            return Err(XfsError::CorruptMetadata);
        }
        put_be32(&mut bytes, 0, XFS_AGF_MAGIC)?;
        put_be32(&mut bytes, 8, self.sequence)?;
        put_be32(&mut bytes, 12, self.length)?;
        put_be32(&mut bytes, 16, self.bno_root)?;
        put_be32(&mut bytes, 20, self.cnt_root)?;
        if let Some(root) = self.rmap_root {
            put_be32(&mut bytes, 24, root)?;
        }
        put_be32(&mut bytes, 40, self.freelist_first)?;
        put_be32(&mut bytes, 44, self.freelist_last)?;
        put_be32(&mut bytes, 48, self.freelist_count)?;
        put_be32(&mut bytes, 52, self.free_blocks)?;
        put_be32(&mut bytes, 56, self.longest_free_extent)?;
        bytes[64..80].copy_from_slice(&self.uuid.0);
        if let Some(root) = self.refcount_root {
            put_be32(&mut bytes, 88, root)?;
        }
        if sb.is_v5() {
            put_be64(&mut bytes, 208, lsn)?;
            rewrite_crc32c(&mut bytes, 216)?;
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsAgi {
    pub sequence: u32,
    pub length: u32,
    pub inode_count: u32,
    pub free_inode_count: u32,
    pub inode_btree_root: u32,
    pub inode_btree_level: u32,
    pub free_inode_btree_root: Option<u32>,
    pub free_inode_btree_level: Option<u32>,
    pub uuid: XfsUuid,
    pub unlinked: [u32; 64],
}

impl XfsAgi {
    fn parse(bytes: &[u8], crc_enabled: bool) -> XfsResult<Self> {
        if bytes.len() < if crc_enabled { 336 } else { 312 } || be32(bytes, 0)? != XFS_AGI_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        if crc_enabled {
            verify_crc32c(bytes, 312)?;
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(slice(bytes, 296, 16)?);
        let mut unlinked = [0; 64];
        for (index, bucket) in unlinked.iter_mut().enumerate() {
            *bucket = be32(bytes, 40 + index * 4)?;
        }
        Ok(Self {
            sequence: be32(bytes, 8)?,
            length: be32(bytes, 12)?,
            inode_count: be32(bytes, 16)?,
            inode_btree_root: be32(bytes, 20)?,
            inode_btree_level: be32(bytes, 24)?,
            free_inode_count: be32(bytes, 28)?,
            free_inode_btree_root: crc_enabled.then(|| be32(bytes, 328)).transpose()?,
            free_inode_btree_level: crc_enabled.then(|| be32(bytes, 332)).transpose()?,
            uuid: XfsUuid(uuid),
            unlinked,
        })
    }

    fn serialize(self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
        let mut bytes = vec![0; sb.sector_size as usize];
        if bytes.len() < if sb.is_v5() { 336 } else { 312 } {
            return Err(XfsError::CorruptMetadata);
        }
        put_be32(&mut bytes, 0, XFS_AGI_MAGIC)?;
        put_be32(&mut bytes, 8, self.sequence)?;
        put_be32(&mut bytes, 12, self.length)?;
        put_be32(&mut bytes, 16, self.inode_count)?;
        put_be32(&mut bytes, 20, self.inode_btree_root)?;
        put_be32(&mut bytes, 24, self.inode_btree_level)?;
        put_be32(&mut bytes, 28, self.free_inode_count)?;
        for (index, bucket) in self.unlinked.iter().enumerate() {
            put_be32(&mut bytes, 40 + index * 4, *bucket)?;
        }
        bytes[296..312].copy_from_slice(&self.uuid.0);
        if sb.is_v5() {
            put_be64(&mut bytes, 320, lsn)?;
            put_be32(&mut bytes, 328, self.free_inode_btree_root.unwrap_or(0))?;
            put_be32(&mut bytes, 332, self.free_inode_btree_level.unwrap_or(0))?;
            rewrite_crc32c(&mut bytes, 312)?;
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsAllocationGroup {
    pub number: u32,
    pub free_space: XfsAgf,
    pub inode: XfsAgi,
}

/// Allocation-group btree families used to establish allocator ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsAgBtreeKind {
    ByBlock,
    ByLength,
    Inode,
    FreeInode,
}

/// The two v5 allocation-group trees whose roots live in AGF.  They are kept
/// separate from [`XfsAgBtreeKind`]: their variable-width records make it too
/// easy to accidentally serialize an rmap/refcount node as an 8-byte free
/// space record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsAgSpecialBtreeKind {
    Rmap,
    Refcount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsRmapRecord {
    pub start_block: u32,
    pub block_count: u32,
    pub owner: u64,
    /// The on-disk offset includes the documented high-bit fork/state flags.
    /// It is intentionally not normalized: replay must preserve the exact
    /// ownership key written by the logged operation.
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsRefcountRecord {
    pub start_block: u32,
    pub block_count: u32,
    pub refcount: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsAgSpecialBtreeRecords {
    Rmap(Vec<XfsRmapRecord>),
    Refcount(Vec<XfsRefcountRecord>),
    RmapKeys(Vec<XfsRmapRecord>),
    RefcountKeys(Vec<u32>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAgSpecialBtreeNode {
    pub kind: XfsAgSpecialBtreeKind,
    pub ag: u32,
    pub block: u32,
    pub level: u16,
    pub left_sibling: u32,
    pub right_sibling: u32,
    pub records: XfsAgSpecialBtreeRecords,
    pub children: Vec<u32>,
}

impl XfsAgSpecialBtreeNode {
    fn header_and_capacity(
        kind: XfsAgSpecialBtreeKind,
        level: u16,
        sb: XfsSuperblock,
    ) -> XfsResult<(usize, usize, usize)> {
        if !sb.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        let (leaf_bytes, key_bytes) = match kind {
            XfsAgSpecialBtreeKind::Rmap => (24usize, 24usize),
            XfsAgSpecialBtreeKind::Refcount => (12usize, 4usize),
        };
        let item_bytes = if level == 0 {
            leaf_bytes
        } else {
            key_bytes
                .checked_add(4)
                .ok_or(XfsError::AddressOutOfRange)?
        };
        let capacity = (sb.block_size as usize)
            .checked_sub(56)
            .ok_or(XfsError::InvalidSuperblock)?
            / item_bytes;
        if capacity < 2 {
            return Err(XfsError::InvalidSuperblock);
        }
        Ok((
            56,
            if level == 0 { leaf_bytes } else { key_bytes },
            capacity,
        ))
    }

    fn expected_magic(kind: XfsAgSpecialBtreeKind) -> u32 {
        match kind {
            XfsAgSpecialBtreeKind::Rmap => XFS_RMAP_CRC_MAGIC,
            XfsAgSpecialBtreeKind::Refcount => XFS_REFCOUNT_CRC_MAGIC,
        }
    }

    fn parse(
        kind: XfsAgSpecialBtreeKind,
        ag: u32,
        block: u32,
        bytes: &[u8],
        sb: XfsSuperblock,
    ) -> XfsResult<Self> {
        if !sb.is_v5() || be32(bytes, 0)? != Self::expected_magic(kind) {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(bytes, 4)?;
        let count = be16(bytes, 6)? as usize;
        let (header, width, capacity) = Self::header_and_capacity(kind, level, sb)?;
        if bytes.len() != sb.block_size as usize || count == 0 || count > capacity {
            return Err(XfsError::CorruptMetadata);
        }
        verify_crc32c(bytes, 52)?;
        let mut uuid = [0; 16];
        uuid.copy_from_slice(slice(bytes, 32, 16)?);
        let fs_block = (ag as u64)
            .checked_mul(sb.ag_blocks as u64)
            .and_then(|base| base.checked_add(block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        if XfsUuid(uuid) != sb.meta_uuid || be32(bytes, 48)? != ag || be64(bytes, 16)? != fs_block {
            return Err(XfsError::CorruptMetadata);
        }
        let left = be32(bytes, 8)?;
        let right = be32(bytes, 12)?;
        let valid_record = |start: u32, blocks: u32| -> XfsResult<()> {
            if blocks == 0
                || start < 4
                || start
                    .checked_add(blocks)
                    .is_none_or(|end| end > sb.ag_blocks)
            {
                Err(XfsError::CorruptMetadata)
            } else {
                Ok(())
            }
        };
        if level == 0 {
            match kind {
                XfsAgSpecialBtreeKind::Rmap => {
                    let mut records = Vec::new();
                    records
                        .try_reserve_exact(count)
                        .map_err(|_| XfsError::NoMemory)?;
                    let mut previous = None;
                    for index in 0..count {
                        let at = header + index * width;
                        let record = XfsRmapRecord {
                            start_block: be32(bytes, at)?,
                            block_count: be32(bytes, at + 4)?,
                            owner: be64(bytes, at + 8)?,
                            offset: be64(bytes, at + 16)?,
                        };
                        valid_record(record.start_block, record.block_count)?;
                        let key = (record.start_block, record.owner, record.offset);
                        if previous.is_some_and(|last| key < last) {
                            return Err(XfsError::CorruptMetadata);
                        }
                        previous = Some(key);
                        records.push(record);
                    }
                    Ok(Self {
                        kind,
                        ag,
                        block,
                        level,
                        left_sibling: left,
                        right_sibling: right,
                        records: XfsAgSpecialBtreeRecords::Rmap(records),
                        children: Vec::new(),
                    })
                }
                XfsAgSpecialBtreeKind::Refcount => {
                    let mut records = Vec::new();
                    records
                        .try_reserve_exact(count)
                        .map_err(|_| XfsError::NoMemory)?;
                    let mut previous = None;
                    for index in 0..count {
                        let at = header + index * width;
                        let record = XfsRefcountRecord {
                            start_block: be32(bytes, at)?,
                            block_count: be32(bytes, at + 4)?,
                            refcount: be32(bytes, at + 8)?,
                        };
                        valid_record(record.start_block, record.block_count)?;
                        if record.refcount < 2
                            || previous.is_some_and(|last| record.start_block < last)
                        {
                            return Err(XfsError::CorruptMetadata);
                        }
                        previous = Some(record.start_block);
                        records.push(record);
                    }
                    Ok(Self {
                        kind,
                        ag,
                        block,
                        level,
                        left_sibling: left,
                        right_sibling: right,
                        records: XfsAgSpecialBtreeRecords::Refcount(records),
                        children: Vec::new(),
                    })
                }
            }
        } else {
            let pointer_base = header
                .checked_add(
                    capacity
                        .checked_mul(width)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut children = Vec::new();
            children
                .try_reserve_exact(count)
                .map_err(|_| XfsError::NoMemory)?;
            match kind {
                XfsAgSpecialBtreeKind::Rmap => {
                    let mut keys = Vec::new();
                    keys.try_reserve_exact(count)
                        .map_err(|_| XfsError::NoMemory)?;
                    let mut previous = None;
                    for index in 0..count {
                        let at = header + index * width;
                        let key = XfsRmapRecord {
                            start_block: be32(bytes, at)?,
                            block_count: 0,
                            owner: be64(bytes, at + 8)?,
                            offset: be64(bytes, at + 16)?,
                        };
                        let sort = (key.start_block, key.owner, key.offset);
                        if previous.is_some_and(|last| sort < last) {
                            return Err(XfsError::CorruptMetadata);
                        }
                        previous = Some(sort);
                        let child = be32(bytes, pointer_base + index * 4)?;
                        if child < 4 || child >= sb.ag_blocks {
                            return Err(XfsError::CorruptMetadata);
                        }
                        keys.push(key);
                        children.push(child);
                    }
                    Ok(Self {
                        kind,
                        ag,
                        block,
                        level,
                        left_sibling: left,
                        right_sibling: right,
                        records: XfsAgSpecialBtreeRecords::RmapKeys(keys),
                        children,
                    })
                }
                XfsAgSpecialBtreeKind::Refcount => {
                    let mut keys = Vec::new();
                    keys.try_reserve_exact(count)
                        .map_err(|_| XfsError::NoMemory)?;
                    let mut previous = None;
                    for index in 0..count {
                        let key = be32(bytes, header + index * width)?;
                        if previous.is_some_and(|last| key < last) {
                            return Err(XfsError::CorruptMetadata);
                        }
                        previous = Some(key);
                        let child = be32(bytes, pointer_base + index * 4)?;
                        if child < 4 || child >= sb.ag_blocks {
                            return Err(XfsError::CorruptMetadata);
                        }
                        keys.push(key);
                        children.push(child);
                    }
                    Ok(Self {
                        kind,
                        ag,
                        block,
                        level,
                        left_sibling: left,
                        right_sibling: right,
                        records: XfsAgSpecialBtreeRecords::RefcountKeys(keys),
                        children,
                    })
                }
            }
        }
    }

    fn record_len(&self) -> usize {
        match &self.records {
            XfsAgSpecialBtreeRecords::Rmap(items) => items.len(),
            XfsAgSpecialBtreeRecords::Refcount(items) => items.len(),
            XfsAgSpecialBtreeRecords::RmapKeys(items) => items.len(),
            XfsAgSpecialBtreeRecords::RefcountKeys(items) => items.len(),
        }
    }

    fn serialize(&self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
        let (header, width, capacity) = Self::header_and_capacity(self.kind, self.level, sb)?;
        if self.ag >= sb.ag_count
            || self.block < 4
            || self.block >= sb.ag_blocks
            || self.record_len() == 0
            || self.record_len() > capacity
            || (self.level == 0 && !self.children.is_empty())
            || (self.level != 0 && self.children.len() != self.record_len())
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut bytes = vec![0; sb.block_size as usize];
        put_be32(&mut bytes, 0, Self::expected_magic(self.kind))?;
        put_be16(&mut bytes, 4, self.level)?;
        put_be16(
            &mut bytes,
            6,
            u16::try_from(self.record_len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be32(&mut bytes, 8, self.left_sibling)?;
        put_be32(&mut bytes, 12, self.right_sibling)?;
        let fs_block = (self.ag as u64)
            .checked_mul(sb.ag_blocks as u64)
            .and_then(|base| base.checked_add(self.block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        put_be64(&mut bytes, 16, fs_block)?;
        put_be64(&mut bytes, 24, lsn)?;
        bytes[32..48].copy_from_slice(&sb.meta_uuid.0);
        put_be32(&mut bytes, 48, self.ag)?;
        match &self.records {
            XfsAgSpecialBtreeRecords::Rmap(records) => {
                for (index, record) in records.iter().enumerate() {
                    let at = header + index * width;
                    put_be32(&mut bytes, at, record.start_block)?;
                    put_be32(&mut bytes, at + 4, record.block_count)?;
                    put_be64(&mut bytes, at + 8, record.owner)?;
                    put_be64(&mut bytes, at + 16, record.offset)?;
                }
            }
            XfsAgSpecialBtreeRecords::Refcount(records) => {
                for (index, record) in records.iter().enumerate() {
                    let at = header + index * width;
                    put_be32(&mut bytes, at, record.start_block)?;
                    put_be32(&mut bytes, at + 4, record.block_count)?;
                    put_be32(&mut bytes, at + 8, record.refcount)?;
                }
            }
            XfsAgSpecialBtreeRecords::RmapKeys(keys) => {
                for (index, key) in keys.iter().enumerate() {
                    let at = header + index * width;
                    put_be32(&mut bytes, at, key.start_block)?;
                    put_be32(&mut bytes, at + 4, 0)?;
                    put_be64(&mut bytes, at + 8, key.owner)?;
                    put_be64(&mut bytes, at + 16, key.offset)?;
                }
            }
            XfsAgSpecialBtreeRecords::RefcountKeys(keys) => {
                for (index, key) in keys.iter().enumerate() {
                    put_be32(&mut bytes, header + index * width, *key)?;
                }
            }
        }
        if self.level != 0 {
            let pointer_base = header + capacity * width;
            for (index, child) in self.children.iter().enumerate() {
                if *child < 4 || *child >= sb.ag_blocks {
                    return Err(XfsError::CorruptMetadata);
                }
                put_be32(&mut bytes, pointer_base + index * 4, *child)?;
            }
        }
        rewrite_crc32c(&mut bytes, 52)?;
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsAgFreeRecord {
    pub start_block: u32,
    pub block_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsAgInodeRecord {
    pub start_inode: u32,
    pub free_count: u32,
    pub free_mask: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsAgBtreeRecords {
    Free(Vec<XfsAgFreeRecord>),
    Inode(Vec<XfsAgInodeRecord>),
    Keys(Vec<(u32, u32)>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAgBtreeNode {
    pub kind: XfsAgBtreeKind,
    pub ag: u32,
    pub block: u32,
    pub level: u16,
    pub left_sibling: u32,
    pub right_sibling: u32,
    pub records: XfsAgBtreeRecords,
    pub children: Vec<u32>,
}

impl XfsAgBtreeNode {
    fn parse(
        kind: XfsAgBtreeKind,
        ag: u32,
        block: u32,
        bytes: &[u8],
        sb: XfsSuperblock,
    ) -> XfsResult<Self> {
        let magic = be32(bytes, 0)?;
        let expected = match (kind, sb.is_v5()) {
            (XfsAgBtreeKind::ByBlock, false) => 0x4142_5442,
            (XfsAgBtreeKind::ByBlock, true) => 0x4142_3342,
            (XfsAgBtreeKind::ByLength, false) => 0x4142_5443,
            (XfsAgBtreeKind::ByLength, true) => 0x4142_3343,
            (XfsAgBtreeKind::Inode, false) => 0x4941_4254,
            (XfsAgBtreeKind::Inode, true) => 0x4941_4233,
            (XfsAgBtreeKind::FreeInode, false) => 0x4649_4254,
            (XfsAgBtreeKind::FreeInode, true) => 0x4649_4233,
        };
        if magic != expected {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(bytes, 4)?;
        let count = be16(bytes, 6)? as usize;
        let (header, left, right) = if sb.is_v5() {
            if bytes.len() < 56 {
                return Err(XfsError::CorruptMetadata);
            }
            verify_crc32c(bytes, 52)?;
            let mut uuid = [0; 16];
            uuid.copy_from_slice(slice(bytes, 32, 16)?);
            if XfsUuid(uuid) != sb.meta_uuid || be32(bytes, 48)? != ag {
                return Err(XfsError::CorruptMetadata);
            }
            (56usize, be32(bytes, 8)?, be32(bytes, 12)?)
        } else {
            (16usize, be32(bytes, 8)?, be32(bytes, 12)?)
        };
        let record_bytes = if matches!(kind, XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode) {
            16
        } else {
            8
        };
        let max = if level == 0 {
            (bytes.len() - header) / record_bytes
        } else {
            (bytes.len() - header) / (record_bytes + 4)
        };
        if count == 0 || count > max {
            return Err(XfsError::CorruptMetadata);
        }
        let mut children = Vec::new();
        if level != 0 {
            // XFS keeps the key and pointer arrays at their *maximum*
            // capacity.  Deriving the pointer base from `count` accepts a
            // compacted, non-XFS encoding and makes a later split rewrite
            // point at attacker-controlled key bytes.
            let pointer_base = header
                .checked_add(
                    max.checked_mul(record_bytes)
                        .ok_or(XfsError::CorruptMetadata)?,
                )
                .ok_or(XfsError::CorruptMetadata)?;
            let mut keys = Vec::new();
            keys.try_reserve_exact(count)
                .map_err(|_| XfsError::NoMemory)?;
            children
                .try_reserve_exact(count)
                .map_err(|_| XfsError::NoMemory)?;
            let mut prior = None;
            for index in 0..count {
                let key = (
                    be32(bytes, header + index * record_bytes)?,
                    be32(bytes, header + index * record_bytes + 4)?,
                );
                if prior.is_some_and(|last| key < last) {
                    return Err(XfsError::CorruptMetadata);
                }
                prior = Some(key);
                let child = be32(bytes, pointer_base + index * 4)?;
                if child == 0 || child >= sb.ag_blocks {
                    return Err(XfsError::CorruptMetadata);
                }
                keys.push(key);
                children.push(child);
            }
            return Ok(Self {
                kind,
                ag,
                block,
                level,
                left_sibling: left,
                right_sibling: right,
                records: XfsAgBtreeRecords::Keys(keys),
                children,
            });
        }
        match kind {
            XfsAgBtreeKind::ByBlock | XfsAgBtreeKind::ByLength => {
                let mut records = Vec::new();
                records
                    .try_reserve_exact(count)
                    .map_err(|_| XfsError::NoMemory)?;
                let mut prior = None;
                for index in 0..count {
                    let (first, second) = (
                        be32(bytes, header + index * 8)?,
                        be32(bytes, header + index * 8 + 4)?,
                    );
                    let record = match kind {
                        XfsAgBtreeKind::ByBlock => XfsAgFreeRecord {
                            start_block: first,
                            block_count: second,
                        },
                        // cntbt records are ordered as (blockcount,startblock)
                        // on media, while the public record stays canonical.
                        XfsAgBtreeKind::ByLength => XfsAgFreeRecord {
                            start_block: second,
                            block_count: first,
                        },
                        XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode => unreachable!(),
                    };
                    let end = record
                        .start_block
                        .checked_add(record.block_count)
                        .ok_or(XfsError::CorruptMetadata)?;
                    let sort = match kind {
                        XfsAgBtreeKind::ByBlock => (record.start_block, record.block_count),
                        XfsAgBtreeKind::ByLength => (record.block_count, record.start_block),
                        XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode => unreachable!(),
                    };
                    if record.block_count == 0
                        || end > sb.ag_blocks
                        || prior.is_some_and(|last| sort < last)
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    prior = Some(sort);
                    records.push(record);
                }
                Ok(Self {
                    kind,
                    ag,
                    block,
                    level,
                    left_sibling: left,
                    right_sibling: right,
                    records: XfsAgBtreeRecords::Free(records),
                    children,
                })
            }
            XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode => {
                let mut records = Vec::new();
                records
                    .try_reserve_exact(count)
                    .map_err(|_| XfsError::NoMemory)?;
                let mut prior = None;
                for index in 0..count {
                    let offset = header + index * 16;
                    let start_inode = be32(bytes, offset)?;
                    let free_count = be32(bytes, offset + 4)?;
                    let free_mask = be64(bytes, offset + 8)?;
                    if free_count > 64
                        || free_mask.count_ones() != free_count
                        || prior.is_some_and(|last| start_inode < last)
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    prior = Some(start_inode);
                    records.push(XfsAgInodeRecord {
                        start_inode,
                        free_count,
                        free_mask,
                    });
                }
                Ok(Self {
                    kind,
                    ag,
                    block,
                    level,
                    left_sibling: left,
                    right_sibling: right,
                    records: XfsAgBtreeRecords::Inode(records),
                    children,
                })
            }
        }
    }

    /// Serializes a complete AG allocation/inode btree block.  The v5 header
    /// binds CRC, owner, UUID, physical block and LSN in one image; callers
    /// stage that image in the same transaction as AGF/AGFL.
    pub fn serialize(&self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
        if self.ag >= sb.ag_count || self.block >= sb.ag_blocks || self.records_len() == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let header = if sb.is_v5() { 56usize } else { 16usize };
        let record_bytes = if matches!(self.kind, XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode)
        {
            16usize
        } else {
            8usize
        };
        let capacity = if self.level == 0 {
            (sb.block_size as usize - header) / record_bytes
        } else {
            (sb.block_size as usize - header) / (record_bytes + 4)
        };
        if self.records_len() > capacity
            || (self.level == 0 && !self.children.is_empty())
            || (self.level != 0 && self.children.len() != self.records_len())
        {
            return Err(XfsError::CorruptMetadata);
        }
        let magic = match (self.kind, sb.is_v5()) {
            (XfsAgBtreeKind::ByBlock, false) => 0x4142_5442,
            (XfsAgBtreeKind::ByBlock, true) => 0x4142_3342,
            (XfsAgBtreeKind::ByLength, false) => 0x4142_5443,
            (XfsAgBtreeKind::ByLength, true) => 0x4142_3343,
            (XfsAgBtreeKind::Inode, false) => 0x4941_4254,
            (XfsAgBtreeKind::Inode, true) => 0x4941_4233,
            (XfsAgBtreeKind::FreeInode, false) => 0x4649_4254,
            (XfsAgBtreeKind::FreeInode, true) => 0x4649_4233,
        };
        let mut bytes = vec![0; sb.block_size as usize];
        put_be32(&mut bytes, 0, magic)?;
        put_be16(&mut bytes, 4, self.level)?;
        put_be16(
            &mut bytes,
            6,
            u16::try_from(self.records_len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be32(&mut bytes, 8, self.left_sibling)?;
        put_be32(&mut bytes, 12, self.right_sibling)?;
        if sb.is_v5() {
            let fs_block = (self.ag as u64)
                .checked_mul(sb.ag_blocks as u64)
                .and_then(|base| base.checked_add(self.block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            put_be64(&mut bytes, 16, fs_block)?;
            put_be64(&mut bytes, 24, lsn)?;
            bytes[32..48].copy_from_slice(&sb.meta_uuid.0);
            put_be32(&mut bytes, 48, self.ag)?;
        }
        match &self.records {
            XfsAgBtreeRecords::Free(records) => {
                for (index, record) in records.iter().enumerate() {
                    let offset = header + index * 8;
                    let (first, second) = if self.kind == XfsAgBtreeKind::ByLength {
                        (record.block_count, record.start_block)
                    } else {
                        (record.start_block, record.block_count)
                    };
                    put_be32(&mut bytes, offset, first)?;
                    put_be32(&mut bytes, offset + 4, second)?;
                }
            }
            XfsAgBtreeRecords::Inode(records) => {
                for (index, record) in records.iter().enumerate() {
                    let offset = header + index * 16;
                    put_be32(&mut bytes, offset, record.start_inode)?;
                    put_be32(&mut bytes, offset + 4, record.free_count)?;
                    put_be64(&mut bytes, offset + 8, record.free_mask)?;
                }
            }
            XfsAgBtreeRecords::Keys(keys) => {
                for (index, key) in keys.iter().enumerate() {
                    let offset = header + index * record_bytes;
                    put_be32(&mut bytes, offset, key.0)?;
                    put_be32(&mut bytes, offset + 4, key.1)?;
                }
            }
        }
        if self.level != 0 {
            let pointer_base = header + capacity * record_bytes;
            for (index, child) in self.children.iter().enumerate() {
                if *child == 0 || *child >= sb.ag_blocks {
                    return Err(XfsError::CorruptMetadata);
                }
                put_be32(&mut bytes, pointer_base + index * 4, *child)?;
            }
        }
        if sb.is_v5() {
            rewrite_crc32c(&mut bytes, 52)?;
        }
        Ok(bytes)
    }

    fn records_len(&self) -> usize {
        match &self.records {
            XfsAgBtreeRecords::Free(records) => records.len(),
            XfsAgBtreeRecords::Inode(records) => records.len(),
            XfsAgBtreeRecords::Keys(keys) => keys.len(),
        }
    }
}

/// Verified ownership view of one allocation group.  Free space is admitted
/// only if both independent allocation btrees describe the same nonoverlap
/// extents; inode allocation is similarly derived from checked inobt leaves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAgOwnershipSnapshot {
    pub ag: u32,
    /// The exact AGF/AGI pair whose roots were walked below.
    pub group: XfsAllocationGroup,
    /// The exact AGFL ring belonging to `group`; allocator planners must not
    /// reopen it after choosing btree homes from this snapshot.
    pub freelist: XfsAgFreelist,
    pub free_extents: Vec<XfsAgFreeRecord>,
    pub inode_records: Vec<XfsAgInodeRecord>,
    pub bno_nodes: Vec<XfsAgBtreeNode>,
    pub cnt_nodes: Vec<XfsAgBtreeNode>,
    pub ino_nodes: Vec<XfsAgBtreeNode>,
    pub fino_nodes: Vec<XfsAgBtreeNode>,
}

/// Checked allocation-group freelist header.  The freelist entries themselves
/// are allocator-private, but its identity and checksum still bind the AG
/// metadata snapshot used by a future transaction allocator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct XfsAgfl {
    sequence: u32,
    uuid: XfsUuid,
}

/// Decoded AG freelist ring.  Entries are AG-relative block numbers owned by
/// allocation-btree maintenance, never general free extents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAgFreelist {
    pub ag: u32,
    pub entries: Vec<u32>,
    pub first: u32,
    pub last: u32,
}

impl XfsAgfl {
    fn parse(bytes: &[u8], crc_enabled: bool) -> XfsResult<Self> {
        if bytes.len() < 36 || be32(bytes, 0)? != XFS_AGFL_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        if crc_enabled {
            verify_crc32c(bytes, 32)?;
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(slice(bytes, 8, 16)?);
        Ok(Self {
            sequence: be32(bytes, 4)?,
            uuid: XfsUuid(uuid),
        })
    }

    fn serialize(
        self,
        sb: XfsSuperblock,
        lsn: u64,
        entries: &[u32],
        first: u32,
        last: u32,
    ) -> XfsResult<Vec<u8>> {
        let mut bytes = vec![0; sb.sector_size as usize];
        let capacity = bytes
            .len()
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if entries.len() > capacity
            || (entries.is_empty() && (first != 0 || last != 0))
            || (!entries.is_empty() && (first as usize >= capacity || last as usize >= capacity))
        {
            return Err(XfsError::CorruptMetadata);
        }
        put_be32(&mut bytes, 0, XFS_AGFL_MAGIC)?;
        put_be32(&mut bytes, 4, self.sequence)?;
        bytes[8..24].copy_from_slice(&self.uuid.0);
        for (index, entry) in entries.iter().enumerate() {
            let slot = (first as usize + index) % capacity;
            put_be32(&mut bytes, 36 + slot * 4, *entry)?;
        }
        if sb.is_v5() {
            put_be64(&mut bytes, 24, lsn)?;
            rewrite_crc32c(&mut bytes, 32)?;
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsForkFormat {
    Device,
    Local,
    Extents,
    Btree,
    Uuid,
}

impl TryFrom<u8> for XfsForkFormat {
    type Error = XfsError;

    fn try_from(value: u8) -> XfsResult<Self> {
        match value {
            0 => Ok(Self::Device),
            1 => Ok(Self::Local),
            2 => Ok(Self::Extents),
            3 => Ok(Self::Btree),
            4 => Ok(Self::Uuid),
            _ => Err(XfsError::CorruptMetadata),
        }
    }
}

/// The stable inode-core fields used by pathname, quota, file-attribute and
/// export-handle code.  Fork payloads are decoded separately so an attacker
/// cannot make a variable length fork alter fixed-core validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsInode {
    pub number: u64,
    pub version: u8,
    pub mode: u16,
    /// `di_metatype`, valid only for v3 metadir inodes.  Older cores use the
    /// same bytes as `di_onlink` and must never be reinterpreted as a type.
    pub metafile_type: Option<u16>,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub project_id: u32,
    pub size: u64,
    pub blocks: u64,
    /// `di_extsize`, stored in filesystem blocks (not UAPI bytes).
    pub extent_size_hint: u32,
    pub data_extents: u64,
    pub attr_extents: u64,
    pub generation: u32,
    pub flags: u16,
    pub flags2: u64,
    /// `di_cowextsize`, stored in filesystem blocks (not UAPI bytes).
    pub cow_extent_size_hint: u32,
    /// Native XFS timestamps.  Keeping the signed seconds here avoids a
    /// lossy conversion at the VFS boundary for pre-epoch files.
    pub atime_seconds: i64,
    pub atime_nanoseconds: u32,
    pub mtime_seconds: i64,
    pub mtime_nanoseconds: u32,
    pub ctime_seconds: i64,
    pub ctime_nanoseconds: u32,
    pub crtime_seconds: i64,
    pub crtime_nanoseconds: u32,
    pub data_format: XfsForkFormat,
    pub attr_format: XfsForkFormat,
    pub fork_offset: u8,
    core_bytes: u16,
}

/// Native inode attributes; no private xattr shadows either the project id or
/// the copy-on-write extent hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsFileAttr {
    pub flags: u16,
    pub flags2: u64,
    pub project_id: u32,
    pub extent_size_hint: u32,
    pub cow_extent_size_hint: u32,
}

/// Selected on-disk quota roots.  A missing root remains absent instead of
/// being emulated by an in-memory accounting table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsQuotaRoots {
    pub flags: u16,
    pub user: Option<u64>,
    pub group: Option<u64>,
    pub project: Option<u64>,
}

/// Native v5 dquot accounting state.  This is deliberately a view of the
/// quota inodes, not a second in-memory quota database: every admission and
/// every update starts with an authenticated on-disk dquot image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsQuotaState {
    pub roots: XfsQuotaRoots,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsDquot {
    pub id: u32,
    pub quota_type: u8,
    quota_type_flags: u8,
    pub block_hard: u64,
    pub block_soft: u64,
    pub inode_hard: u64,
    pub inode_soft: u64,
    pub realtime_hard: u64,
    pub realtime_soft: u64,
    pub blocks: u64,
    pub inodes: u64,
    pub realtime_blocks: u64,
    inode_timer: u32,
    block_timer: u32,
    realtime_timer: u32,
    inode_warnings: u16,
    block_warnings: u16,
    realtime_warnings: u16,
}

/// A preflighted native dquot update.  The complete 136-byte replacement is
/// kept with its physical home binding so quota images can be logged with the
/// inode/AG/directory images that caused the accounting transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDquotDelta {
    pub id: u32,
    pub quota_type: u8,
    pub basic_block: u64,
    pub block_count: u32,
    pub byte_offset: u32,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct XfsDquotAdmission {
    blocks: u64,
    inodes: u64,
    block_timer: u32,
    inode_timer: u32,
    block_warnings: u16,
    inode_warnings: u16,
}

/// A retained inode object for the future VFS adapter.  It carries its volume
/// explicitly, so object operations cannot cross mounts accidentally.
#[derive(Clone)]
pub struct XfsNode {
    volume: Arc<XfsVolume>,
    inode: XfsInode,
}

impl XfsNode {
    pub const fn inode(&self) -> XfsInode {
        self.inode
    }

    pub const fn file_attr(&self) -> XfsFileAttr {
        self.inode.file_attr()
    }

    pub fn export_handle(&self) -> XfsExportHandle {
        XfsExportHandle {
            inode: self.inode.number,
            generation: self.inode.generation,
        }
    }

    pub fn read_at(&self, offset: u64, output: &mut [u8]) -> XfsResult<usize> {
        self.volume.read_inode_at(self.inode.number, offset, output)
    }

    pub fn shortform_entries(&self) -> XfsResult<Vec<XfsDirectoryEntry>> {
        self.volume.shortform_directory(self.inode.number)
    }

    pub fn directory_data_block(&self, index: u64) -> XfsResult<XfsDirectoryDataBlock> {
        self.volume.directory_data_block(self.inode.number, index)
    }

    pub fn directory_leaf_block(&self, index: u64) -> XfsResult<XfsDirectoryLeafBlock> {
        self.volume.directory_leaf_block(self.inode.number, index)
    }

    pub fn lookup_shortform(&self, name: &[u8]) -> XfsResult<XfsNode> {
        if name.is_empty() || name == b"." {
            return Ok(self.clone());
        }
        if name == b".." {
            return Err(XfsError::UnsupportedFeature);
        }
        let entry = self
            .shortform_entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.volume.node(entry.inode)
    }

    /// Resolves any supported native XFS directory format.  This supersedes
    /// the historical shortform-only helper while retaining it for callers
    /// that explicitly require the compact representation.
    pub fn lookup(&self, name: &[u8]) -> XfsResult<XfsNode> {
        if name == b"." || name.is_empty() {
            return Ok(self.clone());
        }
        let inode = self.volume.lookup_directory(self.inode.number, name)?;
        Ok(XfsNode {
            volume: self.volume.clone(),
            inode,
        })
    }
}

impl XfsInode {
    const DIFLAG2_BIGTIME: u64 = 1 << 3;
    const DIFLAG2_NREXT64: u64 = 1 << 4;
    const DIFLAG2_METADATA: u64 = 1 << 5;

    fn is_metadata_inode(&self) -> bool {
        self.version >= 3 && self.flags2 & Self::DIFLAG2_METADATA != 0
    }

    fn parse(
        number: u64,
        bytes: &[u8],
        filesystem_uuid: Option<XfsUuid>,
        metadata_uuid: Option<XfsUuid>,
        metadir: bool,
    ) -> XfsResult<Self> {
        if bytes.len() < 100 || be16(bytes, 0)? != XFS_DINODE_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        let version = byte(bytes, 4)?;
        if !(1..=3).contains(&version) {
            return Err(XfsError::UnsupportedFeature);
        }
        let core_bytes = if version >= 3 { 176 } else { 100 };
        if bytes.len() < core_bytes as usize {
            return Err(XfsError::CorruptMetadata);
        }
        if version >= 3 {
            verify_crc32c(bytes, 100)?;
            if be64(bytes, 152)? != number {
                return Err(XfsError::CorruptMetadata);
            }
        }
        let fork_offset = byte(bytes, 82)?;
        let data_format = XfsForkFormat::try_from(byte(bytes, 5)?)?;
        let attr_format = XfsForkFormat::try_from(byte(bytes, 83)?)?;
        let project_id = be16(bytes, 20)? as u32 | ((be16(bytes, 22)? as u32) << 16);
        let flags2 = if version >= 3 { be64(bytes, 120)? } else { 0 };
        if version >= 3 {
            let mut uuid = [0; 16];
            uuid.copy_from_slice(slice(bytes, 160, 16)?);
            let expected = if flags2 & Self::DIFLAG2_METADATA != 0 {
                metadata_uuid.or(filesystem_uuid)
            } else {
                filesystem_uuid
            };
            if expected.is_some_and(|expected| XfsUuid(uuid) != expected) {
                return Err(XfsError::CorruptMetadata);
            }
        }
        let bigtime = flags2 & Self::DIFLAG2_BIGTIME != 0;
        let (atime_seconds, atime_nanoseconds) = parse_inode_timestamp(bytes, 32, bigtime)?;
        let (mtime_seconds, mtime_nanoseconds) = parse_inode_timestamp(bytes, 40, bigtime)?;
        let (ctime_seconds, ctime_nanoseconds) = parse_inode_timestamp(bytes, 48, bigtime)?;
        let (crtime_seconds, crtime_nanoseconds) = if version >= 3 {
            parse_inode_timestamp(bytes, 144, bigtime)?
        } else {
            (0, 0)
        };
        let nrext64 = flags2 & Self::DIFLAG2_NREXT64 != 0;
        Ok(Self {
            number,
            version,
            mode: be16(bytes, 2)?,
            metafile_type: (version >= 3 && metadir)
                .then(|| be16(bytes, 6))
                .transpose()?,
            uid: be32(bytes, 8)?,
            gid: be32(bytes, 12)?,
            nlink: be32(bytes, 16)?,
            project_id,
            size: be64(bytes, 56)?,
            blocks: be64(bytes, 64)?,
            extent_size_hint: be32(bytes, 72)?,
            data_extents: if nrext64 {
                be64(bytes, 24)?
            } else {
                be32(bytes, 76)? as u64
            },
            attr_extents: if nrext64 {
                be32(bytes, 76)? as u64
            } else {
                be16(bytes, 80)? as u64
            },
            fork_offset,
            attr_format,
            data_format,
            flags: be16(bytes, 90)?,
            // v3's post-v2 core is CRC (100), changecount (104), LSN (112),
            // flags2 (120), cowextsize (128), pad2 (132..143), crtime (144).
            // Do not slide these by four bytes: doing so turns the tail of an
            // LSN into user-visible flags and silently loses COW extent size.
            flags2,
            cow_extent_size_hint: if version >= 3 { be32(bytes, 128)? } else { 0 },
            atime_seconds,
            atime_nanoseconds,
            mtime_seconds,
            mtime_nanoseconds,
            ctime_seconds,
            ctime_nanoseconds,
            crtime_seconds,
            crtime_nanoseconds,
            generation: be32(bytes, 92)?,
            core_bytes,
        })
    }

    fn data_fork<'a>(&self, bytes: &'a [u8]) -> XfsResult<&'a [u8]> {
        let begin = self.core_bytes as usize;
        let end = if self.fork_offset == 0 {
            bytes.len()
        } else {
            (self.fork_offset as usize)
                .checked_mul(8)
                .filter(|end| *end >= begin && *end <= bytes.len())
                .ok_or(XfsError::CorruptMetadata)?
        };
        slice(bytes, begin, end - begin)
    }

    fn attr_fork<'a>(&self, bytes: &'a [u8]) -> XfsResult<&'a [u8]> {
        if self.fork_offset == 0 {
            return Ok(&[]);
        }
        let begin = self.fork_offset as usize * 8;
        slice(bytes, begin, bytes.len().saturating_sub(begin))
    }

    pub const fn file_attr(&self) -> XfsFileAttr {
        XfsFileAttr {
            flags: self.flags,
            flags2: self.flags2,
            project_id: self.project_id,
            extent_size_hint: self.extent_size_hint,
            cow_extent_size_hint: self.cow_extent_size_hint,
        }
    }
}

/// One non-overlapping mapping from file offset blocks to a physical data
/// filesystem block range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsExtent {
    pub unwritten: bool,
    pub file_block: u64,
    pub start_block: u64,
    pub block_count: u32,
}

/// The inode-resident root of an XFS BMBT.  Internal child block numbers are
/// decoded separately from leaf extent records because their pointer array is
/// placed after *capacity* keys, not after the currently used key count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBmbtRoot {
    pub level: u16,
    pub records: u16,
    pub leaf_extents: Vec<XfsExtent>,
    pub children: Vec<u64>,
}

/// One verified on-disk BMBT block.  A caller follows `children` only after
/// comparing this block's level to its parent, which makes malformed cycles
/// and level skips visible to the traversal coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBmbtNode {
    pub filesystem_block: u64,
    pub level: u16,
    pub records: u16,
    pub left_sibling: u64,
    pub right_sibling: u64,
    pub leaf_extents: Vec<XfsExtent>,
    pub children: Vec<u64>,
}

/// An opaque, generation-bound export token.  The caller supplies the
/// filesystem identity separately, so a handle can never be rebound across
/// XFS volumes merely because inode numbers coincide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsExportHandle {
    pub inode: u64,
    pub generation: u32,
}

impl XfsExportHandle {
    pub const ENCODED_LEN: usize = 12;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0; Self::ENCODED_LEN];
        bytes[..8].copy_from_slice(&self.inode.to_be_bytes());
        bytes[8..].copy_from_slice(&self.generation.to_be_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> XfsResult<Self> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(Self {
            inode: be64(bytes, 0)?,
            generation: be32(bytes, 8)?,
        })
    }
}

/// One raw-name entry from an XFS shortform directory.  Names are bytes,
/// matching XFS and avoiding a lossy UTF-8 conversion in the VFS boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryEntry {
    pub name: Vec<u8>,
    pub inode: u64,
    pub file_type: Option<u8>,
}

/// Decoded entries from one dir2/dir3 data block.  The address is a logical
/// directory-byte offset used by leaf hash entries, not a host pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryDataEntry {
    pub address: u32,
    pub name: Vec<u8>,
    pub inode: u64,
    pub file_type: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryDataBlock {
    pub entries: Vec<XfsDirectoryDataEntry>,
    pub dir3: bool,
}

/// One hash/address edge in a dir2/dir3 leaf block.  Address resolution is
/// intentionally separate so stale entries can never be mistaken for names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsDirectoryLeafEntry {
    pub hash: u32,
    pub address: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryLeafBlock {
    pub forward: u32,
    pub backward: u32,
    pub stale: u16,
    pub entries: Vec<XfsDirectoryLeafEntry>,
    pub dir3: bool,
    pub single_leaf: bool,
}

/// Native dir2/dir3 free-space information.  The three slots are sorted by
/// descending length (and then ascending offset), exactly as the on-disk
/// `bestfree` cache requires.  It is a cache only: directory mutation always
/// derives it from the data records it writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsDirectoryBestFree {
    pub offset: u16,
    pub length: u16,
}

/// Fully materialized one-block dir2/dir3 namespace image.  This is used by
/// the writable path for both a block directory and the data half of a
/// leaf/node directory; no host-string representation occurs in between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryBlockImage {
    /// The native `..` target materialized in external directory data.
    pub parent: u64,
    pub entries: Vec<XfsDirectoryEntry>,
    pub bestfree: [XfsDirectoryBestFree; 3],
    pub leaf: Vec<XfsDirectoryLeafEntry>,
    pub dir3: bool,
}

/// One native attribute leaf record.  `value_block` is zero for a local value
/// and otherwise names the first remote-value block; `value_length` remains
/// explicit so remote data is never read to a terminator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAttributeLeafEntry {
    pub hash: u32,
    pub flags: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    pub value_block: u32,
    pub value_length: u32,
}

/// Decoded attr2/attr3 leaf or node block.  Node records retain their hash
/// separators and child addresses; leaf records retain exact byte names and
/// values.  Keeping these variants separate prevents an address from being
/// treated as an attribute value during tree descent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsAttributeBlock {
    Leaf {
        forward: u32,
        backward: u32,
        entries: Vec<XfsAttributeLeafEntry>,
        dir3: bool,
    },
    Node {
        forward: u32,
        backward: u32,
        level: u16,
        entries: Vec<XfsDirectoryLeafEntry>,
        dir3: bool,
    },
}

impl XfsDirectoryLeafBlock {
    pub fn parse(
        bytes: &[u8],
        expected_uuid: XfsUuid,
        expected_owner: u64,
        expected_basic_block: u64,
    ) -> XfsResult<Self> {
        let magic = be16(bytes, 8)?;
        let (header, dir3, single_leaf) = match magic {
            XFS_DIR2_LEAF1_MAGIC => (16usize, false, true),
            XFS_DIR2_LEAFN_MAGIC => (16usize, false, false),
            XFS_DIR3_LEAF1_MAGIC => (64usize, true, true),
            XFS_DIR3_LEAFN_MAGIC => (64usize, true, false),
            _ => return Err(XfsError::CorruptMetadata),
        };
        if bytes.len() < header {
            return Err(XfsError::CorruptMetadata);
        }
        if dir3 {
            verify_crc32c(bytes, 12)?;
            let mut uuid = [0; 16];
            uuid.copy_from_slice(slice(bytes, 32, 16)?);
            if XfsUuid(uuid) != expected_uuid
                || be64(bytes, 16)? != expected_basic_block
                || be64(bytes, 48)? != expected_owner
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        let count_offset = if dir3 { 56 } else { 12 };
        let count = be16(bytes, count_offset)? as usize;
        let stale = be16(bytes, count_offset + 2)?;
        if stale as usize > count {
            return Err(XfsError::CorruptMetadata);
        }
        let entry_bytes = count.checked_mul(8).ok_or(XfsError::CorruptMetadata)?;
        if header
            .checked_add(entry_bytes)
            .ok_or(XfsError::CorruptMetadata)?
            > bytes.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        let mut last_hash = 0;
        for index in 0..count {
            let offset = header + index * 8;
            let hash = be32(bytes, offset)?;
            let address = be32(bytes, offset + 4)?;
            if index != 0 && hash < last_hash {
                return Err(XfsError::CorruptMetadata);
            }
            last_hash = hash;
            entries.push(XfsDirectoryLeafEntry { hash, address });
        }
        Ok(Self {
            forward: be32(bytes, 0)?,
            backward: be32(bytes, 4)?,
            stale,
            entries,
            dir3,
            single_leaf,
        })
    }
}

impl XfsDirectoryDataBlock {
    fn parse(
        bytes: &[u8],
        expected_uuid: XfsUuid,
        expected_owner: u64,
        expected_basic_block: u64,
        ftype: bool,
    ) -> XfsResult<Self> {
        let magic = be32(bytes, 0)?;
        let (header, dir3, data_end) = match magic {
            XFS_DIR2_DATA_MAGIC => (16usize, false, bytes.len()),
            XFS_DIR3_DATA_MAGIC => {
                if bytes.len() < 64 {
                    return Err(XfsError::CorruptMetadata);
                }
                verify_crc32c(bytes, 4)?;
                let mut uuid = [0; 16];
                uuid.copy_from_slice(slice(bytes, 24, 16)?);
                if XfsUuid(uuid) != expected_uuid
                    || be64(bytes, 8)? != expected_basic_block
                    || be64(bytes, 40)? != expected_owner
                {
                    return Err(XfsError::CorruptMetadata);
                }
                (64usize, true, bytes.len())
            }
            XFS_DIR2_BLOCK_MAGIC | XFS_DIR3_BLOCK_MAGIC => {
                let dir3 = magic == XFS_DIR3_BLOCK_MAGIC;
                let header = if dir3 { 64 } else { 16 };
                if bytes.len() < header + 4 {
                    return Err(XfsError::CorruptMetadata);
                }
                if dir3 {
                    verify_crc32c(bytes, 4)?;
                    let mut uuid = [0; 16];
                    uuid.copy_from_slice(slice(bytes, 24, 16)?);
                    if XfsUuid(uuid) != expected_uuid
                        || be64(bytes, 8)? != expected_basic_block
                        || be64(bytes, 40)? != expected_owner
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                }
                let count = be32(bytes, bytes.len() - 4)? as usize;
                let leaf_bytes = count.checked_mul(8).ok_or(XfsError::CorruptMetadata)?;
                let end = bytes
                    .len()
                    .checked_sub(4 + leaf_bytes)
                    .ok_or(XfsError::CorruptMetadata)?;
                if end < header {
                    return Err(XfsError::CorruptMetadata);
                }
                (header, dir3, end)
            }
            _ => return Err(XfsError::CorruptMetadata),
        };
        let mut cursor = header;
        let mut entries = Vec::new();
        while cursor < data_end {
            if data_end - cursor < 2 {
                return Err(XfsError::CorruptMetadata);
            }
            if be16(bytes, cursor)? == XFS_DIR_DATA_FREE_TAG {
                let length = be16(bytes, cursor + 2)? as usize;
                if length < 6 || length % 8 != 0 || cursor + length > data_end {
                    return Err(XfsError::CorruptMetadata);
                }
                if be16(bytes, cursor + length - 2)? as usize != cursor {
                    return Err(XfsError::CorruptMetadata);
                }
                cursor += length;
                continue;
            }
            if data_end - cursor < 11 {
                return Err(XfsError::CorruptMetadata);
            }
            let inode = be64(bytes, cursor)?;
            let name_len = byte(bytes, cursor + 8)? as usize;
            let base = 8usize
                .checked_add(1 + name_len)
                .and_then(|value| value.checked_add(usize::from(ftype)))
                .and_then(|value| value.checked_add(2))
                .ok_or(XfsError::CorruptMetadata)?;
            let length = align8(base).ok_or(XfsError::CorruptMetadata)?;
            if inode == 0
                || cursor + length > data_end
                || be16(bytes, cursor + length - 2)? as usize != cursor
            {
                return Err(XfsError::CorruptMetadata);
            }
            let name = slice(bytes, cursor + 9, name_len)?.to_vec();
            if name.is_empty() || name.iter().any(|byte| *byte == 0 || *byte == b'/') {
                return Err(XfsError::CorruptMetadata);
            }
            let file_type = ftype.then(|| bytes[cursor + 9 + name_len]);
            entries.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            entries.push(XfsDirectoryDataEntry {
                address: cursor as u32,
                name,
                inode,
                file_type,
            });
            cursor += length;
        }
        Ok(Self { entries, dir3 })
    }
}

/// XFS's byte-at-a-time directory/attribute hash.  It deliberately accepts
/// raw names; callers perform only the pathname policy checks appropriate to
/// their namespace before reaching this format primitive.
pub fn xfs_name_hash(name: &[u8]) -> u32 {
    name.iter()
        .fold(0u32, |hash, byte| hash.rotate_left(7) ^ u32::from(*byte))
}

fn directory_type_for_inode(mode: u16) -> u8 {
    ((mode >> 12) & 0xf) as u8
}

impl XfsDirectoryBlockImage {
    /// Builds a native dir2/dir3 block layout from the complete live
    /// namespace.  Sorting the leaf array by (hash,address) makes identical
    /// names deterministic and gives lookup the exact collision range.
    pub fn serialize(
        &self,
        uuid: XfsUuid,
        owner: u64,
        basic_block: u64,
        ftype: bool,
        block_size: usize,
    ) -> XfsResult<Vec<u8>> {
        let header = if self.dir3 { 64 } else { 16 };
        if block_size < header + 4 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut out = vec![0; block_size];
        put_be32(
            &mut out,
            0,
            if self.dir3 {
                XFS_DIR3_BLOCK_MAGIC
            } else {
                XFS_DIR2_BLOCK_MAGIC
            },
        )?;
        if self.dir3 {
            put_be64(&mut out, 8, basic_block)?;
            out[24..40].copy_from_slice(&uuid.0);
            put_be64(&mut out, 40, owner)?;
        }
        let mut namespace = Vec::new();
        namespace
            .try_reserve_exact(
                self.entries
                    .len()
                    .checked_add(2)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        if self.entries.iter().enumerate().any(|(index, entry)| {
            entry.name == b"."
                || entry.name == b".."
                || entry.name.is_empty()
                || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
                || self.entries[..index]
                    .iter()
                    .any(|prior| prior.name == entry.name)
        }) {
            return Err(XfsError::AddressOutOfRange);
        }
        namespace.push(XfsDirectoryEntry {
            name: b".".to_vec(),
            inode: owner,
            file_type: Some(2),
        });
        namespace.push(XfsDirectoryEntry {
            name: b"..".to_vec(),
            inode: self.parent,
            file_type: Some(2),
        });
        namespace.extend(self.entries.iter().cloned());
        if self.parent == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut cursor = header;
        let mut leaf = Vec::new();
        leaf.try_reserve_exact(namespace.len())
            .map_err(|_| XfsError::NoMemory)?;
        for entry in &namespace {
            if entry.inode == 0
                || entry.name.is_empty()
                || entry.name.len() > u8::MAX as usize
                || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
            {
                return Err(XfsError::AddressOutOfRange);
            }
            if ftype && entry.file_type.is_none() {
                return Err(XfsError::CorruptMetadata);
            }
            let length = align8(
                11usize
                    .checked_add(entry.name.len())
                    .and_then(|length| length.checked_add(usize::from(ftype)))
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .ok_or(XfsError::AddressOutOfRange)?;
            if cursor
                .checked_add(length)
                .ok_or(XfsError::AddressOutOfRange)?
                > block_size
            {
                return Err(XfsError::AddressOutOfRange);
            }
            put_be64(&mut out, cursor, entry.inode)?;
            out[cursor + 8] = entry.name.len() as u8;
            out[cursor + 9..cursor + 9 + entry.name.len()].copy_from_slice(&entry.name);
            if ftype {
                out[cursor + 9 + entry.name.len()] = entry.file_type.unwrap();
            }
            put_be16(
                &mut out,
                cursor + length - 2,
                u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            leaf.push(XfsDirectoryLeafEntry {
                hash: xfs_name_hash(&entry.name),
                address: u32::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
            });
            cursor += length;
        }
        leaf.sort_unstable_by_key(|entry| (entry.hash, entry.address));
        let leaf_bytes = leaf
            .len()
            .checked_mul(8)
            .ok_or(XfsError::AddressOutOfRange)?;
        let tail = block_size
            .checked_sub(4 + leaf_bytes)
            .ok_or(XfsError::AddressOutOfRange)?;
        if cursor > tail {
            return Err(XfsError::AddressOutOfRange);
        }
        if cursor < tail {
            let free_length = tail - cursor;
            if free_length < 6 || free_length % 8 != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            put_be16(&mut out, cursor, XFS_DIR_DATA_FREE_TAG)?;
            put_be16(
                &mut out,
                cursor + 2,
                u16::try_from(free_length).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            put_be16(
                &mut out,
                tail - 2,
                u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            // bestfree's first entry is the freshly-derived largest region.
            // Dir3 places the cache after its CRC/owner header; never write
            // it over the CRC field used by recovery and mount verification.
            let bestfree = if self.dir3 { 48 } else { 4 };
            put_be16(
                &mut out,
                bestfree,
                u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            put_be16(
                &mut out,
                bestfree + 2,
                u16::try_from(free_length).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        for (index, entry) in leaf.iter().enumerate() {
            let offset = tail + index * 8;
            put_be32(&mut out, offset, entry.hash)?;
            put_be32(&mut out, offset + 4, entry.address)?;
        }
        put_be32(
            &mut out,
            block_size - 4,
            u32::try_from(leaf.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        if self.dir3 {
            rewrite_crc32c(&mut out, 4)?;
        }
        Ok(out)
    }
}

/// Build one external dir2/dir3 data block.  Unlike a block directory this
/// has no embedded hash array: all names are indexed by the leaf space.
fn serialize_directory_data_block(
    uuid: XfsUuid,
    owner: u64,
    basic: u64,
    entries: &[(XfsDirectoryEntry, bool)],
    ftype: bool,
    dir3: bool,
    block_size: usize,
) -> XfsResult<(
    Vec<u8>,
    Vec<XfsDirectoryLeafEntry>,
    [XfsDirectoryBestFree; 3],
)> {
    let header = if dir3 { 64usize } else { 16usize };
    if block_size < header + 8 {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut out = vec![0; block_size];
    put_be32(
        &mut out,
        0,
        if dir3 {
            XFS_DIR3_DATA_MAGIC
        } else {
            XFS_DIR2_DATA_MAGIC
        },
    )?;
    if dir3 {
        put_be64(&mut out, 8, basic)?;
        out[24..40].copy_from_slice(&uuid.0);
        put_be64(&mut out, 40, owner)?;
    }
    let mut cursor = header;
    let mut leaf = Vec::new();
    for (entry, _) in entries {
        if entry.inode == 0
            || entry.name.is_empty()
            || entry.name.len() > u8::MAX as usize
            || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
            || (ftype && entry.file_type.is_none())
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let length = align8(
            11usize
                .checked_add(entry.name.len())
                .and_then(|value| value.checked_add(usize::from(ftype)))
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .ok_or(XfsError::AddressOutOfRange)?;
        if cursor
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?
            > block_size
        {
            return Err(XfsError::AddressOutOfRange);
        }
        put_be64(&mut out, cursor, entry.inode)?;
        out[cursor + 8] = entry.name.len() as u8;
        out[cursor + 9..cursor + 9 + entry.name.len()].copy_from_slice(&entry.name);
        if ftype {
            out[cursor + 9 + entry.name.len()] =
                entry.file_type.ok_or(XfsError::CorruptMetadata)?;
        }
        put_be16(
            &mut out,
            cursor + length - 2,
            u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        leaf.push(XfsDirectoryLeafEntry {
            hash: xfs_name_hash(&entry.name),
            address: u32::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
        });
        cursor += length;
    }
    let free = block_size - cursor;
    let bestfree = if free >= 8 {
        put_be16(&mut out, cursor, XFS_DIR_DATA_FREE_TAG)?;
        put_be16(
            &mut out,
            cursor + 2,
            u16::try_from(free).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(
            &mut out,
            block_size - 2,
            u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        [
            XfsDirectoryBestFree {
                offset: u16::try_from(cursor).map_err(|_| XfsError::AddressOutOfRange)?,
                length: u16::try_from(free).map_err(|_| XfsError::AddressOutOfRange)?,
            },
            XfsDirectoryBestFree {
                offset: 0,
                length: 0,
            },
            XfsDirectoryBestFree {
                offset: 0,
                length: 0,
            },
        ]
    } else {
        [XfsDirectoryBestFree {
            offset: 0,
            length: 0,
        }; 3]
    };
    if dir3 {
        rewrite_crc32c(&mut out, 4)?;
    }
    Ok((out, leaf, bestfree))
}

fn serialize_directory_leaf(
    entries: &[XfsDirectoryLeafEntry],
    forward: u32,
    backward: u32,
    single: bool,
    uuid: XfsUuid,
    owner: u64,
    basic: u64,
    dir3: bool,
    block_size: usize,
) -> XfsResult<Vec<u8>> {
    let header = if dir3 { 64usize } else { 16usize };
    if entries.len()
        > (block_size
            .checked_sub(header)
            .ok_or(XfsError::AddressOutOfRange)?
            / 8)
    {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut sorted = entries.to_vec();
    sorted.sort_unstable_by_key(|entry| (entry.hash, entry.address));
    let mut out = vec![0; block_size];
    put_be32(&mut out, 0, forward)?;
    put_be32(&mut out, 4, backward)?;
    put_be16(
        &mut out,
        8,
        match (dir3, single) {
            (false, true) => XFS_DIR2_LEAF1_MAGIC,
            (false, false) => XFS_DIR2_LEAFN_MAGIC,
            (true, true) => XFS_DIR3_LEAF1_MAGIC,
            (true, false) => XFS_DIR3_LEAFN_MAGIC,
        },
    )?;
    if dir3 {
        put_be64(&mut out, 16, basic)?;
        out[32..48].copy_from_slice(&uuid.0);
        put_be64(&mut out, 48, owner)?;
        put_be16(
            &mut out,
            56,
            u16::try_from(sorted.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(&mut out, 58, 0)?;
    } else {
        put_be16(
            &mut out,
            12,
            u16::try_from(sorted.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(&mut out, 14, 0)?;
    }
    for (index, entry) in sorted.iter().enumerate() {
        let at = header + index * 8;
        put_be32(&mut out, at, entry.hash)?;
        put_be32(&mut out, at + 4, entry.address)?;
    }
    if dir3 {
        rewrite_crc32c(&mut out, 12)?;
    }
    Ok(out)
}

fn serialize_directory_node(
    entries: &[XfsDirectoryLeafEntry],
    level: u16,
    forward: u32,
    backward: u32,
    uuid: XfsUuid,
    owner: u64,
    basic: u64,
    dir3: bool,
    block_size: usize,
) -> XfsResult<Vec<u8>> {
    let header = if dir3 { 64usize } else { 16usize };
    if level == 0
        || entries.is_empty()
        || entries.len()
            > (block_size
                .checked_sub(header)
                .ok_or(XfsError::AddressOutOfRange)?
                / 8)
    {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut out = vec![0; block_size];
    put_be32(&mut out, 0, forward)?;
    put_be32(&mut out, 4, backward)?;
    put_be16(
        &mut out,
        8,
        if dir3 {
            XFS_DA3_NODE_MAGIC
        } else {
            XFS_DA_NODE_MAGIC
        },
    )?;
    if dir3 {
        put_be64(&mut out, 16, basic)?;
        out[32..48].copy_from_slice(&uuid.0);
        put_be64(&mut out, 48, owner)?;
        put_be16(
            &mut out,
            56,
            u16::try_from(entries.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(&mut out, 58, level)?;
    } else {
        put_be16(
            &mut out,
            12,
            u16::try_from(entries.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(&mut out, 14, level)?;
    }
    for (index, entry) in entries.iter().enumerate() {
        let at = header + index * 8;
        put_be32(&mut out, at, entry.hash)?;
        put_be32(&mut out, at + 4, entry.address)?;
    }
    if dir3 {
        rewrite_crc32c(&mut out, 12)?;
    }
    Ok(out)
}

fn serialize_directory_free(
    best: &[[XfsDirectoryBestFree; 3]],
    uuid: XfsUuid,
    owner: u64,
    basic: u64,
    dir3: bool,
    block_size: usize,
) -> XfsResult<Vec<u8>> {
    let header = if dir3 { 64usize } else { 16usize };
    if best.len()
        > (block_size
            .checked_sub(header)
            .ok_or(XfsError::AddressOutOfRange)?
            / 2)
    {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut out = vec![0; block_size];
    put_be32(
        &mut out,
        0,
        if dir3 {
            XFS_DIR3_FREE_MAGIC
        } else {
            XFS_DIR2_FREE_MAGIC
        },
    )?;
    if dir3 {
        put_be64(&mut out, 8, basic)?;
        out[16..32].copy_from_slice(&uuid.0);
        put_be64(&mut out, 32, owner)?;
        put_be64(&mut out, 40, 0)?;
        put_be32(&mut out, 48, 0)?;
        put_be16(
            &mut out,
            52,
            u16::try_from(best.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(
            &mut out,
            54,
            u16::try_from(best.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
    } else {
        put_be32(&mut out, 4, 0)?;
        put_be16(
            &mut out,
            8,
            u16::try_from(best.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(
            &mut out,
            10,
            u16::try_from(best.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
    }
    for (index, slots) in best.iter().enumerate() {
        put_be16(&mut out, header + index * 2, slots[0].length)?;
    }
    if dir3 {
        rewrite_crc32c(&mut out, 4)?;
    }
    Ok(out)
}

impl XfsAttributeBlock {
    /// Partitions an already storage-classified attribute set into DA leaves.
    /// Callers assign remote value block numbers after logical leaf numbers
    /// are reserved; capacity is tested using nonzero placeholders only.
    fn partition_leaves(
        entries: &[XfsAttributeLeafEntry],
        uuid: XfsUuid,
        owner: u64,
        dir3: bool,
        block_size: usize,
    ) -> XfsResult<Vec<Vec<XfsAttributeLeafEntry>>> {
        let mut ordered = entries.to_vec();
        ordered.sort_unstable_by_key(|entry| (entry.hash, entry.name.clone()));
        let mut leaves = Vec::<Vec<XfsAttributeLeafEntry>>::new();
        for entry in ordered {
            let mut candidate = leaves.last().cloned().unwrap_or_default();
            candidate.push(entry.clone());
            if Self::serialize_leaf(&candidate, 0, 0, uuid, owner, 0, dir3, block_size).is_ok() {
                if let Some(last) = leaves.last_mut() {
                    last.push(entry);
                } else {
                    leaves.push(vec![entry]);
                }
            } else {
                // A single record that cannot fit even after its value is
                // remote is malformed (normally an overlong name), not a
                // reason to manufacture an empty DA leaf.
                if Self::serialize_leaf(
                    core::slice::from_ref(&entry),
                    0,
                    0,
                    uuid,
                    owner,
                    0,
                    dir3,
                    block_size,
                )
                .is_err()
                {
                    return Err(XfsError::AddressOutOfRange);
                }
                leaves.push(vec![entry]);
            }
        }
        if leaves.is_empty() {
            leaves.push(Vec::new());
        }
        Ok(leaves)
    }
    /// Decodes attr2/attr3 leaf records.  Attribute node blocks use the same
    /// DA block-info layout but are intentionally represented separately.
    pub fn parse(
        bytes: &[u8],
        uuid: XfsUuid,
        owner: u64,
        expected_basic_block: u64,
    ) -> XfsResult<Self> {
        let magic = be16(bytes, 8)?;
        let (dir3, leaf) = match magic {
            XFS_ATTR_LEAF_MAGIC => (false, true),
            XFS_ATTR3_LEAF_MAGIC => (true, true),
            XFS_DA_NODE_MAGIC => (false, false),
            XFS_DA3_NODE_MAGIC => (true, false),
            _ => return Err(XfsError::CorruptMetadata),
        };
        let header = if leaf {
            if dir3 { 80usize } else { 32usize }
        } else if dir3 {
            64usize
        } else {
            16usize
        };
        if bytes.len() < header {
            return Err(XfsError::CorruptMetadata);
        }
        if dir3 {
            verify_crc32c(bytes, 12)?;
            let mut found = [0; 16];
            found.copy_from_slice(slice(bytes, 32, 16)?);
            if XfsUuid(found) != uuid
                || be64(bytes, 16)? != expected_basic_block
                || be64(bytes, 48)? != owner
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        let forward = be32(bytes, 0)?;
        let backward = be32(bytes, 4)?;
        if !leaf {
            let count = be16(bytes, if dir3 { 56 } else { 12 })? as usize;
            let level = be16(bytes, if dir3 { 58 } else { 14 })?;
            if count == 0 || level == 0 || level > 5 {
                return Err(XfsError::CorruptMetadata);
            }
            let start = if dir3 { 64 } else { 16 };
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(count)
                .map_err(|_| XfsError::NoMemory)?;
            let mut last = 0;
            for index in 0..count {
                let offset = start + index * 8;
                let hash = be32(bytes, offset)?;
                let address = be32(bytes, offset + 4)?;
                if address == 0 || index != 0 && hash < last {
                    return Err(XfsError::CorruptMetadata);
                }
                last = hash;
                entries.push(XfsDirectoryLeafEntry { hash, address });
            }
            return Ok(Self::Node {
                forward,
                backward,
                level,
                entries,
                dir3,
            });
        }
        let count = be16(bytes, if dir3 { 56 } else { 12 })? as usize;
        let table = header;
        if table
            .checked_add(count.checked_mul(8).ok_or(XfsError::CorruptMetadata)?)
            .ok_or(XfsError::CorruptMetadata)?
            > bytes.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        let mut last = 0;
        for index in 0..count {
            let offset = table + index * 8;
            let hash = be32(bytes, offset)?;
            let name_index = be16(bytes, offset + 4)? as usize;
            let flags = byte(bytes, offset + 6)?;
            if index != 0 && hash < last {
                return Err(XfsError::CorruptMetadata);
            }
            last = hash;
            let (value_block, value_length, name, value) = if flags & XFS_ATTR_LOCAL != 0 {
                let value_length = be16(bytes, name_index)? as u32;
                let name_len = byte(bytes, name_index + 2)? as usize;
                let name_start = name_index.checked_add(3).ok_or(XfsError::CorruptMetadata)?;
                let name = slice(bytes, name_start, name_len)?.to_vec();
                let value = slice(bytes, name_start + name_len, value_length as usize)?.to_vec();
                (0, value_length, name, value)
            } else {
                let value_block = be32(bytes, name_index)?;
                let value_length = be32(bytes, name_index + 4)?;
                let name_len = byte(bytes, name_index + 8)? as usize;
                let name = slice(bytes, name_index + 9, name_len)?.to_vec();
                if value_block == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                (value_block, value_length, name, Vec::new())
            };
            if name.is_empty() || name.iter().any(|byte| *byte == 0) || xfs_name_hash(&name) != hash
            {
                return Err(XfsError::CorruptMetadata);
            }
            entries.push(XfsAttributeLeafEntry {
                hash,
                flags,
                name,
                value,
                value_block,
                value_length,
            });
        }
        Ok(Self::Leaf {
            forward,
            backward,
            entries,
            dir3,
        })
    }

    /// Serializes a checked local-value attribute leaf.  Remote values are
    /// not folded into a fake inline image: callers must reserve and stage
    /// their remote value blocks first, then pass their explicit addresses.
    pub fn serialize_leaf(
        entries: &[XfsAttributeLeafEntry],
        forward: u32,
        backward: u32,
        uuid: XfsUuid,
        owner: u64,
        basic_block: u64,
        dir3: bool,
        block_size: usize,
    ) -> XfsResult<Vec<u8>> {
        let header = if dir3 { 80usize } else { 32usize };
        if block_size < header {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut ordered = entries.to_vec();
        ordered.sort_unstable_by_key(|entry| (entry.hash, entry.name.clone()));
        if ordered.iter().any(|entry| {
            entry.name.is_empty()
                || entry.name.iter().any(|byte| *byte == 0)
                || entry.hash != xfs_name_hash(&entry.name)
                || entry.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE)
                    == (XFS_ATTR_ROOT | XFS_ATTR_SECURE)
        }) {
            return Err(XfsError::AddressOutOfRange);
        }
        let table_bytes = ordered
            .len()
            .checked_mul(8)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut payload = block_size;
        let mut locations = Vec::new();
        locations
            .try_reserve_exact(ordered.len())
            .map_err(|_| XfsError::NoMemory)?;
        for entry in ordered.iter().rev() {
            let raw_body = if entry.value_block == 0 {
                3usize
                    .checked_add(entry.name.len())
                    .and_then(|value| value.checked_add(entry.value.len()))
                    .ok_or(XfsError::AddressOutOfRange)?
            } else {
                if entry.value.len() != 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                9usize
                    .checked_add(entry.name.len())
                    .ok_or(XfsError::AddressOutOfRange)?
            };
            let body = raw_body
                .checked_add(3)
                .map(|length| length & !3)
                .ok_or(XfsError::AddressOutOfRange)?;
            payload = payload
                .checked_sub(body)
                .ok_or(XfsError::AddressOutOfRange)?;
            locations.push((payload, body));
        }
        if payload
            < header
                .checked_add(table_bytes)
                .ok_or(XfsError::AddressOutOfRange)?
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut out = vec![0; block_size];
        put_be32(&mut out, 0, forward)?;
        put_be32(&mut out, 4, backward)?;
        put_be16(
            &mut out,
            8,
            if dir3 {
                XFS_ATTR3_LEAF_MAGIC
            } else {
                XFS_ATTR_LEAF_MAGIC
            },
        )?;
        if dir3 {
            put_be64(&mut out, 16, basic_block)?;
            out[32..48].copy_from_slice(&uuid.0);
            put_be64(&mut out, 48, owner)?;
        }
        let count_offset = if dir3 { 56 } else { 12 };
        let map_offset = if dir3 { 64 } else { 20 };
        put_be16(
            &mut out,
            count_offset,
            u16::try_from(ordered.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(
            &mut out,
            count_offset + 2,
            u16::try_from(block_size - payload).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        put_be16(
            &mut out,
            count_offset + 4,
            u16::try_from(payload).unwrap_or(0),
        )?;
        let free_start = header
            .checked_add(table_bytes)
            .ok_or(XfsError::AddressOutOfRange)?;
        if payload > free_start {
            put_be16(
                &mut out,
                map_offset,
                u16::try_from(free_start).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            put_be16(
                &mut out,
                map_offset + 2,
                u16::try_from(payload - free_start).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        for (index, entry) in ordered.iter().enumerate() {
            let (offset, _) = locations[ordered.len() - 1 - index];
            let table = header + index * 8;
            put_be32(&mut out, table, entry.hash)?;
            put_be16(
                &mut out,
                table + 4,
                u16::try_from(offset).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            out[table + 6] = entry.flags;
            if entry.value_block == 0 {
                if entry.value_length != entry.value.len() as u32 {
                    return Err(XfsError::CorruptMetadata);
                }
                put_be16(
                    &mut out,
                    offset,
                    u16::try_from(entry.value_length).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
                out[offset + 2] =
                    u8::try_from(entry.name.len()).map_err(|_| XfsError::AddressOutOfRange)?;
                out[offset + 3..offset + 3 + entry.name.len()].copy_from_slice(&entry.name);
                out[offset + 3 + entry.name.len()
                    ..offset + 3 + entry.name.len() + entry.value.len()]
                    .copy_from_slice(&entry.value);
            } else {
                put_be32(&mut out, offset, entry.value_block)?;
                put_be32(&mut out, offset + 4, entry.value_length)?;
                out[offset + 8] =
                    u8::try_from(entry.name.len()).map_err(|_| XfsError::AddressOutOfRange)?;
                out[offset + 9..offset + 9 + entry.name.len()].copy_from_slice(&entry.name);
            }
        }
        if dir3 {
            rewrite_crc32c(&mut out, 12)?;
        }
        Ok(out)
    }

    /// Serializes one checked attr2/attr3 node block.  Leaf split/merge code
    /// supplies child logical addresses; this codec deliberately does not
    /// infer them from allocation order.
    pub fn serialize_node(
        entries: &[XfsDirectoryLeafEntry],
        forward: u32,
        backward: u32,
        level: u16,
        uuid: XfsUuid,
        owner: u64,
        basic_block: u64,
        dir3: bool,
        block_size: usize,
    ) -> XfsResult<Vec<u8>> {
        if entries.is_empty() || level == 0 || level > 5 {
            return Err(XfsError::AddressOutOfRange);
        }
        let header = if dir3 { 64usize } else { 16usize };
        let bytes = entries
            .len()
            .checked_mul(8)
            .ok_or(XfsError::AddressOutOfRange)?;
        if header
            .checked_add(bytes)
            .ok_or(XfsError::AddressOutOfRange)?
            > block_size
            || entries.iter().enumerate().any(|(index, entry)| {
                entry.address == 0 || index != 0 && entries[index - 1].hash > entry.hash
            })
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut out = vec![0; block_size];
        put_be32(&mut out, 0, forward)?;
        put_be32(&mut out, 4, backward)?;
        put_be16(
            &mut out,
            8,
            if dir3 {
                XFS_DA3_NODE_MAGIC
            } else {
                XFS_DA_NODE_MAGIC
            },
        )?;
        if dir3 {
            put_be64(&mut out, 16, basic_block)?;
            out[32..48].copy_from_slice(&uuid.0);
            put_be64(&mut out, 48, owner)?;
            put_be16(
                &mut out,
                56,
                u16::try_from(entries.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            put_be16(&mut out, 58, level)?;
        } else {
            put_be16(
                &mut out,
                12,
                u16::try_from(entries.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            put_be16(&mut out, 14, level)?;
        }
        for (index, entry) in entries.iter().enumerate() {
            let offset = header + index * 8;
            put_be32(&mut out, offset, entry.hash)?;
            put_be32(&mut out, offset + 4, entry.address)?;
        }
        if dir3 {
            rewrite_crc32c(&mut out, 12)?;
        }
        Ok(out)
    }
}

/// A shortform extended attribute.  The flags are retained verbatim because
/// namespace/security interpretation belongs to the VFS/LSM layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsShortformXattr {
    pub flags: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsShortformXattrMode {
    Upsert,
    Create,
    Replace,
    CreateAndReplace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsShortformXattrOutcome {
    Applied,
    Exists,
    Missing,
}

/// Namespace mutation requested by the VFS after it has completed permission,
/// sticky-bit, and object-identity checks.  Every variant is applied against
/// one locked directory snapshot and becomes durable in one XFS transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsDirectoryMutation {
    Insert(XfsDirectoryEntry),
    Remove(Vec<u8>),
    Replace {
        name: Vec<u8>,
        entry: XfsDirectoryEntry,
    },
}

/// One complete post-operation directory namespace image.  Multi-directory
/// callers submit every affected image before the log commit, allowing the
/// transaction composer to merge inode-block patches while preserving one
/// durable namespace transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirectoryUpdate {
    pub directory: u64,
    pub parent: u64,
    pub entries: Vec<XfsDirectoryEntry>,
}

/// Header of one committed XFS journal record.  Payload replay is purposely
/// not exposed yet: replay requires verified log-operation item decoders and
/// an atomic metadata write set, neither of which can be replaced by an
/// in-place block write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsLogRecordHeader {
    pub cycle: u32,
    pub version: u32,
    pub payload_bytes: u32,
    pub lsn: u64,
    pub tail_lsn: u64,
    pub previous_block: u32,
    pub operation_count: u32,
    /// Saved values displaced by the physical log's per-basic-block cycle
    /// stamping.  They are opaque to transaction item decoding but must round
    /// trip exactly when a ring writer reuses a record area.
    pub cycle_data: [u32; 64],
    pub format: u32,
    pub filesystem_uuid: XfsUuid,
    pub iclog_bytes: u32,
}

/// One framed log operation.  Its payload is copied from the record so a
/// recovery coordinator can retain a transaction across wrapped log I/O
/// without borrowing a DMA buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogOperation {
    pub transaction_id: u32,
    pub client_id: u8,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl XfsLogOperation {
    /// Splits only the data region of an operation at a physical record
    /// boundary.  The first fragment carries CONTINUE, interior fragments
    /// carry WAS_CONT|CONTINUE, and the final fragment carries WAS_CONT|END.
    /// START stays on the first region and COMMIT stays on the final region,
    /// preserving the transaction visibility rule during recovery.
    pub fn split_for_continuation(&self, maximum_payload: usize) -> XfsResult<Vec<Self>> {
        if maximum_payload == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        if self.payload.len() <= maximum_payload {
            let mut single = Vec::new();
            single.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            single.push(self.clone());
            return Ok(single);
        }
        let parts = self.payload.len().div_ceil(maximum_payload);
        let mut output = Vec::new();
        output
            .try_reserve_exact(parts)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..parts {
            let start = index
                .checked_mul(maximum_payload)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = start
                .checked_add(maximum_payload)
                .map(|end| end.min(self.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut flags =
                self.flags & !(XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS);
            if index == 0 {
                flags |= XLOG_CONTINUE_TRANS;
            } else if index + 1 == parts {
                flags |= XLOG_WAS_CONT_TRANS | XLOG_END_TRANS;
            } else {
                flags |= XLOG_WAS_CONT_TRANS | XLOG_CONTINUE_TRANS;
            }
            if index != 0 {
                flags &= !XLOG_START_TRANS;
            }
            if index + 1 != parts {
                flags &= !XLOG_COMMIT_TRANS;
            }
            output.push(Self {
                transaction_id: self.transaction_id,
                client_id: self.client_id,
                flags,
                payload: self.payload[start..end].to_vec(),
            });
        }
        Ok(output)
    }
}

/// Byte order of host-native log-item payloads. Physical log headers and
/// operation headers are always big-endian; only the item bodies use this.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsLogByteOrder {
    Little,
    Big,
}

/// Transaction-header proof carried at the start of every recovered item
/// sequence. `item_count` is checked before any item is replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsTransactionHeader {
    pub transaction_type: u32,
    pub item_count: u32,
}

impl XfsTransactionHeader {
    pub const CHECKPOINT: u32 = 40;

    /// Native-endian wire body for the transaction-header log region.
    pub fn encode(self, transaction_id: u32, order: XfsLogByteOrder) -> XfsResult<[u8; 16]> {
        if self.item_count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut bytes = [0; 16];
        native_put_u32(&mut bytes, 0, 0x5452_414e, order)?;
        native_put_u32(&mut bytes, 4, self.transaction_type, order)?;
        native_put_u32(&mut bytes, 8, transaction_id, order)?;
        native_put_u32(&mut bytes, 12, self.item_count, order)?;
        Ok(bytes)
    }
}

/// One 128-byte dirty-region map of a logged buffer. The actual bytes are
/// deliberately retained separately; recovery must consume exactly one data
/// region for every set bit before writing a home block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBufferReplayItem {
    pub flags: u16,
    pub block_number: u64,
    pub block_count: u16,
    pub dirty_chunks: Vec<u32>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsMetadataBufferType {
    Agf,
    Agfl,
    Agi,
    Inode,
    Dquot,
    Btree,
    Directory,
    Attribute,
    Superblock,
    Realtime,
}

impl XfsBufferReplayItem {
    /// Decodes the persistent buffer class encoded in the upper BLF flag bits.
    /// Recovery selects checksum/LSN ownership from this typed value instead
    /// of guessing from a post-patch magic number.
    pub fn metadata_type(&self) -> XfsResult<XfsMetadataBufferType> {
        match (self.flags >> 11) & 0x1f {
            5 => Ok(XfsMetadataBufferType::Agf),
            6 => Ok(XfsMetadataBufferType::Agfl),
            3 => Ok(XfsMetadataBufferType::Dquot),
            7 => Ok(XfsMetadataBufferType::Agi),
            8 => Ok(XfsMetadataBufferType::Inode),
            4 => Ok(XfsMetadataBufferType::Btree),
            10..=14 => Ok(XfsMetadataBufferType::Directory),
            15..=17 => Ok(XfsMetadataBufferType::Attribute),
            18 => Ok(XfsMetadataBufferType::Superblock),
            19 | 20 => Ok(XfsMetadataBufferType::Realtime),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    fn crc_lsn_offsets(&self) -> XfsResult<(usize, Option<usize>)> {
        match self.metadata_type()? {
            XfsMetadataBufferType::Agf => Ok((216, Some(208))),
            XfsMetadataBufferType::Agfl => Ok((32, Some(24))),
            XfsMetadataBufferType::Agi => Ok((312, Some(320))),
            XfsMetadataBufferType::Btree => Ok((64, Some(32))),
            // Dir3 data blocks keep their crc at 4/LSN at 16, whereas DA3
            // (attribute/node/leaf) blocks embed xfs_da_blkinfo first and
            // therefore keep them at 12/24.  Attribute buffer items always
            // use the latter format.
            XfsMetadataBufferType::Directory => Ok((4, Some(16))),
            XfsMetadataBufferType::Attribute => Ok((12, Some(24))),
            XfsMetadataBufferType::Superblock => Ok((224, Some(240))),
            // Legacy realtime inode contents are raw native-endian words.
            // Rtgroup media identifies itself with one of the two magic
            // values below, and is handled from the completed image.
            XfsMetadataBufferType::Realtime => return Err(XfsError::UnsupportedFeature),
            // v3 inode cores carry their CRC at byte 100 and last-update LSN
            // at byte 112.  The buffer may contain several fixed-size inodes;
            // callers rewrite every CRC, while the LSN gate uses the first
            // core and rejects mixed-LSN buffers below.
            XfsMetadataBufferType::Inode => Ok((100, Some(112))),
            XfsMetadataBufferType::Dquot => Ok((108, Some(112))),
        }
    }

    fn btree_crc_lsn_offsets(bytes: &[u8]) -> XfsResult<(usize, Option<usize>)> {
        // AG btrees and BMBTs intentionally share the BUF-item class.  Their
        // v5 headers do not share checksum/LSN locations, so dispatch from
        // the completed home image's magic rather than treating every tree as
        // a BMBT during replay.
        match be32(bytes, 0)? {
            XFS_BMAP_CRC_MAGIC => Ok((64, Some(32))),
            0x4142_3342
            | 0x4142_3343
            | 0x4941_4233
            | 0x4649_4233
            | XFS_RMAP_CRC_MAGIC
            | XFS_REFCOUNT_CRC_MAGIC => Ok((52, Some(24))),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    fn rewrite_inode_crcs(&self, bytes: &mut [u8], inode_size: usize) -> XfsResult<()> {
        if self.metadata_type()? != XfsMetadataBufferType::Inode
            || inode_size == 0
            || bytes.len() % inode_size != 0
        {
            return Err(XfsError::CorruptMetadata);
        }
        for inode in bytes.chunks_exact_mut(inode_size) {
            if be16(inode, 0)? != XFS_DINODE_MAGIC {
                continue;
            }
            if byte(inode, 4)? >= 3 {
                rewrite_crc32c(inode, 100)?;
            }
        }
        Ok(())
    }

    /// Applies this item's logged 128-byte regions to one complete home
    /// buffer.  This is deliberately a pure transformation: callers retain
    /// the old image until the complete transaction has been prepared, so a
    /// malformed item can never leave an allocation group partially replayed.
    ///
    /// XFS buffer log addresses are basic-block addresses.  The log carries
    /// only dirty chunks, rather than a whole block image, hence every bitmap
    /// chunk must fit the exact `blf_len` range before the first byte is
    /// changed.  Metadata LSN and CRC are then regenerated from the same
    /// image, making a second replay of an already durable item observable as
    /// an LSN no-op at the volume boundary.
    pub fn materialize_home_image(
        &self,
        home: &[u8],
        lsn: u64,
        inode_size: usize,
    ) -> XfsResult<Vec<u8>> {
        let expected = usize::from(self.block_count)
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        if expected == 0 || home.len() != expected || self.dirty_chunks.len() != self.chunks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut image = home.to_vec();
        for (chunk, bytes) in self.dirty_chunks.iter().zip(&self.chunks) {
            if bytes.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            let offset = usize::try_from(*chunk)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(128)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = offset.checked_add(128).ok_or(XfsError::AddressOutOfRange)?;
            if end > image.len() {
                return Err(XfsError::CorruptMetadata);
            }
            image[offset..end].copy_from_slice(bytes);
        }
        if self.metadata_type()? == XfsMetadataBufferType::Realtime {
            match be32(&image, 0)? {
                0x424d_505a | 0x5355_4d59 => {
                    let field = image.get_mut(24..32).ok_or(XfsError::CorruptMetadata)?;
                    field.copy_from_slice(&lsn.to_be_bytes());
                    rewrite_crc32c(&mut image, 4)?;
                }
                // Pre-rtgroup bitmap and summary files are raw host-endian
                // words with no checksum or LSN field.
                _ => {}
            }
            return Ok(image);
        }
        let (crc_offset, lsn_offset) = if self.metadata_type()? == XfsMetadataBufferType::Btree {
            Self::btree_crc_lsn_offsets(&image)?
        } else {
            self.crc_lsn_offsets()?
        };
        if self.metadata_type()? == XfsMetadataBufferType::Inode {
            if inode_size == 0 || image.len() % inode_size != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            for inode in image.chunks_exact_mut(inode_size) {
                let field = inode
                    .get_mut(lsn_offset.unwrap_or(0)..lsn_offset.unwrap_or(0) + 8)
                    .ok_or(XfsError::CorruptMetadata)?;
                field.copy_from_slice(&lsn.to_be_bytes());
            }
            self.rewrite_inode_crcs(&mut image, inode_size)?;
        } else {
            if let Some(offset) = lsn_offset {
                let field = image
                    .get_mut(offset..offset + 8)
                    .ok_or(XfsError::CorruptMetadata)?;
                field.copy_from_slice(&lsn.to_be_bytes());
            }
            rewrite_crc32c(&mut image, crc_offset)?;
        }
        Ok(image)
    }

    /// Returns the durable LSN encoded in a v5 home buffer when that metadata
    /// class carries one.  A missing LSN is intentionally not treated as zero:
    /// v4 and realtime records cannot be replayed safely by an LSN-only
    /// idempotence rule and remain outside the writable admission set.
    pub fn home_lsn(&self, home: &[u8]) -> XfsResult<Option<u64>> {
        if self.metadata_type()? == XfsMetadataBufferType::Realtime {
            return match be32(home, 0)? {
                0x424d_505a | 0x5355_4d59 => be64(home, 24).map(Some),
                _ => Ok(None),
            };
        }
        let (_, lsn_offset) = if self.metadata_type()? == XfsMetadataBufferType::Btree {
            Self::btree_crc_lsn_offsets(home)?
        } else {
            self.crc_lsn_offsets()?
        };
        let Some(offset) = lsn_offset else {
            return Ok(None);
        };
        if self.metadata_type()? != XfsMetadataBufferType::Inode {
            return be64(home, offset).map(Some);
        }
        // Buffer replay must not silently choose one inode's LSN when a
        // multi-inode logged buffer mixes generations.  The volume checks the
        // full image length before calling this helper and then uses the
        // first core only as its all-cores durable marker.
        be64(home, offset).map(Some)
    }

    /// Encodes the native-endian `BUF` item format plus BCHUNK payload
    /// regions. Physical operation headers and START/COMMIT/CONTINUE flags
    /// are owned by the transaction coordinator, which may split regions at a
    /// log-ring boundary without changing this item wire.
    pub fn encode_log_regions(&self, order: XfsLogByteOrder) -> XfsResult<Vec<Vec<u8>>> {
        if self.block_count == 0 || self.dirty_chunks.len() != self.chunks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut words = 0usize;
        for chunk in &self.dirty_chunks {
            let word = usize::try_from(*chunk)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_div(32)
                .and_then(|word| word.checked_add(1))
                .ok_or(XfsError::AddressOutOfRange)?;
            words = words.max(word);
        }
        if words == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let format_len = 20usize
            .checked_add(words.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut format = vec![0; format_len];
        native_put_u16(&mut format, 0, 0x123c, order)?;
        native_put_u16(
            &mut format,
            2,
            u16::try_from(
                self.chunks
                    .len()
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::AddressOutOfRange)?,
            order,
        )?;
        native_put_u16(&mut format, 4, self.flags, order)?;
        native_put_u16(&mut format, 6, self.block_count, order)?;
        native_put_u64(&mut format, 8, self.block_number, order)?;
        native_put_u32(
            &mut format,
            16,
            u32::try_from(words).map_err(|_| XfsError::AddressOutOfRange)?,
            order,
        )?;
        for (chunk, bytes) in self.dirty_chunks.iter().zip(&self.chunks) {
            if bytes.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            let word = usize::try_from(*chunk).map_err(|_| XfsError::AddressOutOfRange)? / 32;
            let bit = *chunk % 32;
            let previous = native_u32(&format, 20 + word * 4, order)?;
            native_put_u32(&mut format, 20 + word * 4, previous | (1u32 << bit), order)?;
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(
                self.chunks
                    .len()
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        result.push(format);
        for chunk in &self.chunks {
            result.push(chunk.clone());
        }
        Ok(result)
    }
}

/// Typed intent/done pairing key used by EFI/EFD, RUI/RUD, CUI/CUD, and
/// BUI/BUD. An intent that lacks its matching done item remains pending and
/// is replayed; a done without a prior intent corrupts the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsIntentKind {
    ExtentFree,
    Rmap,
    Refcount,
    Bmap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsIntentKey {
    pub kind: XfsIntentKind,
    pub id: u64,
}

/// One on-log extent, decoded from the native-endian payload of an intent or
/// done item.  It deliberately retains the operation flags: interpreting an
/// rmap/refcount/bmap operation is the responsibility of the corresponding
/// metadata replay engine, not the log framing decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsLogReplayExtent {
    ExtentFree {
        start_block: u64,
        block_count: u32,
    },
    Mapping {
        owner: u64,
        start_block: u64,
        start_offset: u64,
        block_count: u32,
        flags: u32,
    },
    Refcount {
        start_block: u64,
        block_count: u32,
        flags: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsInodeReplayItem {
    pub inode: u64,
    pub block_number: u64,
    pub block_count: u32,
    pub byte_offset: u32,
    pub fields: u32,
    /// `ilf_dsize` and `ilf_asize` are byte counts, not region counts.
    pub data_size: u16,
    pub attr_size: u16,
    /// The inode core/fork regions following the format region, in their
    /// native log order.  They are not interpreted by this framing layer.
    pub regions: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDquotReplayItem {
    pub id: u32,
    pub block_number: u64,
    pub block_count: u32,
    pub byte_offset: u32,
    pub disk_dquot: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsDiskDquot {
    pub id: u32,
    pub quota_type: u8,
    pub lsn: Option<u64>,
}

impl XfsInodeReplayItem {
    /// Converts native-endian `xfs_log_dinode` data and fork regions to a
    /// complete big-endian dinode; journal inode cores must never be copied.
    pub fn materialize_home_inode(
        &self,
        home: &[u8],
        lsn: u64,
        meta_uuid: Option<XfsUuid>,
        order: XfsLogByteOrder,
    ) -> XfsResult<Vec<u8>> {
        const CORE: u32 = 0x001;
        const DDATA: u32 = 0x002;
        const DEXT: u32 = 0x004;
        const DBROOT: u32 = 0x008;
        const ADATA: u32 = 0x040;
        const AEXT: u32 = 0x080;
        const ABROOT: u32 = 0x100;
        const SUPPORTED: u32 = CORE | DDATA | DEXT | DBROOT | ADATA | AEXT | ABROOT;
        if lsn == 0
            || self.fields & !SUPPORTED != 0
            || self.fields & CORE == 0
            || (self.fields & (DDATA | DEXT | DBROOT)).count_ones() > 1
            || (self.fields & (ADATA | AEXT | ABROOT)).count_ones() > 1
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let core = self.regions.first().ok_or(XfsError::CorruptMetadata)?;
        let version = byte(core, 4)?;
        let core_bytes = if version >= 3 {
            176
        } else if version >= 1 {
            100
        } else {
            return Err(XfsError::CorruptMetadata);
        };
        if core.len() != core_bytes || home.len() < core_bytes {
            return Err(XfsError::CorruptMetadata);
        }
        let expected = 1 + usize::from(self.data_size != 0) + usize::from(self.attr_size != 0);
        if self.regions.len() != expected
            || (self.fields & (DDATA | DEXT | DBROOT) != 0) != (self.data_size != 0)
            || (self.fields & (ADATA | AEXT | ABROOT) != 0) != (self.attr_size != 0)
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut out = home.to_vec();
        put_be16(&mut out, 0, native_u16(core, 0, order)?)?;
        for &(offset, width) in &[
            (2usize, 2usize),
            (8, 4),
            (12, 4),
            (16, 4),
            (20, 2),
            (22, 2),
            (24, 8),
            (56, 8),
            (64, 8),
            (72, 4),
            (76, 4),
            (80, 2),
            (84, 4),
            (88, 2),
            (90, 2),
            (92, 4),
            (96, 4),
        ] {
            match width {
                2 => put_be16(&mut out, offset, native_u16(core, offset, order)?)?,
                4 => put_be32(&mut out, offset, native_u32(core, offset, order)?)?,
                8 => put_be64(&mut out, offset, native_u64(core, offset, order)?)?,
                _ => return Err(XfsError::CorruptMetadata),
            }
        }
        out[4..8].copy_from_slice(slice(core, 4, 4)?);
        let flags2 = if version >= 3 {
            native_u64(core, 120, order)?
        } else {
            0
        };
        let bigtime = flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        for &offset in &[32usize, 40, 48] {
            if bigtime {
                put_be64(&mut out, offset, native_u64(core, offset, order)?)?;
            } else {
                put_be32(&mut out, offset, native_u32(core, offset, order)?)?;
                put_be32(&mut out, offset + 4, native_u32(core, offset + 4, order)?)?;
            }
        }
        if version >= 3 {
            put_be64(&mut out, 104, native_u64(core, 104, order)?)?;
            put_be64(&mut out, 112, lsn)?;
            put_be64(&mut out, 120, flags2)?;
            put_be32(&mut out, 128, native_u32(core, 128, order)?)?;
            out[132..144].copy_from_slice(slice(core, 132, 12)?);
            if bigtime {
                put_be64(&mut out, 144, native_u64(core, 144, order)?)?;
            } else {
                put_be32(&mut out, 144, native_u32(core, 144, order)?)?;
                put_be32(&mut out, 148, native_u32(core, 148, order)?)?;
            }
            let ino = native_u64(core, 152, order)?;
            if ino != self.inode {
                return Err(XfsError::CorruptMetadata);
            }
            put_be64(&mut out, 152, ino)?;
            out[160..176].copy_from_slice(slice(core, 160, 16)?);
            if let Some(uuid) = meta_uuid {
                if slice(core, 160, 16)? != uuid.0 {
                    return Err(XfsError::CorruptMetadata);
                }
            }
        }
        let forkoff = usize::from(byte(&out, 82)?)
            .checked_mul(8)
            .ok_or(XfsError::AddressOutOfRange)?;
        if forkoff != 0 && (forkoff < core_bytes || forkoff > out.len()) {
            return Err(XfsError::CorruptMetadata);
        }
        let data_end = if forkoff == 0 { out.len() } else { forkoff };
        let mut next = 1usize;
        if self.data_size != 0 {
            let payload = self.regions.get(next).ok_or(XfsError::CorruptMetadata)?;
            next += 1;
            if payload.len() != usize::from(self.data_size) || payload.len() > data_end - core_bytes
            {
                return Err(XfsError::CorruptMetadata);
            }
            out[core_bytes..core_bytes + payload.len()].copy_from_slice(payload);
        }
        if self.attr_size != 0 {
            let payload = self.regions.get(next).ok_or(XfsError::CorruptMetadata)?;
            if forkoff == 0
                || payload.len() != usize::from(self.attr_size)
                || payload.len() > out.len() - forkoff
            {
                return Err(XfsError::CorruptMetadata);
            }
            out[forkoff..forkoff + payload.len()].copy_from_slice(payload);
        }
        if version >= 3 {
            rewrite_crc32c(&mut out, 100)?;
        }
        Ok(out)
    }
}

impl XfsDquotReplayItem {
    pub fn parse_disk_dquot(
        &self,
        v5: bool,
        meta_uuid: Option<XfsUuid>,
        bigtime_enabled: bool,
    ) -> XfsResult<XfsDiskDquot> {
        if self.disk_dquot.len() != 104
            || be16(&self.disk_dquot, 0)? != 0x4451
            || byte(&self.disk_dquot, 2)? != 1
            || be32(&self.disk_dquot, 4)? != self.id
        {
            return Err(XfsError::CorruptMetadata);
        }
        let quota_type = byte(&self.disk_dquot, 3)?;
        if !matches!(quota_type & 0x07, 1 | 2 | 4) || quota_type & !0x87 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let has_bigtime = quota_type & XfsDquot::DQTYPE_BIGTIME != 0;
        if (self.id == 0 && has_bigtime) || (self.id != 0 && has_bigtime != bigtime_enabled) {
            return Err(XfsError::CorruptMetadata);
        }
        if v5 && meta_uuid.is_none() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsDiskDquot {
            id: self.id,
            quota_type: quota_type & 0x07,
            lsn: None,
        })
    }
    fn materialize_home_dquot(
        &self,
        home: &[u8],
        lsn: u64,
        v5: bool,
        meta_uuid: Option<XfsUuid>,
        bigtime_enabled: bool,
    ) -> XfsResult<Vec<u8>> {
        let _ = self.parse_disk_dquot(v5, meta_uuid, bigtime_enabled)?;
        if home.len() != 136 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut out = home.to_vec();
        out[..104].copy_from_slice(&self.disk_dquot);
        if v5 {
            put_be64(&mut out, 112, lsn)?;
            if let Some(uuid) = meta_uuid {
                out[120..136].copy_from_slice(&uuid.0);
            }
            rewrite_crc32c(&mut out, 108)?;
        }
        Ok(out)
    }
}

impl XfsDquot {
    const DQTYPE_BIGTIME: u8 = 0x80;
    /// Checks every invariant that survives an in-flight counter/timer
    /// update.  The caller may deliberately hold a stale CRC until a log LSN
    /// has been selected, so checksum validation belongs in `parse` below.
    fn validate_image_identity(
        bytes: &[u8],
        expected_id: u32,
        expected_type: u8,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<u8> {
        if bytes.len() != 136
            || be16(bytes, 0)? != 0x4451
            || byte(bytes, 2)? != 1
            || byte(bytes, 3)? & 0x07 != expected_type
            || byte(bytes, 3)? & !0x87 != 0
            || be32(bytes, 4)? != expected_id
        {
            return Err(XfsError::CorruptMetadata);
        }
        let quota_type_flags = byte(bytes, 3)?;
        let has_bigtime = quota_type_flags & Self::DQTYPE_BIGTIME != 0;
        // Root dquots store grace *durations*, which are always legacy
        // seconds.  Non-root dquots store expiry timestamps and must match
        // the filesystem's BIGTIME on-disk format exactly.
        if (expected_id == 0 && has_bigtime) || (expected_id != 0 && has_bigtime != bigtime_enabled)
        {
            return Err(XfsError::CorruptMetadata);
        }
        if slice(bytes, 120, 16)? != meta_uuid.0 {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(quota_type_flags)
    }

    fn parse(
        bytes: &[u8],
        expected_id: u32,
        expected_type: u8,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<Self> {
        // xfs_dqblk: the fixed xfs_disk_dquot region is exactly 136 bytes.
        // Verify the v5 checksum, LSN-bearing UUID tail, identity and type
        // before exposing counters to either accounting or admission.
        let quota_type_flags = Self::validate_image_identity(
            bytes,
            expected_id,
            expected_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        verify_crc32c(bytes, 108)?;
        Ok(Self {
            id: expected_id,
            quota_type: expected_type,
            quota_type_flags,
            block_hard: be64(bytes, 8)?,
            block_soft: be64(bytes, 16)?,
            inode_hard: be64(bytes, 24)?,
            inode_soft: be64(bytes, 32)?,
            realtime_hard: be64(bytes, 72)?,
            realtime_soft: be64(bytes, 80)?,
            blocks: be64(bytes, 40)?,
            inodes: be64(bytes, 48)?,
            realtime_blocks: be64(bytes, 88)?,
            inode_timer: be32(bytes, 56)?,
            block_timer: be32(bytes, 60)?,
            realtime_timer: be32(bytes, 96)?,
            inode_warnings: be16(bytes, 64)?,
            block_warnings: be16(bytes, 66)?,
            realtime_warnings: be16(bytes, 100)?,
        })
    }

    fn apply_delta(
        &self,
        block_delta: i64,
        inode_delta: i64,
        enforce: bool,
        now: u64,
        block_grace: u32,
        inode_grace: u32,
    ) -> XfsResult<XfsDquotAdmission> {
        let blocks = if block_delta >= 0 {
            self.blocks.checked_add(block_delta as u64)
        } else {
            self.blocks.checked_sub(block_delta.unsigned_abs())
        }
        .ok_or(XfsError::CorruptMetadata)?;
        let inodes = if inode_delta >= 0 {
            self.inodes.checked_add(inode_delta as u64)
        } else {
            self.inodes.checked_sub(inode_delta.unsigned_abs())
        }
        .ok_or(XfsError::CorruptMetadata)?;
        // A zero limit is unlimited.  Grace timers are persistent policy;
        // crossing a soft limit is allowed here, while hard limits reject the
        // whole enclosing metadata transaction before a log reservation.
        if enforce
            && ((self.block_hard != 0 && blocks > self.block_hard)
                || (self.inode_hard != 0 && inodes > self.inode_hard))
        {
            return Err(XfsError::QuotaExceeded);
        }
        let (block_timer, block_warnings) = self.soft_admission(
            blocks,
            self.block_soft,
            self.block_timer,
            self.block_warnings,
            now,
            block_grace,
            enforce,
        )?;
        let (inode_timer, inode_warnings) = self.soft_admission(
            inodes,
            self.inode_soft,
            self.inode_timer,
            self.inode_warnings,
            now,
            inode_grace,
            enforce,
        )?;
        Ok(XfsDquotAdmission {
            blocks,
            inodes,
            block_timer,
            inode_timer,
            block_warnings,
            inode_warnings,
        })
    }

    fn timer_to_unix(&self, timer: u32) -> XfsResult<u64> {
        if timer == 0 {
            return Ok(0);
        }
        if self.quota_type_flags & Self::DQTYPE_BIGTIME != 0 {
            Ok(u64::from(timer) << 2)
        } else {
            Ok(u64::from(timer))
        }
    }

    fn unix_to_timer(&self, unix: u64) -> XfsResult<u32> {
        if self.quota_type_flags & Self::DQTYPE_BIGTIME != 0 {
            // XFS_DQ_BIGTIME_SHIFT=2; round up to avoid shortening grace.
            u32::try_from(unix.checked_add(3).ok_or(XfsError::AddressOutOfRange)? >> 2)
                .map_err(|_| XfsError::AddressOutOfRange)
        } else {
            u32::try_from(unix).map_err(|_| XfsError::AddressOutOfRange)
        }
    }

    fn soft_admission(
        &self,
        used: u64,
        soft: u64,
        timer: u32,
        warnings: u16,
        now: u64,
        grace: u32,
        enforce: bool,
    ) -> XfsResult<(u32, u16)> {
        if soft == 0 || used <= soft {
            return Ok((0, 0));
        }
        let expires = self.timer_to_unix(timer)?;
        if expires != 0 && enforce && now >= expires {
            return Err(XfsError::QuotaExceeded);
        }
        if timer == 0 {
            let expiry = now
                .checked_add(u64::from(grace))
                .ok_or(XfsError::AddressOutOfRange)?;
            return Ok((
                self.unix_to_timer(expiry.max(1))?,
                warnings.saturating_add(1),
            ));
        }
        Ok((timer, warnings))
    }
}

impl XfsDquotDelta {
    fn log_item(
        &self,
        lsn: u64,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<XfsDquotReplayItem> {
        if lsn == 0
            || self.before.len() != 136
            || self.after.len() != 136
            || self.block_count == 0
            || usize::try_from(self.byte_offset)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_add(136)
                .is_none_or(|end| {
                    end > usize::try_from(self.block_count)
                        .unwrap_or(0)
                        .saturating_mul(512)
                })
        {
            return Err(XfsError::CorruptMetadata);
        }
        let _ = XfsDquot::parse(
            &self.before,
            self.id,
            self.quota_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        // `after` carries changed counters/timers and therefore intentionally
        // has the pre-transaction CRC until this selected LSN is installed.
        let _ = XfsDquot::validate_image_identity(
            &self.after,
            self.id,
            self.quota_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        let mut image = self.after.clone();
        put_be64(&mut image, 112, lsn)?;
        image[120..136].copy_from_slice(&meta_uuid.0);
        rewrite_crc32c(&mut image, 108)?;
        let _ = XfsDquot::parse(&image, self.id, self.quota_type, meta_uuid, bigtime_enabled)?;
        Ok(XfsDquotReplayItem {
            id: self.id,
            block_number: self.basic_block,
            block_count: self.block_count,
            byte_offset: self.byte_offset,
            disk_dquot: image[..104].to_vec(),
        })
    }

    fn encode_log_regions(
        &self,
        lsn: u64,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
        order: XfsLogByteOrder,
    ) -> XfsResult<Vec<Vec<u8>>> {
        let item = self.log_item(lsn, meta_uuid, bigtime_enabled)?;
        let mut format = vec![0; 24];
        native_put_u16(&mut format, 0, 0x123d, order)?;
        native_put_u16(&mut format, 2, 2, order)?;
        native_put_u32(&mut format, 4, item.id, order)?;
        native_put_u64(&mut format, 8, item.block_number, order)?;
        native_put_u32(&mut format, 16, item.block_count, order)?;
        native_put_u32(&mut format, 20, item.byte_offset, order)?;
        Ok(vec![format, item.disk_dquot])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsIntentReplayItem {
    pub key: XfsIntentKey,
    pub extents: Vec<XfsLogReplayExtent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDoneReplayItem {
    pub key: XfsIntentKey,
    /// EFD records include completed extents; the other done-item formats do
    /// not.  Keeping this typed distinction prevents callers from confusing
    /// an empty done body with a truncated EFD.
    pub extents: Vec<XfsLogReplayExtent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsReplayItem {
    Buffer(XfsBufferReplayItem),
    Inode(XfsInodeReplayItem),
    Dquot(XfsDquotReplayItem),
    Intent(XfsIntentReplayItem),
    Done(XfsDoneReplayItem),
    Quotaoff { flags: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsHomeWriteDescriptor {
    pub basic_block: u64,
    pub bytes: Vec<u8>,
    pub lsn: u64,
    pub item: XfsBufferReplayItem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRecoveryCommit {
    pub lsn: u64,
    pub writes: Vec<XfsHomeWriteDescriptor>,
}

/// The replay-relevant journal boundary obtained while walking complete log
/// records.  This is not an on-disk "clean" flag: XFS has no safe synthetic
/// clean marker.  It describes a *plan*; after callers apply every committed
/// item they discard the plan rather than forging a clean journal record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsJournalRecoveryState {
    pub head_lsn: u64,
    pub tail_lsn: u64,
    pub committed_transactions: usize,
    pub interrupted_transactions: usize,
}

/// Result of walking the physical XFS log rather than accepting caller-made
/// record images.  `clean` is deliberately conservative: it is true only
/// when the log region contains no complete, authenticated record.  XFS does
/// not have a synthetic clean-record format, so a scanner never fabricates a
/// clean cursor after seeing stale or torn media.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsPhysicalLogScan {
    pub records: Vec<XfsJournalRecord>,
    pub state: XfsJournalRecoveryState,
    pub cursor: Option<XfsLogRing>,
    pub clean: bool,
}

const XFS_LOG_BASIC_BLOCK: usize = 512;
const XFS_LOG_MAX_INLINE_CYCLE_DATA: usize = 64;
const XFS_DQ_DEFAULT_GRACE_SECONDS: u32 = 7 * 24 * 60 * 60;
// These are the on-disk `sb_qflags` bits from xfs_log_format.h.  They are
// deliberately not the similarly named FS_{U,G,PROJ}QUOTA UAPI flags.
const XFS_UQUOTA_ACCT: u16 = 1 << 0;
const XFS_UQUOTA_ENFD: u16 = 1 << 1;
const XFS_PQUOTA_ACCT: u16 = 1 << 3;
const XFS_GQUOTA_ACCT: u16 = 1 << 6;
const XFS_GQUOTA_ENFD: u16 = 1 << 7;
const XFS_PQUOTA_ENFD: u16 = 1 << 9;

/// One reserved, in-order region of the physical XFS log ring.  A reservation
/// may contain two segments when the record crosses the end of the ring; the
/// second segment has the next cycle and is never presented to media before
/// the first segment's cycle stamps have been durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogReservation {
    pub lsn: u64,
    pub cycle: u32,
    pub first_block: u32,
    pub record_blocks: u32,
    pub first_segment_blocks: u32,
    pub second_segment_blocks: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsLogFragment {
    pub start_block: u32,
    pub blocks: u32,
    pub cycle: u32,
    pub continued: bool,
}

impl XfsLogReservation {
    /// First free log position after this reservation.  This is the cursor
    /// which becomes reclaimable only after every home image for the record
    /// has reached stable storage.
    pub fn end_lsn(&self, ring_blocks: u32) -> XfsResult<u64> {
        if ring_blocks < 2 || self.first_block >= ring_blocks || self.record_blocks == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let end = self
            .first_block
            .checked_add(self.record_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        if end < ring_blocks {
            return Ok((u64::from(self.cycle) << 32) | u64::from(end));
        }
        let cycle = self
            .cycle
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok((u64::from(cycle) << 32) | u64::from(end - ring_blocks))
    }

    /// Returns physical write fragments in ring order.  A coordinator maps
    /// the split point to `CONTINUE/WAS_CONT/END` operation flags while
    /// preserving the original operation byte stream; it must not treat the
    /// second fragment as a new committed transaction.
    pub fn fragments(&self) -> XfsResult<Vec<XfsLogFragment>> {
        let mut fragments = Vec::new();
        fragments
            .try_reserve(if self.second_segment_blocks == 0 {
                1
            } else {
                2
            })
            .map_err(|_| XfsError::NoMemory)?;
        fragments.push(XfsLogFragment {
            start_block: self.first_block,
            blocks: self.first_segment_blocks,
            cycle: self.cycle,
            continued: self.second_segment_blocks != 0,
        });
        if self.second_segment_blocks != 0 {
            fragments.push(XfsLogFragment {
                start_block: 0,
                blocks: self.second_segment_blocks,
                cycle: self
                    .cycle
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                continued: true,
            });
        }
        Ok(fragments)
    }
}

/// Native physical-log cursor and grant accounting.  Addresses are log-basic
/// blocks relative to the log device, never data-device filesystem blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogRing {
    blocks: u32,
    head: u32,
    /// Full LSN of the first log position that has not completed a durable
    /// home-image checkpoint.  Keeping the cycle is essential across wrap.
    tail_lsn: u64,
    cycle: u32,
    last_record: Option<u32>,
}

impl XfsLogRing {
    pub fn new(blocks: u32, head: u32, tail: u32, cycle: u32) -> XfsResult<Self> {
        if blocks < 2 || head >= blocks || tail >= blocks || cycle == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let tail_cycle = if head < tail {
            cycle.checked_sub(1).ok_or(XfsError::CorruptMetadata)?
        } else {
            cycle
        };
        Ok(Self {
            blocks,
            head,
            tail_lsn: (u64::from(tail_cycle) << 32) | u64::from(tail),
            cycle,
            last_record: None,
        })
    }

    /// Restores a recovered cursor together with the physical predecessor of
    /// the next record.  Recovery-generated commits must extend the existing
    /// on-media chain instead of treating the durable tail as a predecessor.
    pub fn recovered(
        blocks: u32,
        head: u32,
        tail: u32,
        cycle: u32,
        last_record: u32,
    ) -> XfsResult<Self> {
        if last_record >= blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let mut ring = Self::new(blocks, head, tail, cycle)?;
        ring.last_record = Some(last_record);
        Ok(ring)
    }

    pub const fn head(&self) -> u32 {
        self.head
    }
    pub const fn tail(&self) -> u32 {
        self.tail_lsn as u32
    }
    pub const fn cycle(&self) -> u32 {
        self.cycle
    }
    pub const fn blocks(&self) -> u32 {
        self.blocks
    }
    pub const fn next_lsn(&self) -> u64 {
        (self.cycle as u64) << 32 | self.head as u64
    }
    pub const fn tail_lsn(&self) -> u64 {
        self.tail_lsn
    }
    pub const fn previous_record(&self) -> u32 {
        match self.last_record {
            Some(block) => block,
            None => self.tail(),
        }
    }

    fn used(&self) -> u32 {
        let head = self.next_lsn();
        let tail = self.tail_lsn;
        if head < tail {
            return self.blocks;
        }
        let cycles = (head >> 32).saturating_sub(tail >> 32);
        let distance = cycles
            .checked_mul(u64::from(self.blocks))
            .and_then(|value| value.checked_add(u64::from(head as u32)))
            .and_then(|value| value.checked_sub(u64::from(tail as u32)));
        distance
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value < self.blocks)
            .unwrap_or(self.blocks)
    }

    pub fn free_blocks(&self) -> u32 {
        self.blocks.saturating_sub(self.used()).saturating_sub(1)
    }

    /// Reserves a complete record and advances the volatile head.  The tail
    /// guard prevents head from becoming indistinguishable from a full ring;
    /// callers must checkpoint AIL entries before retrying an exhausted grant.
    pub fn reserve(&mut self, record_bytes: usize) -> XfsResult<XfsLogReservation> {
        if record_bytes == 0 || record_bytes % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let record_blocks = u32::try_from(record_bytes / XFS_LOG_BASIC_BLOCK)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        if record_blocks == 0 || record_blocks >= self.blocks || record_blocks > self.free_blocks()
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let remaining = self.blocks - self.head;
        let (first, second) = if record_blocks <= remaining {
            (record_blocks, 0)
        } else {
            (remaining, record_blocks - remaining)
        };
        // Wrapping consumes the remaining ring blocks as a cycle boundary;
        // they cannot be allocated to another writer before this record.
        let consumed = if second == 0 {
            record_blocks
        } else {
            remaining
                .checked_add(second)
                .ok_or(XfsError::AddressOutOfRange)?
        };
        if consumed > self.free_blocks() {
            return Err(XfsError::AddressOutOfRange);
        }
        let lsn = (u64::from(self.cycle) << 32) | u64::from(self.head);
        let reservation = XfsLogReservation {
            lsn,
            cycle: self.cycle,
            first_block: self.head,
            record_blocks,
            first_segment_blocks: first,
            second_segment_blocks: second,
        };
        self.last_record = Some(reservation.first_block);
        if second == 0 {
            self.head = self
                .head
                .checked_add(record_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if self.head == self.blocks {
                self.head = 0;
                self.cycle = self
                    .cycle
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        } else {
            self.head = second;
            self.cycle = self
                .cycle
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(reservation)
    }

    /// Advances the durable tail after an AIL checkpoint has written all home
    /// blocks below `lsn`.  A tail never moves past the current head and LSNs
    /// from an older cycle are rejected rather than numerically wrapped.
    pub fn checkpoint_tail(&mut self, lsn: u64) -> XfsResult<()> {
        let cycle = (lsn >> 32) as u32;
        let block = lsn as u32;
        if block >= self.blocks || cycle > self.cycle || self.cycle.saturating_sub(cycle) > 1 {
            return Err(XfsError::CorruptMetadata);
        }
        if lsn < self.tail_lsn || lsn > self.next_lsn() {
            return Err(XfsError::CorruptMetadata);
        }
        self.tail_lsn = lsn;
        Ok(())
    }

    /// Applies physical sector cycle stamping to a single inline record.
    /// Stamping overwrites the leading word of every data basic block; the
    /// displaced words are saved in the header cycle-data array so recovery
    /// can reconstruct the logical record before interpreting log operations.
    pub fn stamp_inline_record(
        &self,
        reservation: &XfsLogReservation,
        record: &mut [u8],
    ) -> XfsResult<()> {
        if reservation.second_segment_blocks != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        self.stamp_record(reservation, record)
    }

    /// Stamps a record that may cross the physical ring end.  The logical
    /// record keeps its original header cycle; individual basic blocks after
    /// the wrap carry the following cycle.  The caller writes the resulting
    /// byte stream in `reservation.fragments()` order.
    pub fn stamp_record(
        &self,
        reservation: &XfsLogReservation,
        record: &mut [u8],
    ) -> XfsResult<()> {
        let header = XfsLogRecordHeader::parse(
            record,
            XfsUuid(
                record[304..320]
                    .try_into()
                    .map_err(|_| XfsError::CorruptMetadata)?,
            ),
            false,
        )?;
        let header_bytes = header.header_bytes()?;
        if record.len() % XFS_LOG_BASIC_BLOCK != 0
            || record.len() / XFS_LOG_BASIC_BLOCK != reservation.record_blocks as usize
            || be32(record, 0)? != XFS_LOG_RECORD_MAGIC
            || be32(record, 4)? != reservation.cycle
            || header_bytes > record.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        for extension in 1..header_bytes / XFS_LOG_BASIC_BLOCK {
            let physical = reservation
                .first_block
                .checked_add(extension as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            let cycle = reservation
                .cycle
                .checked_add((physical >= self.blocks) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            put_be32(record, extension * XFS_LOG_BASIC_BLOCK, cycle)?;
        }
        for basic_block in 0..(record.len() - header_bytes) / XFS_LOG_BASIC_BLOCK {
            let offset = header_bytes + basic_block * XFS_LOG_BASIC_BLOCK;
            let displaced = be32(record, offset)?;
            log_cycle_data_put(record, basic_block, displaced)?;
            let physical = reservation
                .first_block
                .checked_add((header_bytes / XFS_LOG_BASIC_BLOCK + basic_block) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            let cycle = reservation
                .cycle
                .checked_add((physical >= self.blocks) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            put_be32(record, offset, cycle)?;
        }
        rewrite_log_record_crc(record, &header)?;
        Ok(())
    }

    /// Restores an inline record after physical cycle validation.  The caller
    /// must verify the stamped record checksum before invoking this method.
    pub fn unstamp_inline_record(record: &mut [u8]) -> XfsResult<()> {
        if record.len() % XFS_LOG_BASIC_BLOCK != 0 || be32(record, 0)? != XFS_LOG_RECORD_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        let header = XfsLogRecordHeader::parse(
            record,
            XfsUuid(
                record[304..320]
                    .try_into()
                    .map_err(|_| XfsError::CorruptMetadata)?,
            ),
            false,
        )?;
        let header_bytes = header.header_bytes()?;
        if header_bytes > record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let cycle = be32(record, 4)?;
        for basic_block in 0..(record.len() - header_bytes) / XFS_LOG_BASIC_BLOCK {
            let offset = header_bytes + basic_block * XFS_LOG_BASIC_BLOCK;
            let stamped = be32(record, offset)?;
            if stamped != cycle
                && stamped != cycle.checked_add(1).ok_or(XfsError::AddressOutOfRange)?
            {
                return Err(XfsError::CorruptMetadata);
            }
            let displaced = log_cycle_data_get(record, basic_block)?;
            put_be32(record, offset, displaced)?;
        }
        Ok(())
    }
}

/// Active-item-list entry.  The AIL owns only checkpoint ordering: metadata
/// item decoding and home writes remain in the corresponding transaction
/// implementation, so an item can never be removed merely because it was
/// submitted rather than made durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAilEntry {
    pub lsn: u64,
    pub end_lsn: u64,
    pub transaction_id: u32,
    /// Exact post-LSN home images.  They are prepared before the log record
    /// is persisted and retained until the ordered AIL checkpoint succeeds,
    /// so an interrupted home flush never re-plans metadata against changed
    /// media or leaks a ring reservation.
    checkpoint_homes: Vec<(u64, Vec<u8>)>,
}

#[derive(Default)]
pub struct XfsAil {
    entries: Vec<XfsAilEntry>,
}

impl XfsAil {
    fn reserve_insert(&mut self, entry: &XfsAilEntry) -> XfsResult<()> {
        if entry.lsn == 0
            || entry.end_lsn <= entry.lsn
            || self
                .entries
                .iter()
                .any(|present| present.transaction_id == entry.transaction_id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        self.entries.try_reserve(1).map_err(|_| XfsError::NoMemory)
    }

    fn insert_reserved(&mut self, entry: XfsAilEntry) -> XfsResult<()> {
        if entry.lsn == 0
            || entry.end_lsn <= entry.lsn
            || self
                .entries
                .iter()
                .any(|present| present.transaction_id == entry.transaction_id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        let index = self
            .entries
            .partition_point(|present| present.lsn < entry.lsn);
        self.entries.insert(index, entry);
        Ok(())
    }

    pub fn insert(&mut self, entry: XfsAilEntry) -> XfsResult<()> {
        self.reserve_insert(&entry)?;
        self.insert_reserved(entry)
    }
    pub fn oldest(&self) -> Option<&XfsAilEntry> {
        self.entries.first()
    }
    pub fn checkpoint_through(&mut self, lsn: u64) -> Vec<XfsAilEntry> {
        let end = self.entries.partition_point(|entry| entry.lsn <= lsn);
        self.entries.drain(..end).collect()
    }
    pub fn entries(&self) -> &[XfsAilEntry] {
        &self.entries
    }

    fn attach_checkpoint_homes(&mut self, lsn: u64, homes: Vec<(u64, Vec<u8>)>) -> XfsResult<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.lsn == lsn)
            .ok_or(XfsError::CorruptMetadata)?;
        if !entry.checkpoint_homes.is_empty() || homes.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        entry.checkpoint_homes = homes;
        Ok(())
    }
}

/// A fully encoded and cycle-stamped log record that owns its grant until it
/// is either durably written and published to the AIL or retried.  Keeping the
/// byte image here is essential: regenerating it after a partial FUA failure
/// could allocate a different LSN or lose the original continuation layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsPreparedLogCommit {
    pub reservation: XfsLogReservation,
    pub transaction_id: u32,
    pub record: Vec<u8>,
}

/// One typed AG metadata home image staged for an atomic XFS transaction.
/// `basic_block` is a physical 512-byte address, while `before`/`after` are
/// complete buffer images; callers cannot submit an unbounded byte patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirtyMetadataBuffer {
    pub metadata_type: XfsMetadataBufferType,
    pub basic_block: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl XfsDirtyMetadataBuffer {
    pub fn to_log_item(&self) -> XfsResult<XfsBufferReplayItem> {
        if self.before.is_empty()
            || self.before.len() != self.after.len()
            || self.before.len() % XFS_LOG_BASIC_BLOCK != 0
        {
            return Err(XfsError::CorruptMetadata);
        }
        let kind = match self.metadata_type {
            XfsMetadataBufferType::Btree => 4u16,
            XfsMetadataBufferType::Agf => 5,
            XfsMetadataBufferType::Agfl => 6,
            XfsMetadataBufferType::Agi => 7,
            XfsMetadataBufferType::Inode => 8,
            XfsMetadataBufferType::Dquot => 3,
            XfsMetadataBufferType::Directory => 10,
            XfsMetadataBufferType::Attribute => 15,
            XfsMetadataBufferType::Superblock => 18,
            XfsMetadataBufferType::Realtime => 19,
        };
        let mut dirty_chunks = Vec::new();
        let mut chunks = Vec::new();
        for offset in (0..self.after.len()).step_by(128) {
            if self.before[offset..offset + 128] != self.after[offset..offset + 128] {
                dirty_chunks
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                chunks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                dirty_chunks
                    .push(u32::try_from(offset / 128).map_err(|_| XfsError::AddressOutOfRange)?);
                chunks.push(self.after[offset..offset + 128].to_vec());
            }
        }
        if dirty_chunks.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsBufferReplayItem {
            flags: kind << 11,
            block_number: self.basic_block,
            block_count: u16::try_from(self.after.len() / XFS_LOG_BASIC_BLOCK)
                .map_err(|_| XfsError::AddressOutOfRange)?,
            dirty_chunks,
            chunks,
        })
    }
}

/// All metadata buffers for one transaction.  Conversion is all-or-nothing:
/// no log grant or home write is attempted until every buffer's dirty bitmap
/// has been constructed successfully.
/// A data block whose allocation and contents are prepared alongside a
/// metadata transaction.  XFS deliberately writes ordinary file data before
/// publishing the inode mapping; the same ordering is required for remote
/// symlink bodies, otherwise a committed name could resolve to uninitialised
/// bytes after a crash.  The data itself is not replayed from the journal:
/// it is FUA-written before the transaction is committed, while the logged
/// inode and AG updates make it reachable only afterwards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsStagedDataWrite {
    pub fs_block: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct XfsMetadataTransaction {
    pub buffers: Vec<XfsDirtyMetadataBuffer>,
    pub data_writes: Vec<XfsStagedDataWrite>,
    pub realtime_writes: Vec<XfsStagedDataWrite>,
    /// Reservation and admission have already completed for these native
    /// dquots.  They remain explicit until commit construction, where their
    /// full images become DQUOT buffer items in this same log record.
    pub dquots: Vec<XfsDquotDelta>,
}

/// The fixed dinode-core fields which can be replaced without changing an
/// inode's fork layout.  This is deliberately narrower than a generic VFS
/// setattr: device and project-id updates have additional ownership/quota
/// transactions which are not part of this primitive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct XfsInodeCoreUpdate {
    pub mode: Option<u16>,
    pub owner: Option<(u32, u32)>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
    pub ctime: Option<(i64, u32)>,
}

impl XfsInodeCoreUpdate {
    pub const fn is_empty(self) -> bool {
        self.mode.is_none()
            && self.owner.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
            && self.ctime.is_none()
    }
}

/// Aggregate counters reconstructed from the canonical ownership view of
/// every allocation group.  `free_*` values are not copied from a convenient
/// superblock field: each one is checked against the btree-derived snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct XfsStatCounts {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
}

impl XfsMetadataTransaction {
    /// Produces one exact home image per physical buffer.  Independent inode
    /// cores can share a filesystem block, and AG/allocator planners can
    /// contribute disjoint fields to one header; rejecting those merely makes
    /// atomic namespace operations impossible.  Every contributor must start
    /// from the same `before` image.  A byte changed by two contributors is
    /// accepted only when both select the identical value; otherwise there is
    /// no serializable transaction and the whole request is rejected before a
    /// log grant or home write.
    pub fn composed_buffers(&self) -> XfsResult<Vec<XfsDirtyMetadataBuffer>> {
        let mut composed = Vec::<XfsDirtyMetadataBuffer>::new();
        composed
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| XfsError::NoMemory)?;
        for buffer in &self.buffers {
            if buffer.before.is_empty()
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % XFS_LOG_BASIC_BLOCK != 0
            {
                return Err(XfsError::CorruptMetadata);
            }
            let blocks = u64::try_from(buffer.before.len() / XFS_LOG_BASIC_BLOCK)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let end = buffer
                .basic_block
                .checked_add(blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            let existing = composed
                .iter_mut()
                .find(|prior| prior.basic_block == buffer.basic_block);
            let Some(existing) = existing else {
                if composed.iter().any(|prior| {
                    let prior_blocks =
                        u64::try_from(prior.before.len() / XFS_LOG_BASIC_BLOCK).unwrap_or(0);
                    let prior_end = prior.basic_block.checked_add(prior_blocks).unwrap_or(0);
                    prior_blocks == 0 || buffer.basic_block < prior_end && prior.basic_block < end
                }) {
                    return Err(XfsError::CorruptMetadata);
                }
                composed.push(buffer.clone());
                continue;
            };
            // Allocator trees are a semantic reservation, not merely a byte
            // patch.  Two independently prepared AG free-space images can be
            // byte-identical while claiming the same extent.  Only the batch
            // planner is permitted to produce an AGF/AGFL replacement, so
            // never merge a second one here.
            if matches!(
                existing.metadata_type,
                XfsMetadataBufferType::Agf | XfsMetadataBufferType::Agfl
            ) {
                return Err(XfsError::CorruptMetadata);
            }
            if existing.metadata_type != buffer.metadata_type
                || existing.before != buffer.before
                || existing.after.len() != buffer.after.len()
            {
                return Err(XfsError::CorruptMetadata);
            }
            for index in 0..existing.after.len() {
                let old = existing.before[index];
                let prior = existing.after[index];
                let next = buffer.after[index];
                if prior != old && next != old && prior != next {
                    return Err(XfsError::CorruptMetadata);
                }
                if next != old {
                    existing.after[index] = next;
                }
            }
        }
        Ok(composed)
    }

    pub fn log_items(&self) -> XfsResult<Vec<XfsBufferReplayItem>> {
        if self.buffers.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let buffers = self.composed_buffers()?;
        let mut items = Vec::new();
        items
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| XfsError::NoMemory)?;
        for buffer in &buffers {
            items.push(buffer.to_log_item()?);
        }
        Ok(items)
    }
}

/// An extent selected from one AG plus the fully staged free-space metadata
/// transaction.  It is deliberately not a reservation token: publication is
/// only possible through `commit_metadata_transaction`, after the one record
/// containing every AGF/AGFL/bnobt/cntbt buffer is durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsExtentAllocation {
    pub ag: u32,
    pub start_block: u32,
    pub block_count: u32,
    pub transaction: XfsMetadataTransaction,
}

/// Several non-overlapping reservations selected from one AG ownership
/// snapshot.  Its single metadata transaction is the only free-space update
/// that may accompany the individual extents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsExtentAllocationBatch {
    pub ag: u32,
    pub allocations: Vec<XfsExtentAllocation>,
    pub transaction: XfsMetadataTransaction,
}

/// An inode number selected from an inobt record.  The allocation bitmap and
/// (when present) finobt are staged together with AGI; inode-core creation is
/// intentionally a later buffer item in the caller's same transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsInodeAllocation {
    pub ag: u32,
    pub ag_inode: u32,
    pub inode: u64,
    /// A remote symlink data run reserved by the same AG snapshot as this
    /// inode bit.  Its free-space images already live in `transaction`.
    pub remote_data: Option<XfsExtentAllocation>,
    pub transaction: XfsMetadataTransaction,
}

/// Initial persistent state for an inode selected from an AG inobt record.
/// The inode is not namespace-visible until its initialized core, parent
/// directory image and allocator transaction are committed together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsNewInode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub project_id: u32,
    /// Required only for a directory's native shortform `..` record.
    pub parent: Option<u64>,
    /// Bytes for a new symlink.  The bytes are native raw pathname bytes,
    /// never a UTF-8 surrogate; short values use the local fork while long
    /// values allocate and stage a remote data extent before publication.
    pub symlink_target: Option<Vec<u8>>,
}

/// Result of a name publication decision made while the mount coordinator
/// owns its namespace/log lock.  `Existing` is not a failed create: it is the
/// atomic OpenOrCreate outcome, and must not be reconstructed by an unlocked
/// VFS lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsNamedInodeOutcome {
    Created(u64),
    Existing(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRegularWrite {
    pub inode: u64,
    pub offset: u64,
    pub length: usize,
    pub allocated: Vec<XfsExtent>,
    pub mappings: Vec<XfsExtent>,
    pub zero_before_write: Vec<XfsExtent>,
    /// New COW homes whose untouched bytes must be copied from the old
    /// physical block before the caller's partial write is overlaid.  Keeping
    /// this in the prepared transaction is what prevents a partial write to a
    /// shared reflink block from exposing zero-filled neighbour bytes.
    pub copy_before_write: Vec<(u64, u64)>,
    pub metadata: XfsMetadataTransaction,
}

/// Structural result of one external data-fork bmapbt mutation.  `changed`
/// contains both reused and newly reserved blocks; `reclaimed` is returned to
/// its owning AG in the *same* metadata transaction that installs the new
/// inode root.  Keeping this as an explicit result makes root promotion and
/// collapse observable to callers instead of turning them into an accidental
/// side effect of an extent rewrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBmapLocalMutation {
    pub inode: u64,
    pub old_root_level: u16,
    pub new_root_level: u16,
    pub changed_blocks: Vec<u64>,
    pub reclaimed_blocks: Vec<u64>,
}

/// One allocation-group-local reflink mutation.  The planner owns verified
/// snapshots of *all* AG indexes which participate in a sharing operation;
/// callers never update rmap and refcount as independent journal records.
///
/// Keeping this representation block based is intentional.  XFS extent
/// mappings are block aligned, and partitioning at a block boundary gives the
/// rmap/refcount transformations a single, unambiguous owner and offset.
#[derive(Clone, Debug)]
pub struct XfsAgMutationPlanner {
    pub ag: u32,
    pub free: Vec<XfsAgFreeRecord>,
    pub rmap: Vec<XfsRmapRecord>,
    pub refcount: Vec<XfsRefcountRecord>,
}

impl XfsAgMutationPlanner {
    fn new(volume: &XfsVolume, ag: u32) -> XfsResult<Self> {
        let snapshot = volume.ag_ownership_snapshot(ag)?;
        Ok(Self {
            ag,
            free: snapshot.free_extents,
            rmap: volume.rmap_records(ag)?,
            refcount: volume.refcount_records(ag)?,
        })
    }

    fn refcount_at(&self, block: u32) -> XfsResult<u32> {
        match self.refcount.iter().find(|record| {
            block >= record.start_block
                && block
                    < record
                        .start_block
                        .checked_add(record.block_count)
                        .unwrap_or(0)
        }) {
            Some(record) => Ok(record.refcount),
            None => Ok(1),
        }
    }

    fn set_refcount(&mut self, block: u32, count: u32) -> XfsResult<()> {
        if count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let old = core::mem::take(&mut self.refcount);
        let mut next = Vec::new();
        next.try_reserve(old.len().saturating_add(1))
            .map_err(|_| XfsError::NoMemory)?;
        for record in old {
            let end = record
                .start_block
                .checked_add(record.block_count)
                .ok_or(XfsError::CorruptMetadata)?;
            if block < record.start_block || block >= end {
                next.push(record);
                continue;
            }
            if record.start_block < block {
                next.push(XfsRefcountRecord {
                    start_block: record.start_block,
                    block_count: block - record.start_block,
                    refcount: record.refcount,
                });
            }
            if block.checked_add(1).ok_or(XfsError::AddressOutOfRange)? < end {
                next.push(XfsRefcountRecord {
                    start_block: block + 1,
                    block_count: end - block - 1,
                    refcount: record.refcount,
                });
            }
        }
        // A count of one is represented by absence from refcountbt.
        if count > 1 {
            next.push(XfsRefcountRecord {
                start_block: block,
                block_count: 1,
                refcount: count,
            });
        }
        next.sort_unstable_by_key(|record| record.start_block);
        let mut merged: Vec<XfsRefcountRecord> = Vec::new();
        for record in next {
            if let Some(last) = merged.last_mut()
                && last.refcount == record.refcount
                && last.start_block.checked_add(last.block_count) == Some(record.start_block)
            {
                last.block_count = last
                    .block_count
                    .checked_add(record.block_count)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                merged.push(record);
            }
        }
        self.refcount = merged;
        Ok(())
    }

    fn add_owner(&mut self, block: u32, owner: u64, offset: u64) -> XfsResult<()> {
        if self.rmap.iter().any(|record| {
            record.start_block == block
                && record.block_count == 1
                && record.owner == owner
                && record.offset == offset
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        self.rmap.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        self.rmap.push(XfsRmapRecord {
            start_block: block,
            block_count: 1,
            owner,
            offset,
        });
        self.rmap
            .sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
        Ok(())
    }

    fn remove_owner(&mut self, block: u32, owner: u64, offset: u64) -> XfsResult<()> {
        let at = self
            .rmap
            .iter()
            .position(|record| {
                record.owner == owner
                    && block >= record.start_block
                    && block
                        < record
                            .start_block
                            .checked_add(record.block_count)
                            .unwrap_or(0)
                    && record
                        .offset
                        .checked_add(u64::from(block - record.start_block))
                        == Some(offset)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let record = self.rmap.remove(at);
        let relative = block - record.start_block;
        if relative != 0 {
            self.rmap.push(XfsRmapRecord {
                start_block: record.start_block,
                block_count: relative,
                owner,
                offset: record.offset,
            });
        }
        let tail = record.block_count - relative - 1;
        if tail != 0 {
            self.rmap.push(XfsRmapRecord {
                start_block: block + 1,
                block_count: tail,
                owner,
                offset: offset.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
            });
        }
        self.rmap
            .sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
        Ok(())
    }

    fn release_free_block(&mut self, block: u32) -> XfsResult<()> {
        if block < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        self.free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        self.free.push(XfsAgFreeRecord {
            start_block: block,
            block_count: 1,
        });
        self.free.sort_unstable_by_key(|record| record.start_block);
        let mut merged: Vec<XfsAgFreeRecord> = Vec::new();
        for record in core::mem::take(&mut self.free) {
            if let Some(last) = merged.last_mut()
                && last.start_block.checked_add(last.block_count) == Some(record.start_block)
            {
                last.block_count = last
                    .block_count
                    .checked_add(record.block_count)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                merged.push(record);
            }
        }
        self.free = merged;
        Ok(())
    }

    fn claim_free_block(&mut self) -> XfsResult<u32> {
        let index = self
            .free
            .iter()
            .position(|record| record.block_count != 0)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.free[index].start_block;
        self.free[index].start_block = block.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        self.free[index].block_count -= 1;
        if self.free[index].block_count == 0 {
            self.free.remove(index);
        }
        Ok(block)
    }

    /// Claims one already-selected free block.  Allocation selection and the
    /// final four-tree image must use the same snapshot; accepting an
    /// arbitrary later free block here would make the normal allocator and a
    /// reflink COW transaction race for the same AGFL replacement home.
    fn claim_specific_free_block(&mut self, block: u32) -> XfsResult<()> {
        let index = self
            .free
            .iter()
            .position(|record| {
                block >= record.start_block
                    && block
                        < record
                            .start_block
                            .checked_add(record.block_count)
                            .unwrap_or(0)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let record = self.free.remove(index);
        if record.start_block < block {
            self.free.push(XfsAgFreeRecord {
                start_block: record.start_block,
                block_count: block - record.start_block,
            });
        }
        let next = block.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        let end = record
            .start_block
            .checked_add(record.block_count)
            .ok_or(XfsError::CorruptMetadata)?;
        if next < end {
            self.free.push(XfsAgFreeRecord {
                start_block: next,
                block_count: end - next,
            });
        }
        self.free.sort_unstable_by_key(|record| record.start_block);
        Ok(())
    }
}

fn xfs_extent_at(extents: &[XfsExtent], file_block: u64) -> XfsResult<Option<XfsExtent>> {
    let Some(extent) = extents.iter().copied().find(|extent| {
        file_block >= extent.file_block
            && file_block
                < extent
                    .file_block
                    .checked_add(u64::from(extent.block_count))
                    .unwrap_or(0)
    }) else {
        return Ok(None);
    };
    let relative = file_block
        .checked_sub(extent.file_block)
        .ok_or(XfsError::CorruptMetadata)?;
    Ok(Some(XfsExtent {
        unwritten: extent.unwritten,
        file_block,
        start_block: extent
            .start_block
            .checked_add(relative)
            .ok_or(XfsError::AddressOutOfRange)?,
        block_count: 1,
    }))
}

fn xfs_replace_one_mapping(
    extents: &mut Vec<XfsExtent>,
    file_block: u64,
    replacement: Option<XfsExtent>,
) -> XfsResult<()> {
    let mut next = Vec::new();
    next.try_reserve(extents.len().saturating_add(2))
        .map_err(|_| XfsError::NoMemory)?;
    for extent in core::mem::take(extents) {
        let end = extent
            .file_block
            .checked_add(u64::from(extent.block_count))
            .ok_or(XfsError::CorruptMetadata)?;
        if file_block < extent.file_block || file_block >= end {
            next.push(extent);
            continue;
        }
        let before = file_block - extent.file_block;
        if before != 0 {
            next.push(XfsExtent {
                block_count: u32::try_from(before).map_err(|_| XfsError::AddressOutOfRange)?,
                ..extent
            });
        }
        let after = end - file_block - 1;
        if after != 0 {
            next.push(XfsExtent {
                file_block: file_block + 1,
                start_block: extent
                    .start_block
                    .checked_add(before + 1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                block_count: u32::try_from(after).map_err(|_| XfsError::AddressOutOfRange)?,
                ..extent
            });
        }
    }
    if let Some(replacement) = replacement {
        next.push(replacement);
    }
    next.sort_unstable_by_key(|extent| extent.file_block);
    let mut merged: Vec<XfsExtent> = Vec::new();
    for extent in next {
        if let Some(last) = merged.last_mut()
            && last.unwritten == extent.unwritten
            && last.file_block.checked_add(u64::from(last.block_count)) == Some(extent.file_block)
            && last.start_block.checked_add(u64::from(last.block_count)) == Some(extent.start_block)
        {
            last.block_count = last
                .block_count
                .checked_add(extent.block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
        } else {
            merged.push(extent);
        }
    }
    *extents = merged;
    Ok(())
}

fn xfs_planner_for<'a>(
    volume: &XfsVolume,
    planners: &'a mut Vec<XfsAgMutationPlanner>,
    physical: u64,
) -> XfsResult<&'a mut XfsAgMutationPlanner> {
    let ag = u32::try_from(physical / u64::from(volume.superblock.ag_blocks))
        .map_err(|_| XfsError::AddressOutOfRange)?;
    if let Some(index) = planners.iter().position(|entry| entry.ag == ag) {
        return Ok(&mut planners[index]);
    }
    planners.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
    planners.push(XfsAgMutationPlanner::new(volume, ag)?);
    planners.last_mut().ok_or(XfsError::NoMemory)
}

struct XfsLiveLogState {
    ring: XfsLogRing,
    ail: XfsAil,
    next_transaction: u32,
    failed: bool,
}

/// Writable-mount coordinator. It is constructed only with an explicitly
/// recovered log ring; no VFS caller can guess a journal head/tail from a
/// pathname or silently start a second log stream.
pub struct XfsMount {
    volume: Arc<XfsVolume>,
    live: SpinMutex<XfsLiveLogState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RangeRewrite {
    Punch { offset: u64, length: u64 },
    Collapse { offset: u64, length: u64 },
    Insert { offset: u64, length: u64 },
}

impl XfsMount {
    pub(crate) fn new(volume: Arc<XfsVolume>, ring: XfsLogRing) -> XfsResult<Self> {
        if !volume.superblock.is_v5() || ring.blocks() != volume.log_region_blocks()? {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(Self {
            volume,
            live: SpinMutex::new(XfsLiveLogState {
                ring,
                ail: XfsAil::default(),
                next_transaction: 1,
                failed: false,
            }),
        })
    }

    pub(crate) fn volume(&self) -> &Arc<XfsVolume> {
        &self.volume
    }

    fn stage_inode_quota_delta(
        &self,
        inode: &XfsInode,
        block_delta: i64,
        inode_delta: i64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        self.stage_quota_identity_delta(
            inode.number,
            inode.uid,
            inode.gid,
            inode.project_id,
            block_delta,
            inode_delta,
            metadata,
        )
    }

    fn stage_quota_identity_delta(
        &self,
        inode_number: u64,
        uid: u32,
        gid: u32,
        project_id: u32,
        block_delta: i64,
        inode_delta: i64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if !self.volume.has_quota_accounting() {
            return Ok(());
        }
        let roots = self.volume.quota_state()?.roots;
        // Quota files and the root metadata objects are accounting
        // infrastructure, not quota-owned user objects.
        if [roots.user, roots.group, roots.project]
            .into_iter()
            .flatten()
            .any(|root| root == inode_number)
        {
            return Ok(());
        }
        if roots.user.is_some() && self.volume.quota_accounting_enabled(1) {
            self.volume
                .stage_dquot_delta(1, uid, block_delta, inode_delta, metadata)?;
        }
        if roots.group.is_some() && self.volume.quota_accounting_enabled(4) {
            self.volume
                .stage_dquot_delta(4, gid, block_delta, inode_delta, metadata)?;
        }
        if roots.project.is_some() && self.volume.quota_accounting_enabled(2) {
            self.volume
                .stage_dquot_delta(2, project_id, block_delta, inode_delta, metadata)?;
        }
        Ok(())
    }

    fn stage_quota_owner_transfer(
        &self,
        inode: &XfsInode,
        uid: u32,
        gid: u32,
        project_id: u32,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if !self.volume.has_quota_accounting() {
            return Ok(());
        }
        let roots = self.volume.quota_state()?.roots;
        // `d_bcount` and `di_nblocks` are both 512-byte basic blocks.  The
        // transfer must move the complete on-disk ownership charge, including
        // data, attribute, and external-BMBT allocations.
        let blocks = i64::try_from(inode.blocks).map_err(|_| XfsError::AddressOutOfRange)?;
        for (kind, root, old, new) in [
            (1u8, roots.user, inode.uid, uid),
            (4, roots.group, inode.gid, gid),
            (2, roots.project, inode.project_id, project_id),
        ] {
            if root.is_some() && self.volume.quota_accounting_enabled(kind) && old != new {
                self.volume
                    .stage_dquot_delta(kind, old, -blocks, -1, metadata)?;
                self.volume
                    .stage_dquot_delta(kind, new, blocks, 1, metadata)?;
            }
        }
        Ok(())
    }

    fn staged_inode_block_delta(
        &self,
        inode: u64,
        metadata: &XfsMetadataTransaction,
    ) -> XfsResult<i64> {
        let inode_size = usize::from(self.volume.superblock.inode_size);
        for buffer in &metadata.buffers {
            if buffer.metadata_type != XfsMetadataBufferType::Inode
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % inode_size != 0
            {
                continue;
            }
            for (before, after) in buffer
                .before
                .chunks_exact(inode_size)
                .zip(buffer.after.chunks_exact(inode_size))
            {
                if be16(before, 0)? == XFS_DINODE_MAGIC && be64(before, 152)? == inode {
                    let old = be64(before, 64)?;
                    let new = be64(after, 64)?;
                    return if new >= old {
                        i64::try_from(new - old).map_err(|_| XfsError::AddressOutOfRange)
                    } else {
                        i64::try_from(old - new)
                            .map_err(|_| XfsError::AddressOutOfRange)
                            .map(|value| -value)
                    };
                }
            }
        }
        Err(XfsError::CorruptMetadata)
    }

    fn restage_prepared_inode_size(
        &self,
        inode: u64,
        size: u64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode_size = usize::from(self.volume.superblock.inode_size);
        for buffer in &mut metadata.buffers {
            if buffer.metadata_type != XfsMetadataBufferType::Inode
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % inode_size != 0
            {
                continue;
            }
            for (before, after) in buffer
                .before
                .chunks_exact(inode_size)
                .zip(buffer.after.chunks_exact_mut(inode_size))
            {
                if be16(before, 0)? != XFS_DINODE_MAGIC || be64(before, 152)? != inode {
                    continue;
                }
                put_be64(after, 56, size)?;
                rewrite_crc32c(after, 100)?;
                return Ok(());
            }
        }
        Err(XfsError::CorruptMetadata)
    }

    /// Runs one VFS read while excluding publication of every home image in a
    /// live transaction.  The lock deliberately spans decoding as well as the
    /// initial inode lookup: an external directory/attribute tree can require
    /// several home blocks whose individually FUA-complete writes are not a
    /// coherent namespace until the coordinator releases this guard.
    pub(crate) fn read_coherent<T>(
        &self,
        read: impl FnOnce(&XfsVolume) -> XfsResult<T>,
    ) -> XfsResult<T> {
        let _live = self.live.lock();
        read(&self.volume)
    }

    fn push_ail_locked(&self, live: &mut XfsLiveLogState) -> XfsResult<()> {
        let Some(through_lsn) = live.ail.entries().last().map(|entry| entry.lsn) else {
            return Ok(());
        };
        self.volume
            .checkpoint_live_log(&mut live.ring, &mut live.ail, through_lsn, |entry| {
                if entry.checkpoint_homes.is_empty() {
                    return Err(XfsError::CorruptMetadata);
                }
                for (block, image) in &entry.checkpoint_homes {
                    self.volume
                        .write_basic_blocks_fua(&self.volume.data, *block, image)?;
                }
                Ok(())
            })
    }

    /// Forces all ordinary AIL work to durable home blocks and then fences
    /// every member device.  This is the sync path; it intentionally does not
    /// manufacture a clean-unmount marker while callers remain active.
    pub(crate) fn flush_live(&self) -> XfsResult<()> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let result = self
            .push_ail_locked(&mut live)
            .and_then(|()| self.volume.flush());
        if let Err(error) = result {
            live.failed = true;
            return Err(error);
        }
        Ok(())
    }

    /// Writes a terminal XFS unmount record only after every preceding AIL
    /// item has reached its home block and every member is durable.  The
    /// marker itself is log FUA/forced by `persist_live_log_commit`; it carries
    /// no home image, so its ring space becomes reclaimable immediately.
    pub(crate) fn clean_unmount(&self) -> XfsResult<()> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let result = (|| {
            // Force the preceding log before asking the AIL to make any
            // record reclaimable; an external log is intentionally distinct
            // from the data member here.
            self.volume.log_volume()?.flush().map_err(XfsError::from)?;
            self.push_ail_locked(&mut live)?;
            if !live.ail.entries().is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            self.volume.flush()?;
            let transaction = live.next_transaction;
            let operations = [XfsLogOperation {
                transaction_id: transaction,
                // Linux XFS_LOG client and xfs_unmount_log_format magic.
                client_id: 0xaa,
                flags: XLOG_UNMOUNT_TRANS,
                payload: vec![0x55, 0x6e, 0, 0, 0, 0, 0, 0],
            }];
            let tail_lsn = live.ring.tail_lsn();
            let prepared = self.volume.prepare_live_log_commit(
                &mut live.ring,
                transaction,
                tail_lsn,
                &operations,
            )?;
            let end_lsn = prepared.reservation.end_lsn(live.ring.blocks())?;
            self.volume.persist_clean_unmount_record(&prepared)?;
            live.ring.checkpoint_tail(end_lsn)?;
            live.next_transaction = live
                .next_transaction
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
            Ok(())
        })();
        if let Err(error) = result {
            live.failed = true;
            return Err(error);
        }
        Ok(())
    }

    /// Atomically replaces the persistent file-attribute portion of a v3
    /// inode.  Attribute callers cannot publish an inode core without going
    /// through the same recovered live-log coordinator as data and namespace
    /// mutations.
    pub(crate) fn set_file_attr(
        &self,
        inode: u64,
        attr: XfsFileAttr,
        ctime_seconds: i64,
        ctime_nanoseconds: u32,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let current = self.volume.inode(inode)?;
        if current.version < 3 || !self.volume.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }

        let mut metadata = XfsMetadataTransaction::default();
        self.stage_quota_owner_transfer(
            &current,
            current.uid,
            current.gid,
            attr.project_id,
            &mut metadata,
        )?;
        self.volume.stage_file_attr(
            inode,
            attr,
            ctime_seconds,
            ctime_nanoseconds,
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Replaces the supported fixed dinode-core fields in one live XFS
    /// transaction.  The live lock covers sampling the old raw inode, CRC
    /// construction, log commit, and AIL publication; no caller can make a
    /// mode/owner/time subset durable independently of the rest.
    pub(crate) fn update_inode_core(
        &self,
        inode: u64,
        update: XfsInodeCoreUpdate,
    ) -> XfsResult<()> {
        if update.is_empty() {
            return Ok(());
        }
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        let current = self.volume.inode(inode)?;
        if let Some((uid, gid)) = update.owner {
            self.stage_quota_owner_transfer(&current, uid, gid, current.project_id, &mut metadata)?;
        }
        self.volume
            .stage_inode_core_update(inode, update, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Replaces a supported symlink's target under the one live-log lock.
    /// The replacement data is staged in newly allocated blocks before the
    /// log can switch the dinode mapping, so an I/O/log failure cannot expose
    /// a partially overwritten old target.
    pub(crate) fn replace_symlink(
        &self,
        inode: u64,
        target: &[u8],
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let metadata =
            self.volume
                .stage_symlink_replacement(inode, target, seconds, nanoseconds)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Publishes a caller-composed metadata set under the mount's sole log
    /// coordinator.  This is deliberately the only public composition point:
    /// inode, AG and every affected directory image share one transaction id,
    /// one durable log record and one home-write checkpoint.
    pub fn commit_staged(&self, metadata: XfsMetadataTransaction) -> XfsResult<()> {
        let mut live = self.live.lock();
        self.commit_locked(&mut live, &metadata)
    }

    /// Allocates, initializes, and names one regular or directory inode under
    /// the mount log lock.  The inobt/finobt transition, new inode core,
    /// parent namespace image and parent directory link count form one log
    /// record; callers cannot observe an allocated-but-unnamed inode.
    pub fn create_named_inode(
        &self,
        parent: u64,
        name: &[u8],
        initial: XfsNewInode,
        exclusive: bool,
    ) -> XfsResult<XfsNamedInodeOutcome> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut live = self.live.lock();
        let parent_inode = self.volume.inode(parent)?;
        if parent_inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut entries = self.volume.directory_entries(parent)?;
        if let Some(entry) = entries.iter().find(|entry| entry.name == name) {
            if exclusive {
                return Err(XfsError::AddressOutOfRange);
            }
            return Ok(XfsNamedInodeOutcome::Existing(entry.inode));
        }
        let mode = initial.mode;
        let is_directory = mode & 0o170000 == 0o040000;
        let is_symlink = mode & 0o170000 == 0o120000;
        if is_directory != initial.parent.is_some()
            || is_symlink != initial.symlink_target.is_some()
            || (!is_symlink && initial.symlink_target.is_some())
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let (ag, _) = self.volume.split_inode_number(parent)?;
        let remote_blocks = initial
            .symlink_target
            .as_ref()
            .and_then(|target| {
                (target.len() > self.volume.superblock.inode_size as usize - 176).then(|| {
                    u32::try_from(
                        target
                            .len()
                            .div_ceil(self.volume.superblock.block_size as usize),
                    )
                    .ok()
                })
            })
            .flatten();
        let allocation = match remote_blocks {
            Some(blocks) => self
                .volume
                .prepare_inode_allocation_with_remote(ag, blocks)?,
            None => self.volume.prepare_inode_allocation(ag)?,
        };
        let mut metadata = XfsMetadataTransaction::default();
        let (uid, gid, project_id) = (initial.uid, initial.gid, initial.project_id);
        self.volume
            .stage_new_inode(&allocation, initial, &mut metadata)?;
        self.stage_quota_identity_delta(
            allocation.inode,
            uid,
            gid,
            project_id,
            0,
            1,
            &mut metadata,
        )?;
        entries.push(XfsDirectoryEntry {
            name: name.to_vec(),
            inode: allocation.inode,
            file_type: Some(directory_type_for_inode(mode)),
        });
        self.volume.stage_directory_entries_with_parent(
            parent,
            self.volume.directory_parent(parent)?,
            &entries,
            &mut metadata,
        )?;
        if is_directory {
            self.volume.stage_inode_link_count(
                parent,
                parent_inode
                    .nlink
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                &mut metadata,
            )?;
        }
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsNamedInodeOutcome::Created(allocation.inode))
    }

    /// Commits a set of complete directory images atomically.  Shortform
    /// promotions are preflighted first: each needs an AG free-space delta,
    /// and two candidates in one AG cannot be independently selected from
    /// the same on-disk snapshot.  The future leaf/node allocator replaces
    /// that precise conflict with one combined AG planner; until then it is
    /// rejected before any metadata is staged or logged.
    pub fn replace_directories(&self, updates: &[XfsDirectoryUpdate]) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(updates, &[], &[], &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Adds a second hard link for a non-directory inode.  The directory
    /// image and inode-core link count are deliberately staged from the same
    /// locked snapshot, so the name is never durable without its reference.
    pub fn link_named(
        &self,
        directory: u64,
        name: &[u8],
        target: u64,
        expected_generation: u32,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let target_inode = self.volume.inode(target)?;
        if target_inode.generation != expected_generation {
            return Err(XfsError::AddressOutOfRange);
        }
        if target_inode.mode & 0o170000 == 0o040000 || target_inode.nlink == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let links = target_inode
            .nlink
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut entries = self.volume.directory_entries(directory)?;
        if entries.iter().any(|entry| entry.name == name) {
            return Err(XfsError::AddressOutOfRange);
        }
        entries.push(XfsDirectoryEntry {
            name: name.to_vec(),
            inode: target,
            file_type: Some(directory_type_for_inode(target_inode.mode)),
        });
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[],
            &[],
            &mut metadata,
        )?;
        self.volume
            .stage_inode_link_count(target, links, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Removes one non-directory name.  A final regular-file link also
    /// truncates and returns its data extents and inode bit in this exact log
    /// transaction; special inode reclamation is not guessed here.
    pub fn unlink_named(
        &self,
        directory: u64,
        name: &[u8],
        expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let index = entries
            .iter()
            .position(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let target = entries[index].inode;
        let inode = self.volume.inode(target)?;
        if expected
            .is_some_and(|(number, generation)| number != target || generation != inode.generation)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if inode.mode & 0o170000 == 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let links = inode
            .nlink
            .checked_sub(1)
            .ok_or(XfsError::CorruptMetadata)?;
        entries.remove(index);
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[],
            &[],
            &mut metadata,
        )?;
        self.stage_last_unlink(target, &inode, links, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Removes an empty directory.  The child `..`, parent image, parent
    /// nlink, child nlink and inobt/finobt transition share one log record.
    /// External directory trees are intentionally refused until their data,
    /// leaf/node and free-space teardown is implemented as one planner.
    pub fn rmdir_named(
        &self,
        directory: u64,
        name: &[u8],
        expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(name)?;
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let index = entries
            .iter()
            .position(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let target = entries[index].inode;
        let child = self.volume.inode(target)?;
        if expected
            .is_some_and(|(number, generation)| number != target || generation != child.generation)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if child.mode & 0o170000 != 0o040000 || child.nlink != 2 {
            return Err(XfsError::UnsupportedFeature);
        }
        if !self.volume.directory_entries(target)?.is_empty() {
            return Err(XfsError::NotEmpty);
        }
        let parent_inode = self.volume.inode(directory)?;
        let parent_links = parent_inode
            .nlink
            .checked_sub(1)
            .ok_or(XfsError::CorruptMetadata)?;
        entries.remove(index);
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        let teardown = (matches!(
            child.data_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) || matches!(
            child.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ))
        .then_some(target);
        self.stage_directory_updates(
            &[XfsDirectoryUpdate {
                directory,
                parent,
                entries,
            }],
            &[target],
            teardown.as_slice(),
            &mut metadata,
        )?;
        self.volume
            .stage_inode_link_count(directory, parent_links, &mut metadata)?;
        self.volume
            .stage_inode_link_count(target, 0, &mut metadata)?;
        self.volume
            .stage_directory_reclaim_inode(target, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    /// Ordinary (non-exchange, non-whiteout) rename, including replacement.
    /// Every changed directory representation, a moved directory's native
    /// `..`, parent link deltas, and a replaced inode's final reclaim are
    /// composed before the sole log commit.
    pub fn rename_named(
        &self,
        old_parent: u64,
        old_name: &[u8],
        source_expected: (u64, u32),
        new_parent: u64,
        new_name: &[u8],
        destination_expected: Option<(u64, u32)>,
    ) -> XfsResult<()> {
        Self::validate_namespace_name(old_name)?;
        Self::validate_namespace_name(new_name)?;
        let mut live = self.live.lock();
        let mut old_entries = self.volume.directory_entries(old_parent)?;
        let old_index = old_entries
            .iter()
            .position(|entry| entry.name == old_name)
            .ok_or(XfsError::AddressOutOfRange)?;
        let source_entry = old_entries[old_index].clone();
        let source = self.volume.inode(source_entry.inode)?;
        if source.number != source_expected.0 || source.generation != source_expected.1 {
            return Err(XfsError::AddressOutOfRange);
        }
        let source_directory = source.mode & 0o170000 == 0o040000;
        let same_parent = old_parent == new_parent;
        if same_parent && old_name == new_name {
            return Ok(());
        }
        let mut new_entries = if same_parent {
            old_entries.clone()
        } else {
            self.volume.directory_entries(new_parent)?
        };
        let destination = new_entries
            .iter()
            .position(|entry| entry.name == new_name)
            .map(|index| self.volume.inode(new_entries[index].inode))
            .transpose()?;
        match (destination_expected, destination.as_ref()) {
            (None, None) => {}
            (Some((number, generation)), Some(inode))
                if inode.number == number && inode.generation == generation => {}
            _ => return Err(XfsError::AddressOutOfRange),
        }
        if let Some(destination) = &destination {
            let destination_directory = destination.mode & 0o170000 == 0o040000;
            if source_directory != destination_directory {
                return Err(XfsError::UnsupportedFeature);
            }
            if destination_directory
                && (destination.nlink != 2
                    || !self
                        .volume
                        .directory_entries(destination.number)?
                        .is_empty()
                    || !matches!(
                        destination.data_format,
                        XfsForkFormat::Local | XfsForkFormat::Extents | XfsForkFormat::Btree
                    ))
            {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        if source_directory && !same_parent {
            self.reject_directory_cycle(source.number, new_parent)?;
        }

        // Remove the old name first.  For a same-parent rename use one image
        // so an old/new collision cannot create two copies of the same slot.
        old_entries.remove(old_index);
        if same_parent {
            new_entries = old_entries.clone();
        }
        if let Some(index) = new_entries.iter().position(|entry| entry.name == new_name) {
            new_entries[index] = XfsDirectoryEntry {
                name: new_name.to_vec(),
                inode: source.number,
                file_type: Some(directory_type_for_inode(source.mode)),
            };
        } else {
            new_entries.push(XfsDirectoryEntry {
                name: new_name.to_vec(),
                inode: source.number,
                file_type: Some(directory_type_for_inode(source.mode)),
            });
        }

        let old_parent_of_directory = self.volume.directory_parent(old_parent)?;
        let new_parent_of_directory = if same_parent {
            old_parent_of_directory
        } else {
            self.volume.directory_parent(new_parent)?
        };
        let mut updates = Vec::new();
        updates.push(XfsDirectoryUpdate {
            directory: old_parent,
            parent: old_parent_of_directory,
            entries: old_entries,
        });
        if !same_parent {
            updates.push(XfsDirectoryUpdate {
                directory: new_parent,
                parent: new_parent_of_directory,
                entries: new_entries,
            });
        } else {
            updates[0].entries = new_entries;
        }
        if source_directory && !same_parent {
            updates.push(XfsDirectoryUpdate {
                directory: source.number,
                parent: new_parent,
                entries: self.volume.directory_entries(source.number)?,
            });
        }

        let old_parent_inode = self.volume.inode(old_parent)?;
        let new_parent_inode = if same_parent {
            old_parent_inode.clone()
        } else {
            self.volume.inode(new_parent)?
        };
        let mut old_links = old_parent_inode.nlink;
        let mut new_links = new_parent_inode.nlink;
        if source_directory && !same_parent {
            old_links = old_links.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
            new_links = new_links
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        let freed = destination
            .as_ref()
            .filter(|inode| inode.mode & 0o170000 == 0o040000)
            .map(|inode| vec![inode.number])
            .unwrap_or_default();
        let teardown = destination
            .as_ref()
            .filter(|inode| {
                inode.mode & 0o170000 == 0o040000
                    && (matches!(
                        inode.data_format,
                        XfsForkFormat::Extents | XfsForkFormat::Btree
                    ) || matches!(
                        inode.attr_format,
                        XfsForkFormat::Extents | XfsForkFormat::Btree
                    ))
            })
            .map(|inode| inode.number);
        self.stage_directory_updates(&updates, &freed, teardown.as_slice(), &mut metadata)?;
        if let Some(destination) = destination {
            if destination.mode & 0o170000 == 0o040000 {
                // Removing the replaced directory removes one subdirectory
                // from new_parent; the source move above adds its replacement.
                new_links = new_links.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
                self.volume
                    .stage_inode_link_count(destination.number, 0, &mut metadata)?;
                self.volume
                    .stage_directory_reclaim_inode(destination.number, &mut metadata)?;
            } else {
                let links = destination
                    .nlink
                    .checked_sub(1)
                    .ok_or(XfsError::CorruptMetadata)?;
                self.stage_last_unlink(destination.number, &destination, links, &mut metadata)?;
            }
        }
        if same_parent {
            if old_links != old_parent_inode.nlink || new_links != old_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(old_parent, new_links, &mut metadata)?;
            }
        } else {
            if old_links != old_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(old_parent, old_links, &mut metadata)?;
            }
            if new_links != new_parent_inode.nlink {
                self.volume
                    .stage_inode_link_count(new_parent, new_links, &mut metadata)?;
            }
        }
        self.commit_locked(&mut live, &metadata)
    }

    fn validate_namespace_name(name: &[u8]) -> XfsResult<()> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        Ok(())
    }

    fn reject_directory_cycle(&self, source: u64, mut parent: u64) -> XfsResult<()> {
        for _ in 0..=self.volume.superblock.ag_count {
            if parent == source {
                return Err(XfsError::AddressOutOfRange);
            }
            let next = self.volume.directory_parent(parent)?;
            if next == parent {
                return Ok(());
            }
            parent = next;
        }
        Err(XfsError::CorruptMetadata)
    }

    fn stage_last_unlink(
        &self,
        inode_number: u64,
        inode: &XfsInode,
        links: u32,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if links != 0 {
            return self
                .volume
                .stage_inode_link_count(inode_number, links, metadata);
        }
        if inode.mode & 0o170000 != 0o100000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let reclaim = self.volume.prepare_regular_truncate(inode_number, 0)?;
        metadata.buffers.extend(reclaim.buffers);
        self.volume
            .stage_inode_link_count(inode_number, 0, metadata)?;
        metadata
            .buffers
            .extend(self.volume.prepare_inode_free(inode_number)?.buffers);
        self.stage_inode_quota_delta(
            inode,
            -i64::try_from(inode.blocks).map_err(|_| XfsError::AddressOutOfRange)?,
            -1,
            metadata,
        )?;
        Ok(())
    }

    fn stage_directory_updates(
        &self,
        updates: &[XfsDirectoryUpdate],
        free_inodes: &[u64],
        teardown: &[u64],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if updates.is_empty()
            || updates.iter().enumerate().any(|(index, update)| {
                update.directory == 0
                    || updates[..index]
                        .iter()
                        .any(|prior| prior.directory == update.directory)
            })
        {
            return Err(XfsError::AddressOutOfRange);
        }
        // Each tuple is (update index, inode AG, requested blocks, external).
        // A single AG snapshot must serve every directory changed by one
        // rename: independently staged AGF/AGFL images cannot compose.
        let mut reserved = Vec::<(usize, u32, u32, bool)>::new();
        let mut releases = Vec::<u64>::new();
        for (index, update) in updates.iter().enumerate() {
            let inode = self.volume.inode(update.directory)?;
            if inode.data_format == XfsForkFormat::Local {
                let mut probe = XfsMetadataTransaction::default();
                match self.volume.stage_shortform_directory(
                    update.directory,
                    update.parent,
                    &update.entries,
                    &mut probe,
                ) {
                    Ok(()) => {}
                    Err(XfsError::AddressOutOfRange) => {
                        let (ag, _) = self.volume.split_inode_number(update.directory)?;
                        let blocks = u32::try_from(
                            self.volume.directory_block_size()?
                                / self.volume.superblock.block_size as usize,
                        )
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                        reserved.push((index, ag, blocks, false));
                    }
                    Err(error) => return Err(error),
                }
            } else if matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            ) {
                let (ag, _) = self.volume.split_inode_number(update.directory)?;
                let blocks = self.volume.directory_rebuild_blocks(
                    update.directory,
                    update.parent,
                    &update.entries,
                )?;
                releases.extend(self.volume.directory_rebuild_releases(update.directory)?);
                reserved.push((index, ag, blocks, true));
            } else {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        for inode in teardown {
            releases.extend(self.volume.directory_teardown_releases(*inode)?);
        }
        let mut batches = Vec::<(u32, Vec<XfsExtentAllocation>)>::new();
        for (_, ag, _, _) in &reserved {
            if batches.iter().any(|(present, _)| present == ag) {
                continue;
            }
            let requests = reserved
                .iter()
                .filter(|(_, candidate, _, _)| candidate == ag)
                .map(|(_, _, blocks, _)| *blocks)
                .collect::<Vec<_>>();
            let batch = self.volume.prepare_extent_allocations(*ag, &requests)?;
            let allocations = batch.allocations;
            let pairs = allocations
                .iter()
                .map(|allocation| (allocation.start_block, allocation.block_count))
                .collect::<Vec<_>>();
            self.volume.stage_directory_rebuild_allocator_delta(
                *ag,
                &pairs,
                &releases,
                free_inodes,
                metadata,
            )?;
            batches.push((*ag, allocations));
        }
        // Return old external blocks belonging to AGs without a replacement
        // allocation (a cross-AG move can make that happen) through their
        // own single canonical free-space delta.
        for physical in &releases {
            let ag = u32::try_from(*physical / self.volume.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if !batches.iter().any(|(present, _)| *present == ag) {
                self.volume.stage_directory_rebuild_allocator_delta(
                    ag,
                    &[],
                    &releases,
                    free_inodes,
                    metadata,
                )?;
                batches.push((ag, Vec::new()));
            }
        }
        for inode in free_inodes {
            let (ag, _) = self.volume.split_inode_number(*inode)?;
            if !batches.iter().any(|(present, _)| *present == ag) {
                self.volume.stage_directory_rebuild_allocator_delta(
                    ag,
                    &[],
                    &releases,
                    free_inodes,
                    metadata,
                )?;
                batches.push((ag, Vec::new()));
            }
        }
        for (index, update) in updates.iter().enumerate() {
            if let Some((_, ag, _, external)) = reserved
                .iter()
                .find(|(candidate, _, _, _)| *candidate == index)
            {
                let ordinal = reserved
                    .iter()
                    .take_while(|(candidate, _, _, _)| *candidate != index)
                    .filter(|(_, candidate, _, _)| candidate == ag)
                    .count();
                let allocation = batches
                    .iter()
                    .find(|(candidate, _)| candidate == ag)
                    .and_then(|(_, allocations)| allocations.get(ordinal))
                    .ok_or(XfsError::CorruptMetadata)?;
                if *external {
                    self.volume.stage_directory_block_with_reservation(
                        update.directory,
                        update.parent,
                        &update.entries,
                        allocation,
                        metadata,
                    )?;
                } else {
                    self.volume.stage_shortform_directory_promotion(
                        update.directory,
                        update.parent,
                        &update.entries,
                        allocation,
                        metadata,
                    )?;
                }
            } else {
                self.volume.stage_directory_entries_with_parent(
                    update.directory,
                    update.parent,
                    &update.entries,
                    metadata,
                )?;
            }
        }
        Ok(())
    }

    fn commit_locked(
        &self,
        live: &mut XfsLiveLogState,
        metadata: &XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if live.failed {
            return Err(XfsError::Io);
        }
        let transaction = live.next_transaction;
        let result = self
            .volume
            .commit_metadata_transaction(&mut live.ring, &mut live.ail, transaction, metadata)
            .map(|_| ());
        match result {
            Ok(()) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(())
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    pub fn read_at(&self, inode: u64, offset: u64, output: &mut [u8]) -> XfsResult<usize> {
        self.read_coherent(|volume| volume.read_inode_at(inode, offset, output))
    }

    pub fn shortform_xattrs(&self, inode: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let _live = self.live.lock();
        self.volume.shortform_xattrs(inode)
    }

    /// Takes a coherent xattr snapshot irrespective of the fork format.  The
    /// live-log lock makes lookup/list and a following mutation see the same
    /// committed image, rather than mixing a leaf before a journal commit
    /// with an inode after it.
    pub fn xattrs(&self, inode: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let _live = self.live.lock();
        self.volume.xattrs(inode)
    }

    pub fn write_at(&self, inode: u64, offset: u64, data: &[u8]) -> XfsResult<usize> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let transaction = live.next_transaction;
        let owner = self.volume.inode(inode)?;
        let mut prepared = self
            .volume
            .prepare_regular_write(inode, offset, data.len())?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume.write_prepared_regular_at_live(
                ring,
                ail,
                transaction,
                prepared,
                offset,
                data,
            )
        };
        match result {
            Ok(written) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(written)
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Samples EOF and commits the write under one live-log critical section;
    /// two concurrent O_APPEND writers therefore receive distinct offsets.
    pub fn append(&self, inode: u64, data: &[u8]) -> XfsResult<(usize, u64)> {
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let offset = owner.size;
        let transaction = live.next_transaction;
        let mut prepared = self
            .volume
            .prepare_regular_write(inode, offset, data.len())?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume.write_prepared_regular_at_live(
                ring,
                ail,
                transaction,
                prepared,
                offset,
                data,
            )
        };
        match result {
            Ok(written) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok((written, offset))
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Replaces the normal allocator image for a write which touches a
    /// reflink-shared block.  A shared extent is never modified in place:
    /// every new data/BMBT home, rmap owner transition, refcount transition,
    /// free-space transition and final inode mapping is derived from the same
    /// per-AG planner and committed in one log transaction.
    fn rewrite_shared_write_as_cow(
        &self,
        inode: &XfsInode,
        prepared: &mut XfsRegularWrite,
    ) -> XfsResult<()> {
        if !self.volume.superblock.features.has_rmapbt()
            || !self.volume.superblock.features.has_reflink()
        {
            return Ok(());
        }
        let block_size = u64::from(self.volume.superblock.block_size);
        let first = prepared.offset / block_size;
        let last = prepared
            .offset
            .checked_add(prepared.length as u64)
            .ok_or(XfsError::AddressOutOfRange)?
            .checked_sub(1)
            .ok_or(XfsError::AddressOutOfRange)?
            / block_size;
        let old = match inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(inode.number)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(inode.number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let shared = (first..=last).any(|file_block| {
            xfs_extent_at(&old, file_block)
                .ok()
                .flatten()
                .is_some_and(|extent| {
                    let ag =
                        (extent.start_block / u64::from(self.volume.superblock.ag_blocks)) as u32;
                    self.volume
                        .refcount_records(ag)
                        .ok()
                        .is_some_and(|records| {
                            records.iter().any(|record| {
                                let local = (extent.start_block
                                    % u64::from(self.volume.superblock.ag_blocks))
                                    as u32;
                                local >= record.start_block
                                    && local < record.start_block.saturating_add(record.block_count)
                                    && record.refcount > 1
                            })
                        })
                })
        });
        let has_hole = (first..=last)
            .any(|file_block| xfs_extent_at(&old, file_block).ok().flatten().is_none());
        if !shared && !has_hole {
            return Ok(());
        }

        let mut planners = Vec::<XfsAgMutationPlanner>::new();
        let mut mappings = prepared.mappings.clone();
        let mut copies = Vec::new();
        for file_block in first..=last {
            let original = xfs_extent_at(&old, file_block)?;
            let current = xfs_extent_at(&mappings, file_block)?.ok_or(XfsError::CorruptMetadata)?;
            if let Some(source) = original {
                let old_plan = xfs_planner_for(&self.volume, &mut planners, source.start_block)?;
                let old_local =
                    u32::try_from(source.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                // Refcount and rmap must agree that this inode owns the old
                // block before it can be retired.  This rejects corrupt
                // one-sided sharing metadata rather than silently losing an
                // owner during COW.
                let count = old_plan.refcount_at(old_local)?;
                if count <= 1 {
                    continue;
                }
                old_plan.remove_owner(old_local, inode.number, file_block)?;
                old_plan.set_refcount(old_local, count - 1)?;
                let new_local = old_plan.claim_free_block()?;
                old_plan.add_owner(new_local, inode.number, file_block)?;
                let new_physical = u64::from(old_plan.ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(new_local)))
                    .ok_or(XfsError::AddressOutOfRange)?;
                xfs_replace_one_mapping(
                    &mut mappings,
                    file_block,
                    Some(XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: new_physical,
                        block_count: 1,
                    }),
                )?;
                copies.push((source.start_block, new_physical));
            } else {
                // A hole was selected by the ordinary allocator while
                // preparing this write.  Claim that exact home in the same
                // planner and publish its rmap alongside the COW changes.
                let plan = xfs_planner_for(&self.volume, &mut planners, current.start_block)?;
                let local = u32::try_from(
                    current.start_block % u64::from(self.volume.superblock.ag_blocks),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                plan.claim_specific_free_block(local)?;
                plan.add_owner(local, inode.number, file_block)?;
            }
        }

        let mut all = old.clone();
        for mapping in &mappings {
            xfs_replace_one_mapping(&mut all, mapping.file_block, Some(*mapping))?;
        }
        let old_bmap = if inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(inode.number)?
        } else {
            Vec::new()
        };
        let (raw_inode, inode_bytes) = self.volume.inode_and_bytes(inode.number)?;
        let required = bmap_external_blocks(
            self.volume.superblock,
            raw_inode.data_fork(&inode_bytes)?.len(),
            all.len(),
        )?;
        let reused = required.min(old_bmap.len());
        let inode_ag = self.volume.split_inode_number(inode.number)?.0;
        let mut bmap = old_bmap[..reused].to_vec();
        let mut excluded = copies
            .iter()
            .map(|(_, new)| {
                (
                    u32::try_from(*new / u64::from(self.volume.superblock.ag_blocks)).unwrap_or(0),
                    u32::try_from(*new % u64::from(self.volume.superblock.ag_blocks)).unwrap_or(0),
                    1,
                )
            })
            .collect::<Vec<_>>();
        for extent in &prepared.allocated {
            excluded.push((
                u32::try_from(extent.start_block / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?,
                u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?,
                extent.block_count,
            ));
        }
        let fresh = self.volume.reserve_bmap_metadata_blocks(
            inode_ag,
            required.saturating_sub(reused),
            &excluded,
        )?;
        for physical in &fresh {
            let plan = xfs_planner_for(&self.volume, &mut planners, *physical)?;
            let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            plan.claim_specific_free_block(local)?;
            plan.add_owner(local, inode.number, XFS_RMAP_OFF_BMBT)?;
        }
        bmap.extend_from_slice(&fresh);
        for physical in old_bmap.iter().copied().skip(reused) {
            let plan = xfs_planner_for(&self.volume, &mut planners, physical)?;
            let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            plan.remove_owner(local, inode.number, XFS_RMAP_OFF_BMBT)?;
            plan.release_free_block(local)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        for plan in &planners {
            metadata
                .buffers
                .extend(self.volume.stage_reflink_ag_plan(plan)?.buffers);
        }
        let end = prepared
            .offset
            .checked_add(prepared.length as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if required == 0 {
            self.volume.stage_regular_inode_extents(
                inode.number,
                all,
                inode.size.max(end),
                &mut metadata,
            )?;
        } else {
            self.volume.stage_regular_inode_bmap(
                inode.number,
                all,
                inode.size.max(end),
                &bmap,
                &mut metadata,
            )?;
        }
        prepared.mappings = mappings;
        prepared.copy_before_write = copies;
        prepared.metadata = metadata;
        Ok(())
    }

    pub fn truncate(&self, inode: u64, size: u64) -> XfsResult<()> {
        let mut live = self.live.lock();
        let owner = self.volume.inode(inode)?;
        let mut transaction_data = self.volume.prepare_regular_truncate(inode, size)?;
        let blocks = self.staged_inode_block_delta(inode, &transaction_data)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut transaction_data)?;
        self.commit_locked(&mut live, &transaction_data)
    }

    pub fn fallocate(
        &self,
        inode: u64,
        offset: u64,
        length: u64,
        keep_size: bool,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let owner = self.volume.inode(inode)?;
        let mut prepared = self
            .volume
            .prepare_regular_fallocate(inode, offset, length, keep_size)?;
        if prepared.metadata.buffers.is_empty() {
            return Ok(());
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        self.commit_locked(&mut live, &prepared.metadata)
    }

    /// Makes every shared block in a range private without changing file
    /// contents or logical allocation.  The prepared write supplies the
    /// exact extent/BMBT shape; `rewrite_shared_write_as_cow` replaces only
    /// shared physical homes and retains the copied data ordering invariant.
    pub fn unshare_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        let block = u64::from(self.volume.superblock.block_size);
        if length == 0 || offset % block != 0 || length % block != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let end = offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        if end > owner.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut prepared = self.volume.prepare_regular_write(
            inode,
            offset,
            usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        if prepared.copy_before_write.is_empty() {
            return Ok(());
        }
        // Unlike an ordinary write, this has no user payload.  The COW data
        // homes must still be copied and flushed before their mapping is
        // logged, but `commit_metadata_transaction` owns that ordering for
        // staged data writes.
        for (old, new) in &prepared.copy_before_write {
            prepared.metadata.data_writes.push(XfsStagedDataWrite {
                fs_block: *new,
                before: self.volume.read_data_fs_block(*new)?,
                after: self.volume.read_data_fs_block(*old)?,
            });
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        self.commit_locked(&mut live, &prepared.metadata)
    }

    /// Zeroes the complete request in one prepared write transaction.  In
    /// particular, do not turn this into a loop of ordinary writes: a caller
    /// must never observe the first half zeroed while the second half still
    /// carries old data after a failed range operation.
    pub fn zero_range(
        &self,
        inode: u64,
        offset: u64,
        length: u64,
        keep_size: bool,
    ) -> XfsResult<()> {
        if length == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let end = offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        let length = usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?;
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let owner = self.volume.inode(inode)?;
        let mut prepared = self.volume.prepare_regular_write(inode, offset, length)?;
        self.rewrite_shared_write_as_cow(&owner, &mut prepared)?;
        // `prepare_regular_write` grows EOF for normal writes.  KEEP_SIZE
        // keeps its already-prepared extent/BMBT images, replacing only the
        // final dinode EOF in that same transaction.
        if keep_size && end > owner.size {
            self.restage_prepared_inode_size(inode, owner.size, &mut prepared.metadata)?;
        }
        let blocks = self.staged_inode_block_delta(inode, &prepared.metadata)?;
        self.stage_inode_quota_delta(&owner, blocks, 0, &mut prepared.metadata)?;
        let transaction = live.next_transaction;
        let result = {
            let XfsLiveLogState { ring, ail, .. } = &mut *live;
            self.volume
                .zero_prepared_regular_at_live(ring, ail, transaction, prepared, offset)
        };
        match result {
            Ok(_) => {
                live.next_transaction = live
                    .next_transaction
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                Ok(())
            }
            Err(error) => {
                live.failed = true;
                Err(error)
            }
        }
    }

    /// Punches whole interior blocks from the fork and frees (or unshares)
    /// their physical homes in the very same transaction.  Unaligned edge
    /// blocks retain their mapping and are zeroed through the write/COW path.
    pub fn punch_hole(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Punch { offset, length })
    }

    /// Collapse is a logical extent-map translation.  Block-aligned suffixes
    /// retain their physical blocks; only rmap logical offsets move.
    pub fn collapse_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Collapse { offset, length })
    }

    /// Insert is likewise a logical mapping translation; the inserted range
    /// is sparse, so it reads as zero without fabricating data blocks.
    pub fn insert_range(&self, inode: u64, offset: u64, length: u64) -> XfsResult<()> {
        self.rewrite_block_range(inode, RangeRewrite::Insert { offset, length })
    }

    /// Builds the entire result fork before touching the log.  This is the
    /// range-operation counterpart to the reflink planner: the allocator,
    /// rmap/refcount ownership view, BMBT homes and dinode image are all
    /// derived from one locked snapshot and published by one commit.
    fn rewrite_block_range(&self, inode_number: u64, operation: RangeRewrite) -> XfsResult<()> {
        let block_size = u64::from(self.volume.superblock.block_size);
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let inode = self.volume.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o100000 {
            return Err(XfsError::UnsupportedFeature);
        }
        // Every validation which depends on EOF is deliberately after the
        // coordinator lock.  A concurrent truncate cannot turn a previously
        // checked collapse/insert/punch into an out-of-range map mutation.
        let (first, end, operation) = match operation {
            RangeRewrite::Punch { offset, length } => {
                if length == 0 || offset >= inode.size {
                    return Err(XfsError::AddressOutOfRange);
                }
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?
                    .min(inode.size);
                (
                    offset.div_ceil(block_size),
                    end / block_size,
                    RangeRewrite::Punch {
                        offset,
                        length: end - offset,
                    },
                )
            }
            RangeRewrite::Collapse { offset, length } => {
                if length == 0 || offset % block_size != 0 || length % block_size != 0 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                if end > inode.size {
                    return Err(XfsError::AddressOutOfRange);
                }
                (
                    offset / block_size,
                    end / block_size,
                    RangeRewrite::Collapse { offset, length },
                )
            }
            RangeRewrite::Insert { offset, length } => {
                if length == 0
                    || offset % block_size != 0
                    || length % block_size != 0
                    || offset >= inode.size
                {
                    return Err(XfsError::AddressOutOfRange);
                }
                inode
                    .size
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                (
                    offset / block_size,
                    end / block_size,
                    RangeRewrite::Insert { offset, length },
                )
            }
        };
        let old = match inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let old_size_blocks = inode.size.div_ceil(block_size);
        if !matches!(operation, RangeRewrite::Insert { .. }) && end > old_size_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let delta = match operation {
            RangeRewrite::Punch { .. } => end.saturating_sub(first),
            RangeRewrite::Collapse { .. } | RangeRewrite::Insert { .. } => {
                end.checked_sub(first).ok_or(XfsError::AddressOutOfRange)?
            }
        };
        let mut rewritten = Vec::new();
        let mut released = Vec::<XfsExtent>::new();
        let mut moved = Vec::<(u64, u64, u64)>::new();
        for extent in old.iter().copied() {
            for index in 0..u64::from(extent.block_count) {
                let old_file = extent
                    .file_block
                    .checked_add(index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let physical = extent
                    .start_block
                    .checked_add(index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let next_file = match operation {
                    RangeRewrite::Punch { .. } if old_file >= first && old_file < end => {
                        released.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        released.push(XfsExtent {
                            unwritten: extent.unwritten,
                            file_block: old_file,
                            start_block: physical,
                            block_count: 1,
                        });
                        continue;
                    }
                    RangeRewrite::Punch { .. } => old_file,
                    RangeRewrite::Collapse { .. } if old_file >= first && old_file < end => {
                        released.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        released.push(XfsExtent {
                            unwritten: extent.unwritten,
                            file_block: old_file,
                            start_block: physical,
                            block_count: 1,
                        });
                        continue;
                    }
                    RangeRewrite::Collapse { .. } if old_file >= end => old_file
                        .checked_sub(delta)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    RangeRewrite::Collapse { .. } => old_file,
                    RangeRewrite::Insert { .. } if old_file >= first => old_file
                        .checked_add(delta)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    RangeRewrite::Insert { .. } => old_file,
                };
                if next_file != old_file {
                    moved.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    moved.push((physical, old_file, next_file));
                }
                rewritten.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                push_merged_extent(
                    &mut rewritten,
                    XfsExtent {
                        unwritten: extent.unwritten,
                        file_block: next_file,
                        start_block: physical,
                        block_count: 1,
                    },
                )?;
            }
        }
        let new_size = match operation {
            RangeRewrite::Punch { .. } => inode.size,
            RangeRewrite::Collapse { .. } => inode
                .size
                .checked_sub(
                    delta
                        .checked_mul(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::AddressOutOfRange)?,
            RangeRewrite::Insert { .. } => inode
                .size
                .checked_add(
                    delta
                        .checked_mul(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::AddressOutOfRange)?,
        };
        let old_bmap = if inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(inode_number)?
        } else {
            Vec::new()
        };
        let (_, raw_inode) = self.volume.inode_and_bytes(inode_number)?;
        let required = bmap_external_blocks(
            self.volume.superblock,
            inode.data_fork(&raw_inode)?.len(),
            rewritten.len(),
        )?;
        let reused = required.min(old_bmap.len());
        let inode_ag = self.volume.split_inode_number(inode_number)?.0;
        let new_bmap = self.volume.reserve_bmap_metadata_blocks(
            inode_ag,
            required.saturating_sub(reused),
            &[],
        )?;
        let mut bmap = Vec::new();
        bmap.try_reserve_exact(
            reused
                .checked_add(new_bmap.len())
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .map_err(|_| XfsError::NoMemory)?;
        bmap.extend_from_slice(&old_bmap[..reused]);
        bmap.extend_from_slice(&new_bmap);
        let mut metadata = XfsMetadataTransaction::default();
        let mut boundary_data = Vec::<XfsStagedDataWrite>::new();
        if self.volume.superblock.features.has_rmapbt()
            && self.volume.superblock.features.has_reflink()
        {
            let mut planners = Vec::<XfsAgMutationPlanner>::new();
            for extent in &released {
                let planner = xfs_planner_for(&self.volume, &mut planners, extent.start_block)?;
                let local =
                    u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                let count = planner.refcount_at(local)?;
                planner.remove_owner(local, inode_number, extent.file_block)?;
                if count > 1 {
                    planner.set_refcount(local, count - 1)?;
                } else {
                    planner.release_free_block(local)?;
                }
            }
            for (physical, old_file, new_file) in &moved {
                let planner = xfs_planner_for(&self.volume, &mut planners, *physical)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.remove_owner(local, inode_number, *old_file)?;
                planner.add_owner(local, inode_number, *new_file)?;
            }
            if let RangeRewrite::Punch { offset, length } = operation {
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                self.stage_punch_boundary_data(
                    inode_number,
                    &old,
                    &mut rewritten,
                    offset,
                    end,
                    &mut planners,
                    &mut boundary_data,
                )?;
            }
            for physical in &new_bmap {
                let planner = xfs_planner_for(&self.volume, &mut planners, *physical)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.claim_specific_free_block(local)?;
                planner.add_owner(local, inode_number, XFS_RMAP_OFF_BMBT)?;
            }
            for physical in old_bmap.iter().copied().skip(reused) {
                let planner = xfs_planner_for(&self.volume, &mut planners, physical)?;
                let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                planner.remove_owner(local, inode_number, XFS_RMAP_OFF_BMBT)?;
                planner.release_free_block(local)?;
            }
            for planner in &planners {
                let staged = self.volume.stage_reflink_ag_plan(planner)?;
                metadata
                    .buffers
                    .try_reserve(staged.buffers.len())
                    .map_err(|_| XfsError::NoMemory)?;
                metadata.buffers.extend(staged.buffers);
            }
        } else {
            let mut groups = Vec::<(u32, Vec<(u32, u32)>, Vec<u32>)>::new();
            for physical in &new_bmap {
                let ag = u32::try_from(*physical / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let local = u32::try_from(*physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, allocated, _)) = groups.iter_mut().find(|(item, _, _)| *item == ag)
                {
                    allocated.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    allocated.push((local, 1));
                } else {
                    let mut allocated = Vec::new();
                    allocated.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    allocated.push((local, 1));
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, allocated, Vec::new()));
                }
            }
            for extent in &released {
                let ag =
                    u32::try_from(extent.start_block / u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                let local =
                    u32::try_from(extent.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, _, free)) = groups.iter_mut().find(|(item, _, _)| *item == ag) {
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                } else {
                    let mut free = Vec::new();
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, Vec::new(), free));
                }
            }
            for physical in old_bmap.iter().copied().skip(reused) {
                let ag = u32::try_from(physical / u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, _, free)) = groups.iter_mut().find(|(item, _, _)| *item == ag) {
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                } else {
                    let mut free = Vec::new();
                    free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    free.push(local);
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, Vec::new(), free));
                }
            }
            for (ag, allocated, free) in groups {
                let staged = self.volume.stage_extent_delta(ag, &allocated, &free)?;
                metadata
                    .buffers
                    .try_reserve(staged.buffers.len())
                    .map_err(|_| XfsError::NoMemory)?;
                metadata.buffers.extend(staged.buffers);
            }
            if let RangeRewrite::Punch { offset, length } = operation {
                let end = offset
                    .checked_add(length)
                    .ok_or(XfsError::AddressOutOfRange)?;
                self.stage_punch_boundary_data_unshared(&old, offset, end, &mut boundary_data)?;
            }
        }
        if required == 0 {
            self.volume.stage_regular_inode_extents(
                inode_number,
                rewritten,
                new_size,
                &mut metadata,
            )?;
        } else {
            self.volume.stage_regular_inode_bmap(
                inode_number,
                rewritten,
                new_size,
                &bmap,
                &mut metadata,
            )?;
        }
        metadata.data_writes = boundary_data;
        let quota = self.staged_inode_block_delta(inode_number, &metadata)?;
        self.stage_inode_quota_delta(&inode, quota, 0, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    fn punch_boundary_blocks(&self, offset: u64, end: u64) -> XfsResult<Vec<u64>> {
        let block = u64::from(self.volume.superblock.block_size);
        let mut blocks = Vec::new();
        if offset % block != 0 {
            blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            blocks.push(offset / block);
        }
        if end % block != 0 {
            let last = end.checked_sub(1).ok_or(XfsError::AddressOutOfRange)? / block;
            if !blocks.contains(&last) {
                blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                blocks.push(last);
            }
        }
        Ok(blocks)
    }

    fn zero_punch_image(
        &self,
        image: &mut [u8],
        file_block: u64,
        offset: u64,
        end: u64,
    ) -> XfsResult<()> {
        let block = u64::from(self.volume.superblock.block_size);
        let start = file_block
            .checked_mul(block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let finish = start
            .checked_add(block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let begin = offset.max(start);
        let limit = end.min(finish);
        if begin >= limit {
            return Err(XfsError::CorruptMetadata);
        }
        let from = usize::try_from(begin - start).map_err(|_| XfsError::AddressOutOfRange)?;
        let to = usize::try_from(limit - start).map_err(|_| XfsError::AddressOutOfRange)?;
        image
            .get_mut(from..to)
            .ok_or(XfsError::CorruptMetadata)?
            .fill(0);
        Ok(())
    }

    /// Produces boundary RMW images while the reflink planner still owns the
    /// old/new physical homes.  A shared boundary is copied before its
    /// mapping is published, exactly like a normal COW write.
    fn stage_punch_boundary_data(
        &self,
        inode: u64,
        old: &[XfsExtent],
        rewritten: &mut Vec<XfsExtent>,
        offset: u64,
        end: u64,
        planners: &mut Vec<XfsAgMutationPlanner>,
        output: &mut Vec<XfsStagedDataWrite>,
    ) -> XfsResult<()> {
        for file_block in self.punch_boundary_blocks(offset, end)? {
            let Some(mapping) = xfs_extent_at(old, file_block)? else {
                continue;
            };
            let old_physical = mapping.start_block;
            let old_local =
                u32::try_from(old_physical % u64::from(self.volume.superblock.ag_blocks))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
            let shared =
                xfs_planner_for(&self.volume, planners, old_physical)?.refcount_at(old_local)? > 1;
            let physical = if shared {
                {
                    let planner = xfs_planner_for(&self.volume, planners, old_physical)?;
                    planner.remove_owner(old_local, inode, file_block)?;
                    let count = planner.refcount_at(old_local)?;
                    planner.set_refcount(
                        old_local,
                        count.checked_sub(1).ok_or(XfsError::CorruptMetadata)?,
                    )?;
                }
                let (ag, local) = {
                    let planner = xfs_planner_for(&self.volume, planners, old_physical)?;
                    (planner.ag, planner.claim_free_block()?)
                };
                let new_physical = u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(local)))
                    .ok_or(XfsError::AddressOutOfRange)?;
                xfs_planner_for(&self.volume, planners, new_physical)?
                    .add_owner(local, inode, file_block)?;
                xfs_replace_one_mapping(
                    rewritten,
                    file_block,
                    Some(XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: new_physical,
                        block_count: 1,
                    }),
                )?;
                new_physical
            } else {
                old_physical
            };
            let before = self.volume.read_data_fs_block(physical)?;
            let mut after = if shared {
                self.volume.read_data_fs_block(old_physical)?
            } else {
                before.clone()
            };
            self.zero_punch_image(&mut after, file_block, offset, end)?;
            output.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            output.push(XfsStagedDataWrite {
                fs_block: physical,
                before,
                after,
            });
        }
        Ok(())
    }

    fn stage_punch_boundary_data_unshared(
        &self,
        old: &[XfsExtent],
        offset: u64,
        end: u64,
        output: &mut Vec<XfsStagedDataWrite>,
    ) -> XfsResult<()> {
        for file_block in self.punch_boundary_blocks(offset, end)? {
            let Some(mapping) = xfs_extent_at(old, file_block)? else {
                continue;
            };
            let before = self.volume.read_data_fs_block(mapping.start_block)?;
            let mut after = before.clone();
            self.zero_punch_image(&mut after, file_block, offset, end)?;
            output.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            output.push(XfsStagedDataWrite {
                fs_block: mapping.start_block,
                before,
                after,
            });
        }
        Ok(())
    }

    fn reflink_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        dedupe: bool,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<bool> {
        let block_size = u64::from(self.volume.superblock.block_size);
        if length == 0
            || source_offset % block_size != 0
            || destination_offset % block_size != 0
            || length % block_size != 0
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if !self.volume.superblock.features.has_rmapbt()
            || !self.volume.superblock.features.has_reflink()
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut live = self.live.lock();
        if live.failed {
            return Err(XfsError::Io);
        }
        let source_inode = self.volume.inode(source)?;
        let destination_inode = self.volume.inode(destination)?;
        if source_inode.mode & 0o170000 != 0o100000 || destination_inode.mode & 0o170000 != 0o100000
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let source_end = source_offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        let destination_end = destination_offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        if source_end > source_inode.size {
            return Err(XfsError::AddressOutOfRange);
        }
        // Linux rejects overlapping self-reflinks because the source mapping
        // must remain an immutable snapshot for the entire transaction.
        if source == destination
            && source_offset < destination_end
            && destination_offset < source_end
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let source_extents = match source_inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(source)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(source)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let mut destination_extents = match destination_inode.data_format {
            XfsForkFormat::Extents => self.volume.inode_data_extents(destination)?,
            XfsForkFormat::Btree => self.volume.inode_bmbt_extents(destination)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let blocks = length / block_size;
        if dedupe {
            let mut left =
                vec![0u8; usize::try_from(block_size).map_err(|_| XfsError::AddressOutOfRange)?];
            let mut right = left.clone();
            for block in 0..blocks {
                self.volume
                    .read_inode_at(source, source_offset + block * block_size, &mut left)?;
                self.volume.read_inode_at(
                    destination,
                    destination_offset + block * block_size,
                    &mut right,
                )?;
                if left != right {
                    return Ok(false);
                }
            }
        }
        let mut planners = Vec::<XfsAgMutationPlanner>::new();
        let source_block = source_offset / block_size;
        let destination_block = destination_offset / block_size;
        for relative in 0..blocks {
            let source_mapping = xfs_extent_at(&source_extents, source_block + relative)?;
            let old_destination =
                xfs_extent_at(&destination_extents, destination_block + relative)?;
            if let Some(old) = old_destination {
                let old_planner = xfs_planner_for(&self.volume, &mut planners, old.start_block)?;
                let local =
                    u32::try_from(old.start_block % u64::from(self.volume.superblock.ag_blocks))
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                old_planner.remove_owner(local, destination, destination_block + relative)?;
                let count = old_planner.refcount_at(local)?;
                if count == 1 {
                    old_planner.release_free_block(local)?;
                } else {
                    old_planner.set_refcount(local, count - 1)?;
                }
            }
            if let Some(source_mapping) = source_mapping {
                if source_mapping.unwritten {
                    return Err(XfsError::UnsupportedFeature);
                }
                let source_planner =
                    xfs_planner_for(&self.volume, &mut planners, source_mapping.start_block)?;
                let local = u32::try_from(
                    source_mapping.start_block % u64::from(self.volume.superblock.ag_blocks),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                let count = source_planner.refcount_at(local)?;
                source_planner.set_refcount(
                    local,
                    count.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
                )?;
                source_planner.add_owner(local, destination, destination_block + relative)?;
            }
            xfs_replace_one_mapping(
                &mut destination_extents,
                destination_block + relative,
                source_mapping,
            )?;
        }
        let new_size = destination_inode.size.max(destination_end);
        // Rebuild the external bmap tree from the final mapping.  New node
        // homes are claimed from the same AG snapshots and receive BMBT rmap
        // ownership before the AG roots are staged, so root growth cannot
        // leak an unowned metadata block across a crash.
        let (raw_destination, destination_bytes) = self.volume.inode_and_bytes(destination)?;
        let fork_bytes = raw_destination.data_fork(&destination_bytes)?.len();
        let required_bmap = bmap_external_blocks(
            self.volume.superblock,
            fork_bytes,
            destination_extents.len(),
        )?;
        let old_bmap = if destination_inode.data_format == XfsForkFormat::Btree {
            self.volume.inode_bmbt_blocks(destination)?
        } else {
            Vec::new()
        };
        let destination_ag = self.volume.split_inode_number(destination)?.0;
        let reused_bmap = required_bmap.min(old_bmap.len());
        let mut bmap_blocks = Vec::new();
        bmap_blocks
            .try_reserve_exact(required_bmap)
            .map_err(|_| XfsError::NoMemory)?;
        bmap_blocks.extend_from_slice(&old_bmap[..reused_bmap]);
        while bmap_blocks.len() < required_bmap {
            let mut selected = None;
            for step in 0..self.volume.superblock.ag_count {
                let ag = u32::try_from(
                    (u64::from(destination_ag) + u64::from(step))
                        % u64::from(self.volume.superblock.ag_count),
                )
                .map_err(|_| XfsError::AddressOutOfRange)?;
                let physical = u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .ok_or(XfsError::AddressOutOfRange)?;
                let candidate = xfs_planner_for(&self.volume, &mut planners, physical)?;
                if let Ok(local) = candidate.claim_free_block() {
                    selected = Some((ag, local));
                    break;
                }
            }
            let (ag, local) = selected.ok_or(XfsError::AddressOutOfRange)?;
            let candidate = xfs_planner_for(
                &self.volume,
                &mut planners,
                u64::from(ag) * u64::from(self.volume.superblock.ag_blocks),
            )?;
            candidate.add_owner(local, destination, XFS_RMAP_OFF_BMBT)?;
            bmap_blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            bmap_blocks.push(
                u64::from(ag)
                    .checked_mul(u64::from(self.volume.superblock.ag_blocks))
                    .and_then(|base| base.checked_add(u64::from(local)))
                    .ok_or(XfsError::AddressOutOfRange)?,
            );
        }
        for physical in old_bmap.iter().copied().skip(required_bmap) {
            let candidate = xfs_planner_for(&self.volume, &mut planners, physical)?;
            let local = u32::try_from(physical % u64::from(self.volume.superblock.ag_blocks))
                .map_err(|_| XfsError::AddressOutOfRange)?;
            candidate.remove_owner(local, destination, XFS_RMAP_OFF_BMBT)?;
            candidate.release_free_block(local)?;
        }
        let mut metadata = XfsMetadataTransaction::default();
        for planner in &planners {
            let staged = self.volume.stage_reflink_ag_plan(planner)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        match destination_inode.data_format {
            XfsForkFormat::Extents if required_bmap == 0 => {
                self.volume.stage_regular_inode_extents(
                    destination,
                    destination_extents,
                    new_size,
                    &mut metadata,
                )?
            }
            XfsForkFormat::Extents | XfsForkFormat::Btree => self.volume.stage_regular_inode_bmap(
                destination,
                destination_extents,
                new_size,
                &bmap_blocks,
                &mut metadata,
            )?,
            _ => return Err(XfsError::UnsupportedFeature),
        }
        // The final `di_nblocks` image includes both the replaced data
        // mappings and any BMBT growth/shrink.  Charge its exact basic-block
        // delta rather than attempting a per-data-block approximation above.
        let quota_delta = self.staged_inode_block_delta(destination, &metadata)?;
        self.stage_inode_quota_delta(&destination_inode, quota_delta, 0, &mut metadata)?;
        self.volume.stage_inode_core_update(
            destination,
            XfsInodeCoreUpdate {
                mtime: Some((seconds, nanoseconds)),
                ctime: Some((seconds, nanoseconds)),
                ..XfsInodeCoreUpdate::default()
            },
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)?;
        Ok(true)
    }

    pub fn clone_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<()> {
        self.reflink_range(
            source,
            source_offset,
            destination,
            destination_offset,
            length,
            false,
            seconds,
            nanoseconds,
        )
        .map(|_| ())
    }

    pub fn dedupe_range(
        &self,
        source: u64,
        source_offset: u64,
        destination: u64,
        destination_offset: u64,
        length: u64,
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<bool> {
        self.reflink_range(
            source,
            source_offset,
            destination,
            destination_offset,
            length,
            true,
            seconds,
            nanoseconds,
        )
    }

    /// Commits a local attribute-fork replacement under the same live-log
    /// serialization as data-fork mutations.  The caller performs any
    /// namespace policy before this method; existence and replacement state
    /// are sampled only while this lock is held.
    pub fn replace_shortform_xattrs(
        &self,
        inode: u64,
        attrs: &[XfsShortformXattr],
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut metadata = XfsMetadataTransaction::default();
        self.volume
            .stage_shortform_xattrs(inode, attrs, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)
    }

    pub fn mutate_shortform_xattr(
        &self,
        inode: u64,
        flags: u8,
        name: &[u8],
        value: Option<&[u8]>,
        mode: XfsShortformXattrMode,
    ) -> XfsResult<XfsShortformXattrOutcome> {
        if name.is_empty() || name.iter().any(|byte| *byte == 0) {
            return Err(XfsError::AddressOutOfRange);
        }
        if flags & XFS_ATTR_LOCAL == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut live = self.live.lock();
        let mut attrs = self.volume.shortform_xattrs(inode)?;
        let at = attrs
            .iter()
            .position(|attribute| attribute.flags == flags && attribute.name == name);
        match (value, mode, at) {
            (None, _, None) => return Ok(XfsShortformXattrOutcome::Missing),
            (None, _, Some(index)) => {
                attrs.remove(index);
            }
            (
                Some(_),
                XfsShortformXattrMode::Create | XfsShortformXattrMode::CreateAndReplace,
                Some(_),
            ) => return Ok(XfsShortformXattrOutcome::Exists),
            (
                Some(_),
                XfsShortformXattrMode::Replace | XfsShortformXattrMode::CreateAndReplace,
                None,
            ) => return Ok(XfsShortformXattrOutcome::Missing),
            (Some(data), _, Some(index)) => attrs[index].value = data.to_vec(),
            (Some(data), _, None) => attrs.push(XfsShortformXattr {
                flags,
                name: name.to_vec(),
                value: data.to_vec(),
            }),
        }
        let mut metadata = XfsMetadataTransaction::default();
        self.volume
            .stage_shortform_xattrs(inode, &attrs, &mut metadata)?;
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsShortformXattrOutcome::Applied)
    }

    pub fn mutate_xattr(
        &self,
        inode: u64,
        flags: u8,
        name: &[u8],
        value: Option<&[u8]>,
        mode: XfsShortformXattrMode,
    ) -> XfsResult<XfsShortformXattrOutcome> {
        // LOCAL is a leaf storage bit.  Namespace callers always use the
        // normalized identity so moving a value to remote blocks does not
        // make it disappear from getxattr/listxattr.
        if name.is_empty()
            || name.iter().any(|byte| *byte == 0)
            || flags & !(XFS_ATTR_LOCAL | XFS_ATTR_ROOT | XFS_ATTR_SECURE) != 0
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let flags = flags | XFS_ATTR_LOCAL;
        let mut live = self.live.lock();
        let mut attrs = self.volume.xattrs(inode)?;
        let at = attrs
            .iter()
            .position(|attribute| attribute.flags == flags && attribute.name == name);
        match (value, mode, at) {
            (None, _, None) => return Ok(XfsShortformXattrOutcome::Missing),
            (None, _, Some(index)) => {
                attrs.remove(index);
            }
            (
                Some(_),
                XfsShortformXattrMode::Create | XfsShortformXattrMode::CreateAndReplace,
                Some(_),
            ) => return Ok(XfsShortformXattrOutcome::Exists),
            (
                Some(_),
                XfsShortformXattrMode::Replace | XfsShortformXattrMode::CreateAndReplace,
                None,
            ) => return Ok(XfsShortformXattrOutcome::Missing),
            (Some(data), _, Some(index)) => attrs[index].value = data.to_vec(),
            (Some(data), _, None) => attrs.push(XfsShortformXattr {
                flags,
                name: name.to_vec(),
                value: data.to_vec(),
            }),
        }
        let mut metadata = XfsMetadataTransaction::default();
        if self.volume.inode(inode)?.attr_format == XfsForkFormat::Local {
            match self
                .volume
                .stage_shortform_xattrs(inode, &attrs, &mut metadata)
            {
                Ok(()) => {}
                Err(XfsError::AddressOutOfRange) => {
                    self.volume
                        .stage_attribute_values(inode, &attrs, &mut metadata)?
                }
                Err(error) => return Err(error),
            }
        } else {
            self.volume
                .stage_attribute_values(inode, &attrs, &mut metadata)?;
        }
        self.commit_locked(&mut live, &metadata)?;
        Ok(XfsShortformXattrOutcome::Applied)
    }

    /// Applies a raw-name directory mutation to shortform or one-block
    /// dir2/dir3 storage.  Promotion reserves data blocks and updates the AG,
    /// inode and directory image under this one live-log lock; duplicate or
    /// missing names therefore never leak a partly committed namespace.
    pub fn mutate_directory(
        &self,
        directory: u64,
        mutation: XfsDirectoryMutation,
    ) -> XfsResult<()> {
        let mut live = self.live.lock();
        let mut entries = self.volume.directory_entries(directory)?;
        let name = match &mutation {
            XfsDirectoryMutation::Insert(entry) => entry.name.clone(),
            XfsDirectoryMutation::Remove(name) => name.clone(),
            XfsDirectoryMutation::Replace { name, .. } => name.clone(),
        };
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let at = entries.iter().position(|entry| entry.name == name);
        match mutation {
            XfsDirectoryMutation::Insert(entry) => {
                if at.is_some() {
                    return Err(XfsError::AddressOutOfRange);
                }
                if entry.inode == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                entries.push(entry);
            }
            XfsDirectoryMutation::Remove(_) => {
                entries.remove(at.ok_or(XfsError::AddressOutOfRange)?);
            }
            XfsDirectoryMutation::Replace { entry, .. } => {
                let index = at.ok_or(XfsError::AddressOutOfRange)?;
                if entry.inode == 0 || entry.name != entries[index].name {
                    return Err(XfsError::CorruptMetadata);
                }
                entries[index] = entry;
            }
        }
        let parent = self.volume.directory_parent(directory)?;
        let mut metadata = XfsMetadataTransaction::default();
        self.volume.stage_directory_entries_with_parent(
            directory,
            parent,
            &entries,
            &mut metadata,
        )?;
        self.commit_locked(&mut live, &metadata)
    }
}

/// Restartable application of a fully decoded, buffer-only recovery plan.
/// Each step ends with FUA+flush, so a power loss between steps is harmless:
/// opening a new session from the same journal simply observes the installed
/// home LSN and skips that descriptor.  The session deliberately has no
/// `Deref<XfsVolume>` implementation, preventing an unrecovered volume from
/// being accidentally passed to the VFS publication path.
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
pub(crate) struct XfsRecoverySession {
    volume: Arc<XfsVolume>,
    commits: Vec<XfsRecoveryCommit>,
    next: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
struct XfsPendingExtentFreeRecovery {
    lsn: u64,
    extents: Vec<(u64, u32)>,
}

/// Restartable semantic replay for the subset of intent items whose home
/// metadata is implemented here: pending EFIs.  A step keeps its prepared
/// buffer commit across an I/O failure, so retry never re-plans a partially
/// installed AG free-space tree against changed media.
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
pub(crate) struct XfsIntentRecoverySession {
    volume: Arc<XfsVolume>,
    steps: Vec<XfsPendingExtentFreeRecovery>,
    next: usize,
    prepared: Option<XfsRecoveryCommit>,
}

/// Coordinates recovery work that is itself made durable as ordinary XFS log
/// transactions.  The original log establishes *what* must be replayed; this
/// coordinator never writes its home images directly.  Instead it stages one
/// complete replacement set, commits a fresh record with FUA, waits for the
/// normal home-write flush, and only then checkpoints that generated record.
///
/// Intent items are reduced to complete replacement sets before they are
/// committed.  In particular, no recovery path edits a btree leaf in place:
/// the normal staged writers replace the verified tree, journal the new
/// images, install them with FUA, and checkpoint only after the home flush.
pub(crate) struct XfsRecoveryJournalCoordinator {
    volume: Arc<XfsVolume>,
    ring: XfsLogRing,
    ail: XfsAil,
    next_transaction: u32,
}

impl XfsRecoveryJournalCoordinator {
    pub fn new(volume: Arc<XfsVolume>, ring: XfsLogRing) -> XfsResult<Self> {
        if !volume.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(Self {
            volume,
            ring,
            ail: XfsAil::default(),
            // Keep recovery-generated transaction ids distinct from the
            // recovered clients' ids.  They are AIL identities, not an on
            // disk compatibility contract.
            next_transaction: 0x8000_0000,
        })
    }

    fn generated_transaction_id(&mut self) -> XfsResult<u32> {
        let id = self.next_transaction;
        self.next_transaction = self
            .next_transaction
            .checked_add(1)
            .filter(|next| *next != 0)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(id)
    }

    fn append_pending_efi(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut groups = Vec::<(u32, Vec<(u32, u32)>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::ExtentFree {
                start_block,
                block_count,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            let end = start_block
                .checked_add(u64::from(*block_count))
                .ok_or(XfsError::AddressOutOfRange)?;
            if *block_count == 0 || end > self.volume.superblock.data_blocks {
                return Err(XfsError::AddressOutOfRange);
            }
            let mut start = *start_block;
            while start < end {
                let ag =
                    u32::try_from(start / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let relative =
                    u32::try_from(start % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.volume.superblock.ag_count {
                    return Err(XfsError::AddressOutOfRange);
                }
                let available = ag_blocks
                    .checked_sub(u64::from(relative))
                    .ok_or(XfsError::CorruptMetadata)?;
                let length = u32::try_from((end - start).min(available))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if length == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                if let Some((_, frees)) = groups.iter_mut().find(|(candidate, _)| *candidate == ag)
                {
                    frees.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    frees.push((relative, length));
                } else {
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, vec![(relative, length)]));
                }
                start = start
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        for (ag, frees) in groups {
            let staged = self.volume.stage_recovery_extent_frees(ag, &frees)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    fn append_pending_rmap(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const MAP: u32 = 1;
        const MAP_SHARED: u32 = 2;
        const UNMAP: u32 = 3;
        const UNMAP_SHARED: u32 = 4;
        const CONVERT: u32 = 5;
        const CONVERT_SHARED: u32 = 6;
        const ATTR: u32 = 1 << 31;
        const BMBT: u32 = 1 << 30;
        const UNWRITTEN: u32 = 1 << 29;
        // These are the documented high bits of xfs_rmap_rec::rm_offset.
        const RM_ATTR: u64 = 1 << 63;
        const RM_BMBT: u64 = 1 << 62;
        const RM_UNWRITTEN: u64 = 1 << 61;
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut sets = Vec::<(u32, Vec<XfsRmapRecord>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::Mapping {
                owner,
                start_block,
                start_offset,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            let kind = flags & 0xff;
            if !matches!(
                kind,
                MAP | MAP_SHARED | UNMAP | UNMAP_SHARED | CONVERT | CONVERT_SHARED
            ) {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut left = u64::from(*block_count);
            let mut physical = *start_block;
            let mut file = *start_offset;
            while left != 0 {
                let ag =
                    u32::try_from(physical / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let agbno =
                    u32::try_from(physical % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let length = u32::try_from(left.min(ag_blocks - u64::from(agbno)))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.volume.superblock.ag_count || length == 0 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let index =
                    if let Some(index) = sets.iter().position(|(candidate, _)| *candidate == ag) {
                        index
                    } else {
                        let records = self.volume.rmap_records(ag)?;
                        sets.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        sets.push((ag, records));
                        sets.len() - 1
                    };
                let offset = file
                    | if flags & ATTR != 0 { RM_ATTR } else { 0 }
                    | if flags & BMBT != 0 { RM_BMBT } else { 0 }
                    | if flags & UNWRITTEN != 0 {
                        RM_UNWRITTEN
                    } else {
                        0
                    };
                let record = XfsRmapRecord {
                    start_block: agbno,
                    block_count: length,
                    owner: *owner,
                    offset,
                };
                let records = &mut sets[index].1;
                match kind {
                    MAP | MAP_SHARED => {
                        if records.iter().any(|old| {
                            ranges_overlap_u32(
                                old.start_block,
                                old.block_count,
                                record.start_block,
                                record.block_count,
                            ) && old.owner == record.owner
                                && old.offset == record.offset
                        }) {
                            return Err(XfsError::CorruptMetadata);
                        }
                        records.push(record);
                    }
                    UNMAP | UNMAP_SHARED => {
                        // A RUI may retire the middle of a previously coalesced
                        // rmap record.  Do not require byte-for-byte equality:
                        // retain the two still-owned fragments, with their
                        // file offsets advanced by the physical split.
                        replace_rmap_subrange(
                            records,
                            record,
                            None,
                            RM_ATTR | RM_BMBT | RM_UNWRITTEN,
                        )?;
                    }
                    CONVERT | CONVERT_SHARED => {
                        let opposite = XfsRmapRecord {
                            offset: record.offset ^ RM_UNWRITTEN,
                            ..record
                        };
                        replace_rmap_subrange(
                            records,
                            opposite,
                            Some(record),
                            RM_ATTR | RM_BMBT | RM_UNWRITTEN,
                        )?;
                    }
                    _ => return Err(XfsError::UnsupportedFeature),
                }
                physical = physical
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                file = file
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                left -= u64::from(length);
            }
        }
        for (ag, records) in sets {
            let staged = self.volume.stage_rmap_records(ag, records)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    fn append_pending_refcount(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const INCREASE: u32 = 1;
        const DECREASE: u32 = 2;
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut sets = Vec::<(u32, Vec<XfsRefcountRecord>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::Refcount {
                start_block,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            if !matches!(flags & 0xff, INCREASE | DECREASE) {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut left = u64::from(*block_count);
            let mut physical = *start_block;
            while left != 0 {
                let ag =
                    u32::try_from(physical / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let start =
                    u32::try_from(physical % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let length = u32::try_from(left.min(ag_blocks - u64::from(start)))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let index =
                    if let Some(index) = sets.iter().position(|(candidate, _)| *candidate == ag) {
                        index
                    } else {
                        let records = self.volume.refcount_records(ag)?;
                        sets.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        sets.push((ag, records));
                        sets.len() - 1
                    };
                adjust_refcount_records(
                    &mut sets[index].1,
                    start,
                    length,
                    flags & 0xff == INCREASE,
                )?;
                physical = physical
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                left -= u64::from(length);
            }
        }
        for (ag, records) in sets {
            let staged = self.volume.stage_refcount_records(ag, records)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    fn append_pending_bmap(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const MAP: u32 = 1;
        const UNMAP: u32 = 2;
        const ATTR: u32 = 1 << 31;
        const UNWRITTEN: u32 = 1 << 30;
        const REALTIME: u32 = 1 << 29;
        for extent in extents {
            let XfsLogReplayExtent::Mapping {
                owner,
                start_block,
                start_offset,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            if flags & REALTIME != 0 || !matches!(flags & 0xff, MAP | UNMAP) {
                return Err(XfsError::UnsupportedFeature);
            }
            let inode = self.volume.inode(*owner)?;
            let attribute_fork = flags & ATTR != 0;
            // A nonempty shortform fork cannot be converted by merely
            // replacing its extent map: doing that would discard xattrs.
            // Admit only an empty Local fork, which is the legitimate
            // "first external mapping" BUI case; the full shortform-to-DA
            // conversion remains an explicit transaction elsewhere.
            let empty_local_attr = if attribute_fork && inode.attr_format == XfsForkFormat::Local {
                let (_, raw) = self.volume.inode_and_bytes(*owner)?;
                inode.attr_fork(&raw)?.iter().all(|byte| *byte == 0)
            } else {
                false
            };
            if (!attribute_fork && inode.data_format != XfsForkFormat::Extents)
                || (attribute_fork
                    && !(inode.attr_format == XfsForkFormat::Extents || empty_local_attr))
            {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut records = if attribute_fork && empty_local_attr {
                Vec::new()
            } else if attribute_fork {
                self.volume.inode_attr_extents(*owner)?
            } else {
                self.volume.inode_data_extents(*owner)?
            };
            let record = XfsExtent {
                unwritten: flags & UNWRITTEN != 0,
                file_block: *start_offset,
                start_block: *start_block,
                block_count: *block_count,
            };
            match flags & 0xff {
                MAP => {
                    if records.iter().any(|old| {
                        ranges_overlap_u64(
                            old.file_block,
                            old.block_count,
                            record.file_block,
                            record.block_count,
                        )
                    }) {
                        return Err(XfsError::CorruptMetadata);
                    }
                    records.push(record);
                }
                UNMAP => replace_bmap_subrange(&mut records, record)?,
                _ => return Err(XfsError::UnsupportedFeature),
            }
            if attribute_fork {
                self.volume
                    .stage_attribute_fork_extents(*owner, &records, metadata)?;
            } else {
                self.volume
                    .stage_regular_inode_extents(*owner, records, inode.size, metadata)?;
            }
        }
        Ok(())
    }

    fn stage_buffer_replay(
        &self,
        item: XfsBufferReplayItem,
        lsn: u64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let blocks = u64::from(item.block_count);
        let end = item
            .block_number
            .checked_add(blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        if blocks == 0 || end > self.volume.basic_blocks(&self.volume.data)? {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut before = vec![
            0;
            usize::from(item.block_count)
                .checked_mul(XFS_LOG_BASIC_BLOCK)
                .ok_or(XfsError::AddressOutOfRange)?
        ];
        self.volume
            .read_basic_blocks(&self.volume.data, item.block_number, &mut before)?;
        let after =
            item.materialize_home_image(&before, lsn, self.volume.superblock.inode_size as usize)?;
        metadata.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: item.metadata_type()?,
            basic_block: item.block_number,
            before,
            after,
        });
        Ok(())
    }

    fn stage_transaction(
        &self,
        transaction: &XfsRecoveryTransaction,
        pending: &XfsIntentRecovery,
        buffer_cancels: &mut Vec<(u64, u16, u32)>,
        suppressed_quotas: &mut u32,
    ) -> XfsResult<XfsMetadataTransaction> {
        let items = transaction.replay_items(transaction.byte_order)?;
        if items.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut normal_buffers = Vec::new();
        let mut inode_unlink_buffers = Vec::new();
        let mut cancelled_buffers = Vec::new();
        let mut non_buffers = Vec::new();
        // Buffer replay dependencies are not the log's byte-stream order.
        // AG allocator/btree images must be visible before inode items are
        // materialised, while inode-unlink buffers and cancelled buffers
        // belong after the non-buffer item classes.  Preserve source order
        // inside each class.  Cancellation is a two-pass protocol: the
        // table was built from the complete plan before replay starts, and a
        // marker consumes one reference only when its log position is met.
        for item in items {
            match item {
                XfsReplayItem::Buffer(buffer) if buffer_cancelled(buffer_cancels, &buffer)? => {
                    cancelled_buffers.push(buffer)
                }
                // XFS_BLF_INODE_BUF, not the decoded metadata type, marks
                // the special di_next_unlinked-only buffer class.  Ordinary
                // inode allocation buffers must remain in the early list.
                XfsReplayItem::Buffer(buffer) if buffer.flags & XFS_BLF_INODE_BUF != 0 => {
                    inode_unlink_buffers.push(buffer)
                }
                XfsReplayItem::Buffer(buffer) => normal_buffers.push(buffer),
                item => non_buffers.push(item),
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for item in normal_buffers {
            self.stage_buffer_replay(item, transaction.lsn, &mut metadata)?;
        }
        for item in non_buffers {
            match item {
                XfsReplayItem::Buffer(_) => return Err(XfsError::CorruptMetadata),
                XfsReplayItem::Inode(item) => {
                    self.volume.stage_inode_log_replay(
                        &item,
                        transaction.lsn,
                        transaction.byte_order,
                        &mut metadata,
                    )?;
                }
                XfsReplayItem::Dquot(item) => {
                    let dquot = item.parse_disk_dquot(
                        self.volume.superblock.is_v5(),
                        self.volume
                            .superblock
                            .is_v5()
                            .then_some(self.volume.superblock.meta_uuid),
                        self.volume.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME
                            != 0,
                    )?;
                    if *suppressed_quotas & u32::from(dquot.quota_type) == 0 {
                        self.volume.stage_dquot_log_replay(
                            &item,
                            transaction.lsn,
                            &mut metadata,
                        )?;
                    }
                }
                XfsReplayItem::Intent(intent) => match intent.key.kind {
                    XfsIntentKind::ExtentFree if pending.is_pending(intent.key) => {
                        self.append_pending_efi(&intent.extents, &mut metadata)?;
                    }
                    XfsIntentKind::ExtentFree => {}
                    XfsIntentKind::Rmap if pending.is_pending(intent.key) => {
                        self.append_pending_rmap(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Refcount if pending.is_pending(intent.key) => {
                        self.append_pending_refcount(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Bmap if pending.is_pending(intent.key) => {
                        self.append_pending_bmap(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Rmap | XfsIntentKind::Refcount | XfsIntentKind::Bmap => {}
                },
                XfsReplayItem::Done(_) => {}
                // QUOTAOFF is not an on-disk metadata update.  Like Linux
                // recovery, it suppresses replay of later dquot items of the
                // named type; persisting a synthetic superblock change here
                // would corrupt the recorded quota state.
                XfsReplayItem::Quotaoff { flags } => *suppressed_quotas |= flags,
            }
        }
        for item in inode_unlink_buffers {
            self.stage_buffer_replay(item, transaction.lsn, &mut metadata)?;
        }
        // Cancel markers (and every buffer suppressed by their outstanding
        // reference) are deliberately consumed last and never materialised:
        // the block may already have been reused for user data.
        drop(cancelled_buffers);
        Ok(metadata)
    }

    /// Replays every committed transaction in log order.  Intent/done
    /// admission is completed for the entire plan before the first generated
    /// record is written, so a malformed later done item cannot leave an
    /// earlier transaction published as a partial recovery result.
    pub fn replay_plan(&mut self, plan: &XfsRecoveryPlan) -> XfsResult<()> {
        let mut pending = XfsIntentRecovery::default();
        for transaction in &plan.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            pending.apply_transaction(&items)?;
        }
        let mut suppressed_quotas = 0u32;
        let mut buffer_cancels = collect_buffer_cancels(&plan.committed)?;
        for transaction in &plan.committed {
            let metadata = self.stage_transaction(
                transaction,
                &pending,
                &mut buffer_cancels,
                &mut suppressed_quotas,
            )?;
            if metadata.buffers.is_empty() && metadata.dquots.is_empty() {
                // stage_transaction never produces a replayable data-only or
                // realtime-only mutation; both must be anchored by a BUF or
                // DQUOT item in the recovered transaction.
                if !metadata.data_writes.is_empty() || !metadata.realtime_writes.is_empty() {
                    return Err(XfsError::CorruptMetadata);
                }
                continue;
            }
            let transaction_id = self.generated_transaction_id()?;
            self.volume.commit_metadata_transaction(
                &mut self.ring,
                &mut self.ail,
                transaction_id,
                &metadata,
            )?;
        }
        if !buffer_cancels.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(())
    }

    pub fn finish(self) -> XfsResult<(Arc<XfsVolume>, XfsLogRing)> {
        if !self.ail.entries().is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok((self.volume, self.ring))
    }
}

fn ranges_overlap_u32(start: u32, length: u32, other_start: u32, other_length: u32) -> bool {
    let end = start.checked_add(length);
    let other_end = other_start.checked_add(other_length);
    matches!((end, other_end), (Some(end), Some(other_end)) if start < other_end && other_start < end)
}

/// Realtime bitmap words on pre-rtgroup media are deliberately host-endian;
/// rtgroups changed the on-disk representation to BE32 and place a 64-byte
/// authenticated buffer header before the words.  Do not treat either layout
/// as a byte-oriented MSB-first bitmap: XFS numbers bits from the low bit of
/// each 32-bit word.
const XFS_RTBUF_HEADER_BYTES: usize = 48;

fn realtime_payload_offset(rtgroups: bool) -> usize {
    if rtgroups { XFS_RTBUF_HEADER_BYTES } else { 0 }
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn realtime_word(bytes: &[u8], word: usize, rtgroups: bool) -> XfsResult<u32> {
    let offset = realtime_payload_offset(rtgroups)
        .checked_add(word.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    let raw: [u8; 4] = slice(bytes, offset, 4)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(if rtgroups {
        u32::from_be_bytes(raw)
    } else {
        u32::from_ne_bytes(raw)
    })
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn set_realtime_word(bytes: &mut [u8], word: usize, value: u32, rtgroups: bool) -> XfsResult<()> {
    let offset = realtime_payload_offset(rtgroups)
        .checked_add(word.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    if offset.checked_add(4).ok_or(XfsError::AddressOutOfRange)? > bytes.len() {
        return Err(XfsError::AddressOutOfRange);
    }
    let encoded = if rtgroups {
        value.to_be_bytes()
    } else {
        value.to_ne_bytes()
    };
    bytes[offset..offset + 4].copy_from_slice(&encoded);
    Ok(())
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn realtime_bitmap_bit(bytes: &[u8], bit: u64, rtgroups: bool) -> XfsResult<bool> {
    let word = usize::try_from(bit / 32).map_err(|_| XfsError::AddressOutOfRange)?;
    Ok(realtime_word(bytes, word, rtgroups)? & (1u32 << (bit % 32)) != 0)
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn realtime_bitmap_range(
    bytes: &mut [u8],
    first: u64,
    count: u64,
    allocate: bool,
    rtgroups: bool,
) -> XfsResult<()> {
    let bits = bytes
        .len()
        .checked_sub(realtime_payload_offset(rtgroups))
        .ok_or(XfsError::CorruptMetadata)?
        .checked_mul(8)
        .ok_or(XfsError::AddressOutOfRange)?;
    let end = first
        .checked_add(count)
        .ok_or(XfsError::AddressOutOfRange)?;
    if count == 0 || end > bits as u64 {
        return Err(XfsError::AddressOutOfRange);
    }
    for bit in first..end {
        let word = usize::try_from(bit / 32).map_err(|_| XfsError::AddressOutOfRange)?;
        let mask = 1u32 << (bit % 32);
        let value = realtime_word(bytes, word, rtgroups)?;
        // XFS stores one for a free realtime extent and zero for an allocated
        // one.  This is the inverse of most generic bitmap helpers.
        if allocate {
            if value & mask == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            set_realtime_word(bytes, word, value & !mask, rtgroups)?;
        } else {
            if value & mask != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            set_realtime_word(bytes, word, value | mask, rtgroups)?;
        }
    }
    Ok(())
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn realtime_summary_counter(bytes: &[u8], word: usize, rtgroups: bool) -> XfsResult<u32> {
    realtime_word(bytes, word, rtgroups)
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
fn set_realtime_summary_counter(
    bytes: &mut [u8],
    word: usize,
    value: u32,
    rtgroups: bool,
) -> XfsResult<()> {
    set_realtime_word(bytes, word, value, rtgroups)
}

const XFS_BLF_CANCEL: u16 = 0x0001;
const XFS_BLF_INODE_BUF: u16 = 0x0002;

/// Pass-one cancellation table.  A cancellation marker applies to every
/// earlier matching occurrence still covered by its reference count; the
/// marker itself decrements the count when pass two reaches its own position.
fn collect_buffer_cancels(
    transactions: &[XfsRecoveryTransaction],
) -> XfsResult<Vec<(u64, u16, u32)>> {
    let mut table: Vec<(u64, u16, u32)> = Vec::new();
    for transaction in transactions {
        for item in transaction.replay_items(transaction.byte_order)? {
            let XfsReplayItem::Buffer(buffer) = item else {
                continue;
            };
            if buffer.flags & XFS_BLF_CANCEL == 0 {
                continue;
            }
            if buffer.block_count == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            if let Some((_, _, references)) = table.iter_mut().find(|(block, length, _)| {
                *block == buffer.block_number && *length == buffer.block_count
            }) {
                *references = references
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                table.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                table.push((buffer.block_number, buffer.block_count, 1));
            }
        }
    }
    Ok(table)
}

/// Pass-two lookup/consumption counterpart to `collect_buffer_cancels`.
/// Returns true when the caller must not replay this BUF item.
fn buffer_cancelled(
    table: &mut Vec<(u64, u16, u32)>,
    buffer: &XfsBufferReplayItem,
) -> XfsResult<bool> {
    let Some(index) = table.iter().position(|(block, length, _)| {
        *block == buffer.block_number && *length == buffer.block_count
    }) else {
        return if buffer.flags & XFS_BLF_CANCEL != 0 {
            Err(XfsError::CorruptMetadata)
        } else {
            Ok(false)
        };
    };
    if buffer.flags & XFS_BLF_CANCEL != 0 {
        let references = &mut table[index].2;
        *references = references.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
        if *references == 0 {
            table.remove(index);
        }
    }
    Ok(true)
}

fn ranges_overlap_u64(start: u64, length: u32, other_start: u64, other_length: u32) -> bool {
    let end = start.checked_add(u64::from(length));
    let other_end = other_start.checked_add(u64::from(other_length));
    matches!((end, other_end), (Some(end), Some(other_end)) if start < other_end && other_start < end)
}

/// Remove exactly one BUI range while retaining both portions of a coalesced
/// mapping.  BUI unmaps commonly target only the middle of an old extent, so
/// equality with the complete on-disk record is neither required nor safe.
fn replace_bmap_subrange(records: &mut Vec<XfsExtent>, target: XfsExtent) -> XfsResult<()> {
    let target_end = target
        .file_block
        .checked_add(u64::from(target.block_count))
        .ok_or(XfsError::AddressOutOfRange)?;
    let index = records
        .iter()
        .position(|old| {
            let Some(end) = old.file_block.checked_add(u64::from(old.block_count)) else {
                return false;
            };
            let offset = target.file_block.checked_sub(old.file_block);
            old.unwritten == target.unwritten
                && target.file_block >= old.file_block
                && target_end <= end
                && offset.and_then(|delta| old.start_block.checked_add(delta))
                    == Some(target.start_block)
        })
        .ok_or(XfsError::CorruptMetadata)?;
    let old = records.remove(index);
    let left = target.file_block - old.file_block;
    let right = old
        .file_block
        .checked_add(u64::from(old.block_count))
        .ok_or(XfsError::CorruptMetadata)?
        - target_end;
    if right != 0 {
        records.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        records.push(XfsExtent {
            unwritten: old.unwritten,
            file_block: target_end,
            start_block: target
                .start_block
                .checked_add(u64::from(target.block_count))
                .ok_or(XfsError::AddressOutOfRange)?,
            block_count: u32::try_from(right).map_err(|_| XfsError::AddressOutOfRange)?,
        });
    }
    if left != 0 {
        records.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        records.push(XfsExtent {
            unwritten: old.unwritten,
            file_block: old.file_block,
            start_block: old.start_block,
            block_count: u32::try_from(left).map_err(|_| XfsError::AddressOutOfRange)?,
        });
    }
    records.sort_unstable_by_key(|extent| extent.file_block);
    Ok(())
}

/// Removes (or replaces) one physical subrange of a single rmap owner.
/// XFS coalesces adjacent rmap records, so recovery must be able to split a
/// larger home record when a RUI describes only its middle.  The logical
/// portion of `rm_offset` advances in lockstep with the physical block; the
/// fork/state bits remain unchanged on both retained fragments.
fn replace_rmap_subrange(
    records: &mut Vec<XfsRmapRecord>,
    target: XfsRmapRecord,
    replacement: Option<XfsRmapRecord>,
    offset_flags: u64,
) -> XfsResult<()> {
    let target_end = target
        .start_block
        .checked_add(target.block_count)
        .ok_or(XfsError::AddressOutOfRange)?;
    let target_logical = target.offset & !offset_flags;
    let target_flags = target.offset & offset_flags;
    let at = records
        .iter()
        .position(|old| {
            let Some(old_end) = old.start_block.checked_add(old.block_count) else {
                return false;
            };
            if old.owner != target.owner
                || old.offset & offset_flags != target_flags
                || old.start_block > target.start_block
                || old_end < target_end
            {
                return false;
            }
            let delta = u64::from(target.start_block - old.start_block);
            (old.offset & !offset_flags).checked_add(delta) == Some(target_logical)
        })
        .ok_or(XfsError::CorruptMetadata)?;
    let old = records.remove(at);
    let old_end = old
        .start_block
        .checked_add(old.block_count)
        .ok_or(XfsError::CorruptMetadata)?;
    if old.start_block < target.start_block {
        records.push(XfsRmapRecord {
            start_block: old.start_block,
            block_count: target.start_block - old.start_block,
            owner: old.owner,
            offset: old.offset,
        });
    }
    if let Some(replacement) = replacement {
        if replacement.start_block != target.start_block
            || replacement.block_count != target.block_count
            || replacement.owner != target.owner
            || replacement.offset & !offset_flags != target_logical
        {
            return Err(XfsError::CorruptMetadata);
        }
        records.push(replacement);
    }
    if target_end < old_end {
        let delta = u64::from(target_end - old.start_block);
        let logical = (old.offset & !offset_flags)
            .checked_add(delta)
            .ok_or(XfsError::AddressOutOfRange)?;
        records.push(XfsRmapRecord {
            start_block: target_end,
            block_count: old_end - target_end,
            owner: old.owner,
            offset: (old.offset & offset_flags) | logical,
        });
    }
    records.sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
    Ok(())
}

fn adjust_refcount_records(
    records: &mut Vec<XfsRefcountRecord>,
    start: u32,
    length: u32,
    increase: bool,
) -> XfsResult<()> {
    let end = start
        .checked_add(length)
        .ok_or(XfsError::AddressOutOfRange)?;
    let mut out = Vec::new();
    let mut cursor = start;
    for record in records.iter().copied() {
        let record_end = record
            .start_block
            .checked_add(record.block_count)
            .ok_or(XfsError::CorruptMetadata)?;
        if record_end <= start || record.start_block >= end {
            out.push(record);
            continue;
        }
        if record.start_block < start {
            out.push(XfsRefcountRecord {
                start_block: record.start_block,
                block_count: start - record.start_block,
                refcount: record.refcount,
            });
        }
        let overlap_start = record.start_block.max(start);
        let overlap_end = record_end.min(end);
        if overlap_start > cursor {
            if !increase {
                return Err(XfsError::CorruptMetadata);
            }
            out.push(XfsRefcountRecord {
                start_block: cursor,
                block_count: overlap_start - cursor,
                refcount: 2,
            });
        }
        if increase {
            out.push(XfsRefcountRecord {
                start_block: overlap_start,
                block_count: overlap_end - overlap_start,
                refcount: record
                    .refcount
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            });
        } else if record.refcount > 2 {
            out.push(XfsRefcountRecord {
                start_block: overlap_start,
                block_count: overlap_end - overlap_start,
                refcount: record.refcount - 1,
            });
        }
        if record_end > end {
            out.push(XfsRefcountRecord {
                start_block: end,
                block_count: record_end - end,
                refcount: record.refcount,
            });
        }
        cursor = cursor.max(overlap_end);
    }
    if cursor < end {
        if !increase {
            return Err(XfsError::CorruptMetadata);
        }
        out.push(XfsRefcountRecord {
            start_block: cursor,
            block_count: end - cursor,
            refcount: 2,
        });
    }
    out.sort_unstable_by_key(|record| record.start_block);
    let mut merged: Vec<XfsRefcountRecord> = Vec::new();
    for record in out {
        if record.block_count == 0 || record.refcount < 2 {
            return Err(XfsError::CorruptMetadata);
        }
        if let Some(last) = merged.last_mut()
            && last.refcount == record.refcount
            && last.start_block.checked_add(last.block_count) == Some(record.start_block)
        {
            last.block_count = last
                .block_count
                .checked_add(record.block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
        } else {
            merged.push(record);
        }
    }
    *records = merged;
    Ok(())
}

// Recovery-session drivers for the in-progress journal recovery path.
#[allow(dead_code)]
impl XfsIntentRecoverySession {
    pub fn state(&self) -> (usize, usize) {
        (self.next, self.steps.len())
    }

    pub fn apply_next(&mut self) -> XfsResult<bool> {
        let Some(step) = self.steps.get(self.next) else {
            return Ok(false);
        };
        if self.prepared.is_none() {
            self.prepared = self.volume.prepare_pending_extent_free_recovery(step)?;
        }
        if let Some(commit) = &self.prepared {
            self.volume.apply_recovery_commit(commit)?;
        }
        self.prepared = None;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(true)
    }

    pub fn apply_all(&mut self) -> XfsResult<()> {
        while self.apply_next()? {}
        Ok(())
    }

    pub fn finish(self) -> XfsResult<Arc<XfsVolume>> {
        if self.next != self.steps.len() || self.prepared.is_some() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(self.volume)
    }
}

// Recovery-session drivers for the in-progress journal recovery path.
#[allow(dead_code)]
impl XfsRecoverySession {
    pub fn state(&self) -> (usize, usize) {
        (self.next, self.commits.len())
    }

    pub fn apply_next(&mut self) -> XfsResult<bool> {
        let Some(commit) = self.commits.get(self.next) else {
            return Ok(false);
        };
        self.volume.apply_recovery_commit(commit)?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(true)
    }

    pub fn apply_all(&mut self) -> XfsResult<()> {
        while self.apply_next()? {}
        Ok(())
    }

    /// Returns the verified volume only after every prepared transaction has
    /// reached durable home blocks.  The caller still must handle intent,
    /// inode and dquot item classes before publishing a general XFS mount.
    pub fn finish(self) -> XfsResult<Arc<XfsVolume>> {
        if self.next != self.commits.len() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(self.volume)
    }
}

impl XfsJournalRecoveryState {
    pub const fn requires_replay(self) -> bool {
        self.committed_transactions != 0
    }
}

/// A verified record framing boundary, including all its operation headers.
/// Semantic item decoding is intentionally a second phase: item data is only
/// applied after every operation in the transaction has been collected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsJournalRecord {
    pub header: XfsLogRecordHeader,
    pub operations: Vec<XfsLogOperation>,
}

/// A completely framed transaction ready for item-specific replay.  Only a
/// transaction that has both a start and commit operation becomes visible in
/// this list; interrupted log tails remain uncommitted and are discarded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRecoveryTransaction {
    pub transaction_id: u32,
    /// LSN of the record carrying the latest region of this committed
    /// transaction.  This is the LSN installed into every replayed home
    /// buffer, and is therefore part of the replay identity rather than a
    /// diagnostic timestamp.
    pub lsn: u64,
    pub byte_order: XfsLogByteOrder,
    pub operations: Vec<XfsLogOperation>,
    pending_operation: Option<XfsPendingLogOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct XfsPendingLogOperation {
    client_id: u8,
    flags: u8,
    payload: Vec<u8>,
}

impl XfsRecoveryTransaction {
    /// Adds one physical operation region, joining a record-boundary fragment
    /// only when the native continuation flags prove that its predecessor and
    /// successor belong together.  A missing/mismatched fragment is log
    /// corruption; treating it as an independent item would replay a prefix
    /// of an inode, btree, or buffer update.
    fn push_region(&mut self, operation: XfsLogOperation) -> XfsResult<()> {
        const MARKERS: u8 = XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS;
        let was_continued = operation.flags & XLOG_WAS_CONT_TRANS != 0;
        let continues = operation.flags & XLOG_CONTINUE_TRANS != 0;
        let ends = operation.flags & XLOG_END_TRANS != 0;
        if ends && continues {
            return Err(XfsError::CorruptMetadata);
        }
        if was_continued {
            let pending = self
                .pending_operation
                .as_mut()
                .ok_or(XfsError::CorruptMetadata)?;
            if pending.client_id != operation.client_id {
                return Err(XfsError::CorruptMetadata);
            }
            pending
                .payload
                .try_reserve(operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            pending.payload.extend_from_slice(&operation.payload);
            pending.flags |= operation.flags & !MARKERS;
            if continues {
                return Ok(());
            }
            if !ends {
                return Err(XfsError::CorruptMetadata);
            }
            let pending = self
                .pending_operation
                .take()
                .ok_or(XfsError::CorruptMetadata)?;
            self.operations
                .try_reserve(1)
                .map_err(|_| XfsError::NoMemory)?;
            self.operations.push(XfsLogOperation {
                transaction_id: self.transaction_id,
                client_id: pending.client_id,
                flags: pending.flags,
                payload: pending.payload,
            });
            return Ok(());
        }
        if self.pending_operation.is_some() {
            return Err(XfsError::CorruptMetadata);
        }
        if continues {
            let mut payload = Vec::new();
            payload
                .try_reserve(operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            payload.extend_from_slice(&operation.payload);
            self.pending_operation = Some(XfsPendingLogOperation {
                client_id: operation.client_id,
                flags: operation.flags & !MARKERS,
                payload,
            });
            return Ok(());
        }
        if ends {
            return Err(XfsError::CorruptMetadata);
        }
        self.operations
            .try_reserve(1)
            .map_err(|_| XfsError::NoMemory)?;
        self.operations.push(XfsLogOperation {
            transaction_id: operation.transaction_id,
            client_id: operation.client_id,
            flags: operation.flags & !MARKERS,
            payload: operation.payload,
        });
        Ok(())
    }

    fn complete(&self) -> XfsResult<()> {
        if self.pending_operation.is_some() {
            Err(XfsError::CorruptMetadata)
        } else {
            Ok(())
        }
    }
    /// Finds and validates the unique transaction-header region before any
    /// item decoder consumes native-endian payload bytes.  This prevents a
    /// continuation fragment from being mistaken for a standalone replay
    /// transaction.
    pub fn header(&self, order: XfsLogByteOrder) -> XfsResult<XfsTransactionHeader> {
        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        let mut header = None;
        for operation in &self.operations {
            if operation.payload.len() != 16 {
                continue;
            }
            let magic = native_u32(&operation.payload, 0, order)?;
            if magic != 0x5452_414e {
                continue;
            }
            if header.is_some() {
                return Err(XfsError::CorruptMetadata);
            }
            header = Some(XfsTransactionHeader {
                transaction_type: native_u32(&operation.payload, 4, order)?,
                item_count: native_u32(&operation.payload, 12, order)?,
            });
        }
        header.ok_or(XfsError::CorruptMetadata)
    }

    /// Returns log regions after the validated transaction header, preserving
    /// journal order for the item decoder.  The decoder owns format/data
    /// pairing; this method never coalesces adjacent regions heuristically.
    pub fn item_regions(&self, order: XfsLogByteOrder) -> XfsResult<Vec<&XfsLogOperation>> {
        let _ = self.header(order)?;
        let mut regions = Vec::new();
        let mut skipped = false;
        for operation in &self.operations {
            if !skipped
                && operation.payload.len() == 16
                && native_u32(&operation.payload, 0, order).ok() == Some(0x5452_414e)
            {
                skipped = true;
                continue;
            }
            regions.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            regions.push(operation);
        }
        Ok(regions)
    }

    /// Decodes one `xfs_buf_log_format` region and its following BCHUNK
    /// regions. Every dirty 128-byte bitmap bit consumes exactly one complete
    /// region; cancellation buffers intentionally produce no home write.
    pub fn buffer_item(
        &self,
        order: XfsLogByteOrder,
        format: &[u8],
        chunk_regions: &[&[u8]],
    ) -> XfsResult<XfsBufferReplayItem> {
        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        if format.len() < 20 || native_u16(format, 0, order)? != 0x123c {
            return Err(XfsError::CorruptMetadata);
        }
        let flags = native_u16(format, 4, order)?;
        let block_count = native_u16(format, 6, order)?;
        let block_number = native_u64(format, 8, order)?;
        let words = native_u32(format, 16, order)? as usize;
        let map_bytes = words.checked_mul(4).ok_or(XfsError::CorruptMetadata)?;
        if format.len()
            != 20usize
                .checked_add(map_bytes)
                .ok_or(XfsError::CorruptMetadata)?
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut dirty_chunks = Vec::new();
        for word in 0..words {
            let bits = native_u32(format, 20 + word * 4, order)?;
            for bit in 0..32 {
                if bits & (1 << bit) != 0 {
                    dirty_chunks
                        .try_reserve(1)
                        .map_err(|_| XfsError::NoMemory)?;
                    dirty_chunks.push((word * 32 + bit) as u32);
                }
            }
        }
        if dirty_chunks.len() != chunk_regions.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(chunk_regions.len())
            .map_err(|_| XfsError::NoMemory)?;
        for chunk in chunk_regions {
            if chunk.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            chunks.push(chunk.to_vec());
        }
        Ok(XfsBufferReplayItem {
            flags,
            block_number,
            block_count,
            dirty_chunks,
            chunks,
        })
    }

    /// Decodes every native XFS item in this committed transaction.  Item
    /// format IDs and `*_size` fields are authoritative: no item is inferred
    /// from a payload length, and a malformed item can never consume regions
    /// belonging to its successor.
    pub fn replay_items(&self, order: XfsLogByteOrder) -> XfsResult<Vec<XfsReplayItem>> {
        const EFI: u16 = 0x1236;
        const EFD: u16 = 0x1237;
        const INODE: u16 = 0x123b;
        const BUF: u16 = 0x123c;
        const DQUOT: u16 = 0x123d;
        const QUOTAOFF: u16 = 0x123e;
        const RUI: u16 = 0x1240;
        const RUD: u16 = 0x1241;
        const CUI: u16 = 0x1242;
        const CUD: u16 = 0x1243;
        const BUI: u16 = 0x1244;
        const BUD: u16 = 0x1245;

        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        let expected_items = usize::try_from(self.header(order)?.item_count)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        if expected_items == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let regions = self.item_regions(order)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(expected_items)
            .map_err(|_| XfsError::NoMemory)?;
        let mut index = 0usize;
        while index < regions.len() {
            let format = regions[index].payload.as_slice();
            if format.len() < 4 {
                return Err(XfsError::CorruptMetadata);
            }
            let kind = native_u16(format, 0, order)?;
            let count = usize::from(native_u16(format, 2, order)?);
            if count == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            let end = index
                .checked_add(count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if end > regions.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let item = match kind {
                BUF => {
                    if format.len() < 20 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let words = usize::try_from(native_u32(format, 16, order)?)
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                    let map_end = 20usize
                        .checked_add(words.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    if map_end != format.len() {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut chunks = 0usize;
                    for word in 0..words {
                        chunks = chunks
                            .checked_add(
                                native_u32(format, 20 + word * 4, order)?.count_ones() as usize
                            )
                            .ok_or(XfsError::AddressOutOfRange)?;
                    }
                    if count != chunks.checked_add(1).ok_or(XfsError::AddressOutOfRange)? {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut chunk_regions = Vec::new();
                    chunk_regions
                        .try_reserve_exact(chunks)
                        .map_err(|_| XfsError::NoMemory)?;
                    for region in &regions[index + 1..end] {
                        if region.payload.len() != 128 {
                            return Err(XfsError::CorruptMetadata);
                        }
                        chunk_regions.push(region.payload.as_slice());
                    }
                    XfsReplayItem::Buffer(self.buffer_item(order, format, &chunk_regions)?)
                }
                INODE => {
                    // Native x86_64 xfs_inode_log_format is 40 bytes:
                    // type/size/fields/asize/dsize/ino/blkno/len/boffset.
                    // The two size fields describe subsequent logged core
                    // and fork regions, so they must not be mistaken for a
                    // padding word.
                    if format.len() != 40 || count < 2 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let inode = native_u64(format, 16, order)?;
                    let block_number = native_u64(format, 24, order)?;
                    let block_count = native_u32(format, 32, order)?;
                    let byte_offset = native_u32(format, 36, order)?;
                    if inode == 0
                        || block_number >> 63 != 0
                        || block_count == 0
                        || byte_offset >> 31 != 0
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut payloads = Vec::new();
                    payloads
                        .try_reserve_exact(count - 1)
                        .map_err(|_| XfsError::NoMemory)?;
                    for region in &regions[index + 1..end] {
                        if region.payload.is_empty() {
                            return Err(XfsError::CorruptMetadata);
                        }
                        payloads.push(region.payload.clone());
                    }
                    XfsReplayItem::Inode(XfsInodeReplayItem {
                        inode,
                        block_number,
                        block_count,
                        byte_offset,
                        fields: native_u32(format, 4, order)?,
                        attr_size: native_u16(format, 8, order)?,
                        data_size: native_u16(format, 10, order)?,
                        regions: payloads,
                    })
                }
                DQUOT => {
                    // xfs_dq_logformat is followed by exactly one
                    // xfs_disk_dquot region.
                    if format.len() != 24 || count != 2 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let id = native_u32(format, 4, order)?;
                    let block_number = native_u64(format, 8, order)?;
                    let block_count = native_u32(format, 16, order)?;
                    let byte_offset = native_u32(format, 20, order)?;
                    let disk_dquot = regions[index + 1].payload.clone();
                    if block_number >> 63 != 0
                        || block_count == 0
                        || byte_offset >> 31 != 0
                        || disk_dquot.len() != 104
                        || usize::try_from(byte_offset)
                            .ok()
                            .and_then(|offset| offset.checked_add(136))
                            .is_none_or(|end| {
                                end > usize::try_from(block_count)
                                    .unwrap_or(0)
                                    .saturating_mul(512)
                            })
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Dquot(XfsDquotReplayItem {
                        id,
                        block_number,
                        block_count,
                        byte_offset,
                        disk_dquot,
                    })
                }
                QUOTAOFF => {
                    if format.len() != 8 || count != 1 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Quotaoff {
                        flags: native_u32(format, 4, order)?,
                    }
                }
                EFI | EFD | RUI | CUI | BUI => {
                    if count != 1 || format.len() < 16 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let extent_count = usize::try_from(native_u32(format, 4, order)?)
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                    let id = native_u64(format, 8, order)?;
                    if id == 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let (intent_kind, extent_size) = match kind {
                        EFI | EFD => (XfsIntentKind::ExtentFree, 16usize),
                        RUI => (XfsIntentKind::Rmap, 32usize),
                        CUI => (XfsIntentKind::Refcount, 16usize),
                        BUI => (XfsIntentKind::Bmap, 32usize),
                        _ => return Err(XfsError::CorruptMetadata),
                    };
                    let bytes = 16usize
                        .checked_add(
                            extent_count
                                .checked_mul(extent_size)
                                .ok_or(XfsError::AddressOutOfRange)?,
                        )
                        .ok_or(XfsError::AddressOutOfRange)?;
                    if format.len() != bytes {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut extents = Vec::new();
                    extents
                        .try_reserve_exact(extent_count)
                        .map_err(|_| XfsError::NoMemory)?;
                    for number in 0..extent_count {
                        let offset = 16usize
                            .checked_add(
                                number
                                    .checked_mul(extent_size)
                                    .ok_or(XfsError::AddressOutOfRange)?,
                            )
                            .ok_or(XfsError::AddressOutOfRange)?;
                        let extent = match intent_kind {
                            XfsIntentKind::ExtentFree => {
                                let start_block = native_u64(format, offset, order)?;
                                let block_count = native_u32(format, offset + 8, order)?;
                                if start_block >> 63 != 0
                                    || block_count == 0
                                    || native_u32(format, offset + 12, order)? != 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::ExtentFree {
                                    start_block,
                                    block_count,
                                }
                            }
                            XfsIntentKind::Refcount => {
                                let start_block = native_u64(format, offset, order)?;
                                let block_count = native_u32(format, offset + 8, order)?;
                                let flags = native_u32(format, offset + 12, order)?;
                                if start_block >> 63 != 0
                                    || block_count == 0
                                    || flags & !0xff != 0
                                    || flags & 0xff == 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::Refcount {
                                    start_block,
                                    block_count,
                                    flags,
                                }
                            }
                            XfsIntentKind::Rmap | XfsIntentKind::Bmap => {
                                let owner = native_u64(format, offset, order)?;
                                let start_block = native_u64(format, offset + 8, order)?;
                                let start_offset = native_u64(format, offset + 16, order)?;
                                let block_count = native_u32(format, offset + 24, order)?;
                                let flags = native_u32(format, offset + 28, order)?;
                                let allowed = if intent_kind == XfsIntentKind::Rmap {
                                    0xe000_00ff
                                } else {
                                    0xe000_00ff
                                };
                                if owner == 0
                                    || start_block >> 63 != 0
                                    || block_count == 0
                                    || flags & !allowed != 0
                                    || flags & 0xff == 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::Mapping {
                                    owner,
                                    start_block,
                                    start_offset,
                                    block_count,
                                    flags,
                                }
                            }
                        };
                        extents.push(extent);
                    }
                    let key = XfsIntentKey {
                        kind: intent_kind,
                        id,
                    };
                    if kind == EFD {
                        XfsReplayItem::Done(XfsDoneReplayItem { key, extents })
                    } else {
                        XfsReplayItem::Intent(XfsIntentReplayItem { key, extents })
                    }
                }
                RUD | CUD | BUD => {
                    if format.len() != 16 || count != 1 || native_u32(format, 4, order)? != 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let intent_kind = match kind {
                        RUD => XfsIntentKind::Rmap,
                        CUD => XfsIntentKind::Refcount,
                        BUD => XfsIntentKind::Bmap,
                        _ => return Err(XfsError::CorruptMetadata),
                    };
                    let id = native_u64(format, 8, order)?;
                    if id == 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Done(XfsDoneReplayItem {
                        key: XfsIntentKey {
                            kind: intent_kind,
                            id,
                        },
                        extents: Vec::new(),
                    })
                }
                _ => return Err(XfsError::UnsupportedFeature),
            };
            result.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            result.push(item);
            index = end;
        }
        if result.len() != expected_items {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(result)
    }

    /// Decodes the buffer-log items in a transaction without guessing at
    /// region pairing.  Each `xfs_buf_log_format` owns exactly the following
    /// number of 128-byte BCHUNK regions indicated by its bitmap; any other
    /// item kind is rejected so a mixed transaction never receives a partial
    /// home-write commit.
    pub fn buffer_items(&self, order: XfsLogByteOrder) -> XfsResult<Vec<XfsBufferReplayItem>> {
        let regions = self.item_regions(order)?;
        let mut result = Vec::new();
        let mut index = 0usize;
        while index < regions.len() {
            let format = regions[index].payload.as_slice();
            if format.len() < 20 || native_u16(format, 0, order)? != 0x123c {
                return Err(XfsError::UnsupportedFeature);
            }
            let words = native_u32(format, 16, order)? as usize;
            let map_end = 20usize
                .checked_add(words.checked_mul(4).ok_or(XfsError::CorruptMetadata)?)
                .ok_or(XfsError::CorruptMetadata)?;
            if map_end != format.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut chunks = 0usize;
            for word in 0..words {
                chunks = chunks
                    .checked_add(native_u32(format, 20 + word * 4, order)?.count_ones() as usize)
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
            let start = index.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            let end = start
                .checked_add(chunks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if end > regions.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut chunk_regions = Vec::new();
            chunk_regions
                .try_reserve_exact(chunks)
                .map_err(|_| XfsError::NoMemory)?;
            for region in &regions[start..end] {
                if region.payload.len() != 128 {
                    return Err(XfsError::CorruptMetadata);
                }
                chunk_regions.push(region.payload.as_slice());
            }
            result.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            result.push(self.buffer_item(order, format, &chunk_regions)?);
            index = end;
        }
        Ok(result)
    }
}

/// Recovery framing state.  It deliberately owns no block writer: journal
/// decode and metadata application are separate failure domains, so a decode
/// error cannot leave half of a transaction written to the allocation groups.
#[derive(Default)]
pub struct XfsRecoveryPlan {
    open: Vec<XfsRecoveryTransaction>,
    committed: Vec<XfsRecoveryTransaction>,
    head_lsn: u64,
    tail_lsn: u64,
}

#[derive(Clone, Default)]
pub struct XfsIntentRecovery {
    pending: Vec<XfsIntentKey>,
    // Keep the authenticated operation with its key.  The key alone is
    // sufficient for done pairing, but not for replaying an EFI after the
    // complete log chain establishes that no EFD completed it.
    pending_items: Vec<XfsIntentReplayItem>,
    /// Completed keys are retained for the duration of a recovery pass.
    /// Recovery can be restarted after an I/O error, so an already applied
    /// done item must be a no-op rather than turning a clean retry into media
    /// corruption.
    completed: Vec<XfsIntentKey>,
}

impl XfsIntentRecovery {
    /// Registers one decoded intent/done item in journal order.  This
    /// registry deliberately accepts no buffer, inode, or dquot item: a
    /// caller that mixes an unimplemented item class into an intent replay
    /// transaction must fail before publishing a partial recovery result.
    ///
    /// The key includes the item class, so (for example) an EFD cannot
    /// complete a same-numbered RUI.  Repeating an already seen intent or
    /// done is harmless; this is needed when an interrupted recovery pass
    /// restarts from a durable checkpoint and presents that log region again.
    pub fn apply(&mut self, item: XfsReplayItem) -> XfsResult<()> {
        match item {
            XfsReplayItem::Intent(intent) => {
                let key = intent.key;
                // Exact duplicate intents occur when recovery repeats a
                // committed log region after a crash.  Neither duplicate may
                // allocate/free an extent a second time.
                if let Some(existing) = self
                    .pending_items
                    .iter()
                    .find(|existing| existing.key == key)
                {
                    // A replay retry may present the same intent again, but
                    // the identifier cannot be used to smuggle in a second
                    // extent vector.
                    if existing != &intent {
                        return Err(XfsError::CorruptMetadata);
                    }
                    return Ok(());
                }
                if self.completed.iter().any(|existing| *existing == key) {
                    return Ok(());
                }
                self.pending
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.pending_items
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.pending.push(key);
                self.pending_items.push(intent);
            }
            XfsReplayItem::Done(done) => {
                let key = done.key;
                if self.completed.iter().any(|existing| *existing == key) {
                    return Ok(());
                }
                // The retained log window may begin after the matching
                // intent but still contain its done item.  Such a done only
                // proves that there is no pending in-window operation; it is
                // not media corruption and must be a no-op.
                let Some(index) = self.pending.iter().position(|existing| *existing == key) else {
                    return Ok(());
                };
                self.pending.remove(index);
                let item_index = self
                    .pending_items
                    .iter()
                    .position(|existing| existing.key == key)
                    .ok_or(XfsError::CorruptMetadata)?;
                self.pending_items.remove(item_index);
                self.completed
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.completed.push(key);
            }
            XfsReplayItem::Buffer(_)
            | XfsReplayItem::Inode(_)
            | XfsReplayItem::Dquot(_)
            | XfsReplayItem::Quotaoff { .. } => {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        Ok(())
    }

    /// Applies every decoded item of one committed transaction atomically to
    /// the intent registry.  An out-of-order done item, mismatched item kind,
    /// or unsupported mixed item leaves the registry unchanged.  This is the
    /// recovery boundary used before a caller may act on the returned pending
    /// intents.  Ordinary replay items are admitted here but deliberately
    /// left for the transaction materializer; their presence must not make a
    /// valid mixed transaction look corrupt.
    pub fn apply_transaction(&mut self, items: &[XfsReplayItem]) -> XfsResult<()> {
        if items.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut next = self.clone();
        for item in items {
            match item {
                XfsReplayItem::Intent(_) | XfsReplayItem::Done(_) => next.apply(item.clone())?,
                XfsReplayItem::Buffer(_)
                | XfsReplayItem::Inode(_)
                | XfsReplayItem::Dquot(_)
                | XfsReplayItem::Quotaoff { .. } => {}
            }
        }
        *self = next;
        Ok(())
    }

    /// Returns whether this exact intent class and identifier still requires
    /// replay.  Callers must not treat an absent key as permission to apply a
    /// decoded operation: it may instead have been completed or never have
    /// appeared in the authenticated log chain.
    pub fn is_pending(&self, key: XfsIntentKey) -> bool {
        self.pending.iter().any(|existing| *existing == key)
    }

    pub fn pending(&self) -> &[XfsIntentKey] {
        &self.pending
    }
    /// Authenticated pending operations in their original intent order.
    /// Callers must still select a semantic engine by kind; this registry
    /// deliberately never treats a decoded rmap/refcount/bmap payload as an
    /// allocator operation.
    pub fn pending_items(&self) -> &[XfsIntentReplayItem] {
        &self.pending_items
    }
    pub fn completed(&self) -> &[XfsIntentKey] {
        &self.completed
    }
}

impl XfsLogRecordHeader {
    /// A physical XFS log record always reserves one basic block for its
    /// header.  The typed fields finish at byte 328; the remaining bytes are
    /// part of the on-disk header, not the first log operation.
    pub const ENCODED_LEN: usize = 512;
    const CRC_OFFSET: usize = 32;

    fn header_bytes_for(iclog_bytes: u32) -> XfsResult<usize> {
        if iclog_bytes == 0 {
            return Ok(Self::ENCODED_LEN);
        }
        let windows = (usize::try_from(iclog_bytes)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_add(32 * 1024 - 1)
            .ok_or(XfsError::AddressOutOfRange)?)
            / (32 * 1024);
        windows
            .checked_mul(XFS_LOG_BASIC_BLOCK)
            .ok_or(XfsError::AddressOutOfRange)
    }

    fn header_bytes(&self) -> XfsResult<usize> {
        Self::header_bytes_for(self.iclog_bytes)
    }

    fn minimal_iclog_bytes(operations: &[XfsLogOperation]) -> XfsResult<u32> {
        let mut payload = 0usize;
        for operation in operations {
            payload = payload
                .checked_add(12)
                .and_then(|value| value.checked_add(operation.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        payload = align8(payload).ok_or(XfsError::AddressOutOfRange)?;
        let mut iclog = 32 * 1024usize;
        while Self::header_bytes_for(
            u32::try_from(iclog).map_err(|_| XfsError::AddressOutOfRange)?,
        )?
        .checked_add(payload)
        .ok_or(XfsError::AddressOutOfRange)?
            > iclog
        {
            iclog = iclog
                .checked_add(32 * 1024)
                .ok_or(XfsError::AddressOutOfRange)?;
            if iclog > 256 * 1024 {
                return Err(XfsError::AddressOutOfRange);
            }
        }
        u32::try_from(iclog).map_err(|_| XfsError::AddressOutOfRange)
    }

    /// Constructs a Linux-x86 log record header for a complete set of
    /// physical operation regions.  The operation wire remains explicitly
    /// supplied; this constructor merely derives the aligned `h_len` and
    /// binds it to the ring reservation's LSN/cycle.
    pub fn for_operations(
        cycle: u32,
        lsn: u64,
        tail_lsn: u64,
        previous_block: u32,
        filesystem_uuid: XfsUuid,
        iclog_bytes: u32,
        operations: &[XfsLogOperation],
    ) -> XfsResult<Self> {
        let mut bytes = 0usize;
        for operation in operations {
            bytes = bytes
                .checked_add(12)
                .and_then(|value| value.checked_add(operation.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        let payload_bytes = align8(bytes).ok_or(XfsError::AddressOutOfRange)?;
        if Self::header_bytes_for(iclog_bytes)?
            .checked_add(payload_bytes)
            .ok_or(XfsError::AddressOutOfRange)?
            > iclog_bytes as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        Ok(Self {
            cycle,
            version: 2,
            payload_bytes: u32::try_from(payload_bytes).map_err(|_| XfsError::AddressOutOfRange)?,
            lsn,
            tail_lsn,
            previous_block,
            operation_count: u32::try_from(operations.len())
                .map_err(|_| XfsError::AddressOutOfRange)?,
            cycle_data: [0; 64],
            format: 1,
            filesystem_uuid,
            iclog_bytes,
        })
    }

    fn parse(bytes: &[u8], expected_uuid: XfsUuid, require_crc: bool) -> XfsResult<Self> {
        if bytes.len() < Self::ENCODED_LEN || be32(bytes, 0)? != XFS_LOG_RECORD_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        let payload_bytes = be32(bytes, 12)?;
        if payload_bytes & 7 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut filesystem_uuid = [0; 16];
        filesystem_uuid.copy_from_slice(slice(bytes, 304, 16)?);
        let filesystem_uuid = XfsUuid(filesystem_uuid);
        if filesystem_uuid != expected_uuid {
            return Err(XfsError::CorruptMetadata);
        }
        let mut cycle_data = [0; 64];
        for (index, value) in cycle_data.iter_mut().enumerate() {
            *value = be32(bytes, 44 + index * 4)?;
        }
        let header = Self {
            cycle: be32(bytes, 4)?,
            version: be32(bytes, 8)?,
            payload_bytes,
            lsn: be64(bytes, 16)?,
            tail_lsn: be64(bytes, 24)?,
            previous_block: be32(bytes, 36)?,
            operation_count: be32(bytes, 40)?,
            cycle_data,
            format: be32(bytes, 300)?,
            filesystem_uuid,
            iclog_bytes: be32(bytes, 320)?,
        };
        if header.cycle == 0 || header.lsn == 0 || (header.lsn >> 32) as u32 != header.cycle {
            return Err(XfsError::CorruptMetadata);
        }
        let logical_end = header
            .header_bytes()?
            .checked_add(header.payload_bytes as usize)
            .ok_or(XfsError::CorruptMetadata)?;
        let record_end = align_log_basic_block(logical_end)?;
        if require_crc {
            verify_log_record_crc(slice(bytes, 0, record_end)?, &header)?;
        }
        Ok(header)
    }

    /// Encodes a single complete physical record image.  This is deliberately
    /// only the record wire codec: log-ring placement, cycle replacement in
    /// overwritten sectors, AIL insertion, and checkpointing remain owned by
    /// the transaction coordinator.  The caller supplies already-framed log
    /// operations and cannot smuggle a host pointer or VFS object into the
    /// persistent format.
    pub fn encode(&self, operations: &[XfsLogOperation]) -> XfsResult<Vec<u8>> {
        if self.cycle == 0
            || self.lsn == 0
            || (self.lsn >> 32) as u32 != self.cycle
            || self.filesystem_uuid.0 == [0; 16]
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(self.payload_bytes as usize)
            .map_err(|_| XfsError::NoMemory)?;
        for operation in operations {
            let length =
                u32::try_from(operation.payload.len()).map_err(|_| XfsError::AddressOutOfRange)?;
            payload
                .try_reserve(12 + operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            payload.extend_from_slice(&operation.transaction_id.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
            payload.push(operation.client_id);
            payload.push(operation.flags);
            payload.extend_from_slice(&[0, 0]);
            payload.extend_from_slice(&operation.payload);
        }
        let aligned = align8(payload.len()).ok_or(XfsError::AddressOutOfRange)?;
        if aligned != self.payload_bytes as usize
            || self.operation_count as usize != operations.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        payload.resize(aligned, 0);
        let header_bytes = self.header_bytes()?;
        let total = align_log_basic_block(
            header_bytes
                .checked_add(payload.len())
                .ok_or(XfsError::AddressOutOfRange)?,
        )?;
        let mut record = vec![0; total];
        put_be32(&mut record, 0, XFS_LOG_RECORD_MAGIC)?;
        put_be32(&mut record, 4, self.cycle)?;
        put_be32(&mut record, 8, self.version)?;
        put_be32(&mut record, 12, self.payload_bytes)?;
        put_be64(&mut record, 16, self.lsn)?;
        put_be64(&mut record, 24, self.tail_lsn)?;
        put_be32(&mut record, 36, self.previous_block)?;
        put_be32(&mut record, 40, self.operation_count)?;
        for (index, value) in self.cycle_data.iter().enumerate() {
            put_be32(&mut record, 44 + index * 4, *value)?;
        }
        put_be32(&mut record, 300, self.format)?;
        record[304..320].copy_from_slice(&self.filesystem_uuid.0);
        put_be32(&mut record, 320, self.iclog_bytes)?;
        // Every extra 32KiB payload window adds a 512-byte extension header.
        // Its cycle-data table is initialized to zero and receives displaced
        // data words during physical cycle stamping.
        for extension in 1..header_bytes / XFS_LOG_BASIC_BLOCK {
            put_be32(&mut record, extension * XFS_LOG_BASIC_BLOCK, self.cycle)?;
        }
        record[header_bytes..header_bytes + payload.len()].copy_from_slice(&payload);
        rewrite_log_record_crc(&mut record, self)?;
        Ok(record)
    }
}

impl XfsJournalRecord {
    pub fn item_byte_order(&self) -> XfsResult<XfsLogByteOrder> {
        match self.header.format {
            1 => Ok(XfsLogByteOrder::Little),
            2 | 3 => Ok(XfsLogByteOrder::Big),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    /// Parses the native-endian 16-byte transaction header carried by a
    /// `TRANSHDR` operation.  It is intentionally separate from physical
    /// operation framing, whose fields are always big-endian.
    pub fn transaction_header(&self, payload: &[u8]) -> XfsResult<XfsTransactionHeader> {
        if payload.len() != 16 {
            return Err(XfsError::CorruptMetadata);
        }
        let order = self.item_byte_order()?;
        let read = |offset| native_u32(payload, offset, order);
        if read(0)? != 0x5452_414e {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsTransactionHeader {
            transaction_type: read(4)?,
            item_count: read(12)?,
        })
    }

    /// Decodes operation framing from one complete record image.  Continuation
    /// flags are retained verbatim; joining fragments belongs to the recovery
    /// state machine, never to a lossy per-sector reader.
    pub fn decode(bytes: &[u8], expected_uuid: XfsUuid) -> XfsResult<Self> {
        Self::decode_with_crc(bytes, expected_uuid, false)
    }

    pub fn decode_with_crc(
        bytes: &[u8],
        expected_uuid: XfsUuid,
        require_crc: bool,
    ) -> XfsResult<Self> {
        let header = XfsLogRecordHeader::parse(bytes, expected_uuid, require_crc)?;
        let payload_start = header.header_bytes()?;
        let payload_end = payload_start
            .checked_add(header.payload_bytes as usize)
            .ok_or(XfsError::CorruptMetadata)?;
        let payload = slice(bytes, payload_start, payload_end - payload_start)?;
        let mut cursor = 0usize;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(header.operation_count as usize)
            .map_err(|_| XfsError::NoMemory)?;
        for _ in 0..header.operation_count {
            let op_header = slice(payload, cursor, 12)?;
            let transaction_id = be32(op_header, 0)?;
            let length = be32(op_header, 4)? as usize;
            let client_id = byte(op_header, 8)?;
            let flags = byte(op_header, 9)?;
            cursor = cursor.checked_add(12).ok_or(XfsError::CorruptMetadata)?;
            let data = slice(payload, cursor, length)?.to_vec();
            cursor = cursor
                .checked_add(length)
                .ok_or(XfsError::CorruptMetadata)?;
            operations.push(XfsLogOperation {
                transaction_id,
                client_id,
                flags,
                payload: data,
            });
        }
        if cursor > payload.len() || payload[cursor..].iter().any(|byte| *byte != 0) {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(Self { header, operations })
    }
}

impl XfsRecoveryPlan {
    pub const fn new() -> Self {
        Self {
            open: Vec::new(),
            committed: Vec::new(),
            head_lsn: 0,
            tail_lsn: 0,
        }
    }

    /// Adds one record in log order.  Continuations cannot create a
    /// transaction on their own, and a second start for an open id is media
    /// corruption rather than an invitation to overwrite pending operations.
    pub fn ingest(&mut self, record: XfsJournalRecord) -> XfsResult<()> {
        if record.header.lsn == 0 || record.header.tail_lsn > record.header.lsn {
            return Err(XfsError::CorruptMetadata);
        }
        if self.head_lsn != 0 && record.header.lsn <= self.head_lsn {
            // The scanner feeds physical records in log order.  Duplicate or
            // backwards LSNs are not a wrap shortcut: they are stale/corrupt
            // input which must not be replayed as a second transaction.
            return Err(XfsError::CorruptMetadata);
        }
        let record_lsn = record.header.lsn;
        let record_order = record.item_byte_order()?;
        self.head_lsn = record_lsn;
        self.tail_lsn = if self.tail_lsn == 0 {
            record.header.tail_lsn
        } else {
            cmp::min(self.tail_lsn, record.header.tail_lsn)
        };
        for operation in record.operations {
            let flags = operation.flags;
            let starts = flags & XLOG_START_TRANS != 0;
            let commits = flags & XLOG_COMMIT_TRANS != 0;
            let continues =
                flags & (XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS) != 0;
            let existing = self
                .open
                .iter()
                .position(|entry| entry.transaction_id == operation.transaction_id);
            let index = match (starts, existing) {
                (true, Some(_)) => return Err(XfsError::CorruptMetadata),
                (true, None) => {
                    self.open.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    self.open.push(XfsRecoveryTransaction {
                        transaction_id: operation.transaction_id,
                        lsn: record_lsn,
                        byte_order: record_order,
                        operations: Vec::new(),
                        pending_operation: None,
                    });
                    self.open.len() - 1
                }
                (false, Some(index)) => index,
                (false, None) if continues || commits => return Err(XfsError::CorruptMetadata),
                (false, None) => return Err(XfsError::CorruptMetadata),
            };
            self.open[index]
                .operations
                .try_reserve(1)
                .map_err(|_| XfsError::NoMemory)?;
            if self.open[index].byte_order != record_order {
                return Err(XfsError::CorruptMetadata);
            }
            self.open[index].push_region(operation)?;
            self.open[index].lsn = record_lsn;
            if commits {
                self.open[index].complete()?;
                let complete = self.open.remove(index);
                self.committed
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.committed.push(complete);
            }
        }
        Ok(())
    }

    /// Consumes completed transactions in journal order.  Open transactions
    /// are intentionally not returned: they have no commit proof and must not
    /// be replayed after a crash.
    pub fn into_committed(self) -> Vec<XfsRecoveryTransaction> {
        self.committed
    }

    pub fn state(&self) -> XfsJournalRecoveryState {
        XfsJournalRecoveryState {
            head_lsn: self.head_lsn,
            tail_lsn: self.tail_lsn,
            committed_transactions: self.committed.len(),
            interrupted_transactions: self.open.len(),
        }
    }

    /// Builds all-buffer home-write commits from the complete transactions in
    /// this plan.  Transactions containing inode, dquot, intent, or other
    /// item types fail closed here; they need their corresponding allocator
    /// and quota replay coordinator and must not be split into a subset of
    /// buffer writes.
    pub fn prepare_buffer_commits(&self, volume: &XfsVolume) -> XfsResult<Vec<XfsRecoveryCommit>> {
        let mut commits = Vec::new();
        commits
            .try_reserve_exact(self.committed.len())
            .map_err(|_| XfsError::NoMemory)?;
        for transaction in &self.committed {
            if transaction.operations.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            // Decode every item before selecting a replay engine.  This
            // distinguishes an unsupported but well-formed inode/intent item
            // from corrupt framing and prevents a buffer subset from being
            // applied out of a mixed committed transaction.
            let decoded = transaction.replay_items(transaction.byte_order)?;
            let mut buffers = Vec::new();
            buffers
                .try_reserve_exact(decoded.len())
                .map_err(|_| XfsError::NoMemory)?;
            for item in decoded {
                match item {
                    XfsReplayItem::Buffer(buffer) => buffers.push(buffer),
                    _ => return Err(XfsError::UnsupportedFeature),
                }
            }
            if buffers.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            commits.push(volume.prepare_recovery_commit(transaction.lsn, &buffers)?);
        }
        Ok(commits)
    }

    /// Resolves the complete typed intent/done history before exposing any
    /// pending semantic operation.  Every item in a mixed transaction is
    /// decoded before its intent/done contribution is admitted; ordinary
    /// items are materialized by the transaction writer, not discarded here.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    fn pending_extent_free_recovery(&self) -> XfsResult<Vec<XfsPendingExtentFreeRecovery>> {
        let mut registry = XfsIntentRecovery::default();
        let mut observed = Vec::<(u64, XfsIntentReplayItem)>::new();
        for transaction in &self.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            if items.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            for item in &items {
                match item {
                    XfsReplayItem::Intent(intent) => {
                        observed.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        observed.push((transaction.lsn, intent.clone()));
                    }
                    XfsReplayItem::Done(_)
                    | XfsReplayItem::Buffer(_)
                    | XfsReplayItem::Inode(_)
                    | XfsReplayItem::Dquot(_)
                    | XfsReplayItem::Quotaoff { .. } => {}
                }
            }
            // The registry clone-and-commit operation is the transaction
            // boundary: malformed done ordering cannot leave an earlier
            // item visible to semantic replay.
            registry.apply_transaction(&items)?;
        }

        let mut steps = Vec::<XfsPendingExtentFreeRecovery>::new();
        for (lsn, intent) in observed {
            if !registry.is_pending(intent.key) {
                continue;
            }
            if intent.key.kind != XfsIntentKind::ExtentFree {
                // RUI/CUI/BUI require respectively rmapbt, refcountbt, and
                // inode/bmap metadata mutations.  This volume has no
                // atomic writer for those layouts, so a pending operation is
                // never mistaken for an allocator free.
                return Err(XfsError::UnsupportedFeature);
            }
            let mut extents = Vec::new();
            extents
                .try_reserve_exact(intent.extents.len())
                .map_err(|_| XfsError::NoMemory)?;
            for extent in intent.extents {
                let XfsLogReplayExtent::ExtentFree {
                    start_block,
                    block_count,
                } = extent
                else {
                    return Err(XfsError::CorruptMetadata);
                };
                extents.push((start_block, block_count));
            }
            if extents.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            if let Some(step) = steps.last_mut().filter(|step| step.lsn == lsn) {
                step.extents
                    .try_reserve(extents.len())
                    .map_err(|_| XfsError::NoMemory)?;
                step.extents.extend(extents);
            } else {
                steps.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                steps.push(XfsPendingExtentFreeRecovery { lsn, extents });
            }
        }
        Ok(steps)
    }
}

impl XfsExtent {
    fn parse(bytes: &[u8]) -> XfsResult<Self> {
        let encoded = u128::from_be_bytes(
            slice(bytes, 0, 16)?
                .try_into()
                .map_err(|_| XfsError::CorruptMetadata)?,
        );
        let file_block = ((encoded >> 73) & ((1u128 << 54) - 1)) as u64;
        let start_block = ((encoded >> 21) & ((1u128 << 52) - 1)) as u64;
        let block_count = (encoded & ((1u128 << 21) - 1)) as u32;
        if block_count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(Self {
            unwritten: encoded >> 127 != 0,
            file_block,
            start_block,
            block_count,
        })
    }
}

/// A read-only view of a data device plus optional external log and realtime
/// devices.  Device membership is explicit: an XFS external log/realtime
/// device is never guessed from a pathname or a device number.
pub struct XfsVolume {
    data: BlockVolume,
    external_log: Option<BlockVolume>,
    realtime: Option<BlockVolume>,
    rtgroup_inodes: Vec<(u64, u64)>,
    superblock: XfsSuperblock,
    /// Serializes durable home-block replay.  The log itself supplies the
    /// transaction order; this lock only prevents two recovery/teardown
    /// callers from observing and advancing the same home LSN concurrently.
    replay_lock: SpinMutex<()>,
    // Keeps the mount claim alive for `probe`.  Generic `open` takes already
    // owned volumes and intentionally leaves claim ownership with its caller.
    _data_claim: Option<MountedBlockDevice>,
}

impl XfsVolume {
    fn metadir_child(&self, parent: u64, name: &[u8]) -> XfsResult<u64> {
        let parent_inode = self.inode(parent)?;
        if parent_inode.mode & 0o170000 != 0o040000 || !parent_inode.is_metadata_inode() {
            return Err(XfsError::CorruptMetadata);
        }
        let entry = self
            .directory_entries(parent)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(XfsError::CorruptMetadata)?;
        let child = self.inode(entry.inode)?;
        if !child.is_metadata_inode() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(entry.inode)
    }

    fn rtgroup_metadata_inodes(&self, group: u32) -> XfsResult<(u64, u64)> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0
            || group >= self.superblock.rtgroup_count
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let rtgroups = self.metadir_child(self.superblock.metadir_inode, b"rtgroups")?;
        let mut bitmap = alloc::format!("{group}").into_bytes();
        bitmap.extend_from_slice(b".bitmap");
        let mut summary = alloc::format!("{group}").into_bytes();
        summary.extend_from_slice(b".summary");
        let bitmap = self.metadir_child(rtgroups, &bitmap)?;
        let summary = self.metadir_child(rtgroups, &summary)?;
        for (inode_number, metafile_type) in [(bitmap, 5u16), (summary, 6u16)] {
            let inode = self.inode(inode_number)?;
            // The rtgroup loader binds names, metadata-inode type, owning
            // project, and data-fork representation before recovery derives
            // a physical bitmap/summary home from it.
            if inode.mode & 0o170000 != 0o100000
                || !inode.is_metadata_inode()
                || inode.project_id != group
                || inode.metafile_type != Some(metafile_type)
                || !matches!(
                    inode.data_format,
                    XfsForkFormat::Extents | XfsForkFormat::Btree
                )
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        Ok((bitmap, summary))
    }
    /// Replays legacy v4 records only when every logged buffer is a complete
    /// home image.  v4 metadata has neither the v5 LSN nor CRC ownership
    /// fields used by the writable coordinator, so partial-region replay is
    /// not restart-safe.  A full-image operation is: the current home image
    /// is either the captured preimage (write it) or the exact logged image
    /// (a prior FUA write completed); any third state fails closed.
    ///
    /// This intentionally does not manufacture a v4 log tail or publish a
    /// writable log ring.  The caller may expose the verified read
    /// projection after all home images have reached stable storage.
    pub fn replay_v4_whole_image_plan(&self, plan: &XfsRecoveryPlan) -> XfsResult<()> {
        if self.superblock.is_v5() || self.data.geometry().block_size % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        for transaction in &plan.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            if items.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut writes = Vec::<(u64, Vec<u8>)>::new();
            for item in items {
                let XfsReplayItem::Buffer(buffer) = item else {
                    // A v4 inode/dquot/intent item has no durable whole-home
                    // image proof.  Never replay a buffer subset of it.
                    return Err(XfsError::UnsupportedFeature);
                };
                if buffer.flags & XFS_BLF_CANCEL != 0 {
                    return Err(XfsError::UnsupportedFeature);
                }
                let chunks = usize::from(buffer.block_count)
                    .checked_mul(4)
                    .ok_or(XfsError::AddressOutOfRange)?;
                if buffer.block_count == 0
                    || buffer.dirty_chunks.len() != chunks
                    || buffer.chunks.len() != chunks
                    || buffer
                        .dirty_chunks
                        .iter()
                        .enumerate()
                        .any(|(index, chunk)| *chunk != index as u32)
                    || buffer.chunks.iter().any(|chunk| chunk.len() != 128)
                {
                    return Err(XfsError::UnsupportedFeature);
                }
                let end = buffer
                    .block_number
                    .checked_add(u64::from(buffer.block_count))
                    .ok_or(XfsError::AddressOutOfRange)?;
                if end > self.basic_blocks(&self.data)? {
                    return Err(XfsError::AddressOutOfRange);
                }
                let mut image = Vec::new();
                image
                    .try_reserve_exact(chunks.checked_mul(128).ok_or(XfsError::AddressOutOfRange)?)
                    .map_err(|_| XfsError::NoMemory)?;
                for chunk in buffer.chunks {
                    image.extend_from_slice(&chunk);
                }
                if writes
                    .iter()
                    .any(|(block, _)| *block == buffer.block_number)
                {
                    return Err(XfsError::CorruptMetadata);
                }
                writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                writes.push((buffer.block_number, image));
            }
            writes.sort_unstable_by_key(|(block, _)| *block);
            for (block, image) in &writes {
                let mut current = vec![0; image.len()];
                self.read_basic_blocks(&self.data, *block, &mut current)?;
                if current == *image {
                    continue;
                }
                // Full-image redo does not depend on an on-disk LSN: the
                // authenticated log image replaces any older home image, and
                // a completed FUA write is recognized by byte equality.
                self.write_basic_blocks_fua(&self.data, *block, image)?;
            }
            self.data.flush().map_err(XfsError::from)?;
        }
        Ok(())
    }

    /// Reserves and encodes one native physical log record.  No I/O occurs in
    /// this phase; if later persistence fails the returned object remains the
    /// sole retry token for the already consumed ring grant.
    pub(crate) fn prepare_live_log_commit(
        &self,
        ring: &mut XfsLogRing,
        transaction_id: u32,
        tail_lsn: u64,
        operations: &[XfsLogOperation],
    ) -> XfsResult<XfsPreparedLogCommit> {
        if !self.superblock.is_v5()
            || self.data.geometry().block_size % XFS_LOG_BASIC_BLOCK != 0
            || transaction_id == 0
            || operations.is_empty()
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let region_blocks = self.log_region_blocks()?;
        if ring.blocks() != region_blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let iclog_bytes = XfsLogRecordHeader::minimal_iclog_bytes(operations)?;
        let header = XfsLogRecordHeader::for_operations(
            ring.cycle(),
            ring.next_lsn(),
            tail_lsn,
            ring.previous_record(),
            self.superblock.uuid,
            iclog_bytes,
            operations,
        )?;
        let mut record = header.encode(operations)?;
        let reservation = ring.reserve(record.len())?;
        if reservation.lsn != header.lsn {
            return Err(XfsError::CorruptMetadata);
        }
        ring.stamp_record(&reservation, &mut record)?;
        Ok(XfsPreparedLogCommit {
            reservation,
            transaction_id,
            record,
        })
    }

    /// Writes every physical fragment with FUA, flushes the log member, then
    /// publishes the transaction to the AIL.  Allocation for AIL insertion is
    /// completed before the first device write.  Therefore any I/O failure
    /// leaves `prepared` intact and absent from AIL, permitting an exact retry
    /// of the same LSN and bytes rather than a second transaction.
    pub(crate) fn persist_live_log_commit(
        &self,
        prepared: &XfsPreparedLogCommit,
        ail: &mut XfsAil,
    ) -> XfsResult<()> {
        if prepared.record.len()
            != prepared.reservation.record_blocks as usize * XFS_LOG_BASIC_BLOCK
        {
            return Err(XfsError::CorruptMetadata);
        }
        let ail_entry = XfsAilEntry {
            lsn: prepared.reservation.lsn,
            end_lsn: prepared.reservation.end_lsn(self.log_region_blocks()?)?,
            transaction_id: prepared.transaction_id,
            checkpoint_homes: Vec::new(),
        };
        ail.reserve_insert(&ail_entry)?;
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let fragments = prepared.reservation.fragments()?;
        let mut offset = 0usize;
        for fragment in fragments {
            let bytes = fragment.blocks as usize * XFS_LOG_BASIC_BLOCK;
            let end = offset
                .checked_add(bytes)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = base
                .checked_add(fragment.start_block as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            self.write_basic_blocks_fua(log, start, &prepared.record[offset..end])?;
            offset = end;
        }
        if offset != prepared.record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        log.flush().map_err(XfsError::from)?;
        ail.insert_reserved(ail_entry)
    }

    /// Writes the terminal unmount record without placing it in the AIL:
    /// unlike metadata transactions it has no home image to checkpoint.  Its
    /// caller may move the tail to the record end only after all preceding
    /// metadata was pushed and every member device was made durable.
    pub(crate) fn persist_clean_unmount_record(
        &self,
        prepared: &XfsPreparedLogCommit,
    ) -> XfsResult<()> {
        if prepared.record.len()
            != prepared.reservation.record_blocks as usize * XFS_LOG_BASIC_BLOCK
        {
            return Err(XfsError::CorruptMetadata);
        }
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let mut offset = 0usize;
        for fragment in prepared.reservation.fragments()? {
            let bytes = fragment.blocks as usize * XFS_LOG_BASIC_BLOCK;
            let end = offset
                .checked_add(bytes)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = base
                .checked_add(fragment.start_block as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            self.write_basic_blocks_fua(log, start, &prepared.record[offset..end])?;
            offset = end;
        }
        if offset != prepared.record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        log.flush().map_err(XfsError::from)
    }

    /// Pushes AIL entries through a caller-provided home-write routine.  The
    /// tail moves only after every selected item has completed and the caller
    /// has made its home writes durable; a failed push preserves both AIL and
    /// ring tail for retry.
    pub(crate) fn checkpoint_live_log(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        through_lsn: u64,
        mut push_home: impl FnMut(&XfsAilEntry) -> XfsResult<()>,
    ) -> XfsResult<()> {
        let count = ail
            .entries
            .partition_point(|entry| entry.lsn <= through_lsn);
        if count == 0 {
            return Ok(());
        }
        for entry in &ail.entries[..count] {
            push_home(entry)?;
        }
        // Every selected home image is FUA-written by the pusher.  The
        // device flush is the final fence: advancing the log tail before it
        // succeeds would make a power loss unrecoverable.
        self.data.flush().map_err(XfsError::from)?;
        // A record is reclaimable only from the first free position *after*
        // its physical log image, never from the record header itself.
        ring.checkpoint_tail(ail.entries[count - 1].end_lsn)?;
        let _ = ail.checkpoint_through(through_lsn);
        Ok(())
    }

    /// Starts restartable replay for a plan consisting solely of supported
    /// buffer items.  Mixed transactions are rejected by the plan decoder;
    /// callers must not attempt to expose a volume after replaying only its
    /// directory or inode subset.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn begin_buffer_recovery(
        self: &Arc<Self>,
        plan: &XfsRecoveryPlan,
    ) -> XfsResult<XfsRecoverySession> {
        Ok(XfsRecoverySession {
            volume: self.clone(),
            commits: plan.prepare_buffer_commits(self)?,
            next: 0,
        })
    }

    /// Adds one native inode log item to an all-or-nothing metadata batch.
    /// The logged buffer address is cross-checked against the inode number so
    /// a journal item cannot redirect an otherwise valid inode core.
    pub fn stage_inode_log_replay(
        &self,
        item: &XfsInodeReplayItem,
        lsn: u64,
        order: XfsLogByteOrder,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (ag, agino) = self.split_inode_number(item.inode)?;
        let fs_block = u64::from(ag)
            .checked_mul(u64::from(self.superblock.ag_blocks))
            .and_then(|base| base.checked_add(agino >> self.superblock.inodes_per_block_log))
            .ok_or(XfsError::AddressOutOfRange)?;
        let inode_offset =
            usize::try_from(agino & (u64::from(self.superblock.inodes_per_block) - 1))
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(self.superblock.inode_size as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
        let basic_per_fs = u64::from(self.superblock.block_size) / 512;
        let expected_byte = fs_block
            .checked_mul(basic_per_fs)
            .and_then(|block| block.checked_mul(512))
            .and_then(|base| base.checked_add(inode_offset as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let logged_byte = item
            .block_number
            .checked_mul(512)
            .and_then(|base| base.checked_add(u64::from(item.byte_offset)))
            .ok_or(XfsError::AddressOutOfRange)?;
        let image_bytes = usize::try_from(item.block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        if item.block_count == 0
            || expected_byte != logged_byte
            || usize::try_from(item.byte_offset)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_add(self.superblock.inode_size as usize)
                .filter(|end| *end <= image_bytes)
                .is_none()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut after = vec![0; image_bytes];
        self.read_basic_blocks(&self.data, item.block_number, &mut after)?;
        let before = after.clone();
        let offset = item.byte_offset as usize;
        let inode = item.materialize_home_inode(
            &after[offset..offset + self.superblock.inode_size as usize],
            lsn,
            self.superblock.is_v5().then_some(self.superblock.uuid),
            order,
        )?;
        after[offset..offset + inode.len()].copy_from_slice(&inode);
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Inode,
            basic_block: item.block_number,
            before,
            after,
        });
        Ok(())
    }

    /// Adds a dquot home update only after binding its physical log address to
    /// the selected on-disk quota inode and extent map.
    pub fn stage_dquot_log_replay(
        &self,
        item: &XfsDquotReplayItem,
        lsn: u64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let bigtime = self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0;
        let dquot = item.parse_disk_dquot(
            self.superblock.is_v5(),
            self.superblock.is_v5().then_some(self.superblock.meta_uuid),
            bigtime,
        )?;
        let (basic_block, byte_offset, block_count) =
            self.dquot_location(dquot.quota_type, item.id)?;
        if item.block_number != basic_block
            || item.byte_offset != byte_offset
            || item.block_count != block_count
        {
            return Err(XfsError::CorruptMetadata);
        }
        let image_bytes = usize::try_from(block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        let offset = byte_offset as usize;
        let mut image = vec![0; image_bytes];
        self.read_basic_blocks(&self.data, item.block_number, &mut image)?;
        let home = slice(&image, offset, 136)?;
        let _ = XfsDquot::parse(
            home,
            item.id,
            dquot.quota_type,
            self.superblock.meta_uuid,
            bigtime,
        )?;
        if be64(home, 112)? >= lsn {
            return Ok(());
        }
        let payload = item.materialize_home_dquot(
            home,
            lsn,
            self.superblock.is_v5(),
            self.superblock.is_v5().then_some(self.superblock.meta_uuid),
            bigtime,
        )?;
        transaction.dquots.push(XfsDquotDelta {
            id: item.id,
            quota_type: dquot.quota_type,
            basic_block: item.block_number,
            block_count: item.block_count,
            byte_offset: item.byte_offset,
            before: home.to_vec(),
            after: payload,
        });
        Ok(())
    }

    /// Starts typed semantic recovery for pending EFI operations.  The plan
    /// is completely decoded and every intent/done pair is resolved before
    /// the session is returned; pending RUI/CUI/BUI and mixed transactions
    /// fail closed because their metadata writers are not implemented.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn begin_intent_recovery(
        self: &Arc<Self>,
        plan: &XfsRecoveryPlan,
    ) -> XfsResult<XfsIntentRecoverySession> {
        if !self.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(XfsIntentRecoverySession {
            volume: self.clone(),
            steps: plan.pending_extent_free_recovery()?,
            next: 0,
            prepared: None,
        })
    }

    // Journal recovery path in progress.
    #[allow(dead_code)]
    fn prepare_pending_extent_free_recovery(
        &self,
        step: &XfsPendingExtentFreeRecovery,
    ) -> XfsResult<Option<XfsRecoveryCommit>> {
        if step.lsn == 0 || step.extents.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let ag_blocks = u64::from(self.superblock.ag_blocks);
        let mut groups = Vec::<(u32, Vec<(u32, u32)>)>::new();
        for &(start_block, block_count) in &step.extents {
            let end = start_block
                .checked_add(u64::from(block_count))
                .ok_or(XfsError::AddressOutOfRange)?;
            if block_count == 0 || end > self.superblock.data_blocks {
                return Err(XfsError::AddressOutOfRange);
            }
            let mut start = start_block;
            while start < end {
                let ag =
                    u32::try_from(start / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.superblock.ag_count {
                    return Err(XfsError::AddressOutOfRange);
                }
                let relative =
                    u32::try_from(start % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let available = ag_blocks
                    .checked_sub(u64::from(relative))
                    .ok_or(XfsError::CorruptMetadata)?;
                let length = u32::try_from((end - start).min(available))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if length == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                if let Some((_, frees)) = groups.iter_mut().find(|(candidate, _)| *candidate == ag)
                {
                    frees.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    frees.push((relative, length));
                } else {
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, vec![(relative, length)]));
                }
                start = start
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for (ag, frees) in groups {
            let staged = self.stage_recovery_extent_frees(ag, &frees)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        if metadata.buffers.is_empty() {
            return Ok(None);
        }
        let items = metadata.log_items()?;
        self.prepare_recovery_commit(step.lsn, &items).map(Some)
    }

    /// Resolves the only realtime BUF homes this recovery path can prove:
    /// v5 rtgroup bitmap/summary metadata on the data member.  Pre-rtgroup
    /// native-endian words have neither an LSN nor a CRC, and arbitrary
    /// realtime data needs owning-inode/refcount replay, so both remain out
    /// of this narrow admission set.
    fn realtime_replay_home(&self, item: &XfsBufferReplayItem) -> XfsResult<(u64, u32, u64)> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        // The homes below live in metadata inodes on the data member, but a
        // realtime transaction is not admitted without the claimed realtime
        // member whose geometry was validated at open time.
        let realtime = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if realtime.geometry().block_size == 0 || realtime.geometry().blocks == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let sectors = u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64;
        if sectors == 0
            || item.block_number % sectors != 0
            || u64::from(item.block_count) != sectors
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let physical = item.block_number / sectors;
        for group in 0..self.superblock.rtgroup_count {
            let (rtgroups, extents, _bits, bitmap_blocks) = self.realtime_layout(group)?;
            if !rtgroups {
                return Err(XfsError::UnsupportedFeature);
            }
            let (bitmap_owner, summary_owner) = *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::CorruptMetadata)?;
            for logical in 0..bitmap_blocks {
                if self.realtime_metadata_block(u64::MAX, group, logical)? == physical {
                    return Ok((physical, 0x424d_505a, bitmap_owner));
                }
            }
            let levels = 64u64
                .checked_sub(extents.leading_zeros() as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let slots = levels
                .checked_mul(bitmap_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            let words = (u64::from(self.superblock.block_size)
                .checked_sub(
                    u64::try_from(XFS_RTBUF_HEADER_BYTES)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::InvalidSuperblock)?)
                / 4;
            if words == 0 {
                return Err(XfsError::InvalidSuperblock);
            }
            for logical in 0..slots.div_ceil(words) {
                if self.realtime_metadata_block(u64::MAX - 1, group, logical)? == physical {
                    return Ok((physical, 0x5355_4d59, summary_owner));
                }
            }
        }
        Err(XfsError::UnsupportedFeature)
    }

    fn verify_realtime_replay_images(
        &self,
        item: &XfsBufferReplayItem,
        home: &[u8],
        image: &[u8],
    ) -> XfsResult<u64> {
        let (physical, magic, owner) = self.realtime_replay_home(item)?;
        if home.len() != self.superblock.block_size as usize || image.len() != home.len() {
            return Err(XfsError::CorruptMetadata);
        }
        self.verify_rtgroup_buffer(home, magic, owner, physical)?;
        self.verify_rtgroup_buffer(image, magic, owner, physical)?;
        item.home_lsn(home)?.ok_or(XfsError::UnsupportedFeature)
    }

    /// Materializes one committed transaction's buffer items against their
    /// current home blocks.  It is the only constructor for a writable
    /// recovery commit: the log LSN, basic-block address and dirty-chunk map
    /// stay bound together, rather than allowing a VFS operation to submit an
    /// arbitrary sector write under an XFS name.
    pub fn prepare_recovery_commit<'a>(
        &self,
        lsn: u64,
        items: impl IntoIterator<Item = &'a XfsBufferReplayItem>,
    ) -> XfsResult<XfsRecoveryCommit> {
        if !self.superblock.is_v5() || lsn == 0 || self.data.geometry().block_size % 512 != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        let mut writes = Vec::new();
        for item in items {
            let blocks = u64::from(item.block_count);
            let end = item
                .block_number
                .checked_add(blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if blocks == 0 || end > self.basic_blocks(&self.data)? {
                return Err(XfsError::AddressOutOfRange);
            }
            let bytes = usize::from(item.block_count)
                .checked_mul(512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut home = vec![0; bytes];
            self.read_basic_blocks(&self.data, item.block_number, &mut home)?;
            // Materialize first: a newly allocated tree block has arbitrary
            // pre-replay bytes and therefore no decodable home magic/LSN, but
            // its logged image is self-identifying and carries the new LSN.
            let image =
                item.materialize_home_image(&home, lsn, self.superblock.inode_size as usize)?;
            let is_btree = item.metadata_type()? == XfsMetadataBufferType::Btree;
            let is_realtime = item.metadata_type()? == XfsMetadataBufferType::Realtime;
            if is_realtime {
                let existing = self.verify_realtime_replay_images(item, &home, &image)?;
                if existing >= lsn {
                    continue;
                }
                writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                writes.push(XfsHomeWriteDescriptor {
                    basic_block: item.block_number,
                    bytes: image,
                    lsn,
                    item: item.clone(),
                });
                continue;
            }
            // Recovery is idempotent only for metadata with an on-disk LSN.
            match item.home_lsn(&home) {
                Ok(Some(existing)) if existing >= lsn => continue,
                Ok(Some(_)) => {}
                Ok(None) => return Err(XfsError::UnsupportedFeature),
                Err(XfsError::CorruptMetadata)
                    if is_btree && XfsBufferReplayItem::btree_crc_lsn_offsets(&image).is_ok() => {}
                Err(error) => return Err(error),
            }
            writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            writes.push(XfsHomeWriteDescriptor {
                basic_block: item.block_number,
                bytes: image,
                lsn,
                item: item.clone(),
            });
        }
        writes.sort_unstable_by_key(|write| write.basic_block);
        let mut prior = 0u64;
        for write in &writes {
            if write.basic_block < prior {
                return Err(XfsError::CorruptMetadata);
            }
            prior = write
                .basic_block
                .checked_add((write.bytes.len() / 512) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(XfsRecoveryCommit { lsn, writes })
    }

    /// Applies a fully prepared recovery write set in basic-block order.  The
    /// caller constructs descriptors only from committed log transactions;
    /// this routine never accepts an unframed mutation as recovery input.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn apply_recovery_commit(&self, commit: &XfsRecoveryCommit) -> XfsResult<()> {
        if !self.superblock.is_v5() || self.data.geometry().block_size % 512 != 0 || commit.lsn == 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        let mut writes = commit.writes.clone();
        writes.sort_unstable_by_key(|write| write.basic_block);
        let mut prior_end = 0u64;
        for write in &writes {
            if write.lsn != commit.lsn
                || write.item.block_number != write.basic_block
                || write.bytes.is_empty()
                || write.bytes.len() % 512 != 0
                || write.basic_block < prior_end
            {
                return Err(XfsError::CorruptMetadata);
            }
            prior_end = write
                .basic_block
                .checked_add((write.bytes.len() / 512) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if prior_end > self.basic_blocks(&self.data)? {
                return Err(XfsError::AddressOutOfRange);
            }
        }
        // Validate every admitted realtime home before the first FUA.  A
        // malformed later bitmap/summary record must leave this log commit
        // wholly retryable rather than checkpointing an earlier realtime
        // home and then failing mid-set.
        for write in &writes {
            if write.item.metadata_type()? != XfsMetadataBufferType::Realtime {
                continue;
            }
            let mut current = vec![0; write.bytes.len()];
            self.read_basic_blocks(&self.data, write.basic_block, &mut current)?;
            let _ = self.verify_realtime_replay_images(&write.item, &current, &write.bytes)?;
        }
        for write in &writes {
            let mut current = vec![0; write.bytes.len()];
            self.read_basic_blocks(&self.data, write.basic_block, &mut current)?;
            // A completed FUA write may be encountered after the subsequent
            // flush failed.  Its LSN proves the home block is already newer
            // than this replay record, so skipping it preserves idempotence.
            let is_btree = write.item.metadata_type()? == XfsMetadataBufferType::Btree;
            let is_realtime = write.item.metadata_type()? == XfsMetadataBufferType::Realtime;
            if is_realtime {
                let existing =
                    self.verify_realtime_replay_images(&write.item, &current, &write.bytes)?;
                if existing >= write.lsn {
                    continue;
                }
                self.write_basic_blocks_fua(&self.data, write.basic_block, &write.bytes)?;
                continue;
            }
            match write.item.home_lsn(&current) {
                Ok(Some(existing)) if existing >= write.lsn => continue,
                Ok(Some(_)) => {}
                Ok(None) => return Err(XfsError::UnsupportedFeature),
                // A log record may have committed before a freshly allocated
                // BMBT/AG-tree block received its first home write.  Its old
                // contents have no typed LSN; the prepared logged image is
                // the authority for this one initial installation.
                Err(XfsError::CorruptMetadata)
                    if is_btree
                        && XfsBufferReplayItem::btree_crc_lsn_offsets(&write.bytes).is_ok() => {}
                Err(error) => return Err(error),
            }
            self.write_basic_blocks_fua(&self.data, write.basic_block, &write.bytes)?;
        }
        self.data.flush().map_err(XfsError::from)
    }
    /// Claims and probes one data device.  This is useful for ordinary XFS
    /// images whose journal is internal.  It does not publish a VFS mount.
    pub fn probe(device: MountedBlockDevice) -> XfsResult<Arc<Self>> {
        let data = BlockVolume::new(vec![device.device().clone()]).map_err(XfsError::from)?;
        Self::open_inner(data, None, None, Some(device))
    }

    /// Opens XFS over explicit data/log/realtime volumes.  The caller retains
    /// the mount claims for each source device; this API has no global device
    /// lookup and therefore cannot accidentally attach the wrong log.
    pub fn open(
        data: BlockVolume,
        external_log: Option<BlockVolume>,
        realtime: Option<BlockVolume>,
    ) -> XfsResult<Arc<Self>> {
        Self::open_inner(data, external_log, realtime, None)
    }

    fn open_inner(
        data: BlockVolume,
        external_log: Option<BlockVolume>,
        realtime: Option<BlockVolume>,
        data_claim: Option<MountedBlockDevice>,
    ) -> XfsResult<Arc<Self>> {
        let physical = data.geometry().block_size;
        if physical < 264 {
            return Err(XfsError::InvalidSuperblock);
        }
        let mut first = vec![0; physical];
        data.read_blocks(0, &mut first).map_err(XfsError::from)?;
        let superblock = XfsSuperblock::parse(&first)?;
        // `NEEDSREPAIR` is a persistent administrator-visible assertion that
        // metadata is not safe to trust.  It is not a read-only feature bit.
        if superblock.features.needs_repair() {
            return Err(XfsError::CorruptMetadata);
        }
        if !superblock.has_dirv2() {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.block_size as usize % physical != 0
            || superblock.sector_size as usize % physical != 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.log_start == 0 && external_log.is_none() {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.log_start != 0 && external_log.is_some() {
            return Err(XfsError::InvalidSuperblock);
        }
        if superblock.realtime_blocks != 0 && realtime.is_none() {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(log) = &external_log
            && log.geometry().block_size != physical
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(rt) = &realtime
            && rt.geometry().block_size != physical
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(rt) = &realtime {
            // This API is for an explicitly claimed external realtime
            // member.  Internal realtime placement is not silently routed to
            // that member; reject it until a data-member mapping path exists.
            if superblock.realtime_start != 0 {
                return Err(XfsError::UnsupportedFeature);
            }
            let required = superblock
                .realtime_blocks
                .checked_mul(u64::from(superblock.block_size) / rt.geometry().block_size as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if required > rt.geometry().blocks {
                return Err(XfsError::InvalidSuperblock);
            }
            if superblock.realtime_blocks == 0
                || (superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0
                    && (superblock.realtime_bitmap_inode == 0
                        || superblock.realtime_summary_inode == 0))
            {
                return Err(XfsError::InvalidSuperblock);
            }
        }
        // Do not defer log extent validation until recovery: an internal log
        // must lie wholly inside the data device, while an external log is a
        // region beginning at its own block zero.  This is the physical
        // address-space contract used by the circular scanner.
        let log_basic_blocks = u64::from(superblock.log_blocks)
            .checked_mul(u64::from(superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if log_basic_blocks < 2 {
            return Err(XfsError::InvalidSuperblock);
        }
        let log_base = if superblock.log_start == 0 {
            0
        } else {
            superblock
                .log_start
                .checked_mul(u64::from(superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
                .ok_or(XfsError::AddressOutOfRange)?
        };
        let log_member = external_log.as_ref().unwrap_or(&data);
        let log_capacity = log_member
            .geometry()
            .blocks
            .checked_mul((log_member.geometry().block_size / XFS_LOG_BASIC_BLOCK) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if log_base
            .checked_add(log_basic_blocks)
            .ok_or(XfsError::AddressOutOfRange)?
            > log_capacity
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut volume = Arc::new(Self {
            data,
            external_log,
            realtime,
            rtgroup_inodes: Vec::new(),
            superblock,
            replay_lock: SpinMutex::new(()),
            _data_claim: data_claim,
        });
        if superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0
            && superblock.realtime_blocks != 0
        {
            let inner = Arc::get_mut(&mut volume).ok_or(XfsError::NoMemory)?;
            for group in 0..superblock.rtgroup_count {
                inner
                    .rtgroup_inodes
                    .push(inner.rtgroup_metadata_inodes(group)?);
            }
        }
        // Do this before publication, including read-only projections.  A
        // corrupt quota root must not become a mountable metadata view just
        // because no writer has touched it yet.
        if volume.has_quota_accounting() {
            volume.quota_state()?;
        }
        Ok(volume)
    }

    pub const fn superblock(&self) -> XfsSuperblock {
        self.superblock
    }

    pub fn quota_roots(&self) -> XfsQuotaRoots {
        XfsQuotaRoots {
            flags: self.superblock.quota_flags,
            user: (self.superblock.user_quota_inode != 0)
                .then_some(self.superblock.user_quota_inode),
            group: (self.superblock.group_quota_inode != 0)
                .then_some(self.superblock.group_quota_inode),
            project: (self.superblock.project_quota_inode != 0)
                .then_some(self.superblock.project_quota_inode),
        }
    }

    /// Validated native quota view.  A v5 mount never treats the presence of
    /// a quota inode as a policy toggle: the inode itself, its extent map,
    /// and every dquot subsequently addressed through it are media truth.
    pub fn quota_state(&self) -> XfsResult<XfsQuotaState> {
        // v4 stores legacy OQUOTA flags and aliases the group/project quota
        // inode.  This implementation's native dquot path is v5-only, so do
        // not partially emulate Linux's v4 disk-to-memory conversion here.
        if !self.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        let roots = self.quota_roots();
        for (flag, root) in [
            (XFS_UQUOTA_ACCT, roots.user),
            (XFS_GQUOTA_ACCT, roots.group),
            (XFS_PQUOTA_ACCT, roots.project),
        ] {
            if self.superblock.quota_flags & flag != 0 && root.is_none() {
                return Err(XfsError::CorruptMetadata);
            }
        }
        for (account, enforce) in [
            (XFS_UQUOTA_ACCT, XFS_UQUOTA_ENFD),
            (XFS_GQUOTA_ACCT, XFS_GQUOTA_ENFD),
            (XFS_PQUOTA_ACCT, XFS_PQUOTA_ENFD),
        ] {
            if self.superblock.quota_flags & enforce != 0
                && self.superblock.quota_flags & account == 0
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        for (kind, root) in [(1u8, roots.user), (2, roots.project), (4, roots.group)] {
            if !self.quota_accounting_enabled(kind) {
                continue;
            }
            if let Some(root) = root {
                let inode = self.inode(root)?;
                // dquots are 136-byte records, but their clusters are whole
                // filesystem blocks.  A 4KiB block deliberately contains 30
                // records and 16 bytes of unused tail, so the inode EOF is
                // block-aligned rather than necessarily divisible by 136.
                if inode.version < 3 || inode.mode & 0o170000 != 0o100000 {
                    return Err(XfsError::CorruptMetadata);
                }
                let extents = match inode.data_format {
                    XfsForkFormat::Extents => self.inode_data_extents(root)?,
                    XfsForkFormat::Btree => self.inode_bmbt_extents(root)?,
                    _ => return Err(XfsError::CorruptMetadata),
                };
                if extents.is_empty()
                    || extents
                        .iter()
                        .any(|extent| extent.unwritten || extent.block_count == 0)
                {
                    return Err(XfsError::CorruptMetadata);
                }
                // The exact id/type is verified by dquot() before use.
            }
        }
        Ok(XfsQuotaState { roots })
    }

    fn quota_inode_for(&self, quota_type: u8) -> XfsResult<u64> {
        if !self.quota_accounting_enabled(quota_type) {
            return Err(XfsError::UnsupportedFeature);
        }
        let root = match quota_type {
            1 => self.superblock.user_quota_inode,
            2 => self.superblock.project_quota_inode,
            4 => self.superblock.group_quota_inode,
            _ => return Err(XfsError::CorruptMetadata),
        };
        if root == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(root)
    }

    const fn quota_flags_for(quota_type: u8) -> Option<(u16, u16)> {
        match quota_type {
            1 => Some((XFS_UQUOTA_ACCT, XFS_UQUOTA_ENFD)),
            2 => Some((XFS_PQUOTA_ACCT, XFS_PQUOTA_ENFD)),
            4 => Some((XFS_GQUOTA_ACCT, XFS_GQUOTA_ENFD)),
            _ => None,
        }
    }

    pub(crate) fn quota_accounting_enabled(&self, quota_type: u8) -> bool {
        Self::quota_flags_for(quota_type)
            .is_some_and(|(account, _)| self.superblock.quota_flags & account != 0)
    }

    fn quota_enforcement_enabled(&self, quota_type: u8) -> bool {
        Self::quota_flags_for(quota_type)
            .is_some_and(|(_, enforce)| self.superblock.quota_flags & enforce != 0)
    }

    fn dquot_location(&self, quota_type: u8, id: u32) -> XfsResult<(u64, u32, u32)> {
        let inode = self.quota_inode_for(quota_type)?;
        let block_size = u64::from(self.superblock.block_size);
        let per_block = block_size / 136;
        if per_block == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        // XFS allocates dquot clusters one filesystem block at a time; the
        // unused tail of each block is not part of the record address space.
        let file_block = u64::from(id) / per_block;
        let within = (u64::from(id) % per_block)
            .checked_mul(136)
            .ok_or(XfsError::AddressOutOfRange)?;
        let record_end = file_block
            .checked_add(1)
            .and_then(|blocks| blocks.checked_mul(block_size))
            .ok_or(XfsError::AddressOutOfRange)?;
        let quota = self.inode(inode)?;
        if record_end > quota.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let extents = match quota.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        let logical_end = file_block
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .and_then(|start| start.checked_add(136))
            .ok_or(XfsError::AddressOutOfRange)?;
        if logical_end > quota.size || record_end > quota.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + u64::from(extent.block_count)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        if within.checked_add(136).ok_or(XfsError::AddressOutOfRange)? > block_size {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let byte = physical
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .ok_or(XfsError::AddressOutOfRange)?;
        let basic = byte / 512;
        let byte_offset = u32::try_from(byte % 512).map_err(|_| XfsError::AddressOutOfRange)?;
        let block_count = u32::try_from(
            (usize::try_from(byte_offset).map_err(|_| XfsError::AddressOutOfRange)? + 136)
                .div_ceil(512),
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        Ok((basic, byte_offset, block_count))
    }

    pub fn dquot(&self, quota_type: u8, id: u32) -> XfsResult<XfsDquot> {
        self.quota_state()?;
        let (basic, offset, blocks) = self.dquot_location(quota_type, id)?;
        let mut image = vec![
            0;
            usize::try_from(blocks)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(512)
                .ok_or(XfsError::AddressOutOfRange)?
        ];
        self.read_basic_blocks(&self.data, basic, &mut image)?;
        XfsDquot::parse(
            &image[offset as usize..offset as usize + 136],
            id,
            quota_type,
            self.superblock.meta_uuid,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0,
        )
    }

    /// Stages one native dquot, extending the owning quota inode when the
    /// addressed cluster has not been mapped yet.  The extension is not an
    /// in-memory quota cache: its newly allocated block is zeroed before the
    /// mapping can become durable, the bmap/AG images and the first 136-byte
    /// dquot image share this transaction, and the typed DQUOT item supplies
    /// the final LSN/CRC.  This is important for sparse or fragmented quota
    /// files produced by repair/quotaon -- treating an unmapped id as a
    /// permanently absent quota silently bypasses hard limits.
    pub fn stage_dquot_delta(
        &self,
        quota_type: u8,
        id: u32,
        block_delta: i64,
        inode_delta: i64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if transaction
            .dquots
            .iter()
            .any(|delta| delta.quota_type == quota_type && delta.id == id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        self.quota_state()?;
        let (basic_block, byte_offset, block_count) = match self.dquot_location(quota_type, id) {
            Ok(location) => location,
            // A dquot is addressed by id, not by a preallocated byte slot.
            // Only an absent cluster is growable; malformed mapped metadata
            // keeps its original error and cannot be overwritten by quota
            // initialization.
            Err(XfsError::AddressOutOfRange) => {
                self.stage_dquot_cluster_growth(quota_type, id, transaction)?
            }
            Err(error) => return Err(error),
        };
        let bytes = usize::try_from(block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        // A just-grown cluster has an ordered zero data write in this same
        // transaction.  Sample that staged image instead of stale free-space
        // contents; commit_metadata_transaction validates and writes it
        // before it materializes this typed dquot home item.
        let first_fs_block = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?
            / u64::from(self.superblock.block_size);
        let last_fs_block = basic_block
            .checked_add(u64::from(block_count))
            .and_then(|end| end.checked_mul(XFS_LOG_BASIC_BLOCK as u64))
            .ok_or(XfsError::AddressOutOfRange)?
            .saturating_sub(1)
            / u64::from(self.superblock.block_size);
        let staged_zero = transaction.data_writes.iter().any(|write| {
            write.fs_block >= first_fs_block
                && write.fs_block <= last_fs_block
                && write.after.iter().all(|byte| *byte == 0)
        });
        let mut home = vec![0; bytes];
        if !staged_zero {
            self.read_basic_blocks(&self.data, basic_block, &mut home)?;
        }
        let mut before = home[byte_offset as usize..byte_offset as usize + 136].to_vec();
        // Quota inode growth is preallocated in dquot-cluster units by mkfs
        // and quotaon.  A completely zero slot is the only safe "missing"
        // record we can materialize here: it is initialized in this same log
        // transaction, never represented by an in-memory placeholder.
        if before.iter().all(|byte| *byte == 0) {
            put_be16(&mut before, 0, 0x4451)?;
            before[2] = 1;
            before[3] = quota_type
                | if id != 0
                    && self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0
                {
                    XfsDquot::DQTYPE_BIGTIME
                } else {
                    0
                };
            put_be32(&mut before, 4, id)?;
            before[120..136].copy_from_slice(&self.superblock.meta_uuid.0);
            rewrite_crc32c(&mut before, 108)?;
        }
        let current = XfsDquot::parse(
            &before,
            id,
            quota_type,
            self.superblock.meta_uuid,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0,
        )?;
        let root = if id == 0 {
            current
        } else {
            self.dquot(quota_type, 0)?
        };
        let now = wall_time().as_secs();
        let admitted = current.apply_delta(
            block_delta,
            inode_delta,
            self.quota_enforcement_enabled(quota_type),
            now,
            if root.block_timer == 0 {
                XFS_DQ_DEFAULT_GRACE_SECONDS
            } else {
                root.block_timer
            },
            if root.inode_timer == 0 {
                XFS_DQ_DEFAULT_GRACE_SECONDS
            } else {
                root.inode_timer
            },
        )?;
        let mut after = before.clone();
        put_be64(&mut after, 40, admitted.blocks)?;
        put_be64(&mut after, 48, admitted.inodes)?;
        put_be32(&mut after, 56, admitted.inode_timer)?;
        put_be32(&mut after, 60, admitted.block_timer)?;
        put_be16(&mut after, 64, admitted.inode_warnings)?;
        put_be16(&mut after, 66, admitted.block_warnings)?;
        // LSN and CRC are finalized by the normal DQUOT log/home materializer
        // after the sole record reservation has selected its durable LSN.
        transaction.dquots.push(XfsDquotDelta {
            id,
            quota_type,
            basic_block,
            block_count,
            byte_offset,
            before,
            after,
        });
        Ok(())
    }

    /// Materializes exactly the filesystem block containing `id` when that
    /// block is outside the quota inode's current extent map.  It reuses the
    /// ordinary regular-file extent planner so inline/bmap forks, fragmented
    /// maps, AG free-space accounting and bmap-node growth retain the same
    /// crash protocol as a user write.  The caller still owns the enclosing
    /// transaction and appends its DQUOT item afterwards.
    fn stage_dquot_cluster_growth(
        &self,
        quota_type: u8,
        id: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<(u64, u32, u32)> {
        let inode_number = self.quota_inode_for(quota_type)?;
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::CorruptMetadata);
        }
        let block_size = u64::from(self.superblock.block_size);
        let per_block = block_size / 136;
        if per_block == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let file_block = u64::from(id) / per_block;
        let within = (u64::from(id) % per_block)
            .checked_mul(136)
            .ok_or(XfsError::AddressOutOfRange)?;
        let offset = file_block
            .checked_mul(block_size)
            .ok_or(XfsError::AddressOutOfRange)?;

        let old = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        if old.iter().any(|extent| {
            !extent.unwritten
                && file_block >= extent.file_block
                && file_block < extent.file_block + u64::from(extent.block_count)
        }) {
            // A mapped cluster with a short inode size is corrupt rather than
            // an invitation to erase its contents.  dquot_location normally
            // catches this; retain the fail-closed rule if a caller reaches
            // this helper through a malformed size/extent combination.
            return Err(XfsError::CorruptMetadata);
        }

        // prepare_regular_write reserves every AG/bmap resource before it
        // produces a buffer image.  Its data write is deliberately supplied
        // here as an all-zero full cluster, so no post-crash path can expose
        // stale free-space bytes as an initialized dquot record.
        let prepared =
            self.prepare_regular_write(inode_number, offset, self.superblock.block_size as usize)?;
        let mapped = prepared
            .mappings
            .iter()
            .find(|extent| extent.file_block == file_block && extent.block_count != 0)
            .copied()
            .ok_or(XfsError::CorruptMetadata)?;
        if mapped.block_count != 1 || mapped.unwritten {
            return Err(XfsError::CorruptMetadata);
        }
        let before = self.read_data_fs_block(mapped.start_block)?;
        let after = vec![0; self.superblock.block_size as usize];
        transaction.buffers.extend(prepared.metadata.buffers);
        transaction.data_writes.push(XfsStagedDataWrite {
            fs_block: mapped.start_block,
            before,
            after,
        });

        let byte = mapped
            .start_block
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .ok_or(XfsError::AddressOutOfRange)?;
        let basic_block = byte / XFS_LOG_BASIC_BLOCK as u64;
        let byte_offset = u32::try_from(byte % XFS_LOG_BASIC_BLOCK as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        let block_count = u32::try_from(
            (usize::try_from(byte_offset).map_err(|_| XfsError::AddressOutOfRange)? + 136)
                .div_ceil(XFS_LOG_BASIC_BLOCK),
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        Ok((basic_block, byte_offset, block_count))
    }

    /// Whether the superblock carries any native quota accounting root.
    /// Writer admission validates those roots and routes their dquot changes
    /// through the same live transaction as ordinary metadata.
    pub fn has_quota_accounting(&self) -> bool {
        self.superblock.quota_flags & (XFS_UQUOTA_ACCT | XFS_GQUOTA_ACCT | XFS_PQUOTA_ACCT) != 0
    }

    /// Reconstructs the statfs allocation counters from a single validated
    /// ownership snapshot per AG.  The caller is responsible for holding the
    /// mount's coherent-read guard when a live log coordinator exists.
    pub fn stat_counts(&self) -> XfsResult<XfsStatCounts> {
        let mut counts = XfsStatCounts::default();
        for ag in 0..self.superblock.ag_count {
            let snapshot = self.ag_ownership_snapshot(ag)?;
            let group = snapshot.group;
            let free_blocks = snapshot
                .free_extents
                .iter()
                .try_fold(0u64, |total, extent| {
                    total
                        .checked_add(u64::from(extent.block_count))
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
            if free_blocks != u64::from(group.free_space.free_blocks) {
                return Err(XfsError::CorruptMetadata);
            }
            let free_inodes = snapshot
                .inode_records
                .iter()
                .try_fold(0u64, |total, record| {
                    total
                        .checked_add(u64::from(record.free_count))
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
            if free_inodes != u64::from(group.inode.free_inode_count) {
                return Err(XfsError::CorruptMetadata);
            }
            let total_inodes = u64::try_from(snapshot.inode_records.len())
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if total_inodes != u64::from(group.inode.inode_count) {
                return Err(XfsError::CorruptMetadata);
            }
            counts.total_blocks = counts
                .total_blocks
                .checked_add(u64::from(group.free_space.length))
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.free_blocks = counts
                .free_blocks
                .checked_add(free_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.total_inodes = counts
                .total_inodes
                .checked_add(total_inodes)
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.free_inodes = counts
                .free_inodes
                .checked_add(free_inodes)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        if counts.total_blocks != self.superblock.data_blocks
            || counts.free_blocks > counts.total_blocks
            || counts.free_inodes > counts.total_inodes
        {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(counts)
    }

    /// Opens a retained inode object after validating its on-disk core.
    pub fn node(self: &Arc<Self>, number: u64) -> XfsResult<XfsNode> {
        Ok(XfsNode {
            volume: self.clone(),
            inode: self.inode(number)?,
        })
    }

    pub fn root_node(self: &Arc<Self>) -> XfsResult<XfsNode> {
        self.node(self.superblock.root_inode)
    }

    pub fn node_from_export_handle(
        self: &Arc<Self>,
        handle: XfsExportHandle,
    ) -> XfsResult<XfsNode> {
        let inode = self.resolve_export_handle(handle)?;
        Ok(XfsNode {
            volume: self.clone(),
            inode,
        })
    }

    pub fn directory_block_size(&self) -> XfsResult<usize> {
        let bytes = self
            .superblock
            .block_size
            .checked_shl(self.superblock.directory_block_log as u32)
            .ok_or(XfsError::InvalidSuperblock)?;
        usize::try_from(bytes).map_err(|_| XfsError::UnsupportedFeature)
    }

    /// Reads one logical dir2/dir3 data block through the inode mapping.  A
    /// short read is always corruption: directory format headers and leaf
    /// offsets are meaningful only over the complete logical block.
    pub fn directory_data_block(
        &self,
        inode_number: u64,
        index: u64,
    ) -> XfsResult<XfsDirectoryDataBlock> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let bytes = self.directory_block_size()?;
        let offset = index
            .checked_mul(bytes as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut block = vec![0; bytes];
        let read = self.read_inode_at(inode_number, offset, &mut block)?;
        if read != block.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = self
            .inode_physical_file_block(inode_number, offset / self.superblock.block_size as u64)?;
        XfsDirectoryDataBlock::parse(
            &block,
            self.superblock.meta_uuid,
            inode_number,
            physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0,
        )
    }

    /// Reads a dir2/dir3 leaf block from its distinct 32GiB logical address
    /// space.  Data and leaf offsets are never conflated even when their
    /// backing extents happen to be adjacent on disk.
    pub fn directory_leaf_block(
        &self,
        inode_number: u64,
        index: u64,
    ) -> XfsResult<XfsDirectoryLeafBlock> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let bytes = self.directory_block_size()?;
        let offset = XFS_DIR_LEAF_SPACE_BYTES
            .checked_add(
                index
                    .checked_mul(bytes as u64)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut block = vec![0; bytes];
        let read = self.read_inode_at(inode_number, offset, &mut block)?;
        if read != block.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = self
            .inode_physical_file_block(inode_number, offset / self.superblock.block_size as u64)?;
        XfsDirectoryLeafBlock::parse(
            &block,
            self.superblock.meta_uuid,
            inode_number,
            physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
        )
    }

    /// Returns every live name in a directory without converting names to
    /// UTF-8.  Local directories are decoded directly; block, leaf, and node
    /// directories are traversed through their data-space mappings.  Leaf
    /// blocks are indexes only and must never be treated as directory data.
    ///
    /// The data fork can contain holes between populated dir2 data blocks.
    /// Those holes are skipped from the checked extent map rather than being
    /// fed to the directory decoder as an all-zero "block".
    pub fn directory_entries(&self, inode_number: u64) -> XfsResult<Vec<XfsDirectoryEntry>> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        if inode.data_format == XfsForkFormat::Local {
            return self.shortform_directory(inode_number);
        }
        let dir_block = self.directory_block_size()? as u64;
        let data_limit = cmp::min(inode.size, XFS_DIR_LEAF_SPACE_BYTES);
        let data_blocks = data_limit.div_ceil(dir_block);
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let fs_block = self.superblock.block_size as u64;
        let mut entries = Vec::new();
        for index in 0..data_blocks {
            let logical_start = index
                .checked_mul(dir_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let first_file_block = logical_start / fs_block;
            let last_file_block = logical_start
                .checked_add(dir_block)
                .and_then(|end| end.checked_sub(1))
                .ok_or(XfsError::AddressOutOfRange)?
                / fs_block;
            // A dir data block is meaningful only if a single written extent
            // covers it in full.  A partial mapping is corrupt metadata, not
            // a sparse directory hole.
            let Some(mapping) = extents.iter().find(|extent| {
                !extent.unwritten
                    && first_file_block >= extent.file_block
                    && last_file_block < extent.file_block.saturating_add(extent.block_count as u64)
            }) else {
                continue;
            };
            if mapping.file_block > first_file_block {
                return Err(XfsError::CorruptMetadata);
            }
            let block = self.directory_data_block(inode_number, index)?;
            entries
                .try_reserve(block.entries.len())
                .map_err(|_| XfsError::NoMemory)?;
            entries.extend(
                block
                    .entries
                    .into_iter()
                    .filter(|entry| entry.name != b"." && entry.name != b"..")
                    .map(|entry| XfsDirectoryEntry {
                        name: entry.name,
                        inode: entry.inode,
                        file_type: entry.file_type,
                    }),
            );
        }
        Ok(entries)
    }

    /// Reads the directory's one native `..` relationship.  It is kept
    /// separate from `directory_entries`, whose VFS-facing result excludes
    /// dot names.  A rewrite of an external directory must preserve this
    /// value; substituting the directory's own inode would silently detach a
    /// moved directory from its parent.
    pub fn directory_parent(&self, inode_number: u64) -> XfsResult<u64> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        if inode.data_format == XfsForkFormat::Local {
            let (_, raw) = self.inode_and_bytes(inode_number)?;
            let fork = inode.data_fork(&raw)?;
            if fork.len() < 6 {
                return Err(XfsError::CorruptMetadata);
            }
            let parent = if fork[1] == 0 {
                be32(fork, 2)? as u64
            } else {
                be64(fork, 2)?
            };
            return if parent == 0 {
                Err(XfsError::CorruptMetadata)
            } else {
                Ok(parent)
            };
        }
        let dir_block = self.directory_block_size()? as u64;
        let data_limit = cmp::min(inode.size, XFS_DIR_LEAF_SPACE_BYTES);
        let data_blocks = data_limit.div_ceil(dir_block);
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let fs_block = self.superblock.block_size as u64;
        let mut parent = None;
        for index in 0..data_blocks {
            let logical_start = index
                .checked_mul(dir_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let first = logical_start / fs_block;
            let last = logical_start
                .checked_add(dir_block)
                .and_then(|end| end.checked_sub(1))
                .ok_or(XfsError::AddressOutOfRange)?
                / fs_block;
            let Some(mapping) = extents.iter().find(|extent| {
                !extent.unwritten
                    && first >= extent.file_block
                    && last < extent.file_block.saturating_add(extent.block_count as u64)
            }) else {
                continue;
            };
            if mapping.file_block > first {
                return Err(XfsError::CorruptMetadata);
            }
            for entry in self.directory_data_block(inode_number, index)?.entries {
                if entry.name == b".." {
                    if entry.inode == 0 || parent.replace(entry.inode).is_some() {
                        return Err(XfsError::CorruptMetadata);
                    }
                }
            }
        }
        parent.ok_or(XfsError::CorruptMetadata)
    }

    /// Resolves a non-dot name through the native directory representation.
    /// A directory block's ftype is merely a cache hint: the inode core is
    /// always loaded by the caller before exposing an object to VFS.
    pub fn lookup_directory(&self, directory: u64, name: &[u8]) -> XfsResult<XfsInode> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let entry = self
            .directory_entries(directory)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.inode(entry.inode)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn data_volume(&self) -> &BlockVolume {
        &self.data
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn external_log(&self) -> Option<&BlockVolume> {
        self.external_log.as_ref()
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn realtime_volume(&self) -> Option<&BlockVolume> {
        self.realtime.as_ref()
    }

    fn log_volume(&self) -> XfsResult<&BlockVolume> {
        if self.superblock.log_start == 0 {
            self.external_log
                .as_ref()
                .ok_or(XfsError::UnsupportedFeature)
        } else {
            Ok(&self.data)
        }
    }

    fn log_region_start_block(&self) -> XfsResult<u64> {
        if self.superblock.log_start == 0 {
            return Ok(0);
        }
        let per_fs_block = self.superblock.block_size as u64 / XFS_LOG_BASIC_BLOCK as u64;
        self.superblock
            .log_start
            .checked_mul(per_fs_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    fn log_region_blocks(&self) -> XfsResult<u32> {
        let bytes = u64::from(self.superblock.log_blocks)
            .checked_mul(self.superblock.block_size as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if bytes % XFS_LOG_BASIC_BLOCK as u64 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        u32::try_from(bytes / XFS_LOG_BASIC_BLOCK as u64).map_err(|_| XfsError::AddressOutOfRange)
    }

    /// Flushes every explicitly attached XFS member.  An external journal and
    /// realtime device are part of the same mount durability domain; flushing
    /// only the data device would acknowledge a sync while a committed log
    /// record or realtime extent was still volatile.
    pub fn flush(&self) -> XfsResult<()> {
        self.data.flush().map_err(XfsError::from)?;
        if let Some(log) = &self.external_log {
            log.flush().map_err(XfsError::from)?;
        }
        if let Some(realtime) = &self.realtime {
            realtime.flush().map_err(XfsError::from)?;
        }
        Ok(())
    }

    /// Reads and validates the free-space and inode headers of one allocation
    /// group.  The AG sequence and length bind the headers to their expected
    /// group, catching a misplaced but otherwise well-formed block.
    pub fn allocation_group(&self, number: u32) -> XfsResult<XfsAllocationGroup> {
        if number >= self.superblock.ag_count {
            return Err(XfsError::AddressOutOfRange);
        }
        let block = (number as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let base_byte = block
            .checked_mul(self.superblock.block_size as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agf_bytes = self.read_data_bytes(base_byte)?;
        let agi_byte = base_byte
            .checked_add(self.superblock.sector_size as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agi_bytes = self.read_data_bytes(agi_byte)?;
        let sector = self.superblock.sector_size as usize;
        let agf = XfsAgf::parse(
            slice(&agf_bytes, 0, sector)?,
            self.superblock.features,
            self.superblock.is_v5(),
        )?;
        let agi = XfsAgi::parse(slice(&agi_bytes, 0, sector)?, self.superblock.is_v5())?;
        let agfl = if self.superblock.is_v5() {
            let agfl_byte = agi_byte
                .checked_add(self.superblock.sector_size as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let agfl_bytes = self.read_data_bytes(agfl_byte)?;
            Some(XfsAgfl::parse(slice(&agfl_bytes, 0, sector)?, true)?)
        } else {
            None
        };
        if agf.sequence != number
            || agi.sequence != number
            || agf.length == 0
            || agf.length > self.superblock.ag_blocks
            || agi.length != agf.length
            || agf.uuid != self.superblock.uuid
            || agi.uuid != self.superblock.uuid
            || agfl.is_some_and(|header| {
                header.sequence != number || header.uuid != self.superblock.uuid
            })
        {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsAllocationGroup {
            number,
            free_space: agf,
            inode: agi,
        })
    }

    /// Loads the circular AGFL array and binds its active range to AGF's
    /// first/last/count fields. Entries are reserved btree-maintenance blocks,
    /// so duplicate, header, or out-of-AG values are metadata corruption.
    pub fn ag_freelist(&self, ag: u32) -> XfsResult<XfsAgFreelist> {
        let group = self.allocation_group(ag)?;
        self.ag_freelist_from_group(group)
    }

    /// Decodes AGFL against an already verified AGF/AGI header image.  The
    /// combined inode+extent planner uses this to prevent a second header
    /// read from mixing AGF counters with a later AGFL ring.
    fn ag_freelist_from_group(&self, group: XfsAllocationGroup) -> XfsResult<XfsAgFreelist> {
        let ag = group.number;
        let sector = self.superblock.sector_size as usize;
        let base = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|block| block.checked_mul(self.superblock.block_size as u64))
            .and_then(|byte| byte.checked_add(3 * self.superblock.sector_size as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let bytes = self.read_data_bytes(base)?;
        if bytes.len() != sector {
            return Err(XfsError::CorruptMetadata);
        }
        let header = XfsAgfl::parse(&bytes, self.superblock.is_v5())?;
        if header.sequence != ag || header.uuid != self.superblock.uuid {
            return Err(XfsError::CorruptMetadata);
        }
        let capacity = (bytes
            .len()
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?)
            / 4;
        let count = usize::try_from(group.free_space.freelist_count)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        if count > capacity
            || (count == 0
                && (group.free_space.freelist_first != 0 || group.free_space.freelist_last != 0))
            || (count != 0
                && (group.free_space.freelist_first as usize >= capacity
                    || group.free_space.freelist_last as usize >= capacity))
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..count {
            let slot = (group.free_space.freelist_first as usize + index) % capacity;
            let block = be32(&bytes, 36 + slot * 4)?;
            if block < 4
                || block >= group.free_space.length
                || entries.iter().any(|existing| *existing == block)
            {
                return Err(XfsError::CorruptMetadata);
            }
            entries.push(block);
        }
        if count != 0
            && ((group.free_space.freelist_first as usize + count - 1) % capacity) as u32
                != group.free_space.freelist_last
        {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsAgFreelist {
            ag,
            entries,
            first: group.free_space.freelist_first,
            last: group.free_space.freelist_last,
        })
    }

    /// Reads one AG-relative allocation/inode btree node after binding its
    /// CRC owner to the allocation group and its physical address to this
    /// volume.  Traversal callers must level-check child nodes before using
    /// any free extent or inode bitmap for allocation.
    pub fn ag_btree_node(
        &self,
        ag: u32,
        block: u32,
        kind: XfsAgBtreeKind,
    ) -> XfsResult<XfsAgBtreeNode> {
        if ag >= self.superblock.ag_count || block >= self.superblock.ag_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        XfsAgBtreeNode::parse(
            kind,
            ag,
            block,
            &self.read_data_fs_block(fs_block)?,
            self.superblock,
        )
    }

    fn walk_ag_btree(
        &self,
        ag: u32,
        root: u32,
        kind: XfsAgBtreeKind,
    ) -> XfsResult<Vec<XfsAgBtreeNode>> {
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        pending.push((root, None));
        let mut nodes = Vec::new();
        while let Some((block, expected_level)) = pending.pop() {
            if nodes
                .iter()
                .any(|node: &XfsAgBtreeNode| node.block == block)
            {
                return Err(XfsError::CorruptMetadata);
            }
            let node = self.ag_btree_node(ag, block, kind)?;
            if expected_level.is_some_and(|level| node.level != level) {
                return Err(XfsError::CorruptMetadata);
            }
            if node.level != 0 {
                let next = node.level.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
                pending
                    .try_reserve(node.children.len())
                    .map_err(|_| XfsError::NoMemory)?;
                for child in &node.children {
                    pending.push((*child, Some(next)));
                }
            }
            nodes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            nodes.push(node);
        }
        for node in &nodes {
            for (sibling, reverse_left) in [(node.left_sibling, false), (node.right_sibling, true)]
            {
                if sibling == 0 {
                    continue;
                }
                let peer = nodes
                    .iter()
                    .find(|candidate| candidate.block == sibling)
                    .ok_or(XfsError::CorruptMetadata)?;
                let reverse = if reverse_left {
                    peer.left_sibling
                } else {
                    peer.right_sibling
                };
                if peer.level != node.level || reverse != node.block {
                    return Err(XfsError::CorruptMetadata);
                }
            }
        }
        Ok(nodes)
    }

    /// Reads a checksummed rmapbt or refcountbt block.  These trees are v5
    /// only and use their own record/key widths; callers must not route them
    /// through the free-space btree reader.
    pub fn ag_special_btree_node(
        &self,
        ag: u32,
        block: u32,
        kind: XfsAgSpecialBtreeKind,
    ) -> XfsResult<XfsAgSpecialBtreeNode> {
        if ag >= self.superblock.ag_count || block < 4 || block >= self.superblock.ag_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        XfsAgSpecialBtreeNode::parse(
            kind,
            ag,
            block,
            &self.read_data_fs_block(fs_block)?,
            self.superblock,
        )
    }

    fn walk_ag_special_btree(
        &self,
        ag: u32,
        root: u32,
        kind: XfsAgSpecialBtreeKind,
    ) -> XfsResult<Vec<XfsAgSpecialBtreeNode>> {
        if root < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut pending = vec![(root, None)];
        let mut nodes = Vec::new();
        while let Some((block, expected_level)) = pending.pop() {
            if nodes
                .iter()
                .any(|node: &XfsAgSpecialBtreeNode| node.block == block)
            {
                return Err(XfsError::CorruptMetadata);
            }
            let node = self.ag_special_btree_node(ag, block, kind)?;
            if expected_level.is_some_and(|level| node.level != level) {
                return Err(XfsError::CorruptMetadata);
            }
            if node.level != 0 {
                let child_level = node.level.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
                pending
                    .try_reserve(node.children.len())
                    .map_err(|_| XfsError::NoMemory)?;
                for child in &node.children {
                    pending.push((*child, Some(child_level)));
                }
            }
            nodes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            nodes.push(node);
        }
        for node in &nodes {
            for (sibling, reverse_left) in [(node.left_sibling, false), (node.right_sibling, true)]
            {
                if sibling == 0 {
                    continue;
                }
                let peer = nodes
                    .iter()
                    .find(|candidate| candidate.block == sibling)
                    .ok_or(XfsError::CorruptMetadata)?;
                let reverse = if reverse_left {
                    peer.left_sibling
                } else {
                    peer.right_sibling
                };
                if peer.level != node.level || reverse != node.block {
                    return Err(XfsError::CorruptMetadata);
                }
            }
        }
        Ok(nodes)
    }

    pub fn rmap_btree(&self, ag: u32) -> XfsResult<Vec<XfsAgSpecialBtreeNode>> {
        let group = self.allocation_group(ag)?;
        if !self.superblock.features.has_rmapbt() {
            return Err(XfsError::UnsupportedFeature);
        }
        self.walk_ag_special_btree(
            ag,
            group
                .free_space
                .rmap_root
                .ok_or(XfsError::CorruptMetadata)?,
            XfsAgSpecialBtreeKind::Rmap,
        )
    }

    pub fn refcount_btree(&self, ag: u32) -> XfsResult<Vec<XfsAgSpecialBtreeNode>> {
        let group = self.allocation_group(ag)?;
        if !self.superblock.features.has_reflink() {
            return Err(XfsError::UnsupportedFeature);
        }
        self.walk_ag_special_btree(
            ag,
            group
                .free_space
                .refcount_root
                .ok_or(XfsError::CorruptMetadata)?,
            XfsAgSpecialBtreeKind::Refcount,
        )
    }

    /// Returns the canonical, CRC-verified rmap leaf set for an AG.  Recovery
    /// computes an intent's replacement set from this snapshot and then hands
    /// it to [`Self::stage_rmap_records`]; it never mutates a leaf in place.
    pub fn rmap_records(&self, ag: u32) -> XfsResult<Vec<XfsRmapRecord>> {
        let nodes = self.rmap_btree(ag)?;
        let mut records = Vec::new();
        for node in &nodes {
            if let XfsAgSpecialBtreeRecords::Rmap(leaf) = &node.records {
                records
                    .try_reserve(leaf.len())
                    .map_err(|_| XfsError::NoMemory)?;
                records.extend_from_slice(leaf);
            }
        }
        records.sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
        if records.windows(2).any(|pair| {
            (pair[0].start_block, pair[0].owner, pair[0].offset)
                == (pair[1].start_block, pair[1].owner, pair[1].offset)
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(records)
    }

    /// Returns the canonical, CRC-verified refcount leaf set for an AG.
    pub fn refcount_records(&self, ag: u32) -> XfsResult<Vec<XfsRefcountRecord>> {
        let nodes = self.refcount_btree(ag)?;
        let mut records = Vec::new();
        for node in &nodes {
            if let XfsAgSpecialBtreeRecords::Refcount(leaf) = &node.records {
                records
                    .try_reserve(leaf.len())
                    .map_err(|_| XfsError::NoMemory)?;
                records.extend_from_slice(leaf);
            }
        }
        records.sort_unstable_by_key(|record| record.start_block);
        if records.windows(2).any(|pair| {
            pair[0]
                .start_block
                .checked_add(pair[0].block_count)
                .is_none_or(|end| end > pair[1].start_block)
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(records)
    }

    /// Rebuilds one rmapbt/refcountbt from verified leaf records and stages
    /// every replacement node, AGF root, and AGFL ownership change together.
    /// The only growth source is the current AGFL plus blocks made obsolete
    /// by this same tree; ordinary free-space records are never borrowed
    /// without their own allocator transaction.
    fn stage_special_btree(
        &self,
        ag: u32,
        kind: XfsAgSpecialBtreeKind,
        records: XfsAgSpecialBtreeRecords,
    ) -> XfsResult<XfsMetadataTransaction> {
        let group = self.allocation_group(ag)?;
        let freelist = self.ag_freelist_from_group(group)?;
        let (old, other) = match kind {
            XfsAgSpecialBtreeKind::Rmap => (
                self.rmap_btree(ag)?,
                if self.superblock.features.has_reflink() {
                    self.refcount_btree(ag)?
                } else {
                    Vec::new()
                },
            ),
            XfsAgSpecialBtreeKind::Refcount => (
                self.refcount_btree(ag)?,
                if self.superblock.features.has_rmapbt() {
                    self.rmap_btree(ag)?
                } else {
                    Vec::new()
                },
            ),
        };
        if old
            .iter()
            .any(|node| other.iter().any(|peer| peer.block == node.block))
        {
            return Err(XfsError::CorruptMetadata);
        }
        let allocation = self.ag_ownership_snapshot(ag)?;
        if old.iter().chain(other.iter()).any(|node| {
            allocation.free_extents.iter().any(|extent| {
                node.block >= extent.start_block
                    && node.block < extent.start_block + extent.block_count
            })
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        let mut pool = Vec::new();
        for node in &old {
            if !pool.contains(&node.block) {
                pool.push(node.block);
            }
        }
        for block in &freelist.entries {
            if !pool.contains(block) {
                pool.push(*block);
            }
        }
        if pool.iter().any(|block| {
            other.iter().any(|node| node.block == *block)
                || allocation
                    .bno_nodes
                    .iter()
                    .chain(allocation.cnt_nodes.iter())
                    .chain(allocation.ino_nodes.iter())
                    .chain(allocation.fino_nodes.iter())
                    .any(|node| node.block == *block)
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        let (nodes, used) = build_special_tree(kind, ag, self.superblock, records, &pool)?;
        let new_freelist = pool.get(used..).ok_or(XfsError::CorruptMetadata)?.to_vec();
        let capacity = (self.superblock.sector_size as usize)
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if new_freelist.len() > capacity {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut buffers = Vec::new();
        for node in &nodes {
            let fs_block = (ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(node.block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(fs_block)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: fs_block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after,
                });
            }
        }
        let first = 0u32;
        let last = if new_freelist.is_empty() {
            0
        } else {
            u32::try_from(new_freelist.len() - 1).map_err(|_| XfsError::AddressOutOfRange)?
        };
        let mut agf = group.free_space;
        match kind {
            XfsAgSpecialBtreeKind::Rmap => {
                agf.rmap_root = Some(nodes.last().ok_or(XfsError::CorruptMetadata)?.block)
            }
            XfsAgSpecialBtreeKind::Refcount => {
                agf.refcount_root = Some(nodes.last().ok_or(XfsError::CorruptMetadata)?.block)
            }
        }
        agf.freelist_first = first;
        agf.freelist_last = last;
        agf.freelist_count =
            u32::try_from(new_freelist.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let ag_base = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let before_agf = self.read_data_fs_block(ag_base)?;
        let sector = self.superblock.sector_size as usize;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agf,
            basic_block: ag_base
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before: before_agf[..sector].to_vec(),
            after: agf.serialize(self.superblock, 0)?,
        });
        let agfl_byte = 3usize
            .checked_mul(sector)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agfl_block = ag_base
            .checked_add((agfl_byte / self.superblock.block_size as usize) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agfl_offset = agfl_byte % self.superblock.block_size as usize;
        if agfl_offset + sector > self.superblock.block_size as usize {
            return Err(XfsError::UnsupportedFeature);
        }
        let before_agfl_block = self.read_data_fs_block(agfl_block)?;
        let before_agfl = slice(&before_agfl_block, agfl_offset, sector)?.to_vec();
        let header = XfsAgfl {
            sequence: ag,
            uuid: self.superblock.uuid,
        };
        let basic = agfl_block
            .checked_mul((self.superblock.block_size as u64) / 512)
            .and_then(|base| base.checked_add((agfl_offset / 512) as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agfl,
            basic_block: basic,
            before: before_agfl,
            after: header.serialize(self.superblock, 0, &new_freelist, first, last)?,
        });
        Ok(XfsMetadataTransaction {
            buffers,
            data_writes: Vec::new(),
            realtime_writes: Vec::new(),
            dquots: Vec::new(),
        })
    }

    /// Staged RUI target: the caller supplies the fully resolved, canonical
    /// rmap set for one AG.  The resulting transaction is atomic with AGF and
    /// AGFL replacement and can be committed by the normal FUA log/home path.
    pub fn stage_rmap_records(
        &self,
        ag: u32,
        records: Vec<XfsRmapRecord>,
    ) -> XfsResult<XfsMetadataTransaction> {
        self.stage_special_btree(
            ag,
            XfsAgSpecialBtreeKind::Rmap,
            XfsAgSpecialBtreeRecords::Rmap(records),
        )
    }

    /// Staged CUI target corresponding to [`stage_rmap_records`].
    pub fn stage_refcount_records(
        &self,
        ag: u32,
        records: Vec<XfsRefcountRecord>,
    ) -> XfsResult<XfsMetadataTransaction> {
        self.stage_special_btree(
            ag,
            XfsAgSpecialBtreeKind::Refcount,
            XfsAgSpecialBtreeRecords::Refcount(records),
        )
    }

    /// Materialises a complete reflink AG plan from one replacement pool.
    /// The final AGF/AGFL image names all four new roots, so recovery cannot
    /// observe a new rmap root with an old refcount/free-space root.
    pub fn stage_reflink_ag_plan(
        &self,
        planner: &XfsAgMutationPlanner,
    ) -> XfsResult<XfsMetadataTransaction> {
        if !self.superblock.features.has_rmapbt() || !self.superblock.features.has_reflink() {
            return Err(XfsError::UnsupportedFeature);
        }
        // All four trees draw from one disjoint replacement pool.  Do not
        // stage three independent AGFL snapshots and attempt to byte-merge
        // them: a successful merge could still allocate the same old AGFL
        // block to two different trees.
        let group = self.allocation_group(planner.ag)?;
        let snapshot = self.ag_ownership_snapshot(planner.ag)?;
        let old_rmap = self.rmap_btree(planner.ag)?;
        let old_ref = self.refcount_btree(planner.ag)?;
        let mut pool = Vec::new();
        for block in old_rmap
            .iter()
            .map(|node| node.block)
            .chain(old_ref.iter().map(|node| node.block))
            .chain(snapshot.bno_nodes.iter().map(|node| node.block))
            .chain(snapshot.cnt_nodes.iter().map(|node| node.block))
            .chain(snapshot.freelist.entries.iter().copied())
        {
            if !pool.contains(&block) {
                pool.push(block);
            }
        }
        let (rmap, rused) = build_special_tree(
            XfsAgSpecialBtreeKind::Rmap,
            planner.ag,
            self.superblock,
            XfsAgSpecialBtreeRecords::Rmap(planner.rmap.clone()),
            &pool,
        )?;
        let (refs, fused) = build_special_tree(
            XfsAgSpecialBtreeKind::Refcount,
            planner.ag,
            self.superblock,
            XfsAgSpecialBtreeRecords::Refcount(planner.refcount.clone()),
            &pool[rused..],
        )?;
        let (bno, bused) = build_free_tree(
            XfsAgBtreeKind::ByBlock,
            planner.ag,
            self.superblock,
            &planner.free,
            &pool[rused + fused..],
        )?;
        let (cnt, cused) = build_free_tree(
            XfsAgBtreeKind::ByLength,
            planner.ag,
            self.superblock,
            &planner.free,
            &pool[rused + fused + bused..],
        )?;
        let used = rused
            .checked_add(fused)
            .and_then(|n| n.checked_add(bused))
            .and_then(|n| n.checked_add(cused))
            .ok_or(XfsError::AddressOutOfRange)?;
        let freelist = pool.get(used..).ok_or(XfsError::CorruptMetadata)?;
        let capacity = (self.superblock.sector_size as usize)
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if freelist.len() > capacity {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut buffers = Vec::new();
        for node in rmap.iter().chain(refs.iter()) {
            let physical = u64::from(planner.ag)
                .checked_mul(u64::from(self.superblock.ag_blocks))
                .and_then(|base| base.checked_add(u64::from(node.block)))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(physical)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: physical * (u64::from(self.superblock.block_size) / 512),
                    before,
                    after,
                });
            }
        }
        for node in bno.iter().chain(cnt.iter()) {
            let physical = u64::from(planner.ag)
                .checked_mul(u64::from(self.superblock.ag_blocks))
                .and_then(|base| base.checked_add(u64::from(node.block)))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(physical)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: physical * (u64::from(self.superblock.block_size) / 512),
                    before,
                    after,
                });
            }
        }
        let first = 0;
        let last = if freelist.is_empty() {
            0
        } else {
            u32::try_from(freelist.len() - 1).map_err(|_| XfsError::AddressOutOfRange)?
        };
        let mut agf = group.free_space;
        agf.rmap_root = Some(rmap.last().ok_or(XfsError::CorruptMetadata)?.block);
        agf.refcount_root = Some(refs.last().ok_or(XfsError::CorruptMetadata)?.block);
        agf.bno_root = bno.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.cnt_root = cnt.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.free_blocks = u32::try_from(planner.free.iter().try_fold(0u64, |n, e| {
            n.checked_add(u64::from(e.block_count))
                .ok_or(XfsError::AddressOutOfRange)
        })?)
        .map_err(|_| XfsError::AddressOutOfRange)?;
        agf.longest_free_extent = planner
            .free
            .iter()
            .map(|e| e.block_count)
            .max()
            .unwrap_or(0);
        agf.freelist_first = first;
        agf.freelist_last = last;
        agf.freelist_count =
            u32::try_from(freelist.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let ag_base = u64::from(planner.ag) * u64::from(self.superblock.ag_blocks);
        let sector = self.superblock.sector_size as usize;
        let before = self.read_data_fs_block(ag_base)?;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agf,
            basic_block: ag_base * (u64::from(self.superblock.block_size) / 512),
            before: before[..sector].to_vec(),
            after: agf.serialize(self.superblock, 0)?,
        });
        let off = 3 * sector;
        let agfl_block = ag_base + (off / self.superblock.block_size as usize) as u64;
        let within = off % self.superblock.block_size as usize;
        if within + sector > self.superblock.block_size as usize {
            return Err(XfsError::UnsupportedFeature);
        }
        let before = self.read_data_fs_block(agfl_block)?;
        let header = XfsAgfl {
            sequence: planner.ag,
            uuid: self.superblock.uuid,
        };
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agfl,
            basic_block: agfl_block * (u64::from(self.superblock.block_size) / 512)
                + (within / 512) as u64,
            before: before[within..within + sector].to_vec(),
            after: header.serialize(self.superblock, 0, freelist, first, last)?,
        });
        Ok(XfsMetadataTransaction {
            buffers,
            data_writes: Vec::new(),
            realtime_writes: Vec::new(),
            dquots: Vec::new(),
        })
    }

    pub fn ag_ownership_snapshot(&self, ag: u32) -> XfsResult<XfsAgOwnershipSnapshot> {
        let headers = self.allocation_group(ag)?;
        let freelist = self.ag_freelist_from_group(headers)?;
        let bno_nodes =
            self.walk_ag_btree(ag, headers.free_space.bno_root, XfsAgBtreeKind::ByBlock)?;
        let cnt_nodes =
            self.walk_ag_btree(ag, headers.free_space.cnt_root, XfsAgBtreeKind::ByLength)?;
        let ino_nodes =
            self.walk_ag_btree(ag, headers.inode.inode_btree_root, XfsAgBtreeKind::Inode)?;
        let fino_nodes = match headers.inode.free_inode_btree_root {
            Some(root) if root != 0 => self.walk_ag_btree(ag, root, XfsAgBtreeKind::FreeInode)?,
            _ => Vec::new(),
        };
        // Allocation trees and AGFL are mutually exclusive metadata homes.
        // Establish this before accepting either index's free records: a
        // corrupted image must never let a data allocation alias a btree
        // block merely because a later planner happens to deduplicate it.
        let mut reserved = Vec::new();
        for node in bno_nodes
            .iter()
            .chain(cnt_nodes.iter())
            .chain(ino_nodes.iter())
            .chain(fino_nodes.iter())
        {
            if node.block < 4
                || node.block >= headers.free_space.length
                || reserved.contains(&node.block)
            {
                return Err(XfsError::CorruptMetadata);
            }
            reserved.push(node.block);
        }
        for block in &freelist.entries {
            if *block < 4 || *block >= headers.free_space.length || reserved.contains(block) {
                return Err(XfsError::CorruptMetadata);
            }
            reserved.push(*block);
        }
        let mut free_extents = Vec::new();
        let mut count_extents = Vec::new();
        let mut inode_records = Vec::new();
        for node in &bno_nodes {
            if let XfsAgBtreeRecords::Free(records) = &node.records {
                free_extents
                    .try_reserve(records.len())
                    .map_err(|_| XfsError::NoMemory)?;
                free_extents.extend_from_slice(records);
            }
        }
        for node in &cnt_nodes {
            if let XfsAgBtreeRecords::Free(records) = &node.records {
                count_extents
                    .try_reserve(records.len())
                    .map_err(|_| XfsError::NoMemory)?;
                count_extents.extend_from_slice(records);
            }
        }
        for node in &ino_nodes {
            if let XfsAgBtreeRecords::Inode(records) = &node.records {
                inode_records
                    .try_reserve(records.len())
                    .map_err(|_| XfsError::NoMemory)?;
                inode_records.extend_from_slice(records);
            }
        }
        free_extents.sort_unstable_by_key(|record| (record.start_block, record.block_count));
        count_extents.sort_unstable_by_key(|record| (record.start_block, record.block_count));
        if free_extents != count_extents {
            return Err(XfsError::CorruptMetadata);
        }
        let mut end = 0u32;
        for (index, record) in free_extents.iter().enumerate() {
            let record_end = record
                .start_block
                .checked_add(record.block_count)
                .ok_or(XfsError::CorruptMetadata)?;
            if record.block_count == 0
                || record.start_block < 4
                || record_end > headers.free_space.length
                || (index != 0 && record.start_block < end)
            {
                return Err(XfsError::CorruptMetadata);
            }
            end = record_end;
        }
        let free_blocks = u32::try_from(free_extents.iter().try_fold(0u64, |sum, record| {
            sum.checked_add(record.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)
        })?)
        .map_err(|_| XfsError::CorruptMetadata)?;
        let longest = free_extents
            .iter()
            .map(|record| record.block_count)
            .max()
            .unwrap_or(0);
        if free_blocks != headers.free_space.free_blocks
            || longest != headers.free_space.longest_free_extent
        {
            return Err(XfsError::CorruptMetadata);
        }
        if reserved.iter().any(|block| {
            free_extents.iter().any(|extent| {
                *block >= extent.start_block && *block < extent.start_block + extent.block_count
            })
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        inode_records.sort_unstable_by_key(|record| record.start_inode);
        if inode_records
            .windows(2)
            .any(|records| records[0].start_inode >= records[1].start_inode)
        {
            return Err(XfsError::CorruptMetadata);
        }
        if !fino_nodes.is_empty() {
            let mut finodes = Vec::new();
            for node in &fino_nodes {
                if let XfsAgBtreeRecords::Inode(records) = &node.records {
                    finodes.extend_from_slice(records);
                }
            }
            finodes.sort_unstable_by_key(|record| record.start_inode);
            let mut expected = inode_records
                .iter()
                .copied()
                .filter(|record| record.free_count != 0)
                .collect::<Vec<_>>();
            expected.sort_unstable_by_key(|record| record.start_inode);
            if finodes != expected {
                return Err(XfsError::CorruptMetadata);
            }
        }
        Ok(XfsAgOwnershipSnapshot {
            ag,
            group: headers,
            freelist,
            free_extents,
            inode_records,
            bno_nodes,
            cnt_nodes,
            ino_nodes,
            fino_nodes,
        })
    }

    /// Stages a best-fit allocation from one AG.  The two free-space trees
    /// are rebuilt from the same canonical extent vector, so bnobt/cntbt
    /// cannot diverge during a split, merge, root promotion, or collapse.
    /// Nothing reaches a home block here.
    pub fn prepare_extent_allocation(
        &self,
        ag: u32,
        block_count: u32,
    ) -> XfsResult<XfsExtentAllocation> {
        if block_count == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut extents = snapshot.free_extents.clone();
        let chosen = extents
            .iter()
            .enumerate()
            .filter(|(_, extent)| extent.block_count >= block_count)
            .min_by_key(|(_, extent)| (extent.block_count, extent.start_block))
            .map(|(index, extent)| (index, *extent))
            .ok_or(XfsError::AddressOutOfRange)?;
        let start_block = chosen.1.start_block;
        if chosen.1.block_count == block_count {
            extents.remove(chosen.0);
        } else {
            extents[chosen.0].start_block = start_block
                .checked_add(block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
            extents[chosen.0].block_count -= block_count;
        }
        let transaction = self.stage_free_space_trees(ag, extents, &[])?;
        Ok(XfsExtentAllocation {
            ag,
            start_block,
            block_count,
            transaction,
        })
    }

    /// Selects several best-fit extents against one immutable AG snapshot and
    /// rebuilds its free-space trees once.  Calling `prepare_extent_allocation`
    /// repeatedly would select the same first extent each time because none of
    /// the staged AG images is live yet; this batch form is the transaction
    /// primitive used for simultaneous directory/attribute promotions.
    pub fn prepare_extent_allocations(
        &self,
        ag: u32,
        requests: &[u32],
    ) -> XfsResult<XfsExtentAllocationBatch> {
        if requests.is_empty() || requests.iter().any(|count| *count == 0) {
            return Err(XfsError::AddressOutOfRange);
        }
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut free = snapshot.free_extents.clone();
        let mut allocations = Vec::new();
        allocations
            .try_reserve_exact(requests.len())
            .map_err(|_| XfsError::NoMemory)?;
        for count in requests {
            let (index, chosen) = free
                .iter()
                .enumerate()
                .filter(|(_, extent)| extent.block_count >= *count)
                .min_by_key(|(_, extent)| (extent.block_count, extent.start_block))
                .map(|(index, extent)| (index, *extent))
                .ok_or(XfsError::AddressOutOfRange)?;
            let start_block = chosen.start_block;
            if chosen.block_count == *count {
                free.remove(index);
            } else {
                free[index].start_block = start_block
                    .checked_add(*count)
                    .ok_or(XfsError::AddressOutOfRange)?;
                free[index].block_count -= *count;
            }
            allocations.push(XfsExtentAllocation {
                ag,
                start_block,
                block_count: *count,
                transaction: XfsMetadataTransaction::default(),
            });
        }
        let transaction = self.stage_free_space_trees(ag, free, &[])?;
        Ok(XfsExtentAllocationBatch {
            ag,
            allocations,
            transaction,
        })
    }

    /// Stages a free operation, coalescing both predecessor and successor
    /// before constructing either index.  Double frees and AG metadata blocks
    /// are rejected from the verified ownership snapshot rather than quietly
    /// becoming overlapping bnobt records.
    pub fn prepare_extent_free(
        &self,
        ag: u32,
        start_block: u32,
        block_count: u32,
    ) -> XfsResult<XfsMetadataTransaction> {
        let end = start_block
            .checked_add(block_count)
            .ok_or(XfsError::AddressOutOfRange)?;
        if block_count == 0 || start_block < 4 || end > self.superblock.ag_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        // The public allocator contract treats a second ordinary free as
        // corruption.  Recovery alone gets the exact-range durable no-op
        // rule through the private batch helper below.
        if self
            .ag_ownership_snapshot(ag)?
            .free_extents
            .iter()
            .any(|extent| {
                extent
                    .start_block
                    .checked_add(extent.block_count)
                    .is_some_and(|extent_end| start_block < extent_end && end > extent.start_block)
            })
        {
            return Err(XfsError::CorruptMetadata);
        }
        self.stage_recovery_extent_frees(ag, &[(start_block, block_count)])
    }

    /// Stages every EFI free for one allocation group from one immutable
    /// ownership snapshot.  It is deliberately shared by ordinary freeing
    /// and recovery so an EFI crossing an AG boundary still rebuilds each
    /// group's bnobt/cntbt/AGF/AGFL exactly once.  An already-free *entire*
    /// range is a durable replay no-op; any partial overlap is corruption,
    /// not permission to repair a partially applied transaction.
    fn stage_recovery_extent_frees(
        &self,
        ag: u32,
        frees: &[(u32, u32)],
    ) -> XfsResult<XfsMetadataTransaction> {
        if frees.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut extents = snapshot.free_extents.clone();
        let mut requested = frees.to_vec();
        requested.sort_unstable_by_key(|(start, _)| *start);
        let mut changed = false;
        let freelist = self.ag_freelist(ag)?;
        let mut prior_end = 0u32;
        for (index, (start_block, block_count)) in requested.iter().copied().enumerate() {
            let end = start_block
                .checked_add(block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if block_count == 0
                || start_block < 4
                || end > self.superblock.ag_blocks
                || index != 0 && start_block < prior_end
            {
                return Err(XfsError::AddressOutOfRange);
            }
            prior_end = end;
            if snapshot
                .bno_nodes
                .iter()
                .chain(snapshot.cnt_nodes.iter())
                .chain(snapshot.ino_nodes.iter())
                .any(|node| node.block >= start_block && node.block < end)
                || freelist
                    .entries
                    .iter()
                    .any(|block| *block >= start_block && *block < end)
            {
                return Err(XfsError::CorruptMetadata);
            }
            let covered = extents.iter().find(|extent| {
                extent.start_block <= start_block
                    && extent
                        .start_block
                        .checked_add(extent.block_count)
                        .is_some_and(|extent_end| end <= extent_end)
            });
            if covered.is_some() {
                continue;
            }
            if extents.iter().any(|extent| {
                extent
                    .start_block
                    .checked_add(extent.block_count)
                    .is_some_and(|extent_end| start_block < extent_end && end > extent.start_block)
            }) {
                return Err(XfsError::CorruptMetadata);
            }
            extents.push(XfsAgFreeRecord {
                start_block,
                block_count,
            });
            changed = true;
        }
        if !changed {
            return Ok(XfsMetadataTransaction::default());
        }
        extents.sort_unstable_by_key(|extent| extent.start_block);
        let mut coalesced: Vec<XfsAgFreeRecord> = Vec::new();
        for extent in extents {
            if let Some(last) = coalesced.last_mut()
                && last.start_block.checked_add(last.block_count) == Some(extent.start_block)
            {
                last.block_count = last
                    .block_count
                    .checked_add(extent.block_count)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                coalesced.push(extent);
            }
        }
        self.stage_free_space_trees(ag, coalesced, &[])
    }

    /// Stages an inline-extent regular inode after an extent insertion,
    /// removal, split, or merge.  Bmap roots are deliberately not fabricated:
    /// callers whose fork no longer fits the inode receive an explicit error
    /// until the bmapbt buffer-item path is appended to the same transaction.
    pub fn stage_regular_inode_extents(
        &self,
        number: u64,
        mut extents: Vec<XfsExtent>,
        size: u64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Local | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let mut previous_end = 0u64;
        for (index, extent) in extents.iter().enumerate() {
            if extent.block_count == 0
                || extent
                    .file_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || (index != 0 && extent.file_block < previous_end)
            {
                return Err(XfsError::CorruptMetadata);
            }
            previous_end = extent.file_block + extent.block_count as u64;
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            inode.fork_offset as usize * 8
        };
        let bytes = extents
            .len()
            .checked_mul(16)
            .ok_or(XfsError::AddressOutOfRange)?;
        if fork_end < fork_begin || bytes > fork_end - fork_begin {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut after_inode = raw.clone();
        after_inode[5] = XfsForkFormat::Extents as u8;
        put_be64(&mut after_inode, 56, size)?;
        let data_blocks = Self::inode_extent_blocks(&extents)?;
        let attr_blocks = self.attribute_fork_owned_blocks(number, &inode)?;
        let owned_blocks = data_blocks
            .checked_add(attr_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        let blocks = owned_blocks
            .checked_mul((self.superblock.block_size / 512) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        put_be64(&mut after_inode, 64, blocks)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after_inode, 24, extents.len() as u64)?;
        } else {
            put_be32(
                &mut after_inode,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        after_inode[fork_begin..fork_end].fill(0);
        for (index, extent) in extents.iter().enumerate() {
            after_inode[fork_begin + index * 16..fork_begin + (index + 1) * 16]
                .copy_from_slice(&encode_xfs_extent(*extent)?);
        }
        self.stage_inode_image(number, raw, after_inode, transaction)
    }

    /// Converts an overflowing inline data fork into a real external bmapbt.
    /// `blocks` must be freshly allocated, non-file blocks from the same
    /// allocator transaction; every leaf/interior image and the inode root is
    /// appended before the transaction can be committed.
    pub fn stage_regular_inode_bmap(
        &self,
        number: u64,
        mut extents: Vec<XfsExtent>,
        size: u64,
        blocks: &[u64],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o100000 || inode.fork_offset != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let fork_begin = inode.core_bytes as usize;
        let fork_bytes = raw
            .len()
            .checked_sub(fork_begin)
            .ok_or(XfsError::CorruptMetadata)?;
        let needed = bmap_external_blocks(self.superblock, fork_bytes, extents.len())?;
        if needed == 0
            || blocks.len() != needed
            || blocks.iter().any(|block| {
                *block >= self.superblock.data_blocks
                    || *block % (self.superblock.ag_blocks as u64) < 4
            })
            || blocks
                .iter()
                .enumerate()
                .any(|(index, block)| blocks[..index].contains(block))
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let header = if self.superblock.is_v5() {
            72usize
        } else {
            24usize
        };
        let leaf_capacity = (self.superblock.block_size as usize - header) / 16;
        let interior_capacity = (self.superblock.block_size as usize - header) / 16;
        let mut used = 0usize;
        let leaves = extents.len().div_ceil(leaf_capacity);
        let mut current = Vec::new();
        current
            .try_reserve_exact(leaves)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..leaves {
            let start = index * leaf_capacity;
            let end = (start + leaf_capacity).min(extents.len());
            let block = blocks[used];
            used += 1;
            let bytes = serialize_bmap_node(
                self.superblock,
                number,
                block,
                0,
                if index == 0 { 0 } else { blocks[used - 2] },
                if index + 1 == leaves { 0 } else { blocks[used] },
                &extents[start..end],
                &[],
            )?;
            let before = self.read_data_fs_block(block)?;
            transaction.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Btree,
                basic_block: block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?,
                before,
                after: bytes,
            });
            current.push((block, extents[start]));
        }
        let mut level = 1u16;
        while current.len() > (fork_bytes - 4) / 16 {
            let parents = current.len().div_ceil(interior_capacity);
            let mut next = Vec::new();
            next.try_reserve_exact(parents)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..parents {
                let start = index * interior_capacity;
                let end = (start + interior_capacity).min(current.len());
                let block = *blocks.get(used).ok_or(XfsError::AddressOutOfRange)?;
                used += 1;
                let keys = current[start..end]
                    .iter()
                    .map(|entry| entry.1)
                    .collect::<Vec<_>>();
                let children = current[start..end]
                    .iter()
                    .map(|entry| entry.0)
                    .collect::<Vec<_>>();
                let bytes = serialize_bmap_node(
                    self.superblock,
                    number,
                    block,
                    level,
                    if index == 0 { 0 } else { blocks[used - 2] },
                    if index + 1 == parents {
                        0
                    } else {
                        *blocks.get(used).ok_or(XfsError::AddressOutOfRange)?
                    },
                    &keys,
                    &children,
                )?;
                let before = self.read_data_fs_block(block)?;
                transaction.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after: bytes,
                });
                next.push((block, keys[0]));
            }
            current = next;
            level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        if used != blocks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let root_capacity = (fork_bytes - 4) / 16;
        let mut after = raw.clone();
        after[5] = XfsForkFormat::Btree as u8;
        put_be16(&mut after, fork_begin, level)?;
        put_be16(
            &mut after,
            fork_begin + 2,
            u16::try_from(current.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        after[fork_begin + 4..].fill(0);
        for (index, (child, key)) in current.iter().enumerate() {
            put_be64(&mut after, fork_begin + 4 + index * 8, key.file_block)?;
            put_be64(
                &mut after,
                fork_begin + 4 + root_capacity * 8 + index * 8,
                *child,
            )?;
        }
        put_be64(&mut after, 56, size)?;
        // di_nblocks covers both forks.  The replacement data mapping is
        // counted exactly once here; the current attribute fork contributes
        // its mappings and, when present, its external BMBT homes.
        let data_blocks = Self::inode_extent_blocks(&extents)?;
        let bmap_blocks = u64::try_from(blocks.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let data_owned_blocks = data_blocks
            .checked_add(bmap_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        let attr_blocks = self.attribute_fork_owned_blocks(number, &inode)?;
        let owned_blocks = data_owned_blocks
            .checked_add(attr_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        let sectors = owned_blocks
            .checked_mul((self.superblock.block_size / 512) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        put_be64(&mut after, 64, sectors)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after, 24, extents.len() as u64)?;
        } else {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Prepares a regular-file overwrite/extension. Existing unwritten
    /// blocks are converted to written mappings, existing written blocks are
    /// retained for RMW, and every hole in the range is allocated before the
    /// replacement extent set is staged with the inode.
    pub fn prepare_regular_write(
        &self,
        number: u64,
        offset: u64,
        length: usize,
    ) -> XfsResult<XfsRegularWrite> {
        if length == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let (inode, _) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let end = offset
            .checked_add(length as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block_size = self.superblock.block_size as u64;
        let first = offset / block_size;
        let last = end.checked_sub(1).ok_or(XfsError::AddressOutOfRange)? / block_size;
        let _count = u32::try_from(
            last.checked_sub(first)
                .and_then(|span| span.checked_add(1))
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        let old = if inode.data_format == XfsForkFormat::Extents {
            self.inode_data_extents(number)?
        } else {
            self.inode_bmbt_extents(number)?
        };
        let old_bmap_nodes = if inode.data_format == XfsForkFormat::Btree {
            self.inode_bmbt_blocks(number)?
        } else {
            Vec::new()
        };
        let mut holes = 0u32;
        for file_block in first..=last {
            if old.iter().all(|extent| {
                file_block < extent.file_block
                    || file_block >= extent.file_block + extent.block_count as u64
            }) {
                holes = holes.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        let (ag, _) = self.split_inode_number(number)?;
        // File data is placed in the inode's AG.  Bmap metadata is reserved
        // independently after the final fanout is known, allowing an AG-full
        // leaf/interior split to use free space from another AG.
        let data_allocation = if holes == 0 {
            None
        } else {
            Some(self.prepare_extent_allocation(ag, holes)?)
        };
        let mut physical_cursor = data_allocation
            .as_ref()
            .map(|item| (ag as u64) * self.superblock.ag_blocks as u64 + item.start_block as u64)
            .unwrap_or(0);
        let mut replacement = Vec::new();
        let mut zero_before_write = Vec::new();
        for file_block in first..=last {
            let source = old
                .iter()
                .find(|extent| {
                    file_block >= extent.file_block
                        && file_block < extent.file_block + extent.block_count as u64
                })
                .copied();
            let mapped = match source {
                Some(extent) => {
                    let physical = extent.start_block + file_block - extent.file_block;
                    if extent.unwritten {
                        zero_before_write.push(XfsExtent {
                            unwritten: false,
                            file_block,
                            start_block: physical,
                            block_count: 1,
                        });
                    }
                    XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: physical,
                        block_count: 1,
                    }
                }
                None => {
                    let physical = physical_cursor;
                    physical_cursor = physical_cursor
                        .checked_add(1)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    let extent = XfsExtent {
                        unwritten: false,
                        file_block,
                        start_block: physical,
                        block_count: 1,
                    };
                    zero_before_write.push(extent);
                    extent
                }
            };
            push_merged_extent(&mut replacement, mapped)?;
        }
        let mut extents = Vec::new();
        for extent in old.iter().copied() {
            let end_block = extent.file_block + extent.block_count as u64;
            if extent.file_block < first {
                push_merged_extent(
                    &mut extents,
                    XfsExtent {
                        block_count: u32::try_from(first.min(end_block) - extent.file_block)
                            .map_err(|_| XfsError::AddressOutOfRange)?,
                        ..extent
                    },
                )?;
            }
            if end_block > last + 1 {
                let start = (last + 1).max(extent.file_block);
                push_merged_extent(
                    &mut extents,
                    XfsExtent {
                        file_block: start,
                        start_block: extent.start_block + (start - extent.file_block),
                        block_count: u32::try_from(end_block - start)
                            .map_err(|_| XfsError::AddressOutOfRange)?,
                        ..extent
                    },
                )?;
            }
        }
        for extent in replacement.iter().copied() {
            push_merged_extent(&mut extents, extent)?;
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let mut merged: Vec<XfsExtent> = Vec::new();
        for extent in extents {
            push_merged_extent(&mut merged, extent)?;
        }
        let fork_bytes = self.superblock.inode_size as usize - inode.core_bytes as usize;
        let bmap_nodes = bmap_external_blocks(self.superblock, fork_bytes, merged.len())?;
        let additional_nodes = bmap_nodes.saturating_sub(old_bmap_nodes.len());
        let reused = if bmap_nodes == 0 {
            0
        } else {
            old_bmap_nodes.len().min(bmap_nodes)
        };
        let reclaimed = if bmap_nodes == 0 {
            old_bmap_nodes.clone()
        } else {
            old_bmap_nodes[reused..].to_vec()
        };
        let exclusions = data_allocation
            .as_ref()
            .map(|item| vec![(ag, item.start_block, item.block_count)])
            .unwrap_or_default();
        let new_nodes = self.reserve_bmap_metadata_blocks(ag, additional_nodes, &exclusions)?;
        let mut groups: Vec<(u32, Vec<(u32, u32)>, Vec<u32>)> = Vec::new();
        if let Some(allocation) = &data_allocation {
            groups.push((
                ag,
                vec![(allocation.start_block, allocation.block_count)],
                Vec::new(),
            ));
        }
        for block in &new_nodes {
            let node_ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, allocations, _)) = groups
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == node_ag)
            {
                allocations.push((relative, 1));
            } else {
                groups.push((node_ag, vec![(relative, 1)], Vec::new()));
            }
        }
        for block in &reclaimed {
            let node_ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, _, returned)) = groups
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == node_ag)
            {
                returned.push(relative);
            } else {
                groups.push((node_ag, Vec::new(), vec![relative]));
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for (node_ag, allocations, returned) in groups {
            let staged = self.stage_extent_delta(node_ag, &allocations, &returned)?;
            metadata.buffers.extend(staged.buffers);
        }
        if bmap_nodes == 0 {
            self.stage_regular_inode_extents(
                number,
                merged.clone(),
                end.max(inode.size),
                &mut metadata,
            )?;
        } else {
            let mut nodes = old_bmap_nodes[..reused].to_vec();
            nodes.extend_from_slice(&new_nodes);
            self.stage_regular_inode_bmap(
                number,
                merged.clone(),
                end.max(inode.size),
                &nodes,
                &mut metadata,
            )?;
        }
        Ok(XfsRegularWrite {
            inode: number,
            offset,
            length,
            allocated: data_allocation
                .map(|item| XfsExtent {
                    unwritten: false,
                    file_block: first,
                    start_block: (ag as u64) * self.superblock.ag_blocks as u64
                        + item.start_block as u64,
                    block_count: holes,
                })
                .into_iter()
                .collect(),
            mappings: replacement,
            zero_before_write,
            copy_before_write: Vec::new(),
            metadata,
        })
    }

    /// Writes a prepared overwrite/extension, then logs and installs the
    /// inode and allocation-group metadata. Data write failure happens before
    /// log publication; metadata failure can leave only unreachable blocks.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn write_regular_at_live(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        transaction_id: u32,
        number: u64,
        offset: u64,
        data: &[u8],
    ) -> XfsResult<usize> {
        let prepared = self.prepare_regular_write(number, offset, data.len())?;
        self.write_prepared_regular_at_live(ring, ail, transaction_id, prepared, offset, data)
    }

    fn write_prepared_regular_at_live(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        transaction_id: u32,
        prepared: XfsRegularWrite,
        offset: u64,
        data: &[u8],
    ) -> XfsResult<usize> {
        let block_size = self.superblock.block_size as usize;
        let mut cursor = 0usize;
        for extent in &prepared.mappings {
            for block in 0..extent.block_count as u64 {
                let file_block = extent.file_block + block;
                let physical = extent.start_block + block;
                let file_offset = file_block
                    .checked_mul(block_size as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let zero = prepared.zero_before_write.iter().any(|range| {
                    physical >= range.start_block
                        && physical < range.start_block + range.block_count as u64
                });
                let mut image = if zero {
                    vec![0; block_size]
                } else if let Some((old, _)) = prepared
                    .copy_before_write
                    .iter()
                    .find(|(_, new)| *new == physical)
                {
                    self.read_data_fs_block(*old)?
                } else {
                    self.read_data_fs_block(physical)?
                };
                let begin = offset.max(file_offset);
                let end = (offset + data.len() as u64).min(file_offset + block_size as u64);
                if begin < end {
                    let source =
                        usize::try_from(begin - offset).map_err(|_| XfsError::AddressOutOfRange)?;
                    let target = usize::try_from(begin - file_offset)
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                    let count =
                        usize::try_from(end - begin).map_err(|_| XfsError::AddressOutOfRange)?;
                    image[target..target + count].copy_from_slice(&data[source..source + count]);
                    cursor += count;
                }
                self.write_data_fs_block(physical, &image)?;
            }
        }
        if cursor != data.len() {
            return Err(XfsError::CorruptMetadata);
        }
        // The inode mapping must never become durable before the FUA data
        // writes are fenced to the data member.
        self.data.flush().map_err(XfsError::from)?;
        self.commit_metadata_transaction(ring, ail, transaction_id, &prepared.metadata)?;
        Ok(data.len())
    }

    /// The zero-range writer intentionally synthesizes zeroes per mapped
    /// filesystem block.  Holding a user-sized `Vec<u8>` for a multi-gigabyte
    /// fallocate request is both unnecessary and an avoidable OOM surface.
    pub(crate) fn zero_prepared_regular_at_live(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        transaction_id: u32,
        prepared: XfsRegularWrite,
        offset: u64,
    ) -> XfsResult<usize> {
        let block_size = self.superblock.block_size as usize;
        let end = offset
            .checked_add(u64::try_from(prepared.length).map_err(|_| XfsError::AddressOutOfRange)?)
            .ok_or(XfsError::AddressOutOfRange)?;
        for extent in &prepared.mappings {
            for block in 0..u64::from(extent.block_count) {
                let file_block = extent
                    .file_block
                    .checked_add(block)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let physical = extent
                    .start_block
                    .checked_add(block)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let file_offset = file_block
                    .checked_mul(block_size as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let fresh = prepared.zero_before_write.iter().any(|range| {
                    physical >= range.start_block
                        && physical < range.start_block + u64::from(range.block_count)
                });
                let mut image = if fresh {
                    vec![0; block_size]
                } else if let Some((old, _)) = prepared
                    .copy_before_write
                    .iter()
                    .find(|(_, new)| *new == physical)
                {
                    self.read_data_fs_block(*old)?
                } else {
                    self.read_data_fs_block(physical)?
                };
                let begin = offset.max(file_offset);
                let limit = end.min(
                    file_offset
                        .checked_add(block_size as u64)
                        .ok_or(XfsError::AddressOutOfRange)?,
                );
                if begin >= limit {
                    return Err(XfsError::CorruptMetadata);
                }
                let from = usize::try_from(begin - file_offset)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let to = usize::try_from(limit - file_offset)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                image
                    .get_mut(from..to)
                    .ok_or(XfsError::CorruptMetadata)?
                    .fill(0);
                self.write_data_fs_block(physical, &image)?;
            }
        }
        self.data.flush().map_err(XfsError::from)?;
        self.commit_metadata_transaction(ring, ail, transaction_id, &prepared.metadata)?;
        Ok(prepared.length)
    }

    /// Allocates an unwritten regular-file extent. `keep_size` keeps EOF
    /// stable, matching fallocate's preallocation contract; a later write
    /// converts the mapped range to written data in its inode transaction.
    pub fn prepare_regular_fallocate(
        &self,
        number: u64,
        offset: u64,
        length: u64,
        keep_size: bool,
    ) -> XfsResult<XfsRegularWrite> {
        if length == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let (inode, _) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let end = offset
            .checked_add(length)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block_size = self.superblock.block_size as u64;
        let first = offset / block_size;
        let last = end.checked_sub(1).ok_or(XfsError::AddressOutOfRange)? / block_size;
        let mut extents = if inode.data_format == XfsForkFormat::Extents {
            self.inode_data_extents(number)?
        } else {
            self.inode_bmbt_extents(number)?
        };
        let old_bmap_nodes = if inode.data_format == XfsForkFormat::Btree {
            self.inode_bmbt_blocks(number)?
        } else {
            Vec::new()
        };
        let mut holes = Vec::new();
        holes
            .try_reserve_exact(
                usize::try_from(last - first + 1).map_err(|_| XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        for file_block in first..=last {
            if xfs_extent_at(&extents, file_block)?.is_none() {
                holes.push(file_block);
            }
        }
        if holes.is_empty() && (keep_size || end <= inode.size) {
            return Ok(XfsRegularWrite {
                inode: number,
                offset,
                length: usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?,
                allocated: Vec::new(),
                mappings: Vec::new(),
                zero_before_write: Vec::new(),
                copy_before_write: Vec::new(),
                metadata: XfsMetadataTransaction::default(),
            });
        }
        let (ag, _) = self.split_inode_number(number)?;
        let hole_count = u32::try_from(holes.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let allocation = if hole_count == 0 {
            None
        } else {
            Some(self.prepare_extent_allocation(ag, hole_count)?)
        };
        let mut allocated = Vec::new();
        allocated
            .try_reserve_exact(holes.len())
            .map_err(|_| XfsError::NoMemory)?;
        if let Some(allocation) = &allocation {
            extents
                .try_reserve(holes.len())
                .map_err(|_| XfsError::NoMemory)?;
            let physical = (ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(allocation.start_block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            for (index, file_block) in holes.iter().copied().enumerate() {
                let extent = XfsExtent {
                    unwritten: true,
                    file_block,
                    start_block: physical
                        .checked_add(index as u64)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    block_count: 1,
                };
                push_merged_extent(&mut allocated, extent)?;
                push_merged_extent(&mut extents, extent)?;
            }
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let mut merged: Vec<XfsExtent> = Vec::new();
        merged
            .try_reserve_exact(extents.len())
            .map_err(|_| XfsError::NoMemory)?;
        for extent in extents {
            push_merged_extent(&mut merged, extent)?;
        }
        let fork_bytes = self.superblock.inode_size as usize - inode.core_bytes as usize;
        let needed = bmap_external_blocks(self.superblock, fork_bytes, merged.len())?;
        let reused = if needed == 0 {
            0
        } else {
            old_bmap_nodes.len().min(needed)
        };
        let reclaimed = if needed == 0 {
            old_bmap_nodes.clone()
        } else {
            old_bmap_nodes[reused..].to_vec()
        };
        let exclusions = allocation
            .as_ref()
            .map(|item| vec![(ag, item.start_block, item.block_count)])
            .unwrap_or_default();
        let new_nodes = self.reserve_bmap_metadata_blocks(
            ag,
            needed.saturating_sub(old_bmap_nodes.len()),
            &exclusions,
        )?;
        let mut groups: Vec<(u32, Vec<(u32, u32)>, Vec<u32>)> = Vec::new();
        if let Some(allocation) = &allocation {
            let mut allocations = Vec::new();
            allocations.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            allocations.push((allocation.start_block, allocation.block_count));
            groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            groups.push((ag, allocations, Vec::new()));
        }
        for block in &new_nodes {
            let node_ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, allocations, _)) = groups
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == node_ag)
            {
                allocations.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                allocations.push((relative, 1));
            } else {
                let mut allocations = Vec::new();
                allocations.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                allocations.push((relative, 1));
                groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                groups.push((node_ag, allocations, Vec::new()));
            }
        }
        for block in &reclaimed {
            let node_ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, _, releases)) = groups
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == node_ag)
            {
                releases.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                releases.push(relative);
            } else {
                let mut releases = Vec::new();
                releases.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                releases.push(relative);
                groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                groups.push((node_ag, Vec::new(), releases));
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for (node_ag, allocations, releases) in groups {
            let staged = self.stage_extent_delta(node_ag, &allocations, &releases)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        if needed == 0 {
            self.stage_regular_inode_extents(
                number,
                merged,
                if keep_size {
                    inode.size
                } else {
                    inode.size.max(end)
                },
                &mut metadata,
            )?;
        } else {
            let mut nodes = old_bmap_nodes[..reused].to_vec();
            nodes.extend_from_slice(&new_nodes);
            self.stage_regular_inode_bmap(
                number,
                merged,
                if keep_size {
                    inode.size
                } else {
                    inode.size.max(end)
                },
                &nodes,
                &mut metadata,
            )?;
        }
        let mut mappings = Vec::new();
        mappings
            .try_reserve_exact(allocated.len())
            .map_err(|_| XfsError::NoMemory)?;
        mappings.extend_from_slice(&allocated);
        let mut zero_before_write = Vec::new();
        zero_before_write
            .try_reserve_exact(allocated.len())
            .map_err(|_| XfsError::NoMemory)?;
        zero_before_write.extend_from_slice(&allocated);
        Ok(XfsRegularWrite {
            inode: number,
            offset,
            length: usize::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?,
            allocated,
            mappings,
            zero_before_write,
            copy_before_write: Vec::new(),
            metadata,
        })
    }

    /// Stages a shrinking truncate.  Whole tail extents are returned through
    /// the same AG allocator and the final partial extent is split before its
    /// physical tail is freed.  Growing a sparse file only changes inode EOF.
    pub fn prepare_regular_truncate(
        &self,
        number: u64,
        size: u64,
    ) -> XfsResult<XfsMetadataTransaction> {
        let (inode, _) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let block_size = self.superblock.block_size as u64;
        let keep = size.div_ceil(block_size);
        let mut extents = if inode.data_format == XfsForkFormat::Extents {
            self.inode_data_extents(number)?
        } else {
            self.inode_bmbt_extents(number)?
        };
        let old_bmap_nodes = if inode.data_format == XfsForkFormat::Btree {
            self.inode_bmbt_blocks(number)?
        } else {
            Vec::new()
        };
        let mut frees = Vec::new();
        for extent in &mut extents {
            let end = extent
                .file_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
            if extent.file_block >= keep {
                frees.push(*extent);
                extent.block_count = 0;
            } else if end > keep {
                let drop = u32::try_from(end - keep).map_err(|_| XfsError::AddressOutOfRange)?;
                frees.push(XfsExtent {
                    unwritten: extent.unwritten,
                    file_block: keep,
                    start_block: extent.start_block + (keep - extent.file_block),
                    block_count: drop,
                });
                extent.block_count -= drop;
            }
        }
        extents.retain(|extent| extent.block_count != 0);
        let fork_bytes = self.superblock.inode_size as usize - inode.core_bytes as usize;
        let needed = bmap_external_blocks(self.superblock, fork_bytes, extents.len())?;
        let reused = if needed == 0 {
            0
        } else {
            old_bmap_nodes.len().min(needed)
        };
        let reclaimed = if needed == 0 {
            old_bmap_nodes.clone()
        } else {
            old_bmap_nodes[reused..].to_vec()
        };
        let (inode_ag, _) = self.split_inode_number(number)?;
        let new_nodes = self.reserve_bmap_metadata_blocks(
            inode_ag,
            needed.saturating_sub(old_bmap_nodes.len()),
            &[],
        )?;
        let mut groups: Vec<(u32, Vec<(u32, u32)>, Vec<u32>)> = Vec::new();
        for block in &new_nodes {
            let ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, allocations, _)) =
                groups.iter_mut().find(|(candidate, _, _)| *candidate == ag)
            {
                allocations.push((relative, 1));
            } else {
                groups.push((ag, vec![(relative, 1)], Vec::new()));
            }
        }
        for free in frees {
            for block in 0..free.block_count as u64 {
                let physical = free
                    .start_block
                    .checked_add(block)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let relative = u32::try_from(physical % self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, _, releases)) =
                    groups.iter_mut().find(|(candidate, _, _)| *candidate == ag)
                {
                    releases.push(relative);
                } else {
                    groups.push((ag, Vec::new(), vec![relative]));
                }
            }
        }
        for block in &reclaimed {
            let ag = u32::try_from(*block / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let relative = u32::try_from(*block % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, _, releases)) =
                groups.iter_mut().find(|(candidate, _, _)| *candidate == ag)
            {
                releases.push(relative);
            } else {
                groups.push((ag, Vec::new(), vec![relative]));
            }
        }
        let mut transaction = XfsMetadataTransaction::default();
        for (ag, allocations, releases) in groups {
            let staged = self.stage_extent_delta(ag, &allocations, &releases)?;
            transaction.buffers.extend(staged.buffers);
        }
        if needed == 0 {
            self.stage_regular_inode_extents(number, extents, size, &mut transaction)?;
        } else {
            let mut nodes = old_bmap_nodes[..reused].to_vec();
            nodes.extend_from_slice(&new_nodes);
            self.stage_regular_inode_bmap(number, extents, size, &nodes, &mut transaction)?;
        }
        Ok(transaction)
    }

    /// Reserves one inode bit from the lowest non-empty inobt record and
    /// stages matching inobt/finobt/AGI images.  The returned inode is not
    /// visible until its core buffer is appended to this transaction.
    pub fn prepare_inode_allocation(&self, ag: u32) -> XfsResult<XfsInodeAllocation> {
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut records = snapshot.inode_records.clone();
        let (record_index, bit) = records
            .iter()
            .enumerate()
            .filter(|(_, record)| record.free_mask != 0)
            .map(|(index, record)| (index, record.free_mask.trailing_zeros()))
            .next()
            .ok_or(XfsError::AddressOutOfRange)?;
        let ag_inode = records[record_index]
            .start_inode
            .checked_add(bit)
            .ok_or(XfsError::AddressOutOfRange)?;
        records[record_index].free_mask &= !(1u64 << bit);
        records[record_index].free_count = records[record_index].free_mask.count_ones();
        let transaction = self.stage_inode_trees(ag, records)?;
        let shift =
            self.superblock.ag_block_log as u32 + self.superblock.inodes_per_block_log as u32;
        let inode = (ag as u64)
            .checked_shl(shift)
            .and_then(|base| base.checked_add(ag_inode as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(XfsInodeAllocation {
            ag,
            ag_inode,
            inode,
            remote_data: None,
            transaction,
        })
    }

    /// Reserves a newly allocated inode and its remote symlink body.
    pub fn prepare_inode_allocation_with_remote(
        &self,
        ag: u32,
        blocks: u32,
    ) -> XfsResult<XfsInodeAllocation> {
        if blocks == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        // inode bits, the remote body, and every allocator header are one AG
        // ownership decision.  Do not compose independently prepared inode
        // and extent transactions: both can consume the same AGFL block even
        // when their byte images happen not to overlap.
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut records = snapshot.inode_records.clone();
        let (record_index, bit) = records
            .iter()
            .enumerate()
            .filter(|(_, record)| record.free_mask != 0)
            .map(|(index, record)| (index, record.free_mask.trailing_zeros()))
            .next()
            .ok_or(XfsError::AddressOutOfRange)?;
        let ag_inode = records[record_index]
            .start_inode
            .checked_add(bit)
            .ok_or(XfsError::AddressOutOfRange)?;
        records[record_index].free_mask &= !(1u64 << bit);
        records[record_index].free_count = records[record_index].free_mask.count_ones();

        let mut extents = snapshot.free_extents.clone();
        let (index, chosen) = extents
            .iter()
            .enumerate()
            .filter(|(_, extent)| extent.block_count >= blocks)
            .min_by_key(|(_, extent)| (extent.block_count, extent.start_block))
            .map(|(index, extent)| (index, *extent))
            .ok_or(XfsError::AddressOutOfRange)?;
        let start_block = chosen.start_block;
        if chosen.block_count == blocks {
            extents.remove(index);
        } else {
            extents[index].start_block = start_block
                .checked_add(blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            extents[index].block_count -= blocks;
        }
        let transaction =
            self.stage_combined_inode_and_free_space_trees(&snapshot, records, extents)?;
        let shift =
            self.superblock.ag_block_log as u32 + self.superblock.inodes_per_block_log as u32;
        let inode = (ag as u64)
            .checked_shl(shift)
            .and_then(|base| base.checked_add(ag_inode as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(XfsInodeAllocation {
            ag,
            ag_inode,
            inode,
            remote_data: Some(XfsExtentAllocation {
                ag,
                start_block,
                block_count: blocks,
                transaction: XfsMetadataTransaction::default(),
            }),
            transaction,
        })
    }

    /// Initializes an inode selected by [`prepare_inode_allocation`] and
    /// appends its exact inode-block image to `transaction`.  Writable mounts
    /// are v5-only, so newly allocated inodes always use the checksummed v3
    /// core; no v2 layout is guessed on a v5 filesystem.
    pub fn stage_new_inode(
        &self,
        allocation: &XfsInodeAllocation,
        initial: XfsNewInode,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        // Do not let callers accidentally publish the new core without the
        // inobt/finobt/AGI bit transition that owns its number.  Build into a
        // private copy so a malformed initializer leaves the caller's
        // transaction unchanged.
        let mut staged = transaction.clone();
        staged
            .buffers
            .extend(allocation.transaction.buffers.clone());
        if !self.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        let (ag, agino) = self.split_inode_number(allocation.inode)?;
        if ag != allocation.ag
            || agino != allocation.ag_inode as u64
            || initial.mode & 0o170000 == 0
        {
            return Err(XfsError::CorruptMetadata);
        }
        let inode_block = agino >> self.superblock.inodes_per_block_log;
        let inode_index = agino & (self.superblock.inodes_per_block as u64 - 1);
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(inode_block))
            .ok_or(XfsError::AddressOutOfRange)?;
        let before_block = self.read_data_fs_block(fs_block)?;
        let offset = (inode_index as usize)
            .checked_mul(self.superblock.inode_size as usize)
            .ok_or(XfsError::AddressOutOfRange)?;
        let before = slice(&before_block, offset, self.superblock.inode_size as usize)?.to_vec();
        let mut after = vec![0; before.len()];
        put_be16(&mut after, 0, XFS_DINODE_MAGIC)?;
        put_be16(&mut after, 2, initial.mode)?;
        after[4] = 3;
        let is_directory = initial.mode & 0o170000 == 0o040000;
        let is_symlink = initial.mode & 0o170000 == 0o120000;
        after[5] = if is_directory {
            XfsForkFormat::Local as u8
        } else {
            XfsForkFormat::Extents as u8
        };
        put_be32(&mut after, 8, initial.uid)?;
        put_be32(&mut after, 12, initial.gid)?;
        put_be32(&mut after, 16, if is_directory { 2 } else { 1 })?;
        put_be16(&mut after, 20, initial.project_id as u16)?;
        put_be16(&mut after, 22, (initial.project_id >> 16) as u16)?;
        put_be32(&mut after, 76, 0)?;
        put_be16(&mut after, 80, 0)?;
        after[83] = XfsForkFormat::Extents as u8;
        put_be32(&mut after, 92, allocation.inode as u32)?;
        put_be32(&mut after, 96, u32::MAX)?;
        put_be64(&mut after, 152, allocation.inode)?;
        after[160..176].copy_from_slice(&self.superblock.uuid.0);
        if is_directory {
            let parent = initial.parent.ok_or(XfsError::AddressOutOfRange)?;
            let payload = serialize_shortform_directory(
                parent,
                &[],
                self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0,
                64,
            )?;
            let fork = 176usize;
            if payload.len() > after.len().saturating_sub(fork) {
                return Err(XfsError::AddressOutOfRange);
            }
            after[fork..fork + payload.len()].copy_from_slice(&payload);
            put_be64(&mut after, 56, payload.len() as u64)?;
        } else if is_symlink {
            let target = initial
                .symlink_target
                .as_deref()
                .ok_or(XfsError::AddressOutOfRange)?;
            let fork = 176usize;
            let inline = after.len().saturating_sub(fork);
            put_be64(
                &mut after,
                56,
                u64::try_from(target.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
            if target.len() <= inline {
                after[5] = XfsForkFormat::Local as u8;
                after[fork..fork + target.len()].copy_from_slice(target);
            } else {
                // A remote symlink uses a normal data-fork extent.  Allocate
                // from the new inode's AG and attach both the free-space
                // delta and the FUA data images to this same namespace
                // transaction before the parent directory can name it.
                let block_size = self.superblock.block_size as usize;
                let blocks = u32::try_from(target.len().div_ceil(block_size))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let allocation_data = allocation
                    .remote_data
                    .as_ref()
                    .ok_or(XfsError::CorruptMetadata)?;
                if allocation_data.ag != ag || allocation_data.block_count != blocks {
                    return Err(XfsError::CorruptMetadata);
                }
                let first = (ag as u64)
                    .checked_mul(self.superblock.ag_blocks as u64)
                    .and_then(|base| base.checked_add(allocation_data.start_block as u64))
                    .ok_or(XfsError::AddressOutOfRange)?;
                after[5] = XfsForkFormat::Extents as u8;
                after[fork..].fill(0);
                after[fork..fork + 16].copy_from_slice(&encode_xfs_extent(XfsExtent {
                    unwritten: false,
                    file_block: 0,
                    start_block: first,
                    block_count: blocks,
                })?);
                put_be64(
                    &mut after,
                    64,
                    u64::from(blocks)
                        .checked_mul((self.superblock.block_size / 512) as u64)
                        .ok_or(XfsError::AddressOutOfRange)?,
                )?;
                if be64(&after, 120)? & XfsInode::DIFLAG2_NREXT64 != 0 {
                    put_be64(&mut after, 24, 1)?;
                } else {
                    put_be32(&mut after, 76, 1)?;
                }
                for index in 0..blocks as u64 {
                    let fs_block = first
                        .checked_add(index)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    let begin = usize::try_from(index)
                        .map_err(|_| XfsError::AddressOutOfRange)?
                        .checked_mul(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    let end = target.len().min(
                        begin
                            .checked_add(block_size)
                            .ok_or(XfsError::AddressOutOfRange)?,
                    );
                    let mut image = vec![0; block_size];
                    image[..end - begin].copy_from_slice(&target[begin..end]);
                    staged.data_writes.push(XfsStagedDataWrite {
                        fs_block,
                        before: self.read_data_fs_block(fs_block)?,
                        after: image,
                    });
                }
            }
        } else if initial.parent.is_some() {
            return Err(XfsError::AddressOutOfRange);
        }
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(allocation.inode, before, after, &mut staged)?;
        *transaction = staged;
        Ok(())
    }

    /// Returns an inode bit to inobt and inserts/merges its finobt record.
    /// A bit that is already free is corruption, not an idempotent free.
    pub fn prepare_inode_free(&self, inode: u64) -> XfsResult<XfsMetadataTransaction> {
        let (ag, ag_inode) = self.split_inode_number(inode)?;
        let ag_inode = u32::try_from(ag_inode).map_err(|_| XfsError::AddressOutOfRange)?;
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut records = snapshot.inode_records.clone();
        let index = records
            .iter()
            .position(|record| {
                ag_inode >= record.start_inode && ag_inode < record.start_inode.saturating_add(64)
            })
            .ok_or(XfsError::AddressOutOfRange)?;
        let bit = ag_inode - records[index].start_inode;
        if bit >= 64 || records[index].free_mask & (1u64 << bit) != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        records[index].free_mask |= 1u64 << bit;
        records[index].free_count = records[index].free_mask.count_ones();
        self.stage_inode_trees(ag, records)
    }

    /// Commits a staged allocator transaction.  The log is durable before
    /// any metadata home image, and its AIL entry is retired only after every
    /// home image has completed FUA plus the data-device flush.  The record's
    /// tail is derived from the ring's oldest uncheckpointed position; no
    /// caller may substitute the current head and truncate crash replay.
    pub(crate) fn commit_metadata_transaction(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        transaction_id: u32,
        transaction: &XfsMetadataTransaction,
    ) -> XfsResult<u64> {
        let mut staged = transaction.clone();
        // Bitmap and summary inode blocks reside on the data device, but are
        // allocation metadata, not ordinary file data.  They must therefore
        // be BUF items in the same log record as the mapping that consumes
        // them; direct pre-commit home writes create unrecoverable leaks.
        for write in &transaction.realtime_writes {
            if write.fs_block >= self.superblock.data_blocks
                || write.before.len() != self.superblock.block_size as usize
                || write.after.len() != write.before.len()
                || self.read_data_fs_block(write.fs_block)? != write.before
            {
                return Err(XfsError::CorruptMetadata);
            }
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Realtime,
                basic_block: write
                    .fs_block
                    .checked_mul(u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
                    .ok_or(XfsError::AddressOutOfRange)?,
                before: write.before.clone(),
                after: write.after.clone(),
            });
        }
        let buffers = staged.composed_buffers()?;
        if buffers.is_empty() && staged.dquots.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        // Remote symlink bodies are regular data-fork blocks.  Their FUA
        // writes must finish before the log can make the mapping and name
        // durable.  Validate the entire staged set before touching either
        // data or log, so a malformed second block cannot publish a prefix.
        let mut data_writes = transaction.data_writes.clone();
        data_writes.sort_unstable_by_key(|write| write.fs_block);
        for (index, write) in data_writes.iter().enumerate() {
            if write.fs_block >= self.superblock.data_blocks
                || write.before.len() != self.superblock.block_size as usize
                || write.after.len() != write.before.len()
                || index != 0 && data_writes[index - 1].fs_block == write.fs_block
                || self.read_data_fs_block(write.fs_block)? != write.before
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        for write in &data_writes {
            self.write_data_fs_block(write.fs_block, &write.after)?;
        }
        if !data_writes.is_empty() {
            self.data.flush().map_err(XfsError::from)?;
        }
        let mut items = Vec::new();
        items
            .try_reserve_exact(buffers.len())
            .map_err(|_| XfsError::NoMemory)?;
        for buffer in &buffers {
            items.push(buffer.to_log_item()?);
        }
        // The reservation LSN is the current ring head and is independent of
        // the record contents.  DQUOT images must carry that exact LSN/CRC
        // *inside* their native typed item; they cannot be represented as a
        // generic BUF item because a 136-byte record may begin mid-sector.
        let dquot_lsn = ring.next_lsn();
        let mut dquot_regions = Vec::<Vec<Vec<u8>>>::new();
        dquot_regions
            .try_reserve_exact(staged.dquots.len())
            .map_err(|_| XfsError::NoMemory)?;
        let bigtime = self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0;
        for delta in &staged.dquots {
            dquot_regions.push(delta.encode_log_regions(
                dquot_lsn,
                self.superblock.meta_uuid,
                bigtime,
                XfsLogByteOrder::Little,
            )?);
        }
        let mut operations = Vec::new();
        operations
            .try_reserve(
                1 + items
                    .len()
                    .checked_mul(2)
                    .and_then(|count| count.checked_add(dquot_regions.len().checked_mul(2)?))
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        let item_count = items
            .len()
            .checked_add(dquot_regions.len())
            .ok_or(XfsError::AddressOutOfRange)?;
        let header = XfsTransactionHeader {
            transaction_type: XfsTransactionHeader::CHECKPOINT,
            item_count: u32::try_from(item_count).map_err(|_| XfsError::AddressOutOfRange)?,
        };
        operations.push(XfsLogOperation {
            transaction_id,
            client_id: 0x69,
            flags: XLOG_START_TRANS,
            payload: header
                .encode(transaction_id, XfsLogByteOrder::Little)?
                .to_vec(),
        });
        for item in &items {
            for region in item.encode_log_regions(XfsLogByteOrder::Little)? {
                operations.push(XfsLogOperation {
                    transaction_id,
                    client_id: 0x69,
                    flags: 0,
                    payload: region,
                });
            }
        }
        for regions in &dquot_regions {
            for region in regions {
                operations.push(XfsLogOperation {
                    transaction_id,
                    client_id: 0x69,
                    flags: 0,
                    payload: region.clone(),
                });
            }
        }
        let last = operations.last_mut().ok_or(XfsError::CorruptMetadata)?;
        last.flags |= XLOG_COMMIT_TRANS;
        let prepared =
            self.prepare_live_log_commit(ring, transaction_id, ring.tail_lsn(), &operations)?;
        self.persist_live_log_commit(&prepared, ail)?;
        let lsn = prepared.reservation.lsn;
        if lsn != dquot_lsn {
            return Err(XfsError::CorruptMetadata);
        }
        let mut homes: Vec<(u64, Vec<u8>)> = Vec::new();
        homes
            .try_reserve_exact(items.len())
            .map_err(|_| XfsError::NoMemory)?;
        for (item, buffer) in items.iter().zip(&buffers) {
            if let Some((_, image)) = homes
                .iter_mut()
                .find(|(block, _)| *block == item.block_number)
            {
                *image =
                    item.materialize_home_image(image, lsn, self.superblock.inode_size as usize)?;
            } else {
                homes.push((
                    item.block_number,
                    item.materialize_home_image(
                        &buffer.before,
                        lsn,
                        self.superblock.inode_size as usize,
                    )?,
                ));
            }
        }
        for delta in &staged.dquots {
            let item = delta.log_item(lsn, self.superblock.meta_uuid, bigtime)?;
            let bytes = usize::try_from(item.block_count)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut before = vec![0; bytes];
            self.read_basic_blocks(&self.data, item.block_number, &mut before)?;
            let offset = item.byte_offset as usize;
            if before[offset..offset + 136] != delta.before
                && !before[offset..offset + 136].iter().all(|byte| *byte == 0)
            {
                return Err(XfsError::CorruptMetadata);
            }
            let mut image = before.clone();
            let home_record = item.materialize_home_dquot(
                &delta.before,
                lsn,
                true,
                Some(self.superblock.meta_uuid),
                bigtime,
            )?;
            image[offset..offset + 136].copy_from_slice(&home_record);
            if let Some((_, existing)) = homes.iter_mut().find(|(block, existing)| {
                *block == item.block_number && existing.len() == image.len()
            }) {
                for index in 0..image.len() {
                    if existing[index] != before[index]
                        && image[index] != before[index]
                        && existing[index] != image[index]
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    if image[index] != before[index] {
                        existing[index] = image[index];
                    }
                }
            } else {
                homes.push((item.block_number, image));
            }
        }
        // Retain exact post-LSN home images in the AIL before attempting the
        // first home write.  An I/O failure leaves this immutable checkpoint
        // payload and its ring reservation available for the next ordered
        // push (or for crash replay), rather than re-staging a new mutation.
        homes.sort_unstable_by_key(|(block, _)| *block);
        ail.attach_checkpoint_homes(lsn, homes)?;
        self.checkpoint_live_log(ring, ail, lsn, |entry| {
            if entry.checkpoint_homes.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            for (block, image) in &entry.checkpoint_homes {
                self.write_basic_blocks_fua(&self.data, *block, image)?;
            }
            Ok(())
        })?;
        Ok(lsn)
    }

    /// Rebuilds all four allocator trees from one checked AG snapshot.  This
    /// is intentionally narrower than the general allocators: it is the
    /// creation primitive for a long symlink, whose inode bit and remote
    /// data extent must share one AGFL ownership pool and exactly one AGF,
    /// AGI, and AGFL image.
    fn stage_combined_inode_and_free_space_trees(
        &self,
        snapshot: &XfsAgOwnershipSnapshot,
        mut records: Vec<XfsAgInodeRecord>,
        mut extents: Vec<XfsAgFreeRecord>,
    ) -> XfsResult<XfsMetadataTransaction> {
        let ag = snapshot.ag;
        records.sort_unstable_by_key(|record| record.start_inode);
        extents.sort_unstable_by_key(|extent| extent.start_block);
        if records
            .iter()
            .any(|record| record.free_count != record.free_mask.count_ones())
            || records.windows(2).any(|pair| {
                pair[0]
                    .start_inode
                    .checked_add(64)
                    .map_or(true, |end| end > pair[1].start_inode)
            })
            || extents.iter().any(|extent| {
                extent.block_count == 0
                    || extent.start_block < 4
                    || extent
                        .start_block
                        .checked_add(extent.block_count)
                        .map_or(true, |end| end > self.superblock.ag_blocks)
            })
            || extents.windows(2).any(|pair| {
                pair[0]
                    .start_block
                    .checked_add(pair[0].block_count)
                    .map_or(true, |end| end >= pair[1].start_block)
            })
        {
            return Err(XfsError::CorruptMetadata);
        }
        let group = snapshot.group;
        let freelist = &snapshot.freelist;
        // A single pool is the key invariant.  All old btree homes and AGFL
        // entries are exclusive allocator scratch; each builder consumes a
        // prefix and the unused suffix becomes the sole final AGFL.
        let mut pool = Vec::new();
        for node in snapshot
            .ino_nodes
            .iter()
            .chain(snapshot.fino_nodes.iter())
            .chain(snapshot.bno_nodes.iter())
            .chain(snapshot.cnt_nodes.iter())
        {
            if !pool.contains(&node.block) {
                pool.push(node.block);
            }
        }
        for block in &freelist.entries {
            if !pool.contains(block) {
                pool.push(*block);
            }
        }
        let (inobt, inobt_used) =
            build_inode_tree(XfsAgBtreeKind::Inode, ag, self.superblock, &records, &pool)?;
        let finite = records
            .iter()
            .copied()
            .filter(|record| record.free_count != 0)
            .collect::<Vec<_>>();
        let (finobt, fino_used) = if !self.superblock.is_v5() || finite.is_empty() {
            (Vec::new(), 0usize)
        } else {
            build_inode_tree(
                XfsAgBtreeKind::FreeInode,
                ag,
                self.superblock,
                &finite,
                &pool[inobt_used..],
            )?
        };
        let inode_used = inobt_used
            .checked_add(fino_used)
            .ok_or(XfsError::AddressOutOfRange)?;
        let (bno, bno_used) = build_free_tree(
            XfsAgBtreeKind::ByBlock,
            ag,
            self.superblock,
            &extents,
            &pool[inode_used..],
        )?;
        let free_used = inode_used
            .checked_add(bno_used)
            .ok_or(XfsError::AddressOutOfRange)?;
        let (cnt, cnt_used) = build_free_tree(
            XfsAgBtreeKind::ByLength,
            ag,
            self.superblock,
            &extents,
            &pool[free_used..],
        )?;
        let used = free_used
            .checked_add(cnt_used)
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_freelist = pool[used..].to_vec();
        let capacity = (self.superblock.sector_size as usize)
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if new_freelist.len() > capacity {
            return Err(XfsError::AddressOutOfRange);
        }
        let first = 0u32;
        let last = if new_freelist.is_empty() {
            0
        } else {
            u32::try_from(new_freelist.len() - 1).map_err(|_| XfsError::AddressOutOfRange)?
        };
        let mut agi = group.inode;
        agi.inode_btree_root = inobt.last().ok_or(XfsError::CorruptMetadata)?.block;
        agi.inode_btree_level = inobt.last().ok_or(XfsError::CorruptMetadata)?.level as u32;
        agi.free_inode_count = u32::try_from(records.iter().try_fold(0u64, |sum, record| {
            sum.checked_add(record.free_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?)
        .map_err(|_| XfsError::AddressOutOfRange)?;
        if self.superblock.is_v5() {
            agi.free_inode_btree_root = finobt.last().map(|node| node.block);
            agi.free_inode_btree_level = finobt.last().map(|node| node.level as u32);
        }
        let mut agf = group.free_space;
        agf.bno_root = bno.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.cnt_root = cnt.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.free_blocks = u32::try_from(extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?)
        .map_err(|_| XfsError::AddressOutOfRange)?;
        agf.longest_free_extent = extents
            .iter()
            .map(|extent| extent.block_count)
            .max()
            .unwrap_or(0);
        agf.freelist_first = first;
        agf.freelist_last = last;
        agf.freelist_count =
            u32::try_from(new_freelist.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let mut buffers = Vec::new();
        for node in inobt
            .iter()
            .chain(finobt.iter())
            .chain(bno.iter())
            .chain(cnt.iter())
        {
            let fs_block = (ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(node.block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(fs_block)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: fs_block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after,
                });
            }
        }
        let ag_base = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let sector = self.superblock.sector_size as usize;
        let before_agf = self.read_data_fs_block(ag_base)?;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agf,
            basic_block: ag_base
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before: before_agf[..sector].to_vec(),
            after: agf.serialize(self.superblock, 0)?,
        });
        let agi_byte = sector;
        let agi_block = ag_base
            .checked_add((agi_byte / self.superblock.block_size as usize) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agi_offset = agi_byte % self.superblock.block_size as usize;
        if agi_offset + sector > self.superblock.block_size as usize {
            return Err(XfsError::UnsupportedFeature);
        }
        let before_agi = self.read_data_fs_block(agi_block)?;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agi,
            basic_block: agi_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .and_then(|base| base.checked_add((agi_offset / 512) as u64))
                .ok_or(XfsError::AddressOutOfRange)?,
            before: before_agi[agi_offset..agi_offset + sector].to_vec(),
            after: agi.serialize(self.superblock, 0)?,
        });
        if self.superblock.is_v5() {
            let byte_offset = 3usize
                .checked_mul(sector)
                .ok_or(XfsError::AddressOutOfRange)?;
            let block = ag_base
                .checked_add((byte_offset / self.superblock.block_size as usize) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let within = byte_offset % self.superblock.block_size as usize;
            if within + sector > self.superblock.block_size as usize {
                return Err(XfsError::UnsupportedFeature);
            }
            let before = self.read_data_fs_block(block)?;
            let header = XfsAgfl {
                sequence: ag,
                uuid: self.superblock.uuid,
            };
            buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Agfl,
                basic_block: block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .and_then(|base| base.checked_add((within / 512) as u64))
                    .ok_or(XfsError::AddressOutOfRange)?,
                before: before[within..within + sector].to_vec(),
                after: header.serialize(self.superblock, 0, &new_freelist, first, last)?,
            });
        }
        Ok(XfsMetadataTransaction {
            buffers,
            data_writes: Vec::new(),
            realtime_writes: Vec::new(),
            dquots: Vec::new(),
        })
    }

    fn stage_inode_trees(
        &self,
        ag: u32,
        mut records: Vec<XfsAgInodeRecord>,
    ) -> XfsResult<XfsMetadataTransaction> {
        records.sort_unstable_by_key(|record| record.start_inode);
        if records
            .iter()
            .any(|record| record.free_count != record.free_mask.count_ones())
            || records
                .windows(2)
                .any(|pair| match pair[0].start_inode.checked_add(64) {
                    Some(end) => end > pair[1].start_inode,
                    None => true,
                })
        {
            return Err(XfsError::CorruptMetadata);
        }
        let group = self.allocation_group(ag)?;
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let freelist = self.ag_freelist(ag)?;
        let mut pool = Vec::new();
        for node in snapshot.ino_nodes.iter().chain(snapshot.fino_nodes.iter()) {
            if !pool.contains(&node.block) {
                pool.push(node.block);
            }
        }
        for block in &freelist.entries {
            if !pool.contains(block) {
                pool.push(*block);
            }
        }
        let (inobt, inobt_used) =
            build_inode_tree(XfsAgBtreeKind::Inode, ag, self.superblock, &records, &pool)?;
        let finite = records
            .iter()
            .copied()
            .filter(|record| record.free_count != 0)
            .collect::<Vec<_>>();
        let (finobt, fino_used) = if !self.superblock.is_v5() || finite.is_empty() {
            (Vec::new(), 0usize)
        } else {
            build_inode_tree(
                XfsAgBtreeKind::FreeInode,
                ag,
                self.superblock,
                &finite,
                &pool[inobt_used..],
            )?
        };
        let used = inobt_used
            .checked_add(fino_used)
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_freelist = pool[used..].to_vec();
        let capacity = (self.superblock.sector_size as usize)
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if new_freelist.len() > capacity {
            return Err(XfsError::AddressOutOfRange);
        }
        let first = 0u32;
        let last = if new_freelist.is_empty() {
            0
        } else {
            u32::try_from(new_freelist.len() - 1).map_err(|_| XfsError::AddressOutOfRange)?
        };
        let mut agi = group.inode;
        agi.inode_btree_root = inobt.last().ok_or(XfsError::CorruptMetadata)?.block;
        agi.inode_btree_level = inobt.last().ok_or(XfsError::CorruptMetadata)?.level as u32;
        agi.free_inode_count = records
            .iter()
            .try_fold(0u64, |sum, record| {
                sum.checked_add(record.free_count as u64)
                    .ok_or(XfsError::AddressOutOfRange)
            })
            .and_then(|count| u32::try_from(count).map_err(|_| XfsError::AddressOutOfRange))?;
        if self.superblock.is_v5() {
            agi.free_inode_btree_root = finobt.last().map(|node| node.block);
            agi.free_inode_btree_level = finobt.last().map(|node| node.level as u32);
        }
        let mut buffers = Vec::new();
        for node in inobt.iter().chain(finobt.iter()) {
            let fs_block = (ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(node.block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(fs_block)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: fs_block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after,
                });
            }
        }
        let ag_base = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agi_byte = self.superblock.sector_size as usize;
        let agi_block = ag_base
            .checked_add((agi_byte / self.superblock.block_size as usize) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let agi_offset = agi_byte % self.superblock.block_size as usize;
        if agi_offset + self.superblock.sector_size as usize > self.superblock.block_size as usize {
            return Err(XfsError::UnsupportedFeature);
        }
        let before_agi = self.read_data_fs_block(agi_block)?;
        let mut after_agi = before_agi.clone();
        after_agi[agi_offset..agi_offset + self.superblock.sector_size as usize]
            .copy_from_slice(&agi.serialize(self.superblock, 0)?);
        let sector = self.superblock.sector_size as usize;
        let basic = agi_block
            .checked_mul((self.superblock.block_size as u64) / 512)
            .and_then(|base| base.checked_add((agi_offset / 512) as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agi,
            basic_block: basic,
            before: before_agi[agi_offset..agi_offset + sector].to_vec(),
            after: after_agi[agi_offset..agi_offset + sector].to_vec(),
        });
        if self.superblock.is_v5() {
            let agfl_byte = 3usize
                .checked_mul(self.superblock.sector_size as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
            let agfl_block = ag_base
                .checked_add((agfl_byte / self.superblock.block_size as usize) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let agfl_offset = agfl_byte % self.superblock.block_size as usize;
            if agfl_offset + self.superblock.sector_size as usize
                > self.superblock.block_size as usize
            {
                return Err(XfsError::UnsupportedFeature);
            }
            let before = self.read_data_fs_block(agfl_block)?;
            let mut after = before.clone();
            let header = XfsAgfl {
                sequence: ag,
                uuid: self.superblock.uuid,
            };
            after[agfl_offset..agfl_offset + self.superblock.sector_size as usize].copy_from_slice(
                &header.serialize(self.superblock, 0, &new_freelist, first, last)?,
            );
            let sector = self.superblock.sector_size as usize;
            let basic = agfl_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .and_then(|base| base.checked_add((agfl_offset / 512) as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Agfl,
                basic_block: basic,
                before: before[agfl_offset..agfl_offset + sector].to_vec(),
                after: after[agfl_offset..agfl_offset + sector].to_vec(),
            });
        }
        Ok(XfsMetadataTransaction {
            buffers,
            data_writes: Vec::new(),
            realtime_writes: Vec::new(),
            dquots: Vec::new(),
        })
    }

    /// Applies an allocation and any bmap-node returns to one verified AG
    /// snapshot before rebuilding either free-space index.  This is the
    /// transaction-local counterpart of allocate-then-free; it never emits
    /// two stale AGF images for the same home block.
    /// Reserves exact one-block bmap buffer slots from the preferred AG first
    /// and then wraps across the remaining AGs.  The caller supplies already
    /// selected data ranges, so metadata can never alias a newly allocated
    /// file block in the same transaction.  No on-disk allocation state is
    /// changed here; `stage_extent_delta` publishes every reservation and
    /// return together with the inode/bmap images.
    fn reserve_bmap_metadata_blocks(
        &self,
        preferred_ag: u32,
        count: usize,
        exclusions: &[(u32, u32, u32)],
    ) -> XfsResult<Vec<u64>> {
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        if count == 0 {
            return Ok(selected);
        }
        if preferred_ag >= self.superblock.ag_count || self.superblock.ag_count == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        for step in 0..self.superblock.ag_count {
            let ag = u32::try_from(
                (preferred_ag as u64 + step as u64) % self.superblock.ag_count as u64,
            )
            .map_err(|_| XfsError::AddressOutOfRange)?;
            let mut extents = self.ag_ownership_snapshot(ag)?.free_extents;
            for (excluded_ag, start, blocks) in exclusions {
                if *excluded_ag != ag {
                    continue;
                }
                let end = start
                    .checked_add(*blocks)
                    .ok_or(XfsError::AddressOutOfRange)?;
                if extents
                    .iter()
                    .any(|extent| extent.start_block.checked_add(extent.block_count).is_none())
                {
                    return Err(XfsError::CorruptMetadata);
                }
                let index = extents
                    .iter()
                    .position(|extent| {
                        *start >= extent.start_block
                            && end <= extent.start_block + extent.block_count
                    })
                    .ok_or(XfsError::CorruptMetadata)?;
                let source = extents.remove(index);
                if source.start_block < *start {
                    extents.push(XfsAgFreeRecord {
                        start_block: source.start_block,
                        block_count: *start - source.start_block,
                    });
                }
                if end < source.start_block + source.block_count {
                    extents.push(XfsAgFreeRecord {
                        start_block: end,
                        block_count: source.start_block + source.block_count - end,
                    });
                }
            }
            extents.sort_unstable_by_key(|extent| extent.start_block);
            for extent in extents {
                for relative in extent.start_block
                    ..extent
                        .start_block
                        .checked_add(extent.block_count)
                        .ok_or(XfsError::AddressOutOfRange)?
                {
                    selected.push(
                        (ag as u64)
                            .checked_mul(self.superblock.ag_blocks as u64)
                            .and_then(|base| base.checked_add(relative as u64))
                            .ok_or(XfsError::AddressOutOfRange)?,
                    );
                    if selected.len() == count {
                        return Ok(selected);
                    }
                }
            }
        }
        Err(XfsError::AddressOutOfRange)
    }

    fn stage_extent_delta(
        &self,
        ag: u32,
        allocations: &[(u32, u32)],
        releases: &[u32],
    ) -> XfsResult<XfsMetadataTransaction> {
        self.stage_extent_delta_with_metadata(ag, allocations, releases, releases)
    }

    /// Rebuilds an AG free-space image while distinguishing ordinary returned
    /// data blocks from blocks that may be borrowed as AG-tree scratch space.
    /// Attribute remote values are data, not AGFL entries.
    fn stage_extent_delta_with_metadata(
        &self,
        ag: u32,
        allocations: &[(u32, u32)],
        releases: &[u32],
        released_metadata: &[u32],
    ) -> XfsResult<XfsMetadataTransaction> {
        let mut extents = self.ag_ownership_snapshot(ag)?.free_extents;
        for (start, count) in allocations {
            if *count == 0 {
                return Err(XfsError::AddressOutOfRange);
            }
            let end = start
                .checked_add(*count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if extents
                .iter()
                .any(|extent| extent.start_block.checked_add(extent.block_count).is_none())
            {
                return Err(XfsError::CorruptMetadata);
            }
            let index = extents
                .iter()
                .position(|extent| {
                    *start >= extent.start_block && end <= extent.start_block + extent.block_count
                })
                .ok_or(XfsError::CorruptMetadata)?;
            let source = extents.remove(index);
            if source.start_block < *start {
                extents.push(XfsAgFreeRecord {
                    start_block: source.start_block,
                    block_count: *start - source.start_block,
                });
            }
            if end < source.start_block + source.block_count {
                extents.push(XfsAgFreeRecord {
                    start_block: end,
                    block_count: source.start_block + source.block_count - end,
                });
            }
        }
        for block in releases {
            if *block < 4 || *block >= self.superblock.ag_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            extents.push(XfsAgFreeRecord {
                start_block: *block,
                block_count: 1,
            });
        }
        extents.sort_unstable_by_key(|extent| extent.start_block);
        let mut coalesced: Vec<XfsAgFreeRecord> = Vec::new();
        for extent in extents {
            if let Some(last) = coalesced.last_mut()
                && last.start_block + last.block_count == extent.start_block
            {
                last.block_count = last
                    .block_count
                    .checked_add(extent.block_count)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                coalesced.push(extent);
            }
        }
        self.stage_free_space_trees(ag, coalesced, released_metadata)
    }

    fn stage_free_space_trees(
        &self,
        ag: u32,
        mut extents: Vec<XfsAgFreeRecord>,
        released_metadata: &[u32],
    ) -> XfsResult<XfsMetadataTransaction> {
        extents.sort_unstable_by_key(|extent| extent.start_block);
        if extents.iter().any(|extent| {
            extent.block_count == 0
                || extent.start_block < 4
                || match extent.start_block.checked_add(extent.block_count) {
                    Some(end) => end > self.superblock.ag_blocks,
                    None => true,
                }
        }) || extents.windows(2).any(|pair| {
            match pair[0].start_block.checked_add(pair[0].block_count) {
                Some(end) => end >= pair[1].start_block,
                None => true,
            }
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        let group = self.allocation_group(ag)?;
        let freelist = self.ag_freelist(ag)?;
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut pool = Vec::new();
        for node in snapshot.bno_nodes.iter().chain(snapshot.cnt_nodes.iter()) {
            if !pool.contains(&node.block) {
                pool.push(node.block);
            }
        }
        for block in &freelist.entries {
            if !pool.contains(block) {
                pool.push(*block);
            }
        }
        // A returned BMBT block may be the only immediately available home
        // for an AG-tree split.  Remove it from the new free records before
        // lending it to the tree builder; unused candidates are retained in
        // the rebuilt AGFL, never advertised simultaneously as free space.
        for block in released_metadata {
            if pool.contains(block) {
                continue;
            }
            let index = extents
                .iter()
                .position(|extent| {
                    *block >= extent.start_block && *block < extent.start_block + extent.block_count
                })
                .ok_or(XfsError::CorruptMetadata)?;
            let source = extents.remove(index);
            if source.start_block < *block {
                extents.push(XfsAgFreeRecord {
                    start_block: source.start_block,
                    block_count: *block - source.start_block,
                });
            }
            let after = block.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            let source_end = source
                .start_block
                .checked_add(source.block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if after < source_end {
                extents.push(XfsAgFreeRecord {
                    start_block: after,
                    block_count: source_end - after,
                });
            }
            pool.push(*block);
        }
        extents.sort_unstable_by_key(|extent| extent.start_block);
        let (bno, bno_used) = build_free_tree(
            XfsAgBtreeKind::ByBlock,
            ag,
            self.superblock,
            &extents,
            &pool,
        )?;
        let (cnt, cnt_used) = build_free_tree(
            XfsAgBtreeKind::ByLength,
            ag,
            self.superblock,
            &extents,
            &pool[bno_used..],
        )?;
        let used = bno_used
            .checked_add(cnt_used)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut new_freelist = pool[used..].to_vec();
        let capacity = (self.superblock.sector_size as usize)
            .checked_sub(36)
            .ok_or(XfsError::CorruptMetadata)?
            / 4;
        if new_freelist.len() > capacity {
            return Err(XfsError::AddressOutOfRange);
        }
        // Existing AGFL entries are copied after the currently active slots;
        // a single rebuilt ring makes pop/push a transaction-local operation.
        let first = if new_freelist.is_empty() { 0 } else { 0 };
        let last = if new_freelist.is_empty() {
            0
        } else {
            u32::try_from(new_freelist.len() - 1).map_err(|_| XfsError::AddressOutOfRange)?
        };
        let mut agf = group.free_space;
        agf.bno_root = bno.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.cnt_root = cnt.last().ok_or(XfsError::CorruptMetadata)?.block;
        agf.free_blocks = u32::try_from(extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?)
        .map_err(|_| XfsError::AddressOutOfRange)?;
        agf.longest_free_extent = extents
            .iter()
            .map(|extent| extent.block_count)
            .max()
            .unwrap_or(0);
        agf.freelist_first = first;
        agf.freelist_last = last;
        agf.freelist_count =
            u32::try_from(new_freelist.len()).map_err(|_| XfsError::AddressOutOfRange)?;
        let mut buffers = Vec::new();
        for node in bno.iter().chain(cnt.iter()) {
            let fs_block = (ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(node.block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(fs_block)?;
            let after = node.serialize(self.superblock, 0)?;
            if before != after {
                buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: fs_block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after,
                });
            }
        }
        let ag_base = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let ag_base_basic = ag_base
            .checked_mul((self.superblock.block_size as u64) / 512)
            .ok_or(XfsError::AddressOutOfRange)?;
        let before_agf = self.read_data_fs_block(ag_base)?;
        let mut after_agf = before_agf.clone();
        after_agf[..self.superblock.sector_size as usize]
            .copy_from_slice(&agf.serialize(self.superblock, 0)?);
        let sector = self.superblock.sector_size as usize;
        buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Agf,
            basic_block: ag_base_basic,
            before: before_agf[..sector].to_vec(),
            after: after_agf[..sector].to_vec(),
        });
        if self.superblock.is_v5() {
            let byte_offset = 3usize
                .checked_mul(self.superblock.sector_size as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
            let header_block = ag_base
                .checked_add((byte_offset / self.superblock.block_size as usize) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let within = byte_offset % self.superblock.block_size as usize;
            if within + self.superblock.sector_size as usize > self.superblock.block_size as usize {
                return Err(XfsError::UnsupportedFeature);
            }
            let before = self.read_data_fs_block(header_block)?;
            let mut after = before.clone();
            let header = XfsAgfl {
                sequence: ag,
                uuid: self.superblock.uuid,
            };
            after[within..within + self.superblock.sector_size as usize].copy_from_slice(
                &header.serialize(self.superblock, 0, &new_freelist, first, last)?,
            );
            let sector = self.superblock.sector_size as usize;
            let basic = header_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .and_then(|base| base.checked_add((within / 512) as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Agfl,
                basic_block: basic,
                before: before[within..within + sector].to_vec(),
                after: after[within..within + sector].to_vec(),
            });
        }
        new_freelist.clear();
        Ok(XfsMetadataTransaction {
            buffers,
            data_writes: Vec::new(),
            realtime_writes: Vec::new(),
            dquots: Vec::new(),
        })
    }

    /// Decodes an inode by its stable 64-bit XFS number.  The AG and inode
    /// block calculation is checked before every multiplication/addition.
    pub fn inode(&self, number: u64) -> XfsResult<XfsInode> {
        let (ag, agino) = self.split_inode_number(number)?;
        let inode_block = agino >> self.superblock.inodes_per_block_log;
        let inode_index = agino & (self.superblock.inodes_per_block as u64 - 1);
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|start| start.checked_add(inode_block))
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.read_data_fs_block(fs_block)?;
        let offset = (inode_index as usize)
            .checked_mul(self.superblock.inode_size as usize)
            .ok_or(XfsError::AddressOutOfRange)?;
        XfsInode::parse(
            number,
            slice(&block, offset, self.superblock.inode_size as usize)?,
            self.superblock.is_v5().then_some(self.superblock.uuid),
            (self.superblock.features.incompat & XfsFeatures::INCOMPAT_META_UUID != 0)
                .then_some(self.superblock.meta_uuid),
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0,
        )
    }

    /// Returns extents stored directly in the inode data fork.  Btree forks
    /// are intentionally not coerced into an incomplete answer: they must be
    /// traversed through the verified BMBT reader before the VFS can use them.
    pub fn inode_data_extents(&self, number: u64) -> XfsResult<Vec<XfsExtent>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.data_format != XfsForkFormat::Extents {
            return Err(XfsError::UnsupportedFeature);
        }
        self.decode_extent_fork(
            inode.data_fork(&raw)?,
            usize::try_from(inode.data_extents).map_err(|_| XfsError::CorruptMetadata)?,
        )
    }

    /// Counts filesystem blocks owned by one fork's extent mappings.  BMBT
    /// node homes are deliberately supplied separately by the caller: they
    /// belong to the fork too, but are not represented by a data extent.
    fn inode_extent_blocks(extents: &[XfsExtent]) -> XfsResult<u64> {
        extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(u64::from(extent.block_count))
                .ok_or(XfsError::AddressOutOfRange)
        })
    }

    /// Counts the currently installed attribute-fork allocation in
    /// filesystem blocks, including external BMBT nodes.  Data-fork writers
    /// use this when replacing their own mapping so `di_nblocks` retains the
    /// independent xattr ownership exactly once.
    fn attribute_fork_owned_blocks(&self, number: u64, inode: &XfsInode) -> XfsResult<u64> {
        match inode.attr_format {
            XfsForkFormat::Local => Ok(0),
            XfsForkFormat::Extents => Self::inode_extent_blocks(&self.inode_attr_extents(number)?),
            XfsForkFormat::Btree => {
                let mappings = Self::inode_extent_blocks(&self.inode_attr_extents(number)?)?;
                let bmap_nodes = u64::try_from(self.attr_bmbt_blocks(number)?.len())
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                mappings
                    .checked_add(bmap_nodes)
                    .ok_or(XfsError::AddressOutOfRange)
            }
            XfsForkFormat::Device | XfsForkFormat::Uuid => Err(XfsError::CorruptMetadata),
        }
    }

    /// Returns attribute-fork extent mappings when they are local to the
    /// inode.  Attribute btrees deliberately remain non-mountable until their
    /// node verifier is complete.
    pub fn inode_attr_extents(&self, number: u64) -> XfsResult<Vec<XfsExtent>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        match inode.attr_format {
            XfsForkFormat::Extents => self.decode_extent_fork(
                inode.attr_fork(&raw)?,
                usize::try_from(inode.attr_extents).map_err(|_| XfsError::CorruptMetadata)?,
            ),
            XfsForkFormat::Btree => self.attr_bmbt_extents(number, inode.attr_fork(&raw)?),
            _ => Err(XfsError::UnsupportedFeature),
        }
    }

    fn attr_bmbt_extents(&self, number: u64, fork: &[u8]) -> XfsResult<Vec<XfsExtent>> {
        if fork.len() < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(fork, 0)?;
        let records = be16(fork, 2)? as usize;
        if records == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        if level == 0 {
            return self.decode_extent_fork(
                slice(
                    fork,
                    4,
                    records.checked_mul(16).ok_or(XfsError::CorruptMetadata)?,
                )?,
                records,
            );
        }
        let capacity = (fork.len() - 4) / 16;
        if records > capacity {
            return Err(XfsError::CorruptMetadata);
        }
        let pointer_base = 4usize
            .checked_add(capacity.checked_mul(8).ok_or(XfsError::CorruptMetadata)?)
            .ok_or(XfsError::CorruptMetadata)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(records)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..records {
            let child = be64(fork, pointer_base + index * 8)?;
            if child == 0 || child >= self.superblock.data_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            pending.push((child, level - 1));
        }
        let mut visited = Vec::new();
        let mut extents = Vec::new();
        while let Some((block, expected)) = pending.pop() {
            if visited.contains(&block) {
                return Err(XfsError::CorruptMetadata);
            }
            visited.push(block);
            let node = self.bmbt_node(block, number)?;
            if node.level != expected {
                return Err(XfsError::CorruptMetadata);
            }
            if node.level == 0 {
                extents.extend(node.leaf_extents);
            } else {
                for child in node.children {
                    pending.push((child, node.level - 1));
                }
            }
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let mut previous = 0u64;
        for (index, extent) in extents.iter().enumerate() {
            if extent.block_count == 0 || (index != 0 && extent.file_block < previous) {
                return Err(XfsError::CorruptMetadata);
            }
            previous = extent
                .file_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
        }
        Ok(extents)
    }

    /// Enumerates the external blocks owned by an attribute-fork BMBT.  Keep
    /// this separate from the data-fork walker: the inode-root lives in the
    /// attribute fork, but external nodes have the same owner/UUID checks.
    fn attr_bmbt_blocks(&self, number: u64) -> XfsResult<Vec<u64>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.attr_format != XfsForkFormat::Btree {
            return Ok(Vec::new());
        }
        let fork = inode.attr_fork(&raw)?;
        if fork.len() < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(fork, 0)?;
        let records = be16(fork, 2)? as usize;
        if level == 0 || records == 0 {
            return Ok(Vec::new());
        }
        let capacity = (fork.len() - 4) / 16;
        if records > capacity {
            return Err(XfsError::CorruptMetadata);
        }
        let pointer_base = 4usize
            .checked_add(capacity.checked_mul(8).ok_or(XfsError::CorruptMetadata)?)
            .ok_or(XfsError::CorruptMetadata)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(records)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..records {
            pending.push((be64(fork, pointer_base + index * 8)?, level - 1));
        }
        let mut blocks = Vec::new();
        while let Some((block, expected)) = pending.pop() {
            if block == 0 || block >= self.superblock.data_blocks || blocks.contains(&block) {
                return Err(XfsError::CorruptMetadata);
            }
            let node = self.bmbt_node(block, number)?;
            if node.level != expected {
                return Err(XfsError::CorruptMetadata);
            }
            blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            blocks.push(block);
            if node.level != 0 {
                for child in node.children {
                    pending.push((child, node.level - 1));
                }
            }
        }
        blocks.sort_unstable();
        Ok(blocks)
    }

    /// Decodes an inode-rooted BMBT enough to drive a checked block traversal.
    /// For a leaf root this returns its data extent records; for an internal
    /// root it returns the exact child filesystem block numbers.  No caller
    /// receives a partial mapping when a btree level remains unread.
    pub fn inode_bmbt_root(&self, number: u64) -> XfsResult<XfsBmbtRoot> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.data_format != XfsForkFormat::Btree {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork = inode.data_fork(&raw)?;
        if fork.len() < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(fork, 0)?;
        let records = be16(fork, 2)?;
        if records == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let record_count = records as usize;
        let key_bytes = record_count
            .checked_mul(if level == 0 { 16 } else { 8 })
            .ok_or(XfsError::CorruptMetadata)?;
        if fork.len() < 4 + key_bytes {
            return Err(XfsError::CorruptMetadata);
        }
        if level == 0 {
            return Ok(XfsBmbtRoot {
                level,
                records,
                leaf_extents: self.decode_extent_fork(&fork[4..], record_count)?,
                children: Vec::new(),
            });
        }
        // The root reserves a 16-byte key and an 8-byte pointer for every
        // possible record.  Derive capacity from the actual fork length
        // instead of trusting an on-disk count to locate pointers.
        let capacity = (fork.len() - 4) / 16;
        if capacity < record_count {
            return Err(XfsError::CorruptMetadata);
        }
        let pointer_base = 4usize
            .checked_add(capacity.checked_mul(8).ok_or(XfsError::CorruptMetadata)?)
            .ok_or(XfsError::CorruptMetadata)?;
        let mut children = Vec::new();
        children
            .try_reserve_exact(record_count)
            .map_err(|_| XfsError::NoMemory)?;
        let mut prior_key = None;
        for index in 0..record_count {
            let key = be64(fork, 4 + index * 8)?;
            if prior_key.is_some_and(|prior| key < prior) {
                return Err(XfsError::CorruptMetadata);
            }
            prior_key = Some(key);
            let child = be64(fork, pointer_base + index * 8)?;
            if child == 0 || child >= self.superblock.data_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            children.push(child);
        }
        Ok(XfsBmbtRoot {
            level,
            records,
            leaf_extents: Vec::new(),
            children,
        })
    }

    /// Reads one BMBT node.  The caller supplies an XFS filesystem block
    /// number, never a host byte offset; this preserves AG and multi-device
    /// address-space validation at the volume boundary.
    fn bmbt_node(&self, filesystem_block: u64, expected_inode: u64) -> XfsResult<XfsBmbtNode> {
        let block = self.read_data_fs_block(filesystem_block)?;
        let magic = be32(&block, 0)?;
        let (header_bytes, has_crc) = match magic {
            XFS_BMAP_MAGIC => (24usize, false),
            XFS_BMAP_CRC_MAGIC => (72usize, true),
            _ => return Err(XfsError::CorruptMetadata),
        };
        if block.len() < header_bytes {
            return Err(XfsError::CorruptMetadata);
        }
        let level = be16(&block, 4)?;
        let records = be16(&block, 6)?;
        if records == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let left_sibling = be64(&block, 8)?;
        let right_sibling = be64(&block, 16)?;
        if has_crc {
            verify_crc32c(&block, 64)?;
            let mut uuid = [0; 16];
            uuid.copy_from_slice(slice(&block, 40, 16)?);
            if XfsUuid(uuid) != self.superblock.meta_uuid {
                return Err(XfsError::CorruptMetadata);
            }
            if be64(&block, 24)? != filesystem_block {
                return Err(XfsError::CorruptMetadata);
            }
            if be64(&block, 56)? != expected_inode {
                return Err(XfsError::CorruptMetadata);
            }
        }
        let count = records as usize;
        if level == 0 {
            let bytes = count.checked_mul(16).ok_or(XfsError::CorruptMetadata)?;
            let extents = self.decode_extent_fork(slice(&block, header_bytes, bytes)?, count)?;
            return Ok(XfsBmbtNode {
                filesystem_block,
                level,
                records,
                left_sibling,
                right_sibling,
                leaf_extents: extents,
                children: Vec::new(),
            });
        }
        let capacity = block
            .len()
            .checked_sub(header_bytes)
            .ok_or(XfsError::CorruptMetadata)?
            / 16;
        if count > capacity {
            return Err(XfsError::CorruptMetadata);
        }
        let pointer_base = header_bytes
            .checked_add(capacity.checked_mul(8).ok_or(XfsError::CorruptMetadata)?)
            .ok_or(XfsError::CorruptMetadata)?;
        let key_bytes = count.checked_mul(8).ok_or(XfsError::CorruptMetadata)?;
        if header_bytes
            .checked_add(key_bytes)
            .ok_or(XfsError::CorruptMetadata)?
            > pointer_base
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut children = Vec::new();
        children
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        let mut prior_key = None;
        for index in 0..count {
            let key = be64(&block, header_bytes + index * 8)?;
            if prior_key.is_some_and(|prior| key < prior) {
                return Err(XfsError::CorruptMetadata);
            }
            prior_key = Some(key);
            let child = be64(&block, pointer_base + index * 8)?;
            if child == 0 || child >= self.superblock.data_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            children.push(child);
        }
        Ok(XfsBmbtNode {
            filesystem_block,
            level,
            records,
            left_sibling,
            right_sibling,
            leaf_extents: Vec::new(),
            children,
        })
    }

    /// Materializes all data-fork mappings from an inode-rooted BMBT.  Every
    /// child is level-checked and visited at most once; this rejects cycles,
    /// duplicate parentage, and a maliciously deep tree before it can become
    /// a VFS read mapping.
    pub fn inode_bmbt_extents(&self, number: u64) -> XfsResult<Vec<XfsExtent>> {
        let root = self.inode_bmbt_root(number)?;
        if root.level == 0 {
            return Ok(root.leaf_extents);
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(root.children.len())
            .map_err(|_| XfsError::NoMemory)?;
        for child in root.children {
            pending.push((child, root.level - 1));
        }
        let mut visited = Vec::<u64>::new();
        let mut extents = Vec::new();
        while let Some((block, expected_level)) = pending.pop() {
            if visited.iter().any(|visited_block| *visited_block == block) {
                return Err(XfsError::CorruptMetadata);
            }
            visited.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            visited.push(block);
            let node = self.bmbt_node(block, number)?;
            if node.level != expected_level {
                return Err(XfsError::CorruptMetadata);
            }
            if node.level == 0 {
                extents
                    .try_reserve(node.leaf_extents.len())
                    .map_err(|_| XfsError::NoMemory)?;
                extents.extend(node.leaf_extents);
            } else {
                let next_level = node.level - 1;
                pending
                    .try_reserve(node.children.len())
                    .map_err(|_| XfsError::NoMemory)?;
                for child in node.children {
                    pending.push((child, next_level));
                }
            }
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let mut previous_end = 0u64;
        for (index, extent) in extents.iter().enumerate() {
            if index != 0 && extent.file_block < previous_end {
                return Err(XfsError::CorruptMetadata);
            }
            previous_end = extent
                .file_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
        }
        Ok(extents)
    }

    /// Enumerates the exact external bmapbt ownership set for one inode.
    /// A node is returned once only after its parent level and physical block
    /// were verified, so reclaim code never frees an arbitrary extent merely
    /// because it resembles a bmap header.
    pub fn inode_bmbt_blocks(&self, number: u64) -> XfsResult<Vec<u64>> {
        let root = self.inode_bmbt_root(number)?;
        if root.level == 0 {
            return Ok(Vec::new());
        }
        let mut pending = root
            .children
            .into_iter()
            .map(|block| (block, root.level - 1))
            .collect::<Vec<_>>();
        let mut blocks = Vec::new();
        while let Some((block, level)) = pending.pop() {
            if blocks.contains(&block) {
                return Err(XfsError::CorruptMetadata);
            }
            let node = self.bmbt_node(block, number)?;
            if node.level != level {
                return Err(XfsError::CorruptMetadata);
            }
            blocks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            blocks.push(block);
            if node.level != 0 {
                for child in node.children {
                    pending.push((child, node.level - 1));
                }
            }
        }
        blocks.sort_unstable();
        Ok(blocks)
    }

    /// Produces the ownership-constrained local mutation envelope used by
    /// bmap insert/remove.  A root collapse reports its now-unreachable
    /// children for the caller's AG delta; root promotion reports the added
    /// node demand instead of silently rebuilding every existing leaf.
    pub fn plan_bmap_local_mutation(
        &self,
        number: u64,
        new_records: usize,
    ) -> XfsResult<XfsBmapLocalMutation> {
        let root = self.inode_bmbt_root(number)?;
        let inode = self.inode(number)?;
        let fork = self.inode_and_bytes(number)?.1;
        let fork_bytes = inode.data_fork(&fork)?.len();
        let old = self.inode_bmbt_blocks(number)?;
        let required = bmap_external_blocks(self.superblock, fork_bytes, new_records)?;
        let header = if self.superblock.is_v5() {
            72usize
        } else {
            24usize
        };
        let leaf_capacity = (self.superblock.block_size as usize - header) / 16;
        let interior_capacity = (self.superblock.block_size as usize - header) / 16;
        let root_capacity = (fork_bytes - 4) / 16;
        let mut level = 0u16;
        if required != 0 {
            let mut children = new_records.div_ceil(leaf_capacity);
            level = 1;
            while children > root_capacity {
                children = children.div_ceil(interior_capacity);
                level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        let mut changed = old
            .iter()
            .copied()
            .take(required.min(old.len()))
            .collect::<Vec<_>>();
        let reclaimed = if required < old.len() {
            old[required..].to_vec()
        } else {
            Vec::new()
        };
        if changed.is_empty() && required != 0 {
            changed
                .try_reserve(required)
                .map_err(|_| XfsError::NoMemory)?;
        }
        Ok(XfsBmapLocalMutation {
            inode: number,
            old_root_level: root.level,
            new_root_level: level,
            changed_blocks: changed,
            reclaimed_blocks: reclaimed,
        })
    }

    /// Rewrites only the external bmap leaf records intersecting a conversion
    /// from unwritten to written.  Logical start keys stay stable, so interior
    /// separators and root height need not move; a split conversion may add
    /// leaf records and therefore updates the inode extent count atomically
    /// with the changed leaves.
    pub fn stage_bmap_unwritten_conversion(
        &self,
        number: u64,
        start_file_block: u64,
        block_count: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<XfsBmapLocalMutation> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        let end = start_file_block
            .checked_add(block_count as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut changed = Vec::new();
        let mut extent_count = inode.data_extents;
        for block in self.inode_bmbt_blocks(number)? {
            let node = self.bmbt_node(block, number)?;
            if node.level != 0
                || node.leaf_extents.iter().all(|extent| {
                    end <= extent.file_block
                        || start_file_block >= extent.file_block + extent.block_count as u64
                })
            {
                continue;
            }
            let prior_records = node.leaf_extents.len() as u64;
            let mut records = Vec::new();
            for extent in node.leaf_extents {
                let extent_end = extent.file_block + extent.block_count as u64;
                if !extent.unwritten || end <= extent.file_block || start_file_block >= extent_end {
                    records.push(extent);
                    continue;
                }
                let middle_start = start_file_block.max(extent.file_block);
                let middle_end = end.min(extent_end);
                if extent.file_block < middle_start {
                    records.push(XfsExtent {
                        block_count: u32::try_from(middle_start - extent.file_block)
                            .map_err(|_| XfsError::AddressOutOfRange)?,
                        ..extent
                    });
                }
                records.push(XfsExtent {
                    unwritten: false,
                    file_block: middle_start,
                    start_block: extent.start_block + (middle_start - extent.file_block),
                    block_count: u32::try_from(middle_end - middle_start)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                });
                if middle_end < extent_end {
                    records.push(XfsExtent {
                        file_block: middle_end,
                        start_block: extent.start_block + (middle_end - extent.file_block),
                        block_count: u32::try_from(extent_end - middle_end)
                            .map_err(|_| XfsError::AddressOutOfRange)?,
                        ..extent
                    });
                }
            }
            let header = if self.superblock.is_v5() {
                72usize
            } else {
                24usize
            };
            if records.len() > (self.superblock.block_size as usize - header) / 16 {
                return Err(XfsError::AddressOutOfRange);
            }
            extent_count = extent_count
                .checked_sub(prior_records)
                .and_then(|count| count.checked_add(records.len() as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            let before = self.read_data_fs_block(block)?;
            let after = serialize_bmap_node(
                self.superblock,
                number,
                block,
                0,
                node.left_sibling,
                node.right_sibling,
                &records,
                &[],
            )?;
            transaction.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Btree,
                basic_block: block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?,
                before,
                after,
            });
            changed.push(block);
        }
        if !changed.is_empty() && extent_count != inode.data_extents {
            let mut after = raw.clone();
            if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
                put_be64(&mut after, 24, extent_count)?;
            } else {
                put_be32(
                    &mut after,
                    76,
                    u32::try_from(extent_count).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
            }
            self.stage_inode_image(number, raw, after, transaction)?;
        }
        let root = self.inode_bmbt_root(number)?;
        Ok(XfsBmapLocalMutation {
            inode: number,
            old_root_level: root.level,
            new_root_level: root.level,
            changed_blocks: changed,
            reclaimed_blocks: Vec::new(),
        })
    }

    /// Reads regular-file bytes through validated extent mappings.  Holes and
    /// unwritten extents are returned as zeroes; no implicit allocation or
    /// writeback is performed.  This is the read path used by a future VFS
    /// file node once directory lookup and permission checks are complete.
    pub fn read_inode_at(&self, number: u64, offset: u64, output: &mut [u8]) -> XfsResult<usize> {
        let inode = self.inode(number)?;
        if offset >= inode.size || output.is_empty() {
            return Ok(0);
        }
        let requested = cmp::min(output.len() as u64, inode.size - offset) as usize;
        if inode.data_format == XfsForkFormat::Local {
            let (_, raw) = self.inode_and_bytes(number)?;
            let payload = inode.data_fork(&raw)?;
            let local_offset = usize::try_from(offset).map_err(|_| XfsError::AddressOutOfRange)?;
            if local_offset > payload.len() || inode.size != payload.len() as u64 {
                return Err(XfsError::CorruptMetadata);
            }
            let available = cmp::min(requested, payload.len() - local_offset);
            output[..available].copy_from_slice(&payload[local_offset..local_offset + available]);
            return Ok(available);
        }
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let fs_block_size = self.superblock.block_size as u64;
        let mut done = 0usize;
        while done < requested {
            let position = offset + done as u64;
            let file_block = position / fs_block_size;
            let within_block = (position % fs_block_size) as usize;
            let chunk = cmp::min(
                requested - done,
                self.superblock.block_size as usize - within_block,
            );
            let mapping = extents.iter().find(|extent| {
                file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            });
            if let Some(extent) = mapping
                && !extent.unwritten
            {
                let physical_block = extent.start_block + (file_block - extent.file_block);
                let source = self.read_data_fs_block(physical_block)?;
                output[done..done + chunk]
                    .copy_from_slice(&source[within_block..within_block + chunk]);
            } else {
                output[done..done + chunk].fill(0);
            }
            done += chunk;
        }
        Ok(done)
    }

    /// Builds a stable export handle after reading the inode generation from
    /// disk.  A later resolver must compare that generation again before
    /// returning an object, which prevents stale-handle inode reuse.
    pub fn export_handle(&self, number: u64) -> XfsResult<XfsExportHandle> {
        let inode = self.inode(number)?;
        Ok(XfsExportHandle {
            inode: inode.number,
            generation: inode.generation,
        })
    }

    /// Resolves a generation-bound export handle.  This does not perform a
    /// pathname or permission check; those are necessarily done by the VFS
    /// caller with its credential and mount-idmap context.
    pub fn resolve_export_handle(&self, handle: XfsExportHandle) -> XfsResult<XfsInode> {
        let inode = self.inode(handle.inode)?;
        if inode.generation != handle.generation {
            return Err(XfsError::AddressOutOfRange);
        }
        Ok(inode)
    }

    /// Reads the first committed log-record header from the configured log.
    /// This is a genuine media check used to gate later recovery; it does not
    /// claim that recovery occurred or alter any allocation-group metadata.
    pub fn first_log_record(&self) -> XfsResult<XfsLogRecordHeader> {
        let bytes = if self.superblock.log_start != 0 {
            self.read_data_fs_block(self.superblock.log_start)?
        } else {
            let log = self
                .external_log
                .as_ref()
                .ok_or(XfsError::UnsupportedFeature)?;
            self.read_from_volume(log, 0, self.superblock.block_size as usize)?
        };
        // A single filesystem block is sufficient for header geometry but
        // not necessarily for its complete payload.  Full-record CRC
        // validation is performed by `decode_journal_record` after the log
        // I/O layer has assembled every sector.
        XfsLogRecordHeader::parse(&bytes, self.superblock.uuid, false)
    }

    /// Decodes a complete journal record supplied by the log I/O layer.  This
    /// is deliberately separate from `first_log_record`: a physical record
    /// may span multiple sector reads and must be assembled before any op is
    /// trusted.
    pub fn decode_journal_record(&self, bytes: &[u8]) -> XfsResult<XfsJournalRecord> {
        XfsJournalRecord::decode_with_crc(bytes, self.superblock.uuid, self.superblock.is_v5())
    }

    /// Builds a replay plan from already assembled log records.  Applying the
    /// plan remains a separate atomic phase which will couple decoded item
    /// types to AG locks, rmap/refcount updates, and ordered FUA/flush I/O.
    pub fn recovery_plan<'a>(
        &self,
        records: impl IntoIterator<Item = &'a [u8]>,
    ) -> XfsResult<XfsRecoveryPlan> {
        let mut plan = XfsRecoveryPlan::new();
        for bytes in records {
            plan.ingest(self.decode_journal_record(bytes)?)?;
        }
        Ok(plan)
    }

    /// Walks the configured internal or external physical log and returns
    /// only the newest contiguous chain of complete records.  Each candidate
    /// is read through the real ring geometry, including a record that wraps
    /// from the last basic block back to zero.  Before an item body is ever
    /// decoded, this verifies record magic/version/UUID/CRC, the LSN's
    /// physical position, and every overwritten cycle stamp.  A torn tail is
    /// therefore discarded rather than becoming a partial transaction.
    pub fn scan_physical_log(&self) -> XfsResult<XfsPhysicalLogScan> {
        #[derive(Clone)]
        struct Candidate {
            start: u32,
            blocks: u32,
            record: XfsJournalRecord,
        }

        let blocks = self.log_region_blocks()?;
        let mut candidates = Vec::new();
        // A physical log has no synthetic clean marker.  Consequently a
        // nonzero sector which cannot be authenticated as part of a complete
        // chain is not evidence of an empty log; it is torn/corrupt media and
        // must block publication rather than being silently discarded.
        let mut saw_nonzero = false;
        for start in 0..blocks {
            let header_bytes = self.read_log_ring_bytes(start, XFS_LOG_BASIC_BLOCK)?;
            saw_nonzero |= header_bytes.iter().any(|byte| *byte != 0);
            if be32(&header_bytes, 0).ok() != Some(XFS_LOG_RECORD_MAGIC) {
                continue;
            }
            let header = match XfsLogRecordHeader::parse(&header_bytes, self.superblock.uuid, false)
            {
                Ok(header) => header,
                Err(_) => continue, // an old/torn sector is not a record
            };
            if !(header.version == 1 || header.version == 2)
                || !(1..=3).contains(&header.format)
                || header.iclog_bytes == 0
                || header.lsn as u32 != start
                || header.previous_block >= blocks
                || header.tail_lsn > header.lsn
                || (header.tail_lsn as u32) >= blocks
                || header.cycle.saturating_sub((header.tail_lsn >> 32) as u32) > 1
            {
                continue;
            }
            let total = match align_log_basic_block(
                header
                    .header_bytes()?
                    .checked_add(header.payload_bytes as usize)
                    .ok_or(XfsError::CorruptMetadata)?,
            ) {
                Ok(total)
                    if total >= XFS_LOG_BASIC_BLOCK
                        && total / XFS_LOG_BASIC_BLOCK <= blocks as usize =>
                {
                    total
                }
                _ => continue,
            };
            let mut image = match self.read_log_ring_bytes(start, total) {
                Ok(image) => image,
                Err(_) => continue,
            };
            // The CRC covers the on-media cycle-stamped image, not the
            // restored item byte stream.
            if XfsLogRecordHeader::parse(&image, self.superblock.uuid, self.superblock.is_v5())
                .is_err()
            {
                continue;
            }
            let header_blocks = match header.header_bytes() {
                Ok(bytes) => bytes / XFS_LOG_BASIC_BLOCK,
                Err(_) => continue,
            };
            let total_blocks = total / XFS_LOG_BASIC_BLOCK;
            let mut cycle_ok = true;
            for block in 1..header_blocks {
                let expected = header
                    .cycle
                    .checked_add(((start as usize + block) / blocks as usize) as u32);
                if expected.and_then(|cycle| {
                    be32(&image, block * XFS_LOG_BASIC_BLOCK)
                        .ok()
                        .map(|word| word == cycle)
                }) != Some(true)
                {
                    cycle_ok = false;
                    break;
                }
            }
            for block in header_blocks..total_blocks {
                let expected = header
                    .cycle
                    .checked_add(((start as usize + block) / blocks as usize) as u32);
                if expected.and_then(|cycle| {
                    be32(&image, block * XFS_LOG_BASIC_BLOCK)
                        .ok()
                        .map(|word| word == cycle)
                }) != Some(true)
                {
                    cycle_ok = false;
                    break;
                }
            }
            if !cycle_ok || XfsLogRing::unstamp_inline_record(&mut image).is_err() {
                continue;
            }
            let record =
                match XfsJournalRecord::decode_with_crc(&image, self.superblock.uuid, false) {
                    Ok(record) => record,
                    Err(_) => continue,
                };
            candidates.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            candidates.push(Candidate {
                start,
                blocks: total_blocks as u32,
                record,
            });
        }
        if candidates.is_empty() {
            if saw_nonzero {
                return Err(XfsError::CorruptMetadata);
            }
            return Ok(XfsPhysicalLogScan {
                records: Vec::new(),
                state: XfsJournalRecoveryState {
                    head_lsn: 0,
                    tail_lsn: 0,
                    committed_transactions: 0,
                    interrupted_transactions: 0,
                },
                // A zeroed log is a clean v5 log, not a read-only projection.
                // Start its first durable record at the canonical cycle-one
                // origin so a freshly formatted filesystem can enter the
                // same live coordinator as a cleanly unmounted one.
                cursor: Some(XfsLogRing::new(blocks, 0, 0, 1)?),
                clean: true,
            });
        }
        candidates.sort_unstable_by_key(|candidate| candidate.record.header.lsn);
        // Follow physical predecessor pointers backwards from the newest
        // authenticated record.  This rejects a valid-looking stale record
        // that happens to survive outside the current circular log chain.
        let mut chain = Vec::new();
        let mut current = candidates.len() - 1;
        loop {
            chain.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            chain.push(current);
            let record = &candidates[current];
            let predecessor = (0..current).rev().find(|index| {
                let prior = &candidates[*index];
                prior.record.header.lsn < record.record.header.lsn
                    && prior.start == record.record.header.previous_block
                    && (prior.start as u64 + prior.blocks as u64) % blocks as u64
                        == record.start as u64
            });
            // `previous_block` binds the predecessor; its end must be this
            // record's start, while its cycle must immediately precede ours.
            let predecessor = predecessor.and_then(|index| {
                let prior = &candidates[index];
                let prior_end_cycle = prior.record.header.cycle.checked_add(
                    ((prior.start as u64 + prior.blocks as u64) / blocks as u64) as u32,
                )?;
                (prior_end_cycle == record.record.header.cycle).then_some(index)
            });
            match predecessor {
                Some(index) => current = index,
                None => break,
            }
        }
        chain.reverse();
        let newest_start = candidates[*chain.last().ok_or(XfsError::CorruptMetadata)?].start;
        let mut records = Vec::new();
        records
            .try_reserve_exact(chain.len())
            .map_err(|_| XfsError::NoMemory)?;
        for index in chain {
            records.push(candidates[index].record.clone());
        }
        let last = records.last().ok_or(XfsError::CorruptMetadata)?;
        let head_block = ((last.header.lsn as u32 as u64
            + (align_log_basic_block(
                last.header
                    .header_bytes()?
                    .checked_add(last.header.payload_bytes as usize)
                    .ok_or(XfsError::CorruptMetadata)?,
            )? / XFS_LOG_BASIC_BLOCK) as u64)
            % blocks as u64) as u32;
        let head_cycle = last
            .header
            .cycle
            .checked_add(
                ((last.header.lsn as u32 as u64
                    + (align_log_basic_block(
                        last.header
                            .header_bytes()?
                            .checked_add(last.header.payload_bytes as usize)
                            .ok_or(XfsError::CorruptMetadata)?,
                    )? / XFS_LOG_BASIC_BLOCK) as u64)
                    / blocks as u64) as u32,
            )
            .ok_or(XfsError::CorruptMetadata)?;
        let end_lsn = (u64::from(head_cycle) << 32) | u64::from(head_block);
        let is_unmount = |record: &XfsJournalRecord| {
            record.operations.len() == 1
                && record.operations[0].client_id == 0xaa
                && record.operations[0].flags == XLOG_UNMOUNT_TRANS
                && record.operations[0].payload == [0x55, 0x6e, 0, 0, 0, 0, 0, 0]
        };
        // A valid unmount record forms a replay boundary.  All records before
        // it had reached durable homes before the marker was forced; feeding
        // the marker into the ordinary START/COMMIT assembler would turn a
        // later post-remount transaction into a false recovery failure.
        let boundary = records.iter().rposition(is_unmount);
        let tail_lsn = match boundary {
            Some(index) => {
                let record = &records[index];
                let record_bytes = align_log_basic_block(
                    record
                        .header
                        .header_bytes()?
                        .checked_add(record.header.payload_bytes as usize)
                        .ok_or(XfsError::CorruptMetadata)?,
                )?;
                let record_blocks = u32::try_from(record_bytes / XFS_LOG_BASIC_BLOCK)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                XfsLogReservation {
                    lsn: record.header.lsn,
                    cycle: record.header.cycle,
                    first_block: record.header.lsn as u32,
                    record_blocks,
                    first_segment_blocks: 0,
                    second_segment_blocks: 0,
                }
                .end_lsn(blocks)?
            }
            None => last.header.tail_lsn,
        };
        let cursor = XfsLogRing::recovered(
            blocks,
            head_block,
            tail_lsn as u32,
            head_cycle,
            newest_start,
        )?;
        let clean_unmount = boundary == Some(records.len() - 1);
        if clean_unmount {
            return Ok(XfsPhysicalLogScan {
                // The terminal marker proves all preceding history reached
                // home; it is not transaction input for a later mount.
                records: Vec::new(),
                state: XfsJournalRecoveryState {
                    head_lsn: end_lsn,
                    tail_lsn,
                    committed_transactions: 0,
                    interrupted_transactions: 0,
                },
                cursor: Some(cursor),
                clean: true,
            });
        }
        let mut plan = XfsRecoveryPlan::new();
        let replay_start = boundary.map_or(0, |index| index + 1);
        let replay_records = records.split_off(replay_start);
        for record in &replay_records {
            plan.ingest(record.clone())?;
        }
        let plan_state = plan.state();
        Ok(XfsPhysicalLogScan {
            records: replay_records,
            state: XfsJournalRecoveryState {
                head_lsn: end_lsn,
                tail_lsn,
                committed_transactions: plan_state.committed_transactions,
                interrupted_transactions: plan_state.interrupted_transactions,
            },
            cursor: Some(cursor),
            clean: false,
        })
    }

    /// Builds a recovery plan directly from the authenticated physical log.
    /// Unsupported replay items remain unsupported when a caller later asks
    /// for buffer commits; this method only supplies trusted record order.
    pub fn physical_recovery_plan(&self) -> XfsResult<(XfsPhysicalLogScan, XfsRecoveryPlan)> {
        let scan = self.scan_physical_log()?;
        if scan.clean {
            return Ok((scan, XfsRecoveryPlan::new()));
        }
        let mut plan = XfsRecoveryPlan::new();
        for record in &scan.records {
            plan.ingest(record.clone())?;
        }
        Ok((scan, plan))
    }

    /// Decodes the compact directory representation stored in a local data
    /// fork.  Block/leaf/node directories are not returned as partial lists;
    /// their hash and free-space btrees require their own verifier.
    pub fn shortform_directory(&self, number: u64) -> XfsResult<Vec<XfsDirectoryEntry>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o040000 || inode.data_format != XfsForkFormat::Local {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork = inode.data_fork(&raw)?;
        let used = usize::try_from(inode.size).map_err(|_| XfsError::CorruptMetadata)?;
        if used > fork.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = &fork[..used];
        if payload.len() < 6 {
            return Err(XfsError::CorruptMetadata);
        }
        let count = payload[0] as usize;
        let inode_width = if payload[1] == 0 { 4 } else { 8 };
        let has_ftype = self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0;
        let mut cursor = if inode_width == 4 { 6 } else { 10 };
        if cursor > payload.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        for _ in 0..count {
            let name_len = byte(payload, cursor)? as usize;
            cursor = cursor.checked_add(3).ok_or(XfsError::CorruptMetadata)?; // len + offset
            let name = slice(payload, cursor, name_len)?.to_vec();
            cursor = cursor
                .checked_add(name_len)
                .ok_or(XfsError::CorruptMetadata)?;
            let inode = match inode_width {
                4 => be32(payload, cursor)? as u64,
                8 => be64(payload, cursor)?,
                _ => return Err(XfsError::CorruptMetadata),
            };
            cursor = cursor
                .checked_add(inode_width)
                .ok_or(XfsError::CorruptMetadata)?;
            let file_type = if has_ftype {
                let ty = byte(payload, cursor)?;
                cursor = cursor.checked_add(1).ok_or(XfsError::CorruptMetadata)?;
                Some(ty)
            } else {
                None
            };
            if inode == 0 || name.is_empty() || name.iter().any(|byte| *byte == b'/' || *byte == 0)
            {
                return Err(XfsError::CorruptMetadata);
            }
            entries.push(XfsDirectoryEntry {
                name,
                inode,
                file_type,
            });
        }
        if cursor != payload.len() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(entries)
    }

    /// Replaces a local directory's complete namespace image.  Names remain
    /// raw bytes; this is deliberately below VFS policy and does not accept
    /// dot, slash, or NUL spellings.  The caller appends the resulting inode
    /// buffer to the same transaction as every child/link-count change.
    pub fn stage_shortform_directory(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if parent == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o040000 || inode.data_format != XfsForkFormat::Local {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            inode.fork_offset as usize * 8
        };
        if fork_end < fork_begin {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = serialize_shortform_directory(
            parent,
            entries,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0,
            if self.superblock.is_v5() { 64 } else { 16 },
        )?;
        if payload.len() > fork_end - fork_begin {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut after = raw.clone();
        after[fork_begin..fork_end].fill(0);
        after[fork_begin..fork_begin + payload.len()].copy_from_slice(&payload);
        put_be64(&mut after, 56, payload.len() as u64)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after, 24, 0)?;
        } else {
            put_be32(&mut after, 76, 0)?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Selects the persistent directory representation while preserving the
    /// caller's transaction envelope.  In particular, shortform promotion's
    /// AG reservation is appended before the new inode and data image, so it
    /// can be combined with the other half of a cross-directory operation.
    pub fn stage_directory_entries_with_parent(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let mut staged = transaction.clone();
        if parent == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let inode = self.inode(number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        if entries.iter().enumerate().any(|(index, entry)| {
            entry.inode == 0
                || entry.name.is_empty()
                || entry.name == b"."
                || entry.name == b".."
                || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
                || entries[..index]
                    .iter()
                    .any(|prior| prior.name == entry.name)
        }) {
            return Err(XfsError::AddressOutOfRange);
        }
        match inode.data_format {
            XfsForkFormat::Local => {
                match self.stage_shortform_directory(number, parent, entries, &mut staged) {
                    Ok(()) => {
                        *transaction = staged;
                        Ok(())
                    }
                    Err(XfsError::AddressOutOfRange) => {
                        let blocks = u32::try_from(
                            self.directory_block_size()? / self.superblock.block_size as usize,
                        )
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                        let (ag, _) = self.split_inode_number(number)?;
                        let allocation = self.prepare_extent_allocation(ag, blocks)?;
                        let mut promotion = XfsMetadataTransaction::default();
                        self.stage_shortform_directory_promotion(
                            number,
                            parent,
                            entries,
                            &allocation,
                            &mut promotion,
                        )?;
                        staged.buffers.extend(allocation.transaction.buffers);
                        staged.buffers.extend(promotion.buffers);
                        *transaction = staged;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            // External directory parent relocation requires updating its
            // native dot-dot record together with the data/index/free tree.
            // That planner is not present yet; never discard a requested
            // parent change merely because the directory outgrew shortform.
            XfsForkFormat::Extents | XfsForkFormat::Btree => {
                self.stage_directory_block(number, parent, entries, &mut staged)?;
                *transaction = staged;
                Ok(())
            }
            _ => Err(XfsError::UnsupportedFeature),
        }
    }

    /// Directory counterpart of the direct data-fork writer.  Namespace
    /// rebuilds use sparse logical ranges for data, leaf/node and free-space
    /// blocks; keeping them in one checked extent vector makes the inode
    /// image and the allocation transaction a single commit unit.
    fn stage_directory_inode_extents(
        &self,
        number: u64,
        extents: &[XfsExtent],
        size: u64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            inode.fork_offset as usize * 8
        };
        if fork_end < fork_begin
            || extents
                .len()
                .checked_mul(16)
                .ok_or(XfsError::AddressOutOfRange)?
                > fork_end - fork_begin
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut prior_end = 0u64;
        for (index, extent) in extents.iter().enumerate() {
            if extent.block_count == 0
                || extent
                    .file_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || (index != 0 && extent.file_block < prior_end)
            {
                return Err(XfsError::CorruptMetadata);
            }
            prior_end = extent.file_block + extent.block_count as u64;
        }
        let old_data = if matches!(
            inode.data_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            let mappings =
                self.inode_data_extents(number)?
                    .iter()
                    .try_fold(0u64, |total, extent| {
                        total
                            .checked_add(extent.block_count as u64)
                            .ok_or(XfsError::AddressOutOfRange)
                    })?;
            mappings
                .checked_add(if inode.data_format == XfsForkFormat::Btree {
                    self.inode_bmbt_blocks(number)?.len() as u64
                } else {
                    0
                })
                .ok_or(XfsError::AddressOutOfRange)?
        } else {
            0
        };
        let new_data = extents.iter().try_fold(0u64, |total, extent| {
            total
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?;
        let unit = (self.superblock.block_size / 512) as u64;
        let mut after = raw.clone();
        after[5] = XfsForkFormat::Extents as u8;
        after[fork_begin..fork_end].fill(0);
        put_be64(&mut after, 56, size)?;
        let old_blocks = old_data
            .checked_mul(unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_blocks = new_data
            .checked_mul(unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let blocks = inode
            .blocks
            .checked_sub(old_blocks)
            .and_then(|base| base.checked_add(new_blocks))
            .ok_or(XfsError::CorruptMetadata)?;
        put_be64(&mut after, 64, blocks)?;
        for (index, extent) in extents.iter().enumerate() {
            after[fork_begin + index * 16..fork_begin + (index + 1) * 16]
                .copy_from_slice(&encode_xfs_extent(*extent)?);
        }
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after, 24, extents.len() as u64)?;
        } else {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages replacement of an already materialized one-block directory.
    /// The caller has a complete namespace vector, so deletion, collision
    /// replacement, and hash/free-space recomputation are inseparable from
    /// the buffer image appended to its journal transaction.
    pub fn stage_directory_block(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        self.stage_directory_block_reserved(number, parent, entries, None, true, transaction)
    }

    /// Stages an external directory against a reservation selected by the
    /// namespace coordinator.  Cross-parent rename uses this entry point for
    /// every same-AG directory, then publishes one combined AG allocator
    /// image for all reservations and returns.  `stage_allocator` remains
    /// true only for the one-directory convenience wrapper above.
    pub fn stage_directory_block_with_reservation(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        allocation: &XfsExtentAllocation,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        self.stage_directory_block_reserved(
            number,
            parent,
            entries,
            Some(allocation),
            false,
            transaction,
        )
    }

    /// Returns every old data-fork block that a reserved external-directory
    /// rebuild returns.  The coordinator groups this result by AG alongside
    /// its reservation batch before calling the private allocator builder.
    pub fn directory_rebuild_releases(&self, number: u64) -> XfsResult<Vec<u64>> {
        let inode = self.inode(number)?;
        if inode.mode & 0o170000 != 0o040000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Local | XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let extents = match inode.data_format {
            XfsForkFormat::Local => Vec::new(),
            XfsForkFormat::Extents => self.inode_data_extents(number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let mut blocks = Vec::new();
        for extent in extents {
            for block in extent.start_block
                ..extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .ok_or(XfsError::CorruptMetadata)?
            {
                blocks.push(block);
            }
        }
        if inode.data_format == XfsForkFormat::Btree {
            blocks.extend(self.inode_bmbt_blocks(number)?);
        }
        Ok(blocks)
    }

    /// Returns every block removed while destroying a directory inode.  This
    /// intentionally includes its attribute fork: unlike a directory
    /// rebuild, inode teardown writes the attr fork back as local/empty and
    /// can therefore release its remote xattr and attribute-BMBT homes.
    pub fn directory_teardown_releases(&self, number: u64) -> XfsResult<Vec<u64>> {
        let inode = self.inode(number)?;
        let mut blocks = self.directory_rebuild_releases(number)?;
        if matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            for extent in self.inode_attr_extents(number)? {
                for block in extent.start_block
                    ..extent
                        .start_block
                        .checked_add(extent.block_count as u64)
                        .ok_or(XfsError::CorruptMetadata)?
                {
                    blocks.push(block);
                }
            }
            if inode.attr_format == XfsForkFormat::Btree {
                blocks.extend(self.attr_bmbt_blocks(number)?);
            }
        }
        Ok(blocks)
    }

    /// Clears a directory data fork after its verified data/BMBT ownership
    /// has been returned by the unified AG planner.
    pub fn stage_directory_reclaim_inode(
        &self,
        number: u64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let begin = inode.core_bytes as usize;
        let data_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            inode.fork_offset as usize * 8
        };
        if data_end < begin || data_end > raw.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut after = raw.clone();
        after[5] = XfsForkFormat::Local as u8;
        after[begin..data_end].fill(0);
        if inode.fork_offset != 0 {
            after[83] = XfsForkFormat::Local as u8;
            after[data_end..].fill(0);
        }
        put_be64(&mut after, 56, 0)?;
        put_be64(&mut after, 64, 0)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after, 24, 0)?;
            put_be32(&mut after, 76, 0)?;
        } else {
            put_be32(&mut after, 76, 0)?;
            put_be16(&mut after, 80, 0)?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Exact contiguous reservation size for a complete external directory
    /// rebuild.  It intentionally uses the same record alignment and DA
    /// fanout as the serializer, so a coordinator can batch several plans
    /// without speculative allocation or a later reservation mismatch.
    pub fn directory_rebuild_blocks(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
    ) -> XfsResult<u32> {
        let logical = self.directory_block_size()?;
        if logical != self.superblock.block_size as usize || parent == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let ftype = self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0;
        let header = if self.superblock.is_v5() {
            64usize
        } else {
            16usize
        };
        let mut used = header;
        let mut data_blocks = 1usize;
        let mut count = 0usize;
        for entry in core::iter::once(XfsDirectoryEntry {
            name: b".".to_vec(),
            inode: number,
            file_type: Some(2),
        })
        .chain(core::iter::once(XfsDirectoryEntry {
            name: b"..".to_vec(),
            inode: parent,
            file_type: Some(2),
        }))
        .chain(entries.iter().cloned())
        {
            if entry.inode == 0
                || entry.name.is_empty()
                || entry.name.len() > u8::MAX as usize
                || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
                || (ftype && entry.file_type.is_none())
            {
                return Err(XfsError::AddressOutOfRange);
            }
            let length = align8(
                11usize
                    .checked_add(entry.name.len())
                    .and_then(|value| value.checked_add(usize::from(ftype)))
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .ok_or(XfsError::AddressOutOfRange)?;
            if header
                .checked_add(length)
                .ok_or(XfsError::AddressOutOfRange)?
                > logical
            {
                return Err(XfsError::AddressOutOfRange);
            }
            if used
                .checked_add(length)
                .ok_or(XfsError::AddressOutOfRange)?
                > logical
            {
                data_blocks = data_blocks
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                used = header;
            }
            used += length;
            count = count.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        let capacity = logical
            .checked_sub(header)
            .ok_or(XfsError::AddressOutOfRange)?
            / 8;
        if capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let leaves = count.div_ceil(capacity);
        let mut intermediate = 0usize;
        if leaves > 1 {
            let mut nodes = leaves;
            while nodes > capacity {
                nodes = nodes.div_ceil(capacity);
                intermediate = intermediate
                    .checked_add(nodes)
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        u32::try_from(
            data_blocks
                .checked_add(if leaves == 1 {
                    1
                } else {
                    1usize
                        .checked_add(leaves)
                        .and_then(|value| value.checked_add(intermediate))
                        .ok_or(XfsError::AddressOutOfRange)?
                })
                .and_then(|value| value.checked_add(1))
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .map_err(|_| XfsError::AddressOutOfRange)
    }

    /// Publishes one AG image for an entire set of reserved directory
    /// rebuilds and their returned old blocks.  Call once per AG before
    /// `stage_directory_block_with_reservation`; this prevents cross-parent
    /// rename from constructing incompatible duplicate AGF/AGFL images.
    pub fn stage_directory_rebuild_allocator_delta(
        &self,
        ag: u32,
        allocations: &[(u32, u32)],
        releases: &[u64],
        free_inodes: &[u64],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let mut local = Vec::new();
        for physical in releases {
            if *physical / self.superblock.ag_blocks as u64 == ag as u64 {
                local.push(
                    u32::try_from(*physical % self.superblock.ag_blocks as u64)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                );
            }
        }
        let mut local_inodes = Vec::new();
        for inode in free_inodes {
            let (inode_ag, agino) = self.split_inode_number(*inode)?;
            if inode_ag == ag {
                local_inodes.push(u32::try_from(agino).map_err(|_| XfsError::AddressOutOfRange)?);
            }
        }
        transaction.buffers.extend(
            self.stage_unified_ag_snapshot_delta(ag, allocations, &local, &local_inodes)?
                .buffers,
        );
        Ok(())
    }

    /// Applies allocation, return, and inode-free decisions from one immutable
    /// AG ownership snapshot.  Directory rebuilds use this instead of
    /// composing independent AGF/AGFL images; teardown callers can add their
    /// inode bits to the same decision without racing the namespace allocator.
    pub fn stage_unified_ag_snapshot_delta(
        &self,
        ag: u32,
        allocations: &[(u32, u32)],
        releases: &[u32],
        free_inodes: &[u32],
    ) -> XfsResult<XfsMetadataTransaction> {
        let snapshot = self.ag_ownership_snapshot(ag)?;
        let mut extents = snapshot.free_extents.clone();
        let mut requested = allocations.to_vec();
        requested.sort_unstable_by_key(|(start, _)| *start);
        for (start, count) in requested {
            let end = start
                .checked_add(count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if count == 0 || start < 4 || end > self.superblock.ag_blocks {
                return Err(XfsError::AddressOutOfRange);
            }
            let index = extents
                .iter()
                .position(|extent| {
                    extent.start_block <= start
                        && extent
                            .start_block
                            .checked_add(extent.block_count)
                            .is_some_and(|limit| end <= limit)
                })
                .ok_or(XfsError::CorruptMetadata)?;
            let old = extents.remove(index);
            let old_end = old
                .start_block
                .checked_add(old.block_count)
                .ok_or(XfsError::CorruptMetadata)?;
            if old.start_block < start {
                extents.push(XfsAgFreeRecord {
                    start_block: old.start_block,
                    block_count: start - old.start_block,
                });
            }
            if end < old_end {
                extents.push(XfsAgFreeRecord {
                    start_block: end,
                    block_count: old_end - end,
                });
            }
        }
        for block in releases {
            if *block < 4
                || *block >= self.superblock.ag_blocks
                || snapshot
                    .ino_nodes
                    .iter()
                    .chain(snapshot.fino_nodes.iter())
                    .chain(snapshot.bno_nodes.iter())
                    .chain(snapshot.cnt_nodes.iter())
                    .any(|node| node.block == *block)
                || snapshot.freelist.entries.contains(block)
                || extents.iter().any(|extent| {
                    *block >= extent.start_block && *block < extent.start_block + extent.block_count
                })
            {
                return Err(XfsError::CorruptMetadata);
            }
            extents.push(XfsAgFreeRecord {
                start_block: *block,
                block_count: 1,
            });
        }
        extents.sort_unstable_by_key(|extent| extent.start_block);
        let mut merged: Vec<XfsAgFreeRecord> = Vec::new();
        for extent in extents {
            if let Some(last) = merged.last_mut()
                && last.start_block.checked_add(last.block_count) == Some(extent.start_block)
            {
                last.block_count = last
                    .block_count
                    .checked_add(extent.block_count)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                merged.push(extent);
            }
        }
        let mut records = snapshot.inode_records.clone();
        for inode in free_inodes {
            let record = records
                .iter_mut()
                .find(|record| {
                    record
                        .start_inode
                        .checked_add(64)
                        .is_some_and(|end| *inode >= record.start_inode && *inode < end)
                })
                .ok_or(XfsError::AddressOutOfRange)?;
            let bit = *inode - record.start_inode;
            if record.free_mask & (1u64 << bit) != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            record.free_mask |= 1u64 << bit;
            record.free_count = record.free_mask.count_ones();
        }
        self.stage_combined_inode_and_free_space_trees(&snapshot, records, merged)
    }

    fn stage_directory_block_reserved(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        reservation: Option<&XfsExtentAllocation>,
        stage_allocator: bool,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode = self.inode(number)?;
        if inode.mode & 0o170000 != 0o040000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let logical = self.directory_block_size()?;
        let fs = self.superblock.block_size as usize;
        // The journal buffer item is filesystem-block granular.  A dirblk
        // larger than that requires multi-buffer CRC ownership, which is a
        // separate log-item change; do not emit half a dirblk.
        if logical != fs || parent == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let ftype = self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0;
        let mut names = Vec::new();
        names
            .try_reserve_exact(
                entries
                    .len()
                    .checked_add(2)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        names.push(XfsDirectoryEntry {
            name: b".".to_vec(),
            inode: number,
            file_type: Some(2),
        });
        names.push(XfsDirectoryEntry {
            name: b"..".to_vec(),
            inode: parent,
            file_type: Some(2),
        });
        names.extend_from_slice(entries);
        if names.iter().enumerate().any(|(index, entry)| {
            entry.inode == 0
                || entry.name.is_empty()
                || entry.name.len() > u8::MAX as usize
                || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
                || (ftype && entry.file_type.is_none())
                || names[..index].iter().any(|prior| prior.name == entry.name)
        }) {
            return Err(XfsError::AddressOutOfRange);
        }

        // Pack records in namespace order.  No fixed occupancy heuristic is
        // used: an entry moves only when it cannot fit its native record.
        let mut groups: Vec<Vec<(XfsDirectoryEntry, bool)>> = Vec::new();
        for entry in names {
            let mut candidate = groups.last().cloned().unwrap_or_default();
            candidate.push((entry.clone(), false));
            if serialize_directory_data_block(
                self.superblock.meta_uuid,
                number,
                0,
                &candidate,
                ftype,
                self.superblock.is_v5(),
                logical,
            )
            .is_ok()
            {
                if let Some(last) = groups.last_mut() {
                    last.push((entry, false));
                } else {
                    groups.push(vec![(entry, false)]);
                }
            } else {
                if serialize_directory_data_block(
                    self.superblock.meta_uuid,
                    number,
                    0,
                    core::slice::from_ref(&(entry.clone(), false)),
                    ftype,
                    self.superblock.is_v5(),
                    logical,
                )
                .is_err()
                {
                    return Err(XfsError::AddressOutOfRange);
                }
                groups.push(vec![(entry, false)]);
            }
        }
        let data_blocks = groups.len();
        let leaf_header = if self.superblock.is_v5() {
            64usize
        } else {
            16usize
        };
        let leaf_capacity = logical
            .checked_sub(leaf_header)
            .ok_or(XfsError::AddressOutOfRange)?
            / 8;
        if leaf_capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut provisional = Vec::new();
        for (index, group) in groups.iter().enumerate() {
            let (_, edges, _) = serialize_directory_data_block(
                self.superblock.meta_uuid,
                number,
                0,
                group,
                ftype,
                self.superblock.is_v5(),
                logical,
            )?;
            let base_address = u32::try_from(
                index
                    .checked_mul(logical)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::AddressOutOfRange)?;
            for mut edge in edges {
                edge.address = edge
                    .address
                    .checked_add(base_address)
                    .ok_or(XfsError::AddressOutOfRange)?;
                provisional.push(edge);
            }
        }
        provisional.sort_unstable_by_key(|edge| (edge.hash, edge.address));
        let leaf_groups = provisional
            .chunks(leaf_capacity)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        let single_leaf = leaf_groups.len() == 1;
        let mut index_images: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut leaf_roots = Vec::new();
        if single_leaf {
            leaf_roots.push(XfsDirectoryLeafEntry {
                hash: leaf_groups[0].last().ok_or(XfsError::CorruptMetadata)?.hash,
                address: 0,
            });
        } else {
            for (index, leaf) in leaf_groups.iter().enumerate() {
                let logical_index =
                    u64::try_from(index + 1).map_err(|_| XfsError::AddressOutOfRange)?;
                leaf_roots.push(XfsDirectoryLeafEntry {
                    hash: leaf.last().ok_or(XfsError::CorruptMetadata)?.hash,
                    address: u32::try_from(logical_index)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                });
            }
        }
        // Build enough DA levels for the leaf fanout; logical zero remains
        // the root, leaf records start at one, then interior levels follow.
        let mut current = leaf_roots;
        let mut next_logical = if single_leaf {
            1u64
        } else {
            1u64.checked_add(
                u64::try_from(leaf_groups.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )
            .ok_or(XfsError::AddressOutOfRange)?
        };
        let mut level = 1u16;
        while !single_leaf && current.len() > leaf_capacity {
            let mut parents = Vec::new();
            for chunk in current.chunks(leaf_capacity) {
                let logical_index = next_logical;
                next_logical = next_logical
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
                parents.push(XfsDirectoryLeafEntry {
                    hash: chunk.last().ok_or(XfsError::CorruptMetadata)?.hash,
                    address: u32::try_from(logical_index)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                });
                // Image physical placement is filled after allocation.
                index_images.push((
                    logical_index,
                    serialize_directory_node(
                        chunk,
                        level,
                        0,
                        0,
                        self.superblock.meta_uuid,
                        number,
                        0,
                        self.superblock.is_v5(),
                        logical,
                    )?,
                ));
            }
            current = parents;
            level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        let index_blocks = if single_leaf {
            1usize
        } else {
            1usize
                .checked_add(leaf_groups.len())
                .and_then(|count| count.checked_add(index_images.len()))
                .ok_or(XfsError::AddressOutOfRange)?
        };
        let (ag, _) = self.split_inode_number(number)?;
        let required = u32::try_from(
            data_blocks
                .checked_add(index_blocks)
                .and_then(|count| count.checked_add(1))
                .ok_or(XfsError::AddressOutOfRange)?,
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        let owned_allocation;
        let allocation = if let Some(reservation) = reservation {
            if reservation.ag != ag || reservation.block_count != required {
                return Err(XfsError::AddressOutOfRange);
            }
            reservation
        } else {
            owned_allocation = self.prepare_extent_allocation(ag, required)?;
            &owned_allocation
        };
        let base = (allocation.ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|value| value.checked_add(allocation.start_block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let index_base = base
            .checked_add(data_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let free_physical = index_base
            .checked_add(index_blocks as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let sector = (self.superblock.block_size / 512) as u64;
        let mut new_extents = vec![
            XfsExtent {
                unwritten: false,
                file_block: 0,
                start_block: base,
                block_count: u32::try_from(data_blocks).map_err(|_| XfsError::AddressOutOfRange)?,
            },
            XfsExtent {
                unwritten: false,
                file_block: XFS_DIR_LEAF_SPACE_BYTES / fs as u64,
                start_block: index_base,
                block_count: u32::try_from(index_blocks)
                    .map_err(|_| XfsError::AddressOutOfRange)?,
            },
            XfsExtent {
                unwritten: false,
                file_block: XFS_DIR_FREE_SPACE_BYTES / fs as u64,
                start_block: free_physical,
                block_count: 1,
            },
        ];
        new_extents.sort_unstable_by_key(|extent| extent.file_block);
        let old_extents = if inode.data_format == XfsForkFormat::Extents {
            self.inode_data_extents(number)?
        } else {
            self.inode_bmbt_extents(number)?
        };
        let old_bmap_blocks = if inode.data_format == XfsForkFormat::Btree {
            self.inode_bmbt_blocks(number)?
        } else {
            Vec::new()
        };
        let mut releases: Vec<(u32, Vec<u32>)> = Vec::new();
        for extent in &old_extents {
            for physical in extent.start_block
                ..extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .ok_or(XfsError::CorruptMetadata)?
            {
                let old_ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, list)) = releases
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == old_ag)
                {
                    list.push(block);
                } else {
                    releases.push((old_ag, vec![block]));
                }
            }
        }
        for physical in old_bmap_blocks {
            let old_ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, list)) = releases
                .iter_mut()
                .find(|(candidate, _)| *candidate == old_ag)
            {
                list.push(block);
            } else {
                releases.push((old_ag, vec![block]));
            }
        }
        let mut staged = transaction.clone();
        let released_here = releases
            .iter()
            .find(|(candidate, _)| *candidate == ag)
            .map(|(_, list)| list.as_slice())
            .unwrap_or(&[]);
        if stage_allocator {
            staged.buffers.extend(
                self.stage_extent_delta(
                    ag,
                    &[(allocation.start_block, allocation.block_count)],
                    released_here,
                )?
                .buffers,
            );
            for (old_ag, blocks) in &releases {
                if *old_ag != ag {
                    staged
                        .buffers
                        .extend(self.stage_extent_delta(*old_ag, &[], blocks)?.buffers);
                }
            }
        }
        self.stage_directory_inode_extents(
            number,
            &new_extents,
            u64::try_from(data_blocks)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(logical as u64)
                .ok_or(XfsError::AddressOutOfRange)?,
            &mut staged,
        )?;
        let mut free = Vec::new();
        free.try_reserve_exact(data_blocks)
            .map_err(|_| XfsError::NoMemory)?;
        for (index, group) in groups.iter().enumerate() {
            let physical = base
                .checked_add(index as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let basic = physical
                .checked_mul(sector)
                .ok_or(XfsError::AddressOutOfRange)?;
            let (bytes, _, best) = serialize_directory_data_block(
                self.superblock.meta_uuid,
                number,
                basic,
                group,
                ftype,
                self.superblock.is_v5(),
                logical,
            )?;
            free.push(best);
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Directory,
                basic_block: basic,
                before: self.read_data_fs_block(physical)?,
                after: bytes,
            });
        }
        if single_leaf {
            let basic = index_base
                .checked_mul(sector)
                .ok_or(XfsError::AddressOutOfRange)?;
            let bytes = serialize_directory_leaf(
                &provisional,
                0,
                0,
                true,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                logical,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Directory,
                basic_block: basic,
                before: self.read_data_fs_block(index_base)?,
                after: bytes,
            });
        } else {
            for (index, leaf) in leaf_groups.iter().enumerate() {
                let physical = index_base
                    .checked_add(1 + index as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = physical
                    .checked_mul(sector)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let bytes = serialize_directory_leaf(
                    leaf,
                    if index + 1 == leaf_groups.len() {
                        0
                    } else {
                        u32::try_from(index + 2).map_err(|_| XfsError::AddressOutOfRange)?
                    },
                    if index == 0 {
                        0
                    } else {
                        u32::try_from(index).map_err(|_| XfsError::AddressOutOfRange)?
                    },
                    false,
                    self.superblock.meta_uuid,
                    number,
                    basic,
                    self.superblock.is_v5(),
                    logical,
                )?;
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Directory,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: bytes,
                });
            }
            for (logical_index, _) in &index_images {
                let physical = index_base
                    .checked_add(*logical_index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = physical
                    .checked_mul(sector)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let entries_for_node = current.clone();
                let bytes = if *logical_index == 0 {
                    serialize_directory_node(
                        &entries_for_node,
                        level,
                        0,
                        0,
                        self.superblock.meta_uuid,
                        number,
                        basic,
                        self.superblock.is_v5(),
                        logical,
                    )?
                } else {
                    let (_, template) = index_images
                        .iter()
                        .find(|(candidate, _)| candidate == logical_index)
                        .ok_or(XfsError::CorruptMetadata)?;
                    let mut bytes = template.clone();
                    if self.superblock.is_v5() {
                        put_be64(&mut bytes, 16, basic)?;
                        rewrite_crc32c(&mut bytes, 12)?;
                    }
                    bytes
                };
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Directory,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: bytes,
                });
            }
            let basic = index_base
                .checked_mul(sector)
                .ok_or(XfsError::AddressOutOfRange)?;
            let root = serialize_directory_node(
                &current,
                level,
                0,
                0,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                logical,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Directory,
                basic_block: basic,
                before: self.read_data_fs_block(index_base)?,
                after: root,
            });
        }
        let free_basic = free_physical
            .checked_mul(sector)
            .ok_or(XfsError::AddressOutOfRange)?;
        let free_bytes = serialize_directory_free(
            &free,
            self.superblock.meta_uuid,
            number,
            free_basic,
            self.superblock.is_v5(),
            logical,
        )?;
        staged.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Directory,
            basic_block: free_basic,
            before: self.read_data_fs_block(free_physical)?,
            after: free_bytes,
        });
        *transaction = staged;
        Ok(())
    }

    /// Promotes a shortform directory to its first real dir2/dir3 block in
    /// the same transaction as the AG reservation.  This is deliberately a
    /// one-way transition: shrink-to-shortform is only valid after proving
    /// that every external data and index block has been reclaimed.
    pub fn stage_shortform_directory_promotion(
        &self,
        number: u64,
        parent: u64,
        entries: &[XfsDirectoryEntry],
        allocation: &XfsExtentAllocation,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode & 0o170000 != 0o040000
            || inode.data_format != XfsForkFormat::Local
            || self.directory_block_size()? != self.superblock.block_size as usize
            || allocation.block_count != 1
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            inode.fork_offset as usize * 8
        };
        if fork_end < fork_begin || fork_end - fork_begin < 16 {
            return Err(XfsError::AddressOutOfRange);
        }
        let start = (allocation.ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(allocation.start_block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut after = raw.clone();
        after[5] = XfsForkFormat::Extents as u8;
        after[fork_begin..fork_end].fill(0);
        after[fork_begin..fork_begin + 16].copy_from_slice(&encode_xfs_extent(XfsExtent {
            unwritten: false,
            file_block: 0,
            start_block: start,
            block_count: allocation.block_count,
        })?);
        put_be64(&mut after, 56, self.directory_block_size()? as u64)?;
        put_be64(
            &mut after,
            64,
            u64::from(allocation.block_count)
                .checked_mul((self.superblock.block_size / 512) as u64)
                .ok_or(XfsError::AddressOutOfRange)?,
        )?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be64(&mut after, 24, 1)?;
        } else {
            put_be32(&mut after, 76, 1)?;
        }
        self.stage_inode_image(number, raw, after, transaction)?;
        // The inode staging above is private; materialize the data blocks
        // directly from the reservation, whose allocator image is already in
        // this transaction and therefore cannot be reused concurrently.
        let logical = self.directory_block_size()?;
        let image = XfsDirectoryBlockImage {
            parent,
            entries: entries.to_vec(),
            bestfree: [XfsDirectoryBestFree {
                offset: 0,
                length: 0,
            }; 3],
            leaf: Vec::new(),
            dir3: self.superblock.is_v5(),
        }
        .serialize(
            self.superblock.meta_uuid,
            number,
            start
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0,
            logical,
        )?;
        let fs = self.superblock.block_size as usize;
        for index in 0..allocation.block_count as usize {
            let block = start + index as u64;
            let before = self.read_data_fs_block(block)?;
            transaction.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Directory,
                basic_block: block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?,
                before,
                after: image[index * fs..(index + 1) * fs].to_vec(),
            });
        }
        Ok(())
    }

    /// Decodes an inline attribute fork.  The format has no implicit string
    /// conversion and enforces its stored total size before allocating names
    /// or values.  Leaf/node attribute trees are intentionally rejected until
    /// their remote-value and btree readers can preserve atomic xattr updates.
    pub fn shortform_xattrs(&self, number: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.attr_format != XfsForkFormat::Local {
            return Err(XfsError::UnsupportedFeature);
        }
        let payload = inode.attr_fork(&raw)?;
        if payload.len() < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let stored_size = be16(payload, 0)? as usize;
        let count = byte(payload, 2)? as usize;
        if stored_size > payload.len() || stored_size < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = &payload[..stored_size];
        let mut cursor = 4usize;
        let mut attrs = Vec::new();
        attrs
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        for _ in 0..count {
            let name_len = byte(payload, cursor)? as usize;
            let value_len = byte(
                payload,
                cursor.checked_add(1).ok_or(XfsError::CorruptMetadata)?,
            )? as usize;
            let flags = byte(
                payload,
                cursor.checked_add(2).ok_or(XfsError::CorruptMetadata)?,
            )?;
            cursor = cursor.checked_add(3).ok_or(XfsError::CorruptMetadata)?;
            let name = slice(payload, cursor, name_len)?.to_vec();
            cursor = cursor
                .checked_add(name_len)
                .ok_or(XfsError::CorruptMetadata)?;
            let value = slice(payload, cursor, value_len)?.to_vec();
            cursor = cursor
                .checked_add(value_len)
                .ok_or(XfsError::CorruptMetadata)?;
            if name.is_empty() || name.iter().any(|byte| *byte == 0) {
                return Err(XfsError::CorruptMetadata);
            }
            attrs.push(XfsShortformXattr { flags, name, value });
        }
        if cursor != payload.len() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(attrs)
    }

    /// Enumerates the native attribute fork representation currently present
    /// on disk.  Local forks use their compact encoding; non-local forks are
    /// decoded through an attribute leaf block instead of being silently
    /// presented as an empty xattr namespace.
    pub fn xattrs(&self, number: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let inode = self.inode(number)?;
        if inode.attr_format == XfsForkFormat::Local {
            return self.shortform_xattrs(number);
        }
        if !matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            return Err(XfsError::UnsupportedFeature);
        }
        let extents = self.inode_attr_extents(number)?;
        let mut seen = Vec::new();
        let entries = self.attribute_leaf_entries(number, &extents, 0, None, &mut seen)?;
        let mut attrs = Vec::new();
        attrs
            .try_reserve_exact(entries.len())
            .map_err(|_| XfsError::NoMemory)?;
        for entry in entries {
            let value = if entry.value_block == 0 {
                entry.value
            } else {
                self.read_attribute_remote_value(
                    number,
                    &extents,
                    entry.value_block as u64,
                    entry.value_length as usize,
                )?
            };
            attrs.push(XfsShortformXattr {
                flags: entry.flags | XFS_ATTR_LOCAL,
                name: entry.name,
                value,
            });
        }
        Ok(attrs)
    }

    /// Walks a verified DA attribute node using attr-fork *logical* block
    /// addresses.  Node edges are never interpreted as device blocks: each
    /// edge must resolve through the inode mapping and every child is visited
    /// at most once, preventing a damaged cyclic tree from consuming memory.
    fn attribute_leaf_entries(
        &self,
        inode: u64,
        extents: &[XfsExtent],
        file_block: u64,
        expected_level: Option<u16>,
        seen: &mut Vec<u64>,
    ) -> XfsResult<Vec<XfsAttributeLeafEntry>> {
        if seen.len() >= 4096 || seen.contains(&file_block) {
            return Err(XfsError::CorruptMetadata);
        }
        seen.push(file_block);
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let physical = extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.read_data_fs_block(physical)?;
        let basic = physical
            .checked_mul((self.superblock.block_size as u64) / 512)
            .ok_or(XfsError::AddressOutOfRange)?;
        match XfsAttributeBlock::parse(&block, self.superblock.meta_uuid, inode, basic)? {
            XfsAttributeBlock::Leaf { entries, .. } => {
                if expected_level.is_some_and(|level| level != 0) {
                    return Err(XfsError::CorruptMetadata);
                }
                Ok(entries)
            }
            XfsAttributeBlock::Node { level, entries, .. } => {
                if expected_level.is_some_and(|expected| expected != level) {
                    return Err(XfsError::CorruptMetadata);
                }
                let mut all = Vec::new();
                for edge in entries {
                    let child = self.attribute_leaf_entries(
                        inode,
                        extents,
                        edge.address as u64,
                        Some(level.checked_sub(1).ok_or(XfsError::CorruptMetadata)?),
                        seen,
                    )?;
                    all.try_reserve(child.len())
                        .map_err(|_| XfsError::NoMemory)?;
                    all.extend(child);
                }
                all.sort_unstable_by_key(|entry| (entry.hash, entry.name.clone()));
                if all.windows(2).any(|pair| {
                    pair[0].hash == pair[1].hash
                        && pair[0].name == pair[1].name
                        && (pair[0].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                            == (pair[1].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                }) {
                    return Err(XfsError::CorruptMetadata);
                }
                Ok(all)
            }
        }
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn attribute_leaf_has_remote_values(&self, number: u64) -> XfsResult<bool> {
        let inode = self.inode(number)?;
        if inode.attr_format != XfsForkFormat::Extents {
            return Ok(false);
        }
        let extents = self.inode_attr_extents(number)?;
        let extent = extents
            .iter()
            .find(|extent| !extent.unwritten && extent.file_block == 0)
            .ok_or(XfsError::CorruptMetadata)?;
        let block = self.read_data_fs_block(extent.start_block)?;
        let basic = extent
            .start_block
            .checked_mul((self.superblock.block_size as u64) / 512)
            .ok_or(XfsError::AddressOutOfRange)?;
        match XfsAttributeBlock::parse(&block, self.superblock.meta_uuid, number, basic)? {
            XfsAttributeBlock::Leaf { entries, .. } => {
                Ok(entries.iter().any(|entry| entry.value_block != 0))
            }
            XfsAttributeBlock::Node { .. } => Ok(true),
        }
    }

    fn read_attribute_remote_value(
        &self,
        inode: u64,
        extents: &[XfsExtent],
        start_file_block: u64,
        length: usize,
    ) -> XfsResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        const RMT3_HEADER: usize = 56;
        const RMT2_HEADER: usize = 12;
        const RMT_MAGIC: u32 = 0x5841_524d;
        let header = if self.superblock.is_v5() {
            RMT3_HEADER
        } else {
            RMT2_HEADER
        };
        let fs = self.superblock.block_size as usize;
        let payload = fs.checked_sub(header).ok_or(XfsError::CorruptMetadata)?;
        let blocks = length.div_ceil(payload);
        let mut value = Vec::new();
        value
            .try_reserve_exact(length)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..blocks as u64 {
            let file_block = start_file_block
                .checked_add(index)
                .ok_or(XfsError::AddressOutOfRange)?;
            let extent = extents
                .iter()
                .find(|extent| {
                    !extent.unwritten
                        && file_block >= extent.file_block
                        && file_block < extent.file_block + extent.block_count as u64
                })
                .ok_or(XfsError::CorruptMetadata)?;
            let physical = extent.start_block + file_block - extent.file_block;
            let bytes = self.read_data_fs_block(physical)?;
            let basic = physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let offset = usize::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(payload))
                .ok_or(XfsError::AddressOutOfRange)?;
            let count = (length - offset).min(payload);
            if bytes.len() != fs
                || be32(&bytes, 0)? != RMT_MAGIC
                || be32(&bytes, 4)? as usize != offset
                || be32(&bytes, 8)? as usize != count
            {
                return Err(XfsError::CorruptMetadata);
            }
            if self.superblock.is_v5() {
                if be64(&bytes, 32)? != inode || be64(&bytes, 40)? != basic {
                    return Err(XfsError::CorruptMetadata);
                }
                let mut uuid = [0; 16];
                uuid.copy_from_slice(slice(&bytes, 16, 16)?);
                if XfsUuid(uuid) != self.superblock.meta_uuid {
                    return Err(XfsError::CorruptMetadata);
                }
                verify_crc32c(&bytes, 12)?;
            }
            value.extend_from_slice(slice(&bytes, header, count)?);
        }
        Ok(value)
    }

    /// Replaces an already allocated local attribute fork.  Attribute leaf,
    /// node, and remote-value transitions are intentionally not fabricated:
    /// callers receive an exact capacity error and retain the old fork.
    pub fn stage_shortform_xattrs(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.attr_format != XfsForkFormat::Local || inode.fork_offset == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.fork_offset as usize * 8;
        if fork_begin > raw.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = serialize_shortform_xattrs(attrs)?;
        if payload.len() > raw.len() - fork_begin {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut after = raw.clone();
        after[fork_begin..].fill(0);
        after[fork_begin..fork_begin + payload.len()].copy_from_slice(&payload);
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(&mut after, 76, 0)?;
        } else {
            put_be16(&mut after, 80, 0)?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages an inode link-count update without publishing it.  Namespace
    /// code must use this rather than updating a VFS-side counter: the count
    /// is part of the inode core and must reach the log with the directory
    /// mutation that created or removed the name.
    pub fn stage_inode_link_count(
        &self,
        number: u64,
        links: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode == 0 || links == 0 && inode.nlink == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut after = raw.clone();
        put_be32(&mut after, 16, links)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages the fixed inode-core attributes used by XFS fileattr ioctls.
    /// The CRC-covered dinode image, not a VFS cache, remains authoritative
    /// once the enclosing live-log transaction reaches its home checkpoint.
    pub(crate) fn stage_file_attr(
        &self,
        number: u64,
        attr: XfsFileAttr,
        ctime_seconds: i64,
        ctime_nanoseconds: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 || ctime_nanoseconds >= 1_000_000_000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut after = raw.clone();
        put_be16(&mut after, 20, attr.project_id as u16)?;
        put_be16(&mut after, 22, (attr.project_id >> 16) as u16)?;
        put_be32(&mut after, 72, attr.extent_size_hint)?;
        put_be16(&mut after, 90, attr.flags)?;
        put_be64(&mut after, 120, attr.flags2)?;
        put_be32(&mut after, 128, attr.cow_extent_size_hint)?;
        encode_inode_timestamp(
            &mut after,
            48,
            attr.flags2 & XfsInode::DIFLAG2_BIGTIME != 0,
            ctime_seconds,
            ctime_nanoseconds,
        )?;
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages one complete v3 dinode-core replacement.  All timestamp range
    /// checks happen before the image is admitted to `transaction`, and the
    /// v3 CRC covers the final combined mode/owner/time image.
    pub(crate) fn stage_inode_core_update(
        &self,
        number: u64,
        update: XfsInodeCoreUpdate,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 {
            return Err(XfsError::UnsupportedFeature);
        }

        for timestamp in [update.atime, update.mtime, update.ctime]
            .into_iter()
            .flatten()
        {
            if timestamp.1 >= 1_000_000_000 {
                return Err(XfsError::AddressOutOfRange);
            }
            // Run the native encoder on a disposable core image first.  This
            // validates legacy and BIGTIME bounds without publishing a
            // partial mutation when another requested field is unencodable.
            let mut probe = raw.clone();
            encode_inode_timestamp(
                &mut probe,
                32,
                inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0,
                timestamp.0,
                timestamp.1,
            )?;
        }

        let mut after = raw.clone();
        if let Some(mode) = update.mode {
            // MetadataUpdate carries permission/special bits, never a file
            // type.  Keep the on-disk type bits authoritative.
            put_be16(&mut after, 2, (inode.mode & !0o7777) | (mode & 0o7777))?;
        }
        if let Some((uid, gid)) = update.owner {
            put_be32(&mut after, 8, uid)?;
            put_be32(&mut after, 12, gid)?;
        }
        let bigtime = inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        if let Some((seconds, nanoseconds)) = update.atime {
            encode_inode_timestamp(&mut after, 32, bigtime, seconds, nanoseconds)?;
        }
        if let Some((seconds, nanoseconds)) = update.mtime {
            encode_inode_timestamp(&mut after, 40, bigtime, seconds, nanoseconds)?;
        }
        if let Some((seconds, nanoseconds)) = update.ctime {
            encode_inode_timestamp(&mut after, 48, bigtime, seconds, nanoseconds)?;
        }
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Builds a complete target replacement for the native symlink layouts
    /// this provider can prove: a local data fork or one written extent in
    /// the inode's AG.  BMBT/multi-AG symlinks are not coerced into a partial
    /// rewrite; callers receive `UnsupportedFeature` before any data write.
    fn stage_symlink_replacement(
        &self,
        number: u64,
        target: &[u8],
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<XfsMetadataTransaction> {
        if target.iter().any(|byte| *byte == 0) || nanoseconds >= 1_000_000_000 {
            return Err(XfsError::AddressOutOfRange);
        }
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 || inode.mode & 0o170000 != 0o120000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            (inode.fork_offset as usize)
                .checked_mul(8)
                .filter(|end| *end >= fork_begin && *end <= raw.len())
                .ok_or(XfsError::CorruptMetadata)?
        };
        let inline = fork_end
            .checked_sub(fork_begin)
            .ok_or(XfsError::CorruptMetadata)?;
        let (inode_ag, _) = self.split_inode_number(number)?;

        // Existing remote targets are restricted to the one-extent layout
        // emitted by `stage_new_inode`.  Keeping that proof also makes all
        // old data releases one AG allocator transaction.
        let old_remote = match inode.data_format {
            XfsForkFormat::Local => None,
            XfsForkFormat::Extents => {
                let extents = self.inode_data_extents(number)?;
                let extent = *extents.first().ok_or(XfsError::CorruptMetadata)?;
                if extents.len() != 1
                    || extent.unwritten
                    || extent.file_block != 0
                    || extent.start_block / u64::from(self.superblock.ag_blocks)
                        != u64::from(inode_ag)
                    || extent.start_block % u64::from(self.superblock.ag_blocks) < 4
                    || extent
                        .start_block
                        .checked_add(u64::from(extent.block_count))
                        .is_none_or(|end| {
                            end > (u64::from(inode_ag) + 1) * u64::from(self.superblock.ag_blocks)
                        })
                {
                    return Err(XfsError::UnsupportedFeature);
                }
                Some(extent)
            }
            _ => return Err(XfsError::UnsupportedFeature),
        };

        // Remote replacement never reuses the old target blocks: they remain
        // reachable until the new data has FUA-completed and the mapping
        // switch is durable.  This is the key failure-atomicity rule.
        let remote_blocks = (target.len() > inline)
            .then(|| {
                if inline < 16 {
                    return Err(XfsError::UnsupportedFeature);
                }
                u32::try_from(target.len().div_ceil(self.superblock.block_size as usize))
                    .map_err(|_| XfsError::AddressOutOfRange)
            })
            .transpose()?;
        let mut remote_start = None;
        let mut staged = if let Some(blocks) = remote_blocks {
            let snapshot = self.ag_ownership_snapshot(inode_ag)?;
            let allocation = snapshot
                .free_extents
                .iter()
                .filter(|extent| extent.block_count >= blocks)
                .min_by_key(|extent| (extent.block_count, extent.start_block))
                .copied()
                .ok_or(XfsError::AddressOutOfRange)?;
            remote_start = Some(allocation.start_block);
            let releases = old_remote
                .map(|extent| {
                    (0..extent.block_count)
                        .map(|offset| {
                            u32::try_from(
                                extent.start_block % u64::from(self.superblock.ag_blocks)
                                    + u64::from(offset),
                            )
                            .map_err(|_| XfsError::AddressOutOfRange)
                        })
                        .collect::<XfsResult<Vec<_>>>()
                })
                .transpose()?;
            let release = releases.as_deref().unwrap_or(&[]);
            self.stage_extent_delta(inode_ag, &[(allocation.start_block, blocks)], release)?
        } else if let Some(extent) = old_remote {
            let releases = (0..extent.block_count)
                .map(|offset| {
                    u32::try_from(
                        extent.start_block % u64::from(self.superblock.ag_blocks)
                            + u64::from(offset),
                    )
                    .map_err(|_| XfsError::AddressOutOfRange)
                })
                .collect::<XfsResult<Vec<_>>>()?;
            self.stage_extent_delta(inode_ag, &[], &releases)?
        } else {
            XfsMetadataTransaction::default()
        };

        let mut after = raw.clone();
        after[fork_begin..fork_end].fill(0);
        put_be64(
            &mut after,
            56,
            u64::try_from(target.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        let sector_unit = u64::from(self.superblock.block_size) / 512;
        let old_data_sectors = old_remote
            .map(|extent| {
                u64::from(extent.block_count)
                    .checked_mul(sector_unit)
                    .ok_or(XfsError::AddressOutOfRange)
            })
            .transpose()?
            .unwrap_or(0);
        let new_data_sectors = remote_blocks
            .map(|blocks| {
                u64::from(blocks)
                    .checked_mul(sector_unit)
                    .ok_or(XfsError::AddressOutOfRange)
            })
            .transpose()?
            .unwrap_or(0);
        // `di_nblocks` includes both data and attribute forks.  Replace only
        // the old symlink data contribution, retaining any xattr/attr-BMBT
        // ownership already represented in the raw inode core.
        let blocks = inode
            .blocks
            .checked_sub(old_data_sectors)
            .and_then(|base| base.checked_add(new_data_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        put_be64(&mut after, 64, blocks)?;
        let bigtime = inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        encode_inode_timestamp(&mut after, 40, bigtime, seconds, nanoseconds)?;
        encode_inode_timestamp(&mut after, 48, bigtime, seconds, nanoseconds)?;
        put_be64(
            &mut after,
            104,
            be64(&raw, 104)?
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?,
        )?;
        if let Some(blocks) = remote_blocks {
            let allocation = remote_start.ok_or(XfsError::CorruptMetadata)?;
            let first = u64::from(inode_ag)
                .checked_mul(u64::from(self.superblock.ag_blocks))
                .and_then(|base| base.checked_add(u64::from(allocation)))
                .ok_or(XfsError::AddressOutOfRange)?;
            after[5] = XfsForkFormat::Extents as u8;
            after[fork_begin..fork_begin + 16].copy_from_slice(&encode_xfs_extent(XfsExtent {
                unwritten: false,
                file_block: 0,
                start_block: first,
                block_count: blocks,
            })?);
            if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
                put_be64(&mut after, 24, 1)?;
            } else {
                put_be32(&mut after, 76, 1)?;
            }
            staged
                .data_writes
                .try_reserve_exact(blocks as usize)
                .map_err(|_| XfsError::NoMemory)?;
            let block_size = self.superblock.block_size as usize;
            for index in 0..u64::from(blocks) {
                let fs_block = first
                    .checked_add(index)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let begin = usize::try_from(index)
                    .map_err(|_| XfsError::AddressOutOfRange)?
                    .checked_mul(block_size)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let end = target.len().min(
                    begin
                        .checked_add(block_size)
                        .ok_or(XfsError::AddressOutOfRange)?,
                );
                let mut image = Vec::new();
                image
                    .try_reserve_exact(block_size)
                    .map_err(|_| XfsError::NoMemory)?;
                image.resize(block_size, 0);
                image[..end - begin].copy_from_slice(&target[begin..end]);
                staged.data_writes.push(XfsStagedDataWrite {
                    fs_block,
                    before: self.read_data_fs_block(fs_block)?,
                    after: image,
                });
            }
        } else {
            after[5] = XfsForkFormat::Local as u8;
            after[fork_begin..fork_begin + target.len()].copy_from_slice(target);
            if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
                put_be64(&mut after, 24, 0)?;
            } else {
                put_be32(&mut after, 76, 0)?;
            }
        }
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, &mut staged)?;
        Ok(staged)
    }

    /// Rewrites an existing native attribute leaf while retaining its mapped
    /// attribute-fork block.  A leaf overflow is deliberately reported before
    /// any buffer is staged so a future node split can reserve both children
    /// and the parent in one transaction rather than publishing a half-tree.
    pub fn stage_attribute_leaf(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode = self.inode(number)?;
        if inode.attr_format != XfsForkFormat::Extents {
            return Err(XfsError::UnsupportedFeature);
        }
        let extents = self.inode_attr_extents(number)?;
        let extent = extents
            .iter()
            .find(|extent| !extent.unwritten && extent.file_block == 0)
            .ok_or(XfsError::CorruptMetadata)?;
        let mut records = Vec::new();
        records
            .try_reserve_exact(attrs.len())
            .map_err(|_| XfsError::NoMemory)?;
        for attr in attrs {
            if attr.value.len() > u16::MAX as usize {
                return Err(XfsError::AddressOutOfRange);
            }
            records.push(XfsAttributeLeafEntry {
                hash: xfs_name_hash(&attr.name),
                flags: attr.flags | XFS_ATTR_LOCAL,
                name: attr.name.clone(),
                value: attr.value.clone(),
                value_block: 0,
                value_length: attr.value.len() as u32,
            });
        }
        let after = XfsAttributeBlock::serialize_leaf(
            &records,
            0,
            0,
            self.superblock.meta_uuid,
            number,
            extent
                .start_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            self.superblock.is_v5(),
            self.superblock.block_size as usize,
        )?;
        let before = self.read_data_fs_block(extent.start_block)?;
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Attribute,
            basic_block: extent
                .start_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before,
            after,
        });
        Ok(())
    }

    /// Rebuilds the complete native attribute DA image as one transaction.
    /// Large values are stored in ordinary attr-fork remote blocks; leaves
    /// only contain their logical start and exact byte length. Allocation,
    /// stale-block release, leaf/node replacement and inode-fork rewrite are
    /// staged together so no committed edge can name an unowned block.
    pub fn stage_attribute_values(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode = self.inode(number)?;
        if !matches!(
            inode.attr_format,
            XfsForkFormat::Local | XfsForkFormat::Extents | XfsForkFormat::Btree
        ) || inode.fork_offset == 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let fs = self.superblock.block_size as usize;
        let old_extents = if matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            self.inode_attr_extents(number)?
        } else {
            Vec::new()
        };
        let old_bmap_blocks = self.attr_bmbt_blocks(number)?;

        // Rebuild the DA image rather than attempting an in-place leaf edit.
        // This makes insert/replace/remove one transaction: a one-leaf tree
        // collapses to logical block zero, while a split promotes a node at
        // zero and assigns its leaves the stable logical range 1..N.  Remote
        // values are always after that range, so a committed node never
        // points at a block which has not also been mapped by the same fork
        // update.
        let mut records = Vec::new();
        records
            .try_reserve_exact(attrs.len())
            .map_err(|_| XfsError::NoMemory)?;
        for attr in attrs {
            if attr.value.len() > u32::MAX as usize {
                return Err(XfsError::AddressOutOfRange);
            }
            records.push(XfsAttributeLeafEntry {
                hash: xfs_name_hash(&attr.name),
                flags: attr.flags | XFS_ATTR_LOCAL,
                name: attr.name.clone(),
                value: attr.value.clone(),
                value_block: 0,
                value_length: attr.value.len() as u32,
            });
        }
        records.sort_unstable_by_key(|entry| {
            (
                entry.hash,
                entry.name.clone(),
                entry.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE),
            )
        });
        if records.windows(2).any(|pair| {
            pair[0].hash == pair[1].hash
                && pair[0].name == pair[1].name
                && (pair[0].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                    == (pair[1].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
        }) {
            return Err(XfsError::AddressOutOfRange);
        }
        let values: Vec<Vec<u8>> = records.iter().map(|entry| entry.value.clone()).collect();
        // A value is remote only when it cannot inhabit a leaf by itself.
        // Total leaf pressure is handled by partitioning, not by needlessly
        // converting ordinary small xattrs to remote form.
        let mut remote = vec![false; records.len()];
        for (index, record) in records.iter_mut().enumerate() {
            if XfsAttributeBlock::serialize_leaf(
                core::slice::from_ref(record),
                0,
                0,
                self.superblock.meta_uuid,
                number,
                0,
                self.superblock.is_v5(),
                fs,
            )
            .is_err()
            {
                remote[index] = true;
                record.flags &= !XFS_ATTR_LOCAL;
                record.value.clear();
                record.value_block = 1;
                if XfsAttributeBlock::serialize_leaf(
                    core::slice::from_ref(record),
                    0,
                    0,
                    self.superblock.meta_uuid,
                    number,
                    0,
                    self.superblock.is_v5(),
                    fs,
                )
                .is_err()
                {
                    return Err(XfsError::AddressOutOfRange);
                }
            }
        }
        let mut leaves = XfsAttributeBlock::partition_leaves(
            &records,
            self.superblock.meta_uuid,
            number,
            self.superblock.is_v5(),
            fs,
        )?;
        let leaf_count = leaves.len();
        let node_capacity = (fs
            .checked_sub(if self.superblock.is_v5() { 64 } else { 16 })
            .ok_or(XfsError::AddressOutOfRange)?)
            / 8;
        if node_capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        // Logical block zero is permanently the DA root.  Leaves retain the
        // compact 1..N range even when extra interior levels are needed;
        // those levels are allocated after the leaves and are linked upward
        // until the root fanout fits.
        let mut intermediate = Vec::<(u64, u16, Vec<XfsDirectoryLeafEntry>, u32, u32)>::new();
        let (root_level, root_entries, metadata_blocks) = if leaf_count == 1 {
            (0u16, Vec::new(), 1u64)
        } else {
            let mut current = Vec::new();
            current
                .try_reserve_exact(leaf_count)
                .map_err(|_| XfsError::NoMemory)?;
            for (index, leaf) in leaves.iter().enumerate() {
                current.push(XfsDirectoryLeafEntry {
                    hash: leaf.last().ok_or(XfsError::CorruptMetadata)?.hash,
                    address: u32::try_from(index + 1).map_err(|_| XfsError::AddressOutOfRange)?,
                });
            }
            let mut next_logical = 1u64
                .checked_add(u64::try_from(leaf_count).map_err(|_| XfsError::AddressOutOfRange)?)
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut level = 1u16;
            while current.len() > node_capacity {
                if level >= 5 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let groups = current.len().div_ceil(node_capacity);
                let first = next_logical;
                let mut parents = Vec::new();
                parents
                    .try_reserve_exact(groups)
                    .map_err(|_| XfsError::NoMemory)?;
                for group in 0..groups {
                    let start = group * node_capacity;
                    let end = (start + node_capacity).min(current.len());
                    let logical = first
                        .checked_add(group as u64)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    let entries = current[start..end].to_vec();
                    let hash = entries.last().ok_or(XfsError::CorruptMetadata)?.hash;
                    intermediate.push((
                        logical,
                        level,
                        entries,
                        if group + 1 == groups {
                            0
                        } else {
                            u32::try_from(
                                logical.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
                            )
                            .map_err(|_| XfsError::AddressOutOfRange)?
                        },
                        if group == 0 {
                            0
                        } else {
                            u32::try_from(logical - 1).map_err(|_| XfsError::AddressOutOfRange)?
                        },
                    ));
                    parents.push(XfsDirectoryLeafEntry {
                        hash,
                        address: u32::try_from(logical).map_err(|_| XfsError::AddressOutOfRange)?,
                    });
                }
                current = parents;
                next_logical = next_logical
                    .checked_add(u64::try_from(groups).map_err(|_| XfsError::AddressOutOfRange)?)
                    .ok_or(XfsError::AddressOutOfRange)?;
                level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            }
            (level, current, next_logical)
        };
        // Decide the on-inode direct/BMBT representation before reservations.
        // The BMBT allocation is intentionally included in the same extent
        // delta as leaf/remote blocks so the new root never names free space.
        let (inode_again, raw_again) = self.inode_and_bytes(number)?;
        let attr_bytes = inode_again.attr_fork(&raw_again)?.len();
        // Each remote value is one contiguous logical extent irrespective of
        // its physical block count.  The attr BMBT indexes mappings, not leaf
        // records; using `records.len()` here under-reserves the tree when a
        // sparse set of remote values expands the fork.
        let mapped_extents = 1usize
            .checked_add(remote.iter().filter(|remote| **remote).count())
            .ok_or(XfsError::AddressOutOfRange)?;
        let bmap_needed = bmap_external_blocks(self.superblock, attr_bytes, mapped_extents)?;
        let bmap_reused = old_bmap_blocks.len().min(bmap_needed);
        let bmap_fresh = bmap_needed
            .checked_sub(bmap_reused)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut requests = Vec::<u32>::new();
        requests.push(u32::try_from(metadata_blocks).map_err(|_| XfsError::AddressOutOfRange)?);
        let remote_payload = fs
            .checked_sub(if self.superblock.is_v5() { 56 } else { 12 })
            .ok_or(XfsError::AddressOutOfRange)?;
        for (index, _record) in records.iter().enumerate() {
            if remote[index] {
                requests.push(
                    u32::try_from(values[index].len().div_ceil(remote_payload))
                        .map_err(|_| XfsError::AddressOutOfRange)?
                        .max(1),
                );
            }
        }
        if bmap_fresh != 0 {
            requests.push(u32::try_from(bmap_fresh).map_err(|_| XfsError::AddressOutOfRange)?);
        }
        let (inode_ag, _) = self.split_inode_number(number)?;
        let batch = if requests.is_empty() {
            None
        } else {
            Some(self.prepare_extent_allocations(inode_ag, &requests)?)
        };
        let mut next_allocation = 0usize;
        let metadata_allocation = batch
            .as_ref()
            .and_then(|batch| batch.allocations.get(next_allocation))
            .ok_or(XfsError::CorruptMetadata)?;
        next_allocation += 1;
        if metadata_allocation.block_count as u64 != metadata_blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let metadata_physical = (metadata_allocation.ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(metadata_allocation.start_block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut extents = vec![XfsExtent {
            unwritten: false,
            file_block: 0,
            start_block: metadata_physical,
            block_count: metadata_allocation.block_count,
        }];
        let mut remote_writes = Vec::<(u64, Vec<u8>)>::new();
        let mut file_block = metadata_blocks;
        for (index, record) in records.iter_mut().enumerate() {
            if !remote[index] {
                continue;
            }
            let allocation = batch
                .as_ref()
                .and_then(|batch| batch.allocations.get(next_allocation))
                .ok_or(XfsError::CorruptMetadata)?;
            next_allocation += 1;
            let physical = (allocation.ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(allocation.start_block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            record.value_block =
                u32::try_from(file_block).map_err(|_| XfsError::AddressOutOfRange)?;
            record.value_length =
                u32::try_from(values[index].len()).map_err(|_| XfsError::AddressOutOfRange)?;
            extents.push(XfsExtent {
                unwritten: false,
                file_block,
                start_block: physical,
                block_count: allocation.block_count,
            });
            remote_writes.push((physical, values[index].clone()));
            file_block = file_block
                .checked_add(allocation.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        // Re-partition after remote addresses are final and install them in
        // the leaf copies. `partition_leaves` is deterministic by hash/name,
        // so indexing the records this way cannot alter the DA ordering.
        for leaf in &mut leaves {
            for entry in leaf {
                if entry.value_block == 0 {
                    continue;
                }
                let source = records
                    .iter()
                    .find(|record| {
                        record.hash == entry.hash
                            && record.name == entry.name
                            && (record.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                                == (entry.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                    })
                    .ok_or(XfsError::CorruptMetadata)?;
                entry.value_block = source.value_block;
                entry.value_length = source.value_length;
            }
        }
        let mut bmap_blocks = old_bmap_blocks[..bmap_reused].to_vec();
        if bmap_fresh != 0 {
            let allocation = batch
                .as_ref()
                .and_then(|batch| batch.allocations.get(next_allocation))
                .ok_or(XfsError::CorruptMetadata)?;
            next_allocation += 1;
            if allocation.block_count as usize != bmap_fresh {
                return Err(XfsError::CorruptMetadata);
            }
            let base = (allocation.ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(allocation.start_block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            bmap_blocks
                .try_reserve_exact(bmap_fresh)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..bmap_fresh {
                bmap_blocks.push(
                    base.checked_add(index as u64)
                        .ok_or(XfsError::AddressOutOfRange)?,
                );
            }
        }
        if next_allocation != batch.as_ref().map_or(0, |batch| batch.allocations.len()) {
            return Err(XfsError::CorruptMetadata);
        }
        // All old remote mappings are replaced.  Combine releases in the
        // allocation AG with the batch image; other AGs have no allocation
        // and can be independently rebuilt in this same journal record.
        let mut releases: Vec<(u32, Vec<u32>)> = Vec::new();
        for extent in &old_extents {
            for physical in extent.start_block
                ..extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .ok_or(XfsError::CorruptMetadata)?
            {
                let ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, blocks)) =
                    releases.iter_mut().find(|(candidate, _)| *candidate == ag)
                {
                    blocks.push(block);
                } else {
                    releases.push((ag, vec![block]));
                }
            }
        }
        let mut metadata_releases: Vec<(u32, Vec<u32>)> = Vec::new();
        for physical in old_bmap_blocks.iter().skip(bmap_reused).copied() {
            let ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, blocks)) = releases.iter_mut().find(|(candidate, _)| *candidate == ag) {
                blocks.push(block);
            } else {
                releases.push((ag, vec![block]));
            }
            if let Some((_, blocks)) = metadata_releases
                .iter_mut()
                .find(|(candidate, _)| *candidate == ag)
            {
                blocks.push(block);
            } else {
                metadata_releases.push((ag, vec![block]));
            }
        }
        let mut staged = transaction.clone();
        if let Some(batch) = &batch {
            let allocs: Vec<(u32, u32)> = batch
                .allocations
                .iter()
                .map(|allocation| (allocation.start_block, allocation.block_count))
                .collect();
            let released = releases
                .iter()
                .find(|(ag, _)| *ag == inode_ag)
                .map(|(_, blocks)| blocks.as_slice())
                .unwrap_or(&[]);
            let metadata = metadata_releases
                .iter()
                .find(|(ag, _)| *ag == inode_ag)
                .map(|(_, blocks)| blocks.as_slice())
                .unwrap_or(&[]);
            staged.buffers.extend(
                self.stage_extent_delta_with_metadata(inode_ag, &allocs, released, metadata)?
                    .buffers,
            );
        }
        for (ag, blocks) in &releases {
            if *ag != inode_ag {
                let metadata = metadata_releases
                    .iter()
                    .find(|(candidate, _)| *candidate == *ag)
                    .map(|(_, blocks)| blocks.as_slice())
                    .unwrap_or(&[]);
                staged.buffers.extend(
                    self.stage_extent_delta_with_metadata(*ag, &[], blocks, metadata)?
                        .buffers,
                );
            }
        }
        if bmap_needed == 0 {
            self.stage_attribute_fork_extents(number, &extents, &mut staged)?;
        } else {
            self.stage_attribute_fork_bmap(number, extents, &bmap_blocks, &mut staged)?;
        }
        if leaf_count == 1 {
            let basic = metadata_physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let leaf = XfsAttributeBlock::serialize_leaf(
                &leaves[0],
                0,
                0,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                fs,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Attribute,
                basic_block: basic,
                before: self.read_data_fs_block(metadata_physical)?,
                after: leaf,
            });
        } else {
            for index in 0..leaf_count {
                let logical = u32::try_from(index + 1).map_err(|_| XfsError::AddressOutOfRange)?;
                let physical = metadata_physical
                    .checked_add(logical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let forward = if index + 1 == leaf_count {
                    0
                } else {
                    logical.checked_add(1).ok_or(XfsError::AddressOutOfRange)?
                };
                let backward = if index == 0 { 0 } else { logical - 1 };
                let basic = physical
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let leaf = XfsAttributeBlock::serialize_leaf(
                    &leaves[index],
                    forward,
                    backward,
                    self.superblock.meta_uuid,
                    number,
                    basic,
                    self.superblock.is_v5(),
                    fs,
                )?;
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: leaf,
                });
            }
            for (logical, level, entries, forward, backward) in intermediate {
                let physical = metadata_physical
                    .checked_add(logical)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = physical
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let node = XfsAttributeBlock::serialize_node(
                    &entries,
                    forward,
                    backward,
                    level,
                    self.superblock.meta_uuid,
                    number,
                    basic,
                    self.superblock.is_v5(),
                    fs,
                )?;
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: node,
                });
            }
            let basic = metadata_physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let root = XfsAttributeBlock::serialize_node(
                &root_entries,
                0,
                0,
                root_level,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                fs,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Attribute,
                basic_block: basic,
                before: self.read_data_fs_block(metadata_physical)?,
                after: root,
            });
        }
        for (physical, value) in remote_writes {
            let remote_header = if self.superblock.is_v5() {
                56usize
            } else {
                12usize
            };
            let payload = fs
                .checked_sub(remote_header)
                .ok_or(XfsError::AddressOutOfRange)?;
            let blocks = value.len().div_ceil(payload);
            for index in 0..blocks {
                let mut after = vec![0; fs];
                let begin = index * payload;
                let end = (begin + payload).min(value.len());
                let block = physical
                    .checked_add(index as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                put_be32(&mut after, 0, 0x5841_524d)?;
                put_be32(
                    &mut after,
                    4,
                    u32::try_from(begin).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
                put_be32(
                    &mut after,
                    8,
                    u32::try_from(end - begin).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
                if self.superblock.is_v5() {
                    after[16..32].copy_from_slice(&self.superblock.meta_uuid.0);
                    put_be64(&mut after, 32, number)?;
                    put_be64(&mut after, 40, basic)?;
                    put_be64(&mut after, 48, 0)?;
                }
                after[remote_header..remote_header + end - begin]
                    .copy_from_slice(&value[begin..end]);
                if self.superblock.is_v5() {
                    rewrite_crc32c(&mut after, 12)?;
                }
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(block)?,
                    after,
                });
            }
        }
        *transaction = staged;
        Ok(())
    }

    fn stage_attribute_fork_extents(
        &self,
        number: u64,
        extents: &[XfsExtent],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        let begin = inode.fork_offset as usize * 8;
        if begin == 0
            || begin > raw.len()
            || extents
                .len()
                .checked_mul(16)
                .ok_or(XfsError::AddressOutOfRange)?
                > raw.len() - begin
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut previous = 0u64;
        for (index, extent) in extents.iter().enumerate() {
            if extent.block_count == 0
                || extent
                    .file_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .is_none()
                || (index != 0 && extent.file_block < previous)
            {
                return Err(XfsError::CorruptMetadata);
            }
            previous = extent.file_block + extent.block_count as u64;
        }
        let old_blocks = if matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            let mappings =
                self.inode_attr_extents(number)?
                    .iter()
                    .try_fold(0u64, |sum, extent| {
                        sum.checked_add(extent.block_count as u64)
                            .ok_or(XfsError::AddressOutOfRange)
                    })?;
            mappings
                .checked_add(self.attr_bmbt_blocks(number)?.len() as u64)
                .ok_or(XfsError::AddressOutOfRange)?
        } else {
            0
        };
        let new_blocks = extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?;
        let sector_unit = (self.superblock.block_size / 512) as u64;
        let old_sectors = old_blocks
            .checked_mul(sector_unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_sectors = new_blocks
            .checked_mul(sector_unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let sectors = inode
            .blocks
            .checked_sub(old_sectors)
            .and_then(|base| base.checked_add(new_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        let mut after = raw.clone();
        after[83] = XfsForkFormat::Extents as u8;
        after[begin..].fill(0);
        put_be64(&mut after, 64, sectors)?;
        for (index, extent) in extents.iter().enumerate() {
            after[begin + index * 16..begin + (index + 1) * 16]
                .copy_from_slice(&encode_xfs_extent(*extent)?);
        }
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        } else {
            put_be16(
                &mut after,
                80,
                u16::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Serializes an attribute-fork BMBT and its inode root.  The caller owns
    /// allocation/reclaim staging; this routine only consumes exactly that
    /// owned node set and updates `di_nblocks` with the mapping-tree blocks.
    fn stage_attribute_fork_bmap(
        &self,
        number: u64,
        mut extents: Vec<XfsExtent>,
        blocks: &[u64],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        let begin = inode.fork_offset as usize * 8;
        if begin == 0 || begin > raw.len() {
            return Err(XfsError::CorruptMetadata);
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let fork_bytes = raw.len() - begin;
        let needed = bmap_external_blocks(self.superblock, fork_bytes, extents.len())?;
        if needed == 0
            || blocks.len() != needed
            || blocks.iter().enumerate().any(|(index, block)| {
                *block >= self.superblock.data_blocks
                    || *block % (self.superblock.ag_blocks as u64) < 4
                    || blocks[..index].contains(block)
            })
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let header = if self.superblock.is_v5() {
            72usize
        } else {
            24usize
        };
        let leaf_capacity = (self.superblock.block_size as usize - header) / 16;
        let interior_capacity = (self.superblock.block_size as usize - header) / 16;
        let root_capacity = fork_bytes
            .checked_sub(4)
            .ok_or(XfsError::AddressOutOfRange)?
            / 16;
        if leaf_capacity == 0 || interior_capacity == 0 || root_capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut used = 0usize;
        let leaves = extents.len().div_ceil(leaf_capacity);
        let mut current = Vec::new();
        current
            .try_reserve_exact(leaves)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..leaves {
            let start = index * leaf_capacity;
            let end = (start + leaf_capacity).min(extents.len());
            let block = blocks[used];
            used += 1;
            let after = serialize_bmap_node(
                self.superblock,
                number,
                block,
                0,
                if index == 0 { 0 } else { blocks[used - 2] },
                if index + 1 == leaves { 0 } else { blocks[used] },
                &extents[start..end],
                &[],
            )?;
            let before = self.read_data_fs_block(block)?;
            transaction.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Btree,
                basic_block: block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?,
                before,
                after,
            });
            current.push((block, extents[start]));
        }
        let mut level = 1u16;
        while current.len() > root_capacity {
            let parents = current.len().div_ceil(interior_capacity);
            let mut next = Vec::new();
            next.try_reserve_exact(parents)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..parents {
                let start = index * interior_capacity;
                let end = (start + interior_capacity).min(current.len());
                let block = *blocks.get(used).ok_or(XfsError::AddressOutOfRange)?;
                used += 1;
                let keys = current[start..end]
                    .iter()
                    .map(|entry| entry.1)
                    .collect::<Vec<_>>();
                let children = current[start..end]
                    .iter()
                    .map(|entry| entry.0)
                    .collect::<Vec<_>>();
                let after = serialize_bmap_node(
                    self.superblock,
                    number,
                    block,
                    level,
                    if index == 0 { 0 } else { blocks[used - 2] },
                    if index + 1 == parents {
                        0
                    } else {
                        *blocks.get(used).ok_or(XfsError::AddressOutOfRange)?
                    },
                    &keys,
                    &children,
                )?;
                let before = self.read_data_fs_block(block)?;
                transaction.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Btree,
                    basic_block: block
                        .checked_mul((self.superblock.block_size as u64) / 512)
                        .ok_or(XfsError::AddressOutOfRange)?,
                    before,
                    after,
                });
                next.push((block, keys[0]));
            }
            current = next;
            level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        if used != blocks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let old_mapping_blocks =
            self.inode_attr_extents(number)?
                .iter()
                .try_fold(0u64, |sum, extent| {
                    sum.checked_add(extent.block_count as u64)
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
        let old_tree_blocks = self.attr_bmbt_blocks(number)?.len() as u64;
        let new_mapping_blocks = extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?;
        let new_tree_blocks = blocks.len() as u64;
        let sector_unit = (self.superblock.block_size / 512) as u64;
        let old_sectors = old_mapping_blocks
            .checked_add(old_tree_blocks)
            .and_then(|count| count.checked_mul(sector_unit))
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_sectors = new_mapping_blocks
            .checked_add(new_tree_blocks)
            .and_then(|count| count.checked_mul(sector_unit))
            .ok_or(XfsError::AddressOutOfRange)?;
        let sectors = inode
            .blocks
            .checked_sub(old_sectors)
            .and_then(|base| base.checked_add(new_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        let mut after = raw.clone();
        after[83] = XfsForkFormat::Btree as u8;
        after[begin..].fill(0);
        put_be16(&mut after, begin, level)?;
        put_be16(
            &mut after,
            begin + 2,
            u16::try_from(current.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        for (index, (child, key)) in current.iter().enumerate() {
            put_be64(&mut after, begin + 4 + index * 8, key.file_block)?;
            put_be64(
                &mut after,
                begin + 4 + root_capacity * 8 + index * 8,
                *child,
            )?;
        }
        put_be64(&mut after, 64, sectors)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        } else {
            put_be16(
                &mut after,
                80,
                u16::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Decodes the persistent `xfs_dev_t` carried by a character or block
    /// device inode. XFS stores Linux's packed 32-bit device representation,
    /// so the VFS can retain it directly as its `DeviceId` bit pattern.
    pub fn inode_rdev(&self, number: u64) -> XfsResult<u32> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !matches!(inode.mode & 0o170000, 0o020000 | 0o060000)
            || inode.data_format != XfsForkFormat::Device
        {
            return Err(XfsError::UnsupportedFeature);
        }
        be32(inode.data_fork(&raw)?, 0)
    }

    fn inode_and_bytes(&self, number: u64) -> XfsResult<(XfsInode, Vec<u8>)> {
        let (ag, agino) = self.split_inode_number(number)?;
        let inode_block = agino >> self.superblock.inodes_per_block_log;
        let inode_index = agino & (self.superblock.inodes_per_block as u64 - 1);
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|start| start.checked_add(inode_block))
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.read_data_fs_block(fs_block)?;
        let offset = (inode_index as usize)
            .checked_mul(self.superblock.inode_size as usize)
            .ok_or(XfsError::AddressOutOfRange)?;
        let raw = slice(&block, offset, self.superblock.inode_size as usize)?.to_vec();
        let inode = XfsInode::parse(
            number,
            &raw,
            self.superblock.is_v5().then_some(self.superblock.uuid),
            (self.superblock.features.incompat & XfsFeatures::INCOMPAT_META_UUID != 0)
                .then_some(self.superblock.meta_uuid),
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0,
        )?;
        Ok((inode, raw))
    }

    /// Resolves one mapped file block to its physical filesystem block after
    /// validating the inode's complete extent representation.  Metadata
    /// verifiers use the result to bind v5 blkno fields to their home.
    fn inode_physical_file_block(&self, number: u64, file_block: u64) -> XfsResult<u64> {
        let inode = self.inode(number)?;
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    fn decode_extent_fork(&self, fork: &[u8], expected: usize) -> XfsResult<Vec<XfsExtent>> {
        let bytes = expected.checked_mul(16).ok_or(XfsError::CorruptMetadata)?;
        if fork.len() < bytes {
            return Err(XfsError::CorruptMetadata);
        }
        let mut extents = Vec::new();
        extents
            .try_reserve_exact(expected)
            .map_err(|_| XfsError::NoMemory)?;
        let mut prior_end = 0u64;
        for index in 0..expected {
            let extent = XfsExtent::parse(slice(fork, index * 16, 16)?)?;
            if index != 0 && extent.file_block < prior_end {
                return Err(XfsError::CorruptMetadata);
            }
            prior_end = extent
                .file_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
            let physical_end = extent
                .start_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
            if physical_end > self.superblock.data_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            extents.push(extent);
        }
        Ok(extents)
    }

    fn split_inode_number(&self, number: u64) -> XfsResult<(u32, u64)> {
        let agino_bits =
            self.superblock.ag_block_log as u32 + self.superblock.inodes_per_block_log as u32;
        if agino_bits >= 63 {
            return Err(XfsError::InvalidSuperblock);
        }
        let ag = (number >> agino_bits) as u32;
        if ag >= self.superblock.ag_count {
            return Err(XfsError::AddressOutOfRange);
        }
        let agino_mask = (1u64 << agino_bits) - 1;
        Ok((ag, number & agino_mask))
    }

    fn read_data_fs_block(&self, block: u64) -> XfsResult<Vec<u8>> {
        if block >= self.superblock.data_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        self.read_from_volume(&self.data, block, self.superblock.block_size as usize)
    }

    /// Reads one filesystem-sized realtime block.  Realtime addressing is
    /// deliberately kept separate from the data-device AG address space.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn read_realtime_fs_block(&self, block: u64) -> XfsResult<Vec<u8>> {
        let volume = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if block >= self.superblock.realtime_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        self.read_from_volume(volume, block, self.superblock.block_size as usize)
    }

    /// Resolves a realtime bitmap/summary logical block through its metadata
    /// inode.  These inodes live on the data device; only the bits they
    /// contain describe allocation on the separate realtime member.
    fn realtime_metadata_block(
        &self,
        inode_number: u64,
        group: u32,
        file_block: u64,
    ) -> XfsResult<u64> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            let (bitmap, summary) = *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
            let inode_number = if inode_number == u64::MAX {
                bitmap
            } else if inode_number == u64::MAX - 1 {
                summary
            } else {
                return Err(XfsError::CorruptMetadata);
            };
            return self.inode_physical_file_block(inode_number, file_block);
        }
        let inode = self.inode(inode_number)?;
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn read_realtime_bitmap_block(&self, file_block: u64) -> XfsResult<Vec<u8>> {
        let inode = if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            u64::MAX
        } else {
            self.superblock.realtime_bitmap_inode
        };
        self.read_data_fs_block(self.realtime_metadata_block(inode, 0, file_block)?)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn read_realtime_summary_block(&self, file_block: u64) -> XfsResult<Vec<u8>> {
        let inode = if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            u64::MAX - 1
        } else {
            self.superblock.realtime_summary_inode
        };
        self.read_data_fs_block(self.realtime_metadata_block(inode, 0, file_block)?)
    }

    fn realtime_layout(&self, group: u32) -> XfsResult<(bool, u64, u64, u64)> {
        let rtgroups = self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0;
        let payload = u64::try_from(
            (self.superblock.block_size as usize)
                .checked_sub(realtime_payload_offset(rtgroups))
                .ok_or(XfsError::InvalidSuperblock)?,
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        let bits_per_block = payload.checked_mul(8).ok_or(XfsError::AddressOutOfRange)?;
        if rtgroups {
            if group >= self.superblock.rtgroup_count {
                return Err(XfsError::AddressOutOfRange);
            }
            let first = u64::from(group)
                .checked_mul(u64::from(self.superblock.rtgroup_extents))
                .ok_or(XfsError::AddressOutOfRange)?;
            let extents = (self.superblock.realtime_extents - first)
                .min(u64::from(self.superblock.rtgroup_extents));
            if extents == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            Ok((
                true,
                extents,
                bits_per_block,
                extents.div_ceil(bits_per_block),
            ))
        } else {
            if group != 0
                || self.superblock.realtime_extents == 0
                || self.superblock.realtime_bitmap_blocks == 0
            {
                return Err(XfsError::AddressOutOfRange);
            }
            let required = self.superblock.realtime_extents.div_ceil(bits_per_block);
            if required != u64::from(self.superblock.realtime_bitmap_blocks) {
                return Err(XfsError::CorruptMetadata);
            }
            Ok((
                false,
                self.superblock.realtime_extents,
                bits_per_block,
                required,
            ))
        }
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn stage_realtime_image(
        &self,
        physical: u64,
        before: Vec<u8>,
        after: Vec<u8>,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if before.len() != self.superblock.block_size as usize || after.len() != before.len() {
            return Err(XfsError::CorruptMetadata);
        }
        if let Some(existing) = transaction
            .realtime_writes
            .iter_mut()
            .find(|write| write.fs_block == physical)
        {
            if existing.before != before {
                return Err(XfsError::CorruptMetadata);
            }
            existing.after = after;
        } else {
            transaction.realtime_writes.push(XfsStagedDataWrite {
                fs_block: physical,
                before,
                after,
            });
        }
        Ok(())
    }

    fn verify_rtgroup_buffer(
        &self,
        bytes: &[u8],
        magic: u32,
        owner: u64,
        physical: u64,
    ) -> XfsResult<()> {
        let checksum_uuid =
            if self.superblock.features.incompat & XfsFeatures::INCOMPAT_META_UUID != 0 {
                self.superblock.meta_uuid
            } else {
                self.superblock.uuid
            };
        if be32(bytes, 0)? != magic
            || be64(bytes, 8)? != owner
            || be64(bytes, 16)?
                != physical
                    .checked_mul(u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            || slice(bytes, 32, 16)? != checksum_uuid.0
        {
            return Err(XfsError::CorruptMetadata);
        }
        verify_crc32c(bytes, 4)
    }

    /// Rebuild the complete rtsummary index for one allocation domain.  A
    /// summary counter is keyed by `(floor(log2(run length)), bitmap block
    /// containing run start)`, not by a longest-run approximation.  Scanning
    /// all bitmap blocks also handles a run spanning a bitmap-block boundary.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn materialize_realtime_summary(
        &self,
        group: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (rtgroups, extents, bits_per_block, bitmap_blocks) = self.realtime_layout(group)?;
        let levels = 64u64
            .checked_sub(extents.leading_zeros() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let slots = levels
            .checked_mul(bitmap_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        let words_per_block = (u64::from(self.superblock.block_size)
            - u64::try_from(realtime_payload_offset(rtgroups))
                .map_err(|_| XfsError::AddressOutOfRange)?)
            / 4;
        if words_per_block == 0 {
            return Err(XfsError::InvalidSuperblock);
        }
        let summary_blocks = slots.div_ceil(words_per_block);
        let bitmap_inode = if rtgroups {
            u64::MAX
        } else {
            self.superblock.realtime_bitmap_inode
        };
        let summary_inode = if rtgroups {
            u64::MAX - 1
        } else {
            self.superblock.realtime_summary_inode
        };
        let (bitmap_owner, summary_owner) = if rtgroups {
            *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::AddressOutOfRange)?
        } else {
            (bitmap_inode, summary_inode)
        };
        let mut bitmap = Vec::new();
        bitmap
            .try_reserve_exact(
                usize::try_from(bitmap_blocks).map_err(|_| XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        for logical in 0..bitmap_blocks {
            let physical = self.realtime_metadata_block(bitmap_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x424d_505a, bitmap_owner, physical)?;
            }
            let image = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or(before);
            bitmap.push(image);
        }
        let mut counters =
            vec![0u32; usize::try_from(slots).map_err(|_| XfsError::AddressOutOfRange)?];
        let mut bit = 0u64;
        while bit < extents {
            let block =
                usize::try_from(bit / bits_per_block).map_err(|_| XfsError::AddressOutOfRange)?;
            let local = bit % bits_per_block;
            if !realtime_bitmap_bit(&bitmap[block], local, rtgroups)? {
                bit += 1;
                continue;
            }
            let start = bit;
            loop {
                bit += 1;
                if bit == extents {
                    break;
                }
                let next_block = usize::try_from(bit / bits_per_block)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if !realtime_bitmap_bit(&bitmap[next_block], bit % bits_per_block, rtgroups)? {
                    break;
                }
            }
            let length = bit - start;
            let level = u64::from(63 - length.leading_zeros());
            let bitmap_block = start / bits_per_block;
            let slot = level
                .checked_mul(bitmap_blocks)
                .and_then(|base| base.checked_add(bitmap_block))
                .ok_or(XfsError::AddressOutOfRange)?;
            let counter = counters
                .get_mut(usize::try_from(slot).map_err(|_| XfsError::AddressOutOfRange)?)
                .ok_or(XfsError::CorruptMetadata)?;
            *counter = counter.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        for logical in 0..summary_blocks {
            let physical = self.realtime_metadata_block(summary_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x5355_4d59, summary_owner, physical)?;
            }
            let mut after = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or_else(|| before.clone());
            let start = logical
                .checked_mul(words_per_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = (start + words_per_block).min(slots);
            for slot in start..end {
                set_realtime_summary_counter(
                    &mut after,
                    usize::try_from(slot - start).map_err(|_| XfsError::AddressOutOfRange)?,
                    counters[usize::try_from(slot).map_err(|_| XfsError::AddressOutOfRange)?],
                    rtgroups,
                )?;
            }
            self.stage_realtime_image(physical, before, after, transaction)?;
        }
        Ok(())
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn stage_realtime_bitmap_delta(
        &self,
        first_bit: u64,
        count: u64,
        allocate: bool,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let rtgroups = self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0;
        let (_, _, bits, _) = self.realtime_layout(0)?;
        let end = first_bit
            .checked_add(count)
            .ok_or(XfsError::AddressOutOfRange)?;
        if count == 0 || end > self.superblock.realtime_extents {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut cursor = first_bit;
        while cursor < end {
            let group = if rtgroups {
                u32::try_from(cursor / u64::from(self.superblock.rtgroup_extents))
                    .map_err(|_| XfsError::AddressOutOfRange)?
            } else {
                0
            };
            let group_first = if rtgroups {
                u64::from(group)
                    .checked_mul(u64::from(self.superblock.rtgroup_extents))
                    .ok_or(XfsError::AddressOutOfRange)?
            } else {
                0
            };
            let (_, group_extents, _, _) = self.realtime_layout(group)?;
            let local_extent = cursor - group_first;
            let logical = local_extent / bits;
            let within = local_extent % bits;
            let take = (end - cursor)
                .min(bits - within)
                .min(group_extents - local_extent);
            let bitmap_inode = if rtgroups {
                u64::MAX
            } else {
                self.superblock.realtime_bitmap_inode
            };
            let physical = self.realtime_metadata_block(bitmap_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            let bitmap_owner = if rtgroups {
                self.rtgroup_inodes
                    .get(group as usize)
                    .ok_or(XfsError::AddressOutOfRange)?
                    .0
            } else {
                bitmap_inode
            };
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x424d_505a, bitmap_owner, physical)?;
            }
            let mut after = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or_else(|| before.clone());
            realtime_bitmap_range(&mut after, within, take, allocate, rtgroups)?;
            self.stage_realtime_image(physical, before, after, transaction)?;
            self.materialize_realtime_summary(group, transaction)?;
            cursor = cursor
                .checked_add(take)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(())
    }

    fn write_data_fs_block(&self, block: u64, bytes: &[u8]) -> XfsResult<()> {
        if block >= self.superblock.data_blocks
            || bytes.len() != self.superblock.block_size as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = self.data.geometry().block_size;
        if physical == 0 || bytes.len() % physical != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start = block
            .checked_mul((bytes.len() / physical) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.data
            .write_blocks_fua(start, bytes)
            .map_err(XfsError::from)
    }

    /// FUA-writes one realtime filesystem block.  Callers must flush the
    /// realtime member before making a data-device mapping durable.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    fn write_realtime_fs_block_fua(&self, block: u64, bytes: &[u8]) -> XfsResult<()> {
        let volume = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if block >= self.superblock.realtime_blocks
            || bytes.len() != self.superblock.block_size as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let basic = block
            .checked_mul(u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.write_basic_blocks_fua(volume, basic, bytes)
    }

    fn stage_inode_image(
        &self,
        number: u64,
        before_inode: Vec<u8>,
        after_inode: Vec<u8>,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if before_inode.len() != self.superblock.inode_size as usize
            || after_inode.len() != before_inode.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let (ag, agino) = self.split_inode_number(number)?;
        let inode_block = agino >> self.superblock.inodes_per_block_log;
        let inode_index = agino & (self.superblock.inodes_per_block as u64 - 1);
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(inode_block))
            .ok_or(XfsError::AddressOutOfRange)?;
        let before = self.read_data_fs_block(fs_block)?;
        let mut after = before.clone();
        let offset = (inode_index as usize)
            .checked_mul(self.superblock.inode_size as usize)
            .ok_or(XfsError::AddressOutOfRange)?;
        if before[offset..offset + before_inode.len()] != before_inode {
            return Err(XfsError::CorruptMetadata);
        }
        after[offset..offset + after_inode.len()].copy_from_slice(&after_inode);
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Inode,
            basic_block: fs_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before,
            after,
        });
        Ok(())
    }

    fn read_data_bytes(&self, byte_offset: u64) -> XfsResult<Vec<u8>> {
        let physical = self.data.geometry().block_size;
        if byte_offset % physical as u64 != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let block = byte_offset / physical as u64;
        let mut bytes = vec![0; physical];
        self.data
            .read_blocks(block, &mut bytes)
            .map_err(XfsError::from)?;
        Ok(bytes)
    }

    fn basic_blocks(&self, volume: &BlockVolume) -> XfsResult<u64> {
        let physical = volume.geometry().block_size;
        if physical == 0 || physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        volume
            .geometry()
            .blocks
            .checked_mul((physical / XFS_LOG_BASIC_BLOCK) as u64)
            .ok_or(XfsError::AddressOutOfRange)
    }

    /// Reads a byte image from the circular log address space.  The log's
    /// basic-block geometry is independent of its host volume's physical
    /// sector size, so both halves are routed through `read_basic_blocks`.
    fn read_log_ring_bytes(&self, start: u32, bytes: usize) -> XfsResult<Vec<u8>> {
        if bytes == 0 || bytes % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let region = self.log_region_blocks()?;
        if start >= region || bytes / XFS_LOG_BASIC_BLOCK > region as usize {
            return Err(XfsError::AddressOutOfRange);
        }
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let mut output = vec![0; bytes];
        let first_blocks = cmp::min(bytes / XFS_LOG_BASIC_BLOCK, (region - start) as usize);
        if first_blocks != 0 {
            self.read_basic_blocks(
                log,
                base.checked_add(start as u64)
                    .ok_or(XfsError::AddressOutOfRange)?,
                &mut output[..first_blocks * XFS_LOG_BASIC_BLOCK],
            )?;
        }
        if first_blocks * XFS_LOG_BASIC_BLOCK != bytes {
            self.read_basic_blocks(log, base, &mut output[first_blocks * XFS_LOG_BASIC_BLOCK..])?;
        }
        Ok(output)
    }

    /// Reads XFS 512-byte basic blocks through an arbitrary physical-sector
    /// BlockVolume.  Callers may use an unaligned log fragment; the complete
    /// enclosing sectors are always fetched before slicing.
    fn read_basic_blocks(
        &self,
        volume: &BlockVolume,
        basic_block: u64,
        output: &mut [u8],
    ) -> XfsResult<()> {
        if output.is_empty() || output.len() % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = volume.geometry().block_size;
        if physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start_byte = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let end_byte = start_byte
            .checked_add(output.len() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let first = start_byte / physical as u64;
        let last = end_byte.div_ceil(physical as u64);
        if last > volume.geometry().blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut whole = vec![
            0;
            usize::try_from(
                (last - first)
                    .checked_mul(physical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            )
            .map_err(|_| XfsError::AddressOutOfRange)?
        ];
        volume
            .read_blocks(first, &mut whole)
            .map_err(XfsError::from)?;
        let offset = usize::try_from(start_byte % physical as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        output.copy_from_slice(&whole[offset..offset + output.len()]);
        Ok(())
    }

    /// Writes basic blocks with FUA.  Aligned writes avoid a read-modify
    /// cycle; boundary fragments retain neighboring basic blocks exactly.
    fn write_basic_blocks_fua(
        &self,
        volume: &BlockVolume,
        basic_block: u64,
        input: &[u8],
    ) -> XfsResult<()> {
        if input.is_empty() || input.len() % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = volume.geometry().block_size;
        if physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start_byte = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let end_byte = start_byte
            .checked_add(input.len() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let first = start_byte / physical as u64;
        let last = end_byte.div_ceil(physical as u64);
        if last > volume.geometry().blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        if start_byte % physical as u64 == 0 && input.len() % physical == 0 {
            return volume
                .write_blocks_fua(first, input)
                .map_err(XfsError::from);
        }
        let mut whole = vec![
            0;
            usize::try_from(
                (last - first)
                    .checked_mul(physical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            )
            .map_err(|_| XfsError::AddressOutOfRange)?
        ];
        volume
            .read_blocks(first, &mut whole)
            .map_err(XfsError::from)?;
        let offset = usize::try_from(start_byte % physical as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        whole[offset..offset + input.len()].copy_from_slice(input);
        volume
            .write_blocks_fua(first, &whole)
            .map_err(XfsError::from)
    }

    fn read_from_volume(
        &self,
        volume: &BlockVolume,
        fs_block: u64,
        bytes: usize,
    ) -> XfsResult<Vec<u8>> {
        let physical = volume.geometry().block_size;
        if bytes == 0 || bytes % physical != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let blocks_per_fs_block = bytes / physical;
        let start = fs_block
            .checked_mul(blocks_per_fs_block as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut output = vec![0; bytes];
        volume
            .read_blocks(start, &mut output)
            .map_err(XfsError::from)?;
        Ok(output)
    }
}

/// Builds a complete, compact AG free-space B+tree from canonical extents.
/// Leaf splitting and parent promotion are iterative; the final singleton
/// parent is the root, while a single leaf naturally collapses the root.
/// Callers provide a verified pool containing old tree blocks followed by
/// AGFL blocks, making all growth/promotion consume freelist entries first.
fn build_free_tree(
    kind: XfsAgBtreeKind,
    ag: u32,
    sb: XfsSuperblock,
    extents: &[XfsAgFreeRecord],
    blocks: &[u32],
) -> XfsResult<(Vec<XfsAgBtreeNode>, usize)> {
    if !matches!(kind, XfsAgBtreeKind::ByBlock | XfsAgBtreeKind::ByLength) || extents.is_empty() {
        return Err(XfsError::AddressOutOfRange);
    }
    let header = if sb.is_v5() { 56usize } else { 16usize };
    let leaf_capacity = (sb.block_size as usize - header) / 8;
    let interior_capacity = (sb.block_size as usize - header) / 12;
    if leaf_capacity < 2 || interior_capacity < 2 {
        return Err(XfsError::InvalidSuperblock);
    }
    let mut ordered = extents.to_vec();
    if kind == XfsAgBtreeKind::ByLength {
        ordered.sort_unstable_by_key(|record| (record.block_count, record.start_block));
    }
    let leaf_count = ordered.len().div_ceil(leaf_capacity);
    let mut used = leaf_count;
    let mut current = Vec::new();
    current
        .try_reserve_exact(leaf_count)
        .map_err(|_| XfsError::NoMemory)?;
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(leaf_count)
        .map_err(|_| XfsError::NoMemory)?;
    for index in 0..leaf_count {
        let block = *blocks.get(index).ok_or(XfsError::AddressOutOfRange)?;
        let start = index * leaf_capacity;
        let end = (start + leaf_capacity).min(ordered.len());
        let records = ordered[start..end].to_vec();
        let key = free_tree_key(kind, records.first().ok_or(XfsError::CorruptMetadata)?);
        current.push((block, key));
        nodes.push(XfsAgBtreeNode {
            kind,
            ag,
            block,
            level: 0,
            left_sibling: if index == 0 { 0 } else { blocks[index - 1] },
            right_sibling: if index + 1 == leaf_count {
                0
            } else {
                *blocks.get(index + 1).ok_or(XfsError::AddressOutOfRange)?
            },
            records: XfsAgBtreeRecords::Free(records),
            children: Vec::new(),
        });
    }
    let mut level = 1u16;
    while current.len() > 1 {
        let parent_count = current.len().div_ceil(interior_capacity);
        let base = used;
        used = used
            .checked_add(parent_count)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut next = Vec::new();
        next.try_reserve_exact(parent_count)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..parent_count {
            let block = *blocks
                .get(base + index)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = index * interior_capacity;
            let end = (start + interior_capacity).min(current.len());
            let children = current[start..end]
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>();
            let keys = current[start..end]
                .iter()
                .map(|entry| entry.1)
                .collect::<Vec<_>>();
            next.push((block, keys[0]));
            nodes.push(XfsAgBtreeNode {
                kind,
                ag,
                block,
                level,
                left_sibling: if index == 0 {
                    0
                } else {
                    *blocks
                        .get(base + index - 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                right_sibling: if index + 1 == parent_count {
                    0
                } else {
                    *blocks
                        .get(base + index + 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                records: XfsAgBtreeRecords::Keys(keys),
                children,
            });
        }
        current = next;
        level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
    }
    // Keep root last.  `stage_free_space_trees` uses this invariant when
    // atomically replacing AGF roots after a promotion or collapse.
    let root = current[0].0;
    let index = nodes
        .iter()
        .position(|node| node.block == root)
        .ok_or(XfsError::CorruptMetadata)?;
    if index + 1 != nodes.len() {
        let root_node = nodes.remove(index);
        nodes.push(root_node);
    }
    Ok((nodes, used))
}

/// Inobt and finobt use identical 16-byte leaf records.  Keeping their
/// builder separate from free-space trees makes the root-level/record-key
/// contract explicit and prevents a finobt update from accidentally using
/// cntbt's `(length,start)` comparator.
fn build_inode_tree(
    kind: XfsAgBtreeKind,
    ag: u32,
    sb: XfsSuperblock,
    records: &[XfsAgInodeRecord],
    blocks: &[u32],
) -> XfsResult<(Vec<XfsAgBtreeNode>, usize)> {
    if !matches!(kind, XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode) || records.is_empty() {
        return Err(XfsError::AddressOutOfRange);
    }
    let header = if sb.is_v5() { 56usize } else { 16usize };
    let leaf_capacity = (sb.block_size as usize - header) / 16;
    let interior_capacity = (sb.block_size as usize - header) / 20;
    if leaf_capacity < 2 || interior_capacity < 2 {
        return Err(XfsError::InvalidSuperblock);
    }
    let mut ordered = records.to_vec();
    ordered.sort_unstable_by_key(|record| record.start_inode);
    if ordered
        .iter()
        .any(|record| record.free_count != record.free_mask.count_ones())
        || ordered
            .windows(2)
            .any(|pair| match pair[0].start_inode.checked_add(64) {
                Some(end) => end > pair[1].start_inode,
                None => true,
            })
    {
        return Err(XfsError::CorruptMetadata);
    }
    let leaves = ordered.len().div_ceil(leaf_capacity);
    let mut used = leaves;
    let mut current = Vec::new();
    let mut nodes = Vec::new();
    current
        .try_reserve_exact(leaves)
        .map_err(|_| XfsError::NoMemory)?;
    nodes
        .try_reserve_exact(leaves)
        .map_err(|_| XfsError::NoMemory)?;
    for index in 0..leaves {
        let block = *blocks.get(index).ok_or(XfsError::AddressOutOfRange)?;
        let start = index * leaf_capacity;
        let end = (start + leaf_capacity).min(ordered.len());
        let leaf = ordered[start..end].to_vec();
        let key = (leaf[0].start_inode, 0);
        current.push((block, key));
        nodes.push(XfsAgBtreeNode {
            kind,
            ag,
            block,
            level: 0,
            left_sibling: if index == 0 { 0 } else { blocks[index - 1] },
            right_sibling: if index + 1 == leaves {
                0
            } else {
                *blocks.get(index + 1).ok_or(XfsError::AddressOutOfRange)?
            },
            records: XfsAgBtreeRecords::Inode(leaf),
            children: Vec::new(),
        });
    }
    let mut level = 1u16;
    while current.len() > 1 {
        let parents = current.len().div_ceil(interior_capacity);
        let base = used;
        used = used
            .checked_add(parents)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut next = Vec::new();
        next.try_reserve_exact(parents)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..parents {
            let block = *blocks
                .get(base + index)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = index * interior_capacity;
            let end = (start + interior_capacity).min(current.len());
            let children = current[start..end]
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>();
            let keys = current[start..end]
                .iter()
                .map(|entry| entry.1)
                .collect::<Vec<_>>();
            next.push((block, keys[0]));
            nodes.push(XfsAgBtreeNode {
                kind,
                ag,
                block,
                level,
                left_sibling: if index == 0 {
                    0
                } else {
                    *blocks
                        .get(base + index - 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                right_sibling: if index + 1 == parents {
                    0
                } else {
                    *blocks
                        .get(base + index + 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                records: XfsAgBtreeRecords::Keys(keys),
                children,
            });
        }
        current = next;
        level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
    }
    let root = current[0].0;
    let index = nodes
        .iter()
        .position(|node| node.block == root)
        .ok_or(XfsError::CorruptMetadata)?;
    if index + 1 != nodes.len() {
        let root_node = nodes.remove(index);
        nodes.push(root_node);
    }
    Ok((nodes, used))
}

/// Builds the complete v5 rmapbt/refcountbt image from canonical leaves.
/// As with the allocator builders above, roots are kept last so an AGF
/// replacement can use the final node without depending on allocation order.
fn build_special_tree(
    kind: XfsAgSpecialBtreeKind,
    ag: u32,
    sb: XfsSuperblock,
    records: XfsAgSpecialBtreeRecords,
    blocks: &[u32],
) -> XfsResult<(Vec<XfsAgSpecialBtreeNode>, usize)> {
    let (leaf_width, key_width, count) = match (&records, kind) {
        (XfsAgSpecialBtreeRecords::Rmap(items), XfsAgSpecialBtreeKind::Rmap) => {
            (24usize, 24usize, items.len())
        }
        (XfsAgSpecialBtreeRecords::Refcount(items), XfsAgSpecialBtreeKind::Refcount) => {
            (12usize, 4usize, items.len())
        }
        _ => return Err(XfsError::CorruptMetadata),
    };
    if !sb.is_v5() || count == 0 {
        return Err(XfsError::AddressOutOfRange);
    }
    let leaf_capacity = (sb.block_size as usize)
        .checked_sub(56)
        .ok_or(XfsError::InvalidSuperblock)?
        / leaf_width;
    let interior_capacity = (sb.block_size as usize)
        .checked_sub(56)
        .ok_or(XfsError::InvalidSuperblock)?
        / key_width
            .checked_add(4)
            .ok_or(XfsError::InvalidSuperblock)?;
    if leaf_capacity < 2 || interior_capacity < 2 {
        return Err(XfsError::InvalidSuperblock);
    }
    let mut nodes = Vec::new();
    let mut current: Vec<(u32, XfsAgSpecialBtreeRecords)> = Vec::new();
    match records {
        XfsAgSpecialBtreeRecords::Rmap(mut values) => {
            values.sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
            if values.iter().any(|record| {
                record.block_count == 0
                    || record.start_block < 4
                    || record
                        .start_block
                        .checked_add(record.block_count)
                        .is_none_or(|end| end > sb.ag_blocks)
            }) || values.windows(2).any(|pair| {
                (pair[0].start_block, pair[0].owner, pair[0].offset)
                    == (pair[1].start_block, pair[1].owner, pair[1].offset)
            }) {
                return Err(XfsError::CorruptMetadata);
            }
            let leaves = values.len().div_ceil(leaf_capacity);
            current
                .try_reserve_exact(leaves)
                .map_err(|_| XfsError::NoMemory)?;
            nodes
                .try_reserve_exact(leaves)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..leaves {
                let block = *blocks.get(index).ok_or(XfsError::AddressOutOfRange)?;
                let start = index * leaf_capacity;
                let end = (start + leaf_capacity).min(values.len());
                let leaf = values[start..end].to_vec();
                current.push((block, XfsAgSpecialBtreeRecords::RmapKeys(vec![leaf[0]])));
                nodes.push(XfsAgSpecialBtreeNode {
                    kind,
                    ag,
                    block,
                    level: 0,
                    left_sibling: if index == 0 {
                        0
                    } else {
                        *blocks.get(index - 1).ok_or(XfsError::AddressOutOfRange)?
                    },
                    right_sibling: if index + 1 == leaves {
                        0
                    } else {
                        *blocks.get(index + 1).ok_or(XfsError::AddressOutOfRange)?
                    },
                    records: XfsAgSpecialBtreeRecords::Rmap(leaf),
                    children: Vec::new(),
                });
            }
        }
        XfsAgSpecialBtreeRecords::Refcount(mut values) => {
            values.sort_unstable_by_key(|record| record.start_block);
            if values.iter().any(|record| {
                record.block_count == 0
                    || record.refcount < 2
                    || record.start_block < 4
                    || record
                        .start_block
                        .checked_add(record.block_count)
                        .is_none_or(|end| end > sb.ag_blocks)
            }) || values.windows(2).any(|pair| {
                pair[0]
                    .start_block
                    .checked_add(pair[0].block_count)
                    .is_none_or(|end| end > pair[1].start_block)
            }) {
                return Err(XfsError::CorruptMetadata);
            }
            let leaves = values.len().div_ceil(leaf_capacity);
            current
                .try_reserve_exact(leaves)
                .map_err(|_| XfsError::NoMemory)?;
            nodes
                .try_reserve_exact(leaves)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..leaves {
                let block = *blocks.get(index).ok_or(XfsError::AddressOutOfRange)?;
                let start = index * leaf_capacity;
                let end = (start + leaf_capacity).min(values.len());
                let leaf = values[start..end].to_vec();
                current.push((
                    block,
                    XfsAgSpecialBtreeRecords::RefcountKeys(vec![leaf[0].start_block]),
                ));
                nodes.push(XfsAgSpecialBtreeNode {
                    kind,
                    ag,
                    block,
                    level: 0,
                    left_sibling: if index == 0 {
                        0
                    } else {
                        *blocks.get(index - 1).ok_or(XfsError::AddressOutOfRange)?
                    },
                    right_sibling: if index + 1 == leaves {
                        0
                    } else {
                        *blocks.get(index + 1).ok_or(XfsError::AddressOutOfRange)?
                    },
                    records: XfsAgSpecialBtreeRecords::Refcount(leaf),
                    children: Vec::new(),
                });
            }
        }
        _ => return Err(XfsError::CorruptMetadata),
    }
    let mut used = current.len();
    let mut level = 1u16;
    while current.len() > 1 {
        let parents = current.len().div_ceil(interior_capacity);
        let base = used;
        used = used
            .checked_add(parents)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut next = Vec::new();
        next.try_reserve_exact(parents)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..parents {
            let block = *blocks
                .get(base + index)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = index * interior_capacity;
            let end = (start + interior_capacity).min(current.len());
            let children = current[start..end]
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>();
            let keys = match kind {
                XfsAgSpecialBtreeKind::Rmap => {
                    let mut values = Vec::new();
                    for (_, key) in &current[start..end] {
                        let XfsAgSpecialBtreeRecords::RmapKeys(keys) = key else {
                            return Err(XfsError::CorruptMetadata);
                        };
                        values.push(keys[0]);
                    }
                    XfsAgSpecialBtreeRecords::RmapKeys(values)
                }
                XfsAgSpecialBtreeKind::Refcount => {
                    let mut values = Vec::new();
                    for (_, key) in &current[start..end] {
                        let XfsAgSpecialBtreeRecords::RefcountKeys(keys) = key else {
                            return Err(XfsError::CorruptMetadata);
                        };
                        values.push(keys[0]);
                    }
                    XfsAgSpecialBtreeRecords::RefcountKeys(values)
                }
            };
            let first = match &keys {
                XfsAgSpecialBtreeRecords::RmapKeys(values) => {
                    XfsAgSpecialBtreeRecords::RmapKeys(vec![values[0]])
                }
                XfsAgSpecialBtreeRecords::RefcountKeys(values) => {
                    XfsAgSpecialBtreeRecords::RefcountKeys(vec![values[0]])
                }
                _ => return Err(XfsError::CorruptMetadata),
            };
            next.push((block, first));
            nodes.push(XfsAgSpecialBtreeNode {
                kind,
                ag,
                block,
                level,
                left_sibling: if index == 0 {
                    0
                } else {
                    *blocks
                        .get(base + index - 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                right_sibling: if index + 1 == parents {
                    0
                } else {
                    *blocks
                        .get(base + index + 1)
                        .ok_or(XfsError::AddressOutOfRange)?
                },
                records: keys,
                children,
            });
        }
        current = next;
        level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
    }
    let root = current[0].0;
    let index = nodes
        .iter()
        .position(|node| node.block == root)
        .ok_or(XfsError::CorruptMetadata)?;
    if index + 1 != nodes.len() {
        let root_node = nodes.remove(index);
        nodes.push(root_node);
    }
    Ok((nodes, used))
}

fn free_tree_key(kind: XfsAgBtreeKind, record: &XfsAgFreeRecord) -> (u32, u32) {
    match kind {
        XfsAgBtreeKind::ByBlock => (record.start_block, record.block_count),
        XfsAgBtreeKind::ByLength => (record.block_count, record.start_block),
        XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode => (0, 0),
    }
}

fn encode_xfs_extent(extent: XfsExtent) -> XfsResult<[u8; 16]> {
    if extent.block_count == 0 || extent.file_block >= 1 << 54 || extent.start_block >= 1 << 52 {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut encoded = (extent.file_block as u128) << 73
        | (extent.start_block as u128) << 21
        | extent.block_count as u128;
    if extent.unwritten {
        encoded |= 1u128 << 127;
    }
    Ok(encoded.to_be_bytes())
}

fn serialize_shortform_directory(
    parent: u64,
    entries: &[XfsDirectoryEntry],
    has_ftype: bool,
    mut data_offset: usize,
) -> XfsResult<Vec<u8>> {
    if entries.len() > u8::MAX as usize {
        return Err(XfsError::AddressOutOfRange);
    }
    let wide =
        parent > u32::MAX as u64 || entries.iter().any(|entry| entry.inode > u32::MAX as u64);
    let width = if wide { 8usize } else { 4usize };
    let header = 2usize + width;
    let mut length = header;
    for entry in entries {
        if entry.inode == 0
            || entry.name.is_empty()
            || entry.name.len() > u8::MAX as usize
            || entry.name == b"."
            || entry.name == b".."
            || entry.name.iter().any(|byte| *byte == 0 || *byte == b'/')
        {
            return Err(XfsError::AddressOutOfRange);
        }
        if has_ftype && entry.file_type.is_none() {
            return Err(XfsError::CorruptMetadata);
        }
        length = length
            .checked_add(3 + entry.name.len() + width + usize::from(has_ftype))
            .ok_or(XfsError::AddressOutOfRange)?;
    }
    let mut out = vec![0; length];
    out[0] = entries.len() as u8;
    out[1] = if wide { entries.len() as u8 } else { 0 };
    if wide {
        put_be64(&mut out, 2, parent)?;
    } else {
        put_be32(
            &mut out,
            2,
            u32::try_from(parent).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
    }
    let mut cursor = header;
    for entry in entries.iter() {
        out[cursor] = entry.name.len() as u8;
        put_be16(
            &mut out,
            cursor + 1,
            u16::try_from(data_offset).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        cursor += 3;
        out[cursor..cursor + entry.name.len()].copy_from_slice(&entry.name);
        cursor += entry.name.len();
        if wide {
            put_be64(&mut out, cursor, entry.inode)?;
        } else {
            put_be32(
                &mut out,
                cursor,
                u32::try_from(entry.inode).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        cursor += width;
        if has_ftype {
            out[cursor] = entry.file_type.ok_or(XfsError::CorruptMetadata)?;
            cursor += 1;
        }
        let data_bytes = 11usize
            .checked_add(entry.name.len())
            .and_then(|bytes| bytes.checked_add(usize::from(has_ftype)))
            .and_then(|bytes| bytes.checked_add(7))
            .map(|bytes| bytes & !7)
            .ok_or(XfsError::AddressOutOfRange)?;
        data_offset = data_offset
            .checked_add(data_bytes)
            .ok_or(XfsError::AddressOutOfRange)?;
    }
    Ok(out)
}

fn serialize_shortform_xattrs(attrs: &[XfsShortformXattr]) -> XfsResult<Vec<u8>> {
    if attrs.len() > u8::MAX as usize {
        return Err(XfsError::AddressOutOfRange);
    }
    let mut length = 4usize;
    for attr in attrs {
        let namespace = attr.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE);
        if attr.flags & XFS_ATTR_LOCAL == 0
            || attr.flags & !(XFS_ATTR_LOCAL | XFS_ATTR_ROOT | XFS_ATTR_SECURE) != 0
            || namespace == (XFS_ATTR_ROOT | XFS_ATTR_SECURE)
            || attr.name.is_empty()
            || attr.name.len() > u8::MAX as usize
            || attr.value.len() > u8::MAX as usize
            || attr.name.iter().any(|byte| *byte == 0)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        length = length
            .checked_add(3 + attr.name.len() + attr.value.len())
            .ok_or(XfsError::AddressOutOfRange)?;
    }
    let total = u16::try_from(length).map_err(|_| XfsError::AddressOutOfRange)?;
    let mut out = vec![0; length];
    put_be16(&mut out, 0, total)?;
    out[2] = attrs.len() as u8;
    let mut cursor = 4usize;
    for attr in attrs {
        out[cursor] = attr.name.len() as u8;
        out[cursor + 1] = attr.value.len() as u8;
        out[cursor + 2] = attr.flags;
        cursor += 3;
        out[cursor..cursor + attr.name.len()].copy_from_slice(&attr.name);
        cursor += attr.name.len();
        out[cursor..cursor + attr.value.len()].copy_from_slice(&attr.value);
        cursor += attr.value.len();
    }
    Ok(out)
}

fn push_merged_extent(output: &mut Vec<XfsExtent>, extent: XfsExtent) -> XfsResult<()> {
    if extent.block_count == 0 {
        return Ok(());
    }
    if let Some(last) = output.last_mut()
        && last.unwritten == extent.unwritten
        && last.file_block.checked_add(last.block_count as u64) == Some(extent.file_block)
        && last.start_block.checked_add(last.block_count as u64) == Some(extent.start_block)
    {
        last.block_count = last
            .block_count
            .checked_add(extent.block_count)
            .ok_or(XfsError::AddressOutOfRange)?;
        return Ok(());
    }
    output.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
    output.push(extent);
    Ok(())
}

/// Calculates the number of external bmapbt blocks required when an inode
/// root no longer has room for all extent records.  The final level remains
/// in the inode fork; every lower leaf/interior level owns one allocated XFS
/// filesystem block.
fn bmap_external_blocks(sb: XfsSuperblock, fork_bytes: usize, records: usize) -> XfsResult<usize> {
    let root_capacity = fork_bytes
        .checked_sub(4)
        .ok_or(XfsError::AddressOutOfRange)?
        / 16;
    let header = if sb.is_v5() { 72usize } else { 24usize };
    let leaf_capacity = (sb.block_size as usize - header) / 16;
    let interior_capacity = (sb.block_size as usize - header) / 16;
    if root_capacity == 0 || leaf_capacity == 0 || interior_capacity == 0 {
        return Err(XfsError::AddressOutOfRange);
    }
    if records <= fork_bytes / 16 {
        return Ok(0);
    }
    let mut count = records.div_ceil(leaf_capacity);
    let mut blocks = count;
    while count > root_capacity {
        count = count.div_ceil(interior_capacity);
        blocks = blocks
            .checked_add(count)
            .ok_or(XfsError::AddressOutOfRange)?;
    }
    Ok(blocks)
}

fn serialize_bmap_node(
    sb: XfsSuperblock,
    inode: u64,
    block: u64,
    level: u16,
    left: u64,
    right: u64,
    records: &[XfsExtent],
    children: &[u64],
) -> XfsResult<Vec<u8>> {
    let header = if sb.is_v5() { 72usize } else { 24usize };
    let record_bytes = if level == 0 { 16usize } else { 8usize };
    let capacity =
        (sb.block_size as usize - header) / (record_bytes + if level == 0 { 0 } else { 8 });
    if records.is_empty()
        || records.len() > capacity
        || (level == 0 && !children.is_empty())
        || (level != 0 && children.len() != records.len())
    {
        return Err(XfsError::CorruptMetadata);
    }
    let mut bytes = vec![0; sb.block_size as usize];
    put_be32(
        &mut bytes,
        0,
        if sb.is_v5() {
            XFS_BMAP_CRC_MAGIC
        } else {
            XFS_BMAP_MAGIC
        },
    )?;
    put_be16(&mut bytes, 4, level)?;
    put_be16(
        &mut bytes,
        6,
        u16::try_from(records.len()).map_err(|_| XfsError::AddressOutOfRange)?,
    )?;
    put_be64(&mut bytes, 8, left)?;
    put_be64(&mut bytes, 16, right)?;
    if sb.is_v5() {
        put_be64(&mut bytes, 24, block)?;
        put_be64(&mut bytes, 32, 0)?;
        bytes[40..56].copy_from_slice(&sb.meta_uuid.0);
        put_be64(&mut bytes, 56, inode)?;
    }
    for (index, record) in records.iter().enumerate() {
        if level == 0 {
            bytes[header + index * 16..header + (index + 1) * 16]
                .copy_from_slice(&encode_xfs_extent(*record)?);
        } else {
            put_be64(&mut bytes, header + index * 8, record.file_block)?;
        }
    }
    if level != 0 {
        let base = header
            .checked_add(capacity.checked_mul(8).ok_or(XfsError::AddressOutOfRange)?)
            .ok_or(XfsError::AddressOutOfRange)?;
        for (index, child) in children.iter().enumerate() {
            put_be64(&mut bytes, base + index * 8, *child)?;
        }
    }
    if sb.is_v5() {
        rewrite_crc32c(&mut bytes, 64)?;
    }
    Ok(bytes)
}

fn byte(bytes: &[u8], offset: usize) -> XfsResult<u8> {
    bytes.get(offset).copied().ok_or(XfsError::CorruptMetadata)
}

fn slice(bytes: &[u8], offset: usize, len: usize) -> XfsResult<&[u8]> {
    let end = offset.checked_add(len).ok_or(XfsError::CorruptMetadata)?;
    bytes.get(offset..end).ok_or(XfsError::CorruptMetadata)
}

fn be16(bytes: &[u8], offset: usize) -> XfsResult<u16> {
    Ok(u16::from_be_bytes(
        slice(bytes, offset, 2)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

fn be32(bytes: &[u8], offset: usize) -> XfsResult<u32> {
    Ok(u32::from_be_bytes(
        slice(bytes, offset, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

fn be_i32(bytes: &[u8], offset: usize) -> XfsResult<i32> {
    Ok(i32::from_be_bytes(
        slice(bytes, offset, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

fn checked_nanoseconds(value: u32) -> XfsResult<u32> {
    if value >= 1_000_000_000 {
        return Err(XfsError::CorruptMetadata);
    }
    Ok(value)
}

/// XFS bigtime stores an unsigned nanosecond count from the beginning of the
/// signed-32-bit Unix-time range (1901-12-13), extending timestamps through
/// the year 2486 without changing inode size.
fn parse_inode_timestamp(bytes: &[u8], offset: usize, bigtime: bool) -> XfsResult<(i64, u32)> {
    if !bigtime {
        return Ok((
            be_i32(bytes, offset)? as i64,
            checked_nanoseconds(be32(bytes, offset + 4)?)?,
        ));
    }
    const BIGTIME_EPOCH_OFFSET: i64 = -2_147_483_648;
    let encoded = be64(bytes, offset)?;
    let seconds = ((encoded / 1_000_000_000) as i64)
        .checked_add(BIGTIME_EPOCH_OFFSET)
        .ok_or(XfsError::CorruptMetadata)?;
    let nanoseconds = (encoded % 1_000_000_000) as u32;
    Ok((seconds, nanoseconds))
}

/// Inverse of [`parse_inode_timestamp`].  Bigtime encodes an unsigned
/// nanosecond count relative to the legacy signed-32-bit epoch floor.
fn encode_inode_timestamp(
    bytes: &mut [u8],
    offset: usize,
    bigtime: bool,
    seconds: i64,
    nanoseconds: u32,
) -> XfsResult<()> {
    if nanoseconds >= 1_000_000_000 {
        return Err(XfsError::AddressOutOfRange);
    }
    if !bigtime {
        put_be32(
            bytes,
            offset,
            i32::try_from(seconds).map_err(|_| XfsError::AddressOutOfRange)? as u32,
        )?;
        return put_be32(bytes, offset + 4, nanoseconds);
    }
    const BIGTIME_EPOCH_OFFSET: i64 = -2_147_483_648;
    let relative = seconds
        .checked_sub(BIGTIME_EPOCH_OFFSET)
        .ok_or(XfsError::AddressOutOfRange)?;
    let encoded = u64::try_from(relative)
        .map_err(|_| XfsError::AddressOutOfRange)?
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(u64::from(nanoseconds)))
        .ok_or(XfsError::AddressOutOfRange)?;
    put_be64(bytes, offset, encoded)
}

/// Validates one little-endian CRC32c field in otherwise big-endian XFS
/// metadata.  The Castagnoli computation is deliberately local and
/// allocation-free, so metadata verification works before any VFS/cache
/// object has been admitted.  XFS stores the CRC field little-endian even in
/// structures whose numeric fields are big-endian.
fn verify_crc32c(bytes: &[u8], crc_offset: usize) -> XfsResult<()> {
    let stored = u32::from_le_bytes(
        slice(bytes, crc_offset, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    );
    let mut crc = !0u32;
    for (index, byte) in bytes.iter().enumerate() {
        let byte = if (crc_offset..crc_offset + 4).contains(&index) {
            0
        } else {
            *byte
        };
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((crc & 1 != 0) as u32 * 0x82f6_3b78);
        }
    }
    if crc != stored {
        return Err(XfsError::CorruptMetadata);
    }
    Ok(())
}

fn rewrite_crc32c(bytes: &mut [u8], crc_offset: usize) -> XfsResult<()> {
    slice(bytes, crc_offset, 4)?;
    bytes[crc_offset..crc_offset + 4].fill(0);
    let mut crc = !0u32;
    for byte in bytes.iter().copied() {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((crc & 1 != 0) as u32 * 0x82f6_3b78);
        }
    }
    bytes[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// XFS log-record CRCs deliberately omit 512-byte header padding.  The base
/// header has defined fields plus `h_pad0` through byte 327 (old i386 media
/// ends at 323); each 32 KiB window contributes its complete 260-byte
/// extension header, and the final segment is the declared log-operation
/// payload (not its basic-block alignment padding).
fn log_record_crc32c(
    bytes: &[u8],
    header: &XfsLogRecordHeader,
    base_bytes: usize,
) -> XfsResult<u32> {
    let header_bytes = header.header_bytes()?;
    let payload_end = header_bytes
        .checked_add(header.payload_bytes as usize)
        .ok_or(XfsError::CorruptMetadata)?;
    if !(base_bytes == 324 || base_bytes == 328)
        || bytes.len() < payload_end
        || header_bytes < XFS_LOG_BASIC_BLOCK
    {
        return Err(XfsError::CorruptMetadata);
    }
    let mut crc = !0u32;
    for (index, byte) in slice(bytes, 0, base_bytes)?.iter().copied().enumerate() {
        crc = crc32c_step(
            crc,
            if (XfsLogRecordHeader::CRC_OFFSET..XfsLogRecordHeader::CRC_OFFSET + 4).contains(&index)
            {
                0
            } else {
                byte
            },
        );
    }
    let extension_count = (header.payload_bytes as usize)
        .div_ceil(32 * 1024)
        .saturating_sub(1);
    if extension_count > header_bytes / XFS_LOG_BASIC_BLOCK - 1 {
        return Err(XfsError::CorruptMetadata);
    }
    for extension in 0..extension_count {
        let offset = (extension + 1) * XFS_LOG_BASIC_BLOCK;
        for byte in slice(bytes, offset, 260)?.iter().copied() {
            crc = crc32c_step(crc, byte);
        }
    }
    for byte in slice(bytes, header_bytes, header.payload_bytes as usize)?
        .iter()
        .copied()
    {
        crc = crc32c_step(crc, byte);
    }
    Ok(crc)
}

fn verify_log_record_crc(bytes: &[u8], header: &XfsLogRecordHeader) -> XfsResult<()> {
    let stored = u32::from_le_bytes(
        slice(bytes, XfsLogRecordHeader::CRC_OFFSET, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    );
    if log_record_crc32c(bytes, header, 328)? != stored
        && log_record_crc32c(bytes, header, 324)? != stored
    {
        return Err(XfsError::CorruptMetadata);
    }
    Ok(())
}

fn rewrite_log_record_crc(bytes: &mut [u8], header: &XfsLogRecordHeader) -> XfsResult<()> {
    let crc = log_record_crc32c(bytes, header, 328)?;
    bytes[XfsLogRecordHeader::CRC_OFFSET..XfsLogRecordHeader::CRC_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn crc32c_step(mut crc: u32, byte: u8) -> u32 {
    crc ^= byte as u32;
    for _ in 0..8 {
        crc = (crc >> 1) ^ ((crc & 1 != 0) as u32 * 0x82f6_3b78);
    }
    crc
}

fn be64(bytes: &[u8], offset: usize) -> XfsResult<u64> {
    Ok(u64::from_be_bytes(
        slice(bytes, offset, 8)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

fn put_be32(bytes: &mut [u8], offset: usize, value: u32) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be16(bytes: &mut [u8], offset: usize, value: u16) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(2).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be64(bytes: &mut [u8], offset: usize, value: u64) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(8).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn native_u32(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u32> {
    let raw: [u8; 4] = slice(bytes, offset, 4)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u32::from_le_bytes(raw),
        XfsLogByteOrder::Big => u32::from_be_bytes(raw),
    })
}
fn native_u16(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u16> {
    let raw: [u8; 2] = slice(bytes, offset, 2)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u16::from_le_bytes(raw),
        XfsLogByteOrder::Big => u16::from_be_bytes(raw),
    })
}
fn native_u64(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u64> {
    let raw: [u8; 8] = slice(bytes, offset, 8)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u64::from_le_bytes(raw),
        XfsLogByteOrder::Big => u64::from_be_bytes(raw),
    })
}
fn native_put_u16(
    bytes: &mut [u8],
    offset: usize,
    value: u16,
    order: XfsLogByteOrder,
) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(2).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&match order {
        XfsLogByteOrder::Little => value.to_le_bytes(),
        XfsLogByteOrder::Big => value.to_be_bytes(),
    });
    Ok(())
}
fn native_put_u32(
    bytes: &mut [u8],
    offset: usize,
    value: u32,
    order: XfsLogByteOrder,
) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&match order {
        XfsLogByteOrder::Little => value.to_le_bytes(),
        XfsLogByteOrder::Big => value.to_be_bytes(),
    });
    Ok(())
}
fn native_put_u64(
    bytes: &mut [u8],
    offset: usize,
    value: u64,
    order: XfsLogByteOrder,
) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(8).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&match order {
        XfsLogByteOrder::Little => value.to_le_bytes(),
        XfsLogByteOrder::Big => value.to_be_bytes(),
    });
    Ok(())
}

fn align8(value: usize) -> Option<usize> {
    value.checked_add(7).map(|value| value & !7)
}

fn align_log_basic_block(value: usize) -> XfsResult<usize> {
    value
        .checked_add(XFS_LOG_BASIC_BLOCK - 1)
        .map(|value| value & !(XFS_LOG_BASIC_BLOCK - 1))
        .ok_or(XfsError::AddressOutOfRange)
}

fn log_cycle_data_offset(record: &[u8], data_block: usize) -> XfsResult<usize> {
    let window = data_block / XFS_LOG_MAX_INLINE_CYCLE_DATA;
    let entry = data_block % XFS_LOG_MAX_INLINE_CYCLE_DATA;
    let offset = if window == 0 {
        44usize.checked_add(entry.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
    } else {
        window
            .checked_mul(XFS_LOG_BASIC_BLOCK)
            .and_then(|base| base.checked_add(4))
            .and_then(|base| base.checked_add(entry.checked_mul(4)?))
    }
    .ok_or(XfsError::AddressOutOfRange)?;
    slice(record, offset, 4)?;
    Ok(offset)
}

fn log_cycle_data_get(record: &[u8], data_block: usize) -> XfsResult<u32> {
    be32(record, log_cycle_data_offset(record, data_block)?)
}

fn log_cycle_data_put(record: &mut [u8], data_block: usize, value: u32) -> XfsResult<()> {
    let offset = log_cycle_data_offset(record, data_block)?;
    put_be32(record, offset, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-ramdisk")]
    use axdriver::{
        AxBlockDevice, BlockFaultLifetime, BlockFaultOperation, BlockFaultRule, SharedBlockDevice,
    };
    #[cfg(feature = "test-ramdisk")]
    use axdriver_block::ramdisk::RamDisk;

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
