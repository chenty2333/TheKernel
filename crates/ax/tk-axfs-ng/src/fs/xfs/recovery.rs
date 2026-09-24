//! XFS journal recovery: sessions, intents and replay helpers.

use super::*;

/// Restartable application of a fully decoded, buffer-only recovery plan.
/// Each step ends with FUA+flush, so a power loss between steps is harmless:
/// opening a new session from the same journal simply observes the installed
/// home LSN and skips that descriptor.  The session deliberately has no
/// `Deref<XfsVolume>` implementation, preventing an unrecovered volume from
/// being accidentally passed to the VFS publication path.
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
pub(crate) struct XfsRecoverySession {
    pub(super) volume: Arc<XfsVolume>,
    pub(super) commits: Vec<XfsRecoveryCommit>,
    pub(super) next: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
pub(super) struct XfsPendingExtentFreeRecovery {
    pub(super) lsn: u64,
    pub(super) extents: Vec<(u64, u32)>,
}

/// Restartable semantic replay for the subset of intent items whose home
/// metadata is implemented here: pending EFIs.  A step keeps its prepared
/// buffer commit across an I/O failure, so retry never re-plans a partially
/// installed AG free-space tree against changed media.
// Recovery-session driver for the in-progress journal recovery path.
#[allow(dead_code)]
pub(crate) struct XfsIntentRecoverySession {
    pub(super) volume: Arc<XfsVolume>,
    pub(super) steps: Vec<XfsPendingExtentFreeRecovery>,
    pub(super) next: usize,
    pub(super) prepared: Option<XfsRecoveryCommit>,
}

/// Coordinates recovery work that is itself made durable as ordinary XFS log
/// transactions.  The original log establishes *what* must be replayed; this
/// coordinator never writes its home images directly.  Instead it stages one
/// complete replacement set, commits a fresh record with FUA, waits for the
/// normal home-write flush, and only then checkpoints that generated record.
///
/// Intent items are reduced to complete replacement sets before they are
/// committed.  In particular, no recovery path edits a btree leaf in place:
/// the normal staged writers replace the verified tree, journal the new
/// images, install them with FUA, and checkpoint only after the home flush.
pub(crate) struct XfsRecoveryJournalCoordinator {
    pub(super) volume: Arc<XfsVolume>,
    pub(super) ring: XfsLogRing,
    pub(super) ail: XfsAil,
    pub(super) next_transaction: u32,
}

impl XfsRecoveryJournalCoordinator {
    pub fn new(volume: Arc<XfsVolume>, ring: XfsLogRing) -> XfsResult<Self> {
        if !volume.superblock.is_v5() {
            return Err(XfsError::UnsupportedFeature);
        }
        Ok(Self {
            volume,
            ring,
            ail: XfsAil::default(),
            // Keep recovery-generated transaction ids distinct from the
            // recovered clients' ids.  They are AIL identities, not an on
            // disk compatibility contract.
            next_transaction: 0x8000_0000,
        })
    }

    pub(super) fn generated_transaction_id(&mut self) -> XfsResult<u32> {
        let id = self.next_transaction;
        self.next_transaction = self
            .next_transaction
            .checked_add(1)
            .filter(|next| *next != 0)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(id)
    }

