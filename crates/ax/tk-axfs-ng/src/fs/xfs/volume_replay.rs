//! XfsVolume: staging log replay and recovery commits.

use super::*;

impl XfsVolume {
    pub(super) fn metadir_child(&self, parent: u64, name: &[u8]) -> XfsResult<u64> {
        let parent_inode = self.inode(parent)?;
        if parent_inode.mode & 0o170000 != 0o040000 || !parent_inode.is_metadata_inode() {
            return Err(XfsError::CorruptMetadata);
        }
        let entry = self
            .directory_entries(parent)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(XfsError::CorruptMetadata)?;
        let child = self.inode(entry.inode)?;
        if !child.is_metadata_inode() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(entry.inode)
    }

    pub(super) fn rtgroup_metadata_inodes(&self, group: u32) -> XfsResult<(u64, u64)> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0
            || group >= self.superblock.rtgroup_count
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let rtgroups = self.metadir_child(self.superblock.metadir_inode, b"rtgroups")?;
        let mut bitmap = alloc::format!("{group}").into_bytes();
        bitmap.extend_from_slice(b".bitmap");
        let mut summary = alloc::format!("{group}").into_bytes();
        summary.extend_from_slice(b".summary");
        let bitmap = self.metadir_child(rtgroups, &bitmap)?;
        let summary = self.metadir_child(rtgroups, &summary)?;
        for (inode_number, metafile_type) in [(bitmap, 5u16), (summary, 6u16)] {
            let inode = self.inode(inode_number)?;
            // The rtgroup loader binds names, metadata-inode type, owning
            // project, and data-fork representation before recovery derives
            // a physical bitmap/summary home from it.
            if inode.mode & 0o170000 != 0o100000
                || !inode.is_metadata_inode()
                || inode.project_id != group
                || inode.metafile_type != Some(metafile_type)
                || !matches!(
                    inode.data_format,
                    XfsForkFormat::Extents | XfsForkFormat::Btree
                )
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        Ok((bitmap, summary))
    }
    /// Replays legacy v4 records only when every logged buffer is a complete
    /// home image.  v4 metadata has neither the v5 LSN nor CRC ownership
    /// fields used by the writable coordinator, so partial-region replay is
    /// not restart-safe.  A full-image operation is: the current home image
    /// is either the captured preimage (write it) or the exact logged image
    /// (a prior FUA write completed); any third state fails closed.
    ///
    /// This intentionally does not manufacture a v4 log tail or publish a
    /// writable log ring.  The caller may expose the verified read
    /// projection after all home images have reached stable storage.
    pub fn replay_v4_whole_image_plan(&self, plan: &XfsRecoveryPlan) -> XfsResult<()> {
        if self.superblock.is_v5() || self.data.geometry().block_size % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        for transaction in &plan.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            if items.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut writes = Vec::<(u64, Vec<u8>)>::new();
            for item in items {
                let XfsReplayItem::Buffer(buffer) = item else {
                    // A v4 inode/dquot/intent item has no durable whole-home
                    // image proof.  Never replay a buffer subset of it.
                    return Err(XfsError::UnsupportedFeature);
                };
                if buffer.flags & XFS_BLF_CANCEL != 0 {
                    return Err(XfsError::UnsupportedFeature);
                }
                let chunks = usize::from(buffer.block_count)
                    .checked_mul(4)
                    .ok_or(XfsError::AddressOutOfRange)?;
                if buffer.block_count == 0
                    || buffer.dirty_chunks.len() != chunks
                    || buffer.chunks.len() != chunks
                    || buffer
                        .dirty_chunks
                        .iter()
                        .enumerate()
                        .any(|(index, chunk)| *chunk != index as u32)
                    || buffer.chunks.iter().any(|chunk| chunk.len() != 128)
                {
                    return Err(XfsError::UnsupportedFeature);
                }
                let end = buffer
                    .block_number
                    .checked_add(u64::from(buffer.block_count))
                    .ok_or(XfsError::AddressOutOfRange)?;
                if end > self.basic_blocks(&self.data)? {
                    return Err(XfsError::AddressOutOfRange);
                }
                let mut image = Vec::new();
                image
                    .try_reserve_exact(chunks.checked_mul(128).ok_or(XfsError::AddressOutOfRange)?)
                    .map_err(|_| XfsError::NoMemory)?;
                for chunk in buffer.chunks {
                    image.extend_from_slice(&chunk);
                }
                if writes
                    .iter()
                    .any(|(block, _)| *block == buffer.block_number)
                {
                    return Err(XfsError::CorruptMetadata);
                }
                writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                writes.push((buffer.block_number, image));
            }
            writes.sort_unstable_by_key(|(block, _)| *block);
            for (block, image) in &writes {
                let mut current = vec![0; image.len()];
                self.read_basic_blocks(&self.data, *block, &mut current)?;
                if current == *image {
                    continue;
                }
                // Full-image redo does not depend on an on-disk LSN: the
                // authenticated log image replaces any older home image, and
                // a completed FUA write is recognized by byte equality.
                self.write_basic_blocks_fua(&self.data, *block, image)?;
            }
            self.data.flush().map_err(XfsError::from)?;
        }
        Ok(())
    }

    /// Reserves and encodes one native physical log record.  No I/O occurs in
    /// this phase; if later persistence fails the returned object remains the
    /// sole retry token for the already consumed ring grant.
    pub(crate) fn prepare_live_log_commit(
        &self,
        ring: &mut XfsLogRing,
        transaction_id: u32,
        tail_lsn: u64,
        operations: &[XfsLogOperation],
    ) -> XfsResult<XfsPreparedLogCommit> {
        if !self.superblock.is_v5()
            || self.data.geometry().block_size % XFS_LOG_BASIC_BLOCK != 0
            || transaction_id == 0
            || operations.is_empty()
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let region_blocks = self.log_region_blocks()?;
        if ring.blocks() != region_blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let iclog_bytes = XfsLogRecordHeader::minimal_iclog_bytes(operations)?;
        let header = XfsLogRecordHeader::for_operations(
            ring.cycle(),
            ring.next_lsn(),
            tail_lsn,
            ring.previous_record(),
            self.superblock.uuid,
            iclog_bytes,
            operations,
        )?;
        let mut record = header.encode(operations)?;
        let reservation = ring.reserve(record.len())?;
        if reservation.lsn != header.lsn {
            return Err(XfsError::CorruptMetadata);
        }
        ring.stamp_record(&reservation, &mut record)?;
        Ok(XfsPreparedLogCommit {
            reservation,
            transaction_id,
            record,
        })
    }

    /// Writes every physical fragment with FUA, flushes the log member, then
    /// publishes the transaction to the AIL.  Allocation for AIL insertion is
    /// completed before the first device write.  Therefore any I/O failure
    /// leaves `prepared` intact and absent from AIL, permitting an exact retry
    /// of the same LSN and bytes rather than a second transaction.
    pub(crate) fn persist_live_log_commit(
        &self,
        prepared: &XfsPreparedLogCommit,
        ail: &mut XfsAil,
    ) -> XfsResult<()> {
        if prepared.record.len()
            != prepared.reservation.record_blocks as usize * XFS_LOG_BASIC_BLOCK
        {
            return Err(XfsError::CorruptMetadata);
        }
        let ail_entry = XfsAilEntry {
            lsn: prepared.reservation.lsn,
            end_lsn: prepared.reservation.end_lsn(self.log_region_blocks()?)?,
            transaction_id: prepared.transaction_id,
            checkpoint_homes: Vec::new(),
        };
        ail.reserve_insert(&ail_entry)?;
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let fragments = prepared.reservation.fragments()?;
        let mut offset = 0usize;
        for fragment in fragments {
            let bytes = fragment.blocks as usize * XFS_LOG_BASIC_BLOCK;
            let end = offset
                .checked_add(bytes)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = base
                .checked_add(fragment.start_block as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            self.write_basic_blocks_fua(log, start, &prepared.record[offset..end])?;
            offset = end;
        }
        if offset != prepared.record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        log.flush().map_err(XfsError::from)?;
        ail.insert_reserved(ail_entry)
    }

    /// Writes the terminal unmount record without placing it in the AIL:
    /// unlike metadata transactions it has no home image to checkpoint.  Its
    /// caller may move the tail to the record end only after all preceding
    /// metadata was pushed and every member device was made durable.
    pub(crate) fn persist_clean_unmount_record(
        &self,
        prepared: &XfsPreparedLogCommit,
    ) -> XfsResult<()> {
        if prepared.record.len()
            != prepared.reservation.record_blocks as usize * XFS_LOG_BASIC_BLOCK
        {
            return Err(XfsError::CorruptMetadata);
        }
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let mut offset = 0usize;
        for fragment in prepared.reservation.fragments()? {
            let bytes = fragment.blocks as usize * XFS_LOG_BASIC_BLOCK;
            let end = offset
                .checked_add(bytes)
                .ok_or(XfsError::AddressOutOfRange)?;
            let start = base
                .checked_add(fragment.start_block as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            self.write_basic_blocks_fua(log, start, &prepared.record[offset..end])?;
            offset = end;
        }
        if offset != prepared.record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        log.flush().map_err(XfsError::from)
    }

    /// Pushes AIL entries through a caller-provided home-write routine.  The
    /// tail moves only after every selected item has completed and the caller
    /// has made its home writes durable; a failed push preserves both AIL and
    /// ring tail for retry.
    pub(crate) fn checkpoint_live_log(
        &self,
        ring: &mut XfsLogRing,
        ail: &mut XfsAil,
        through_lsn: u64,
        mut push_home: impl FnMut(&XfsAilEntry) -> XfsResult<()>,
    ) -> XfsResult<()> {
        let count = ail
            .entries
            .partition_point(|entry| entry.lsn <= through_lsn);
        if count == 0 {
            return Ok(());
        }
        for entry in &ail.entries[..count] {
            push_home(entry)?;
        }
        // Every selected home image is FUA-written by the pusher.  The
        // device flush is the final fence: advancing the log tail before it
        // succeeds would make a power loss unrecoverable.
        self.data.flush().map_err(XfsError::from)?;
        // A record is reclaimable only from the first free position *after*
        // its physical log image, never from the record header itself.
        ring.checkpoint_tail(ail.entries[count - 1].end_lsn)?;
        let _ = ail.checkpoint_through(through_lsn);
        Ok(())
    }

    /// Starts restartable replay for a plan consisting solely of supported
    /// buffer items.  Mixed transactions are rejected by the plan decoder;
    /// callers must not attempt to expose a volume after replaying only its
    /// directory or inode subset.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn begin_buffer_recovery(
        self: &Arc<Self>,
        plan: &XfsRecoveryPlan,
    ) -> XfsResult<XfsRecoverySession> {
        Ok(XfsRecoverySession {
            volume: self.clone(),
            commits: plan.prepare_buffer_commits(self)?,
            next: 0,
        })
    }

    /// Adds one native inode log item to an all-or-nothing metadata batch.
    /// The logged buffer address is cross-checked against the inode number so
    /// a journal item cannot redirect an otherwise valid inode core.
    pub fn stage_inode_log_replay(
        &self,
        item: &XfsInodeReplayItem,
        lsn: u64,
        order: XfsLogByteOrder,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (ag, agino) = self.split_inode_number(item.inode)?;
        let fs_block = u64::from(ag)
            .checked_mul(u64::from(self.superblock.ag_blocks))
            .and_then(|base| base.checked_add(agino >> self.superblock.inodes_per_block_log))
            .ok_or(XfsError::AddressOutOfRange)?;
        let inode_offset =
            usize::try_from(agino & (u64::from(self.superblock.inodes_per_block) - 1))
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(self.superblock.inode_size as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
        let basic_per_fs = u64::from(self.superblock.block_size) / 512;
        let expected_byte = fs_block
            .checked_mul(basic_per_fs)
            .and_then(|block| block.checked_mul(512))
            .and_then(|base| base.checked_add(inode_offset as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let logged_byte = item
            .block_number
            .checked_mul(512)
            .and_then(|base| base.checked_add(u64::from(item.byte_offset)))
            .ok_or(XfsError::AddressOutOfRange)?;
        let image_bytes = usize::try_from(item.block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        if item.block_count == 0
            || expected_byte != logged_byte
            || usize::try_from(item.byte_offset)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_add(self.superblock.inode_size as usize)
                .filter(|end| *end <= image_bytes)
                .is_none()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut after = vec![0; image_bytes];
        self.read_basic_blocks(&self.data, item.block_number, &mut after)?;
        let before = after.clone();
        let offset = item.byte_offset as usize;
        let inode = item.materialize_home_inode(
            &after[offset..offset + self.superblock.inode_size as usize],
            lsn,
            self.superblock.is_v5().then_some(self.superblock.uuid),
            order,
        )?;
        after[offset..offset + inode.len()].copy_from_slice(&inode);
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Inode,
            basic_block: item.block_number,
            before,
            after,
        });
        Ok(())
    }

    /// Adds a dquot home update only after binding its physical log address to
    /// the selected on-disk quota inode and extent map.
    pub fn stage_dquot_log_replay(
        &self,
        item: &XfsDquotReplayItem,
        lsn: u64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let bigtime = self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0;
        let dquot = item.parse_disk_dquot(
            self.superblock.is_v5(),
            self.superblock.is_v5().then_some(self.superblock.meta_uuid),
            bigtime,
        )?;
        let (basic_block, byte_offset, block_count) =
            self.dquot_location(dquot.quota_type, item.id)?;
        if item.block_number != basic_block
            || item.byte_offset != byte_offset
            || item.block_count != block_count
        {
            return Err(XfsError::CorruptMetadata);
        }
        let image_bytes = usize::try_from(block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        let offset = byte_offset as usize;
        let mut image = vec![0; image_bytes];
        self.read_basic_blocks(&self.data, item.block_number, &mut image)?;
        let home = slice(&image, offset, 136)?;
        let _ = XfsDquot::parse(
            home,
            item.id,
            dquot.quota_type,
            self.superblock.meta_uuid,
            bigtime,
        )?;
        if be64(home, 112)? >= lsn {
            return Ok(());
        }
        let payload = item.materialize_home_dquot(
            home,
            lsn,
            self.superblock.is_v5(),
            self.superblock.is_v5().then_some(self.superblock.meta_uuid),
            bigtime,
        )?;
        transaction.dquots.push(XfsDquotDelta {
            id: item.id,
            quota_type: dquot.quota_type,
            basic_block: item.block_number,
            block_count: item.block_count,
            byte_offset: item.byte_offset,
            before: home.to_vec(),
            after: payload,
        });
        Ok(())
    }

    /// Starts typed semantic recovery for pending EFI operations.  The plan
    /// is completely decoded and every intent/done pair is resolved before
    /// the session is returned; pending RUI/CUI/BUI and mixed transactions
    /// fail closed because their metadata writers are not implemented.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn begin_intent_recovery(
        self: &Arc<Self>,
        plan: &XfsRecoveryPlan,
    ) -> XfsResult<XfsIntentRecoverySession> {
        if !self.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(XfsIntentRecoverySession {
            volume: self.clone(),
            steps: plan.pending_extent_free_recovery()?,
            next: 0,
            prepared: None,
        })
    }

    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(super) fn prepare_pending_extent_free_recovery(
        &self,
        step: &XfsPendingExtentFreeRecovery,
    ) -> XfsResult<Option<XfsRecoveryCommit>> {
        if step.lsn == 0 || step.extents.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let ag_blocks = u64::from(self.superblock.ag_blocks);
        let mut groups = Vec::<(u32, Vec<(u32, u32)>)>::new();
        for &(start_block, block_count) in &step.extents {
            let end = start_block
                .checked_add(u64::from(block_count))
                .ok_or(XfsError::AddressOutOfRange)?;
            if block_count == 0 || end > self.superblock.data_blocks {
                return Err(XfsError::AddressOutOfRange);
            }
            let mut start = start_block;
            while start < end {
                let ag =
                    u32::try_from(start / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.superblock.ag_count {
                    return Err(XfsError::AddressOutOfRange);
                }
                let relative =
                    u32::try_from(start % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let available = ag_blocks
                    .checked_sub(u64::from(relative))
                    .ok_or(XfsError::CorruptMetadata)?;
                let length = u32::try_from((end - start).min(available))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if length == 0 {
                    return Err(XfsError::CorruptMetadata);
                }
                if let Some((_, frees)) = groups.iter_mut().find(|(candidate, _)| *candidate == ag)
                {
                    frees.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    frees.push((relative, length));
                } else {
                    groups.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    groups.push((ag, vec![(relative, length)]));
                }
                start = start
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for (ag, frees) in groups {
            let staged = self.stage_recovery_extent_frees(ag, &frees)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        if metadata.buffers.is_empty() {
            return Ok(None);
        }
        let items = metadata.log_items()?;
        self.prepare_recovery_commit(step.lsn, &items).map(Some)
    }

    /// Resolves the only realtime BUF homes this recovery path can prove:
    /// v5 rtgroup bitmap/summary metadata on the data member.  Pre-rtgroup
    /// native-endian words have neither an LSN nor a CRC, and arbitrary
    /// realtime data needs owning-inode/refcount replay, so both remain out
    /// of this narrow admission set.
    pub(super) fn realtime_replay_home(
        &self,
        item: &XfsBufferReplayItem,
    ) -> XfsResult<(u64, u32, u64)> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        // The homes below live in metadata inodes on the data member, but a
        // realtime transaction is not admitted without the claimed realtime
        // member whose geometry was validated at open time.
        let realtime = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if realtime.geometry().block_size == 0 || realtime.geometry().blocks == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let sectors = u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64;
        if sectors == 0
            || item.block_number % sectors != 0
            || u64::from(item.block_count) != sectors
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let physical = item.block_number / sectors;
        for group in 0..self.superblock.rtgroup_count {
            let (rtgroups, extents, _bits, bitmap_blocks) = self.realtime_layout(group)?;
            if !rtgroups {
                return Err(XfsError::UnsupportedFeature);
            }
            let (bitmap_owner, summary_owner) = *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::CorruptMetadata)?;
            for logical in 0..bitmap_blocks {
                if self.realtime_metadata_block(u64::MAX, group, logical)? == physical {
                    return Ok((physical, 0x424d_505a, bitmap_owner));
                }
            }
            let levels = 64u64
                .checked_sub(extents.leading_zeros() as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            let slots = levels
                .checked_mul(bitmap_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            let words = (u64::from(self.superblock.block_size)
                .checked_sub(
                    u64::try_from(XFS_RTBUF_HEADER_BYTES)
                        .map_err(|_| XfsError::AddressOutOfRange)?,
                )
                .ok_or(XfsError::InvalidSuperblock)?)
                / 4;
            if words == 0 {
                return Err(XfsError::InvalidSuperblock);
            }
            for logical in 0..slots.div_ceil(words) {
                if self.realtime_metadata_block(u64::MAX - 1, group, logical)? == physical {
                    return Ok((physical, 0x5355_4d59, summary_owner));
                }
            }
        }
        Err(XfsError::UnsupportedFeature)
    }

    pub(super) fn verify_realtime_replay_images(
        &self,
        item: &XfsBufferReplayItem,
        home: &[u8],
        image: &[u8],
    ) -> XfsResult<u64> {
        let (physical, magic, owner) = self.realtime_replay_home(item)?;
        if home.len() != self.superblock.block_size as usize || image.len() != home.len() {
            return Err(XfsError::CorruptMetadata);
        }
        self.verify_rtgroup_buffer(home, magic, owner, physical)?;
        self.verify_rtgroup_buffer(image, magic, owner, physical)?;
        item.home_lsn(home)?.ok_or(XfsError::UnsupportedFeature)
    }

    /// Materializes one committed transaction's buffer items against their
    /// current home blocks.  It is the only constructor for a writable
    /// recovery commit: the log LSN, basic-block address and dirty-chunk map
    /// stay bound together, rather than allowing a VFS operation to submit an
    /// arbitrary sector write under an XFS name.
    pub fn prepare_recovery_commit<'a>(
        &self,
        lsn: u64,
        items: impl IntoIterator<Item = &'a XfsBufferReplayItem>,
    ) -> XfsResult<XfsRecoveryCommit> {
        if !self.superblock.is_v5() || lsn == 0 || self.data.geometry().block_size % 512 != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        let mut writes = Vec::new();
        for item in items {
            let blocks = u64::from(item.block_count);
            let end = item
                .block_number
                .checked_add(blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if blocks == 0 || end > self.basic_blocks(&self.data)? {
                return Err(XfsError::AddressOutOfRange);
            }
            let bytes = usize::from(item.block_count)
                .checked_mul(512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut home = vec![0; bytes];
            self.read_basic_blocks(&self.data, item.block_number, &mut home)?;
            // Materialize first: a newly allocated tree block has arbitrary
            // pre-replay bytes and therefore no decodable home magic/LSN, but
            // its logged image is self-identifying and carries the new LSN.
            let image =
                item.materialize_home_image(&home, lsn, self.superblock.inode_size as usize)?;
            let is_btree = item.metadata_type()? == XfsMetadataBufferType::Btree;
            let is_realtime = item.metadata_type()? == XfsMetadataBufferType::Realtime;
            if is_realtime {
                let existing = self.verify_realtime_replay_images(item, &home, &image)?;
                if existing >= lsn {
                    continue;
                }
                writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                writes.push(XfsHomeWriteDescriptor {
                    basic_block: item.block_number,
                    bytes: image,
                    lsn,
                    item: item.clone(),
                });
                continue;
            }
            // Recovery is idempotent only for metadata with an on-disk LSN.
            match item.home_lsn(&home) {
                Ok(Some(existing)) if existing >= lsn => continue,
                Ok(Some(_)) => {}
                Ok(None) => return Err(XfsError::UnsupportedFeature),
                Err(XfsError::CorruptMetadata)
                    if is_btree && XfsBufferReplayItem::btree_crc_lsn_offsets(&image).is_ok() => {}
                Err(error) => return Err(error),
            }
            writes.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            writes.push(XfsHomeWriteDescriptor {
                basic_block: item.block_number,
                bytes: image,
                lsn,
                item: item.clone(),
            });
        }
        writes.sort_unstable_by_key(|write| write.basic_block);
        let mut prior = 0u64;
        for write in &writes {
            if write.basic_block < prior {
                return Err(XfsError::CorruptMetadata);
            }
            prior = write
                .basic_block
                .checked_add((write.bytes.len() / 512) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(XfsRecoveryCommit { lsn, writes })
    }

    /// Applies a fully prepared recovery write set in basic-block order.  The
    /// caller constructs descriptors only from committed log transactions;
    /// this routine never accepts an unframed mutation as recovery input.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(crate) fn apply_recovery_commit(&self, commit: &XfsRecoveryCommit) -> XfsResult<()> {
        if !self.superblock.is_v5() || self.data.geometry().block_size % 512 != 0 || commit.lsn == 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let _serial = self.replay_lock.lock();
        let mut writes = commit.writes.clone();
        writes.sort_unstable_by_key(|write| write.basic_block);
        let mut prior_end = 0u64;
        for write in &writes {
            if write.lsn != commit.lsn
                || write.item.block_number != write.basic_block
                || write.bytes.is_empty()
                || write.bytes.len() % 512 != 0
                || write.basic_block < prior_end
            {
                return Err(XfsError::CorruptMetadata);
            }
            prior_end = write
                .basic_block
                .checked_add((write.bytes.len() / 512) as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if prior_end > self.basic_blocks(&self.data)? {
                return Err(XfsError::AddressOutOfRange);
            }
        }
        // Validate every admitted realtime home before the first FUA.  A
        // malformed later bitmap/summary record must leave this log commit
        // wholly retryable rather than checkpointing an earlier realtime
        // home and then failing mid-set.
        for write in &writes {
            if write.item.metadata_type()? != XfsMetadataBufferType::Realtime {
                continue;
            }
            let mut current = vec![0; write.bytes.len()];
            self.read_basic_blocks(&self.data, write.basic_block, &mut current)?;
            let _ = self.verify_realtime_replay_images(&write.item, &current, &write.bytes)?;
        }
        for write in &writes {
            let mut current = vec![0; write.bytes.len()];
            self.read_basic_blocks(&self.data, write.basic_block, &mut current)?;
            // A completed FUA write may be encountered after the subsequent
            // flush failed.  Its LSN proves the home block is already newer
            // than this replay record, so skipping it preserves idempotence.
            let is_btree = write.item.metadata_type()? == XfsMetadataBufferType::Btree;
            let is_realtime = write.item.metadata_type()? == XfsMetadataBufferType::Realtime;
            if is_realtime {
                let existing =
                    self.verify_realtime_replay_images(&write.item, &current, &write.bytes)?;
                if existing >= write.lsn {
                    continue;
                }
                self.write_basic_blocks_fua(&self.data, write.basic_block, &write.bytes)?;
                continue;
            }
            match write.item.home_lsn(&current) {
                Ok(Some(existing)) if existing >= write.lsn => continue,
                Ok(Some(_)) => {}
                Ok(None) => return Err(XfsError::UnsupportedFeature),
                // A log record may have committed before a freshly allocated
                // BMBT/AG-tree block received its first home write.  Its old
                // contents have no typed LSN; the prepared logged image is
                // the authority for this one initial installation.
                Err(XfsError::CorruptMetadata)
                    if is_btree
                        && XfsBufferReplayItem::btree_crc_lsn_offsets(&write.bytes).is_ok() => {}
                Err(error) => return Err(error),
            }
            self.write_basic_blocks_fua(&self.data, write.basic_block, &write.bytes)?;
        }
        self.data.flush().map_err(XfsError::from)
    }
}
