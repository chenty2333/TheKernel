//! XfsVolume: block I/O on the data, log and realtime devices.

use super::*;

impl XfsVolume {
    /// Decodes the persistent `xfs_dev_t` carried by a character or block
    /// device inode. XFS stores Linux's packed 32-bit device representation,
    /// so the VFS can retain it directly as its `DeviceId` bit pattern.
    pub fn inode_rdev(&self, number: u64) -> XfsResult<u32> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !matches!(inode.mode & 0o170000, 0o020000 | 0o060000)
            || inode.data_format != XfsForkFormat::Device
        {
            return Err(XfsError::UnsupportedFeature);
        }
        be32(inode.data_fork(&raw)?, 0)
    }

    pub(super) fn inode_and_bytes(&self, number: u64) -> XfsResult<(XfsInode, Vec<u8>)> {
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
        let raw = slice(&block, offset, self.superblock.inode_size as usize)?.to_vec();
        let inode = XfsInode::parse(
            number,
            &raw,
            self.superblock.is_v5().then_some(self.superblock.uuid),
            (self.superblock.features.incompat & XfsFeatures::INCOMPAT_META_UUID != 0)
                .then_some(self.superblock.meta_uuid),
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0,
        )?;
        Ok((inode, raw))
    }

    /// Resolves one mapped file block to its physical filesystem block after
    /// validating the inode's complete extent representation.  Metadata
    /// verifiers use the result to bind v5 blkno fields to their home.
    pub(super) fn inode_physical_file_block(&self, number: u64, file_block: u64) -> XfsResult<u64> {
        let inode = self.inode(number)?;
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    pub(super) fn decode_extent_fork(
        &self,
        fork: &[u8],
        expected: usize,
    ) -> XfsResult<Vec<XfsExtent>> {
        let bytes = expected.checked_mul(16).ok_or(XfsError::CorruptMetadata)?;
        if fork.len() < bytes {
            return Err(XfsError::CorruptMetadata);
        }
        let mut extents = Vec::new();
        extents
            .try_reserve_exact(expected)
            .map_err(|_| XfsError::NoMemory)?;
        let mut prior_end = 0u64;
        for index in 0..expected {
            let extent = XfsExtent::parse(slice(fork, index * 16, 16)?)?;
            if index != 0 && extent.file_block < prior_end {
                return Err(XfsError::CorruptMetadata);
            }
            prior_end = extent
                .file_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
            let physical_end = extent
                .start_block
                .checked_add(extent.block_count as u64)
                .ok_or(XfsError::CorruptMetadata)?;
            if physical_end > self.superblock.data_blocks {
                return Err(XfsError::CorruptMetadata);
            }
            extents.push(extent);
        }
        Ok(extents)
    }

    pub(super) fn split_inode_number(&self, number: u64) -> XfsResult<(u32, u64)> {
        let agino_bits =
            self.superblock.ag_block_log as u32 + self.superblock.inodes_per_block_log as u32;
        if agino_bits >= 63 {
            return Err(XfsError::InvalidSuperblock);
        }
        let ag = (number >> agino_bits) as u32;
        if ag >= self.superblock.ag_count {
            return Err(XfsError::AddressOutOfRange);
        }
        let agino_mask = (1u64 << agino_bits) - 1;
        Ok((ag, number & agino_mask))
    }

    pub(super) fn read_data_fs_block(&self, block: u64) -> XfsResult<Vec<u8>> {
        if block >= self.superblock.data_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        self.read_from_volume(&self.data, block, self.superblock.block_size as usize)
    }

    /// Reads one filesystem-sized realtime block.  Realtime addressing is
    /// deliberately kept separate from the data-device AG address space.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn read_realtime_fs_block(&self, block: u64) -> XfsResult<Vec<u8>> {
        let volume = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if block >= self.superblock.realtime_blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        self.read_from_volume(volume, block, self.superblock.block_size as usize)
    }

    /// Resolves a realtime bitmap/summary logical block through its metadata
    /// inode.  These inodes live on the data device; only the bits they
    /// contain describe allocation on the separate realtime member.
    pub(super) fn realtime_metadata_block(
        &self,
        inode_number: u64,
        group: u32,
        file_block: u64,
    ) -> XfsResult<u64> {
        if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            let (bitmap, summary) = *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::AddressOutOfRange)?;
            let inode_number = if inode_number == u64::MAX {
                bitmap
            } else if inode_number == u64::MAX - 1 {
                summary
            } else {
                return Err(XfsError::CorruptMetadata);
            };
            return self.inode_physical_file_block(inode_number, file_block);
        }
        let inode = self.inode(inode_number)?;
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::CorruptMetadata),
        };
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn read_realtime_bitmap_block(&self, file_block: u64) -> XfsResult<Vec<u8>> {
        let inode = if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            u64::MAX
        } else {
            self.superblock.realtime_bitmap_inode
        };
        self.read_data_fs_block(self.realtime_metadata_block(inode, 0, file_block)?)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn read_realtime_summary_block(&self, file_block: u64) -> XfsResult<Vec<u8>> {
        let inode = if self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0 {
            u64::MAX - 1
        } else {
            self.superblock.realtime_summary_inode
        };
        self.read_data_fs_block(self.realtime_metadata_block(inode, 0, file_block)?)
    }

    pub(super) fn realtime_layout(&self, group: u32) -> XfsResult<(bool, u64, u64, u64)> {
        let rtgroups = self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0;
        let payload = u64::try_from(
            (self.superblock.block_size as usize)
                .checked_sub(realtime_payload_offset(rtgroups))
                .ok_or(XfsError::InvalidSuperblock)?,
        )
        .map_err(|_| XfsError::AddressOutOfRange)?;
        let bits_per_block = payload.checked_mul(8).ok_or(XfsError::AddressOutOfRange)?;
        if rtgroups {
            if group >= self.superblock.rtgroup_count {
                return Err(XfsError::AddressOutOfRange);
            }
            let first = u64::from(group)
                .checked_mul(u64::from(self.superblock.rtgroup_extents))
                .ok_or(XfsError::AddressOutOfRange)?;
            let extents = (self.superblock.realtime_extents - first)
                .min(u64::from(self.superblock.rtgroup_extents));
            if extents == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            Ok((
                true,
                extents,
                bits_per_block,
                extents.div_ceil(bits_per_block),
            ))
        } else {
            if group != 0
                || self.superblock.realtime_extents == 0
                || self.superblock.realtime_bitmap_blocks == 0
            {
                return Err(XfsError::AddressOutOfRange);
            }
            let required = self.superblock.realtime_extents.div_ceil(bits_per_block);
            if required != u64::from(self.superblock.realtime_bitmap_blocks) {
                return Err(XfsError::CorruptMetadata);
            }
            Ok((
                false,
                self.superblock.realtime_extents,
                bits_per_block,
                required,
            ))
        }
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn stage_realtime_image(
        &self,
        physical: u64,
        before: Vec<u8>,
        after: Vec<u8>,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if before.len() != self.superblock.block_size as usize || after.len() != before.len() {
            return Err(XfsError::CorruptMetadata);
        }
        if let Some(existing) = transaction
            .realtime_writes
            .iter_mut()
            .find(|write| write.fs_block == physical)
        {
            if existing.before != before {
                return Err(XfsError::CorruptMetadata);
            }
            existing.after = after;
        } else {
            transaction.realtime_writes.push(XfsStagedDataWrite {
                fs_block: physical,
                before,
                after,
            });
        }
        Ok(())
    }

    pub(super) fn verify_rtgroup_buffer(
        &self,
        bytes: &[u8],
        magic: u32,
        owner: u64,
        physical: u64,
    ) -> XfsResult<()> {
        let checksum_uuid =
            if self.superblock.features.incompat & XfsFeatures::INCOMPAT_META_UUID != 0 {
                self.superblock.meta_uuid
            } else {
                self.superblock.uuid
            };
        if be32(bytes, 0)? != magic
            || be64(bytes, 8)? != owner
            || be64(bytes, 16)?
                != physical
                    .checked_mul(u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            || slice(bytes, 32, 16)? != checksum_uuid.0
        {
            return Err(XfsError::CorruptMetadata);
        }
        verify_crc32c(bytes, 4)
    }

    /// Rebuild the complete rtsummary index for one allocation domain.  A
    /// summary counter is keyed by `(floor(log2(run length)), bitmap block
    /// containing run start)`, not by a longest-run approximation.  Scanning
    /// all bitmap blocks also handles a run spanning a bitmap-block boundary.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn materialize_realtime_summary(
        &self,
        group: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (rtgroups, extents, bits_per_block, bitmap_blocks) = self.realtime_layout(group)?;
        let levels = 64u64
            .checked_sub(extents.leading_zeros() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let slots = levels
            .checked_mul(bitmap_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        let words_per_block = (u64::from(self.superblock.block_size)
            - u64::try_from(realtime_payload_offset(rtgroups))
                .map_err(|_| XfsError::AddressOutOfRange)?)
            / 4;
        if words_per_block == 0 {
            return Err(XfsError::InvalidSuperblock);
        }
        let summary_blocks = slots.div_ceil(words_per_block);
        let bitmap_inode = if rtgroups {
            u64::MAX
        } else {
            self.superblock.realtime_bitmap_inode
        };
        let summary_inode = if rtgroups {
            u64::MAX - 1
        } else {
            self.superblock.realtime_summary_inode
        };
        let (bitmap_owner, summary_owner) = if rtgroups {
            *self
                .rtgroup_inodes
                .get(group as usize)
                .ok_or(XfsError::AddressOutOfRange)?
        } else {
            (bitmap_inode, summary_inode)
        };
        let mut bitmap = Vec::new();
        bitmap
            .try_reserve_exact(
                usize::try_from(bitmap_blocks).map_err(|_| XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        for logical in 0..bitmap_blocks {
            let physical = self.realtime_metadata_block(bitmap_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x424d_505a, bitmap_owner, physical)?;
            }
            let image = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or(before);
            bitmap.push(image);
        }
        let mut counters =
            vec![0u32; usize::try_from(slots).map_err(|_| XfsError::AddressOutOfRange)?];
        let mut bit = 0u64;
        while bit < extents {
            let block =
                usize::try_from(bit / bits_per_block).map_err(|_| XfsError::AddressOutOfRange)?;
            let local = bit % bits_per_block;
            if !realtime_bitmap_bit(&bitmap[block], local, rtgroups)? {
                bit += 1;
                continue;
            }
            let start = bit;
            loop {
                bit += 1;
                if bit == extents {
                    break;
                }
                let next_block = usize::try_from(bit / bits_per_block)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if !realtime_bitmap_bit(&bitmap[next_block], bit % bits_per_block, rtgroups)? {
                    break;
                }
            }
            let length = bit - start;
            let level = u64::from(63 - length.leading_zeros());
            let bitmap_block = start / bits_per_block;
            let slot = level
                .checked_mul(bitmap_blocks)
                .and_then(|base| base.checked_add(bitmap_block))
                .ok_or(XfsError::AddressOutOfRange)?;
            let counter = counters
                .get_mut(usize::try_from(slot).map_err(|_| XfsError::AddressOutOfRange)?)
                .ok_or(XfsError::CorruptMetadata)?;
            *counter = counter.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        for logical in 0..summary_blocks {
            let physical = self.realtime_metadata_block(summary_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x5355_4d59, summary_owner, physical)?;
            }
            let mut after = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or_else(|| before.clone());
            let start = logical
                .checked_mul(words_per_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = (start + words_per_block).min(slots);
            for slot in start..end {
                set_realtime_summary_counter(
                    &mut after,
                    usize::try_from(slot - start).map_err(|_| XfsError::AddressOutOfRange)?,
                    counters[usize::try_from(slot).map_err(|_| XfsError::AddressOutOfRange)?],
                    rtgroups,
                )?;
            }
            self.stage_realtime_image(physical, before, after, transaction)?;
        }
        Ok(())
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn stage_realtime_bitmap_delta(
        &self,
        first_bit: u64,
        count: u64,
        allocate: bool,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let rtgroups = self.superblock.features.incompat & XfsFeatures::INCOMPAT_METADIR != 0;
        let (_, _, bits, _) = self.realtime_layout(0)?;
        let end = first_bit
            .checked_add(count)
            .ok_or(XfsError::AddressOutOfRange)?;
        if count == 0 || end > self.superblock.realtime_extents {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut cursor = first_bit;
        while cursor < end {
            let group = if rtgroups {
                u32::try_from(cursor / u64::from(self.superblock.rtgroup_extents))
                    .map_err(|_| XfsError::AddressOutOfRange)?
            } else {
                0
            };
            let group_first = if rtgroups {
                u64::from(group)
                    .checked_mul(u64::from(self.superblock.rtgroup_extents))
                    .ok_or(XfsError::AddressOutOfRange)?
            } else {
                0
            };
            let (_, group_extents, ..) = self.realtime_layout(group)?;
            let local_extent = cursor - group_first;
            let logical = local_extent / bits;
            let within = local_extent % bits;
            let take = (end - cursor)
                .min(bits - within)
                .min(group_extents - local_extent);
            let bitmap_inode = if rtgroups {
                u64::MAX
            } else {
                self.superblock.realtime_bitmap_inode
            };
            let physical = self.realtime_metadata_block(bitmap_inode, group, logical)?;
            let before = self.read_data_fs_block(physical)?;
            let bitmap_owner = if rtgroups {
                self.rtgroup_inodes
                    .get(group as usize)
                    .ok_or(XfsError::AddressOutOfRange)?
                    .0
            } else {
                bitmap_inode
            };
            if rtgroups {
                self.verify_rtgroup_buffer(&before, 0x424d_505a, bitmap_owner, physical)?;
            }
            let mut after = transaction
                .realtime_writes
                .iter()
                .find(|write| write.fs_block == physical)
                .map(|write| write.after.clone())
                .unwrap_or_else(|| before.clone());
            realtime_bitmap_range(&mut after, within, take, allocate, rtgroups)?;
            self.stage_realtime_image(physical, before, after, transaction)?;
            self.materialize_realtime_summary(group, transaction)?;
            cursor = cursor
                .checked_add(take)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(())
    }

    pub(super) fn write_data_fs_block(&self, block: u64, bytes: &[u8]) -> XfsResult<()> {
        if block >= self.superblock.data_blocks
            || bytes.len() != self.superblock.block_size as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = self.data.geometry().block_size;
        if physical == 0 || bytes.len() % physical != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start = block
            .checked_mul((bytes.len() / physical) as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.data
            .write_blocks_fua(start, bytes)
            .map_err(XfsError::from)
    }

    /// FUA-writes one realtime filesystem block.  Callers must flush the
    /// realtime member before making a data-device mapping durable.
    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn write_realtime_fs_block_fua(&self, block: u64, bytes: &[u8]) -> XfsResult<()> {
        let volume = self.realtime.as_ref().ok_or(XfsError::UnsupportedFeature)?;
        if block >= self.superblock.realtime_blocks
            || bytes.len() != self.superblock.block_size as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let basic = block
            .checked_mul(u64::from(self.superblock.block_size) / XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.write_basic_blocks_fua(volume, basic, bytes)
    }

    pub(super) fn stage_inode_image(
        &self,
        number: u64,
        before_inode: Vec<u8>,
        after_inode: Vec<u8>,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        if before_inode.len() != self.superblock.inode_size as usize
            || after_inode.len() != before_inode.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        let (ag, agino) = self.split_inode_number(number)?;
        let inode_block = agino >> self.superblock.inodes_per_block_log;
        let inode_index = agino & (self.superblock.inodes_per_block as u64 - 1);
        let fs_block = (ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(inode_block))
            .ok_or(XfsError::AddressOutOfRange)?;
        let before = self.read_data_fs_block(fs_block)?;
        let mut after = before.clone();
        let offset = (inode_index as usize)
            .checked_mul(self.superblock.inode_size as usize)
            .ok_or(XfsError::AddressOutOfRange)?;
        if before[offset..offset + before_inode.len()] != before_inode {
            return Err(XfsError::CorruptMetadata);
        }
        after[offset..offset + after_inode.len()].copy_from_slice(&after_inode);
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Inode,
            basic_block: fs_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before,
            after,
        });
        Ok(())
    }

    pub(super) fn read_data_bytes(&self, byte_offset: u64) -> XfsResult<Vec<u8>> {
        let physical = self.data.geometry().block_size;
        if byte_offset % physical as u64 != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let block = byte_offset / physical as u64;
        let mut bytes = vec![0; physical];
        self.data
            .read_blocks(block, &mut bytes)
            .map_err(XfsError::from)?;
        Ok(bytes)
    }

    pub(super) fn basic_blocks(&self, volume: &BlockVolume) -> XfsResult<u64> {
        let physical = volume.geometry().block_size;
        if physical == 0 || physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        volume
            .geometry()
            .blocks
            .checked_mul((physical / XFS_LOG_BASIC_BLOCK) as u64)
            .ok_or(XfsError::AddressOutOfRange)
    }

    /// Reads a byte image from the circular log address space.  The log's
    /// basic-block geometry is independent of its host volume's physical
    /// sector size, so both halves are routed through `read_basic_blocks`.
    pub(super) fn read_log_ring_bytes(&self, start: u32, bytes: usize) -> XfsResult<Vec<u8>> {
        if bytes == 0 || bytes % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let region = self.log_region_blocks()?;
        if start >= region || bytes / XFS_LOG_BASIC_BLOCK > region as usize {
            return Err(XfsError::AddressOutOfRange);
        }
        let log = self.log_volume()?;
        let base = self.log_region_start_block()?;
        let mut output = vec![0; bytes];
        let first_blocks = cmp::min(bytes / XFS_LOG_BASIC_BLOCK, (region - start) as usize);
        if first_blocks != 0 {
            self.read_basic_blocks(
                log,
                base.checked_add(start as u64)
                    .ok_or(XfsError::AddressOutOfRange)?,
                &mut output[..first_blocks * XFS_LOG_BASIC_BLOCK],
            )?;
        }
        if first_blocks * XFS_LOG_BASIC_BLOCK != bytes {
            self.read_basic_blocks(log, base, &mut output[first_blocks * XFS_LOG_BASIC_BLOCK..])?;
        }
        Ok(output)
    }

    /// Reads XFS 512-byte basic blocks through an arbitrary physical-sector
    /// BlockVolume.  Callers may use an unaligned log fragment; the complete
    /// enclosing sectors are always fetched before slicing.
    pub(super) fn read_basic_blocks(
        &self,
        volume: &BlockVolume,
        basic_block: u64,
        output: &mut [u8],
    ) -> XfsResult<()> {
        if output.is_empty() || output.len() % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = volume.geometry().block_size;
        if physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start_byte = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let end_byte = start_byte
            .checked_add(output.len() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let first = start_byte / physical as u64;
        let last = end_byte.div_ceil(physical as u64);
        if last > volume.geometry().blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut whole = vec![
            0;
            usize::try_from(
                (last - first)
                    .checked_mul(physical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            )
            .map_err(|_| XfsError::AddressOutOfRange)?
        ];
        volume
            .read_blocks(first, &mut whole)
            .map_err(XfsError::from)?;
        let offset = usize::try_from(start_byte % physical as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        output.copy_from_slice(&whole[offset..offset + output.len()]);
        Ok(())
    }

    /// Writes basic blocks with FUA.  Aligned writes avoid a read-modify
    /// cycle; boundary fragments retain neighboring basic blocks exactly.
    pub(super) fn write_basic_blocks_fua(
        &self,
        volume: &BlockVolume,
        basic_block: u64,
        input: &[u8],
    ) -> XfsResult<()> {
        if input.is_empty() || input.len() % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        let physical = volume.geometry().block_size;
        if physical % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let start_byte = basic_block
            .checked_mul(XFS_LOG_BASIC_BLOCK as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let end_byte = start_byte
            .checked_add(input.len() as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let first = start_byte / physical as u64;
        let last = end_byte.div_ceil(physical as u64);
        if last > volume.geometry().blocks {
            return Err(XfsError::AddressOutOfRange);
        }
        if start_byte % physical as u64 == 0 && input.len() % physical == 0 {
            return volume
                .write_blocks_fua(first, input)
                .map_err(XfsError::from);
        }
        let mut whole = vec![
            0;
            usize::try_from(
                (last - first)
                    .checked_mul(physical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?
            )
            .map_err(|_| XfsError::AddressOutOfRange)?
        ];
        volume
            .read_blocks(first, &mut whole)
            .map_err(XfsError::from)?;
        let offset = usize::try_from(start_byte % physical as u64)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        whole[offset..offset + input.len()].copy_from_slice(input);
        volume
            .write_blocks_fua(first, &whole)
            .map_err(XfsError::from)
    }

    pub(super) fn read_from_volume(
        &self,
        volume: &BlockVolume,
        fs_block: u64,
        bytes: usize,
    ) -> XfsResult<Vec<u8>> {
        let physical = volume.geometry().block_size;
        if bytes == 0 || bytes % physical != 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let blocks_per_fs_block = bytes / physical;
        let start = fs_block
            .checked_mul(blocks_per_fs_block as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut output = vec![0; bytes];
        volume
            .read_blocks(start, &mut output)
            .map_err(XfsError::from)?;
        Ok(output)
    }
}
