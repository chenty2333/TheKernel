//! XfsVolume: extended attributes, link counts and symlinks.

use super::*;

impl XfsVolume {
    /// Decodes an inline attribute fork.  The format has no implicit string
    /// conversion and enforces its stored total size before allocating names
    /// or values.  Leaf/node attribute trees are intentionally rejected until
    /// their remote-value and btree readers can preserve atomic xattr updates.
    pub fn shortform_xattrs(&self, number: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.attr_format != XfsForkFormat::Local {
            return Err(XfsError::UnsupportedFeature);
        }
        let payload = inode.attr_fork(&raw)?;
        if payload.len() < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let stored_size = be16(payload, 0)? as usize;
        let count = byte(payload, 2)? as usize;
        if stored_size > payload.len() || stored_size < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = &payload[..stored_size];
        let mut cursor = 4usize;
        let mut attrs = Vec::new();
        attrs
            .try_reserve_exact(count)
            .map_err(|_| XfsError::NoMemory)?;
        for _ in 0..count {
            let name_len = byte(payload, cursor)? as usize;
            let value_len = byte(
                payload,
                cursor.checked_add(1).ok_or(XfsError::CorruptMetadata)?,
            )? as usize;
            let flags = byte(
                payload,
                cursor.checked_add(2).ok_or(XfsError::CorruptMetadata)?,
            )?;
            cursor = cursor.checked_add(3).ok_or(XfsError::CorruptMetadata)?;
            let name = slice(payload, cursor, name_len)?.to_vec();
            cursor = cursor
                .checked_add(name_len)
                .ok_or(XfsError::CorruptMetadata)?;
            let value = slice(payload, cursor, value_len)?.to_vec();
            cursor = cursor
                .checked_add(value_len)
                .ok_or(XfsError::CorruptMetadata)?;
            if name.is_empty() || name.iter().any(|byte| *byte == 0) {
                return Err(XfsError::CorruptMetadata);
            }
            attrs.push(XfsShortformXattr { flags, name, value });
        }
        if cursor != payload.len() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(attrs)
    }

