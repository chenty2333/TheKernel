//! Checked decoders for the Btrfs item payloads needed by namespace and file
//! I/O.  These types are intentionally value objects: all tree/block bounds
//! and checksums are established before a payload reaches this module.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, Metadata, NodePermission, NodeType, Timestamp};

use super::crc32c_seed;

/// Item-type values in the Btrfs key space used by the mounted namespace.
pub const INODE_ITEM: u8 = 1;
pub const INODE_REF: u8 = 12;
pub const INODE_EXTREF: u8 = 13;
pub const XATTR_ITEM: u8 = 24;
pub const ORPHAN_ITEM: u8 = 48;
pub const DIR_LOG_ITEM: u8 = 60;
pub const DIR_LOG_INDEX: u8 = 72;
pub const DIR_ITEM: u8 = 84;
pub const DIR_INDEX: u8 = 96;
pub const EXTENT_DATA: u8 = 108;
pub const CSUM_ITEM: u8 = 128;
pub const ROOT_ITEM: u8 = 132;
pub const FREE_SPACE_INFO: u8 = 198;
pub const FREE_SPACE_EXTENT: u8 = 199;
pub const FREE_SPACE_BITMAP: u8 = 200;
// On-disk item type kept for the complete writer-side format table.
#[allow(dead_code)]
pub const DEV_EXTENT: u8 = 204;
pub const DEV_ITEM: u8 = 216;
// On-disk item type kept for the complete writer-side format table.
#[allow(dead_code)]
pub const CHUNK_ITEM: u8 = 228;
pub const EXTENT_ITEM: u8 = 168;
pub const TREE_BLOCK_REF: u8 = 176;
pub const EXTENT_DATA_REF: u8 = 178;
// On-disk item type kept for the complete writer-side format table.
#[allow(dead_code)]
pub const QGROUP_STATUS: u8 = 0xF0;
pub const QGROUP_INFO: u8 = 0xF2;
pub const QGROUP_LIMIT: u8 = 0xF4;
pub const QGROUP_RELATION: u8 = 0xF6;

/// Native `btrfs_extref_hash(parent_objectid, name, len)`.  The on-media key
/// offset is a u64, while the Linux helper stores the raw seeded CRC-32C
/// result in that field; using a plain name CRC loses the parent namespace
/// and aliases unrelated extended references.
pub fn btrfs_extref_hash(parent_objectid: u64, name: &[u8]) -> u64 {
    u64::from(crc32c_seed(parent_objectid as u32, name))
}

const INODE_ITEM_BYTES: usize = 160;
const ROOT_ITEM_GENERATION: usize = INODE_ITEM_BYTES;
const ROOT_ITEM_ROOT_DIRID: usize = ROOT_ITEM_GENERATION + 8;
const ROOT_ITEM_BYTENR: usize = INODE_ITEM_BYTES + 16;
// The packed root item has `refs` at 216, then its 17-byte `drop_progress`
// key at 220 and `drop_level` at 237, so `level` is byte 238.
const ROOT_ITEM_LEVEL: usize = INODE_ITEM_BYTES + 78;
const DIR_ITEM_HEADER_BYTES: usize = 30;
const FILE_EXTENT_HEADER_BYTES: usize = 21;
const DEV_ITEM_BYTES: usize = 98;

/// Native tree-log directory-index deletion range.  The key identifies the
/// parent and inclusive first index; the payload is exactly the inclusive
/// last index.  Keeping the range typed prevents an empty/short log item
/// from being mistaken for a request to remove an arbitrary directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtrfsDirLogRange {
    pub first: u64,
    pub last: u64,
}

