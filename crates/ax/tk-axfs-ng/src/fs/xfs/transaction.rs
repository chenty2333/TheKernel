//! XFS log reservations, the in-memory log ring, AIL and metadata transactions.

use super::*;

pub(super) const XFS_LOG_BASIC_BLOCK: usize = 512;
pub(super) const XFS_LOG_MAX_INLINE_CYCLE_DATA: usize = 64;
pub(super) const XFS_DQ_DEFAULT_GRACE_SECONDS: u32 = 7 * 24 * 60 * 60;
// These are the on-disk `sb_qflags` bits from xfs_log_format.h.  They are
// deliberately not the similarly named FS_{U,G,PROJ}QUOTA UAPI flags.
pub(super) const XFS_UQUOTA_ACCT: u16 = 1 << 0;
pub(super) const XFS_UQUOTA_ENFD: u16 = 1 << 1;
pub(super) const XFS_PQUOTA_ACCT: u16 = 1 << 3;
pub(super) const XFS_GQUOTA_ACCT: u16 = 1 << 6;
pub(super) const XFS_GQUOTA_ENFD: u16 = 1 << 7;
pub(super) const XFS_PQUOTA_ENFD: u16 = 1 << 9;

/// One reserved, in-order region of the physical XFS log ring.  A reservation
/// may contain two segments when the record crosses the end of the ring; the
/// second segment has the next cycle and is never presented to media before
/// the first segment's cycle stamps have been durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogReservation {
    pub lsn: u64,
    pub cycle: u32,
    pub first_block: u32,
    pub record_blocks: u32,
    pub first_segment_blocks: u32,
    pub second_segment_blocks: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XfsLogFragment {
    pub start_block: u32,
    pub blocks: u32,
    pub cycle: u32,
    pub continued: bool,
}

impl XfsLogReservation {
    /// First free log position after this reservation.  This is the cursor
    /// which becomes reclaimable only after every home image for the record
    /// has reached stable storage.
    pub fn end_lsn(&self, ring_blocks: u32) -> XfsResult<u64> {
        if ring_blocks < 2 || self.first_block >= ring_blocks || self.record_blocks == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let end = self
            .first_block
            .checked_add(self.record_blocks)
            .ok_or(XfsError::AddressOutOfRange)?;
        if end < ring_blocks {
            return Ok((u64::from(self.cycle) << 32) | u64::from(end));
        }
        let cycle = self
            .cycle
            .checked_add(1)
            .ok_or(XfsError::AddressOutOfRange)?;
        Ok((u64::from(cycle) << 32) | u64::from(end - ring_blocks))
    }

    /// Returns physical write fragments in ring order.  A coordinator maps
    /// the split point to `CONTINUE/WAS_CONT/END` operation flags while
    /// preserving the original operation byte stream; it must not treat the
    /// second fragment as a new committed transaction.
    pub fn fragments(&self) -> XfsResult<Vec<XfsLogFragment>> {
        let mut fragments = Vec::new();
        fragments
            .try_reserve(if self.second_segment_blocks == 0 {
                1
            } else {
                2
            })
            .map_err(|_| XfsError::NoMemory)?;
        fragments.push(XfsLogFragment {
            start_block: self.first_block,
            blocks: self.first_segment_blocks,
            cycle: self.cycle,
            continued: self.second_segment_blocks != 0,
        });
        if self.second_segment_blocks != 0 {
            fragments.push(XfsLogFragment {
                start_block: 0,
                blocks: self.second_segment_blocks,
                cycle: self
                    .cycle
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                continued: true,
            });
        }
        Ok(fragments)
    }
}

/// Native physical-log cursor and grant accounting.  Addresses are log-basic
/// blocks relative to the log device, never data-device filesystem blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsLogRing {
    pub(super) blocks: u32,
    pub(super) head: u32,
    /// Full LSN of the first log position that has not completed a durable
    /// home-image checkpoint.  Keeping the cycle is essential across wrap.
    pub(super) tail_lsn: u64,
    pub(super) cycle: u32,
    pub(super) last_record: Option<u32>,
}

impl XfsLogRing {
    pub fn new(blocks: u32, head: u32, tail: u32, cycle: u32) -> XfsResult<Self> {
        if blocks < 2 || head >= blocks || tail >= blocks || cycle == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let tail_cycle = if head < tail {
            cycle.checked_sub(1).ok_or(XfsError::CorruptMetadata)?
        } else {
            cycle
        };
        Ok(Self {
            blocks,
            head,
            tail_lsn: (u64::from(tail_cycle) << 32) | u64::from(tail),
            cycle,
            last_record: None,
        })
    }