    /// Enumerates the native attribute fork representation currently present
    /// on disk.  Local forks use their compact encoding; non-local forks are
    /// decoded through an attribute leaf block instead of being silently
    /// presented as an empty xattr namespace.
    pub fn xattrs(&self, number: u64) -> XfsResult<Vec<XfsShortformXattr>> {
        let inode = self.inode(number)?;
        if inode.attr_format == XfsForkFormat::Local {
            return self.shortform_xattrs(number);
        }
        if !matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            return Err(XfsError::UnsupportedFeature);
        }
        let extents = self.inode_attr_extents(number)?;
        let mut seen = Vec::new();
        let entries = self.attribute_leaf_entries(number, &extents, 0, None, &mut seen)?;
        let mut attrs = Vec::new();
        attrs
            .try_reserve_exact(entries.len())
            .map_err(|_| XfsError::NoMemory)?;
        for entry in entries {
            let value = if entry.value_block == 0 {
                entry.value
            } else {
                self.read_attribute_remote_value(
                    number,
                    &extents,
                    entry.value_block as u64,
                    entry.value_length as usize,
                )?
            };
            attrs.push(XfsShortformXattr {
                flags: entry.flags | XFS_ATTR_LOCAL,
                name: entry.name,
                value,
            });
        }
        Ok(attrs)
    }

    /// Walks a verified DA attribute node using attr-fork *logical* block
    /// addresses.  Node edges are never interpreted as device blocks: each
    /// edge must resolve through the inode mapping and every child is visited
    /// at most once, preventing a damaged cyclic tree from consuming memory.
    pub(super) fn attribute_leaf_entries(
        &self,
        inode: u64,
        extents: &[XfsExtent],
        file_block: u64,
        expected_level: Option<u16>,
        seen: &mut Vec<u64>,
    ) -> XfsResult<Vec<XfsAttributeLeafEntry>> {
        if seen.len() >= 4096 || seen.contains(&file_block) {
            return Err(XfsError::CorruptMetadata);
        }
        seen.push(file_block);
        let extent = extents
            .iter()
            .find(|extent| {
                !extent.unwritten
                    && file_block >= extent.file_block
                    && file_block < extent.file_block + extent.block_count as u64
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let physical = extent
            .start_block
            .checked_add(file_block - extent.file_block)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.read_data_fs_block(physical)?;
        let basic = physical
            .checked_mul((self.superblock.block_size as u64) / 512)
            .ok_or(XfsError::AddressOutOfRange)?;
        match XfsAttributeBlock::parse(&block, self.superblock.meta_uuid, inode, basic)? {
            XfsAttributeBlock::Leaf { entries, .. } => {
                if expected_level.is_some_and(|level| level != 0) {
                    return Err(XfsError::CorruptMetadata);
                }
                Ok(entries)
            }
            XfsAttributeBlock::Node { level, entries, .. } => {
                if expected_level.is_some_and(|expected| expected != level) {
                    return Err(XfsError::CorruptMetadata);
                }
                let mut all = Vec::new();
                for edge in entries {
                    let child = self.attribute_leaf_entries(
                        inode,
                        extents,
                        edge.address as u64,
                        Some(level.checked_sub(1).ok_or(XfsError::CorruptMetadata)?),
                        seen,
                    )?;
                    all.try_reserve(child.len())
                        .map_err(|_| XfsError::NoMemory)?;
                    all.extend(child);
                }
                all.sort_unstable_by_key(|entry| (entry.hash, entry.name.clone()));
                if all.windows(2).any(|pair| {
                    pair[0].hash == pair[1].hash
                        && pair[0].name == pair[1].name
                        && (pair[0].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                            == (pair[1].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                }) {
                    return Err(XfsError::CorruptMetadata);
                }
                Ok(all)
            }
        }
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(super) fn attribute_leaf_has_remote_values(&self, number: u64) -> XfsResult<bool> {
        let inode = self.inode(number)?;
        if inode.attr_format != XfsForkFormat::Extents {
            return Ok(false);
        }
        let extents = self.inode_attr_extents(number)?;
        let extent = extents
            .iter()
            .find(|extent| !extent.unwritten && extent.file_block == 0)
            .ok_or(XfsError::CorruptMetadata)?;
        let block = self.read_data_fs_block(extent.start_block)?;
        let basic = extent
            .start_block
            .checked_mul((self.superblock.block_size as u64) / 512)
            .ok_or(XfsError::AddressOutOfRange)?;
        match XfsAttributeBlock::parse(&block, self.superblock.meta_uuid, number, basic)? {
            XfsAttributeBlock::Leaf { entries, .. } => {
                Ok(entries.iter().any(|entry| entry.value_block != 0))
            }
            XfsAttributeBlock::Node { .. } => Ok(true),
        }
    }

    pub(super) fn read_attribute_remote_value(
        &self,
        inode: u64,
        extents: &[XfsExtent],
        start_file_block: u64,
        length: usize,
    ) -> XfsResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        const RMT3_HEADER: usize = 56;
        const RMT2_HEADER: usize = 12;
        const RMT_MAGIC: u32 = 0x5841_524d;
        let header = if self.superblock.is_v5() {
            RMT3_HEADER
        } else {
            RMT2_HEADER
        };
        let fs = self.superblock.block_size as usize;
        let payload = fs.checked_sub(header).ok_or(XfsError::CorruptMetadata)?;
        let blocks = length.div_ceil(payload);
        let mut value = Vec::new();
        value
            .try_reserve_exact(length)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..blocks as u64 {
            let file_block = start_file_block
                .checked_add(index)
                .ok_or(XfsError::AddressOutOfRange)?;
            let extent = extents
                .iter()
                .find(|extent| {
                    !extent.unwritten
                        && file_block >= extent.file_block
                        && file_block < extent.file_block + extent.block_count as u64
                })
                .ok_or(XfsError::CorruptMetadata)?;
            let physical = extent.start_block + file_block - extent.file_block;
            let bytes = self.read_data_fs_block(physical)?;
            let basic = physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let offset = usize::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(payload))
                .ok_or(XfsError::AddressOutOfRange)?;
            let count = (length - offset).min(payload);
            if bytes.len() != fs
                || be32(&bytes, 0)? != RMT_MAGIC
                || be32(&bytes, 4)? as usize != offset
                || be32(&bytes, 8)? as usize != count
            {
                return Err(XfsError::CorruptMetadata);
            }
            if self.superblock.is_v5() {
                if be64(&bytes, 32)? != inode || be64(&bytes, 40)? != basic {
                    return Err(XfsError::CorruptMetadata);
                }
                let mut uuid = [0; 16];
                uuid.copy_from_slice(slice(&bytes, 16, 16)?);
                if XfsUuid(uuid) != self.superblock.meta_uuid {
                    return Err(XfsError::CorruptMetadata);
                }
                verify_crc32c(&bytes, 12)?;
            }
            value.extend_from_slice(slice(&bytes, header, count)?);
        }
        Ok(value)
    }

    /// Replaces an already allocated local attribute fork.  Attribute leaf,
    /// node, and remote-value transitions are intentionally not fabricated:
    /// callers receive an exact capacity error and retain the old fork.
    pub fn stage_shortform_xattrs(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.attr_format != XfsForkFormat::Local || inode.fork_offset == 0 {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.fork_offset as usize * 8;
        if fork_begin > raw.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let payload = serialize_shortform_xattrs(attrs)?;
        if payload.len() > raw.len() - fork_begin {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut after = raw.clone();
        after[fork_begin..].fill(0);
        after[fork_begin..fork_begin + payload.len()].copy_from_slice(&payload);
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(&mut after, 76, 0)?;
        } else {
            put_be16(&mut after, 80, 0)?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages an inode link-count update without publishing it.  Namespace
    /// code must use this rather than updating a VFS-side counter: the count
    /// is part of the inode core and must reach the log with the directory
    /// mutation that created or removed the name.
    pub fn stage_inode_link_count(
        &self,
        number: u64,
        links: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if inode.mode == 0 || links == 0 && inode.nlink == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut after = raw.clone();
        put_be32(&mut after, 16, links)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages the fixed inode-core attributes used by XFS fileattr ioctls.
    /// The CRC-covered dinode image, not a VFS cache, remains authoritative
    /// once the enclosing live-log transaction reaches its home checkpoint.
    pub(crate) fn stage_file_attr(
        &self,
        number: u64,
        attr: XfsFileAttr,
        ctime_seconds: i64,
        ctime_nanoseconds: u32,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 || ctime_nanoseconds >= 1_000_000_000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let mut after = raw.clone();
        put_be16(&mut after, 20, attr.project_id as u16)?;
        put_be16(&mut after, 22, (attr.project_id >> 16) as u16)?;
        put_be32(&mut after, 72, attr.extent_size_hint)?;
        put_be16(&mut after, 90, attr.flags)?;
        put_be64(&mut after, 120, attr.flags2)?;
        put_be32(&mut after, 128, attr.cow_extent_size_hint)?;
        encode_inode_timestamp(
            &mut after,
            48,
            attr.flags2 & XfsInode::DIFLAG2_BIGTIME != 0,
            ctime_seconds,
            ctime_nanoseconds,
        )?;
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Stages one complete v3 dinode-core replacement.  All timestamp range
    /// checks happen before the image is admitted to `transaction`, and the
    /// v3 CRC covers the final combined mode/owner/time image.
    pub(crate) fn stage_inode_core_update(
        &self,
        number: u64,
        update: XfsInodeCoreUpdate,
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 {
            return Err(XfsError::UnsupportedFeature);
        }

        for timestamp in [update.atime, update.mtime, update.ctime]
            .into_iter()
            .flatten()
        {
            if timestamp.1 >= 1_000_000_000 {
                return Err(XfsError::AddressOutOfRange);
            }
            // Run the native encoder on a disposable core image first.  This
            // validates legacy and BIGTIME bounds without publishing a
            // partial mutation when another requested field is unencodable.
            let mut probe = raw.clone();
            encode_inode_timestamp(
                &mut probe,
                32,
                inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0,
                timestamp.0,
                timestamp.1,
            )?;
        }

        let mut after = raw.clone();
        if let Some(mode) = update.mode {
            // MetadataUpdate carries permission/special bits, never a file
            // type.  Keep the on-disk type bits authoritative.
            put_be16(&mut after, 2, (inode.mode & !0o7777) | (mode & 0o7777))?;
        }
        if let Some((uid, gid)) = update.owner {
            put_be32(&mut after, 8, uid)?;
            put_be32(&mut after, 12, gid)?;
        }
        let bigtime = inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        if let Some((seconds, nanoseconds)) = update.atime {
            encode_inode_timestamp(&mut after, 32, bigtime, seconds, nanoseconds)?;
        }
        if let Some((seconds, nanoseconds)) = update.mtime {
            encode_inode_timestamp(&mut after, 40, bigtime, seconds, nanoseconds)?;
        }
        if let Some((seconds, nanoseconds)) = update.ctime {
            encode_inode_timestamp(&mut after, 48, bigtime, seconds, nanoseconds)?;
        }
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Builds a complete target replacement for the native symlink layouts
    /// this provider can prove: a local data fork or one written extent in
    /// the inode's AG.  BMBT/multi-AG symlinks are not coerced into a partial
    /// rewrite; callers receive `UnsupportedFeature` before any data write.
    pub(super) fn stage_symlink_replacement(
        &self,
        number: u64,
        target: &[u8],
        seconds: i64,
        nanoseconds: u32,
    ) -> XfsResult<XfsMetadataTransaction> {
        if target.iter().any(|byte| *byte == 0) || nanoseconds >= 1_000_000_000 {
            return Err(XfsError::AddressOutOfRange);
        }
        let (inode, raw) = self.inode_and_bytes(number)?;
        if !self.superblock.is_v5() || inode.version < 3 || inode.mode & 0o170000 != 0o120000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let fork_begin = inode.core_bytes as usize;
        let fork_end = if inode.fork_offset == 0 {
            raw.len()
        } else {
            (inode.fork_offset as usize)
                .checked_mul(8)
                .filter(|end| *end >= fork_begin && *end <= raw.len())
                .ok_or(XfsError::CorruptMetadata)?
        };
        let inline = fork_end
            .checked_sub(fork_begin)
            .ok_or(XfsError::CorruptMetadata)?;
        let (inode_ag, _) = self.split_inode_number(number)?;

        // Existing remote targets are restricted to the one-extent layout
        // emitted by `stage_new_inode`.  Keeping that proof also makes all
        // old data releases one AG allocator transaction.
        let old_remote = match inode.data_format {
            XfsForkFormat::Local => None,
            XfsForkFormat::Extents => {
                let extents = self.inode_data_extents(number)?;
                let extent = *extents.first().ok_or(XfsError::CorruptMetadata)?;
                if extents.len() != 1
                    || extent.unwritten
                    || extent.file_block != 0
                    || extent.start_block / u64::from(self.superblock.ag_blocks)
                        != u64::from(inode_ag)
                    || extent.start_block % u64::from(self.superblock.ag_blocks) < 4
                    || extent
                        .start_block
                        .checked_add(u64::from(extent.block_count))
                        .is_none_or(|end| {
                            end > (u64::from(inode_ag) + 1) * u64::from(self.superblock.ag_blocks)
                        })
                {
                    return Err(XfsError::UnsupportedFeature);
                }
                Some(extent)
            }
            _ => return Err(XfsError::UnsupportedFeature),
        };

        // Remote replacement never reuses the old target blocks: they remain
        // reachable until the new data has FUA-completed and the mapping
        // switch is durable.  This is the key failure-atomicity rule.
        let remote_blocks = (target.len() > inline)
            .then(|| {
                if inline < 16 {
                    return Err(XfsError::UnsupportedFeature);
                }
                u32::try_from(target.len().div_ceil(self.superblock.block_size as usize))
                    .map_err(|_| XfsError::AddressOutOfRange)
            })
            .transpose()?;
        let mut remote_start = None;
        let mut staged = if let Some(blocks) = remote_blocks {
            let snapshot = self.ag_ownership_snapshot(inode_ag)?;
            let allocation = snapshot
                .free_extents
                .iter()
                .filter(|extent| extent.block_count >= blocks)
                .min_by_key(|extent| (extent.block_count, extent.start_block))
                .copied()
                .ok_or(XfsError::AddressOutOfRange)?;
            remote_start = Some(allocation.start_block);
            let releases = old_remote
                .map(|extent| {
                    (0..extent.block_count)
                        .map(|offset| {
                            u32::try_from(
                                extent.start_block % u64::from(self.superblock.ag_blocks)
                                    + u64::from(offset),
                            )
                            .map_err(|_| XfsError::AddressOutOfRange)
                        })
                        .collect::<XfsResult<Vec<_>>>()
                })
                .transpose()?;
            let release = releases.as_deref().unwrap_or(&[]);
            self.stage_extent_delta(inode_ag, &[(allocation.start_block, blocks)], release)?
        } else if let Some(extent) = old_remote {
            let releases = (0..extent.block_count)
                .map(|offset| {
                    u32::try_from(
                        extent.start_block % u64::from(self.superblock.ag_blocks)
                            + u64::from(offset),
                    )
                    .map_err(|_| XfsError::AddressOutOfRange)
                })
                .collect::<XfsResult<Vec<_>>>()?;
            self.stage_extent_delta(inode_ag, &[], &releases)?
        } else {
            XfsMetadataTransaction::default()
        };

        let mut after = raw.clone();
        after[fork_begin..fork_end].fill(0);
        put_be64(
            &mut after,
            56,
            u64::try_from(target.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        let sector_unit = u64::from(self.superblock.block_size) / 512;
        let old_data_sectors = old_remote
            .map(|extent| {
                u64::from(extent.block_count)
                    .checked_mul(sector_unit)
                    .ok_or(XfsError::AddressOutOfRange)
            })
            .transpose()?
            .unwrap_or(0);
        let new_data_sectors = remote_blocks
            .map(|blocks| {
                u64::from(blocks)
                    .checked_mul(sector_unit)
                    .ok_or(XfsError::AddressOutOfRange)
            })
            .transpose()?
            .unwrap_or(0);
        // `di_nblocks` includes both data and attribute forks.  Replace only
        // the old symlink data contribution, retaining any xattr/attr-BMBT
        // ownership already represented in the raw inode core.
        let blocks = inode
            .blocks
            .checked_sub(old_data_sectors)
            .and_then(|base| base.checked_add(new_data_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        put_be64(&mut after, 64, blocks)?;
        let bigtime = inode.flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        encode_inode_timestamp(&mut after, 40, bigtime, seconds, nanoseconds)?;
        encode_inode_timestamp(&mut after, 48, bigtime, seconds, nanoseconds)?;
        put_be64(
            &mut after,
            104,
            be64(&raw, 104)?
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?,
        )?;
        if let Some(blocks) = remote_blocks {
            let allocation = remote_start.ok_or(XfsError::CorruptMetadata)?;
            let first = u64::from(inode_ag)
                .checked_mul(u64::from(self.superblock.ag_blocks))
                .and_then(|base| base.checked_add(u64::from(allocation)))
                .ok_or(XfsError::AddressOutOfRange)?;
            after[5] = XfsForkFormat::Extents as u8;
            after[fork_begin..fork_begin + 16].copy_from_slice(&encode_xfs_extent(XfsExtent {
                unwritten: false,
                file_block: 0,
                start_block: first,
                block_count: blocks,
            })?);
            if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
                put_be64(&mut after, 24, 1)?;
            } else {
                put_be32(&mut after, 76, 1)?;
            }
            staged
                .data_writes
                .try_reserve_exact(blocks as usize)
                .map_err(|_| XfsError::NoMemory)?;
            let block_size = self.superblock.block_size as usize;
            for index in 0..u64::from(blocks) {
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
                let mut image = Vec::new();
                image
                    .try_reserve_exact(block_size)
                    .map_err(|_| XfsError::NoMemory)?;
                image.resize(block_size, 0);
                image[..end - begin].copy_from_slice(&target[begin..end]);
                staged.data_writes.push(XfsStagedDataWrite {
                    fs_block,
                    before: self.read_data_fs_block(fs_block)?,
                    after: image,
                });
            }
        } else {
            after[5] = XfsForkFormat::Local as u8;
            after[fork_begin..fork_begin + target.len()].copy_from_slice(target);
            if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
                put_be64(&mut after, 24, 0)?;
            } else {
                put_be32(&mut after, 76, 0)?;
            }
        }
        rewrite_crc32c(&mut after, 100)?;
        self.stage_inode_image(number, raw, after, &mut staged)?;
        Ok(staged)
    }

    /// Rewrites an existing native attribute leaf while retaining its mapped
    /// attribute-fork block.  A leaf overflow is deliberately reported before
    /// any buffer is staged so a future node split can reserve both children
    /// and the parent in one transaction rather than publishing a half-tree.
    pub fn stage_attribute_leaf(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode = self.inode(number)?;
        if inode.attr_format != XfsForkFormat::Extents {
            return Err(XfsError::UnsupportedFeature);
        }
        let extents = self.inode_attr_extents(number)?;
        let extent = extents
            .iter()
            .find(|extent| !extent.unwritten && extent.file_block == 0)
            .ok_or(XfsError::CorruptMetadata)?;
        let mut records = Vec::new();
        records
            .try_reserve_exact(attrs.len())
            .map_err(|_| XfsError::NoMemory)?;
        for attr in attrs {
            if attr.value.len() > u16::MAX as usize {
                return Err(XfsError::AddressOutOfRange);
            }
            records.push(XfsAttributeLeafEntry {
                hash: xfs_name_hash(&attr.name),
                flags: attr.flags | XFS_ATTR_LOCAL,
                name: attr.name.clone(),
                value: attr.value.clone(),
                value_block: 0,
                value_length: attr.value.len() as u32,
            });
        }
        let after = XfsAttributeBlock::serialize_leaf(
            &records,
            0,
            0,
            self.superblock.meta_uuid,
            number,
            extent
                .start_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            self.superblock.is_v5(),
            self.superblock.block_size as usize,
        )?;
        let before = self.read_data_fs_block(extent.start_block)?;
        transaction.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: XfsMetadataBufferType::Attribute,
            basic_block: extent
                .start_block
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            before,
            after,
        });
        Ok(())
    }

    /// Rebuilds the complete native attribute DA image as one transaction.
    /// Large values are stored in ordinary attr-fork remote blocks; leaves
    /// only contain their logical start and exact byte length. Allocation,
    /// stale-block release, leaf/node replacement and inode-fork rewrite are
    /// staged together so no committed edge can name an unowned block.
    pub fn stage_attribute_values(
        &self,
        number: u64,
        attrs: &[XfsShortformXattr],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let inode = self.inode(number)?;
        if !matches!(
            inode.attr_format,
            XfsForkFormat::Local | XfsForkFormat::Extents | XfsForkFormat::Btree
        ) || inode.fork_offset == 0
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let fs = self.superblock.block_size as usize;
        let old_extents = if matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            self.inode_attr_extents(number)?
        } else {
            Vec::new()
        };
        let old_bmap_blocks = self.attr_bmbt_blocks(number)?;

        // Rebuild the DA image rather than attempting an in-place leaf edit.
        // This makes insert/replace/remove one transaction: a one-leaf tree
        // collapses to logical block zero, while a split promotes a node at
        // zero and assigns its leaves the stable logical range 1..N.  Remote
        // values are always after that range, so a committed node never
        // points at a block which has not also been mapped by the same fork
        // update.
        let mut records = Vec::new();
        records
            .try_reserve_exact(attrs.len())
            .map_err(|_| XfsError::NoMemory)?;
        for attr in attrs {
            if attr.value.len() > u32::MAX as usize {
                return Err(XfsError::AddressOutOfRange);
            }
            records.push(XfsAttributeLeafEntry {
                hash: xfs_name_hash(&attr.name),
                flags: attr.flags | XFS_ATTR_LOCAL,
                name: attr.name.clone(),
                value: attr.value.clone(),
                value_block: 0,
                value_length: attr.value.len() as u32,
            });
        }
        records.sort_unstable_by_key(|entry| {
            (
                entry.hash,
                entry.name.clone(),
                entry.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE),
            )
        });
        if records.windows(2).any(|pair| {
            pair[0].hash == pair[1].hash
                && pair[0].name == pair[1].name
                && (pair[0].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                    == (pair[1].flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
        }) {
            return Err(XfsError::AddressOutOfRange);
        }
        let values: Vec<Vec<u8>> = records.iter().map(|entry| entry.value.clone()).collect();
        // A value is remote only when it cannot inhabit a leaf by itself.
        // Total leaf pressure is handled by partitioning, not by needlessly
        // converting ordinary small xattrs to remote form.
        let mut remote = vec![false; records.len()];
        for (index, record) in records.iter_mut().enumerate() {
            if XfsAttributeBlock::serialize_leaf(
                core::slice::from_ref(record),
                0,
                0,
                self.superblock.meta_uuid,
                number,
                0,
                self.superblock.is_v5(),
                fs,
            )
            .is_err()
            {
                remote[index] = true;
                record.flags &= !XFS_ATTR_LOCAL;
                record.value.clear();
                record.value_block = 1;
                if XfsAttributeBlock::serialize_leaf(
                    core::slice::from_ref(record),
                    0,
                    0,
                    self.superblock.meta_uuid,
                    number,
                    0,
                    self.superblock.is_v5(),
                    fs,
                )
                .is_err()
                {
                    return Err(XfsError::AddressOutOfRange);
                }
            }
        }
        let mut leaves = XfsAttributeBlock::partition_leaves(
            &records,
            self.superblock.meta_uuid,
            number,
            self.superblock.is_v5(),
            fs,
        )?;
        let leaf_count = leaves.len();
        let node_capacity = (fs
            .checked_sub(if self.superblock.is_v5() { 64 } else { 16 })
            .ok_or(XfsError::AddressOutOfRange)?)
            / 8;
        if node_capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        // Logical block zero is permanently the DA root.  Leaves retain the
        // compact 1..N range even when extra interior levels are needed;
        // those levels are allocated after the leaves and are linked upward
        // until the root fanout fits.
        let mut intermediate = Vec::<(u64, u16, Vec<XfsDirectoryLeafEntry>, u32, u32)>::new();
        let (root_level, root_entries, metadata_blocks) = if leaf_count == 1 {
            (0u16, Vec::new(), 1u64)
        } else {
            let mut current = Vec::new();
            current
                .try_reserve_exact(leaf_count)
                .map_err(|_| XfsError::NoMemory)?;
            for (index, leaf) in leaves.iter().enumerate() {
                current.push(XfsDirectoryLeafEntry {
                    hash: leaf.last().ok_or(XfsError::CorruptMetadata)?.hash,
                    address: u32::try_from(index + 1).map_err(|_| XfsError::AddressOutOfRange)?,
                });
            }
            let mut next_logical = 1u64
                .checked_add(u64::try_from(leaf_count).map_err(|_| XfsError::AddressOutOfRange)?)
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut level = 1u16;
            while current.len() > node_capacity {
                if level >= 5 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let groups = current.len().div_ceil(node_capacity);
                let first = next_logical;
                let mut parents = Vec::new();
                parents
                    .try_reserve_exact(groups)
                    .map_err(|_| XfsError::NoMemory)?;
                for group in 0..groups {
                    let start = group * node_capacity;
                    let end = (start + node_capacity).min(current.len());
                    let logical = first
                        .checked_add(group as u64)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    let entries = current[start..end].to_vec();
                    let hash = entries.last().ok_or(XfsError::CorruptMetadata)?.hash;
                    intermediate.push((
                        logical,
                        level,
                        entries,
                        if group + 1 == groups {
                            0
                        } else {
                            u32::try_from(
                                logical.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
                            )
                            .map_err(|_| XfsError::AddressOutOfRange)?
                        },
                        if group == 0 {
                            0
                        } else {
                            u32::try_from(logical - 1).map_err(|_| XfsError::AddressOutOfRange)?
                        },
                    ));
                    parents.push(XfsDirectoryLeafEntry {
                        hash,
                        address: u32::try_from(logical).map_err(|_| XfsError::AddressOutOfRange)?,
                    });
                }
                current = parents;
                next_logical = next_logical
                    .checked_add(u64::try_from(groups).map_err(|_| XfsError::AddressOutOfRange)?)
                    .ok_or(XfsError::AddressOutOfRange)?;
                level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            }
            (level, current, next_logical)
        };
        // Decide the on-inode direct/BMBT representation before reservations.
        // The BMBT allocation is intentionally included in the same extent
        // delta as leaf/remote blocks so the new root never names free space.
        let (inode_again, raw_again) = self.inode_and_bytes(number)?;
        let attr_bytes = inode_again.attr_fork(&raw_again)?.len();
        // Each remote value is one contiguous logical extent irrespective of
        // its physical block count.  The attr BMBT indexes mappings, not leaf
        // records; using `records.len()` here under-reserves the tree when a
        // sparse set of remote values expands the fork.
        let mapped_extents = 1usize
            .checked_add(remote.iter().filter(|remote| **remote).count())
            .ok_or(XfsError::AddressOutOfRange)?;
        let bmap_needed = bmap_external_blocks(self.superblock, attr_bytes, mapped_extents)?;
        let bmap_reused = old_bmap_blocks.len().min(bmap_needed);
        let bmap_fresh = bmap_needed
            .checked_sub(bmap_reused)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut requests = Vec::<u32>::new();
        requests.push(u32::try_from(metadata_blocks).map_err(|_| XfsError::AddressOutOfRange)?);
        let remote_payload = fs
            .checked_sub(if self.superblock.is_v5() { 56 } else { 12 })
            .ok_or(XfsError::AddressOutOfRange)?;
        for (index, _record) in records.iter().enumerate() {
            if remote[index] {
                requests.push(
                    u32::try_from(values[index].len().div_ceil(remote_payload))
                        .map_err(|_| XfsError::AddressOutOfRange)?
                        .max(1),
                );
            }
        }
        if bmap_fresh != 0 {
            requests.push(u32::try_from(bmap_fresh).map_err(|_| XfsError::AddressOutOfRange)?);
        }
        let (inode_ag, _) = self.split_inode_number(number)?;
        let batch = if requests.is_empty() {
            None
        } else {
            Some(self.prepare_extent_allocations(inode_ag, &requests)?)
        };
        let mut next_allocation = 0usize;
        let metadata_allocation = batch
            .as_ref()
            .and_then(|batch| batch.allocations.get(next_allocation))
            .ok_or(XfsError::CorruptMetadata)?;
        next_allocation += 1;
        if metadata_allocation.block_count as u64 != metadata_blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let metadata_physical = (metadata_allocation.ag as u64)
            .checked_mul(self.superblock.ag_blocks as u64)
            .and_then(|base| base.checked_add(metadata_allocation.start_block as u64))
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut extents = vec![XfsExtent {
            unwritten: false,
            file_block: 0,
            start_block: metadata_physical,
            block_count: metadata_allocation.block_count,
        }];
        let mut remote_writes = Vec::<(u64, Vec<u8>)>::new();
        let mut file_block = metadata_blocks;
        for (index, record) in records.iter_mut().enumerate() {
            if !remote[index] {
                continue;
            }
            let allocation = batch
                .as_ref()
                .and_then(|batch| batch.allocations.get(next_allocation))
                .ok_or(XfsError::CorruptMetadata)?;
            next_allocation += 1;
            let physical = (allocation.ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(allocation.start_block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            record.value_block =
                u32::try_from(file_block).map_err(|_| XfsError::AddressOutOfRange)?;
            record.value_length =
                u32::try_from(values[index].len()).map_err(|_| XfsError::AddressOutOfRange)?;
            extents.push(XfsExtent {
                unwritten: false,
                file_block,
                start_block: physical,
                block_count: allocation.block_count,
            });
            remote_writes.push((physical, values[index].clone()));
            file_block = file_block
                .checked_add(allocation.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        // Re-partition after remote addresses are final and install them in
        // the leaf copies. `partition_leaves` is deterministic by hash/name,
        // so indexing the records this way cannot alter the DA ordering.
        for leaf in &mut leaves {
            for entry in leaf {
                if entry.value_block == 0 {
                    continue;
                }
                let source = records
                    .iter()
                    .find(|record| {
                        record.hash == entry.hash
                            && record.name == entry.name
                            && (record.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                                == (entry.flags & (XFS_ATTR_ROOT | XFS_ATTR_SECURE))
                    })
                    .ok_or(XfsError::CorruptMetadata)?;
                entry.value_block = source.value_block;
                entry.value_length = source.value_length;
            }
        }
        let mut bmap_blocks = old_bmap_blocks[..bmap_reused].to_vec();
        if bmap_fresh != 0 {
            let allocation = batch
                .as_ref()
                .and_then(|batch| batch.allocations.get(next_allocation))
                .ok_or(XfsError::CorruptMetadata)?;
            next_allocation += 1;
            if allocation.block_count as usize != bmap_fresh {
                return Err(XfsError::CorruptMetadata);
            }
            let base = (allocation.ag as u64)
                .checked_mul(self.superblock.ag_blocks as u64)
                .and_then(|base| base.checked_add(allocation.start_block as u64))
                .ok_or(XfsError::AddressOutOfRange)?;
            bmap_blocks
                .try_reserve_exact(bmap_fresh)
                .map_err(|_| XfsError::NoMemory)?;
            for index in 0..bmap_fresh {
                bmap_blocks.push(
                    base.checked_add(index as u64)
                        .ok_or(XfsError::AddressOutOfRange)?,
                );
            }
        }
        if next_allocation != batch.as_ref().map_or(0, |batch| batch.allocations.len()) {
            return Err(XfsError::CorruptMetadata);
        }
        // All old remote mappings are replaced.  Combine releases in the
        // allocation AG with the batch image; other AGs have no allocation
        // and can be independently rebuilt in this same journal record.
        let mut releases: Vec<(u32, Vec<u32>)> = Vec::new();
        for extent in &old_extents {
            for physical in extent.start_block
                ..extent
                    .start_block
                    .checked_add(extent.block_count as u64)
                    .ok_or(XfsError::CorruptMetadata)?
            {
                let ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if let Some((_, blocks)) =
                    releases.iter_mut().find(|(candidate, _)| *candidate == ag)
                {
                    blocks.push(block);
                } else {
                    releases.push((ag, vec![block]));
                }
            }
        }
        let mut metadata_releases: Vec<(u32, Vec<u32>)> = Vec::new();
        for physical in old_bmap_blocks.iter().skip(bmap_reused).copied() {
            let ag = u32::try_from(physical / self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let block = u32::try_from(physical % self.superblock.ag_blocks as u64)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            if let Some((_, blocks)) = releases.iter_mut().find(|(candidate, _)| *candidate == ag) {
                blocks.push(block);
            } else {
                releases.push((ag, vec![block]));
            }
            if let Some((_, blocks)) = metadata_releases
                .iter_mut()
                .find(|(candidate, _)| *candidate == ag)
            {
                blocks.push(block);
            } else {
                metadata_releases.push((ag, vec![block]));
            }
        }
        let mut staged = transaction.clone();
        if let Some(batch) = &batch {
            let allocs: Vec<(u32, u32)> = batch
                .allocations
                .iter()
                .map(|allocation| (allocation.start_block, allocation.block_count))
                .collect();
            let released = releases
                .iter()
                .find(|(ag, _)| *ag == inode_ag)
                .map(|(_, blocks)| blocks.as_slice())
                .unwrap_or(&[]);
            let metadata = metadata_releases
                .iter()
                .find(|(ag, _)| *ag == inode_ag)
                .map(|(_, blocks)| blocks.as_slice())
                .unwrap_or(&[]);
            staged.buffers.extend(
                self.stage_extent_delta_with_metadata(inode_ag, &allocs, released, metadata)?
                    .buffers,
            );
        }
        for (ag, blocks) in &releases {
            if *ag != inode_ag {
                let metadata = metadata_releases
                    .iter()
                    .find(|(candidate, _)| *candidate == *ag)
                    .map(|(_, blocks)| blocks.as_slice())
                    .unwrap_or(&[]);
                staged.buffers.extend(
                    self.stage_extent_delta_with_metadata(*ag, &[], blocks, metadata)?
                        .buffers,
                );
            }
        }
        if bmap_needed == 0 {
            self.stage_attribute_fork_extents(number, &extents, &mut staged)?;
        } else {
            self.stage_attribute_fork_bmap(number, extents, &bmap_blocks, &mut staged)?;
        }
        if leaf_count == 1 {
            let basic = metadata_physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let leaf = XfsAttributeBlock::serialize_leaf(
                &leaves[0],
                0,
                0,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                fs,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Attribute,
                basic_block: basic,
                before: self.read_data_fs_block(metadata_physical)?,
                after: leaf,
            });
        } else {
            for index in 0..leaf_count {
                let logical = u32::try_from(index + 1).map_err(|_| XfsError::AddressOutOfRange)?;
                let physical = metadata_physical
                    .checked_add(logical as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let forward = if index + 1 == leaf_count {
                    0
                } else {
                    logical.checked_add(1).ok_or(XfsError::AddressOutOfRange)?
                };
                let backward = if index == 0 { 0 } else { logical - 1 };
                let basic = physical
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let leaf = XfsAttributeBlock::serialize_leaf(
                    &leaves[index],
                    forward,
                    backward,
                    self.superblock.meta_uuid,
                    number,
                    basic,
                    self.superblock.is_v5(),
                    fs,
                )?;
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: leaf,
                });
            }
            for (logical, level, entries, forward, backward) in intermediate {
                let physical = metadata_physical
                    .checked_add(logical)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = physical
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let node = XfsAttributeBlock::serialize_node(
                    &entries,
                    forward,
                    backward,
                    level,
                    self.superblock.meta_uuid,
                    number,
                    basic,
                    self.superblock.is_v5(),
                    fs,
                )?;
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(physical)?,
                    after: node,
                });
            }
            let basic = metadata_physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?;
            let root = XfsAttributeBlock::serialize_node(
                &root_entries,
                0,
                0,
                root_level,
                self.superblock.meta_uuid,
                number,
                basic,
                self.superblock.is_v5(),
                fs,
            )?;
            staged.buffers.push(XfsDirtyMetadataBuffer {
                metadata_type: XfsMetadataBufferType::Attribute,
                basic_block: basic,
                before: self.read_data_fs_block(metadata_physical)?,
                after: root,
            });
        }
        for (physical, value) in remote_writes {
            let remote_header = if self.superblock.is_v5() {
                56usize
            } else {
                12usize
            };
            let payload = fs
                .checked_sub(remote_header)
                .ok_or(XfsError::AddressOutOfRange)?;
            let blocks = value.len().div_ceil(payload);
            for index in 0..blocks {
                let mut after = vec![0; fs];
                let begin = index * payload;
                let end = (begin + payload).min(value.len());
                let block = physical
                    .checked_add(index as u64)
                    .ok_or(XfsError::AddressOutOfRange)?;
                let basic = block
                    .checked_mul((self.superblock.block_size as u64) / 512)
                    .ok_or(XfsError::AddressOutOfRange)?;
                put_be32(&mut after, 0, 0x5841_524d)?;
                put_be32(
                    &mut after,
                    4,
                    u32::try_from(begin).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
                put_be32(
                    &mut after,
                    8,
                    u32::try_from(end - begin).map_err(|_| XfsError::AddressOutOfRange)?,
                )?;
                if self.superblock.is_v5() {
                    after[16..32].copy_from_slice(&self.superblock.meta_uuid.0);
                    put_be64(&mut after, 32, number)?;
                    put_be64(&mut after, 40, basic)?;
                    put_be64(&mut after, 48, 0)?;
                }
                after[remote_header..remote_header + end - begin]
                    .copy_from_slice(&value[begin..end]);
                if self.superblock.is_v5() {
                    rewrite_crc32c(&mut after, 12)?;
                }
                staged.buffers.push(XfsDirtyMetadataBuffer {
                    metadata_type: XfsMetadataBufferType::Attribute,
                    basic_block: basic,
                    before: self.read_data_fs_block(block)?,
                    after,
                });
            }
        }
        *transaction = staged;
        Ok(())
    }

    pub(super) fn stage_attribute_fork_extents(
        &self,
        number: u64,
        extents: &[XfsExtent],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        let begin = inode.fork_offset as usize * 8;
        if begin == 0
            || begin > raw.len()
            || extents
                .len()
                .checked_mul(16)
                .ok_or(XfsError::AddressOutOfRange)?
                > raw.len() - begin
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut previous = 0u64;
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
                || (index != 0 && extent.file_block < previous)
            {
                return Err(XfsError::CorruptMetadata);
            }
            previous = extent.file_block + extent.block_count as u64;
        }
        let old_blocks = if matches!(
            inode.attr_format,
            XfsForkFormat::Extents | XfsForkFormat::Btree
        ) {
            let mappings =
                self.inode_attr_extents(number)?
                    .iter()
                    .try_fold(0u64, |sum, extent| {
                        sum.checked_add(extent.block_count as u64)
                            .ok_or(XfsError::AddressOutOfRange)
                    })?;
            mappings
                .checked_add(self.attr_bmbt_blocks(number)?.len() as u64)
                .ok_or(XfsError::AddressOutOfRange)?
        } else {
            0
        };
        let new_blocks = extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?;
        let sector_unit = (self.superblock.block_size / 512) as u64;
        let old_sectors = old_blocks
            .checked_mul(sector_unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_sectors = new_blocks
            .checked_mul(sector_unit)
            .ok_or(XfsError::AddressOutOfRange)?;
        let sectors = inode
            .blocks
            .checked_sub(old_sectors)
            .and_then(|base| base.checked_add(new_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        let mut after = raw.clone();
        after[83] = XfsForkFormat::Extents as u8;
        after[begin..].fill(0);
        put_be64(&mut after, 64, sectors)?;
        for (index, extent) in extents.iter().enumerate() {
            after[begin + index * 16..begin + (index + 1) * 16]
                .copy_from_slice(&encode_xfs_extent(*extent)?);
        }
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        } else {
            put_be16(
                &mut after,
                80,
                u16::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }

    /// Serializes an attribute-fork BMBT and its inode root.  The caller owns
    /// allocation/reclaim staging; this routine only consumes exactly that
    /// owned node set and updates `di_nblocks` with the mapping-tree blocks.
    pub(super) fn stage_attribute_fork_bmap(
        &self,
        number: u64,
        mut extents: Vec<XfsExtent>,
        blocks: &[u64],
        transaction: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let (inode, raw) = self.inode_and_bytes(number)?;
        let begin = inode.fork_offset as usize * 8;
        if begin == 0 || begin > raw.len() {
            return Err(XfsError::CorruptMetadata);
        }
        extents.sort_unstable_by_key(|extent| extent.file_block);
        let fork_bytes = raw.len() - begin;
        let needed = bmap_external_blocks(self.superblock, fork_bytes, extents.len())?;
        if needed == 0
            || blocks.len() != needed
            || blocks.iter().enumerate().any(|(index, block)| {
                *block >= self.superblock.data_blocks
                    || *block % (self.superblock.ag_blocks as u64) < 4
                    || blocks[..index].contains(block)
            })
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
        let root_capacity = fork_bytes
            .checked_sub(4)
            .ok_or(XfsError::AddressOutOfRange)?
            / 16;
        if leaf_capacity == 0 || interior_capacity == 0 || root_capacity == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
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
            let after = serialize_bmap_node(
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
                after,
            });
            current.push((block, extents[start]));
        }
        let mut level = 1u16;
        while current.len() > root_capacity {
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
                let after = serialize_bmap_node(
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
                    after,
                });
                next.push((block, keys[0]));
            }
            current = next;
            level = level.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        }
        if used != blocks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let old_mapping_blocks =
            self.inode_attr_extents(number)?
                .iter()
                .try_fold(0u64, |sum, extent| {
                    sum.checked_add(extent.block_count as u64)
                        .ok_or(XfsError::AddressOutOfRange)
                })?;
        let old_tree_blocks = self.attr_bmbt_blocks(number)?.len() as u64;
        let new_mapping_blocks = extents.iter().try_fold(0u64, |sum, extent| {
            sum.checked_add(extent.block_count as u64)
                .ok_or(XfsError::AddressOutOfRange)
        })?;
        let new_tree_blocks = blocks.len() as u64;
        let sector_unit = (self.superblock.block_size / 512) as u64;
        let old_sectors = old_mapping_blocks
            .checked_add(old_tree_blocks)
            .and_then(|count| count.checked_mul(sector_unit))
            .ok_or(XfsError::AddressOutOfRange)?;
        let new_sectors = new_mapping_blocks
            .checked_add(new_tree_blocks)
            .and_then(|count| count.checked_mul(sector_unit))
            .ok_or(XfsError::AddressOutOfRange)?;
        let sectors = inode
            .blocks
            .checked_sub(old_sectors)
            .and_then(|base| base.checked_add(new_sectors))
            .ok_or(XfsError::CorruptMetadata)?;
        let mut after = raw.clone();
        after[83] = XfsForkFormat::Btree as u8;
        after[begin..].fill(0);
        put_be16(&mut after, begin, level)?;
        put_be16(
            &mut after,
            begin + 2,
            u16::try_from(current.len()).map_err(|_| XfsError::AddressOutOfRange)?,
        )?;
        for (index, (child, key)) in current.iter().enumerate() {
            put_be64(&mut after, begin + 4 + index * 8, key.file_block)?;
            put_be64(
                &mut after,
                begin + 4 + root_capacity * 8 + index * 8,
                *child,
            )?;
        }
        put_be64(&mut after, 64, sectors)?;
        if inode.flags2 & XfsInode::DIFLAG2_NREXT64 != 0 {
            put_be32(
                &mut after,
                76,
                u32::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        } else {
            put_be16(
                &mut after,
                80,
                u16::try_from(extents.len()).map_err(|_| XfsError::AddressOutOfRange)?,
            )?;
        }
        self.stage_inode_image(number, raw, after, transaction)
    }
}
