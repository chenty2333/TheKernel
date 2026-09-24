//! XFS log record headers and replayable log items.

use super::*;

/// Header of one committed XFS journal record.  Payload replay is purposely
/// not exposed yet: replay requires verified log-operation item decoders and
/// an atomic metadata write set, neither of which can be replaced by an
/// in-place block write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsLogRecordHeader {
    pub cycle: u32,
    pub version: u32,
    pub payload_bytes: u32,
    pub lsn: u64,
    pub tail_lsn: u64,
    pub previous_block: u32,
    pub operation_count: u32,
    /// Saved values displaced by the physical log's per-basic-block cycle
    /// stamping.  They are opaque to transaction item decoding but must round
    /// trip exactly when a ring writer reuses a record area.
    pub cycle_data: [u32; 64],
    pub format: u32,
    pub filesystem_uuid: XfsUuid,
    pub iclog_bytes: u32,
}

/// One framed log operation.  Its payload is copied from the record so a
/// recovery coordinator can retain a transaction across wrapped log I/O
/// without borrowing a DMA buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogOperation {
    pub transaction_id: u32,
    pub client_id: u8,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl XfsLogOperation {
    /// Splits only the data region of an operation at a physical record
    /// boundary.  The first fragment carries CONTINUE, interior fragments
    /// carry WAS_CONT|CONTINUE, and the final fragment carries WAS_CONT|END.
    /// START stays on the first region and COMMIT stays on the final region,
    /// preserving the transaction visibility rule during recovery.
    pub fn split_for_continuation(&self, maximum_payload: usize) -> XfsResult<Vec<Self>> {
        if maximum_payload == 0 {
            return Err(XfsError::AddressOutOfRange);
        }
        if self.payload.len() <= maximum_payload {
            let mut single = Vec::new();
            single.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            single.push(self.clone());
            return Ok(single);
        }
        let parts = self.payload.len().div_ceil(maximum_payload);
        let mut output = Vec::new();
        output
            .try_reserve_exact(parts)
            .map_err(|_| XfsError::NoMemory)?;
        for index in 0..parts {
            let start = index
                .checked_mul(maximum_payload)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = start
                .checked_add(maximum_payload)
                .map(|end| end.min(self.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
            let mut flags =
                self.flags & !(XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS);
            if index == 0 {
                flags |= XLOG_CONTINUE_TRANS;
            } else if index + 1 == parts {
                flags |= XLOG_WAS_CONT_TRANS | XLOG_END_TRANS;
            } else {
                flags |= XLOG_WAS_CONT_TRANS | XLOG_CONTINUE_TRANS;
            }
            if index != 0 {
                flags &= !XLOG_START_TRANS;
            }
            if index + 1 != parts {
                flags &= !XLOG_COMMIT_TRANS;
            }
            output.push(Self {
                transaction_id: self.transaction_id,
                client_id: self.client_id,
                flags,
                payload: self.payload[start..end].to_vec(),
            });
        }
        Ok(output)
    }
}

/// Byte order of host-native log-item payloads. Physical log headers and
/// operation headers are always big-endian; only the item bodies use this.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsLogByteOrder {
    Little,
    Big,
}

/// Transaction-header proof carried at the start of every recovered item
/// sequence. `item_count` is checked before any item is replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsTransactionHeader {
    pub transaction_type: u32,
    pub item_count: u32,
}

impl XfsTransactionHeader {
    pub const CHECKPOINT: u32 = 40;

    /// Native-endian wire body for the transaction-header log region.
    pub fn encode(self, transaction_id: u32, order: XfsLogByteOrder) -> XfsResult<[u8; 16]> {
        if self.item_count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut bytes = [0; 16];
        native_put_u32(&mut bytes, 0, 0x5452_414e, order)?;
        native_put_u32(&mut bytes, 4, self.transaction_type, order)?;
        native_put_u32(&mut bytes, 8, transaction_id, order)?;
        native_put_u32(&mut bytes, 12, self.item_count, order)?;
        Ok(bytes)
    }
}

/// One 128-byte dirty-region map of a logged buffer. The actual bytes are
/// deliberately retained separately; recovery must consume exactly one data
/// region for every set bit before writing a home block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBufferReplayItem {
    pub flags: u16,
    pub block_number: u64,
    pub block_count: u16,
    pub dirty_chunks: Vec<u32>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsMetadataBufferType {
    Agf,
    Agfl,
    Agi,
    Inode,
    Dquot,
    Btree,
    Directory,
    Attribute,
    Superblock,
    Realtime,
}

