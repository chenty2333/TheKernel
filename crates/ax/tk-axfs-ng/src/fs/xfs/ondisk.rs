//! XFS superblock, allocation-group headers and AG B+tree records.

use super::*;

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

    pub(super) fn validate(&self) -> XfsResult<()> {
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
    pub(super) fn parse(bytes: &[u8], features: XfsFeatures, crc_enabled: bool) -> XfsResult<Self> {
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

    pub(super) fn serialize(self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
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
    pub(super) fn parse(bytes: &[u8], crc_enabled: bool) -> XfsResult<Self> {
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

    pub(super) fn serialize(self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
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
    pub(super) fn header_and_capacity(
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

    pub(super) fn expected_magic(kind: XfsAgSpecialBtreeKind) -> u32 {
        match kind {
            XfsAgSpecialBtreeKind::Rmap => XFS_RMAP_CRC_MAGIC,
            XfsAgSpecialBtreeKind::Refcount => XFS_REFCOUNT_CRC_MAGIC,
        }
    }

    pub(super) fn parse(
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

    pub(super) fn record_len(&self) -> usize {
        match &self.records {
            XfsAgSpecialBtreeRecords::Rmap(items) => items.len(),
            XfsAgSpecialBtreeRecords::Refcount(items) => items.len(),
            XfsAgSpecialBtreeRecords::RmapKeys(items) => items.len(),
            XfsAgSpecialBtreeRecords::RefcountKeys(items) => items.len(),
        }
    }

    pub(super) fn serialize(&self, sb: XfsSuperblock, lsn: u64) -> XfsResult<Vec<u8>> {
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
    pub(super) fn parse(
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

    pub(super) fn records_len(&self) -> usize {
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
pub(super) struct XfsAgfl {
    pub(super) sequence: u32,
    pub(super) uuid: XfsUuid,
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
    pub(super) fn parse(bytes: &[u8], crc_enabled: bool) -> XfsResult<Self> {
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

    pub(super) fn serialize(
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
