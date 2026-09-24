//! XFS directory and extended-attribute block formats and serializers.

use super::*;

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
    pub(super) fn parse(
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

pub(super) fn directory_type_for_inode(mode: u16) -> u8 {
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
pub(super) fn serialize_directory_data_block(
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

pub(super) fn serialize_directory_leaf(
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

pub(super) fn serialize_directory_node(
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

pub(super) fn serialize_directory_free(
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
    pub(super) fn partition_leaves(
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
