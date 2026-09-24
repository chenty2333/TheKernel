//! XFS B+tree builders, serializers and big-endian/CRC helpers.

use super::*;

/// Builds a complete, compact AG free-space B+tree from canonical extents.
/// Leaf splitting and parent promotion are iterative; the final singleton
/// parent is the root, while a single leaf naturally collapses the root.
/// Callers provide a verified pool containing old tree blocks followed by
/// AGFL blocks, making all growth/promotion consume freelist entries first.
pub(super) fn build_free_tree(
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
pub(super) fn build_inode_tree(
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
pub(super) fn build_special_tree(
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

pub(super) fn free_tree_key(kind: XfsAgBtreeKind, record: &XfsAgFreeRecord) -> (u32, u32) {
    match kind {
        XfsAgBtreeKind::ByBlock => (record.start_block, record.block_count),
        XfsAgBtreeKind::ByLength => (record.block_count, record.start_block),
        XfsAgBtreeKind::Inode | XfsAgBtreeKind::FreeInode => (0, 0),
    }
}

pub(super) fn encode_xfs_extent(extent: XfsExtent) -> XfsResult<[u8; 16]> {
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

pub(super) fn serialize_shortform_directory(
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

pub(super) fn serialize_shortform_xattrs(attrs: &[XfsShortformXattr]) -> XfsResult<Vec<u8>> {
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

pub(super) fn push_merged_extent(output: &mut Vec<XfsExtent>, extent: XfsExtent) -> XfsResult<()> {
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
pub(super) fn bmap_external_blocks(
    sb: XfsSuperblock,
    fork_bytes: usize,
    records: usize,
) -> XfsResult<usize> {
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

pub(super) fn serialize_bmap_node(
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

pub(super) fn byte(bytes: &[u8], offset: usize) -> XfsResult<u8> {
    bytes.get(offset).copied().ok_or(XfsError::CorruptMetadata)
}

pub(super) fn slice(bytes: &[u8], offset: usize, len: usize) -> XfsResult<&[u8]> {
    let end = offset.checked_add(len).ok_or(XfsError::CorruptMetadata)?;
    bytes.get(offset..end).ok_or(XfsError::CorruptMetadata)
}

pub(super) fn be16(bytes: &[u8], offset: usize) -> XfsResult<u16> {
    Ok(u16::from_be_bytes(
        slice(bytes, offset, 2)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

pub(super) fn be32(bytes: &[u8], offset: usize) -> XfsResult<u32> {
    Ok(u32::from_be_bytes(
        slice(bytes, offset, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

pub(super) fn be_i32(bytes: &[u8], offset: usize) -> XfsResult<i32> {
    Ok(i32::from_be_bytes(
        slice(bytes, offset, 4)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

pub(super) fn checked_nanoseconds(value: u32) -> XfsResult<u32> {
    if value >= 1_000_000_000 {
        return Err(XfsError::CorruptMetadata);
    }
    Ok(value)
}

/// XFS bigtime stores an unsigned nanosecond count from the beginning of the
/// signed-32-bit Unix-time range (1901-12-13), extending timestamps through
/// the year 2486 without changing inode size.
pub(super) fn parse_inode_timestamp(
    bytes: &[u8],
    offset: usize,
    bigtime: bool,
) -> XfsResult<(i64, u32)> {
    if !bigtime {
        return Ok((
            be_i32(bytes, offset)? as i64,
            checked_nanoseconds(be32(bytes, offset + 4)?)?,
        ));
    }
    pub(super) const BIGTIME_EPOCH_OFFSET: i64 = -2_147_483_648;
    let encoded = be64(bytes, offset)?;
    let seconds = ((encoded / 1_000_000_000) as i64)
        .checked_add(BIGTIME_EPOCH_OFFSET)
        .ok_or(XfsError::CorruptMetadata)?;
    let nanoseconds = (encoded % 1_000_000_000) as u32;
    Ok((seconds, nanoseconds))
}

/// Inverse of [`parse_inode_timestamp`].  Bigtime encodes an unsigned
/// nanosecond count relative to the legacy signed-32-bit epoch floor.
pub(super) fn encode_inode_timestamp(
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
    pub(super) const BIGTIME_EPOCH_OFFSET: i64 = -2_147_483_648;
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
pub(super) fn verify_crc32c(bytes: &[u8], crc_offset: usize) -> XfsResult<()> {
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

pub(super) fn rewrite_crc32c(bytes: &mut [u8], crc_offset: usize) -> XfsResult<()> {
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
pub(super) fn log_record_crc32c(
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

pub(super) fn verify_log_record_crc(bytes: &[u8], header: &XfsLogRecordHeader) -> XfsResult<()> {
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

pub(super) fn rewrite_log_record_crc(
    bytes: &mut [u8],
    header: &XfsLogRecordHeader,
) -> XfsResult<()> {
    let crc = log_record_crc32c(bytes, header, 328)?;
    bytes[XfsLogRecordHeader::CRC_OFFSET..XfsLogRecordHeader::CRC_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

pub(super) fn crc32c_step(mut crc: u32, byte: u8) -> u32 {
    crc ^= byte as u32;
    for _ in 0..8 {
        crc = (crc >> 1) ^ ((crc & 1 != 0) as u32 * 0x82f6_3b78);
    }
    crc
}

pub(super) fn be64(bytes: &[u8], offset: usize) -> XfsResult<u64> {
    Ok(u64::from_be_bytes(
        slice(bytes, offset, 8)?
            .try_into()
            .map_err(|_| XfsError::CorruptMetadata)?,
    ))
}

pub(super) fn put_be32(bytes: &mut [u8], offset: usize, value: u32) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(super) fn put_be16(bytes: &mut [u8], offset: usize, value: u16) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(2).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(super) fn put_be64(bytes: &mut [u8], offset: usize, value: u64) -> XfsResult<()> {
    let slot = bytes
        .get_mut(offset..offset.checked_add(8).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(super) fn native_u32(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u32> {
    let raw: [u8; 4] = slice(bytes, offset, 4)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u32::from_le_bytes(raw),
        XfsLogByteOrder::Big => u32::from_be_bytes(raw),
    })
}
pub(super) fn native_u16(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u16> {
    let raw: [u8; 2] = slice(bytes, offset, 2)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u16::from_le_bytes(raw),
        XfsLogByteOrder::Big => u16::from_be_bytes(raw),
    })
}
pub(super) fn native_u64(bytes: &[u8], offset: usize, order: XfsLogByteOrder) -> XfsResult<u64> {
    let raw: [u8; 8] = slice(bytes, offset, 8)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(match order {
        XfsLogByteOrder::Little => u64::from_le_bytes(raw),
        XfsLogByteOrder::Big => u64::from_be_bytes(raw),
    })
}
pub(super) fn native_put_u16(
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
pub(super) fn native_put_u32(
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
pub(super) fn native_put_u64(
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

pub(super) fn align8(value: usize) -> Option<usize> {
    value.checked_add(7).map(|value| value & !7)
}

pub(super) fn align_log_basic_block(value: usize) -> XfsResult<usize> {
    value
        .checked_add(XFS_LOG_BASIC_BLOCK - 1)
        .map(|value| value & !(XFS_LOG_BASIC_BLOCK - 1))
        .ok_or(XfsError::AddressOutOfRange)
}

pub(super) fn log_cycle_data_offset(record: &[u8], data_block: usize) -> XfsResult<usize> {
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

pub(super) fn log_cycle_data_get(record: &[u8], data_block: usize) -> XfsResult<u32> {
    be32(record, log_cycle_data_offset(record, data_block)?)
}

pub(super) fn log_cycle_data_put(
    record: &mut [u8],
    data_block: usize,
    value: u32,
) -> XfsResult<()> {
    let offset = log_cycle_data_offset(record, data_block)?;
    put_be32(record, offset, value)
}