    /// Restores a recovered cursor together with the physical predecessor of
    /// the next record.  Recovery-generated commits must extend the existing
    /// on-media chain instead of treating the durable tail as a predecessor.
    pub fn recovered(
        blocks: u32,
        head: u32,
        tail: u32,
        cycle: u32,
        last_record: u32,
    ) -> XfsResult<Self> {
        if last_record >= blocks {
            return Err(XfsError::CorruptMetadata);
        }
        let mut ring = Self::new(blocks, head, tail, cycle)?;
        ring.last_record = Some(last_record);
        Ok(ring)
    }

    pub const fn head(&self) -> u32 {
        self.head
    }
    pub const fn tail(&self) -> u32 {
        self.tail_lsn as u32
    }
    pub const fn cycle(&self) -> u32 {
        self.cycle
    }
    pub const fn blocks(&self) -> u32 {
        self.blocks
    }
    pub const fn next_lsn(&self) -> u64 {
        (self.cycle as u64) << 32 | self.head as u64
    }
    pub const fn tail_lsn(&self) -> u64 {
        self.tail_lsn
    }
    pub const fn previous_record(&self) -> u32 {
        match self.last_record {
            Some(block) => block,
            None => self.tail(),
        }
    }

    pub(super) fn used(&self) -> u32 {
        let head = self.next_lsn();
        let tail = self.tail_lsn;
        if head < tail {
            return self.blocks;
        }
        let cycles = (head >> 32).saturating_sub(tail >> 32);
        let distance = cycles
            .checked_mul(u64::from(self.blocks))
            .and_then(|value| value.checked_add(u64::from(head as u32)))
            .and_then(|value| value.checked_sub(u64::from(tail as u32)));
        distance
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value < self.blocks)
            .unwrap_or(self.blocks)
    }

    pub fn free_blocks(&self) -> u32 {
        self.blocks.saturating_sub(self.used()).saturating_sub(1)
    }

    /// Reserves a complete record and advances the volatile head.  The tail
    /// guard prevents head from becoming indistinguishable from a full ring;
    /// callers must checkpoint AIL entries before retrying an exhausted grant.
    pub fn reserve(&mut self, record_bytes: usize) -> XfsResult<XfsLogReservation> {
        if record_bytes == 0 || record_bytes % XFS_LOG_BASIC_BLOCK != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let record_blocks = u32::try_from(record_bytes / XFS_LOG_BASIC_BLOCK)
            .map_err(|_| XfsError::AddressOutOfRange)?;
        if record_blocks == 0 || record_blocks >= self.blocks || record_blocks > self.free_blocks()
        {
            return Err(XfsError::AddressOutOfRange);
        }
        let remaining = self.blocks - self.head;
        let (first, second) = if record_blocks <= remaining {
            (record_blocks, 0)
        } else {
            (remaining, record_blocks - remaining)
        };
        // Wrapping consumes the remaining ring blocks as a cycle boundary;
        // they cannot be allocated to another writer before this record.
        let consumed = if second == 0 {
            record_blocks
        } else {
            remaining
                .checked_add(second)
                .ok_or(XfsError::AddressOutOfRange)?
        };
        if consumed > self.free_blocks() {
            return Err(XfsError::AddressOutOfRange);
        }
        let lsn = (u64::from(self.cycle) << 32) | u64::from(self.head);
        let reservation = XfsLogReservation {
            lsn,
            cycle: self.cycle,
            first_block: self.head,
            record_blocks,
            first_segment_blocks: first,
            second_segment_blocks: second,
        };
        self.last_record = Some(reservation.first_block);
        if second == 0 {
            self.head = self
                .head
                .checked_add(record_blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            if self.head == self.blocks {
                self.head = 0;
                self.cycle = self
                    .cycle
                    .checked_add(1)
                    .ok_or(XfsError::AddressOutOfRange)?;
            }
        } else {
            self.head = second;
            self.cycle = self
                .cycle
                .checked_add(1)
                .ok_or(XfsError::AddressOutOfRange)?;
        }
        Ok(reservation)
    }

    /// Advances the durable tail after an AIL checkpoint has written all home
    /// blocks below `lsn`.  A tail never moves past the current head and LSNs
    /// from an older cycle are rejected rather than numerically wrapped.
    pub fn checkpoint_tail(&mut self, lsn: u64) -> XfsResult<()> {
        let cycle = (lsn >> 32) as u32;
        let block = lsn as u32;
        if block >= self.blocks || cycle > self.cycle || self.cycle.saturating_sub(cycle) > 1 {
            return Err(XfsError::CorruptMetadata);
        }
        if lsn < self.tail_lsn || lsn > self.next_lsn() {
            return Err(XfsError::CorruptMetadata);
        }
        self.tail_lsn = lsn;
        Ok(())
    }

    /// Applies physical sector cycle stamping to a single inline record.
    /// Stamping overwrites the leading word of every data basic block; the
    /// displaced words are saved in the header cycle-data array so recovery
    /// can reconstruct the logical record before interpreting log operations.
    pub fn stamp_inline_record(
        &self,
        reservation: &XfsLogReservation,
        record: &mut [u8],
    ) -> XfsResult<()> {
        if reservation.second_segment_blocks != 0 {
            return Err(XfsError::CorruptMetadata);
        }
        self.stamp_record(reservation, record)
    }

    /// Stamps a record that may cross the physical ring end.  The logical
    /// record keeps its original header cycle; individual basic blocks after
    /// the wrap carry the following cycle.  The caller writes the resulting
    /// byte stream in `reservation.fragments()` order.
    pub fn stamp_record(
        &self,
        reservation: &XfsLogReservation,
        record: &mut [u8],
    ) -> XfsResult<()> {
        let header = XfsLogRecordHeader::parse(
            record,
            XfsUuid(
                record[304..320]
                    .try_into()
                    .map_err(|_| XfsError::CorruptMetadata)?,
            ),
            false,
        )?;
        let header_bytes = header.header_bytes()?;
        if record.len() % XFS_LOG_BASIC_BLOCK != 0
            || record.len() / XFS_LOG_BASIC_BLOCK != reservation.record_blocks as usize
            || be32(record, 0)? != XFS_LOG_RECORD_MAGIC
            || be32(record, 4)? != reservation.cycle
            || header_bytes > record.len()
        {
            return Err(XfsError::CorruptMetadata);
        }
        for extension in 1..header_bytes / XFS_LOG_BASIC_BLOCK {
            let physical = reservation
                .first_block
                .checked_add(extension as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            let cycle = reservation
                .cycle
                .checked_add((physical >= self.blocks) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            put_be32(record, extension * XFS_LOG_BASIC_BLOCK, cycle)?;
        }
        for basic_block in 0..(record.len() - header_bytes) / XFS_LOG_BASIC_BLOCK {
            let offset = header_bytes + basic_block * XFS_LOG_BASIC_BLOCK;
            let displaced = be32(record, offset)?;
            log_cycle_data_put(record, basic_block, displaced)?;
            let physical = reservation
                .first_block
                .checked_add((header_bytes / XFS_LOG_BASIC_BLOCK + basic_block) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            let cycle = reservation
                .cycle
                .checked_add((physical >= self.blocks) as u32)
                .ok_or(XfsError::AddressOutOfRange)?;
            put_be32(record, offset, cycle)?;
        }
        rewrite_log_record_crc(record, &header)?;
        Ok(())
    }

    /// Restores an inline record after physical cycle validation.  The caller
    /// must verify the stamped record checksum before invoking this method.
    pub fn unstamp_inline_record(record: &mut [u8]) -> XfsResult<()> {
        if record.len() % XFS_LOG_BASIC_BLOCK != 0 || be32(record, 0)? != XFS_LOG_RECORD_MAGIC {
            return Err(XfsError::CorruptMetadata);
        }
        let header = XfsLogRecordHeader::parse(
            record,
            XfsUuid(
                record[304..320]
                    .try_into()
                    .map_err(|_| XfsError::CorruptMetadata)?,
            ),
            false,
        )?;
        let header_bytes = header.header_bytes()?;
        if header_bytes > record.len() {
            return Err(XfsError::CorruptMetadata);
        }
        let cycle = be32(record, 4)?;
        for basic_block in 0..(record.len() - header_bytes) / XFS_LOG_BASIC_BLOCK {
            let offset = header_bytes + basic_block * XFS_LOG_BASIC_BLOCK;
            let stamped = be32(record, offset)?;
            if stamped != cycle
                && stamped != cycle.checked_add(1).ok_or(XfsError::AddressOutOfRange)?
            {
                return Err(XfsError::CorruptMetadata);
            }
            let displaced = log_cycle_data_get(record, basic_block)?;
            put_be32(record, offset, displaced)?;
        }
        Ok(())
    }
}

/// Active-item-list entry.  The AIL owns only checkpoint ordering: metadata
/// item decoding and home writes remain in the corresponding transaction
/// implementation, so an item can never be removed merely because it was
/// submitted rather than made durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsAilEntry {
    pub lsn: u64,
    pub end_lsn: u64,
    pub transaction_id: u32,
    /// Exact post-LSN home images.  They are prepared before the log record
    /// is persisted and retained until the ordered AIL checkpoint succeeds,
    /// so an interrupted home flush never re-plans metadata against changed
    /// media or leaks a ring reservation.
    pub(super) checkpoint_homes: Vec<(u64, Vec<u8>)>,
}

#[derive(Default)]
pub struct XfsAil {
    pub(super) entries: Vec<XfsAilEntry>,
}

impl XfsAil {
    pub(super) fn reserve_insert(&mut self, entry: &XfsAilEntry) -> XfsResult<()> {
        if entry.lsn == 0
            || entry.end_lsn <= entry.lsn
            || self
                .entries
                .iter()
                .any(|present| present.transaction_id == entry.transaction_id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        self.entries.try_reserve(1).map_err(|_| XfsError::NoMemory)
    }

    pub(super) fn insert_reserved(&mut self, entry: XfsAilEntry) -> XfsResult<()> {
        if entry.lsn == 0
            || entry.end_lsn <= entry.lsn
            || self
                .entries
                .iter()
                .any(|present| present.transaction_id == entry.transaction_id)
        {
            return Err(XfsError::CorruptMetadata);
        }
        let index = self
            .entries
            .partition_point(|present| present.lsn < entry.lsn);
        self.entries.insert(index, entry);
        Ok(())
    }

    pub fn insert(&mut self, entry: XfsAilEntry) -> XfsResult<()> {
        self.reserve_insert(&entry)?;
        self.insert_reserved(entry)
    }
    pub fn oldest(&self) -> Option<&XfsAilEntry> {
        self.entries.first()
    }
    pub fn checkpoint_through(&mut self, lsn: u64) -> Vec<XfsAilEntry> {
        let end = self.entries.partition_point(|entry| entry.lsn <= lsn);
        self.entries.drain(..end).collect()
    }
    pub fn entries(&self) -> &[XfsAilEntry] {
        &self.entries
    }

    pub(super) fn attach_checkpoint_homes(
        &mut self,
        lsn: u64,
        homes: Vec<(u64, Vec<u8>)>,
    ) -> XfsResult<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.lsn == lsn)
            .ok_or(XfsError::CorruptMetadata)?;
        if !entry.checkpoint_homes.is_empty() || homes.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        entry.checkpoint_homes = homes;
        Ok(())
    }
}

/// A fully encoded and cycle-stamped log record that owns its grant until it
/// is either durably written and published to the AIL or retried.  Keeping the
/// byte image here is essential: regenerating it after a partial FUA failure
/// could allocate a different LSN or lose the original continuation layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsPreparedLogCommit {
    pub reservation: XfsLogReservation,
    pub transaction_id: u32,
    pub record: Vec<u8>,
}

