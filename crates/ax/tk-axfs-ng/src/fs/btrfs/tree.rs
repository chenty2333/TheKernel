use axerrno::{AxError, AxResult};

use super::{Checksum, TreeItemKey, crc32c};

const HEADER_SIZE: usize = 0x65;
const LEAF_ITEM_SIZE: usize = 25;
const INTERNAL_ITEM_SIZE: usize = 33;

/// A checked Btrfs tree block.  The block borrows its verified backing bytes;
/// no on-media field is exposed before range, checksum, fsid, owner, and sort
/// validation has completed.
pub struct BtrfsTreeBlock<'a> {
    bytes: &'a [u8],
    level: u8,
    item_count: u32,
    generation: u64,
    owner: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeLeafItem<'a> {
    pub key: TreeItemKey,
    pub value: &'a [u8],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeChild {
    pub key: TreeItemKey,
    pub bytenr: u64,
    pub generation: u64,
}

/// A COW leaf item prepared for serialisation.  The writer accepts the
/// already ordered raw payload and derives all on-media offsets itself.
#[derive(Clone, Copy, Debug)]
pub struct TreeWriteItem<'a> {
    pub key: TreeItemKey,
    pub value: &'a [u8],
}

impl<'a> BtrfsTreeBlock<'a> {
    /// Serialises a complete checked leaf suitable for a COW tree write.  It
    /// never modifies an old block: the caller allocates a fresh logical
    /// bytenr, writes this image, and only then publishes a new root.
    pub fn encode_leaf(
        nodesize: usize,
        fsid: &[u8; 16],
        bytenr: u64,
        generation: u64,
        owner: u64,
        items: &[TreeWriteItem<'_>],
    ) -> AxResult<alloc::vec::Vec<u8>> {
        if nodesize < HEADER_SIZE || bytenr == 0 || generation == 0 || owner == 0 {
            return Err(AxError::InvalidInput);
        }
        if items.len() > u32::MAX as usize {
            return Err(AxError::StorageFull);
        }
        let table_size = items
            .len()
            .checked_mul(LEAF_ITEM_SIZE)
            .ok_or(AxError::NoMemory)?;
        let mut cursor = nodesize;
        if HEADER_SIZE
            .checked_add(table_size)
            .map_or(true, |end| end > nodesize)
        {
            return Err(AxError::StorageFull);
        }
        let mut output = alloc::vec::Vec::new();
        output
            .try_reserve_exact(nodesize)
            .map_err(|_| AxError::NoMemory)?;
        output.resize(nodesize, 0);
        output[32..48].copy_from_slice(fsid);
        output[48..56].copy_from_slice(&bytenr.to_le_bytes());
        output[80..88].copy_from_slice(&generation.to_le_bytes());
        output[88..96].copy_from_slice(&owner.to_le_bytes());
        output[96..100].copy_from_slice(&(items.len() as u32).to_le_bytes());
        output[100] = 0;
        let mut previous = None;
        for (index, item) in items.iter().enumerate() {
            if item.value.len() > u32::MAX as usize {
                return Err(AxError::StorageFull);
            }
            if previous.map_or(false, |key| item.key <= key) {
                return Err(AxError::InvalidInput);
            }
            previous = Some(item.key);
            cursor = cursor
                .checked_sub(item.value.len())
                .ok_or(AxError::StorageFull)?;
            if cursor < HEADER_SIZE + table_size {
                return Err(AxError::StorageFull);
            }
            output[cursor..cursor + item.value.len()].copy_from_slice(item.value);
            let offset = HEADER_SIZE + index * LEAF_ITEM_SIZE;
            output[offset..offset + 8].copy_from_slice(&item.key.objectid.to_le_bytes());
            output[offset + 8] = item.key.item_type;
            output[offset + 9..offset + 17].copy_from_slice(&item.key.offset.to_le_bytes());
            output[offset + 17..offset + 21].copy_from_slice(&(cursor as u32).to_le_bytes());
            output[offset + 21..offset + 25]
                .copy_from_slice(&(item.value.len() as u32).to_le_bytes());
        }
        let checksum = crc32c(&output[32..]);
        output[..4].copy_from_slice(&checksum.to_le_bytes());
        Ok(output)
    }

    /// Serialises a checked internal node.  The key recorded for every child
    /// is that child's lower bound; it is not inferred from a caller supplied
    /// byte offset.  Keeping node construction here matters because the
    /// extent/chunk/root trees all rely on the same COW ordering invariant.
    pub fn encode_internal(
        nodesize: usize,
        fsid: &[u8; 16],
        bytenr: u64,
        generation: u64,
        owner: u64,
        level: u8,
        children: &[TreeChild],
    ) -> AxResult<alloc::vec::Vec<u8>> {
        if nodesize < HEADER_SIZE
            || bytenr == 0
            || generation == 0
            || owner == 0
            || level == 0
            || children.is_empty()
        {
            return Err(AxError::InvalidInput);
        }
        let table_size = children
            .len()
            .checked_mul(INTERNAL_ITEM_SIZE)
            .ok_or(AxError::NoMemory)?;
        if HEADER_SIZE
            .checked_add(table_size)
            .map_or(true, |end| end > nodesize)
        {
            return Err(AxError::StorageFull);
        }
        let mut output = alloc::vec::Vec::new();
        output
            .try_reserve_exact(nodesize)
            .map_err(|_| AxError::NoMemory)?;
        output.resize(nodesize, 0);
        output[32..48].copy_from_slice(fsid);
        output[48..56].copy_from_slice(&bytenr.to_le_bytes());
        output[80..88].copy_from_slice(&generation.to_le_bytes());
        output[88..96].copy_from_slice(&owner.to_le_bytes());
        output[96..100].copy_from_slice(&(children.len() as u32).to_le_bytes());
        output[100] = level;
        let mut previous = None;
        for (index, child) in children.iter().enumerate() {
            if child.bytenr == 0
                || child.generation == 0
                || previous.map_or(false, |key| child.key <= key)
            {
                return Err(AxError::InvalidInput);
            }
            previous = Some(child.key);
            let offset = HEADER_SIZE + index * INTERNAL_ITEM_SIZE;
            output[offset..offset + 8].copy_from_slice(&child.key.objectid.to_le_bytes());
            output[offset + 8] = child.key.item_type;
            output[offset + 9..offset + 17].copy_from_slice(&child.key.offset.to_le_bytes());
            output[offset + 17..offset + 25].copy_from_slice(&child.bytenr.to_le_bytes());
            output[offset + 25..offset + 33].copy_from_slice(&child.generation.to_le_bytes());
        }
        let checksum = crc32c(&output[32..]);
        output[..4].copy_from_slice(&checksum.to_le_bytes());
        Ok(output)
    }
    pub fn decode(
        bytes: &'a [u8],
        fsid: &[u8; 16],
        checksum: Checksum,
        expected_bytenr: u64,
    ) -> AxResult<Self> {
        if bytes.len() < HEADER_SIZE || !checksum.verify(&bytes[32..]) {
            return Err(AxError::Io);
        }
        if &bytes[32..48] != fsid || le64(bytes, 48)? != expected_bytenr {
            return Err(AxError::Io);
        }
        let generation = le64(bytes, 80)?;
        let owner = le64(bytes, 88)?;
        let item_count = le32(bytes, 96)?;
        let level = *bytes.get(100).ok_or(AxError::Io)?;
        let item_size = if level == 0 {
            LEAF_ITEM_SIZE
        } else {
            INTERNAL_ITEM_SIZE
        };
        let table_end = HEADER_SIZE
            .checked_add(
                (item_count as usize)
                    .checked_mul(item_size)
                    .ok_or(AxError::Io)?,
            )
            .ok_or(AxError::Io)?;
        if table_end > bytes.len() {
            return Err(AxError::Io);
        }
        let block = Self {
            bytes,
            level,
            item_count,
            generation,
            owner,
        };
        block.validate_items()?;
        Ok(block)
    }

    pub const fn level(&self) -> u8 {
        self.level
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn owner(&self) -> u64 {
        self.owner
    }
    pub const fn item_count(&self) -> u32 {
        self.item_count
    }

    pub fn leaf_item(&self, index: u32) -> AxResult<TreeLeafItem<'a>> {
        if self.level != 0 || index >= self.item_count {
            return Err(AxError::InvalidInput);
        }
        let offset = HEADER_SIZE + index as usize * LEAF_ITEM_SIZE;
        let key = parse_key(&self.bytes[offset..offset + 17])?;
        let value_offset = le32(self.bytes, offset + 17)? as usize;
        let value_size = le32(self.bytes, offset + 21)? as usize;
        let end = value_offset.checked_add(value_size).ok_or(AxError::Io)?;
        let value = self.bytes.get(value_offset..end).ok_or(AxError::Io)?;
        Ok(TreeLeafItem { key, value })
    }

    pub fn child(&self, index: u32) -> AxResult<TreeChild> {
        if self.level == 0 || index >= self.item_count {
            return Err(AxError::InvalidInput);
        }
        let offset = HEADER_SIZE + index as usize * INTERNAL_ITEM_SIZE;
        Ok(TreeChild {
            key: parse_key(&self.bytes[offset..offset + 17])?,
            bytenr: le64(self.bytes, offset + 17)?,
            generation: le64(self.bytes, offset + 25)?,
        })
    }

    /// Finds an exact leaf key; internal descent is deliberately left to the
    /// mount reader because each child must be read through chunk mapping and
    /// independently checksum-verified before use.
    pub fn find_leaf(&self, wanted: TreeItemKey) -> AxResult<Option<TreeLeafItem<'a>>> {
        if self.level != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut lower = 0;
        let mut upper = self.item_count;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            let item = self.leaf_item(middle)?;
            if item.key < wanted {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        if lower < self.item_count {
            let item = self.leaf_item(lower)?;
            if item.key == wanted {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    /// Chooses the child whose key range contains `wanted`.  This is the
    /// exact lower-bound rule used by a Btrfs internal node; callers still
    /// have to checksum-verify the returned child before descending.
    pub fn child_for(&self, wanted: TreeItemKey) -> AxResult<TreeChild> {
        if self.level == 0 || self.item_count == 0 {
            return Err(AxError::InvalidInput);
        }
        let mut lower = 0;
        let mut upper = self.item_count;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if self.child(middle)?.key <= wanted {
                lower = middle + 1;
            } else {
                upper = middle;
            }
        }
        self.child(lower.saturating_sub(1))
    }

    fn validate_items(&self) -> AxResult<()> {
        let mut prior = None;
        for index in 0..self.item_count {
            let key = if self.level == 0 {
                self.leaf_item_unchecked(index)?.0
            } else {
                self.child_unchecked(index)?.0
            };
            if prior.map_or(false, |previous| key <= previous) {
                return Err(AxError::Io);
            }
            if self.level == 0 {
                let (_, offset, size) = self.leaf_item_unchecked(index)?;
                let end = offset.checked_add(size).ok_or(AxError::Io)?;
                // Item payloads are an unordered area at the end of a leaf,
                // hence checking adjacent table entries is insufficient.  Do
                // the bounded pairwise check before handing out any slice.
                for earlier in 0..index {
                    let (_, earlier_offset, earlier_size) = self.leaf_item_unchecked(earlier)?;
                    let earlier_end = earlier_offset
                        .checked_add(earlier_size)
                        .ok_or(AxError::Io)?;
                    if offset < earlier_end && earlier_offset < end {
                        return Err(AxError::Io);
                    }
                }
            }
            prior = Some(key);
        }
        Ok(())
    }

    fn leaf_item_unchecked(&self, index: u32) -> AxResult<(TreeItemKey, usize, usize)> {
        let offset = HEADER_SIZE + index as usize * LEAF_ITEM_SIZE;
        let key = parse_key(&self.bytes[offset..offset + 17])?;
        let value_offset = le32(self.bytes, offset + 17)? as usize;
        let value_size = le32(self.bytes, offset + 21)? as usize;
        let end = value_offset.checked_add(value_size).ok_or(AxError::Io)?;
        if value_offset < HEADER_SIZE + self.item_count as usize * LEAF_ITEM_SIZE
            || end > self.bytes.len()
        {
            return Err(AxError::Io);
        }
        Ok((key, value_offset, value_size))
    }
    fn child_unchecked(&self, index: u32) -> AxResult<(TreeItemKey, u64, u64)> {
        let offset = HEADER_SIZE + index as usize * INTERNAL_ITEM_SIZE;
        let key = parse_key(&self.bytes[offset..offset + 17])?;
        let bytenr = le64(self.bytes, offset + 17)?;
        let generation = le64(self.bytes, offset + 25)?;
        if bytenr == 0 || generation == 0 {
            return Err(AxError::Io);
        }
        Ok((key, bytenr, generation))
    }
}

fn parse_key(bytes: &[u8]) -> AxResult<TreeItemKey> {
    if bytes.len() != 17 {
        return Err(AxError::Io);
    }
    Ok(TreeItemKey {
        objectid: le64(bytes, 0)?,
        item_type: bytes[8],
        offset: le64(bytes, 9)?,
    })
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
