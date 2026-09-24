//! XfsVolume: node and directory lookup and log-region geometry.

use super::*;

impl XfsVolume {
    /// Opens a retained inode object after validating its on-disk core.
    pub fn node(self: &Arc<Self>, number: u64) -> XfsResult<XfsNode> {
        Ok(XfsNode {
            volume: self.clone(),
            inode: self.inode(number)?,
        })
    }

    pub fn root_node(self: &Arc<Self>) -> XfsResult<XfsNode> {
        self.node(self.superblock.root_inode)
    }

    pub fn node_from_export_handle(
        self: &Arc<Self>,
        handle: XfsExportHandle,
    ) -> XfsResult<XfsNode> {
        let inode = self.resolve_export_handle(handle)?;
        Ok(XfsNode {
            volume: self.clone(),
            inode,
        })
    }

    pub fn directory_block_size(&self) -> XfsResult<usize> {
        let bytes = self
            .superblock
            .block_size
            .checked_shl(self.superblock.directory_block_log as u32)
            .ok_or(XfsError::InvalidSuperblock)?;
        usize::try_from(bytes).map_err(|_| XfsError::UnsupportedFeature)
    }

    /// Reads one logical dir2/dir3 data block through the inode mapping.  A
    /// short read is always corruption: directory format headers and leaf
    /// offsets are meaningful only over the complete logical block.
    pub fn directory_data_block(
        &self,
        inode_number: u64,
        index: u64,
    ) -> XfsResult<XfsDirectoryDataBlock> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let bytes = self.directory_block_size()?;
        let offset = index
            .checked_mul(bytes as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut block = vec![0; bytes];
        let read = self.read_inode_at(inode_number, offset, &mut block)?;
        if read != block.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = self
            .inode_physical_file_block(inode_number, offset / self.superblock.block_size as u64)?;
        XfsDirectoryDataBlock::parse(
            &block,
            self.superblock.meta_uuid,
            inode_number,
            physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
            self.superblock.features.incompat & XfsFeatures::INCOMPAT_FTYPE != 0,
        )
    }

    /// Reads a dir2/dir3 leaf block from its distinct 32GiB logical address
    /// space.  Data and leaf offsets are never conflated even when their
    /// backing extents happen to be adjacent on disk.
    pub fn directory_leaf_block(
        &self,
        inode_number: u64,
        index: u64,
    ) -> XfsResult<XfsDirectoryLeafBlock> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        let bytes = self.directory_block_size()?;
        let offset = XFS_DIR_LEAF_SPACE_BYTES
            .checked_add(
                index
                    .checked_mul(bytes as u64)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut block = vec![0; bytes];
        let read = self.read_inode_at(inode_number, offset, &mut block)?;
        if read != block.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let physical = self
            .inode_physical_file_block(inode_number, offset / self.superblock.block_size as u64)?;
        XfsDirectoryLeafBlock::parse(
            &block,
            self.superblock.meta_uuid,
            inode_number,
            physical
                .checked_mul((self.superblock.block_size as u64) / 512)
                .ok_or(XfsError::AddressOutOfRange)?,
        )
    }

    /// Returns every live name in a directory without converting names to
    /// UTF-8.  Local directories are decoded directly; block, leaf, and node
    /// directories are traversed through their data-space mappings.  Leaf
    /// blocks are indexes only and must never be treated as directory data.
    ///
    /// The data fork can contain holes between populated dir2 data blocks.
    /// Those holes are skipped from the checked extent map rather than being
    /// fed to the directory decoder as an all-zero "block".
    pub fn directory_entries(&self, inode_number: u64) -> XfsResult<Vec<XfsDirectoryEntry>> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        if inode.data_format == XfsForkFormat::Local {
            return self.shortform_directory(inode_number);
        }
        let dir_block = self.directory_block_size()? as u64;
        let data_limit = cmp::min(inode.size, XFS_DIR_LEAF_SPACE_BYTES);
        let data_blocks = data_limit.div_ceil(dir_block);
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let fs_block = self.superblock.block_size as u64;
        let mut entries = Vec::new();
        for index in 0..data_blocks {
            let logical_start = index
                .checked_mul(dir_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let first_file_block = logical_start / fs_block;
            let last_file_block = logical_start
                .checked_add(dir_block)
                .and_then(|end| end.checked_sub(1))
                .ok_or(XfsError::AddressOutOfRange)?
                / fs_block;
            // A dir data block is meaningful only if a single written extent
            // covers it in full.  A partial mapping is corrupt metadata, not
            // a sparse directory hole.
            let Some(mapping) = extents.iter().find(|extent| {
                !extent.unwritten
                    && first_file_block >= extent.file_block
                    && last_file_block < extent.file_block.saturating_add(extent.block_count as u64)
            }) else {
                continue;
            };
            if mapping.file_block > first_file_block {
                return Err(XfsError::CorruptMetadata);
            }
            let block = self.directory_data_block(inode_number, index)?;
            entries
                .try_reserve(block.entries.len())
                .map_err(|_| XfsError::NoMemory)?;
            entries.extend(
                block
                    .entries
                    .into_iter()
                    .filter(|entry| entry.name != b"." && entry.name != b"..")
                    .map(|entry| XfsDirectoryEntry {
                        name: entry.name,
                        inode: entry.inode,
                        file_type: entry.file_type,
                    }),
            );
        }
        Ok(entries)
    }

