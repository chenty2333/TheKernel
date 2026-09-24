//! XfsVolume: probing, opening and quota state.

use super::*;

impl XfsVolume {
    /// Claims and probes one data device.  This is useful for ordinary XFS
    /// images whose journal is internal.  It does not publish a VFS mount.
    pub fn probe(device: MountedBlockDevice) -> XfsResult<Arc<Self>> {
        let data = BlockVolume::new(vec![device.device().clone()]).map_err(XfsError::from)?;
        Self::open_inner(data, None, None, Some(device))
    }

    /// Opens XFS over explicit data/log/realtime volumes.  The caller retains
    /// the mount claims for each source device; this API has no global device
    /// lookup and therefore cannot accidentally attach the wrong log.
    pub fn open(
        data: BlockVolume,
        external_log: Option<BlockVolume>,
        realtime: Option<BlockVolume>,
    ) -> XfsResult<Arc<Self>> {
        Self::open_inner(data, external_log, realtime, None)
    }

    pub(super) fn open_inner(
        data: BlockVolume,
        external_log: Option<BlockVolume>,
        realtime: Option<BlockVolume>,
        data_claim: Option<MountedBlockDevice>,
    ) -> XfsResult<Arc<Self>> {
        let physical = data.geometry().block_size;
        if physical < 264 {
            return Err(XfsError::InvalidSuperblock);
        }
        let mut first = vec![0; physical];
        data.read_blocks(0, &mut first).map_err(XfsError::from)?;
        let superblock = XfsSuperblock::parse(&first)?;
        // `NEEDSREPAIR` is a persistent administrator-visible assertion that
        // metadata is not safe to trust.  It is not a read-only feature bit.
        if superblock.features.needs_repair() {
            return Err(XfsError::CorruptMetadata);
        }
        if !superblock.has_dirv2() {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.block_size as usize % physical != 0
            || superblock.sector_size as usize % physical != 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.log_start == 0 && external_log.is_none() {
            return Err(XfsError::UnsupportedFeature);
        }
        if superblock.log_start != 0 && external_log.is_some() {
            return Err(XfsError::InvalidSuperblock);
        }
        if superblock.realtime_blocks != 0 && realtime.is_none() {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(log) = &external_log
            && log.geometry().block_size != physical
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(rt) = &realtime
            && rt.geometry().block_size != physical
        {
            return Err(XfsError::UnsupportedFeature);
        }
        if let Some(rt) = &realtime {
            // This API is for an explicitly claimed external realtime
            // member.  Internal realtime placement is not silently routed to
            // that member; reject it until a data-member mapping path exists.
            if superblock.realtime_start != 0 {
                return Err(XfsError::UnsupportedFeature);
            }
            let required = superblock
                .realtime_blocks
                .checked_mul(u64::from(superblock.block_size) / rt.geometry().block_size as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if required > rt.geometry().blocks {
                return Err(XfsError::InvalidSuperblock);
            }
            if superblock.realtime_blocks == 0
                || (superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR == 0
                    && (superblock.realtime_bitmap_inode == 0
                        || superblock.realtime_summary_inode == 0))
            {
                return Err(XfsError::InvalidSuperblock);
            }
        }
        // Do not defer log extent validation until recovery: an internal log
        // must lie wholly inside the data device, while an external log is a
        // region beginning at its own block zero.  This is the physical
        // address-space contract used by the circular scanner.
        let log_basic_blocks = u64::from(superblock.log_blocks)
            .checked_mul(u64::from(superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if log_basic_blocks < 2 {
            return Err(XfsError::InvalidSuperblock);
        }
        let log_base = if superblock.log_start == 0 {
            0
        } else {
            superblock
                .log_start
                .checked_mul(u64::from(superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
                .ok_or(XfsError::AddressOutOfRange)?
        };
        let log_member = external_log.as_ref().unwrap_or(&data);
        let log_capacity = log_member
            .geometry()
            .blocks
            .checked_mul((log_member.geometry().block_size / XFS_LOG_BASIC_BLOCK) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if log_base
            .checked_add(log_basic_blocks)
            .ok_or(XfsError::AddressOutOfRange)?
            > log_capacity
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut volume = Arc::new(Self {
            data,
            external_log,
            realtime,
            rtgroup_inodes: Vec::new(),
            superblock,
            replay_lock: SpinMutex::new(()),
            _data_claim: data_claim,
        });
        if superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0
            && superblock.realtime_blocks != 0
        {
            let inner = Arc::get_mut(&mut volume).ok_or(XfsError::NoMemory)?;
            for group in 0..superblock.rtgroup_count {
                inner
                    .rtgroup_inodes
                    .push(inner.rtgroup_metadata_inodes(group)?);
            }
        }
        // Do this before publication, including read-only projections.  A
        // corrupt quota root must not become a mountable metadata view just
        // because no writer has touched it yet.
        if volume.has_quota_accounting() {
            volume.quota_state()?;
        }
        Ok(volume)
    }

    pub const fn superblock(&self) -> XfsSuperblock {
        self.superblock
    }

    pub fn quota_roots(&self) -> XfsQuotaRoots {
        XfsQuotaRoots {
            flags: self.superblock.quota_flags,
            user: (self.superblock.user_quota_inode != 0)
                .then_some(self.superblock.user_quota_inode),
            group: (self.superblock.group_quota_inode != 0)
                .then_some(self.superblock.group_quota_inode),
            project: (self.superblock.project_quota_inode != 0)
                .then_some(self.superblock.project_quota_inode),
        }
    }

    /// Validated native quota view.  A v5 mount never treats the presence of
    /// a quota inode as a policy toggle: the inode itself, its extent map,
    /// and every dquot subsequently addressed through it are media truth.
    pub fn quota_state(&self) -> XfsResult<XfsQuotaState> {
        // v4 stores legacy OQUOTA flags and aliases the group/project quota
        // inode.  This implementation's native dquot path is v5-only, so do
        // not partially emulate Linux's v4 disk-to-memory conversion here.
        if !self.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        let roots = self.quota_roots();
        for (flag, root) in [
            (XFS_UQUOTA_ACCT, roots.user),
            (XFS_GQUOTA_ACCT, roots.group),
            (XFS_PQUOTA_ACCT, roots.project),
        ] {
            if self.superblock.quota_flags & flag != 0 && root.is_none() {
                return Err(XfsError::CorruptMetadata);
            }
        }
        for (account, enforce) in [
            (XFS_UQUOTA_ACCT, XFS_UQUOTA_ENFD),
            (XFS_GQUOTA_ACCT, XFS_GQUOTA_ENFD),
            (XFS_PQUOTA_ACCT, XFS_PQUOTA_ENFD),
        ] {
            if self.superblock.quota_flags & enforce != 0
                && self.superblock.quota_flags & account == 0
            {
                return Err(XfsError::CorruptMetadata);
            }
        }
        for (kind, root) in [(1u8, roots.user), (2, roots.project), (4, roots.group)] {
            if !self.quota_accounting_enabled(kind) {
                continue;
            }
            if let Some(root) = root {
                let inode = self.inode(root)?;
                // dquots are 136-byte records, but their clusters are whole
                // filesystem blocks.  A 4KiB block deliberately contains 30
                // records and 16 bytes of unused tail, so the inode EOF is
                // block-aligned rather than necessarily divisible by 136.
                if inode.version < 3 || inode.mode & 0o170000 != 0o100000 {
                    return Err(XfsError::CorruptMetadata);
                }
                let extents = match inode.data_format {
                    XfsForkFormat::Extents => self.inode_data_extents(root)?,
                    XfsForkFormat::Btree => self.inode_bmbt_extents(root)?,
                    _ => return Err(XfsError::CorruptMetadata),
                };
                if extents.is_empty()
                    || extents
                        .iter()
                        .any(|extent| extent.unwritten || extent.block_count == 0)
                {
                    return Err(XfsError::CorruptMetadata);
                }
                // The exact id/type is verified by dquot() before use.
            }
        }
        Ok(XfsQuotaState { roots })
    }

    pub(super) fn quota_inode_for(&self, quota_type: u8) -> XfsResult<u64> {
        if !self.quota_accounting_enabled(quota_type) {
            return Err(XfsError::UnsupportedFeature);
        }
        let root = match quota_type {
            1 => self.superblock.user_quota_inode,
            2 => self.superblock.project_quota_inode,
            4 => self.superblock.group_quota_inode,
            _ => return Err(XfsError::CorruptMetadata),
        };
        if root == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(root)
    }

    pub(super) const fn quota_flags_for(quota_type: u8) -> Option<(u16, u16)> {
        match quota_type {
            1 => Some((XFS_UQUOTA_ACCT, XFS_UQUOTA_ENFD)),
            2 => Some((XFS_PQUOTA_ACCT, XFS_PQUOTA_ENFD)),
            4 => Some((XFS_GQUOTA_ACCT, XFS_GQUOTA_ENFD)),
            _ => None,
        }
    }

    pub(crate) fn quota_accounting_enabled(&self, quota_type: u8) -> bool {
        Self::quota_flags_for(quota_type)
            .is_some_and(|(account, _)| self.superblock.quota_flags & account != 0)
    }

    pub(super) fn quota_enforcement_enabled(&self, quota_type: u8) -> bool {
        Self::quota_flags_for(quota_type)
            .is_some_and(|(_, enforce)| self.superblock.quota_flags & enforce != 0)
    }

    pub(super) fn dquot_location(&self, quota_type: u8, id: u32) -> XfsResult<(u64, u32, u32)> {
        let inode = self.quota_inode_for(quota_type)?;
        let block_size = u64::from(self.superblock.block_size);
        let per_block = block_size / 136;
        if per_block == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        // XFS allocates dquot clusters one filesystem block at a time; the
        // unused tail of each block is not part of the record address space.
        let file_block = u64::from(id) / per_block;
        let within = (u64::from(id) % per_block)
            .checked_mul(136)
            .ok_or(XfsError::AddressOutOfRange)?;
        let record_end = file_block
            .checked_add(1)
            .and_then(|blocks| blocks.checked_mul(block_size))
            .ok_or(XfsError::AddressOutOfRange)?;
        let quota = self.inode(inode)?;
        if record_end > quota.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let extents = match quota.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        let logical_end = file_block
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .and_then(|start| start.checked_add(136))
            .ok_or(XfsError::AddressOutOfRange)?;
        if logical_end > quota.size || record_end > quota.size {
            return Err(XfsError::AddressOutOfRange);
        }
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + u64::from(extent.block_count)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        if within.checked_add(136).ok_or(XfsError::AddressOutOfRange)? > block_size {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let byte = physical
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .ok_or(XfsError::AddressOutOfRange)?;
        let basic = byte / 512;
        let byte_offset = u32::try_from(byte % 512).map_err(|_| XfsError::AddressOutOfRange)?;
        let block_count = u32::try_from(
            (usize::try_from(byte_offset).map_err(|_| XfsError::AddressOutOfRange)? + 136)
                .div_ceil(512),
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        Ok((basic, byte_offset, block_count))
    }

    pub fn dquot(&self, quota_type: u8, id: u32) -> XfsResult<XfsDquot> {
        self.quota_state()?;
        let (basic, offset, blocks) = self.dquot_location(quota_type, id)?;
        let mut image = vec![
            0;
            usize::try_from(blocks)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(512)
                .ok_or(XfsError::AddressOutOfRange)?
        ];
        self.read_basic_blocks(&self.data, basic, &mut image)?;
        XfsDquot::parse(
            &image[offset as usize..offset as usize + 136],
            id,
            quota_type,
            self.superblock.meta_uuid,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0,
        )
    }

    /// Stages one native dquot, extending the owning quota inode when the
    /// addressed cluster has not been mapped yet.  The extension is not an
    /// in-memory quota cache: its newly allocated block is zeroed before the
    /// mapping can become durable, the bmap/AG images and the first 136-byte
    /// dquot image share this transaction, and the typed DQUOT item supplies
    /// the final LSN/CRC.  This is important for sparse or fragmented quota
    /// files produced by repair/quotaon -- treating an unmapped id as a
    /// permanently absent quota silently bypasses hard limits.
    pub fn stage_dquot_delta(
        &self,
        quota_type: u8,
        id: u32,
        block_delta: i64,
        inode_delta: i64,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if transaction
            .dquots
            .iter()
            .any(|delta| delta.quota_type == quota_type && delta.id == id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        self.quota_state()?;
        let (basic_block, byte_offset, block_count) = match self.dquot_location(quota_type, id) {
            Ok(location) => location,
            // A dquot is addressed by id, not by a preallocated byte slot.
            // Only an absent cluster is growable; malformed mapped metadata
            // keeps its original error and cannot be overwritten by quota
            // initialization.
            Err(XfsError::AddressOutOfRange) => {
                self.stage_dquot_cluster_growth(quota_type, id, transaction)?
            }
            Err(error) => return Err(error),
        };
        let bytes = usize::try_from(block_count)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        // A just-grown cluster has an ordered zero data write in this same
        // transaction.  Sample that staged image instead of stale free-space
        // contents; commit_metadata_transaction validates and writes it
        // before it materializes this typed dquot home item.
        let first_fs_block = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?
            / u64::from(self.superblock.block_size);
        let last_fs_block = basic_block
            .checked_add(u64::from(block_count))
            .and_then(|end| end.checked_mul(XFS_LOG_BASIC_BLOCK as u64))
            .ok_or(XfsError::AddressOutOfRange)?
            .saturating_sub(1)
            / u64::from(self.superblock.block_size);
        let staged_zero = transaction.data_writes.iter().any(|write| {
            write.fs_block >= first_fs_block
                && write.fs_block <= last_fs_block
                && write.after.iter().all(|byte| *byte == 0)
        });
        let mut home = vec![0; bytes];
        if !staged_zero {
            self.read_basic_blocks(&self.data, basic_block, &mut home)?;
        }
        let mut before = home[byte_offset as usize..byte_offset as usize + 136].to_vec();
        // Quota inode growth is preallocated in dquot-cluster units by mkfs
        // and quotaon.  A completely zero slot is the only safe "missing"
        // record we can materialize here: it is initialized in this same log
        // transaction, never represented by an in-memory placeholder.
        if before.iter().all(|byte| *byte == 0) {
            put_be16(&mut before, 0, 0x4451)?;
            before[2] = 1;
            before[3] = quota_type
                | if id != 0
                    && self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0
                {
                    XfsDquot::DQTYPE_BIGTIME
                } else {
                    0
                };
            put_be32(&mut before, 4, id)?;
            before[120..136].copy_from_slice(&self.superblock.meta_uuid.0);
            rewrite_crc32c(&mut before, 108)?;
        }
        let current = XfsDquot::parse(
            &before,
            id,
            quota_type,
            self.superblock.meta_uuid,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME != 0,
        )?;
        let root = if id == 0 {
            current
        } else {
            self.dquot(quota_type, 0)?
        };
        let now = wall_time().as_secs();
        let admitted = current.apply_delta(
            block_delta,
            inode_delta,
            self.quota_enforcement_enabled(quota_type),
            now,
            if root.block_timer == 0 {
                XFS_DQ_DEFAULT_GRACE_SECONDS
            } else {
                root.block_timer
            },
            if root.inode_timer == 0 {
                XFS_DQ_DEFAULT_GRACE_SECONDS
            } else {
                root.inode_timer
            },
        )?;
        let mut after = before.clone();
        put_be64(&mut after, 40, admitted.blocks)?;
        put_be64(&mut after, 48, admitted.inodes)?;
        put_be32(&mut after, 56, admitted.inode_timer)?;
        put_be32(&mut after, 60, admitted.block_timer)?;
        put_be16(&mut after, 64, admitted.inode_warnings)?;
        put_be16(&mut after, 66, admitted.block_warnings)?;
        // LSN and CRC are finalized by the normal DQUOT log/home materializer
        // after the sole record reservation has selected its durable LSN.
        transaction.dquots.push(XfsDquotDelta {
            id,
            quota_type,
            basic_block,
            block_count,
            byte_offset,
            before,
            after,
        });
        Ok(())
    }

    /// Materializes exactly the filesystem block containing `id` when that
    /// block is outside the quota inode's current extent map.  It reuses the
    /// ordinary regular-file extent planner so inline/bmap forks, fragmented
    /// maps, AG free-space accounting and bmap-node growth retain the same
    /// crash protocol as a user write.  The caller still owns the enclosing
    /// transaction and appends its DQUOT item afterwards.
    pub(super) fn stage_dquot_cluster_growth(
        &self,
        quota_type: u8,
        id: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<(u64, u32, u32)> {
        let inode_number = self.quota_inode_for(quota_type)?;
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o100000
            || !matches!(
                inode.data_format,
                XfsForkFormat::Extents | XfsForkFormat::Btree
            )
        {
            return Err(XfsError::CorruptMetadata);
        }
        let block_size = u64::from(self.superblock.block_size);
        let per_block = block_size / 136;
        if per_block == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let file_block = u64::from(id) / per_block;
        let within = (u64::from(id) % per_block)
            .checked_mul(136)
            .ok_or(XfsError::AddressOutOfRange)?;
        let offset = file_block
            .checked_mul(block_size)
            .ok_or(XfsError::AddressOutOfRange)?;

        let old = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        if old.iter().any(|extent| {
            !extent.unwritten
                && file_block >= extent.file_block
                && file_block < extent.file_block + u64::from(extent.block_count)
        }) {
            // A mapped cluster with a short inode size is corrupt rather than
            // an invitation to erase its contents.  dquot_location normally
            // catches this; retain the fail-closed rule if a caller reaches
            // this helper through a malformed size/extent combination.
            return Err(XfsError::CorruptMetadata);
        }

        // prepare_regular_write reserves every AG/bmap resource before it
        // produces a buffer image.  Its data write is deliberately supplied
        // here as an all-zero full cluster, so no post-crash path can expose
        // stale free-space bytes as an initialized dquot record.
        let prepared =
            self.prepare_regular_write(inode_number, offset, self.superblock.block_size as usize)?;
        let mapped = prepared
            .mappings
            .iter()
            .find(|extent| extent.file_block == file_block && extent.block_count != 0)
            .copied()
            .ok_or(XfsError::CorruptMetadata)?;
        if mapped.block_count != 1 || mapped.unwritten {
            return Err(XfsError::CorruptMetadata);
        }
        let before = self.read_data_fs_block(mapped.start_block)?;
        let after = vec![0; self.superblock.block_size as usize];
        transaction.buffers.extend(prepared.metadata.buffers);
        transaction.data_writes.push(XfsStagedDataWrite {
            fs_block: mapped.start_block,
            before,
            after,
        });

        let byte = mapped
            .start_block
            .checked_mul(block_size)
            .and_then(|base| base.checked_add(within))
            .ok_or(XfsError::AddressOutOfRange)?;
        let basic_block = byte / XFS_LOG_BASIC_BLOCK as u64;
        let byte_offset = u32::try_from(byte % XFS_LOG_BASIC_BLOCK as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        let block_count = u32::try_from(
            (usize::try_from(byte_offset).map_err(|_| XfsError::AddressOutOfRange)? + 136)
                .div_ceil(XFS_LOG_BASIC_BLOCK),
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        Ok((basic_block, byte_offset, block_count))
    }

    /// Whether the superblock carries any native quota accounting root.
    /// Writer admission validates those roots and routes their dquot changes
    /// through the same live transaction as ordinary metadata.
    pub fn has_quota_accounting(&self) -> bool {
        self.superblock.quota_flags & (XFS_UQUOTA_ACCT | XFS_GQUOTA_ACCT | XFS_PQUOTA_ACCT) != 0
    }

    /// Reconstructs the statfs allocation counters from a single validated
    /// ownership snapshot per AG.  The caller is responsible for holding the
    /// mount's coherent-read guard when a live log coordinator exists.
    pub fn stat_counts(&self) -> XfsResult<XfsStatCounts> {
        let mut counts = XfsStatCounts::default();
        for ag in 0..self.superblock.ag_count {
            let snapshot = self.ag_ownership_snapshot(ag)?;
            let group = snapshot.group;
            let free_blocks = snapshot
                .free_extents
                .iter()
                .try_fold(0u64, |total, extent| {
                    total
                        .checked_add(u64::from(extent.block_count))
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
            if free_blocks != u64::from(group.free_space.free_blocks) {
                return Err(XfsError::CorruptMetadata);
            }
            let free_inodes = snapshot
                .inode_records
                .iter()
                .try_fold(0u64, |total, record| {
                    total
                        .checked_add(u64::from(record.free_count))
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
            if free_inodes != u64::from(group.inode.free_inode_count) {
                return Err(XfsError::CorruptMetadata);
            }
            let total_inodes = u64::try_from(snapshot.inode_records.len())
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(64)
                .ok_or(XfsError::AddressOutOfRange)?;
            if total_inodes != u64::from(group.inode.inode_count) {
                return Err(XfsError::CorruptMetadata);
            }
            counts.total_blocks = counts
                .total_blocks
                .checked_add(u64::from(group.free_space.length))
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.free_blocks = counts
                .free_blocks
                .checked_add(free_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.total_inodes = counts
                .total_inodes
                .checked_add(total_inodes)
                .ok_or(XfsError::AddressOutOfRange)?;
            counts.free_inodes = counts
                .free_inodes
                .checked_add(free_inodes)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        if counts.total_blocks != self.superblock.data_blocks
            || counts.free_blocks > counts.total_blocks
            || counts.free_inodes > counts.total_inodes
        {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(counts)
    }
}