/// One typed AG metadata home image staged for an atomic XFS transaction.
/// `basic_block` is a physical 512-byte address, while `before`/`after` are
/// complete buffer images; callers cannot submit an unbounded byte patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsDirtyMetadataBuffer {
    pub metadata_type: XfsMetadataBufferType,
    pub basic_block: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl XfsDirtyMetadataBuffer {
    pub fn to_log_item(&self) -> XfsResult<XfsBufferReplayItem> {
        if self.before.is_empty()
            || self.before.len() != self.after.len()
            || self.before.len() % XFS_LOG_BASIC_BLOCK != 0
        {
            return Err(XfsError::CorruptMetadata);
        }
        let kind = match self.metadata_type {
            XfsMetadataBufferType::Btree => 4u16,
            XfsMetadataBufferType::Agf => 5,
            XfsMetadataBufferType::Agfl => 6,
            XfsMetadataBufferType::Agi => 7,
            XfsMetadataBufferType::Inode => 8,
            XfsMetadataBufferType::Dquot => 3,
            XfsMetadataBufferType::Directory => 10,
            XfsMetadataBufferType::Attribute => 15,
            XfsMetadataBufferType::Superblock => 18,
            XfsMetadataBufferType::Realtime => 19,
        };
        let mut dirty_chunks = Vec::new();
        let mut chunks = Vec::new();
        for offset in (0..self.after.len()).step_by(128) {
            if self.before[offset..offset + 128] != self.after[offset..offset + 128] {
                dirty_chunks
                    .try_reserve(1)
                    .map_err(|_| XfsError::NoMemory)?;
                chunks.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
                dirty_chunks
                    .push(u32::try_from(offset / 128).map_err(|_| XfsError::AddressOutOfRange)?);
                chunks.push(self.after[offset..offset + 128].to_vec());
            }
        }
        if dirty_chunks.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        Ok(XfsBufferReplayItem {
            flags: kind << 11,
            block_number: self.basic_block,
            block_count: u16::try_from(self.after.len() / XFS_LOG_BASIC_BLOCK)
                .map_err(|_| XfsError::AddressOutOfRange)?,
            dirty_chunks,
            chunks,
        })
    }
}