impl BtrfsDirLogRange {
    pub fn decode(first: u64, bytes: &[u8]) -> AxResult<Self> {
        if first == 0 || bytes.len() != 8 {
            return Err(AxError::Io);
        }
        let last = le64(bytes, 0)?;
        if last < first {
            return Err(AxError::Io);
        }
        Ok(Self { first, last })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtrfsInodeItem {
    pub generation: u64,
    pub transid: u64,
    pub size: u64,
    pub nbytes: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub rdev: u64,
    pub flags: u64,
    pub sequence: u64,
    /// Btrfs stores the Linux project ID in the first reserved inode-item
    /// word.  Keeping it native avoids a private project-id xattr.
    pub project_id: u32,
    pub atime: Timestamp,
    pub ctime: Timestamp,
    pub mtime: Timestamp,
    pub otime: Timestamp,
}

/// Checked native `btrfs_dev_item`.  UUID bytes are retained exactly so a
/// topology transaction cannot quietly reassign a member identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtrfsDeviceItem {
    pub devid: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub io_align: u32,
    pub io_width: u32,
    pub sector_size: u32,
    pub kind: u64,
    pub generation: u64,
    pub start_offset: u64,
    pub dev_group: u32,
    pub seek_speed: u8,
    pub bandwidth: u8,
    pub uuid: [u8; 16],
    pub fsid: [u8; 16],
}

impl BtrfsDeviceItem {
    pub fn decode(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() != DEV_ITEM_BYTES {
            return Err(AxError::Io);
        }
        let total_bytes = le64(bytes, 8)?;
        let bytes_used = le64(bytes, 16)?;
        if le64(bytes, 0)? == 0 || total_bytes == 0 || bytes_used > total_bytes {
            return Err(AxError::Io);
        }
        Ok(Self {
            devid: le64(bytes, 0)?,
            total_bytes,
            bytes_used,
            io_align: le32(bytes, 24)?,
            io_width: le32(bytes, 28)?,
            sector_size: le32(bytes, 32)?,
            kind: le64(bytes, 36)?,
            generation: le64(bytes, 44)?,
            start_offset: le64(bytes, 52)?,
            dev_group: le32(bytes, 60)?,
            seek_speed: bytes[64],
            bandwidth: bytes[65],
            uuid: bytes[66..82].try_into().map_err(|_| AxError::Io)?,
            fsid: bytes[82..98].try_into().map_err(|_| AxError::Io)?,
        })
    }
    pub fn encode(self) -> AxResult<Vec<u8>> {
        if self.devid == 0 || self.total_bytes == 0 || self.bytes_used > self.total_bytes {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = alloc::vec![0; DEV_ITEM_BYTES];
        put64(&mut bytes, 0, self.devid);
        put64(&mut bytes, 8, self.total_bytes);
        put64(&mut bytes, 16, self.bytes_used);
        put32(&mut bytes, 24, self.io_align);
        put32(&mut bytes, 28, self.io_width);
        put32(&mut bytes, 32, self.sector_size);
        put64(&mut bytes, 36, self.kind);
        put64(&mut bytes, 44, self.generation);
        put64(&mut bytes, 52, self.start_offset);
        put32(&mut bytes, 60, self.dev_group);
        bytes[64] = self.seek_speed;
        bytes[65] = self.bandwidth;
        bytes[66..82].copy_from_slice(&self.uuid);
        bytes[82..98].copy_from_slice(&self.fsid);
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// Writer-side device-extent record kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct BtrfsDevExtent {
    pub chunk_tree: u64,
    pub chunk_objectid: u64,
    pub chunk_offset: u64,
    pub length: u64,
}
#[allow(dead_code)]
impl BtrfsDevExtent {
    pub fn decode(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() != 32 {
            return Err(AxError::Io);
        }
        let value = Self {
            chunk_tree: le64(bytes, 0)?,
            chunk_objectid: le64(bytes, 8)?,
            chunk_offset: le64(bytes, 16)?,
            length: le64(bytes, 24)?,
        };
        if value.chunk_tree == 0 || value.chunk_objectid == 0 || value.length == 0 {
            return Err(AxError::Io);
        }
        Ok(value)
    }
    pub fn encode(self) -> AxResult<Vec<u8>> {
        if self.chunk_tree == 0 || self.chunk_objectid == 0 || self.length == 0 {
            return Err(AxError::InvalidInput);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(32).map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(&self.chunk_tree.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_objectid.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_offset.to_le_bytes());
        bytes.extend_from_slice(&self.length.to_le_bytes());
        Ok(bytes)
    }
}

impl BtrfsInodeItem {
    /// Constructs the native inode payload used by a create transaction.  It
    /// intentionally initializes no private compatibility metadata: every
    /// Linux-visible field has a Btrfs inode-item representation.
    pub fn new(
        generation: u64,
        node_type: NodeType,
        permission: NodePermission,
        uid: u32,
        gid: u32,
        rdev: u64,
        project_id: u32,
        timestamp: Timestamp,
    ) -> AxResult<Self> {
        let type_bits = match node_type {
            NodeType::Fifo => 0o010000,
            NodeType::CharacterDevice => 0o020000,
            NodeType::Directory => 0o040000,
            NodeType::BlockDevice => 0o060000,
            NodeType::RegularFile => 0o100000,
            NodeType::Symlink => 0o120000,
            NodeType::Socket => 0o140000,
            NodeType::Unknown => return Err(AxError::InvalidInput),
        };
        Ok(Self {
            generation,
            transid: generation,
            size: 0,
            nbytes: 0,
            nlink: if node_type == NodeType::Directory {
                2
            } else {
                1
            },
            uid,
            gid,
            mode: type_bits | u32::from(permission.bits()),
            rdev,
            flags: 0,
            sequence: 0,
            project_id,
            atime: timestamp,
            ctime: timestamp,
            mtime: timestamp,
            otime: timestamp,
        })
    }
    pub fn decode(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() != INODE_ITEM_BYTES {
            return Err(AxError::Io);
        }
        Ok(Self {
            generation: le64(bytes, 0)?,
            transid: le64(bytes, 8)?,
            size: le64(bytes, 16)?,
            nbytes: le64(bytes, 24)?,
            nlink: le32(bytes, 40)?,
            uid: le32(bytes, 44)?,
            gid: le32(bytes, 48)?,
            mode: le32(bytes, 52)?,
            rdev: le64(bytes, 56)?,
            flags: le64(bytes, 64)?,
            sequence: le64(bytes, 72)?,
            project_id: le32(bytes, 80)?,
            atime: timestamp(bytes, 112)?,
            ctime: timestamp(bytes, 124)?,
            mtime: timestamp(bytes, 136)?,
            otime: timestamp(bytes, 148)?,
        })
    }

    pub fn metadata(self, inode: u64, device: u64, project_id: u32) -> AxResult<Metadata> {
        let node_type = match self.mode & 0o170000 {
            0o010000 => NodeType::Fifo,
            0o020000 => NodeType::CharacterDevice,
            0o040000 => NodeType::Directory,
            0o060000 => NodeType::BlockDevice,
            0o100000 => NodeType::RegularFile,
            0o120000 => NodeType::Symlink,
            0o140000 => NodeType::Socket,
            _ => NodeType::Unknown,
        };
        Ok(Metadata {
            inode,
            device,
            nlink: u64::from(self.nlink),
            mode: NodePermission::from_bits_truncate((self.mode & 0o7777) as u16),
            node_type,
            uid: self.uid,
            gid: self.gid,
            project_id: if project_id == 0 {
                self.project_id
            } else {
                project_id
            },
            size: self.size,
            block_size: 4096,
            blocks: self.nbytes.checked_add(511).ok_or(AxError::Io)? / 512,
            rdev: DeviceId(self.rdev),
            atime: self.atime,
            btime: self.otime,
            mtime: self.mtime,
            ctime: self.ctime,
        })
    }

    /// Serializes an inode item without discarding unknown reserved words.
    /// This writer owns only fields represented by `BtrfsInodeItem`; callers
    /// constructing a new inode start from zero, while metadata updates first
    /// decode the existing item and retain all opaque bytes through this
    /// canonical native layout.
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = alloc::vec![0; INODE_ITEM_BYTES];
        put64(&mut bytes, 0, self.generation);
        put64(&mut bytes, 8, self.transid);
        put64(&mut bytes, 16, self.size);
        put64(&mut bytes, 24, self.nbytes);
        put32(&mut bytes, 40, self.nlink);
        put32(&mut bytes, 44, self.uid);
        put32(&mut bytes, 48, self.gid);
        put32(&mut bytes, 52, self.mode);
        put64(&mut bytes, 56, self.rdev);
        put64(&mut bytes, 64, self.flags);
        put64(&mut bytes, 72, self.sequence);
        put32(&mut bytes, 80, self.project_id);
        put_timestamp(&mut bytes, 112, self.atime);
        put_timestamp(&mut bytes, 124, self.ctime);
        put_timestamp(&mut bytes, 136, self.mtime);
        put_timestamp(&mut bytes, 148, self.otime);
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtrfsRootItem {
    pub generation: u64,
    pub root_dirid: u64,
    pub bytenr: u64,
    pub level: u8,
}

impl BtrfsRootItem {
    pub fn decode(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() <= ROOT_ITEM_LEVEL {
            return Err(AxError::Io);
        }
        let generation = le64(bytes, ROOT_ITEM_GENERATION)?;
        let root_dirid = le64(bytes, ROOT_ITEM_ROOT_DIRID)?;
        let bytenr = le64(bytes, ROOT_ITEM_BYTENR)?;
        let level = bytes[ROOT_ITEM_LEVEL];
        if generation == 0 || root_dirid == 0 || bytenr == 0 {
            return Err(AxError::Io);
        }
        Ok(Self {
            generation,
            root_dirid,
            bytenr,
            level,
        })
    }

    /// Produces a replacement root-item payload while retaining every field
    /// unknown to this implementation.  Root COW must change only the root
    /// node address at this layer; generation/flags/drop-progress semantics
    /// are owned by the caller's root transaction.
    pub fn replace_bytenr(bytes: &[u8], bytenr: u64) -> AxResult<Vec<u8>> {
        if bytenr == 0 || bytes.len() < ROOT_ITEM_BYTENR + 8 {
            return Err(AxError::InvalidInput);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bytes.len())
            .map_err(|_| AxError::NoMemory)?;
        output.extend_from_slice(bytes);
        output[ROOT_ITEM_BYTENR..ROOT_ITEM_BYTENR + 8].copy_from_slice(&bytenr.to_le_bytes());
        Ok(output)
    }

    pub fn replace_root(bytes: &[u8], bytenr: u64, generation: u64) -> AxResult<Vec<u8>> {
        if generation == 0 || bytenr == 0 || bytes.len() < ROOT_ITEM_BYTENR + 8 {
            return Err(AxError::InvalidInput);
        }
        let mut output = Self::replace_bytenr(bytes, bytenr)?;
        output[ROOT_ITEM_GENERATION..ROOT_ITEM_GENERATION + 8]
            .copy_from_slice(&generation.to_le_bytes());
        Ok(output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtrfsDirItem {
    /// Full embedded `btrfs_disk_key`, not merely its objectid.  Directory
    /// collision buckets may be edited entry-by-entry, so dropping either
    /// the embedded key type or offset would rewrite surviving records into
    /// a different on-media object.
    pub inode: u64,
    pub location_type: u8,
    pub location_offset: u64,
    pub item_type: u8,
    pub transid: u64,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// Decodes every packed directory item in one leaf payload.  Names are raw
/// bytes, preserving valid non-UTF-8 Btrfs directory entries through the VFS.
pub fn decode_dir_items(bytes: &[u8]) -> AxResult<Vec<BtrfsDirItem>> {
    let mut cursor = 0usize;
    let mut output = Vec::new();
    while cursor < bytes.len() {
        let header = bytes
            .get(
                cursor
                    ..cursor
                        .checked_add(DIR_ITEM_HEADER_BYTES)
                        .ok_or(AxError::Io)?,
            )
            .ok_or(AxError::Io)?;
        let inode = le64(header, 0)?;
        let location_type = header[8];
        let location_offset = le64(header, 9)?;
        let item_type = header[29];
        let transid = le64(header, 17)?;
        let data_len = usize::from(le16(header, 25)?);
        let name_len = usize::from(le16(header, 27)?);
        let payload_start = cursor
            .checked_add(DIR_ITEM_HEADER_BYTES)
            .ok_or(AxError::Io)?;
        let name_end = payload_start.checked_add(name_len).ok_or(AxError::Io)?;
        let data_end = name_end.checked_add(data_len).ok_or(AxError::Io)?;
        let name = bytes.get(payload_start..name_end).ok_or(AxError::Io)?;
        let data = bytes.get(name_end..data_end).ok_or(AxError::Io)?;
        if inode == 0 || name.is_empty() || name.iter().any(|byte| *byte == b'/') {
            return Err(AxError::Io);
        }
        let mut owned_name = Vec::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_name.extend_from_slice(name);
        let mut owned_data = Vec::new();
        owned_data
            .try_reserve_exact(data.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_data.extend_from_slice(data);
        output.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        output.push(BtrfsDirItem {
            inode,
            location_type,
            location_offset,
            item_type,
            transid,
            name: owned_name,
            data: owned_data,
        });
        cursor = data_end;
    }
    Ok(output)
}

/// Encodes a collision bucket for DIR_ITEM, DIR_INDEX, or XATTR_ITEM.  The
/// item key selects the bucket; each payload retains the target inode/type,
/// transid, raw name and raw value.  This is deliberately shared by xattrs
/// and directory entries because the on-media format is the same packed
/// `btrfs_dir_item` sequence.
pub fn encode_dir_items(items: &[BtrfsDirItem]) -> AxResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for item in items {
        if item.inode == 0
            || item.name.is_empty()
            || item.name.iter().any(|byte| *byte == b'/')
            || item.name.len() > u16::MAX as usize
            || item.data.len() > u16::MAX as usize
        {
            return Err(AxError::InvalidInput);
        }
        let length = DIR_ITEM_HEADER_BYTES
            .checked_add(item.name.len())
            .and_then(|value| value.checked_add(item.data.len()))
            .ok_or(AxError::NoMemory)?;
        bytes.try_reserve(length).map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(&item.inode.to_le_bytes());
        bytes.push(item.location_type);
        bytes.extend_from_slice(&item.location_offset.to_le_bytes());
        bytes.extend_from_slice(&item.transid.to_le_bytes());
        bytes.extend_from_slice(&(item.data.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(item.name.len() as u16).to_le_bytes());
        bytes.push(item.item_type);
        bytes.extend_from_slice(&item.name);
        bytes.extend_from_slice(&item.data);
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BtrfsExtentKind {
    Inline,
    Regular,
    Prealloc,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtrfsFileExtent {
    pub generation: u64,
    pub ram_bytes: u64,
    pub compression: u8,
    pub encryption: u8,
    /// `btrfs_file_extent_item::other_encoding`.  The current native range
    /// writer has no representation for a non-zero value, so callers that
    /// rewrite/split an extent must reject it instead of silently clearing
    /// the on-media ABI field.
    pub other_encoding: u16,
    pub kind: BtrfsExtentKind,
    pub disk_bytenr: u64,
    pub disk_num_bytes: u64,
    pub extent_offset: u64,
    pub num_bytes: u64,
    pub inline_data: Vec<u8>,
}

impl BtrfsFileExtent {
    pub fn decode(bytes: &[u8]) -> AxResult<Self> {
        if bytes.len() < FILE_EXTENT_HEADER_BYTES {
            return Err(AxError::Io);
        }
        let kind = match bytes[20] {
            0 => BtrfsExtentKind::Inline,
            1 => BtrfsExtentKind::Regular,
            2 => BtrfsExtentKind::Prealloc,
            _ => return Err(AxError::Io),
        };
        let mut result = Self {
            generation: le64(bytes, 0)?,
            ram_bytes: le64(bytes, 8)?,
            compression: bytes[16],
            encryption: bytes[17],
            other_encoding: le16(bytes, 18)?,
            kind,
            disk_bytenr: 0,
            disk_num_bytes: 0,
            extent_offset: 0,
            num_bytes: 0,
            inline_data: Vec::new(),
        };
        match kind {
            BtrfsExtentKind::Inline => {
                let payload = &bytes[FILE_EXTENT_HEADER_BYTES..];
                result
                    .inline_data
                    .try_reserve_exact(payload.len())
                    .map_err(|_| AxError::NoMemory)?;
                result.inline_data.extend_from_slice(payload);
                result.num_bytes = result.ram_bytes;
                if result.num_bytes == 0 {
                    return Err(AxError::Io);
                }
            }
            BtrfsExtentKind::Regular | BtrfsExtentKind::Prealloc => {
                if bytes.len() != FILE_EXTENT_HEADER_BYTES + 32 {
                    return Err(AxError::Io);
                }
                result.disk_bytenr = le64(bytes, 21)?;
                result.disk_num_bytes = le64(bytes, 29)?;
                result.extent_offset = le64(bytes, 37)?;
                result.num_bytes = le64(bytes, 45)?;
                // `disk_num_bytes` measures compressed storage while
                // `extent_offset`/`num_bytes` address the decompressed
                // extent.  Comparing those domains rejects valid compressed
                // reflink extents.  The bound is meaningful only for the
                // uncompressed representation.
                // Only a regular record with both disk fields zero is Btrfs's
                // explicit-hole encoding.  It still occupies a logical file
                // interval, but owns no extent-tree ref, checksum, or media
                // range.  One zero and one non-zero field is malformed.
                if (result.disk_bytenr == 0) != (result.disk_num_bytes == 0)
                    || (result.kind == BtrfsExtentKind::Prealloc && result.disk_bytenr == 0)
                    || (!result.is_explicit_hole()
                        && result.compression == 0
                        && result
                            .extent_offset
                            .checked_add(result.num_bytes)
                            .map_or(true, |end| end > result.disk_num_bytes))
                {
                    return Err(AxError::Io);
                }
            }
        }
        Ok(result)
    }

    pub fn is_explicit_hole(&self) -> bool {
        self.kind == BtrfsExtentKind::Regular && self.disk_bytenr == 0 && self.disk_num_bytes == 0
    }

    pub fn owns_physical_storage(&self) -> bool {
        matches!(
            self.kind,
            BtrfsExtentKind::Regular | BtrfsExtentKind::Prealloc
        ) && !self.is_explicit_hole()
    }
}

/// Encodes a native inline file extent.  Inline extents are used by the
/// mutation planner only when the entire resulting extent fits in one leaf;
/// larger writes allocate a checksummed regular extent instead of truncating
/// the request or pretending success.
pub fn encode_inline_extent(generation: u64, bytes: &[u8]) -> AxResult<Vec<u8>> {
    if bytes.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(
            FILE_EXTENT_HEADER_BYTES
                .checked_add(bytes.len())
                .ok_or(AxError::NoMemory)?,
        )
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&generation.to_le_bytes());
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.push(0); // compression
    output.push(0); // encryption
    output.extend_from_slice(&0u16.to_le_bytes());
    output.push(0); // BTRFS_FILE_EXTENT_INLINE
    output.extend_from_slice(bytes);
    Ok(output)
}

/// Encodes an uncompressed regular extent mapping after its data sectors and
/// checksum-tree records have been prepared.  The caller supplies only
/// sector-aligned, nonzero physical/logical storage; compression writers use
/// their own payload encoder because `disk_num_bytes` then differs from the
/// logical range length.
pub fn encode_regular_extent(
    generation: u64,
    disk_bytenr: u64,
    disk_num_bytes: u64,
    extent_offset: u64,
    num_bytes: u64,
) -> AxResult<Vec<u8>> {
    if disk_bytenr == 0
        || disk_num_bytes == 0
        || num_bytes == 0
        || extent_offset
            .checked_add(num_bytes)
            .map_or(true, |end| end > disk_num_bytes)
    {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(FILE_EXTENT_HEADER_BYTES + 32)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&generation.to_le_bytes());
    output.extend_from_slice(&disk_num_bytes.to_le_bytes()); // ram_bytes
    output.extend_from_slice(&[0, 0]);
    output.extend_from_slice(&0u16.to_le_bytes());
    output.push(1); // BTRFS_FILE_EXTENT_REG
    output.extend_from_slice(&disk_bytenr.to_le_bytes());
    output.extend_from_slice(&disk_num_bytes.to_le_bytes());
    output.extend_from_slice(&extent_offset.to_le_bytes());
    output.extend_from_slice(&num_bytes.to_le_bytes());
    Ok(output)
}

/// Encodes an unwritten preallocated extent.  It owns normal extent-tree
/// references and physical reservation just like a regular extent, but reads
/// as zero until a later COW write converts the covered range.
pub fn encode_prealloc_extent(
    generation: u64,
    disk_bytenr: u64,
    disk_num_bytes: u64,
    extent_offset: u64,
    num_bytes: u64,
) -> AxResult<Vec<u8>> {
    if disk_bytenr == 0
        || disk_num_bytes == 0
        || num_bytes == 0
        || extent_offset
            .checked_add(num_bytes)
            .map_or(true, |end| end > disk_num_bytes)
    {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(FILE_EXTENT_HEADER_BYTES + 32)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&generation.to_le_bytes());
    output.extend_from_slice(&disk_num_bytes.to_le_bytes());
    output.extend_from_slice(&[0, 0]);
    output.extend_from_slice(&0u16.to_le_bytes());
    output.push(2); // BTRFS_FILE_EXTENT_PREALLOC
    output.extend_from_slice(&disk_bytenr.to_le_bytes());
    output.extend_from_slice(&disk_num_bytes.to_le_bytes());
    output.extend_from_slice(&extent_offset.to_le_bytes());
    output.extend_from_slice(&num_bytes.to_le_bytes());
    Ok(output)
}

/// Encodes the fixed header of a data `EXTENT_ITEM`.  The following inline
/// reference item is stored separately as `EXTENT_DATA_REF`; keeping them as
/// distinct tree keys lets the delayed-ref transaction add/remove references
/// without rewriting an opaque payload owned by a future extent-tree reader.
pub fn encode_data_extent_item(generation: u64, references: u64) -> AxResult<Vec<u8>> {
    if generation == 0 || references == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(24)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&references.to_le_bytes());
    output.extend_from_slice(&generation.to_le_bytes());
    output.extend_from_slice(&1u64.to_le_bytes()); // BTRFS_EXTENT_FLAG_DATA
    Ok(output)
}

/// Native metadata/tree-block `EXTENT_ITEM` header.  Metadata extents carry
/// the TREE_BLOCK flag and an inline `btrfs_tree_block_info` (empty key plus
/// level); ownership relations are emitted as `TREE_BLOCK_REF` keys by the
/// transaction writer rather than being confused with data backrefs.
pub fn encode_tree_extent_item(generation: u64, references: u64, level: u8) -> AxResult<Vec<u8>> {
    if generation == 0 || references == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(42)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&references.to_le_bytes());
    output.extend_from_slice(&generation.to_le_bytes());
    output.extend_from_slice(&2u64.to_le_bytes()); // BTRFS_EXTENT_FLAG_TREE_BLOCK
    output.extend_from_slice(&[0; 17]); // btrfs_disk_key: tree root is in TREE_BLOCK_REF
    output.push(level);
    Ok(output)
}

pub fn decode_tree_extent_item(bytes: &[u8]) -> AxResult<(u64, u64, u8)> {
    if bytes.len() != 42 {
        return Err(AxError::Io);
    }
    let references = le64(bytes, 0)?;
    let generation = le64(bytes, 8)?;
    if references == 0 || generation == 0 || le64(bytes, 16)? != 2 {
        return Err(AxError::Io);
    }
    Ok((references, generation, bytes[41]))
}

pub fn encode_tree_block_ref(root: u64) -> AxResult<Vec<u8>> {
    if root == 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(Vec::new())
}

pub fn decode_tree_block_ref(bytes: &[u8]) -> AxResult<()> {
    if !bytes.is_empty() {
        return Err(AxError::Io);
    }
    Ok(())
}

/// Encodes one on-media data extent reference: root, inode owner, file
/// offset, and reference count.  The caller chooses the key offset from a
/// collision-resistant relation identifier; the payload remains complete so
/// recovery does not depend on that implementation detail.
pub fn encode_extent_data_ref(
    root: u64,
    owner: u64,
    file_offset: u64,
    count: u32,
) -> AxResult<Vec<u8>> {
    if root == 0 || owner == 0 || count == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(28)
        .map_err(|_| AxError::NoMemory)?;
    output.extend_from_slice(&root.to_le_bytes());
    output.extend_from_slice(&owner.to_le_bytes());
    output.extend_from_slice(&file_offset.to_le_bytes());
    output.extend_from_slice(&count.to_le_bytes());
    Ok(output)
}

pub fn decode_extent_data_ref(bytes: &[u8]) -> AxResult<(u64, u64, u64, u32)> {
    if bytes.len() != 28 {
        return Err(AxError::Io);
    }
    let root = le64(bytes, 0)?;
    let owner = le64(bytes, 8)?;
    let file_offset = le64(bytes, 16)?;
    let count = le32(bytes, 24)?;
    if root == 0 || owner == 0 || count == 0 {
        return Err(AxError::Io);
    }
    Ok((root, owner, file_offset, count))
}

/// One packed `btrfs_inode_ref` alias in an `(inode, INODE_REF, parent)`
/// payload.  The key is a bucket, not a single-name record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BtrfsInodeRef {
    pub index: u64,
    pub name: Vec<u8>,
}

pub fn encode_inode_refs(records: &[BtrfsInodeRef]) -> AxResult<Vec<u8>> {
    let mut output = Vec::new();
    for record in records {
        if record.index == 0
            || record.name.is_empty()
            || record.name.len() > u16::MAX as usize
            || record.name.iter().any(|byte| *byte == b'/')
        {
            return Err(AxError::InvalidInput);
        }
        output
            .try_reserve(
                10usize
                    .checked_add(record.name.len())
                    .ok_or(AxError::NoMemory)?,
            )
            .map_err(|_| AxError::NoMemory)?;
        output.extend_from_slice(&record.index.to_le_bytes());
        output.extend_from_slice(&(record.name.len() as u16).to_le_bytes());
        output.extend_from_slice(&record.name);
    }
    if output.is_empty() {
        return Err(AxError::InvalidInput);
    }
    Ok(output)
}

pub fn decode_inode_refs(bytes: &[u8]) -> AxResult<Vec<BtrfsInodeRef>> {
    let mut cursor = 0usize;
    let mut output = Vec::new();
    while cursor < bytes.len() {
        let header = bytes
            .get(cursor..cursor.checked_add(10).ok_or(AxError::Io)?)
            .ok_or(AxError::Io)?;
        let index = le64(header, 0)?;
        let length = usize::from(le16(header, 8)?);
        let end = cursor
            .checked_add(10)
            .and_then(|start| start.checked_add(length))
            .ok_or(AxError::Io)?;
        let name = bytes.get(cursor + 10..end).ok_or(AxError::Io)?;
        if index == 0 || name.is_empty() || name.iter().any(|byte| *byte == b'/') {
            return Err(AxError::Io);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(name);
        output.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        output.push(BtrfsInodeRef { index, name: owned });
        cursor = end;
    }
    if output.is_empty() {
        return Err(AxError::Io);
    }
    Ok(output)
}

/// Native packed `btrfs_inode_extref` records.  The EXTREF key is
/// `(inode, INODE_EXTREF, name_hash)`; collisions are represented by multiple
/// variable-length records in the same item exactly like Linux Btrfs.
pub fn decode_inode_extrefs(bytes: &[u8]) -> AxResult<Vec<(u64, u64, Vec<u8>)>> {
    let mut cursor = 0usize;
    let mut records = Vec::new();
    while cursor < bytes.len() {
        let header_end = cursor.checked_add(18).ok_or(AxError::Io)?;
        let header = bytes.get(cursor..header_end).ok_or(AxError::Io)?;
        let parent = le64(header, 0)?;
        let index = le64(header, 8)?;
        let length = usize::from(le16(header, 16)?);
        let end = header_end.checked_add(length).ok_or(AxError::Io)?;
        let name = bytes.get(header_end..end).ok_or(AxError::Io)?;
        if parent == 0 || index == 0 || name.is_empty() || name.iter().any(|byte| *byte == b'/') {
            return Err(AxError::Io);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(name);
        records.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        records.push((parent, index, owned));
        cursor = end;
    }
    Ok(records)
}

pub fn encode_inode_extrefs(records: &[(u64, u64, Vec<u8>)]) -> AxResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for (parent, index, name) in records {
        if *parent == 0
            || *index == 0
            || name.is_empty()
            || name.len() > u16::MAX as usize
            || name.iter().any(|byte| *byte == b'/')
        {
            return Err(AxError::InvalidInput);
        }
        let size = 18usize.checked_add(name.len()).ok_or(AxError::NoMemory)?;
        bytes.try_reserve(size).map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(&parent.to_le_bytes());
        bytes.extend_from_slice(&index.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name);
    }
    Ok(bytes)
}

fn timestamp(bytes: &[u8], offset: usize) -> AxResult<Timestamp> {
    let seconds = le64(bytes, offset)?;
    let nanos = le32(bytes, offset + 8)?;
    if nanos >= 1_000_000_000 {
        return Err(AxError::Io);
    }
    Ok(Timestamp::new(seconds as i64, nanos))
}
fn le16(bytes: &[u8], offset: usize) -> AxResult<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(AxError::Io)
}
fn le32(bytes: &[u8], offset: usize) -> AxResult<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AxError::Io)
}
fn le64(bytes: &[u8], offset: usize) -> AxResult<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::Io)
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_timestamp(bytes: &mut [u8], offset: usize, value: Timestamp) {
    put64(bytes, offset, value.seconds() as u64);
    put32(bytes, offset + 8, value.subsec_nanos());
}
