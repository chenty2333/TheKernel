//! XfsVolume: inode reads, bmap B+trees and physical log scans.

use super::*;

impl XfsVolume {
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
    pub(super) fn inode_extent_blocks(extents: &[XfsExtent]) -> XfsResult<u64> {
        extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(u64::from(extent.block_count))
                .ok_or(XfsError::AddressOutOfRange)
        })
    }

    /// Counts the currently installed attribute-fork allocation in
    /// filesystem blocks, including external BMBT nodes.  Data-fork writers
    /// use this when replacing their own mapping so `di_nblocks` retains the
    /// independent xattr ownership exactly once.
    pub(super) fn attribute_fork_owned_blocks(
        &self,
        number: u64,
        inode: &XfsInode,
    ) -> XfsResult<u64> {
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

    pub(super) fn attr_bmbt_extents(&self, number: u64, fork: &[u8]) -> XfsResult<Vec<XfsExtent>> {
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
    pub(super) fn attr_bmbt_blocks(&self, number: u64) -> XfsResult<Vec<u64>> {
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
    pub(super) fn bmbt_node(
        &self,
        filesystem_block: u64,
        expected_inode: u64,
    ) -> XfsResult<XfsBmbtNode> {
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
}