/// All metadata buffers for one transaction.  Conversion is all-or-nothing:
/// no log grant or home write is attempted until every buffer's dirty bitmap
/// has been constructed successfully.
/// A data block whose allocation and contents are prepared alongside a
/// metadata transaction.  XFS deliberately writes ordinary file data before
/// publishing the inode mapping; the same ordering is required for remote
/// symlink bodies, otherwise a committed name could resolve to uninitialised
/// bytes after a crash.  The data itself is not replayed from the journal:
/// it is FUA-written before the transaction is committed, while the logged
/// inode and AG updates make it reachable only afterwards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsStagedDataWrite {
    pub fs_block: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct XfsMetadataTransaction {
    pub buffers: Vec<XfsDirtyMetadataBuffer>,
    pub data_writes: Vec<XfsStagedDataWrite>,
    pub realtime_writes: Vec<XfsStagedDataWrite>,
    /// Reservation and admission have already completed for these native
    /// dquots.  They remain explicit until commit construction, where their
    /// full images become DQUOT buffer items in this same log record.
    pub dquots: Vec<XfsDquotDelta>,
}

/// The fixed dinode-core fields which can be replaced without changing an
/// inode's fork layout.  This is deliberately narrower than a generic VFS
/// setattr: device and project-id updates have additional ownership/quota
/// transactions which are not part of this primitive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct XfsInodeCoreUpdate {
    pub mode: Option<u16>,
    pub owner: Option<(u32, u32)>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
    pub ctime: Option<(i64, u32)>,
}