impl XfsBufferReplayItem {
    /// Decodes the persistent buffer class encoded in the upper BLF flag bits.
    /// Recovery selects checksum/LSN ownership from this typed value instead
    /// of guessing from a post-patch magic number.
    pub fn metadata_type(&self) -> XfsResult<XfsMetadataBufferType> {
        match (self.flags >> 11) & 0x1f {
            5 => Ok(XfsMetadataBufferType::Agf),
            6 => Ok(XfsMetadataBufferType::Agfl),
            3 => Ok(XfsMetadataBufferType::Dquot),
            7 => Ok(XfsMetadataBufferType::Agi),
            8 => Ok(XfsMetadataBufferType::Inode),
            4 => Ok(XfsMetadataBufferType::Btree),
            10..=14 => Ok(XfsMetadataBufferType::Directory),
            15..=17 => Ok(XfsMetadataBufferType::Attribute),
            18 => Ok(XfsMetadataBufferType::Superblock),
            19 | 20 => Ok(XfsMetadataBufferType::Realtime),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    pub(super) fn crc_lsn_offsets(&self) -> XfsResult<(usize, Option<usize>)> {
        match self.metadata_type()? {
            XfsMetadataBufferType::Agf => Ok((216, Some(208))),
            XfsMetadataBufferType::Agfl => Ok((32, Some(24))),
            XfsMetadataBufferType::Agi => Ok((312, Some(320))),
            XfsMetadataBufferType::Btree => Ok((64, Some(32))),
            // Dir3 data blocks keep their crc at 4/LSN at 16, whereas DA3
            // (attribute/node/leaf) blocks embed xfs_da_blkinfo first and
            // therefore keep them at 12/24.  Attribute buffer items always
            // use the latter format.
            XfsMetadataBufferType::Directory => Ok((4, Some(16))),
            XfsMetadataBufferType::Attribute => Ok((12, Some(24))),
            XfsMetadataBufferType::Superblock => Ok((224, Some(240))),
            // Legacy realtime inode contents are raw native-endian words.
            // Rtgroup media identifies itself with one of the two magic
            // values below, and is handled from the completed image.
            XfsMetadataBufferType::Realtime => return Err(XfsError::UnsupportedFeature),
            // v3 inode cores carry their CRC at byte 100 and last-update LSN
            // at byte 112.  The buffer may contain several fixed-size inodes;
            // callers rewrite every CRC, while the LSN gate uses the first
            // core and rejects mixed-LSN buffers below.
            XfsMetadataBufferType::Inode => Ok((100, Some(112))),
            XfsMetadataBufferType::Dquot => Ok((108, Some(112))),
        }
    }

    pub(super) fn btree_crc_lsn_offsets(bytes: &[u8]) -> XfsResult<(usize, Option<usize>)> {
        // AG btrees and BMBTs intentionally share the BUF-item class.  Their
        // v5 headers do not share checksum/LSN locations, so dispatch from
        // the completed home image's magic rather than treating every tree as
        // a BMBT during replay.
        match be32(bytes, 0)? {
            XFS_BMAP_CRC_MAGIC => Ok((64, Some(32))),
            0x4142_3342
            | 0x4142_3343
            | 0x4941_4233
            | 0x4649_4233
            | XFS_RMAP_CRC_MAGIC
            | XFS_REFCOUNT_CRC_MAGIC => Ok((52, Some(24))),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    pub(super) fn rewrite_inode_crcs(&self, bytes: &mut [u8], inode_size: usize) -> XfsResult<()> {
        if self.metadata_type()? != XfsMetadataBufferType::Inode
            || inode_size == 0
            || bytes.len() % inode_size != 0
        {
            return Err(XfsError::CorruptMetadata);
        }
        for inode in bytes.chunks_exact_mut(inode_size) {
            if be16(inode, 0)? != XFS_DINODE_MAGIC {
                continue;
            }
            if byte(inode, 4)? >= 3 {
                rewrite_crc32c(inode, 100)?;
            }
        }
        Ok(())
    }

    /// Applies this item's logged 128-byte regions to one complete home
    /// buffer.  This is deliberately a pure transformation: callers retain
    /// the old image until the complete transaction has been prepared, so a
    /// malformed item can never leave an allocation group partially replayed.
    ///
    /// XFS buffer log addresses are basic-block addresses.  The log carries
    /// only dirty chunks, rather than a whole block image, hence every bitmap
    /// chunk must fit the exact `blf_len` range before the first byte is
    /// changed.  Metadata LSN and CRC are then regenerated from the same
    /// image, making a second replay of an already durable item observable as
    /// an LSN no-op at the volume boundary.
    pub fn materialize_home_image(
        &self,
        home: &[u8],
        lsn: u64,
        inode_size: usize,
    ) -> XfsResult<Vec<u8>> {
        let expected = usize::from(self.block_count)
            .checked_mul(512)
            .ok_or(XfsError::AddressOutOfRange)?;
        if expected == 0 || home.len() != expected || self.dirty_chunks.len() != self.chunks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut image = home.to_vec();
        for (chunk, bytes) in self.dirty_chunks.iter().zip(&self.chunks) {
            if bytes.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            let offset = usize::try_from(*chunk)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_mul(128)
                .ok_or(XfsError::AddressOutOfRange)?;
            let end = offset.checked_add(128).ok_or(XfsError::AddressOutOfRange)?;
            if end > image.len() {
                return Err(XfsError::CorruptMetadata);
            }
            image[offset..end].copy_from_slice(bytes);
        }
        if self.metadata_type()? == XfsMetadataBufferType::Realtime {
            match be32(&image, 0)? {
                0x424d_505a | 0x5355_4d59 => {
                    let field = image.get_mut(24..32).ok_or(XfsError::CorruptMetadata)?;
                    field.copy_from_slice(&lsn.to_be_bytes());
                    rewrite_crc32c(&mut image, 4)?;
                }
                // Pre-rtgroup bitmap and summary files are raw host-endian
                // words with no checksum or LSN field.
                _ => {}
            }
            return Ok(image);
        }
        let (crc_offset, lsn_offset) = if self.metadata_type()? == XfsMetadataBufferType::Btree {
            Self::btree_crc_lsn_offsets(&image)?
        } else {
            self.crc_lsn_offsets()?
        };
        if self.metadata_type()? == XfsMetadataBufferType::Inode {
            if inode_size == 0 || image.len() % inode_size != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            for inode in image.chunks_exact_mut(inode_size) {
                let field = inode
                    .get_mut(lsn_offset.unwrap_or(0)..lsn_offset.unwrap_or(0) + 8)
                    .ok_or(XfsError::CorruptMetadata)?;
                field.copy_from_slice(&lsn.to_be_bytes());
            }
            self.rewrite_inode_crcs(&mut image, inode_size)?;
        } else {
            if let Some(offset) = lsn_offset {
                let field = image
                    .get_mut(offset..offset + 8)
                    .ok_or(XfsError::CorruptMetadata)?;
                field.copy_from_slice(&lsn.to_be_bytes());
            }
            rewrite_crc32c(&mut image, crc_offset)?;
        }
        Ok(image)
    }

    /// Returns the durable LSN encoded in a v5 home buffer when that metadata
    /// class carries one.  A missing LSN is intentionally not treated as zero:
    /// v4 and realtime records cannot be replayed safely by an LSN-only
    /// idempotence rule and remain outside the writable admission set.
    pub fn home_lsn(&self, home: &[u8]) -> XfsResult<Option<u64>> {
        if self.metadata_type()? == XfsMetadataBufferType::Realtime {
            return match be32(home, 0)? {
                0x424d_505a | 0x5355_4d59 => be64(home, 24).map(Some),
                _ => Ok(None),
            };
        }
        let (_, lsn_offset) = if self.metadata_type()? == XfsMetadataBufferType::Btree {
            Self::btree_crc_lsn_offsets(home)?
        } else {
            self.crc_lsn_offsets()?
        };
        let Some(offset) = lsn_offset else {
            return Ok(None);
        };
        if self.metadata_type()? != XfsMetadataBufferType::Inode {
            return be64(home, offset).map(Some);
        }
        // Buffer replay must not silently choose one inode's LSN when a
        // multi-inode logged buffer mixes generations.  The volume checks the
        // full image length before calling this helper and then uses the
        // first core only as its all-cores durable marker.
        be64(home, offset).map(Some)
    }

    /// Encodes the native-endian `BUF` item format plus BCHUNK payload
    /// regions. Physical operation headers and START/COMMIT/CONTINUE flags
    /// are owned by the transaction coordinator, which may split regions at a
    /// log-ring boundary without changing this item wire.
    pub fn encode_log_regions(&self, order: XfsLogByteOrder) -> XfsResult<Vec<Vec<u8>>> {
        if self.block_count == 0 || self.dirty_chunks.len() != self.chunks.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut words = 0usize;
        for chunk in &self.dirty_chunks {
            let word = usize::try_from(*chunk)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_div(32)
                .and_then(|word| word.checked_add(1))
                .ok_or(XfsError::AddressOutOfRange)?;
            words = words.max(word);
        }
        if words == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let format_len = 20usize
            .checked_add(words.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
            .ok_or(XfsError::AddressOutOfRange)?;
        let mut format = vec![0; format_len];
        native_put_u16(&mut format, 0, 0x123c, order)?;
        native_put_u16(
            &mut format,
            2,
            u16::try_from(
                self.chunks
                    .len()
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::AddressOutOfRange)?,
            order,
        )?;
        native_put_u16(&mut format, 4, self.flags, order)?;
        native_put_u16(&mut format, 6, self.block_count, order)?;
        native_put_u64(&mut format, 8, self.block_number, order)?;
        native_put_u32(
            &mut format,
            16,
            u32::try_from(words).map_err(|_| XfsError::AddressOutOfRange)?,
            order,
        )?;
        for (chunk, bytes) in self.dirty_chunks.iter().zip(&self.chunks) {
            if bytes.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            let word = usize::try_from(*chunk).map_err(|_| XfsError::AddressOutOfRange)? / 32;
            let bit = *chunk % 32;
            let previous = native_u32(&format, 20 + word * 4, order)?;
            native_put_u32(&mut format, 20 + word * 4, previous | (1u32 << bit), order)?;
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(
                self.chunks
                    .len()
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            )
            .map_err(|_| XfsError::NoMemory)?;
        result.push(format);
        for chunk in &self.chunks {
            result.push(chunk.clone());
        }
        Ok(result)
    }
}

/// Typed intent/done pairing key used by EFI/EFD, RUI/RUD, CUI/CUD, and
/// BUI/BUD. An intent that lacks its matching done item remains pending and
/// is replayed; a done without a prior intent corrupts the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsIntentKind {
    ExtentFree,
    Rmap,
    Refcount,
    Bmap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsIntentKey {
    pub kind: XfsIntentKind,
    pub id: u64,
}

/// One on-log extent, decoded from the native-endian payload of an intent or
/// done item.  It deliberately retains the operation flags: interpreting an
/// rmap/refcount/bmap operation is the responsibility of the corresponding
/// metadata replay engine, not the log framing decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsLogReplayExtent {
    ExtentFree {
        start_block: u64,
        block_count: u32,
    },
    Mapping {
        owner: u64,
        start_block: u64,
        start_offset: u64,
        block_count: u32,
        flags: u32,
    },
    Refcount {
        start_block: u64,
        block_count: u32,
        flags: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsInodeReplayItem {
    pub inode: u64,
    pub block_number: u64,
    pub block_count: u32,
    pub byte_offset: u32,
    pub fields: u32,
    /// `ilf_dsize` and `ilf_asize` are byte counts, not region counts.
    pub data_size: u16,
    pub attr_size: u16,
    /// The inode core/fork regions following the format region, in their
    /// native log order.  They are not interpreted by this framing layer.
    pub regions: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDquotReplayItem {
    pub id: u32,
    pub block_number: u64,
    pub block_count: u32,
    pub byte_offset: u32,
    pub disk_dquot: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsDiskDquot {
    pub id: u32,
    pub quota_type: u8,
    pub lsn: Option<u64>,
}

impl XfsInodeReplayItem {
    /// Converts native-endian `xfs_log_dinode` data and fork regions to a
    /// complete big-endian dinode; journal inode cores must never be copied.
    pub fn materialize_home_inode(
        &self,
        home: &[u8],
        lsn: u64,
        meta_uuid: Option<XfsUuid>,
        order: XfsLogByteOrder,
    ) -> XfsResult<Vec<u8>> {
        const CORE: u32 = 0x001;
        const DDATA: u32 = 0x002;
        const DEXT: u32 = 0x004;
        const DBROOT: u32 = 0x008;
        const ADATA: u32 = 0x040;
        const AEXT: u32 = 0x080;
        const ABROOT: u32 = 0x100;
        const SUPPORTED: u32 = CORE | DDATA | DEXT | DBROOT | ADATA | AEXT | ABROOT;
        if lsn == 0
            || self.fields & !SUPPORTED != 0
            || self.fields & CORE == 0
            || (self.fields & (DDATA | DEXT | DBROOT)).count_ones() > 1
            || (self.fields & (ADATA | AEXT | ABROOT)).count_ones() > 1
        {
            return Err(XfsError::UnsupportedFeature);
        }
        let core = self.regions.first().ok_or(XfsError::CorruptMetadata)?;
        let version = byte(core, 4)?;
        let core_bytes = if version >= 3 {
            176
        } else if version >= 1 {
            100
        } else {
            return Err(XfsError::CorruptMetadata);
        };
        if core.len() != core_bytes || home.len() < core_bytes {
            return Err(XfsError::CorruptMetadata);
        }
        let expected = 1 + usize::from(self.data_size != 0) + usize::from(self.attr_size != 0);
        if self.regions.len() != expected
            || (self.fields & (DDATA | DEXT | DBROOT) != 0) != (self.data_size != 0)
            || (self.fields & (ADATA | AEXT | ABROOT) != 0) != (self.attr_size != 0)
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut out = home.to_vec();
        put_be16(&mut out, 0, native_u16(core, 0, order)?)?;
        for &(offset, width) in &[
            (2usize, 2usize),
            (8, 4),
            (12, 4),
            (16, 4),
            (20, 2),
            (22, 2),
            (24, 8),
            (56, 8),
            (64, 8),
            (72, 4),
            (76, 4),
            (80, 2),
            (84, 4),
            (88, 2),
            (90, 2),
            (92, 4),
            (96, 4),
        ] {
            match width {
                2 => put_be16(&mut out, offset, native_u16(core, offset, order)?)?,
                4 => put_be32(&mut out, offset, native_u32(core, offset, order)?)?,
                8 => put_be64(&mut out, offset, native_u64(core, offset, order)?)?,
                _ => return Err(XfsError::CorruptMetadata),
            }
        }
        out[4..8].copy_from_slice(slice(core, 4, 4)?);
        let flags2 = if version >= 3 {
            native_u64(core, 120, order)?
        } else {
            0
        };
        let bigtime = flags2 & XfsInode::DIFLAG2_BIGTIME != 0;
        for &offset in &[32usize, 40, 48] {
            if bigtime {
                put_be64(&mut out, offset, native_u64(core, offset, order)?)?;
            } else {
                put_be32(&mut out, offset, native_u32(core, offset, order)?)?;
                put_be32(&mut out, offset + 4, native_u32(core, offset + 4, order)?)?;
            }
        }
        if version >= 3 {
            put_be64(&mut out, 104, native_u64(core, 104, order)?)?;
            put_be64(&mut out, 112, lsn)?;
            put_be64(&mut out, 120, flags2)?;
            put_be32(&mut out, 128, native_u32(core, 128, order)?)?;
            out[132..144].copy_from_slice(slice(core, 132, 12)?);
            if bigtime {
                put_be64(&mut out, 144, native_u64(core, 144, order)?)?;
            } else {
                put_be32(&mut out, 144, native_u32(core, 144, order)?)?;
                put_be32(&mut out, 148, native_u32(core, 148, order)?)?;
            }
            let ino = native_u64(core, 152, order)?;
            if ino != self.inode {
                return Err(XfsError::CorruptMetadata);
            }
            put_be64(&mut out, 152, ino)?;
            out[160..176].copy_from_slice(slice(core, 160, 16)?);
            if let Some(uuid) = meta_uuid {
                if slice(core, 160, 16)? != uuid.0 {
                    return Err(XfsError::CorruptMetadata);
                }
            }
        }
        let forkoff = usize::from(byte(&out, 82)?)
            .checked_mul(8)
            .ok_or(XfsError::AddressOutOfRange)?;
        if forkoff != 0 && (forkoff < core_bytes || forkoff > out.len()) {
            return Err(XfsError::CorruptMetadata);
        }
        let data_end = if forkoff == 0 { out.len() } else { forkoff };
        let mut next = 1usize;
        if self.data_size != 0 {
            let payload = self.regions.get(next).ok_or(XfsError::CorruptMetadata)?;
            next += 1;
            if payload.len() != usize::from(self.data_size) || payload.len() > data_end - core_bytes
            {
                return Err(XfsError::CorruptMetadata);
            }
            out[core_bytes..core_bytes + payload.len()].copy_from_slice(payload);
        }
        if self.attr_size != 0 {
            let payload = self.regions.get(next).ok_or(XfsError::CorruptMetadata)?;
            if forkoff == 0
                || payload.len() != usize::from(self.attr_size)
                || payload.len() > out.len() - forkoff
            {
                return Err(XfsError::CorruptMetadata);
            }
            out[forkoff..forkoff + payload.len()].copy_from_slice(payload);
        }
        if version >= 3 {
            rewrite_crc32c(&mut out, 100)?;
        }
        Ok(out)
    }
}

impl XfsDquotReplayItem {
    pub fn parse_disk_dquot(
        &self,
        v5: bool,
        meta_uuid: Option<XfsUuid>,
        bigtime_enabled: bool,
    ) -> XfsResult<XfsDiskDquot> {
        if self.disk_dquot.len() != 104
            || be16(&self.disk_dquot, 0)? != 0x4451
            || byte(&self.disk_dquot, 2)? != 1
            || be32(&self.disk_dquot, 4)? != self.id
        {
            return Err(XfsError::CorruptMetadata);
        }
        let quota_type = byte(&self.disk_dquot, 3)?;
        if !matches!(quota_type & 0x07, 1 | 2 | 4) || quota_type & !0x87 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let has_bigtime = quota_type & XfsDquot::DQTYPE_BIGTIME != 0;
        if (self.id == 0 && has_bigtime) || (self.id != 0 && has_bigtime != bigtime_enabled) {
            return Err(XfsError::CorruptMetadata);
        }
        if v5 && meta_uuid.is_none() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsDiskDquot {
            id: self.id,
            quota_type: quota_type & 0x07,
            lsn: None,
        })
    }
    pub(super) fn materialize_home_dquot(
        &self,
        home: &[u8],
        lsn: u64,
        v5: bool,
        meta_uuid: Option<XfsUuid>,
        bigtime_enabled: bool,
    ) -> XfsResult<Vec<u8>> {
        let _ = self.parse_disk_dquot(v5, meta_uuid, bigtime_enabled)?;
        if home.len() != 136 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut out = home.to_vec();
        out[..104].copy_from_slice(&self.disk_dquot);
        if v5 {
            put_be64(&mut out, 112, lsn)?;
            if let Some(uuid) = meta_uuid {
                out[120..136].copy_from_slice(&uuid.0);
            }
            rewrite_crc32c(&mut out, 108)?;
        }
        Ok(out)
    }
}

impl XfsDquot {
    pub(super) const DQTYPE_BIGTIME: u8 = 0x80;
    /// Checks every invariant that survives an in-flight counter/timer
    /// update.  The caller may deliberately hold a stale CRC until a log LSN
    /// has been selected, so checksum validation belongs in `parse` below.
    pub(super) fn validate_image_identity(
        bytes: &[u8],
        expected_id: u32,
        expected_type: u8,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<u8> {
        if bytes.len() != 136
            || be16(bytes, 0)? != 0x4451
            || byte(bytes, 2)? != 1
            || byte(bytes, 3)? & 0x07 != expected_type
            || byte(bytes, 3)? & !0x87 != 0
            || be32(bytes, 4)? != expected_id
        {
            return Err(XfsError::CorruptMetadata);
        }
        let quota_type_flags = byte(bytes, 3)?;
        let has_bigtime = quota_type_flags & Self::DQTYPE_BIGTIME != 0;
        // Root dquots store grace *durations*, which are always legacy
        // seconds.  Non-root dquots store expiry timestamps and must match
        // the filesystem's BIGTIME on-disk format exactly.
        if (expected_id == 0 && has_bigtime) || (expected_id != 0 && has_bigtime != bigtime_enabled)
        {
            return Err(XfsError::CorruptMetadata);
        }
        if slice(bytes, 120, 16)? != meta_uuid.0 {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(quota_type_flags)
    }

    pub(super) fn parse(
        bytes: &[u8],
        expected_id: u32,
        expected_type: u8,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<Self> {
        // xfs_dqblk: the fixed xfs_disk_dquot region is exactly 136 bytes.
        // Verify the v5 checksum, LSN-bearing UUID tail, identity and type
        // before exposing counters to either accounting or admission.
        let quota_type_flags = Self::validate_image_identity(
            bytes,
            expected_id,
            expected_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        verify_crc32c(bytes, 108)?;
        Ok(Self {
            id: expected_id,
            quota_type: expected_type,
            quota_type_flags,
            block_hard: be64(bytes, 8)?,
            block_soft: be64(bytes, 16)?,
            inode_hard: be64(bytes, 24)?,
            inode_soft: be64(bytes, 32)?,
            realtime_hard: be64(bytes, 72)?,
            realtime_soft: be64(bytes, 80)?,
            blocks: be64(bytes, 40)?,
            inodes: be64(bytes, 48)?,
            realtime_blocks: be64(bytes, 88)?,
            inode_timer: be32(bytes, 56)?,
            block_timer: be32(bytes, 60)?,
            realtime_timer: be32(bytes, 96)?,
            inode_warnings: be16(bytes, 64)?,
            block_warnings: be16(bytes, 66)?,
            realtime_warnings: be16(bytes, 100)?,
        })
    }

    pub(super) fn apply_delta(
        &self,
        block_delta: i64,
        inode_delta: i64,
        enforce: bool,
        now: u64,
        block_grace: u32,
        inode_grace: u32,
    ) -> XfsResult<XfsDquotAdmission> {
        let blocks = if block_delta >= 0 {
            self.blocks.checked_add(block_delta as u64)
        } else {
            self.blocks.checked_sub(block_delta.unsigned_abs())
        }
        .ok_or(XfsError::CorruptMetadata)?;
        let inodes = if inode_delta >= 0 {
            self.inodes.checked_add(inode_delta as u64)
        } else {
            self.inodes.checked_sub(inode_delta.unsigned_abs())
        }
        .ok_or(XfsError::CorruptMetadata)?;
        // A zero limit is unlimited.  Grace timers are persistent policy;
        // crossing a soft limit is allowed here, while hard limits reject the
        // whole enclosing metadata transaction before a log reservation.
        if enforce
            && ((self.block_hard != 0 && blocks > self.block_hard)
                || (self.inode_hard != 0 && inodes > self.inode_hard))
        {
            return Err(XfsError::QuotaExceeded);
        }
        let (block_timer, block_warnings) = self.soft_admission(
            blocks,
            self.block_soft,
            self.block_timer,
            self.block_warnings,
            now,
            block_grace,
            enforce,
        )?;
        let (inode_timer, inode_warnings) = self.soft_admission(
            inodes,
            self.inode_soft,
            self.inode_timer,
            self.inode_warnings,
            now,
            inode_grace,
            enforce,
        )?;
        Ok(XfsDquotAdmission {
            blocks,
            inodes,
            block_timer,
            inode_timer,
            block_warnings,
            inode_warnings,
        })
    }

    pub(super) fn timer_to_unix(&self, timer: u32) -> XfsResult<u64> {
        if timer == 0 {
            return Ok(0);
        }
        if self.quota_type_flags & Self::DQTYPE_BIGTIME != 0 {
            Ok(u64::from(timer) << 2)
        } else {
            Ok(u64::from(timer))
        }
    }

    pub(super) fn unix_to_timer(&self, unix: u64) -> XfsResult<u32> {
        if self.quota_type_flags & Self::DQTYPE_BIGTIME != 0 {
            // XFS_DQ_BIGTIME_SHIFT=2; round up to avoid shortening grace.
            u32::try_from(unix.checked_add(3).ok_or(XfsError::AddressOutOfRange)? >> 2)
                .map_err(|_| XfsError::AddressOutOfRange)
        } else {
            u32::try_from(unix).map_err(|_| XfsError::AddressOutOfRange)
        }
    }

    pub(super) fn soft_admission(
        &self,
        used: u64,
        soft: u64,
        timer: u32,
        warnings: u16,
        now: u64,
        grace: u32,
        enforce: bool,
    ) -> XfsResult<(u32, u16)> {
        if soft == 0 || used <= soft {
            return Ok((0, 0));
        }
        let expires = self.timer_to_unix(timer)?;
        if expires != 0 && enforce && now >= expires {
            return Err(XfsError::QuotaExceeded);
        }
        if timer == 0 {
            let expiry = now
                .checked_add(u64::from(grace))
                .ok_or(XfsError::AddressOutOfRange)?;
            return Ok((
                self.unix_to_timer(expiry.max(1))?,
                warnings.saturating_add(1),
            ));
        }
        Ok((timer, warnings))
    }
}

impl XfsDquotDelta {
    pub(super) fn log_item(
        &self,
        lsn: u64,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
    ) -> XfsResult<XfsDquotReplayItem> {
        if lsn == 0
            || self.before.len() != 136
            || self.after.len() != 136
            || self.block_count == 0
            || usize::try_from(self.byte_offset)
                .map_err(|_| XfsError::AddressOutOfRange)?
                .checked_add(136)
                .is_none_or(|end| {
                    end > usize::try_from(self.block_count)
                        .unwrap_or(0)
                        .saturating_mul(512)
                })
        {
            return Err(XfsError::CorruptMetadata);
        }
        let _ = XfsDquot::parse(
            &self.before,
            self.id,
            self.quota_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        // `after` carries changed counters/timers and therefore intentionally
        // has the pre-transaction CRC until this selected LSN is installed.
        let _ = XfsDquot::validate_image_identity(
            &self.after,
            self.id,
            self.quota_type,
            meta_uuid,
            bigtime_enabled,
        )?;
        let mut image = self.after.clone();
        put_be64(&mut image, 112, lsn)?;
        image[120..136].copy_from_slice(&meta_uuid.0);
        rewrite_crc32c(&mut image, 108)?;
        let _ = XfsDquot::parse(&image, self.id, self.quota_type, meta_uuid, bigtime_enabled)?;
        Ok(XfsDquotReplayItem {
            id: self.id,
            block_number: self.basic_block,
            block_count: self.block_count,
            byte_offset: self.byte_offset,
            disk_dquot: image[..104].to_vec(),
        })
    }

    pub(super) fn encode_log_regions(
        &self,
        lsn: u64,
        meta_uuid: XfsUuid,
        bigtime_enabled: bool,
        order: XfsLogByteOrder,
    ) -> XfsResult<Vec<Vec<u8>>> {
        let item = self.log_item(lsn, meta_uuid, bigtime_enabled)?;
        let mut format = vec![0; 24];
        native_put_u16(&mut format, 0, 0x123d, order)?;
        native_put_u16(&mut format, 2, 2, order)?;
        native_put_u32(&mut format, 4, item.id, order)?;
        native_put_u64(&mut format, 8, item.block_number, order)?;
        native_put_u32(&mut format, 16, item.block_count, order)?;
        native_put_u32(&mut format, 20, item.byte_offset, order)?;
        Ok(vec![format, item.disk_dquot])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsIntentReplayItem {
    pub key: XfsIntentKey,
    pub extents: Vec<XfsLogReplayExtent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDoneReplayItem {
    pub key: XfsIntentKey,
    /// EFD records include completed extents; the other done-item formats do
    /// not.  Keeping this typed distinction prevents callers from confusing
    /// an empty done body with a truncated EFD.
    pub extents: Vec<XfsLogReplayExtent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XfsReplayItem {
    Buffer(XfsBufferReplayItem),
    Inode(XfsInodeReplayItem),
    Dquot(XfsDquotReplayItem),
    Intent(XfsIntentReplayItem),
    Done(XfsDoneReplayItem),
    Quotaoff { flags: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsHomeWriteDescriptor {
    pub basic_block: u64,
    pub bytes: Vec<u8>,
    pub lsn: u64,
    pub item: XfsBufferReplayItem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRecoveryCommit {
    pub lsn: u64,
    pub writes: Vec<XfsHomeWriteDescriptor>,
}

/// The replay-relevant journal boundary obtained while walking complete log
/// records.  This is not an on-disk "clean" flag: XFS has no safe synthetic
/// clean marker.  It describes a *plan*; after callers apply every committed
/// item they discard the plan rather than forging a clean journal record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsJournalRecoveryState {
    pub head_lsn: u64,
    pub tail_lsn: u64,
    pub committed_transactions: usize,
    pub interrupted_transactions: usize,
}

/// Result of walking the physical XFS log rather than accepting caller-made
/// record images.  `clean` is deliberately conservative: it is true only
/// when the log region contains no complete, authenticated record.  XFS does
/// not have a synthetic clean-record format, so a scanner never fabricates a
/// clean cursor after seeing stale or torn media.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsPhysicalLogScan {
    pub records: Vec<XfsJournalRecord>,
    pub state: XfsJournalRecoveryState,
    pub cursor: Option<XfsLogRing>,
    pub clean: bool,
}
