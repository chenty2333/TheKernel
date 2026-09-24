//! XfsVolume: directory reads and staged directory mutations.

use super::*;

impl XfsVolume {
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
    pub(super) fn stage_directory_inode_extents(
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

    pub(super) fn stage_directory_block_reserved(
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
}