impl XfsInodeCoreUpdate {
    pub const fn is_empty(self) -> bool {
        self.mode.is_none()
            && self.owner.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
            && self.ctime.is_none()
    }
}

/// Aggregate counters reconstructed from the canonical ownership view of
/// every allocation group.  `free_*` values are not copied from a convenient
/// superblock field: each one is checked against the btree-derived snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct XfsStatCounts {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
}

impl XfsMetadataTransaction {
    /// Produces one exact home image per physical buffer.  Independent inode
    /// cores can share a filesystem block, and AG/allocator planners can
    /// contribute disjoint fields to one header; rejecting those merely makes
    /// atomic namespace operations impossible.  Every contributor must start
    /// from the same `before` image.  A byte changed by two contributors is
    /// accepted only when both select the identical value; otherwise there is
    /// no serializable transaction and the whole request is rejected before a
    /// log grant or home write.
    pub fn composed_buffers(&self) -> XfsResult<Vec<XfsDirtyMetadataBuffer>> {
        let mut composed = Vec::<XfsDirtyMetadataBuffer>::new();
        composed
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| XfsError::NoMemory)?;
        for buffer in &self.buffers {
            if buffer.before.is_empty()
                || buffer.before.len() != buffer.after.len()
                || buffer.before.len() % XFS_LOG_BASIC_BLOCK != 0
            {
                return Err(XfsError::CorruptMetadata);
            }
            let blocks = u64::try_from(buffer.before.len() / XFS_LOG_BASIC_BLOCK)
                .map_err(|_| XfsError::AddressOutOfRange)?;
            let end = buffer
                .basic_block
                .checked_add(blocks)
                .ok_or(XfsError::AddressOutOfRange)?;
            let existing = composed
                .iter_mut()
                .find(|prior| prior.basic_block == buffer.basic_block);
            let Some(existing) = existing else {
                if composed.iter().any(|prior| {
                    let prior_blocks =
                        u64::try_from(prior.before.len() / XFS_LOG_BASIC_BLOCK).unwrap_or(0);
                    let prior_end = prior.basic_block.checked_add(prior_blocks).unwrap_or(0);
                    prior_blocks == 0 || buffer.basic_block < prior_end && prior.basic_block < end
                }) {
                    return Err(XfsError::CorruptMetadata);
                }
                composed.push(buffer.clone());
                continue;
            };
            // Allocator trees are a semantic reservation, not merely a byte
            // patch.  Two independently prepared AG free-space images can be
            // byte-identical while claiming the same extent.  Only the batch
            // planner is permitted to produce an AGF/AGFL replacement, so
            // never merge a second one here.
            if matches!(
                existing.metadata_type,
                XfsMetadataBufferType::Agf | XfsMetadataBufferType::Agfl
            ) {
                return Err(XfsError::CorruptMetadata);
            }
            if existing.metadata_type != buffer.metadata_type
                || existing.before != buffer.before
                || existing.after.len() != buffer.after.len()
            {
                return Err(XfsError::CorruptMetadata);
            }
            for index in 0..existing.after.len() {
                let old = existing.before[index];
                let prior = existing.after[index];
                let next = buffer.after[index];
                if prior != old && next != old && prior != next {
                    return Err(XfsError::CorruptMetadata);
                }
                if next != old {
                    existing.after[index] = next;
                }
            }
        }
        Ok(composed)
    }

    pub fn log_items(&self) -> XfsResult<Vec<XfsBufferReplayItem>> {
        if self.buffers.is_empty() {
            return Err(XfsError::CorruptMetadata);
        }
        let buffers = self.composed_buffers()?;
        let mut items = Vec::new();
        items
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| XfsError::NoMemory)?;
        for buffer in &buffers {
            items.push(buffer.to_log_item()?);
        }
        Ok(items)
    }
}

