//! XfsVolume: allocation-group headers and AG B+tree walks and staging.

use super::*;

impl XfsVolume {
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
    pub(super) fn ag_freelist_from_group(
        &self,
        group: XfsAllocationGroup,
    ) -> XfsResult<XfsAgFreelist> {
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

    pub(super) fn walk_ag_btree(
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

    pub(super) fn walk_ag_special_btree(
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
    pub(super) fn stage_special_btree(
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
}