    /// Reads the directory's one native `..` relationship.  It is kept
    /// separate from `directory_entries`, whose VFS-facing result excludes
    /// dot names.  A rewrite of an external directory must preserve this
    /// value; substituting the directory's own inode would silently detach a
    /// moved directory from its parent.
    pub fn directory_parent(&self, inode_number: u64) -> XfsResult<u64> {
        let inode = self.inode(inode_number)?;
        if inode.mode & 0o170000 != 0o040000 {
            return Err(XfsError::UnsupportedFeature);
        }
        if inode.data_format == XfsForkFormat::Local {
            let (_, raw) = self.inode_and_bytes(inode_number)?;
            let fork = inode.data_fork(&raw)?;
            if fork.len() < 6 {
                return Err(XfsError::CorruptMetadata);
            }
            let parent = if fork[1] == 0 {
                be32(fork, 2)? as u64
            } else {
                be64(fork, 2)?
            };
            return if parent == 0 {
                Err(XfsError::CorruptMetadata)
            } else {
                Ok(parent)
            };
        }
        let dir_block = self.directory_block_size()? as u64;
        let data_limit = cmp::min(inode.size, XFS_DIR_LEAF_SPACE_BYTES);
        let data_blocks = data_limit.div_ceil(dir_block);
        let extents = match inode.data_format {
            XfsForkFormat::Extents => self.inode_data_extents(inode_number)?,
            XfsForkFormat::Btree => self.inode_bmbt_extents(inode_number)?,
            _ => return Err(XfsError::UnsupportedFeature),
        };
        let fs_block = self.superblock.block_size as u64;
        let mut parent = None;
        for index in 0..data_blocks {
            let logical_start = index
                .checked_mul(dir_block)
                .ok_or(XfsError::AddressOutOfRange)?;
            let first = logical_start / fs_block;
            let last = logical_start
                .checked_add(dir_block)
                .and_then(|end| end.checked_sub(1))
                .ok_or(XfsError::AddressOutOfRange)?
                / fs_block;
            let Some(mapping) = extents.iter().find(|extent| {
                !extent.unwritten
                    && first >= extent.file_block
                    && last < extent.file_block.saturating_add(extent.block_count as u64)
            }) else {
                continue;
            };
            if mapping.file_block > first {
                return Err(XfsError::CorruptMetadata);
            }
            for entry in self.directory_data_block(inode_number, index)?.entries {
                if entry.name == b".." {
                    if entry.inode == 0 || parent.replace(entry.inode).is_some() {
                        return Err(XfsError::CorruptMetadata);
                    }
                }
            }
        }
        parent.ok_or(XfsError::CorruptMetadata)
    }

    /// Resolves a non-dot name through the native directory representation.
    /// A directory block's ftype is merely a cache hint: the inode core is
    /// always loaded by the caller before exposing an object to VFS.
    pub fn lookup_directory(&self, directory: u64, name: &[u8]) -> XfsResult<XfsInode> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let entry = self
            .directory_entries(directory)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(XfsError::AddressOutOfRange)?;
        self.inode(entry.inode)
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn data_volume(&self) -> &BlockVolume {
        &self.data
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn external_log(&self) -> Option<&BlockVolume> {
        self.external_log.as_ref()
    }

    // Volume write/realtime support in progress.
    #[allow(dead_code)]
    pub(crate) fn realtime_volume(&self) -> Option<&BlockVolume> {
        self.realtime.as_ref()
    }

    pub(super) fn log_volume(&self) -> XfsResult<&BlockVolume> {
        if self.superblock.log_start == 0 {
            self.external_log
                .as_ref()
                .ok_or(XfsError::UnsupportedFeature)
        } else {
            Ok(&self.data)
        }
    }

    pub(super) fn log_region_start_block(&self) -> XfsResult<u64> {
        if self.superblock.log_start == 0 {
            return Ok(0);
        }
        let per_fs_block = self.superblock.block_size as u64 / XFS_LOG_BASIC_BLOCK as u64;
        self.superblock
            .log_start
            .checked_mul(per_fs_block)
            .ok_or(XfsError::AddressOutOfRange)
    }

    pub(super) fn log_region_blocks(&self) -> XfsResult<u32> {
        let bytes = u64::from(self.superblock.log_blocks)
            .checked_mul(self.superblock.block_size as u64)
            .ok_or(XfsError::AddressOutOfRange)?;
        if bytes % XFS_LOG_BASIC_BLOCK as u64 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        u32::try_from(bytes / XFS_LOG_BASIC_BLOCK as u64).map_err(|_| XfsError::AddressOutOfRange)
    }

    /// Flushes every explicitly attached XFS member.  An external journal and
    /// realtime device are part of the same mount durability domain; flushing
    /// only the data device would acknowledge a sync while a committed log
    /// record or realtime extent was still volatile.
    pub fn flush(&self) -> XfsResult<()> {
        self.data.flush().map_err(XfsError::from)?;
        if let Some(log) = &self.external_log {
            log.flush().map_err(XfsError::from)?;
        }
        if let Some(realtime) = &self.realtime {
            realtime.flush().map_err(XfsError::from)?;
        }
        Ok(())
    }
}