/// An extent selected from one AG plus the fully staged free-space metadata
/// transaction.  It is deliberately not a reservation token: publication is
/// only possible through `commit_metadata_transaction`, after the one record
/// containing every AGF/AGFL/bnobt/cntbt buffer is durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsExtentAllocation {
    pub ag: u32,
    pub start_block: u32,
    pub block_count: u32,
    pub transaction: XfsMetadataTransaction,
}

/// Several non-overlapping reservations selected from one AG ownership
/// snapshot.  Its single metadata transaction is the only free-space update
/// that may accompany the individual extents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsExtentAllocationBatch {
    pub ag: u32,
    pub allocations: Vec<XfsExtentAllocation>,
    pub transaction: XfsMetadataTransaction,
}

/// An inode number selected from an inobt record.  The allocation bitmap and
/// (when present) finobt are staged together with AGI; inode-core creation is
/// intentionally a later buffer item in the caller's same transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsInodeAllocation {
    pub ag: u32,
    pub ag_inode: u32,
    pub inode: u64,
    /// A remote symlink data run reserved by the same AG snapshot as this
    /// inode bit.  Its free-space images already live in `transaction`.
    pub remote_data: Option<XfsExtentAllocation>,
    pub transaction: XfsMetadataTransaction,
}

/// Initial persistent state for an inode selected from an AG inobt record.
/// The inode is not namespace-visible until its initialized core, parent
/// directory image and allocator transaction are committed together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsNewInode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub project_id: u32,
    /// Required only for a directory's native shortform `..` record.
    pub parent: Option<u64>,
    /// Bytes for a new symlink.  The bytes are native raw pathname bytes,
    /// never a UTF-8 surrogate; short values use the local fork while long
    /// values allocate and stage a remote data extent before publication.
    pub symlink_target: Option<Vec<u8>>,
}

/// Result of a name publication decision made while the mount coordinator
/// owns its namespace/log lock.  `Existing` is not a failed create: it is the
/// atomic OpenOrCreate outcome, and must not be reconstructed by an unlocked
/// VFS lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XfsNamedInodeOutcome {
    Created(u64),
    Existing(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsRegularWrite {
    pub inode: u64,
    pub offset: u64,
    pub length: usize,
    pub allocated: Vec<XfsExtent>,
    pub mappings: Vec<XfsExtent>,
    pub zero_before_write: Vec<XfsExtent>,
    /// New COW homes whose untouched bytes must be copied from the old
    /// physical block before the caller's partial write is overlaid.  Keeping
    /// this in the prepared transaction is what prevents a partial write to a
    /// shared reflink block from exposing zero-filled neighbour bytes.
    pub copy_before_write: Vec<(u64, u64)>,
    pub metadata: XfsMetadataTransaction,
}

/// Structural result of one external data-fork bmapbt mutation.  `changed`
/// contains both reused and newly reserved blocks; `reclaimed` is returned to
/// its owning AG in the *same* metadata transaction that installs the new
/// inode root.  Keeping this as an explicit result makes root promotion and
/// collapse observable to callers instead of turning them into an accidental
/// side effect of an extent rewrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XfsBmapLocalMutation {
    pub inode: u64,
    pub old_root_level: u16,
    pub new_root_level: u16,
    pub changed_blocks: Vec<u64>,
    pub reclaimed_blocks: Vec<u64>,
}

/// One allocation-group-local reflink mutation.  The planner owns verified
/// snapshots of *all* AG indexes which participate in a sharing operation;
/// callers never update rmap and refcount as independent journal records.
///
/// Keeping this representation block based is intentional.  XFS extent
/// mappings are block aligned, and partitioning at a block boundary gives the
/// rmap/refcount transformations a single, unambiguous owner and offset.
#[derive(Clone, Debug)]
pub struct XfsAgMutationPlanner {
    pub ag: u32,
    pub free: Vec<XfsAgFreeRecord>,
    pub rmap: Vec<XfsRmapRecord>,
    pub refcount: Vec<XfsRefcountRecord>,
}

impl XfsAgMutationPlanner {
    pub(super) fn new(volume: &XfsVolume, ag: u32) -> XfsResult<Self> {
        let snapshot = volume.ag_ownership_snapshot(ag)?;
        Ok(Self {
            ag,
            free: snapshot.free_extents,
            rmap: volume.rmap_records(ag)?,
            refcount: volume.refcount_records(ag)?,
        })
    }

