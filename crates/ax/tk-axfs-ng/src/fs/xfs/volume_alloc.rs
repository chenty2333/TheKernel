//! XfsVolume: extent and inode allocation, regular-file writes and truncation.

use super::*;

impl XfsVolume {
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
    pub(super) fn stage_recovery_extent_frees(
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
                .find(|(candidate, ..)| *candidate == node_ag)
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
                .find(|(candidate, ..)| *candidate == node_ag)
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

    pub(super) fn write_prepared_regular_at_live(
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
                .find(|(candidate, ..)| *candidate == node_ag)
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
                .find(|(candidate, ..)| *candidate == node_ag)
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
                groups.iter_mut().find(|(candidate, ..)| *candidate == ag)
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
                    groups.iter_mut().find(|(candidate, ..)| *candidate == ag)
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
                groups.iter_mut().find(|(candidate, ..)| *candidate == ag)
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
    pub(super) fn stage_combined_inode_and_free_space_trees(
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

    pub(super) fn stage_inode_trees(
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
    pub(super) fn reserve_bmap_metadata_blocks(
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

    pub(super) fn stage_extent_delta(
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
    pub(super) fn stage_extent_delta_with_metadata(
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

    pub(super) fn stage_free_space_trees(
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
}
