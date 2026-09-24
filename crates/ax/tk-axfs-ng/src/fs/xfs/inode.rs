//! XFS inode cores, forks, quota records and export handles.

use super::*;

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
    pub(super) core_bytes: u16,
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
    pub(super) quota_type_flags: u8,
    pub block_hard: u64,
    pub block_soft: u64,
    pub inode_hard: u64,
    pub inode_soft: u64,
    pub realtime_hard: u64,
    pub realtime_soft: u64,
    pub blocks: u64,
    pub inodes: u64,
    pub realtime_blocks: u64,
    pub(super) inode_timer: u32,
    pub(super) block_timer: u32,
    pub(super) realtime_timer: u32,
    pub(super) inode_warnings: u16,
    pub(super) block_warnings: u16,
    pub(super) realtime_warnings: u16,
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
pub(super) struct XfsDquotAdmission {
    pub(super) blocks: u64,
    pub(super) inodes: u64,
    pub(super) block_timer: u32,
    pub(super) inode_timer: u32,
    pub(super) block_warnings: u16,
    pub(super) inode_warnings: u16,
}

/// A retained inode object for the future VFS adapter.  It carries its volume
/// explicitly, so object operations cannot cross mounts accidentally.
#[derive(Clone)]
pub struct XfsNode {
    pub(super) volume: Arc<XfsVolume>,
    pub(super) inode: XfsInode,
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
    pub(super) const DIFLAG2_BIGTIME: u64 = 1 << 3;
    pub(super) const DIFLAG2_NREXT64: u64 = 1 << 4;
    pub(super) const DIFLAG2_METADATA: u64 = 1 << 5;

    pub(super) fn is_metadata_inode(&self) -> bool {
        self.version >= 3 && self.flags2 & Self::DIFLAG2_METADATA != 0
    }

    pub(super) fn parse(
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

    pub(super) fn data_fork<'a>(&self, bytes: &'a [u8]) -> XfsResult<&'a [u8]> {
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

    pub(super) fn attr_fork<'a>(&self, bytes: &'a [u8]) -> XfsResult<&'a [u8]> {
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