    pub(super) fn refcount_at(&self, block: u32) -> XfsResult<u32> {
        match self.refcount.iter().find(|record| {
            block >= record.start_block
                && block
                    < record
                        .start_block
                        .checked_add(record.block_count)
                        .unwrap_or(0)
        }) {
            Some(record) => Ok(record.refcount),
            None => Ok(1),
        }
    }

    pub(super) fn set_refcount(&mut self, block: u32, count: u32) -> XfsResult<()> {
        if count == 0 {
            return Err(XfsError::CorruptMetadata);
        }
        let old = core::mem::take(&mut self.refcount);
        let mut next = Vec::new();
        next.try_reserve(old.len().saturating_add(1))
            .map_err(|_| XfsError::NoMemory)?;
        for record in old {
            let end = record
                .start_block
                .checked_add(record.block_count)
                .ok_or(XfsError::CorruptMetadata)?;
            if block < record.start_block || block >= end {
                next.push(record);
                continue;
            }
            if record.start_block < block {
                next.push(XfsRefcountRecord {
                    start_block: record.start_block,
                    block_count: block - record.start_block,
                    refcount: record.refcount,
                });
            }
            if block.checked_add(1).ok_or(XfsError::AddressOutOfRange)? < end {
                next.push(XfsRefcountRecord {
                    start_block: block + 1,
                    block_count: end - block - 1,
                    refcount: record.refcount,
                });
            }
        }
        // A count of one is represented by absence from refcountbt.
        if count > 1 {
            next.push(XfsRefcountRecord {
                start_block: block,
                block_count: 1,
                refcount: count,
            });
        }
        next.sort_unstable_by_key(|record| record.start_block);
        let mut merged: Vec<XfsRefcountRecord> = Vec::new();
        for record in next {
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
        self.refcount = merged;
        Ok(())
    }

    pub(super) fn add_owner(&mut self, block: u32, owner: u64, offset: u64) -> XfsResult<()> {
        if self.rmap.iter().any(|record| {
            record.start_block == block
                && record.block_count == 1
                && record.owner == owner
                && record.offset == offset
        }) {
            return Err(XfsError::CorruptMetadata);
        }
        self.rmap.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        self.rmap.push(XfsRmapRecord {
            start_block: block,
            block_count: 1,
            owner,
            offset,
        });
        self.rmap
            .sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
        Ok(())
    }

    pub(super) fn remove_owner(&mut self, block: u32, owner: u64, offset: u64) -> XfsResult<()> {
        let at = self
            .rmap
            .iter()
            .position(|record| {
                record.owner == owner
                    && block >= record.start_block
                    && block
                        < record
                            .start_block
                            .checked_add(record.block_count)
                            .unwrap_or(0)
                    && record
                        .offset
                        .checked_add(u64::from(block - record.start_block))
                        == Some(offset)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let record = self.rmap.remove(at);
        let relative = block - record.start_block;
        if relative != 0 {
            self.rmap.push(XfsRmapRecord {
                start_block: record.start_block,
                block_count: relative,
                owner,
                offset: record.offset,
            });
        }
        let tail = record.block_count - relative - 1;
        if tail != 0 {
            self.rmap.push(XfsRmapRecord {
                start_block: block + 1,
                block_count: tail,
                owner,
                offset: offset.checked_add(1).ok_or(XfsError::AddressOutOfRange)?,
            });
        }
        self.rmap
            .sort_unstable_by_key(|record| (record.start_block, record.owner, record.offset));
        Ok(())
    }

    pub(super) fn release_free_block(&mut self, block: u32) -> XfsResult<()> {
        if block < 4 {
            return Err(XfsError::CorruptMetadata);
        }
        self.free.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
        self.free.push(XfsAgFreeRecord {
            start_block: block,
            block_count: 1,
        });
        self.free.sort_unstable_by_key(|record| record.start_block);
        let mut merged: Vec<XfsAgFreeRecord> = Vec::new();
        for record in core::mem::take(&mut self.free) {
            if let Some(last) = merged.last_mut()
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
        self.free = merged;
        Ok(())
    }

    pub(super) fn claim_free_block(&mut self) -> XfsResult<u32> {
        let index = self
            .free
            .iter()
            .position(|record| record.block_count != 0)
            .ok_or(XfsError::AddressOutOfRange)?;
        let block = self.free[index].start_block;
        self.free[index].start_block = block.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        self.free[index].block_count -= 1;
        if self.free[index].block_count == 0 {
            self.free.remove(index);
        }
        Ok(block)
    }

    /// Claims one already-selected free block.  Allocation selection and the
    /// final four-tree image must use the same snapshot; accepting an
    /// arbitrary later free block here would make the normal allocator and a
    /// reflink COW transaction race for the same AGFL replacement home.
    pub(super) fn claim_specific_free_block(&mut self, block: u32) -> XfsResult<()> {
        let index = self
            .free
            .iter()
            .position(|record| {
                block >= record.start_block
                    && block
                        < record
                            .start_block
                            .checked_add(record.block_count)
                            .unwrap_or(0)
            })
            .ok_or(XfsError::CorruptMetadata)?;
        let record = self.free.remove(index);
        if record.start_block < block {
            self.free.push(XfsAgFreeRecord {
                start_block: record.start_block,
                block_count: block - record.start_block,
            });
        }
        let next = block.checked_add(1).ok_or(XfsError::AddressOutOfRange)?;
        let end = record
            .start_block
            .checked_add(record.block_count)
            .ok_or(XfsError::CorruptMetadata)?;
        if next < end {
            self.free.push(XfsAgFreeRecord {
                start_block: next,
                block_count: end - next,
            });
        }
        self.free.sort_unstable_by_key(|record| record.start_block);
        Ok(())
    }
}

pub(super) fn xfs_extent_at(
    extents: &[XfsExtent],
    file_block: u64,
) -> XfsResult<Option<XfsExtent>> {
    let Some(extent) = extents.iter().copied().find(|extent| {
        file_block >= extent.file_block
            && file_block
                < extent
                    .file_block
                    .checked_add(u64::from(extent.block_count))
                    .unwrap_or(0)
    }) else {
        return Ok(None);
    };
    let relative = file_block
        .checked_sub(extent.file_block)
        .ok_or(XfsError::CorruptMetadata)?;
    Ok(Some(XfsExtent {
        unwritten: extent.unwritten,
        file_block,
        start_block: extent
            .start_block
            .checked_add(relative)
            .ok_or(XfsError::AddressOutOfRange)?,
        block_count: 1,
    }))
}

pub(super) fn xfs_replace_one_mapping(
    extents: &mut Vec<XfsExtent>,
    file_block: u64,
    replacement: Option<XfsExtent>,
) -> XfsResult<()> {
    let mut next = Vec::new();
    next.try_reserve(extents.len().saturating_add(2))
        .map_err(|_| XfsError::NoMemory)?;
    for extent in core::mem::take(extents) {
        let end = extent
            .file_block
            .checked_add(u64::from(extent.block_count))
            .ok_or(XfsError::CorruptMetadata)?;
        if file_block < extent.file_block || file_block >= end {
            next.push(extent);
            continue;
        }
        let before = file_block - extent.file_block;
        if before != 0 {
            next.push(XfsExtent {
                block_count: u32::try_from(before).map_err(|_| XfsError::AddressOutOfRange)?,
                ..extent
            });
        }
        let after = end - file_block - 1;
        if after != 0 {
            next.push(XfsExtent {
                file_block: file_block + 1,
                start_block: extent
                    .start_block
                    .checked_add(before + 1)
                    .ok_or(XfsError::AddressOutOfRange)?,
                block_count: u32::try_from(after).map_err(|_| XfsError::AddressOutOfRange)?,
                ..extent
            });
        }
    }
    if let Some(replacement) = replacement {
        next.push(replacement);
    }
    next.sort_unstable_by_key(|extent| extent.file_block);
    let mut merged: Vec<XfsExtent> = Vec::new();
    for extent in next {
        if let Some(last) = merged.last_mut()
            && last.unwritten == extent.unwritten
            && last.file_block.checked_add(u64::from(last.block_count)) == Some(extent.file_block)
            && last.start_block.checked_add(u64::from(last.block_count)) == Some(extent.start_block)
        {
            last.block_count = last
                .block_count
                .checked_add(extent.block_count)
                .ok_or(XfsError::AddressOutOfRange)?;
        } else {
            merged.push(extent);
        }
    }
    *extents = merged;
    Ok(())
}

pub(super) fn xfs_planner_for<'a>(
    volume: &XfsVolume,
    planners: &'a mut Vec<XfsAgMutationPlanner>,
    physical: u64,
) -> XfsResult<&'a mut XfsAgMutationPlanner> {
    let ag = u32::try_from(physical / u64::from(volume.superblock.ag_blocks))
        .map_err(|_| XfsError::AddressOutOfRange)?;
    if let Some(index) = planners.iter().position(|entry| entry.ag == ag) {
        return Ok(&mut planners[index]);
    }
    planners.try_reserve(1).map_err(|_| XfsError::NoMemory)?;
    planners.push(XfsAgMutationPlanner::new(volume, ag)?);
    planners.last_mut().ok_or(XfsError::NoMemory)
}