    pub(super) fn append_pending_efi(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut groups = Vec::<(u32, Vec<(u32, u32)>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::ExtentFree {
                start_block,
                block_count,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            let end = start_block
                .checked_add(u64::from(*block_count))
                .ok_or(XfsError::AddressOutOfRange)?;
            if *block_count == 0 || end > self.volume.superblock.data_blocks {
                return Err(XfsError::AddressOutOfRange);
            }
            let mut start = *start_block;
            while start < end {
                let ag =
                    u32::try_from(start / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let relative =
                    u32::try_from(start % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.volume.superblock.ag_count {
                    return Err(XfsError::AddressOutOfRange);
                }
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
        for (ag, frees) in groups {
            let staged = self.volume.stage_recovery_extent_frees(ag, &frees)?;
            metadata
                .buffers
                .try_reserve(staged.buffers.len())
                .map_err(|_| XfsError::NoMemory)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    pub(super) fn append_pending_rmap(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const MAP: u32 = 1;
        const MAP_SHARED: u32 = 2;
        const UNMAP: u32 = 3;
        const UNMAP_SHARED: u32 = 4;
        const CONVERT: u32 = 5;
        const CONVERT_SHARED: u32 = 6;
        const ATTR: u32 = 1 << 31;
        const BMBT: u32 = 1 << 30;
        const UNWRITTEN: u32 = 1 << 29;
        // These are the documented high bits of xfs_rmap_rec::rm_offset.
        const RM_ATTR: u64 = 1 << 63;
        const RM_BMBT: u64 = 1 << 62;
        const RM_UNWRITTEN: u64 = 1 << 61;
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut sets = Vec::<(u32, Vec<XfsRmapRecord>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::Mapping {
                owner,
                start_block,
                start_offset,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            let kind = flags & 0xff;
            if !matches!(
                kind,
                MAP | MAP_SHARED | UNMAP | UNMAP_SHARED | CONVERT | CONVERT_SHARED
            ) {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut left = u64::from(*block_count);
            let mut physical = *start_block;
            let mut file = *start_offset;
            while left != 0 {
                let ag =
                    u32::try_from(physical / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let agbno =
                    u32::try_from(physical % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let length = u32::try_from(left.min(ag_blocks - u64::from(agbno)))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                if ag >= self.volume.superblock.ag_count || length == 0 {
                    return Err(XfsError::AddressOutOfRange);
                }
                let index =
                    if let Some(index) = sets.iter().position(|(candidate, _)| *candidate == ag) {
                        index
                    } else {
                        let records = self.volume.rmap_records(ag)?;
                        sets.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        sets.push((ag, records));
                        sets.len() - 1
                    };
                let offset = file
                    | if flags & ATTR != 0 { RM_ATTR } else { 0 }
                    | if flags & BMBT != 0 { RM_BMBT } else { 0 }
                    | if flags & UNWRITTEN != 0 {
                        RM_UNWRITTEN
                    } else {
                        0
                    };
                let record = XfsRmapRecord {
                    start_block: agbno,
                    block_count: length,
                    owner: *owner,
                    offset,
                };
                let records = &mut sets[index].1;
                match kind {
                    MAP | MAP_SHARED => {
                        if records.iter().any(|old| {
                            ranges_overlap_u32(
                                old.start_block,
                                old.block_count,
                                record.start_block,
                                record.block_count,
                            ) && old.owner == record.owner
                                && old.offset == record.offset
                        }) {
                            return Err(XfsError::CorruptMetadata);
                        }
                        records.push(record);
                    }
                    UNMAP | UNMAP_SHARED => {
                        // A RUI may retire the middle of a previously coalesced
                        // rmap record.  Do not require byte-for-byte equality:
                        // retain the two still-owned fragments, with their
                        // file offsets advanced by the physical split.
                        replace_rmap_subrange(
                            records,
                            record,
                            None,
                            RM_ATTR | RM_BMBT | RM_UNWRITTEN,
                        )?;
                    }
                    CONVERT | CONVERT_SHARED => {
                        let opposite = XfsRmapRecord {
                            offset: record.offset ^ RM_UNWRITTEN,
                            ..record
                        };
                        replace_rmap_subrange(
                            records,
                            opposite,
                            Some(record),
                            RM_ATTR | RM_BMBT | RM_UNWRITTEN,
                        )?;
                    }
                    _ => return Err(XfsError::UnsupportedFeature),
                }
                physical = physical
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                file = file
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                left -= u64::from(length);
            }
        }
        for (ag, records) in sets {
            let staged = self.volume.stage_rmap_records(ag, records)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    pub(super) fn append_pending_refcount(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const INCREASE: u32 = 1;
        const DECREASE: u32 = 2;
        let ag_blocks = u64::from(self.volume.superblock.ag_blocks);
        let mut sets = Vec::<(u32, Vec<XfsRefcountRecord>)>::new();
        for extent in extents {
            let XfsLogReplayExtent::Refcount {
                start_block,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            if !matches!(flags & 0xff, INCREASE | DECREASE) {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut left = u64::from(*block_count);
            let mut physical = *start_block;
            while left != 0 {
                let ag =
                    u32::try_from(physical / ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let start =
                    u32::try_from(physical % ag_blocks).map_err(|_| XfsError::AddressOutOfRange)?;
                let length = u32::try_from(left.min(ag_blocks - u64::from(start)))
                    .map_err(|_| XfsError::AddressOutOfRange)?;
                let index =
                    if let Some(index) = sets.iter().position(|(candidate, _)| *candidate == ag) {
                        index
                    } else {
                        let records = self.volume.refcount_records(ag)?;
                        sets.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        sets.push((ag, records));
                        sets.len() - 1
                    };
                adjust_refcount_records(
                    &mut sets[index].1,
                    start,
                    length,
                    flags & 0xff == INCREASE,
                )?;
                physical = physical
                    .checked_add(u64::from(length))
                    .ok_or(XfsError::AddressOutOfRange)?;
                left -= u64::from(length);
            }
        }
        for (ag, records) in sets {
            let staged = self.volume.stage_refcount_records(ag, records)?;
            metadata.buffers.extend(staged.buffers);
        }
        Ok(())
    }

    pub(super) fn append_pending_bmap(
        &self,
        extents: &[XfsLogReplayExtent],
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        const MAP: u32 = 1;
        const UNMAP: u32 = 2;
        const ATTR: u32 = 1 << 31;
        const UNWRITTEN: u32 = 1 << 30;
        const REALTIME: u32 = 1 << 29;
        for extent in extents {
            let XfsLogReplayExtent::Mapping {
                owner,
                start_block,
                start_offset,
                block_count,
                flags,
            } = extent
            else {
                return Err(XfsError::CorruptMetadata);
            };
            if flags & REALTIME != 0 || !matches!(flags & 0xff, MAP | UNMAP) {
                return Err(XfsError::UnsupportedFeature);
            }
            let inode = self.volume.inode(*owner)?;
            let attribute_fork = flags & ATTR != 0;
            // A nonempty shortform fork cannot be converted by merely
            // replacing its extent map: doing that would discard xattrs.
            // Admit only an empty Local fork, which is the legitimate
            // "first external mapping" BUI case; the full shortform-to-DA
            // conversion remains an explicit transaction elsewhere.
            let empty_local_attr = if attribute_fork && inode.attr_format == XfsForkFormat::Local {
                let (_, raw) = self.volume.inode_and_bytes(*owner)?;
                inode.attr_fork(&raw)?.iter().all(|byte| *byte == 0)
            } else {
                false
            };
            if (!attribute_fork && inode.data_format != XfsForkFormat::Extents)
                || (attribute_fork
                    && !(inode.attr_format == XfsForkFormat::Extents || empty_local_attr))
            {
                return Err(XfsError::UnsupportedFeature);
            }
            let mut records = if attribute_fork && empty_local_attr {
                Vec::new()
            } else if attribute_fork {
                self.volume.inode_attr_extents(*owner)?
            } else {
                self.volume.inode_data_extents(*owner)?
            };
            let record = XfsExtent {
                unwritten: flags & UNWRITTEN != 0,
                file_block: *start_offset,
                start_block: *start_block,
                block_count: *block_count,
            };
            match flags & 0xff {
                MAP => {
                    if records.iter().any(|old| {
                        ranges_overlap_u64(
                            old.file_block,
                            old.block_count,
                            record.file_block,
                            record.block_count,
                        )
                    }) {
                        return Err(XfsError::CorruptMetadata);
                    }
                    records.push(record);
                }
                UNMAP => replace_bmap_subrange(&mut records, record)?,
                _ => return Err(XfsError::UnsupportedFeature),
            }
            if attribute_fork {
                self.volume
                    .stage_attribute_fork_extents(*owner, &records, metadata)?;
            } else {
                self.volume
                    .stage_regular_inode_extents(*owner, records, inode.size, metadata)?;
            }
        }
        Ok(())
    }

    pub(super) fn stage_buffer_replay(
        &self,
        item: XfsBufferReplayItem,
        lsn: u64,
        metadata: &mut XfsMetadataTransaction,
    ) -> XfsResult<()> {
        let blocks = u64::from(item.block_count);
        let end = item
            .block_number
            .checked_add(blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        if blocks == 0 || end > self.volume.basic_blocks(&self.volume.data)? {
            return Err(XfsError::AddressOutOfRange);
        }
        let mut before = vec![
            0;
            usize::from(item.block_count)
                .checked_mul(XFS_LOG_BASIC_BLOCK)
                .ok_or(XfsError::AddressOutOfRange)?
        ];
        self.volume
            .read_basic_blocks(&self.volume.data, item.block_number, &mut before)?;
        let after =
            item.materialize_home_image(&before, lsn, self.volume.superblock.inode_size as usize)?;
        metadata.buffers.push(XfsDirtyMetadataBuffer {
            metadata_type: item.metadata_type()?,
            basic_block: item.block_number,
            before,
            after,
        });
        Ok(())
    }

    pub(super) fn stage_transaction(
        &self,
        transaction: &XfsRecoveryTransaction,
        pending: &XfsIntentRecovery,
        buffer_cancels: &mut Vec<(u64, u16, u32)>,
        suppressed_quotas: &mut u32,
    ) -> XfsResult<XfsMetadataTransaction> {
        let items = transaction.replay_items(transaction.byte_order)?;
        if items.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut normal_buffers = Vec::new();
        let mut inode_unlink_buffers = Vec::new();
        let mut cancelled_buffers = Vec::new();
        let mut non_buffers = Vec::new();
        // Buffer replay dependencies are not the log's byte-stream order.
        // AG allocator/btree images must be visible before inode items are
        // materialised, while inode-unlink buffers and cancelled buffers
        // belong after the non-buffer item classes.  Preserve source order
        // inside each class.  Cancellation is a two-pass protocol: the
        // table was built from the complete plan before replay starts, and a
        // marker consumes one reference only when its log position is met.
        for item in items {
            match item {
                XfsReplayItem::Buffer(buffer) if buffer_cancelled(buffer_cancels, &buffer)? => {
                    cancelled_buffers.push(buffer)
                }
                // XFS_BLF_INODE_BUF, not the decoded metadata type, marks
                // the special di_next_unlinked-only buffer class.  Ordinary
                // inode allocation buffers must remain in the early list.
                XfsReplayItem::Buffer(buffer) if buffer.flags & XFS_BLF_INODE_BUF != 0 => {
                    inode_unlink_buffers.push(buffer)
                }
                XfsReplayItem::Buffer(buffer) => normal_buffers.push(buffer),
                item => non_buffers.push(item),
            }
        }
        let mut metadata = XfsMetadataTransaction::default();
        for item in normal_buffers {
            self.stage_buffer_replay(item, transaction.lsn, &mut metadata)?;
        }
        for item in non_buffers {
            match item {
                XfsReplayItem::Buffer(_) => return Err(XfsError::CorruptMetadata),
                XfsReplayItem::Inode(item) => {
                    self.volume.stage_inode_log_replay(
                        &item,
                        transaction.lsn,
                        transaction.byte_order,
                        &mut metadata,
                    )?;
                }
                XfsReplayItem::Dquot(item) => {
                    let dquot = item.parse_disk_dquot(
                        self.volume.superblock.is_v5(),
                        self.volume
                            .superblock
                            .is_v5()
                            .then_some(self.volume.superblock.meta_uuid),
                        self.volume.superblock.features.incompat & XfsFeatures::INCOMPAT_BIGTIME
                            != 0,
                    )?;
                    if *suppressed_quotas & u32::from(dquot.quota_type) == 0 {
                        self.volume.stage_dquot_log_replay(
                            &item,
                            transaction.lsn,
                            &mut metadata,
                        )?;
                    }
                }
                XfsReplayItem::Intent(intent) => match intent.key.kind {
                    XfsIntentKind::ExtentFree if pending.is_pending(intent.key) => {
                        self.append_pending_efi(&intent.extents, &mut metadata)?;
                    }
                    XfsIntentKind::ExtentFree => {}
                    XfsIntentKind::Rmap if pending.is_pending(intent.key) => {
                        self.append_pending_rmap(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Refcount if pending.is_pending(intent.key) => {
                        self.append_pending_refcount(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Bmap if pending.is_pending(intent.key) => {
                        self.append_pending_bmap(&intent.extents, &mut metadata)?
                    }
                    XfsIntentKind::Rmap | XfsIntentKind::Refcount | XfsIntentKind::Bmap => {}
                },
                XfsReplayItem::Done(_) => {}
                // QUOTAOFF is not an on-disk metadata update.  Like Linux
                // recovery, it suppresses replay of later dquot items of the
                // named type; persisting a synthetic superblock change here
                // would corrupt the recorded quota state.
                XfsReplayItem::Quotaoff { flags } => *suppressed_quotas |= flags,
            }
        }
        for item in inode_unlink_buffers {
            self.stage_buffer_replay(item, transaction.lsn, &mut metadata)?;
        }
        // Cancel markers (and every buffer suppressed by their outstanding
        // reference) are deliberately consumed last and never materialised:
        // the block may already have been reused for user data.
        drop(cancelled_buffers);
        Ok(metadata)
    }

    /// Replays every committed transaction in log order.  Intent/done
    /// admission is completed for the entire plan before the first generated
    /// record is written, so a malformed later done item cannot leave an
    /// earlier transaction published as a partial recovery result.
    pub fn replay_plan(&mut self, plan: &XfsRecoveryPlan) -> XfsResult<()> {
        let mut pending = XfsIntentRecovery::default();
        for transaction in &plan.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            pending.apply_transaction(&items)?;
        }
        let mut suppressed_quotas = 0u32;
        let mut buffer_cancels = collect_buffer_cancels(&plan.committed)?;
        for transaction in &plan.committed {
            let metadata = self.stage_transaction(
                transaction,
                &pending,
                &mut buffer_cancels,
                &mut suppressed_quotas,
            )?;
            if metadata.buffers.is_empty() && metadata.dquots.is_empty() {
                // stage_transaction never produces a replayable data-only or
                // realtime-only mutation; both must be anchored by a BUF or
                // DQUOT item in the recovered transaction.
                if !metadata.data_writes.is_empty() || !metadata.realtime_writes.is_empty() {
                    return Err(XfsError::CorruptMetadata);
                }
                continue;
            }
            let transaction_id = self.generated_transaction_id()?;
            self.volume.commit_metadata_transaction(
                &mut self.ring,
                &mut self.ail,
                transaction_id,
                &metadata,
            )?;
        }
        if !buffer_cancels.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(())
    }

    pub fn finish(self) -> XfsResult<(Arc<XfsVolume>, XfsLogRing)> {
        if !self.ail.entries().is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok((self.volume, self.ring))
    }
}

pub(super) fn ranges_overlap_u32(
    start: u32,
    length: u32,
    other_start: u32,
    other_length: u32,
) -> bool {
    let end = start.checked_add(length);
    let other_end = other_start.checked_add(other_length);
    matches!((end, other_end), (Some(end), Some(other_end)) if start < other_end && other_start < end)
}

/// Realtime bitmap words on pre-rtgroup media are deliberately host-endian;
/// rtgroups changed the on-disk representation to BE32 and place a 64-byte
/// authenticated buffer header before the words.  Do not treat either layout
/// as a byte-oriented MSB-first bitmap: XFS numbers bits from the low bit of
/// each 32-bit word.
pub(super) const XFS_RTBUF_HEADER_BYTES: usize = 48;

pub(super) fn realtime_payload_offset(rtgroups: bool) -> usize {
    if rtgroups { XFS_RTBUF_HEADER_BYTES } else { 0 }
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn realtime_word(bytes: &[u8], word: usize, rtgroups: bool) -> XfsResult<u32> {
    let offset = realtime_payload_offset(rtgroups)
        .checked_add(word.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    let raw: [u8; 4] = slice(bytes, offset, 4)?
        .try_into()
        .map_err(|_| XfsError::CorruptMetadata)?;
    Ok(if rtgroups {
        u32::from_be_bytes(raw)
    } else {
        u32::from_ne_bytes(raw)
    })
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn set_realtime_word(
    bytes: &mut [u8],
    word: usize,
    value: u32,
    rtgroups: bool,
) -> XfsResult<()> {
    let offset = realtime_payload_offset(rtgroups)
        .checked_add(word.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
        .ok_or(XfsError::AddressOutOfRange)?;
    if offset.checked_add(4).ok_or(XfsError::AddressOutOfRange)? > bytes.len() {
        return Err(XfsError::AddressOutOfRange);
    }
    let encoded = if rtgroups {
        value.to_be_bytes()
    } else {
        value.to_ne_bytes()
    };
    bytes[offset..offset + 4].copy_from_slice(&encoded);
    Ok(())
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn realtime_bitmap_bit(bytes: &[u8], bit: u64, rtgroups: bool) -> XfsResult<bool> {
    let word = usize::try_from(bit / 32).map_err(|_| XfsError::AddressOutOfRange)?;
    Ok(realtime_word(bytes, word, rtgroups)? & (1u32 << (bit % 32)) != 0)
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn realtime_bitmap_range(
    bytes: &mut [u8],
    first: u64,
    count: u64,
    allocate: bool,
    rtgroups: bool,
) -> XfsResult<()> {
    let bits = bytes
        .len()
        .checked_sub(realtime_payload_offset(rtgroups))
        .ok_or(XfsError::CorruptMetadata)?
        .checked_mul(8)
        .ok_or(XfsError::AddressOutOfRange)?;
    let end = first
        .checked_add(count)
        .ok_or(XfsError::AddressOutOfRange)?;
    if count == 0 || end > bits as u64 {
        return Err(XfsError::AddressOutOfRange);
    }
    for bit in first..end {
        let word = usize::try_from(bit / 32).map_err(|_| XfsError::AddressOutOfRange)?;
        let mask = 1u32 << (bit % 32);
        let value = realtime_word(bytes, word, rtgroups)?;
        // XFS stores one for a free realtime extent and zero for an allocated
        // one.  This is the inverse of most generic bitmap helpers.
        if allocate {
            if value & mask == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            set_realtime_word(bytes, word, value & !mask, rtgroups)?;
        } else {
            if value & mask != 0 {
                return Err(XfsError::CorruptMetadata);
            }
            set_realtime_word(bytes, word, value | mask, rtgroups)?;
        }
    }
    Ok(())
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn realtime_summary_counter(
    bytes: &[u8],
    word: usize,
    rtgroups: bool,
) -> XfsResult<u32> {
    realtime_word(bytes, word, rtgroups)
}

// Realtime-device bitmap helpers kept for the in-progress RT allocator.
#[allow(dead_code)]
pub(super) fn set_realtime_summary_counter(
    bytes: &mut [u8],
    word: usize,
    value: u32,
    rtgroups: bool,
) -> XfsResult<()> {
    set_realtime_word(bytes, word, value, rtgroups)
}

pub(super) const XFS_BLF_CANCEL: u16 = 0x0001;
pub(super) const XFS_BLF_INODE_BUF: u16 = 0x0002;

/// Pass-one cancellation table.  A cancellation marker applies to every
/// earlier matching occurrence still covered by its reference count; the
/// marker itself decrements the count when pass two reaches its own position.
pub(super) fn collect_buffer_cancels(
    transactions: &[XfsRecoveryTransaction],
) -> XfsResult<Vec<(u64, u16, u32)>> {
    let mut table: Vec<(u64, u16, u32)> = Vec::new();
    for transaction in transactions {
        for item in transaction.replay_items(transaction.byte_order)? {
            let XfsReplayItem::Buffer(buffer) = item else {
                continue;
            };
            if buffer.flags & XFS_BLF_CANCEL == 0 {
                continue;
            }
            if buffer.block_count == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            if let Some((_, _, references)) = table.iter_mut().find(|(block, length, _)| {
                *block == buffer.block_number && *length == buffer.block_count
            }) {
                *references = references
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
            } else {
                table.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                table.push((buffer.block_number, buffer.block_count, 1));
            }
        }
    }
    Ok(table)
}

/// Pass-two lookup/consumption counterpart to `collect_buffer_cancels`.
/// Returns true when the caller must not replay this BUF item.
pub(super) fn buffer_cancelled(
    table: &mut Vec<(u64, u16, u32)>,
    buffer: &XfsBufferReplayItem,
) -> XfsResult<bool> {
    let Some(index) = table.iter().position(|(block, length, _)| {
        *block == buffer.block_number && *length == buffer.block_count
    }) else {
        return if buffer.flags & XFS_BLF_CANCEL != 0 {
            Err(XfsError::CorruptMetadata)
        } else {
            Ok(false)
        };
    };
    if buffer.flags & XFS_BLF_CANCEL != 0 {
        let references = &mut table[index].2;
        *references = references.checked_sub(1).ok_or(XfsError::CorruptMetadata)?;
        if *references == 0 {
            table.remove(index);
        }
    }
    Ok(true)
}

pub(super) fn ranges_overlap_u64(
    start: u64,
    length: u32,
    other_start: u64,
    other_length: u32,
) -> bool {
    let end = start.checked_add(u64::from(length));
    let other_end = other_start.checked_add(u64::from(other_length));
    matches!((end, other_end), (Some(end), Some(other_end)) if start < other_end && other_start < end)
}

/// Remove exactly one BUI range while retaining both portions of a coalesced
/// mapping.  BUI unmaps commonly target only the middle of an old extent, so
/// equality with the complete on-disk record is neither required nor safe.
pub(super) fn replace_bmap_subrange(
    records: &mut Vec<XfsExtent>,
    target: XfsExtent,
) -> XfsResult<()> {
    let target_end = target
        .file_block
        .checked_add(u64::from(target.block_count))
        .ok_or(XfsError::AddressOutOfRange)?;
    let index = records
        .iter()
        .position(|old| {
            let Some(end) = old.file_block.checked_add(u64::from(old.block_count)) else {
                return false;
            };
            let offset = target.file_block.checked_sub(old.file_block);
            old.unwritten == target.unwritten
                && target.file_block >= old.file_block
                && target_end <= end
                && offset.and_then(|delta| old.start_block.checked_add(delta))
                    == Some(target.start_block)
        })
        .ok_or(XfsError::CorruptMetadata)?;
    let old = records.remove(index);
    let left = target.file_block - old.file_block;
    let right = old
        .file_block
        .checked_add(u64::from(old.block_count))
        .ok_or(XfsError::CorruptMetadata)?
        - target_end;
    if right != 0 {
        records.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        records.push(XfsExtent {
            unwritten: old.unwritten,
            file_block: target_end,
            start_block: target
                .start_block
                .checked_add(u64::from(target.block_count))
                .ok_or(XfsError::AddressOutOfRange)?,
            block_count: u32::try_from(right).map_err(|_| XfsError::AddressOutOfRange)?,
        });
    }
    if left != 0 {
        records.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        records.push(XfsExtent {
            unwritten: old.unwritten,
            file_block: old.file_block,
            start_block: old.start_block,
            block_count: u32::try_from(left).map_err(|_| XfsError::AddressOutOfRange)?,
        });
    }
    records.sort_unstable_by_key(|extent| extent.file_block);
    Ok(())
}

/// Removes (or replaces) one physical subrange of a single rmap owner.
/// XFS coalesces adjacent rmap records, so recovery must be able to split a
/// larger home record when a RUI describes only its middle.  The logical
/// portion of `rm_offset` advances in lockstep with the physical block; the
/// fork/state bits remain unchanged on both retained fragments.
pub(super) fn replace_rmap_subrange(
    records: &mut Vec<XfsRmapRecord>,
    target: XfsRmapRecord,
    replacement: Option<XfsRmapRecord>,
    offset_flags: u64,
) -> XfsResult<()> {
    let target_end = target
        .start_block
        .checked_add(target.block_count)
        .ok_or(XfsError::AddressOutOfRange)?;
    let target_logical = target.offset & !offset_flags;
    let target_flags = target.offset & offset_flags;
    let at = records
        .iter()
        .position(|old| {
            let Some(old_end) = old.start_block.checked_add(old.block_count) else {
                return false;
            };
            if old.owner != target.owner
                || old.offset & offset_flags != target_flags
                || old.start_block > target.start_block
                || old_end < target_end
            {
                return false;
            }
            let delta = u64::from(target.start_block - old.start_block);
            (old.offset & !offset_flags).checked_add(delta) == Some(target_logical)
        })
        .ok_or(XfsError::CorruptMetadata)?;
    let old = records.remove(at);
    let old_end = old
        .start_block
        .checked_add(old.block_count)
        .ok_or(XfsError::CorruptMetadata)?;
    if old.start_block < target.start_block {
        records.push(XfsRmapRecord {
            start_block: old.start_block,
            block_count: target.start_block - old.start_block,
            owner: old.owner,
            offset: old.offset,
        });
    }
    if let Some(replacement) = replacement {
        if replacement.start_block != target.start_block
            || replacement.block_count != target.block_count
            || replacement.owner != target.owner
            || replacement.offset & !offset_flags != target_logical
        {
            return Err(XfsError::CorruptMetadata);
        }
        records.push(replacement);
    }
    if target_end < old_end {
        let delta = u64::from(target_end - old.start_block);
        let logical = (old.offset & !offset_flags)
            .checked_add(delta)
            .ok_or(XfsError::AddressOutOfRange)?;
        records.push(XfsRmapRecord {
            start_block: target_end,
            block_count: old_end - target_end,
            owner: old.owner,
            offset: (old.offset & offset_flags) | logical,
        });
    }
    records.sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
    Ok(())
}

pub(super) fn adjust_refcount_records(
    records: &mut Vec<XfsRefcountRecord>,
    start: u32,
    length: u32,
    increase: bool,
) -> XfsResult<()> {
    let end = start
        .checked_add(length)
        .ok_or(XfsError::AddressOutOfRange)?;
    let mut out = Vec::new();
    let mut cursor = start;
    for record in records.iter().copied() {
        let record_end = record
            .start_block
            .checked_add(record.block_count)
            .ok_or(XfsError::CorruptMetadata)?;
        if record_end <= start || record.start_block >= end {
            out.push(record);
            continue;
        }
        if record.start_block < start {
            out.push(XfsRefcountRecord {
                start_block: record.start_block,
                block_count: start - record.start_block,
                refcount: record.refcount,
            });
        }
        let overlap_start = record.start_block.max(start);
        let overlap_end = record_end.min(end);
        if overlap_start > cursor {
            if !increase {
                return Err(XfsError::CorruptMetadata);
            }
            out.push(XfsRefcountRecord {
                start_block: cursor,
                block_count: overlap_start - cursor,
                refcount: 2,
            });
        }
        if increase {
            out.push(XfsRefcountRecord {
                start_block: overlap_start,
                block_count: overlap_end - overlap_start,
                refcount: record
                    .refcount
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
            });
        } else if record.refcount > 2 {
            out.push(XfsRefcountRecord {
                start_block: overlap_start,
                block_count: overlap_end - overlap_start,
                refcount: record.refcount - 1,
            });
        }
        if record_end > end {
            out.push(XfsRefcountRecord {
                start_block: end,
                block_count: record_end - end,
                refcount: record.refcount,
            });
        }
        cursor = cursor.max(overlap_end);
    }
    if cursor < end {
        if !increase {
            return Err(XfsError::CorruptMetadata);
        }
        out.push(XfsRefcountRecord {
            start_block: cursor,
            block_count: end - cursor,
            refcount: 2,
        });
    }
    out.sort_unstable_by_key(|record| record.start_block);
    let mut merged: Vec<XfsRefcountRecord> = Vec::new();
    for record in out {
        if record.block_count == 0 || record.refcount < 2 {
            return Err(XfsError::CorruptMetadata);
        }
        if let Some(last) = merged.last_mut()
            && last.refcount == record.refcount
            && last.start_block.checked_add(last.block_count) == Some(record.start_block)
        {
            last.block_count = last
                .block_count
                .checked_add(record.block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
        } else {
            merged.push(record);
        }
    }
    *records = merged;
    Ok(())
}

// Recovery-session drivers for the in-progress journal recovery path.
#[allow(dead_code)]
impl XfsIntentRecoverySession {
    pub fn state(&self) -> (usize, usize) {
        (self.next, self.steps.len())
    }

    pub fn apply_next(&mut self) -> XfsResult<bool> {
        let Some(step) = self.steps.get(self.next) else {
            return Ok(false);
        };
        if self.prepared.is_none() {
            self.prepared = self.volume.prepare_pending_extent_free_recovery(step)?;
        }
        if let Some(commit) = &self.prepared {
            self.volume.apply_recovery_commit(commit)?;
        }
        self.prepared = None;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(true)
    }

    pub fn apply_all(&mut self) -> XfsResult<()> {
        while self.apply_next()? {}
        Ok(())
    }

    pub fn finish(self) -> XfsResult<Arc<XfsVolume>> {
        if self.next != self.steps.len() || self.prepared.is_some() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(self.volume)
    }
}

// Recovery-session drivers for the in-progress journal recovery path.
#[allow(dead_code)]
impl XfsRecoverySession {
    pub fn state(&self) -> (usize, usize) {
        (self.next, self.commits.len())
    }

    pub fn apply_next(&mut self) -> XfsResult<bool> {
        let Some(commit) = self.commits.get(self.next) else {
            return Ok(false);
        };
        self.volume.apply_recovery_commit(commit)?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok(true)
    }

    pub fn apply_all(&mut self) -> XfsResult<()> {
        while self.apply_next()? {}
        Ok(())
    }

    /// Returns the verified volume only after every prepared transaction has
    /// reached durable home blocks.  The caller still must handle intent,
    /// inode and dquot item classes before publishing a general XFS mount.
    pub fn finish(self) -> XfsResult<Arc<XfsVolume>> {
        if self.next != self.commits.len() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(self.volume)
    }
}

impl XfsJournalRecoveryState {
    pub const fn requires_replay(self) -> bool {
        self.committed_transactions != 0
    }
}

/// A verified record framing boundary, including all its operation headers.
/// Semantic item decoding is intentionally a second phase: item data is only
/// applied after every operation in the transaction has been collected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsJournalRecord {
    pub header: XfsLogRecordHeader,
    pub operations: Vec<XfsLogOperation>,
}

/// A completely framed transaction ready for item-specific replay.  Only a
/// transaction that has both a start and commit operation becomes visible in
/// this list; interrupted log tails remain uncommitted and are discarded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRecoveryTransaction {
    pub transaction_id: u32,
    /// LSN of the record carrying the latest region of this committed
    /// transaction.  This is the LSN installed into every replayed home
    /// buffer, and is therefore part of the replay identity rather than a
    /// diagnostic timestamp.
    pub lsn: u64,
    pub byte_order: XfsLogByteOrder,
    pub operations: Vec<XfsLogOperation>,
    pub(super) pending_operation: Option<XfsPendingLogOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct XfsPendingLogOperation {
    pub(super) client_id: u8,
    pub(super) flags: u8,
    pub(super) payload: Vec<u8>,
}

impl XfsRecoveryTransaction {
    /// Adds one physical operation region, joining a record-boundary fragment
    /// only when the native continuation flags prove that its predecessor and
    /// successor belong together.  A missing/mismatched fragment is log
    /// corruption; treating it as an independent item would replay a prefix
    /// of an inode, btree, or buffer update.
    pub(super) fn push_region(&mut self, operation: XfsLogOperation) -> XfsResult<()> {
        const MARKERS: u8 = XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS;
        let was_continued = operation.flags & XLOG_WAS_CONT_TRANS != 0;
        let continues = operation.flags & XLOG_CONTINUE_TRANS != 0;
        let ends = operation.flags & XLOG_END_TRANS != 0;
        if ends && continues {
            return Err(XfsError::CorruptMetadata);
        }
        if was_continued {
            let pending = self
                .pending_operation
                .as_mut()
                .ok_or(XfsError::CorruptMetadata)?;
            if pending.client_id != operation.client_id {
                return Err(XfsError::CorruptMetadata);
            }
            pending
                .payload
                .try_reserve(operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            pending.payload.extend_from_slice(&operation.payload);
            pending.flags |= operation.flags & !MARKERS;
            if continues {
                return Ok(());
            }
            if !ends {
                return Err(XfsError::CorruptMetadata);
            }
            let pending = self
                .pending_operation
                .take()
                .ok_or(XfsError::CorruptMetadata)?;
            self.operations
                .try_reserve(1)
                .map_err(|_| XfsError::NoMemory)?;
            self.operations.push(XfsLogOperation {
                transaction_id: self.transaction_id,
                client_id: pending.client_id,
                flags: pending.flags,
                payload: pending.payload,
            });
            return Ok(());
        }
        if self.pending_operation.is_some() {
            return Err(XfsError::CorruptMetadata);
        }
        if continues {
            let mut payload = Vec::new();
            payload
                .try_reserve(operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            payload.extend_from_slice(&operation.payload);
            self.pending_operation = Some(XfsPendingLogOperation {
                client_id: operation.client_id,
                flags: operation.flags & !MARKERS,
                payload,
            });
            return Ok(());
        }
        if ends {
            return Err(XfsError::CorruptMetadata);
        }
        self.operations
            .try_reserve(1)
            .map_err(|_| XfsError::NoMemory)?;
        self.operations.push(XfsLogOperation {
            transaction_id: operation.transaction_id,
            client_id: operation.client_id,
            flags: operation.flags & !MARKERS,
            payload: operation.payload,
        });
        Ok(())
    }

    pub(super) fn complete(&self) -> XfsResult<()> {
        if self.pending_operation.is_some() {
            Err(XfsError::CorruptMetadata)
        } else {
            Ok(())
        }
    }
    /// Finds and validates the unique transaction-header region before any
    /// item decoder consumes native-endian payload bytes.  This prevents a
    /// continuation fragment from being mistaken for a standalone replay
    /// transaction.
    pub fn header(&self, order: XfsLogByteOrder) -> XfsResult<XfsTransactionHeader> {
        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        let mut header = None;
        for operation in &self.operations {
            if operation.payload.len() != 16 {
                continue;
            }
            let magic = native_u32(&operation.payload, 0, order)?;
            if magic != 0x5452_414e {
                continue;
            }
            if header.is_some() {
                return Err(XfsError::CorruptMetadata);
            }
            header = Some(XfsTransactionHeader {
                transaction_type: native_u32(&operation.payload, 4, order)?,
                item_count: native_u32(&operation.payload, 12, order)?,
            });
        }
        header.ok_or(XfsError::CorruptMetadata)
    }

    /// Returns log regions after the validated transaction header, preserving
    /// journal order for the item decoder.  The decoder owns format/data
    /// pairing; this method never coalesces adjacent regions heuristically.
    pub fn item_regions(&self, order: XfsLogByteOrder) -> XfsResult<Vec<&XfsLogOperation>> {
        let _ = self.header(order)?;
        let mut regions = Vec::new();
        let mut skipped = false;
        for operation in &self.operations {
            if !skipped
                && operation.payload.len() == 16
                && native_u32(&operation.payload, 0, order).ok() == Some(0x5452_414e)
            {
                skipped = true;
                continue;
            }
            regions.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            regions.push(operation);
        }
        Ok(regions)
    }

    /// Decodes one `xfs_buf_log_format` region and its following BCHUNK
    /// regions. Every dirty 128-byte bitmap bit consumes exactly one complete
    /// region; cancellation buffers intentionally produce no home write.
    pub fn buffer_item(
        &self,
        order: XfsLogByteOrder,
        format: &[u8],
        chunk_regions: &[&[u8]],
    ) -> XfsResult<XfsBufferReplayItem> {
        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        if format.len() < 20 || native_u16(format, 0, order)? != 0x123c {
            return Err(XfsError::CorruptMetadata);
        }
        let flags = native_u16(format, 4, order)?;
        let block_count = native_u16(format, 6, order)?;
        let block_number = native_u64(format, 8, order)?;
        let words = native_u32(format, 16, order)? as usize;
        let map_bytes = words.checked_mul(4).ok_or(XfsError::CorruptMetadata)?;
        if format.len()
            != 20usize
                .checked_add(map_bytes)
                .ok_or(XfsError::CorruptMetadata)?
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut dirty_chunks = Vec::new();
        for word in 0..words {
            let bits = native_u32(format, 20 + word * 4, order)?;
            for bit in 0..32 {
                if bits & (1 << bit) != 0 {
                    dirty_chunks
                        .try_reserve(1)
                        .map_err(|_| XfsError::NoMemory)?;
                    dirty_chunks.push((word * 32 + bit) as u32);
                }
            }
        }
        if dirty_chunks.len() != chunk_regions.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(chunk_regions.len())
            .map_err(|_| XfsError::NoMemory)?;
        for chunk in chunk_regions {
            if chunk.len() != 128 {
                return Err(XfsError::CorruptMetadata);
            }
            chunks.push(chunk.to_vec());
        }
        Ok(XfsBufferReplayItem {
            flags,
            block_number,
            block_count,
            dirty_chunks,
            chunks,
        })
    }

    /// Decodes every native XFS item in this committed transaction.  Item
    /// format IDs and `*_size` fields are authoritative: no item is inferred
    /// from a payload length, and a malformed item can never consume regions
    /// belonging to its successor.
    pub fn replay_items(&self, order: XfsLogByteOrder) -> XfsResult<Vec<XfsReplayItem>> {
        const EFI: u16 = 0x1236;
        const EFD: u16 = 0x1237;
        const INODE: u16 = 0x123b;
        const BUF: u16 = 0x123c;
        const DQUOT: u16 = 0x123d;
        const QUOTAOFF: u16 = 0x123e;
        const RUI: u16 = 0x1240;
        const RUD: u16 = 0x1241;
        const CUI: u16 = 0x1242;
        const CUD: u16 = 0x1243;
        const BUI: u16 = 0x1244;
        const BUD: u16 = 0x1245;

        if order != self.byte_order {
            return Err(XfsError::CorruptMetadata);
        }
        let expected_items = usize::try_from(self.header(order)?.item_count)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        if expected_items == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let regions = self.item_regions(order)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(expected_items)
            .map_err(|_| XfsError::NoMemory)?;
        let mut index = 0usize;
        while index < regions.len() {
            let format = regions[index].payload.as_slice();
            if format.len() < 4 {
                return Err(XfsError::CorruptMetadata);
            }
            let kind = native_u16(format, 0, order)?;
            let count = usize::from(native_u16(format, 2, order)?);
            if count == 0 {
                return Err(XfsError::CorruptMetadata);
            }
            let end = index
                .checked_add(count)
                .ok_or(XfsError::AddressOutOfRange)?;
            if end > regions.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let item = match kind {
                BUF => {
                    if format.len() < 20 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let words = usize::try_from(native_u32(format, 16, order)?)
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                    let map_end = 20usize
                        .checked_add(words.checked_mul(4).ok_or(XfsError::AddressOutOfRange)?)
                        .ok_or(XfsError::AddressOutOfRange)?;
                    if map_end != format.len() {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut chunks = 0usize;
                    for word in 0..words {
                        chunks = chunks
                            .checked_add(
                                native_u32(format, 20 + word * 4, order)?.count_ones() as usize
                            )
                            .ok_or(XfsError::AddressOutOfRange)?;
                    }
                    if count != chunks.checked_add(1).ok_or(XfsError::AddressOutOfRange)? {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut chunk_regions = Vec::new();
                    chunk_regions
                        .try_reserve_exact(chunks)
                        .map_err(|_| XfsError::NoMemory)?;
                    for region in &regions[index + 1..end] {
                        if region.payload.len() != 128 {
                            return Err(XfsError::CorruptMetadata);
                        }
                        chunk_regions.push(region.payload.as_slice());
                    }
                    XfsReplayItem::Buffer(self.buffer_item(order, format, &chunk_regions)?)
                }
                INODE => {
                    // Native x86_64 xfs_inode_log_format is 40 bytes:
                    // type/size/fields/asize/dsize/ino/blkno/len/boffset.
                    // The two size fields describe subsequent logged core
                    // and fork regions, so they must not be mistaken for a
                    // padding word.
                    if format.len() != 40 || count < 2 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let inode = native_u64(format, 16, order)?;
                    let block_number = native_u64(format, 24, order)?;
                    let block_count = native_u32(format, 32, order)?;
                    let byte_offset = native_u32(format, 36, order)?;
                    if inode == 0
                        || block_number >> 63 != 0
                        || block_count == 0
                        || byte_offset >> 31 != 0
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut payloads = Vec::new();
                    payloads
                        .try_reserve_exact(count - 1)
                        .map_err(|_| XfsError::NoMemory)?;
                    for region in &regions[index + 1..end] {
                        if region.payload.is_empty() {
                            return Err(XfsError::CorruptMetadata);
                        }
                        payloads.push(region.payload.clone());
                    }
                    XfsReplayItem::Inode(XfsInodeReplayItem {
                        inode,
                        block_number,
                        block_count,
                        byte_offset,
                        fields: native_u32(format, 4, order)?,
                        attr_size: native_u16(format, 8, order)?,
                        data_size: native_u16(format, 10, order)?,
                        regions: payloads,
                    })
                }
                DQUOT => {
                    // xfs_dq_logformat is followed by exactly one
                    // xfs_disk_dquot region.
                    if format.len() != 24 || count != 2 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let id = native_u32(format, 4, order)?;
                    let block_number = native_u64(format, 8, order)?;
                    let block_count = native_u32(format, 16, order)?;
                    let byte_offset = native_u32(format, 20, order)?;
                    let disk_dquot = regions[index + 1].payload.clone();
                    if block_number >> 63 != 0
                        || block_count == 0
                        || byte_offset >> 31 != 0
                        || disk_dquot.len() != 104
                        || usize::try_from(byte_offset)
                            .ok()
                            .and_then(|offset| offset.checked_add(136))
                            .is_none_or(|end| {
                                end > usize::try_from(block_count)
                                    .unwrap_or(0)
                                    .saturating_mul(512)
                            })
                    {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Dquot(XfsDquotReplayItem {
                        id,
                        block_number,
                        block_count,
                        byte_offset,
                        disk_dquot,
                    })
                }
                QUOTAOFF => {
                    if format.len() != 8 || count != 1 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Quotaoff {
                        flags: native_u32(format, 4, order)?,
                    }
                }
                EFI | EFD | RUI | CUI | BUI => {
                    if count != 1 || format.len() < 16 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let extent_count = usize::try_from(native_u32(format, 4, order)?)
                        .map_err(|_| XfsError::AddressOutOfRange)?;
                    let id = native_u64(format, 8, order)?;
                    if id == 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let (intent_kind, extent_size) = match kind {
                        EFI | EFD => (XfsIntentKind::ExtentFree, 16usize),
                        RUI => (XfsIntentKind::Rmap, 32usize),
                        CUI => (XfsIntentKind::Refcount, 16usize),
                        BUI => (XfsIntentKind::Bmap, 32usize),
                        _ => return Err(XfsError::CorruptMetadata),
                    };
                    let bytes = 16usize
                        .checked_add(
                            extent_count
                                .checked_mul(extent_size)
                                .ok_or(XfsError::AddressOutOfRange)?,
                        )
                        .ok_or(XfsError::AddressOutOfRange)?;
                    if format.len() != bytes {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let mut extents = Vec::new();
                    extents
                        .try_reserve_exact(extent_count)
                        .map_err(|_| XfsError::NoMemory)?;
                    for number in 0..extent_count {
                        let offset = 16usize
                            .checked_add(
                                number
                                    .checked_mul(extent_size)
                                    .ok_or(XfsError::AddressOutOfRange)?,
                            )
                            .ok_or(XfsError::AddressOutOfRange)?;
                        let extent = match intent_kind {
                            XfsIntentKind::ExtentFree => {
                                let start_block = native_u64(format, offset, order)?;
                                let block_count = native_u32(format, offset + 8, order)?;
                                if start_block >> 63 != 0
                                    || block_count == 0
                                    || native_u32(format, offset + 12, order)? != 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::ExtentFree {
                                    start_block,
                                    block_count,
                                }
                            }
                            XfsIntentKind::Refcount => {
                                let start_block = native_u64(format, offset, order)?;
                                let block_count = native_u32(format, offset + 8, order)?;
                                let flags = native_u32(format, offset + 12, order)?;
                                if start_block >> 63 != 0
                                    || block_count == 0
                                    || flags & !0xff != 0
                                    || flags & 0xff == 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::Refcount {
                                    start_block,
                                    block_count,
                                    flags,
                                }
                            }
                            XfsIntentKind::Rmap | XfsIntentKind::Bmap => {
                                let owner = native_u64(format, offset, order)?;
                                let start_block = native_u64(format, offset + 8, order)?;
                                let start_offset = native_u64(format, offset + 16, order)?;
                                let block_count = native_u32(format, offset + 24, order)?;
                                let flags = native_u32(format, offset + 28, order)?;
                                let allowed = if intent_kind == XfsIntentKind::Rmap {
                                    0xe000_00ff
                                } else {
                                    0xe000_00ff
                                };
                                if owner == 0
                                    || start_block >> 63 != 0
                                    || block_count == 0
                                    || flags & !allowed != 0
                                    || flags & 0xff == 0
                                {
                                    return Err(XfsError::CorruptMetadata);
                                }
                                XfsLogReplayExtent::Mapping {
                                    owner,
                                    start_block,
                                    start_offset,
                                    block_count,
                                    flags,
                                }
                            }
                        };
                        extents.push(extent);
                    }
                    let key = XfsIntentKey {
                        kind: intent_kind,
                        id,
                    };
                    if kind == EFD {
                        XfsReplayItem::Done(XfsDoneReplayItem { key, extents })
                    } else {
                        XfsReplayItem::Intent(XfsIntentReplayItem { key, extents })
                    }
                }
                RUD | CUD | BUD => {
                    if format.len() != 16 || count != 1 || native_u32(format, 4, order)? != 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    let intent_kind = match kind {
                        RUD => XfsIntentKind::Rmap,
                        CUD => XfsIntentKind::Refcount,
                        BUD => XfsIntentKind::Bmap,
                        _ => return Err(XfsError::CorruptMetadata),
                    };
                    let id = native_u64(format, 8, order)?;
                    if id == 0 {
                        return Err(XfsError::CorruptMetadata);
                    }
                    XfsReplayItem::Done(XfsDoneReplayItem {
                        key: XfsIntentKey {
                            kind: intent_kind,
                            id,
                        },
                        extents: Vec::new(),
                    })
                }
                _ => return Err(XfsError::UnsupportedFeature),
            };
            result.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            result.push(item);
            index = end;
        }
        if result.len() != expected_items {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(result)
    }

    /// Decodes the buffer-log items in a transaction without guessing at
    /// region pairing.  Each `xfs_buf_log_format` owns exactly the following
    /// number of 128-byte BCHUNK regions indicated by its bitmap; any other
    /// item kind is rejected so a mixed transaction never receives a partial
    /// home-write commit.
    pub fn buffer_items(&self, order: XfsLogByteOrder) -> XfsResult<Vec<XfsBufferReplayItem>> {
        let regions = self.item_regions(order)?;
        let mut result = Vec::new();
        let mut index = 0usize;
        while index < regions.len() {
            let format = regions[index].payload.as_slice();
            if format.len() < 20 || native_u16(format, 0, order)? != 0x123c {
                return Err(XfsError::UnsupportedFeature);
            }
            let words = native_u32(format, 16, order)? as usize;
            let map_end = 20usize
                .checked_add(words.checked_mul(4).ok_or(XfsError::CorruptMetadata)?)
                .ok_or(XfsError::CorruptMetadata)?;
            if map_end != format.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut chunks = 0usize;
            for word in 0..words {
                chunks = chunks
                    .checked_add(native_u32(format, 20 + word * 4, order)?.count_ones() as usize)
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
            let start = index.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
            let end = start
                .checked_add(chunks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if end > regions.len() {
                return Err(XfsError::CorruptMetadata);
            }
            let mut chunk_regions = Vec::new();
            chunk_regions
                .try_reserve_exact(chunks)
                .map_err(|_| XfsError::NoMemory)?;
            for region in &regions[start..end] {
                if region.payload.len() != 128 {
                    return Err(XfsError::CorruptMetadata);
                }
                chunk_regions.push(region.payload.as_slice());
            }
            result.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
            result.push(self.buffer_item(order, format, &chunk_regions)?);
            index = end;
        }
        Ok(result)
    }
}

/// Recovery framing state.  It deliberately owns no block writer: journal
/// decode and metadata application are separate failure domains, so a decode
/// error cannot leave half of a transaction written to the allocation groups.
#[derive(Default)]
pub struct XfsRecoveryPlan {
    pub(super) open: Vec<XfsRecoveryTransaction>,
    pub(super) committed: Vec<XfsRecoveryTransaction>,
    pub(super) head_lsn: u64,
    pub(super) tail_lsn: u64,
}

#[derive(Clone, Default)]
pub struct XfsIntentRecovery {
    pub(super) pending: Vec<XfsIntentKey>,
    // Keep the authenticated operation with its key.  The key alone is
    // sufficient for done pairing, but not for replaying an EFI after the
    // complete log chain establishes that no EFD completed it.
    pub(super) pending_items: Vec<XfsIntentReplayItem>,
    /// Completed keys are retained for the duration of a recovery pass.
    /// Recovery can be restarted after an I/O error, so an already applied
    /// done item must be a no-op rather than turning a clean retry into media
    /// corruption.
    pub(super) completed: Vec<XfsIntentKey>,
}

impl XfsIntentRecovery {
    /// Registers one decoded intent/done item in journal order.  This
    /// registry deliberately accepts no buffer, inode, or dquot item: a
    /// caller that mixes an unimplemented item class into an intent replay
    /// transaction must fail before publishing a partial recovery result.
    ///
    /// The key includes the item class, so (for example) an EFD cannot
    /// complete a same-numbered RUI.  Repeating an already seen intent or
    /// done is harmless; this is needed when an interrupted recovery pass
    /// restarts from a durable checkpoint and presents that log region again.
    pub fn apply(&mut self, item: XfsReplayItem) -> XfsResult<()> {
        match item {
            XfsReplayItem::Intent(intent) => {
                let key = intent.key;
                // Exact duplicate intents occur when recovery repeats a
                // committed log region after a crash.  Neither duplicate may
                // allocate/free an extent a second time.
                if let Some(existing) = self
                    .pending_items
                    .iter()
                    .find(|existing| existing.key == key)
                {
                    // A replay retry may present the same intent again, but
                    // the identifier cannot be used to smuggle in a second
                    // extent vector.
                    if existing != &intent {
                        return Err(XfsError::CorruptMetadata);
                    }
                    return Ok(());
                }
                if self.completed.iter().any(|existing| *existing == key) {
                    return Ok(());
                }
                self.pending
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.pending_items
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.pending.push(key);
                self.pending_items.push(intent);
            }
            XfsReplayItem::Done(done) => {
                let key = done.key;
                if self.completed.iter().any(|existing| *existing == key) {
                    return Ok(());
                }
                // The retained log window may begin after the matching
                // intent but still contain its done item.  Such a done only
                // proves that there is no pending in-window operation; it is
                // not media corruption and must be a no-op.
                let Some(index) = self.pending.iter().position(|existing| *existing == key) else {
                    return Ok(());
                };
                self.pending.remove(index);
                let item_index = self
                    .pending_items
                    .iter()
                    .position(|existing| existing.key == key)
                    .ok_or(XfsError::CorruptMetadata)?;
                self.pending_items.remove(item_index);
                self.completed
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.completed.push(key);
            }
            XfsReplayItem::Buffer(_)
            | XfsReplayItem::Inode(_)
            | XfsReplayItem::Dquot(_)
            | XfsReplayItem::Quotaoff { .. } => {
                return Err(XfsError::UnsupportedFeature);
            }
        }
        Ok(())
    }

    /// Applies every decoded item of one committed transaction atomically to
    /// the intent registry.  An out-of-order done item, mismatched item kind,
    /// or unsupported mixed item leaves the registry unchanged.  This is the
    /// recovery boundary used before a caller may act on the returned pending
    /// intents.  Ordinary replay items are admitted here but deliberately
    /// left for the transaction materializer; their presence must not make a
    /// valid mixed transaction look corrupt.
    pub fn apply_transaction(&mut self, items: &[XfsReplayItem]) -> XfsResult<()> {
        if items.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let mut next = self.clone();
        for item in items {
            match item {
                XfsReplayItem::Intent(_) | XfsReplayItem::Done(_) => next.apply(item.clone())?,
                XfsReplayItem::Buffer(_)
                | XfsReplayItem::Inode(_)
                | XfsReplayItem::Dquot(_)
                | XfsReplayItem::Quotaoff { .. } => {}
            }
        }
        *self = next;
        Ok(())
    }

    /// Returns whether this exact intent class and identifier still requires
    /// replay.  Callers must not treat an absent key as permission to apply a
    /// decoded operation: it may instead have been completed or never have
    /// appeared in the authenticated log chain.
    pub fn is_pending(&self, key: XfsIntentKey) -> bool {
        self.pending.iter().any(|existing| *existing == key)
    }

    pub fn pending(&self) -> &[XfsIntentKey] {
        &self.pending
    }
    /// Authenticated pending operations in their original intent order.
    /// Callers must still select a semantic engine by kind; this registry
    /// deliberately never treats a decoded rmap/refcount/bmap payload as an
    /// allocator operation.
    pub fn pending_items(&self) -> &[XfsIntentReplayItem] {
        &self.pending_items
    }
    pub fn completed(&self) -> &[XfsIntentKey] {
        &self.completed
    }
}

impl XfsLogRecordHeader {
    /// A physical XFS log record always reserves one basic block for its
    /// header.  The typed fields finish at byte 328; the remaining bytes are
    /// part of the on-disk header, not the first log operation.
    pub const ENCODED_LEN: usize = 512;
    pub(super) const CRC_OFFSET: usize = 32;

    pub(super) fn header_bytes_for(iclog_bytes: u32) -> XfsResult<usize> {
        if iclog_bytes == 0 {
            return Ok(Self::ENCODED_LEN);
        }
        let windows = (usize::try_from(iclog_bytes)
            .map_err(|_| XfsError::AddressOutOfRange)?
            .checked_add(32 * 1024 - 1)
            .ok_or(XfsError::AddressOutOfRange)?)
            / (32 * 1024);
        windows
            .checked_mul(XFS_LOG_BASIC_BLOCK)
            .ok_or(XfsError::AddressOutOfRange)
    }

    pub(super) fn header_bytes(&self) -> XfsResult<usize> {
        Self::header_bytes_for(self.iclog_bytes)
    }

    pub(super) fn minimal_iclog_bytes(operations: &[XfsLogOperation]) -> XfsResult<u32> {
        let mut payload = 0usize;
        for operation in operations {
            payload = payload
                .checked_add(12)
                .and_then(|value| value.checked_add(operation.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        payload = align8(payload).ok_or(XfsError::AddressOutOfRange)?;
        let mut iclog = 32 * 1024usize;
        while Self::header_bytes_for(
            u32::try_from(iclog).map_err(|_| XfsError::AddressOutOfRange)?,
        )?
        .checked_add(payload)
        .ok_or(XfsError::AddressOutOfRange)?
            > iclog
        {
            iclog = iclog
                .checked_add(32 * 1024)
                .ok_or(XfsError::AddressOutOfRange)?;
            if iclog > 256 * 1024 {
                return Err(XfsError::AddressOutOfRange);
            }
        }
        u32::try_from(iclog).map_err(|_| XfsError::AddressOutOfRange)
    }

    /// Constructs a Linux-x86 log record header for a complete set of
    /// physical operation regions.  The operation wire remains explicitly
    /// supplied; this constructor merely derives the aligned `h_len` and
    /// binds it to the ring reservation's LSN/cycle.
    pub fn for_operations(
        cycle: u32,
        lsn: u64,
        tail_lsn: u64,
        previous_block: u32,
        filesystem_uuid: XfsUuid,
        iclog_bytes: u32,
        operations: &[XfsLogOperation],
    ) -> XfsResult<Self> {
        let mut bytes = 0usize;
        for operation in operations {
            bytes = bytes
                .checked_add(12)
                .and_then(|value| value.checked_add(operation.payload.len()))
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        let payload_bytes = align8(bytes).ok_or(XfsError::AddressOutOfRange)?;
        if Self::header_bytes_for(iclog_bytes)?
            .checked_add(payload_bytes)
            .ok_or(XfsError::AddressOutOfRange)?
            > iclog_bytes as usize
        {
            return Err(XfsError::AddressOutOfRange);
        }
        Ok(Self {
            cycle,
            version: 2,
            payload_bytes: u32::try_from(payload_bytes).map_err(|_| XfsError::AddressOutOfRange)?,
            lsn,
            tail_lsn,
            previous_block,
            operation_count: u32::try_from(operations.len())
                .map_err(|_| XfsError::AddressOutOfRange)?,
            cycle_data: [0; 64],
            format: 1,
            filesystem_uuid,
            iclog_bytes,
        })
    }

    pub(super) fn parse(
        bytes: &[u8],
        expected_uuid: XfsUuid,
        require_crc: bool,
    ) -> XfsResult<Self> {
        if bytes.len() < Self::ENCODED_LEN || be32(bytes, 0)? != XFS_LOG_RECORD_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        let payload_bytes = be32(bytes, 12)?;
        if payload_bytes & 7 != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let mut filesystem_uuid = [0; 16];
        filesystem_uuid.copy_from_slice(slice(bytes, 304, 16)?);
        let filesystem_uuid = XfsUuid(filesystem_uuid);
        if filesystem_uuid != expected_uuid {
            return Err(XfsError::CorruptMetadata);
        }
        let mut cycle_data = [0; 64];
        for (index, value) in cycle_data.iter_mut().enumerate() {
            *value = be32(bytes, 44 + index * 4)?;
        }
        let header = Self {
            cycle: be32(bytes, 4)?,
            version: be32(bytes, 8)?,
            payload_bytes,
            lsn: be64(bytes, 16)?,
            tail_lsn: be64(bytes, 24)?,
            previous_block: be32(bytes, 36)?,
            operation_count: be32(bytes, 40)?,
            cycle_data,
            format: be32(bytes, 300)?,
            filesystem_uuid,
            iclog_bytes: be32(bytes, 320)?,
        };
        if header.cycle == 0 || header.lsn == 0 || (header.lsn >> 32) as u32 != header.cycle {
            return Err(XfsError::CorruptMetadata);
        }
        let logical_end = header
            .header_bytes()?
            .checked_add(header.payload_bytes as usize)
            .ok_or(XfsError::CorruptMetadata)?;
        let record_end = align_log_basic_block(logical_end)?;
        if require_crc {
            verify_log_record_crc(slice(bytes, 0, record_end)?, &header)?;
        }
        Ok(header)
    }

    /// Encodes a single complete physical record image.  This is deliberately
    /// only the record wire codec: log-ring placement, cycle replacement in
    /// overwritten sectors, AIL insertion, and checkpointing remain owned by
    /// the transaction coordinator.  The caller supplies already-framed log
    /// operations and cannot smuggle a host pointer or VFS object into the
    /// persistent format.
    pub fn encode(&self, operations: &[XfsLogOperation]) -> XfsResult<Vec<u8>> {
        if self.cycle == 0
            || self.lsn == 0
            || (self.lsn >> 32) as u32 != self.cycle
            || self.filesystem_uuid.0 == [0; 16]
        {
            return Err(XfsError::CorruptMetadata);
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(self.payload_bytes as usize)
            .map_err(|_| XfsError::NoMemory)?;
        for operation in operations {
            let length =
                u32::try_from(operation.payload.len()).map_err(|_| XfsError::AddressOutOfRange)?;
            payload
                .try_reserve(12 + operation.payload.len())
                .map_err(|_| XfsError::NoMemory)?;
            payload.extend_from_slice(&operation.transaction_id.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
            payload.push(operation.client_id);
            payload.push(operation.flags);
            payload.extend_from_slice(&[0, 0]);
            payload.extend_from_slice(&operation.payload);
        }
        let aligned = align8(payload.len()).ok_or(XfsError::AddressOutOfRange)?;
        if aligned != self.payload_bytes as usize
            || self.operation_count as usize != operations.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        payload.resize(aligned, 0);
        let header_bytes = self.header_bytes()?;
        let total = align_log_basic_block(
            header_bytes
                .checked_add(payload.len())
                .ok_or(XfsError::AddressOutOfRange)?,
        )?;
        let mut record = vec![0; total];
        put_be32(&mut record, 0, XFS_LOG_RECORD_MAGIC)?;
        put_be32(&mut record, 4, self.cycle)?;
        put_be32(&mut record, 8, self.version)?;
        put_be32(&mut record, 12, self.payload_bytes)?;
        put_be64(&mut record, 16, self.lsn)?;
        put_be64(&mut record, 24, self.tail_lsn)?;
        put_be32(&mut record, 36, self.previous_block)?;
        put_be32(&mut record, 40, self.operation_count)?;
        for (index, value) in self.cycle_data.iter().enumerate() {
            put_be32(&mut record, 44 + index * 4, *value)?;
        }
        put_be32(&mut record, 300, self.format)?;
        record[304..320].copy_from_slice(&self.filesystem_uuid.0);
        put_be32(&mut record, 320, self.iclog_bytes)?;
        // Every extra 32KiB payload window adds a 512-byte extension header.
        // Its cycle-data table is initialized to zero and receives displaced
        // data words during physical cycle stamping.
        for extension in 1..header_bytes / XFS_LOG_BASIC_BLOCK {
            put_be32(&mut record, extension * XFS_LOG_BASIC_BLOCK, self.cycle)?;
        }
        record[header_bytes..header_bytes + payload.len()].copy_from_slice(&payload);
        rewrite_log_record_crc(&mut record, self)?;
        Ok(record)
    }
}

impl XfsJournalRecord {
    pub fn item_byte_order(&self) -> XfsResult<XfsLogByteOrder> {
        match self.header.format {
            1 => Ok(XfsLogByteOrder::Little),
            2 | 3 => Ok(XfsLogByteOrder::Big),
            _ => Err(XfsError::CorruptMetadata),
        }
    }

    /// Parses the native-endian 16-byte transaction header carried by a
    /// `TRANSHDR` operation.  It is intentionally separate from physical
    /// operation framing, whose fields are always big-endian.
    pub fn transaction_header(&self, payload: &[u8]) -> XfsResult<XfsTransactionHeader> {
        if payload.len() != 16 {
            return Err(XfsError::CorruptMetadata);
        }
        let order = self.item_byte_order()?;
        let read = |offset| native_u32(payload, offset, order);
        if read(0)? != 0x5452_414e {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsTransactionHeader {
            transaction_type: read(4)?,
            item_count: read(12)?,
        })
    }

    /// Decodes operation framing from one complete record image.  Continuation
    /// flags are retained verbatim; joining fragments belongs to the recovery
    /// state machine, never to a lossy per-sector reader.
    pub fn decode(bytes: &[u8], expected_uuid: XfsUuid) -> XfsResult<Self> {
        Self::decode_with_crc(bytes, expected_uuid, false)
    }

    pub fn decode_with_crc(
        bytes: &[u8],
        expected_uuid: XfsUuid,
        require_crc: bool,
    ) -> XfsResult<Self> {
        let header = XfsLogRecordHeader::parse(bytes, expected_uuid, require_crc)?;
        let payload_start = header.header_bytes()?;
        let payload_end = payload_start
            .checked_add(header.payload_bytes as usize)
            .ok_or(XfsError::CorruptMetadata)?;
        let payload = slice(bytes, payload_start, payload_end - payload_start)?;
        let mut cursor = 0usize;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(header.operation_count as usize)
            .map_err(|_| XfsError::NoMemory)?;
        for _ in 0..header.operation_count {
            let op_header = slice(payload, cursor, 12)?;
            let transaction_id = be32(op_header, 0)?;
            let length = be32(op_header, 4)? as usize;
            let client_id = byte(op_header, 8)?;
            let flags = byte(op_header, 9)?;
            cursor = cursor.checked_add(12).ok_or(XfsError::CorruptMetadata)?;
            let data = slice(payload, cursor, length)?.to_vec();
            cursor = cursor
                .checked_add(length)
                .ok_or(XfsError::CorruptMetadata)?;
            operations.push(XfsLogOperation {
                transaction_id,
                client_id,
                flags,
                payload: data,
            });
        }
        if cursor > payload.len() || payload[cursor..].iter().any(|byte| *byte != 0) {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(Self { header, operations })
    }
}

impl XfsRecoveryPlan {
    pub const fn new() -> Self {
        Self {
            open: Vec::new(),
            committed: Vec::new(),
            head_lsn: 0,
            tail_lsn: 0,
        }
    }

    /// Adds one record in log order.  Continuations cannot create a
    /// transaction on their own, and a second start for an open id is media
    /// corruption rather than an invitation to overwrite pending operations.
    pub fn ingest(&mut self, record: XfsJournalRecord) -> XfsResult<()> {
        if record.header.lsn == 0 || record.header.tail_lsn > record.header.lsn {
            return Err(XfsError::CorruptMetadata);
        }
        if self.head_lsn != 0 && record.header.lsn <= self.head_lsn {
            // The scanner feeds physical records in log order.  Duplicate or
            // backwards LSNs are not a wrap shortcut: they are stale/corrupt
            // input which must not be replayed as a second transaction.
            return Err(XfsError::CorruptMetadata);
        }
        let record_lsn = record.header.lsn;
        let record_order = record.item_byte_order()?;
        self.head_lsn = record_lsn;
        self.tail_lsn = if self.tail_lsn == 0 {
            record.header.tail_lsn
        } else {
            cmp::min(self.tail_lsn, record.header.tail_lsn)
        };
        for operation in record.operations {
            let flags = operation.flags;
            let starts = flags & XLOG_START_TRANS != 0;
            let commits = flags & XLOG_COMMIT_TRANS != 0;
            let continues =
                flags & (XLOG_CONTINUE_TRANS | XLOG_WAS_CONT_TRANS | XLOG_END_TRANS) != 0;
            let existing = self
                .open
                .iter()
                .position(|entry| entry.transaction_id == operation.transaction_id);
            let index = match (starts, existing) {
                (true, Some(_)) => return Err(XfsError::CorruptMetadata),
                (true, None) => {
                    self.open.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                    self.open.push(XfsRecoveryTransaction {
                        transaction_id: operation.transaction_id,
                        lsn: record_lsn,
                        byte_order: record_order,
                        operations: Vec::new(),
                        pending_operation: None,
                    });
                    self.open.len() - 1
                }
                (false, Some(index)) => index,
                (false, None) if continues || commits => return Err(XfsError::CorruptMetadata),
                (false, None) => return Err(XfsError::CorruptMetadata),
            };
            self.open[index]
                .operations
                .try_reserve(1)
                .map_err(|_| XfsError::NoMemory)?;
            if self.open[index].byte_order != record_order {
                return Err(XfsError::CorruptMetadata);
            }
            self.open[index].push_region(operation)?;
            self.open[index].lsn = record_lsn;
            if commits {
                self.open[index].complete()?;
                let complete = self.open.remove(index);
                self.committed
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                self.committed.push(complete);
            }
        }
        Ok(())
    }

    /// Consumes completed transactions in journal order.  Open transactions
    /// are intentionally not returned: they have no commit proof and must not
    /// be replayed after a crash.
    pub fn into_committed(self) -> Vec<XfsRecoveryTransaction> {
        self.committed
    }

    pub fn state(&self) -> XfsJournalRecoveryState {
        XfsJournalRecoveryState {
            head_lsn: self.head_lsn,
            tail_lsn: self.tail_lsn,
            committed_transactions: self.committed.len(),
            interrupted_transactions: self.open.len(),
        }
    }

    /// Builds all-buffer home-write commits from the complete transactions in
    /// this plan.  Transactions containing inode, dquot, intent, or other
    /// item types fail closed here; they need their corresponding allocator
    /// and quota replay coordinator and must not be split into a subset of
    /// buffer writes.
    pub fn prepare_buffer_commits(&self, volume: &XfsVolume) -> XfsResult<Vec<XfsRecoveryCommit>> {
        let mut commits = Vec::new();
        commits
            .try_reserve_exact(self.committed.len())
            .map_err(|_| XfsError::NoMemory)?;
        for transaction in &self.committed {
            if transaction.operations.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            // Decode every item before selecting a replay engine.  This
            // distinguishes an unsupported but well-formed inode/intent item
            // from corrupt framing and prevents a buffer subset from being
            // applied out of a mixed committed transaction.
            let decoded = transaction.replay_items(transaction.byte_order)?;
            let mut buffers = Vec::new();
            buffers
                .try_reserve_exact(decoded.len())
                .map_err(|_| XfsError::NoMemory)?;
            for item in decoded {
                match item {
                    XfsReplayItem::Buffer(buffer) => buffers.push(buffer),
                    _ => return Err(XfsError::UnsupportedFeature),
                }
            }
            if buffers.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            commits.push(volume.prepare_recovery_commit(transaction.lsn, &buffers)?);
        }
        Ok(commits)
    }

    /// Resolves the complete typed intent/done history before exposing any
    /// pending semantic operation.  Every item in a mixed transaction is
    /// decoded before its intent/done contribution is admitted; ordinary
    /// items are materialized by the transaction writer, not discarded here.
    // Journal recovery path in progress.
    #[allow(dead_code)]
    pub(super) fn pending_extent_free_recovery(
        &self,
    ) -> XfsResult<Vec<XfsPendingExtentFreeRecovery>> {
        let mut registry = XfsIntentRecovery::default();
        let mut observed = Vec::<(u64, XfsIntentReplayItem)>::new();
        for transaction in &self.committed {
            let items = transaction.replay_items(transaction.byte_order)?;
            if items.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            for item in &items {
                match item {
                    XfsReplayItem::Intent(intent) => {
                        observed.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                        observed.push((transaction.lsn, intent.clone()));
                    }
                    XfsReplayItem::Done(_)
                    | XfsReplayItem::Buffer(_)
                    | XfsReplayItem::Inode(_)
                    | XfsReplayItem::Dquot(_)
                    | XfsReplayItem::Quotaoff { .. } => {}
                }
            }
            // The registry clone-and-commit operation is the transaction
            // boundary: malformed done ordering cannot leave an earlier
            // item visible to semantic replay.
            registry.apply_transaction(&items)?;
        }

        let mut steps = Vec::<XfsPendingExtentFreeRecovery>::new();
        for (lsn, intent) in observed {
            if !registry.is_pending(intent.key) {
                continue;
            }
            if intent.key.kind != XfsIntentKind::ExtentFree {
                // RUI/CUI/BUI require respectively rmapbt, refcountbt, and
                // inode/bmap metadata mutations.  This volume has no
                // atomic writer for those layouts, so a pending operation is
                // never mistaken for an allocator free.
                return Err(XfsError::UnsupportedFeature);
            }
            let mut extents = Vec::new();
            extents
                .try_reserve_exact(intent.extents.len())
                .map_err(|_| XfsError::NoMemory)?;
            for extent in intent.extents {
                let XfsLogReplayExtent::ExtentFree {
                    start_block,
                    block_count,
                } = extent
                else {
                    return Err(XfsError::CorruptMetadata);
                };
                extents.push((start_block, block_count));
            }
            if extents.is_empty() {
                return Err(XfsError::CorruptMetadata);
            }
            if let Some(step) = steps.last_mut().filter(|step| step.lsn == lsn) {
                step.extents
                    .try_reserve(extents.len())
                    .map_err(|_| XfsError::NoMemory)?;
                step.extents.extend(extents);
            } else {
                steps.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                steps.push(XfsPendingExtentFreeRecovery { lsn, extents });
            }
        }
        Ok(steps)
    }
}

impl XfsExtent {
    pub(super) fn parse(bytes: &[u8]) -> XfsResult<Self> {
        let encoded = u128::from_be_bytes(
            slice(bytes, 0, 16)?
                .try_into()
                .map_err(|_| XfsError::CorruptMetadata)?,
        );
        let file_block = ((encoded >> 73) & ((1u128 << 54) - 1)) as u64;
        let start_block = ((encoded >> 21) & ((1u128 << 52) - 1)) as u64;
        let block_count = (encoded & ((1u128 << 21) - 1)) as u32;
        if block_count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(Self {
            unwritten: encoded >> 127 != 0,
            file_block,
            start_block,
            block_count,
        })
    }
}
