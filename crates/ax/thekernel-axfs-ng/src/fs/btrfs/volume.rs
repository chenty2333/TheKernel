use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};

use axdriver::{BlockVolume, BlockVolumeDevice, MemberMap, SharedBlockDevice};
use axerrno::{AxError, AxResult};

use super::{BTRFS_SUPERBLOCK_SIZE, BtrfsSuperblock, BtrfsTreeBlock, Checksum, ChecksumType};

/// Fixed on-media locations of redundant Btrfs superblock copies.
const SUPERBLOCK_OFFSETS: [u64; 3] = [64 * 1024, 64 * 1024 * 1024, 256 * 1024 * 1024 * 1024];
const CHUNK_HEADER_BYTES: usize = 48;
const STRIPE_BYTES: usize = 32;
const DISK_KEY_BYTES: usize = 17;
const CHUNK_ITEM_KEY: u8 = 228;
const PROFILE_MASK: u64 = (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8);
const BLOCK_GROUP_DATA: u64 = 1;
const BLOCK_GROUP_SYSTEM: u64 = 2;
const BLOCK_GROUP_METADATA: u64 = 4;

/// Native Btrfs allocation profile.  Parity writes use full stripe-set
/// read-modify-write with regenerated P/Q; their transaction caller still
/// owns the final flush and metadata root publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkProfile {
    Single,
    Dup,
    Raid0,
    Raid1,
    Raid10,
    Raid5,
    Raid6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stripe {
    pub device: usize,
    pub physical: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub stripe_len: u64,
    pub profile: ChunkProfile,
    pub sub_stripes: u16,
    /// Btrfs block-group class bits (data/system/metadata), kept separate
    /// from RAID profile bits so metadata COW allocation cannot drift into a
    /// data-only chunk.
    pub block_group_flags: u64,
    pub stripes: Vec<Stripe>,
}

impl Chunk {
    /// Decodes one chunk-tree item and resolves its on-media device IDs into
    /// the fixed member indices of this mount.  An unknown device cannot be
    /// redirected to member zero: it makes the chunk unusable until the
    /// complete multi-device set is supplied.
    pub fn decode_item(
        logical: u64,
        bytes: &[u8],
        mut resolve_device: impl FnMut(u64) -> Option<usize>,
    ) -> AxResult<Self> {
        if bytes.len() < CHUNK_HEADER_BYTES {
            return Err(AxError::Io);
        }
        let length = le64(bytes, 0)?;
        let stripe_len = le64(bytes, 16)?;
        let type_flags = le64(bytes, 24)?;
        let profile = decode_profile(type_flags & PROFILE_MASK)?;
        let block_group_flags =
            type_flags & (BLOCK_GROUP_DATA | BLOCK_GROUP_SYSTEM | BLOCK_GROUP_METADATA);
        if block_group_flags == 0 {
            return Err(AxError::Io);
        }
        let num_stripes = usize::from(le16(bytes, 44)?);
        let sub_stripes = le16(bytes, 46)?;
        if num_stripes == 0
            || bytes.len()
                != CHUNK_HEADER_BYTES
                    .checked_add(num_stripes.checked_mul(STRIPE_BYTES).ok_or(AxError::Io)?)
                    .ok_or(AxError::Io)?
        {
            return Err(AxError::Io);
        }
        let mut stripes = Vec::new();
        stripes
            .try_reserve_exact(num_stripes)
            .map_err(|_| AxError::NoMemory)?;
        for index in 0..num_stripes {
            let offset = CHUNK_HEADER_BYTES + index * STRIPE_BYTES;
            let devid = le64(bytes, offset)?;
            let physical = le64(bytes, offset + 8)?;
            let device = resolve_device(devid).ok_or(AxError::NoSuchDevice)?;
            stripes.push(Stripe { device, physical });
        }
        Ok(Self {
            logical,
            length,
            stripe_len,
            profile,
            sub_stripes,
            block_group_flags,
            stripes,
        })
    }

    /// Serializes the typed CHUNK_ITEM payload.  Device identity is supplied
    /// by the transaction's checked member map; positional stripe indices are
    /// never emitted as a synthetic devid.
    #[allow(dead_code)]
    pub fn encode_item(
        &self,
        mut device_id: impl FnMut(usize) -> Option<u64>,
    ) -> AxResult<Vec<u8>> {
        if self.stripes.is_empty() || self.stripes.len() > u16::MAX as usize {
            return Err(AxError::InvalidInput);
        }
        let mut output = Vec::new();
        let length = CHUNK_HEADER_BYTES
            .checked_add(
                self.stripes
                    .len()
                    .checked_mul(STRIPE_BYTES)
                    .ok_or(AxError::NoMemory)?,
            )
            .ok_or(AxError::NoMemory)?;
        output
            .try_reserve_exact(length)
            .map_err(|_| AxError::NoMemory)?;
        output.extend_from_slice(&self.length.to_le_bytes());
        output.extend_from_slice(&0u64.to_le_bytes()); // owner
        output.extend_from_slice(&self.stripe_len.to_le_bytes());
        output.extend_from_slice(
            &(self.block_group_flags | profile_bits(self.profile)).to_le_bytes(),
        );
        output.extend_from_slice(&0u32.to_le_bytes()); // io_align
        output.extend_from_slice(&0u32.to_le_bytes()); // io_width
        output.extend_from_slice(&0u32.to_le_bytes()); // sector_size
        output.extend_from_slice(
            &u16::try_from(self.stripes.len())
                .map_err(|_| AxError::InvalidInput)?
                .to_le_bytes(),
        );
        output.extend_from_slice(&self.sub_stripes.to_le_bytes());
        for stripe in &self.stripes {
            let devid = device_id(stripe.device).ok_or(AxError::NoSuchDevice)?;
            output.extend_from_slice(&devid.to_le_bytes());
            output.extend_from_slice(&stripe.physical.to_le_bytes());
            output.extend_from_slice(&[0; 16]); // dev UUID belongs to checked device item; preserve no invented identity
        }
        Ok(output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StripeRead {
    pub device: usize,
    pub physical: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StripeWrite {
    pub device: usize,
    pub physical: u64,
    pub len: u64,
}

/// Result of a checksum-tree guided scrub of one extent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
// Scrub reporting API kept for the in-progress scrub path.
#[allow(dead_code)]
pub struct ScrubReport {
    pub checked_mirrors: u16,
    pub bad_mirrors: u16,
    pub repaired_mirrors: u16,
}

/// Checked logical-to-physical mapping layered over the generic multi-device
/// block volume.  The constructor makes chunk overlap and device identity
/// errors impossible to defer until an I/O path.
pub struct BtrfsVolume {
    volume: BlockVolume,
    chunks: Vec<Chunk>,
    members: BTreeMap<u64, usize>,
}

/// An unpublished device routing and chunk-map replacement.  The candidate
/// owns neither mounted state nor a visibility side effect: tree writers may
/// reject it, and a failed superblock publication simply drops it.  Only a
/// successful topology transaction may call `publish_staged_topology`.
pub struct BtrfsTopologyStage {
    routing: Arc<MemberMap>,
    writer: BlockVolume,
    chunks: Vec<Chunk>,
    members: BTreeMap<u64, usize>,
}

/// One explicit member-map transition.  Device replacement intentionally
/// retains its `devid`; changing that identity is a remove/add operation and
/// must be reflected by the chunk/device trees rather than hidden under an
/// existing stripe reference.
// The mutating transitions are admitted with the in-progress device
// add/remove/replace path.
#[allow(dead_code)]
pub enum BtrfsDeviceTopologyChange {
    Keep,
    Add {
        devid: u64,
        device: SharedBlockDevice,
    },
    Remove {
        devid: u64,
    },
    Replace {
        devid: u64,
        device: SharedBlockDevice,
    },
}

impl BtrfsVolume {
    pub const CHUNK_ITEM_TYPE: u8 = CHUNK_ITEM_KEY;
    /// Reads every usable superblock mirror from every member and returns the
    /// newest valid copy.  A corrupt mirror is ignored only after its own
    /// checksum/address validation fails; an empty result is a hard I/O
    /// failure, never an implicit freshly formatted filesystem.
    pub fn discover_superblock(volume: &BlockVolume) -> AxResult<BtrfsSuperblock> {
        let block_size = volume.geometry().block_size as u64;
        if BTRFS_SUPERBLOCK_SIZE as u64 % block_size != 0 {
            return Err(AxError::Unsupported);
        }
        let mut selected: Option<BtrfsSuperblock> = None;
        for device in volume.devices() {
            for &offset in &SUPERBLOCK_OFFSETS {
                let bytes = offset
                    .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
                    .ok_or(AxError::Io)?;
                if bytes > device.geometry.blocks.saturating_mul(block_size) {
                    continue;
                }
                let mut buffer = Vec::new();
                buffer
                    .try_reserve_exact(BTRFS_SUPERBLOCK_SIZE)
                    .map_err(|_| AxError::NoMemory)?;
                buffer.resize(BTRFS_SUPERBLOCK_SIZE, 0);
                let start = device
                    .volume_start
                    .checked_add(offset / block_size)
                    .ok_or(AxError::Io)?;
                if volume.read_blocks(start, &mut buffer).is_err() {
                    continue;
                }
                let Ok(candidate) = BtrfsSuperblock::decode(&buffer, offset) else {
                    continue;
                };
                match selected {
                    // A caller handed us a multi-device set.  Mixing two
                    // filesystem UUIDs is never a recoverable mirror error:
                    // accepting whichever one happened to be scanned first
                    // could write into an unrelated filesystem.
                    Some(current) if current.fsid != candidate.fsid => return Err(AxError::Io),
                    Some(current) if current.generation >= candidate.generation => {}
                    _ => selected = Some(candidate),
                }
            }
        }
        selected.ok_or(AxError::Io)
    }

    // Convenience constructor kept for the in-progress device-topology path.
    #[allow(dead_code)]
    pub fn new(volume: BlockVolume, chunks: Vec<Chunk>) -> AxResult<Self> {
        Self::new_with_members(volume, chunks, BTreeMap::new())
    }

    fn new_with_members(
        volume: BlockVolume,
        mut chunks: Vec<Chunk>,
        members: BTreeMap<u64, usize>,
    ) -> AxResult<Self> {
        chunks.sort_by_key(|chunk| chunk.logical);
        let devices: Vec<BlockVolumeDevice> = volume.devices().collect();
        let mut previous_end = 0;
        for chunk in &chunks {
            validate_chunk(chunk, &devices)?;
            if chunk.logical < previous_end {
                return Err(AxError::InvalidInput);
            }
            previous_end = chunk
                .logical
                .checked_add(chunk.length)
                .ok_or(AxError::InvalidInput)?;
        }
        Ok(Self {
            volume,
            chunks,
            members,
        })
    }

    /// Builds the initial logical map from the superblock's system-chunk
    /// array.  This is the only map available before the chunk tree itself
    /// can be read.  A single-device mount never aliases an unknown `devid`
    /// to its supplied member.
    pub fn bootstrap_single(volume: BlockVolume, superblock: &BtrfsSuperblock) -> AxResult<Self> {
        let mut members = BTreeMap::new();
        members.insert(superblock.devid, 0);
        Self::bootstrap_with_members(volume, superblock, members)
    }

    /// Builds the system-chunk bootstrap map after each supplied member's
    /// superblock has been checked for the selected filesystem UUID.  A
    /// missing `devid` remains a hard admission failure when a chunk refers
    /// to it; there is no positional fallback in a multi-device filesystem.
    pub fn bootstrap_multi(volume: BlockVolume, superblock: &BtrfsSuperblock) -> AxResult<Self> {
        let mut members = BTreeMap::new();
        let block_size = volume.geometry().block_size as u64;
        for device in volume.devices() {
            let offset = SUPERBLOCK_OFFSETS[0];
            if offset
                .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
                .ok_or(AxError::Io)?
                > device.geometry.blocks.saturating_mul(block_size)
            {
                continue;
            }
            let mut image = Vec::new();
            image
                .try_reserve_exact(BTRFS_SUPERBLOCK_SIZE)
                .map_err(|_| AxError::NoMemory)?;
            image.resize(BTRFS_SUPERBLOCK_SIZE, 0);
            volume
                .read_blocks(
                    device
                        .volume_start
                        .checked_add(offset / block_size)
                        .ok_or(AxError::Io)?,
                    &mut image,
                )
                .map_err(|_| AxError::Io)?;
            let candidate = BtrfsSuperblock::decode(&image, offset)?;
            if candidate.fsid != superblock.fsid {
                return Err(AxError::Io);
            }
            if members.insert(candidate.devid, device.index).is_some() {
                return Err(AxError::Io);
            }
        }
        if members.is_empty() {
            return Err(AxError::NoSuchDevice);
        }
        Self::bootstrap_with_members(volume, superblock, members)
    }

    fn bootstrap_with_members(
        volume: BlockVolume,
        superblock: &BtrfsSuperblock,
        members: BTreeMap<u64, usize>,
    ) -> AxResult<Self> {
        let bytes = superblock.system_chunk_array();
        let mut cursor = 0usize;
        let mut chunks = Vec::new();
        while cursor < bytes.len() {
            let key_end = cursor.checked_add(DISK_KEY_BYTES).ok_or(AxError::Io)?;
            let key = bytes.get(cursor..key_end).ok_or(AxError::Io)?;
            let logical = le64(key, 0)?;
            if key[8] != CHUNK_ITEM_KEY {
                return Err(AxError::Io);
            }
            let header = bytes
                .get(key_end..key_end.checked_add(CHUNK_HEADER_BYTES).ok_or(AxError::Io)?)
                .ok_or(AxError::Io)?;
            let stripes = usize::from(le16(header, 44)?);
            if stripes == 0 {
                return Err(AxError::Io);
            }
            let item_len = CHUNK_HEADER_BYTES
                .checked_add(stripes.checked_mul(STRIPE_BYTES).ok_or(AxError::Io)?)
                .ok_or(AxError::Io)?;
            let item_end = key_end.checked_add(item_len).ok_or(AxError::Io)?;
            let item = bytes.get(key_end..item_end).ok_or(AxError::Io)?;
            let chunk = Chunk::decode_item(logical, item, |devid| members.get(&devid).copied())?;
            chunks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            chunks.push(chunk);
            cursor = item_end;
        }
        Self::new_with_members(volume, chunks, members)
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Replaces the bootstrap system-chunk map after the checked chunk tree
    /// has been walked.  Keeping this consuming operation inside the volume
    /// type prevents a caller from retaining a system-only map while claiming
    /// that normal filesystem/data chunks are reachable.
    pub fn with_chunk_tree(self, chunks: Vec<Chunk>) -> AxResult<Self> {
        Self::new_with_members(self.volume, chunks, self.members)
    }

    pub fn member_index(&self, devid: u64) -> Option<usize> {
        self.members.get(&devid).copied()
    }
    // Device-topology API kept for the in-progress device add/remove path.
    #[allow(dead_code)]
    pub fn member_devid(&self, index: usize) -> Option<u64> {
        self.members
            .iter()
            .find_map(|(&devid, &member)| (member == index).then_some(devid))
    }

    /// Stages a complete member routing table and its corresponding in-memory
    /// chunk index.  It deliberately rejects removal while a chunk still
    /// names the device: evacuation must first COW-relocate every block group
    /// and publish that new chunk tree.  Existing I/O retains the old
    /// `BlockVolume` map until the caller publishes this stage.
    pub fn stage_member_change(
        &self,
        change: BtrfsDeviceTopologyChange,
    ) -> AxResult<BtrfsTopologyStage> {
        let mut queues = self.volume.member_queues();
        let mut by_index = vec![0u64; queues.len()];
        for (&devid, &index) in &self.members {
            *by_index.get_mut(index).ok_or(AxError::Io)? = devid;
        }
        if by_index.iter().any(|devid| *devid == 0) {
            return Err(AxError::Io);
        }

        match change {
            BtrfsDeviceTopologyChange::Keep => {}
            BtrfsDeviceTopologyChange::Add { devid, device } => {
                if devid == 0
                    || self.members.contains_key(&devid)
                    || queues
                        .iter()
                        .any(|queue| queue.identity_token() == device.identity_token())
                {
                    return Err(AxError::InvalidInput);
                }
                queues.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                by_index.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                queues.push(device);
                by_index.push(devid);
            }
            BtrfsDeviceTopologyChange::Remove { devid } => {
                let index = self.member_index(devid).ok_or(AxError::NoSuchDevice)?;
                if queues.len() <= 1 {
                    return Err(AxError::ResourceBusy);
                }
                queues.remove(index);
                by_index.remove(index);
            }
            BtrfsDeviceTopologyChange::Replace { devid, device } => {
                let index = self.member_index(devid).ok_or(AxError::NoSuchDevice)?;
                if queues.iter().enumerate().any(|(other, queue)| {
                    other != index && queue.identity_token() == device.identity_token()
                }) {
                    return Err(AxError::InvalidInput);
                }
                queues[index] = device;
            }
        }
        // A private candidate volume reaches both newly admitted and existing
        // members while the mount's published routing table remains old.
        // This is what lets topology tree writes and a new member's initial
        // superblock be durable before any new I/O can select that member.
        let writer = BlockVolume::new(queues.clone()).map_err(|_| AxError::Io)?;
        let routing = BlockVolume::stage_member_map(queues).map_err(|_| AxError::Io)?;
        // The caller supplies its fully rewritten CHUNK tree immediately
        // after staging.  Do not carry old chunks across a removal/replacement
        // candidate: doing so would either retain a removed stripe or silently
        // retarget it before relocation has been made durable.
        let chunks = Vec::new();
        let mut members = BTreeMap::new();
        for (index, devid) in by_index.into_iter().enumerate() {
            if members.insert(devid, index).is_some() {
                return Err(AxError::Io);
            }
        }
        Ok(BtrfsTopologyStage {
            routing,
            writer,
            chunks,
            members,
        })
    }

    /// Publishes the device routing map only after the caller has durably
    /// committed the chunk tree and every affected superblock mirror.  This
    /// is intentionally `&mut self`, preventing a mount from exposing a new
    /// map while retaining an old in-memory chunk/devid relation.
    pub fn publish_staged_topology(&mut self, stage: BtrfsTopologyStage) {
        self.volume.publish_member_map(stage.routing);
        self.chunks = stage.chunks;
        self.members = stage.members;
    }

    // Device-topology API kept for the in-progress device add/remove path.
    #[allow(dead_code)]
    pub fn stage_member_index(stage: &BtrfsTopologyStage, devid: u64) -> Option<usize> {
        stage.members.get(&devid).copied()
    }

    /// Final checked chunk map carried by an unpublished topology stage.
    /// Tree writers use it to build per-block-group free-space accounting
    /// before this routing/chunk map becomes visible.
    pub fn staged_chunks(stage: &BtrfsTopologyStage) -> &[Chunk] {
        &stage.chunks
    }

    // Device-topology API kept for the in-progress device add/remove path.
    #[allow(dead_code)]
    pub fn staged_member_has_stripes(stage: &BtrfsTopologyStage, devid: u64) -> AxResult<bool> {
        let index = Self::stage_member_index(stage, devid).ok_or(AxError::NoSuchDevice)?;
        Ok(stage
            .chunks
            .iter()
            .any(|chunk| chunk.stripes.iter().any(|stripe| stripe.device == index)))
    }

    /// Replaces the candidate logical chunk map after the caller rebuilt the
    /// CHUNK tree image.  This validates all stripe indices against the
    /// staged, not currently published, routing table.
    pub fn stage_chunks(stage: &mut BtrfsTopologyStage, mut chunks: Vec<Chunk>) -> AxResult<()> {
        chunks.sort_by_key(|chunk| chunk.logical);
        let devices = BlockVolume::staged_devices(&stage.routing);
        let mut previous_end = 0;
        for chunk in &chunks {
            validate_chunk(chunk, &devices)?;
            if chunk.logical < previous_end {
                return Err(AxError::InvalidInput);
            }
            previous_end = chunk
                .logical
                .checked_add(chunk.length)
                .ok_or(AxError::InvalidInput)?;
        }
        stage.chunks = chunks;
        Ok(())
    }

    /// Verifies the bootstrap array against the *final* candidate chunk map.
    /// A topology commit must never publish an old system stripe which refers
    /// to a removed member merely because the normal chunk tree is correct.
    pub fn validate_staged_system_chunks(stage: &BtrfsTopologyStage, bytes: &[u8]) -> AxResult<()> {
        if bytes.len() > super::BtrfsSuperblock::system_chunk_array_capacity() {
            return Err(AxError::InvalidInput);
        }
        let mut cursor = 0usize;
        let mut bootstrap = Vec::new();
        while cursor < bytes.len() {
            let key_end = cursor.checked_add(DISK_KEY_BYTES).ok_or(AxError::Io)?;
            let key = bytes.get(cursor..key_end).ok_or(AxError::Io)?;
            if key[8] != CHUNK_ITEM_KEY {
                return Err(AxError::Io);
            }
            let logical = le64(key, 0)?;
            let header_end = key_end.checked_add(CHUNK_HEADER_BYTES).ok_or(AxError::Io)?;
            let header = bytes.get(key_end..header_end).ok_or(AxError::Io)?;
            let stripes = usize::from(le16(header, 44)?);
            let item_len = CHUNK_HEADER_BYTES
                .checked_add(stripes.checked_mul(STRIPE_BYTES).ok_or(AxError::Io)?)
                .ok_or(AxError::Io)?;
            let end = key_end.checked_add(item_len).ok_or(AxError::Io)?;
            let item = bytes.get(key_end..end).ok_or(AxError::Io)?;
            let chunk =
                Chunk::decode_item(logical, item, |devid| stage.members.get(&devid).copied())?;
            if chunk.block_group_flags & BLOCK_GROUP_SYSTEM == 0
                || !stage.chunks.iter().any(|final_chunk| final_chunk == &chunk)
            {
                return Err(AxError::Io);
            }
            bootstrap.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            bootstrap.push(chunk);
            cursor = end;
        }
        let final_systems = stage
            .chunks
            .iter()
            .filter(|chunk| chunk.block_group_flags & BLOCK_GROUP_SYSTEM != 0)
            .count();
        if bootstrap.len() != final_systems
            || stage
                .chunks
                .iter()
                .filter(|chunk| chunk.block_group_flags & BLOCK_GROUP_SYSTEM != 0)
                .any(|chunk| {
                    !bootstrap
                        .iter()
                        .any(|bootstrap_chunk| bootstrap_chunk == chunk)
                })
        {
            return Err(AxError::Io);
        }
        Ok(())
    }

    /// Executes an ordered topology-superblock publication through an
    /// unpublished candidate map.  Every member gets its own validated
    /// device-item image; a newly added blank member is initialized from the
    /// current superblock only after the replacement trees are durable.
    pub fn publish_staged_topology_superblocks(
        &self,
        stage: &BtrfsTopologyStage,
        superblock: &BtrfsSuperblock,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        log_root_level: u8,
        bytes_used: u64,
        system_chunks: &[u8],
        device_items: &BTreeMap<u64, super::BtrfsDeviceItem>,
    ) -> AxResult<()> {
        let block_size = stage.writer.geometry().block_size as u64;
        if BTRFS_SUPERBLOCK_SIZE as u64 % block_size != 0
            || device_items.len() != stage.members.len()
        {
            return Err(AxError::InvalidInput);
        }
        let total_bytes = device_items
            .values()
            .try_fold(0u64, |sum, item| sum.checked_add(item.total_bytes))
            .ok_or(AxError::NoMemory)?;
        if bytes_used > total_bytes {
            return Err(AxError::InvalidInput);
        }
        // A member becomes visible in `num_devices` only if it can carry the
        // mandatory 64KiB mirror.  Do this for every candidate before any
        // mirror write, so a too-small add/replace cannot leave a partially
        // initialized published member after a later device iteration.
        let first_mirror_end = SUPERBLOCK_OFFSETS[0]
            .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
            .ok_or(AxError::Io)?;
        // Once a superblock mirror write is attempted, the generation may
        // survive an I/O error or a later flush failure.  From that point
        // this API deliberately reports completion: rolling the caller's
        // allocator back would permit reuse of blocks that a mount after
        // power loss can reach through that mirror.  The remaining mirror
        // failure is a degraded-redundancy condition, never an uncommitted
        // topology transaction.
        let mut possibly_published = false;
        for device in stage.writer.devices() {
            let devid = stage
                .members
                .iter()
                .find_map(|(&devid, &index)| (index == device.index).then_some(devid))
                .ok_or(AxError::Io)?;
            let item = device_items.get(&devid).ok_or(AxError::NoSuchDevice)?;
            if item.total_bytes < first_mirror_end
                || device.geometry.blocks.saturating_mul(block_size) < first_mirror_end
            {
                return Err(AxError::InvalidInput);
            }
        }
        // Complete every fallible image/address calculation before the first
        // submission.  A later allocation or arithmetic error must never
        // escape after a previous mirror may have made this generation live.
        let mut mirror_writes = Vec::new();
        mirror_writes
            .try_reserve(
                stage
                    .writer
                    .devices()
                    .len()
                    .checked_mul(SUPERBLOCK_OFFSETS.len())
                    .ok_or(AxError::NoMemory)?,
            )
            .map_err(|_| AxError::NoMemory)?;
        for device in stage.writer.devices() {
            let devid = stage
                .members
                .iter()
                .find_map(|(&devid, &index)| (index == device.index).then_some(devid))
                .ok_or(AxError::Io)?;
            let item = device_items.get(&devid).ok_or(AxError::NoSuchDevice)?;
            if item.devid != devid
                || item.fsid != superblock.fsid
                || item.total_bytes > device.geometry.blocks.saturating_mul(block_size)
            {
                return Err(AxError::InvalidInput);
            }
            for &offset in &SUPERBLOCK_OFFSETS {
                if offset
                    .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
                    .ok_or(AxError::Io)?
                    > device.geometry.blocks.saturating_mul(block_size)
                {
                    continue;
                }
                let image = superblock.prepare_topology_member_commit(
                    generation,
                    root,
                    chunk_root,
                    log_root,
                    log_root_level,
                    bytes_used,
                    total_bytes,
                    u64::try_from(stage.members.len()).map_err(|_| AxError::NoMemory)?,
                    system_chunks,
                    *item,
                    offset,
                )?;
                let start = device
                    .volume_start
                    .checked_add(offset / block_size)
                    .ok_or(AxError::Io)?;
                mirror_writes
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                mirror_writes.push((start, image));
            }
        }
        if mirror_writes.is_empty() {
            return Err(AxError::InvalidInput);
        }
        for (start, image) in mirror_writes {
            // A block-device error is an acknowledgement failure, not a
            // proof that no sector reached stable media.  Flip this before
            // the first submission: from here the caller must retain every
            // COW and physical reservation even if this write reports error.
            possibly_published = true;
            if stage.writer.write_blocks(start, &image).is_err() {
                return Ok(());
            }
        }
        if stage.writer.flush().is_err() && !possibly_published {
            return Err(AxError::Io);
        }
        Ok(())
    }

    pub fn metadata_contains(&self, logical: u64, len: u64) -> bool {
        self.chunks.iter().any(|chunk| {
            logical >= chunk.logical
                && logical
                    .checked_add(len)
                    .is_some_and(|end| end <= chunk.logical.saturating_add(chunk.length))
                && chunk.block_group_flags & (BLOCK_GROUP_METADATA | BLOCK_GROUP_SYSTEM) != 0
        })
    }

    /// True only for a complete range inside a data block group.  Data COW
    /// allocation must not borrow a metadata/system range merely because it
    /// is addressable through the chunk map.
    pub fn data_contains(&self, logical: u64, len: u64) -> bool {
        self.chunks.iter().any(|chunk| {
            logical >= chunk.logical
                && logical
                    .checked_add(len)
                    .is_some_and(|end| end <= chunk.logical.saturating_add(chunk.length))
                && chunk.block_group_flags & BLOCK_GROUP_DATA != 0
        })
    }

    /// Whether any chunk covering this logical range has a stripe on the
    /// supplied member.  Balance uses the complete range walk rather than a
    /// first-chunk shortcut, because one regular extent may cross a block
    /// group boundary during a prior COW allocation.
    // Volume scrub/relocation API in progress.
    #[allow(dead_code)]
    pub fn logical_range_uses_member(
        &self,
        logical: u64,
        len: u64,
        device: usize,
    ) -> AxResult<bool> {
        if len == 0 {
            return Ok(false);
        }
        let end = logical.checked_add(len).ok_or(AxError::InvalidInput)?;
        let mut cursor = logical;
        while cursor < end {
            let chunk = self
                .chunks
                .iter()
                .find(|chunk| {
                    cursor >= chunk.logical && cursor < chunk.logical.saturating_add(chunk.length)
                })
                .ok_or(AxError::InvalidInput)?;
            if chunk.stripes.iter().any(|stripe| stripe.device == device) {
                return Ok(true);
            }
            cursor = end.min(chunk.logical.checked_add(chunk.length).ok_or(AxError::Io)?);
        }
        Ok(false)
    }

    /// Publishes all redundant superblock copies only after the caller made
    /// the new data/tree/log blocks durable.  This is the final transaction
    /// visibility point; any failed write leaves the prior generation usable
    /// on at least one previously valid mirror and is reported to the caller.
    // Volume scrub/relocation API in progress.
    #[allow(dead_code)]
    pub fn publish_superblock(
        &self,
        superblock: &BtrfsSuperblock,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<()> {
        let block_size = self.volume.geometry().block_size as u64;
        if BTRFS_SUPERBLOCK_SIZE as u64 % block_size != 0 {
            return Err(AxError::Unsupported);
        }
        for device in self.volume.devices() {
            for &offset in &SUPERBLOCK_OFFSETS {
                let end = offset
                    .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
                    .ok_or(AxError::Io)?;
                if end > device.geometry.blocks.saturating_mul(block_size) {
                    continue;
                }
                let start = device
                    .volume_start
                    .checked_add(offset / block_size)
                    .ok_or(AxError::Io)?;
                // A Btrfs member has its own dev_item/devid.  Reusing the
                // selected member's raw superblock for every device silently
                // duplicates that identity and corrupts multi-device mounts.
                // Rebase just the generation/root fields on each validated
                // member image instead.
                let mut member_bytes = Vec::new();
                member_bytes
                    .try_reserve_exact(BTRFS_SUPERBLOCK_SIZE)
                    .map_err(|_| AxError::NoMemory)?;
                member_bytes.resize(BTRFS_SUPERBLOCK_SIZE, 0);
                self.volume
                    .read_blocks(start, &mut member_bytes)
                    .map_err(|_| AxError::Io)?;
                let member = BtrfsSuperblock::decode(&member_bytes, offset)?;
                if member.fsid != superblock.fsid {
                    return Err(AxError::Io);
                }
                let image =
                    member.prepare_commit(generation, root, chunk_root, log_root, bytes_used)?;
                self.volume
                    .write_blocks(start, &image)
                    .map_err(|_| AxError::Io)?;
            }
        }
        self.flush()
    }

    // Volume scrub/relocation API in progress.
    #[allow(dead_code)]
    pub fn publish_topology_superblock(
        &self,
        superblock: &BtrfsSuperblock,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
        num_devices: u64,
        system_chunks: &[u8],
    ) -> AxResult<()> {
        let block_size = self.volume.geometry().block_size as u64;
        if BTRFS_SUPERBLOCK_SIZE as u64 % block_size != 0 {
            return Err(AxError::Unsupported);
        }
        for device in self.volume.devices() {
            for &offset in &SUPERBLOCK_OFFSETS {
                let end = offset
                    .checked_add(BTRFS_SUPERBLOCK_SIZE as u64)
                    .ok_or(AxError::Io)?;
                if end > device.geometry.blocks.saturating_mul(block_size) {
                    continue;
                }
                let start = device
                    .volume_start
                    .checked_add(offset / block_size)
                    .ok_or(AxError::Io)?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(BTRFS_SUPERBLOCK_SIZE)
                    .map_err(|_| AxError::NoMemory)?;
                bytes.resize(BTRFS_SUPERBLOCK_SIZE, 0);
                self.volume
                    .read_blocks(start, &mut bytes)
                    .map_err(|_| AxError::Io)?;
                let member = BtrfsSuperblock::decode(&bytes, offset)?;
                if member.fsid != superblock.fsid {
                    return Err(AxError::Io);
                }
                let image = member.prepare_topology_commit(
                    generation,
                    root,
                    chunk_root,
                    log_root,
                    bytes_used,
                    num_devices,
                    system_chunks,
                )?;
                self.volume
                    .write_blocks(start, &image)
                    .map_err(|_| AxError::Io)?;
            }
        }
        self.flush()
    }

    /// Reads and validates a complete B-tree node through the chunk map.  A
    /// checksum is recovered from each mirror's own header before validation,
    /// so a bad primary can be retried without ever exposing its contents.
    pub fn read_checked_tree_block(
        &self,
        logical: u64,
        nodesize: usize,
        fsid: &[u8; 16],
        checksum_type: ChecksumType,
    ) -> AxResult<Vec<u8>> {
        let block_size = self.volume.geometry().block_size;
        if nodesize < 4096 || nodesize % block_size != 0 {
            return Err(AxError::InvalidInput);
        }
        let candidates = self.mirror_reads(logical, nodesize as u64)?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(nodesize)
            .map_err(|_| AxError::NoMemory)?;
        buffer.resize(nodesize, 0);
        for candidate in candidates {
            if self.read_one(candidate, &mut buffer).is_err() {
                continue;
            }
            let Ok(checksum) = Checksum::from_disk(checksum_type, &buffer[..32]) else {
                continue;
            };
            if BtrfsTreeBlock::decode(&buffer, fsid, checksum, logical).is_ok() {
                return Ok(buffer);
            }
        }
        Err(AxError::Io)
    }

    /// Reads a logical extent, trying every available mirror only after a
    /// complete read/checksum failure.  An absent checksum never silently
    /// turns a failed mirror into valid data.
    pub fn read_verified(
        &self,
        logical: u64,
        out: &mut [u8],
        checksum: Option<Checksum>,
    ) -> AxResult<()> {
        let block_size = self.volume.geometry().block_size as u64;
        if logical % block_size != 0 || (out.len() as u64) % block_size != 0 {
            return Err(AxError::InvalidInput);
        }
        let initial = self
            .find_chunk(logical, out.len() as u64)
            .or_else(|_| self.find_chunk(logical, 1))?;
        let relative = logical.checked_sub(initial.logical).ok_or(AxError::Io)?;
        let crosses_parity_stripe =
            matches!(initial.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6)
                && (out.len() as u64
                    > initial
                        .stripe_len
                        .checked_sub(relative % initial.stripe_len)
                        .ok_or(AxError::Io)?);
        if crosses_parity_stripe {
            // Preserve one end-to-end checksum decision while delegating
            // individual native-stripe reads (and reconstruction retries) to
            // read_logical.  This avoids asking map_primary to represent a
            // range it intentionally rejects.
            self.read_logical(logical, out)?;
            return checksum.map_or(Ok(()), |expected| {
                expected.verify(out).then_some(()).ok_or(AxError::Io)
            });
        }
        let reads = self.read_plan(logical, out.len() as u64)?;
        if reads.is_empty() {
            return Ok(());
        }
        // Extents crossing chunks are resolved separately.  A mirror retry is
        // only meaningful for a single physical extent, so multi-chunk input
        // is read deterministically without declaring it checksum-verified.
        if reads.len() != 1 && checksum.is_some() {
            return Err(AxError::InvalidInput);
        }
        let read = reads[0];
        let mut first = Vec::new();
        first
            .try_reserve_exact(out.len())
            .map_err(|_| AxError::NoMemory)?;
        first.resize(out.len(), 0);
        if self.read_one(read, &mut first).is_ok()
            && checksum.map_or(true, |expected| expected.verify(&first))
        {
            out.copy_from_slice(&first);
            return Ok(());
        }
        let chunk = self.find_chunk(logical, out.len() as u64)?;
        if matches!(chunk.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6) {
            let mut rebuilt = Vec::new();
            rebuilt
                .try_reserve_exact(out.len())
                .map_err(|_| AxError::NoMemory)?;
            rebuilt.resize(out.len(), 0);
            if self
                .reconstruct_parity_data(chunk, logical - chunk.logical, &mut rebuilt)
                .is_ok()
                && checksum.map_or(true, |expected| expected.verify(&rebuilt))
            {
                out.copy_from_slice(&rebuilt);
                return Ok(());
            }
        }
        for candidate in self.mirror_reads(logical, out.len() as u64)? {
            if candidate == read {
                continue;
            }
            if self.read_one(candidate, &mut first).is_ok()
                && checksum.map_or(true, |expected| expected.verify(&first))
            {
                out.copy_from_slice(&first);
                return Ok(());
            }
        }
        Err(AxError::Io)
    }

    /// Reads an arbitrary logical byte range without weakening mirror
    /// validation of metadata callers.  Data I/O is split at chunk boundaries
    /// and rounded only inside private temporary buffers, so the public file
    /// path never assumes a userspace buffer is sector-aligned.
    pub fn read_logical(&self, logical: u64, out: &mut [u8]) -> AxResult<()> {
        if out.is_empty() {
            return Ok(());
        }
        let block = self.volume.geometry().block_size as u64;
        let mut logical_cursor = logical;
        let mut output_cursor = 0usize;
        while output_cursor < out.len() {
            let chunk = self
                .chunks
                .iter()
                .find(|chunk| {
                    logical_cursor >= chunk.logical && logical_cursor < chunk.logical + chunk.length
                })
                .ok_or(AxError::Io)?;
            let chunk_remaining = chunk
                .logical
                .checked_add(chunk.length)
                .ok_or(AxError::Io)?
                .checked_sub(logical_cursor)
                .ok_or(AxError::Io)?;
            let mut requested = u64::try_from(out.len() - output_cursor)
                .map_err(|_| AxError::Io)?
                .min(chunk_remaining);
            if matches!(chunk.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6) {
                let relative = logical_cursor
                    .checked_sub(chunk.logical)
                    .ok_or(AxError::Io)?;
                requested = requested.min(
                    chunk
                        .stripe_len
                        .checked_sub(relative % chunk.stripe_len)
                        .ok_or(AxError::Io)?,
                );
            }
            let aligned_start = logical_cursor / block * block;
            let prefix =
                usize::try_from(logical_cursor - aligned_start).map_err(|_| AxError::Io)?;
            let needed = prefix
                .checked_add(usize::try_from(requested).map_err(|_| AxError::Io)?)
                .ok_or(AxError::Io)?;
            let aligned_len = ((u64::try_from(needed).map_err(|_| AxError::Io)? + block - 1)
                / block)
                .checked_mul(block)
                .ok_or(AxError::Io)?;
            let mut scratch = Vec::new();
            scratch
                .try_reserve_exact(usize::try_from(aligned_len).map_err(|_| AxError::NoMemory)?)
                .map_err(|_| AxError::NoMemory)?;
            scratch.resize(
                usize::try_from(aligned_len).map_err(|_| AxError::NoMemory)?,
                0,
            );
            self.read_verified(aligned_start, &mut scratch, None)?;
            let end = prefix
                .checked_add(usize::try_from(requested).map_err(|_| AxError::Io)?)
                .ok_or(AxError::Io)?;
            out[output_cursor..output_cursor + requested as usize]
                .copy_from_slice(&scratch[prefix..end]);
            logical_cursor = logical_cursor.checked_add(requested).ok_or(AxError::Io)?;
            output_cursor += requested as usize;
        }
        Ok(())
    }

    /// Produces all physical destinations for a write.  Callers perform the
    /// transaction's ordered data writes, tree writes, then final flush; this
    /// method does not pretend that normal writes are durable.
    pub fn write_plan(&self, logical: u64, len: u64) -> AxResult<Vec<StripeWrite>> {
        let chunk = self.find_chunk(logical, len)?;
        let relative = logical - chunk.logical;
        let primary = map_primary(chunk, relative, len)?;
        let mut output = Vec::new();
        match chunk.profile {
            ChunkProfile::Single | ChunkProfile::Raid0 => output.push(StripeWrite {
                device: primary.device,
                physical: primary.physical,
                len,
            }),
            ChunkProfile::Dup | ChunkProfile::Raid1 => {
                for stripe in &chunk.stripes {
                    output.push(StripeWrite {
                        device: stripe.device,
                        physical: stripe
                            .physical
                            .checked_add(relative)
                            .ok_or(AxError::InvalidInput)?,
                        len,
                    });
                }
            }
            ChunkProfile::Raid10 => {
                let group_width = usize::from(chunk.sub_stripes);
                let group = raid10_group(chunk, relative)?;
                for stripe in &chunk.stripes[group * group_width..(group + 1) * group_width] {
                    output.push(StripeWrite {
                        device: stripe.device,
                        physical: stripe
                            .physical
                            .checked_add(raid10_offset(chunk, relative)?)
                            .ok_or(AxError::InvalidInput)?,
                        len,
                    });
                }
            }
            ChunkProfile::Raid5 | ChunkProfile::Raid6 => return Err(AxError::Unsupported),
        }
        Ok(output)
    }

    pub fn read_plan(&self, logical: u64, len: u64) -> AxResult<Vec<StripeRead>> {
        let chunk = self.find_chunk(logical, len)?;
        let primary = map_primary(chunk, logical - chunk.logical, len)?;
        Ok(Vec::from([primary]))
    }

    /// Writes a complete logical extent to every profile-required mirror.
    /// When `durable` is requested, success means the volume-wide flush has
    /// completed after all writes; a device which cannot flush is rejected
    /// rather than being treated as a stable transaction target.
    pub fn write_mirrors(&self, logical: u64, data: &[u8], durable: bool) -> AxResult<Checksum> {
        let block_size = self.volume.geometry().block_size as u64;
        if logical % block_size != 0 || (data.len() as u64) % block_size != 0 {
            return Err(AxError::InvalidInput);
        }
        let chunk = self.find_chunk(logical, data.len() as u64)?;
        if matches!(chunk.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6) {
            self.write_parity_range(chunk, logical - chunk.logical, data)?;
            if durable {
                self.volume.flush().map_err(|_| AxError::Io)?;
            }
            return Ok(Checksum::crc32c(super::crc32c(data)));
        }
        let writes = self.write_plan(logical, data.len() as u64)?;
        for write in writes {
            self.write_one(write, data)?;
        }
        if durable {
            self.volume.flush().map_err(|_| AxError::Io)?;
        }
        Ok(Checksum::crc32c(super::crc32c(data)))
    }

    /// Writes one logical data range without imposing the internal stripe
    /// boundary on a file-extent caller.  Every sub-write remains a complete
    /// sector range and is mirrored according to its own chunk/profile plan;
    /// the caller retains the final transaction flush boundary.
    pub fn write_data_range(&self, logical: u64, data: &[u8]) -> AxResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let block = self.volume.geometry().block_size as u64;
        if logical % block != 0 || (data.len() as u64) % block != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut address = logical;
        let mut cursor = 0usize;
        while cursor < data.len() {
            let chunk = self
                .chunks
                .iter()
                .find(|chunk| {
                    address >= chunk.logical && address < chunk.logical.saturating_add(chunk.length)
                })
                .ok_or(AxError::InvalidInput)?;
            let relative = address.checked_sub(chunk.logical).ok_or(AxError::Io)?;
            let chunk_remaining = chunk.length.checked_sub(relative).ok_or(AxError::Io)?;
            let stripe_remaining = chunk
                .stripe_len
                .checked_sub(relative % chunk.stripe_len)
                .ok_or(AxError::Io)?;
            let remaining = u64::try_from(data.len() - cursor).map_err(|_| AxError::Io)?;
            let count = remaining.min(chunk_remaining).min(stripe_remaining);
            let count = count / block * block;
            if count == 0 {
                return Err(AxError::Io);
            }
            let end = cursor
                .checked_add(usize::try_from(count).map_err(|_| AxError::Io)?)
                .ok_or(AxError::Io)?;
            self.write_mirrors(address, &data[cursor..end], false)?;
            address = address.checked_add(count).ok_or(AxError::Io)?;
            cursor = end;
        }
        Ok(())
    }

    /// Writes a freshly allocated COW tree node to all required mirrors.  The
    /// caller publishes its parent/root only after this succeeds.
    pub fn write_tree_block(&self, logical: u64, image: &[u8]) -> AxResult<Checksum> {
        self.write_mirrors(logical, image, false)
    }

    /// Issues the final ordered persistence barrier for a transaction that
    /// already wrote its data, delayed-ref tree nodes, log tree, and root.
    pub fn flush(&self) -> AxResult<()> {
        self.volume.flush().map_err(|_| AxError::Io)
    }

    /// Scrubs one checksum-covered extent and, when requested, repairs only
    /// mirrors that failed verification from a separately verified good copy.
    /// There is no repair path for RAID0 or parity profiles because no valid
    /// redundant source exists in this layer.
    // Volume scrub/relocation API in progress.
    #[allow(dead_code)]
    pub fn scrub_extent(
        &self,
        logical: u64,
        len: usize,
        checksum: Checksum,
        repair: bool,
    ) -> AxResult<ScrubReport> {
        let block_size = self.volume.geometry().block_size;
        if len == 0 || len % block_size != 0 {
            return Err(AxError::InvalidInput);
        }
        let first = self.find_chunk(logical, 1)?;
        let relative = logical.checked_sub(first.logical).ok_or(AxError::Io)?;
        if matches!(first.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6)
            && len as u64
                > first
                    .stripe_len
                    .checked_sub(relative % first.stripe_len)
                    .ok_or(AxError::Io)?
        {
            let mut verified = Vec::new();
            verified
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            verified.resize(len, 0);
            self.read_logical(logical, &mut verified)?;
            if checksum.verify(&verified) {
                return Ok(ScrubReport {
                    checked_mirrors: 1,
                    ..ScrubReport::default()
                });
            }
            if !repair {
                return Err(AxError::Io);
            }
            // An aggregate caller supplied one checksum, so localize a
            // single bad native data stripe by substituting its independent
            // P/Q reconstruction and rechecking the aggregate.  Only a
            // uniquely checksum-confirmed reconstruction is written back;
            // ambiguous/multiple corruption remains EIO rather than guessed.
            let mut cursor = 0usize;
            while cursor < len {
                let address = logical
                    .checked_add(u64::try_from(cursor).map_err(|_| AxError::Io)?)
                    .ok_or(AxError::Io)?;
                let owner = self.find_chunk(address, 1)?;
                let owner_relative = address.checked_sub(owner.logical).ok_or(AxError::Io)?;
                let span = (len - cursor).min(
                    usize::try_from(
                        owner
                            .stripe_len
                            .checked_sub(owner_relative % owner.stripe_len)
                            .ok_or(AxError::Io)?,
                    )
                    .map_err(|_| AxError::Io)?,
                );
                let mut rebuilt = Vec::new();
                rebuilt
                    .try_reserve_exact(span)
                    .map_err(|_| AxError::NoMemory)?;
                rebuilt.resize(span, 0);
                self.reconstruct_parity_data(owner, owner_relative, &mut rebuilt)?;
                let old = verified[cursor..cursor + span].to_vec();
                verified[cursor..cursor + span].copy_from_slice(&rebuilt);
                if checksum.verify(&verified) {
                    self.write_mirrors(address, &rebuilt, false)?;
                    self.flush()?;
                    return Ok(ScrubReport {
                        checked_mirrors: 1,
                        bad_mirrors: 1,
                        repaired_mirrors: 1,
                    });
                }
                verified[cursor..cursor + span].copy_from_slice(&old);
                cursor += span;
            }
            return Err(AxError::Io);
        }
        let chunk = self.find_chunk(logical, len as u64)?;
        if matches!(chunk.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6) {
            let primary = map_primary(chunk, logical - chunk.logical, len as u64)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            bytes.resize(len, 0);
            if self.read_one(primary, &mut bytes).is_ok() && checksum.verify(&bytes) {
                return Ok(ScrubReport {
                    checked_mirrors: 1,
                    ..ScrubReport::default()
                });
            }
            let mut repaired = Vec::new();
            repaired
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            repaired.resize(len, 0);
            self.reconstruct_parity_data(chunk, logical - chunk.logical, &mut repaired)?;
            if !checksum.verify(&repaired) {
                return Err(AxError::Io);
            }
            if repair {
                self.write_one(
                    StripeWrite {
                        device: primary.device,
                        physical: primary.physical,
                        len: primary.len,
                    },
                    &repaired,
                )?;
                self.flush()?;
                return Ok(ScrubReport {
                    checked_mirrors: 1,
                    bad_mirrors: 1,
                    repaired_mirrors: 1,
                });
            }
            return Ok(ScrubReport {
                checked_mirrors: 1,
                bad_mirrors: 1,
                repaired_mirrors: 0,
            });
        }
        let candidates = self.mirror_reads(logical, len as u64)?;
        if candidates.len() < 2 && repair {
            return Err(AxError::Unsupported);
        }
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        scratch.resize(len, 0);
        let mut good = None;
        let mut invalid = Vec::new();
        for candidate in candidates {
            let valid = self.read_one(candidate, &mut scratch).is_ok() && checksum.verify(&scratch);
            if valid {
                if good.is_none() {
                    let mut saved = Vec::new();
                    saved
                        .try_reserve_exact(len)
                        .map_err(|_| AxError::NoMemory)?;
                    saved.extend_from_slice(&scratch);
                    good = Some(saved);
                }
            } else {
                invalid.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                invalid.push(candidate);
            }
        }
        let good = good.ok_or(AxError::Io)?;
        let mut report = ScrubReport {
            checked_mirrors: u16::try_from(invalid.len()).unwrap_or(u16::MAX),
            bad_mirrors: u16::try_from(invalid.len()).unwrap_or(u16::MAX),
            repaired_mirrors: 0,
        };
        // Count valid candidates without relying on a fallible allocation.
        report.checked_mirrors =
            u16::try_from(self.mirror_reads(logical, len as u64)?.len()).unwrap_or(u16::MAX);
        if repair {
            for bad in invalid {
                self.write_one(
                    StripeWrite {
                        device: bad.device,
                        physical: bad.physical,
                        len: bad.len,
                    },
                    &good,
                )?;
                report.repaired_mirrors = report.repaired_mirrors.saturating_add(1);
            }
            if report.repaired_mirrors != 0 {
                self.flush()?;
            }
        }
        Ok(report)
    }

    /// Copies a checksum-verified extent into a freshly reserved destination
    /// chunk.  Balance code records the matching delayed refs in the same
    /// transaction and publishes new tree roots only after this returns.
    // Volume scrub/relocation API in progress.
    #[allow(dead_code)]
    pub fn relocate_verified(
        &self,
        source_logical: u64,
        destination: &BtrfsVolume,
        destination_logical: u64,
        len: usize,
        checksum: Checksum,
    ) -> AxResult<Checksum> {
        let mut data = Vec::new();
        data.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
        data.resize(len, 0);
        self.read_verified(source_logical, &mut data, Some(checksum))?;
        destination.write_mirrors(destination_logical, &data, false)
    }

    fn mirror_reads(&self, logical: u64, len: u64) -> AxResult<Vec<StripeRead>> {
        let chunk = self.find_chunk(logical, len)?;
        let relative = logical - chunk.logical;
        let mut reads = Vec::new();
        match chunk.profile {
            ChunkProfile::Dup | ChunkProfile::Raid1 => {
                for stripe in &chunk.stripes {
                    reads.push(StripeRead {
                        device: stripe.device,
                        physical: stripe
                            .physical
                            .checked_add(relative)
                            .ok_or(AxError::InvalidInput)?,
                        len,
                    });
                }
            }
            ChunkProfile::Raid10 => {
                let width = usize::from(chunk.sub_stripes);
                let group = raid10_group(chunk, relative)?;
                let offset = raid10_offset(chunk, relative)?;
                for stripe in &chunk.stripes[group * width..(group + 1) * width] {
                    reads.push(StripeRead {
                        device: stripe.device,
                        physical: stripe
                            .physical
                            .checked_add(offset)
                            .ok_or(AxError::InvalidInput)?,
                        len,
                    });
                }
            }
            _ => reads.push(map_primary(chunk, relative, len)?),
        }
        Ok(reads)
    }

    /// Read-modify-write one RAID5/6 data stripe.  The public range splitter
    /// calls this with at most one native stripe's worth of data, so every
    /// parity calculation has an unambiguous rotating parity position and
    /// cannot accidentally combine two stripe sets.
    fn write_parity_range(&self, chunk: &Chunk, relative: u64, data: &[u8]) -> AxResult<()> {
        let parity_count = if chunk.profile == ChunkProfile::Raid5 {
            1usize
        } else {
            2usize
        };
        let width = chunk.stripes.len();
        if width <= parity_count || data.is_empty() || data.len() as u64 > chunk.stripe_len {
            return Err(AxError::InvalidInput);
        }
        let (set, data_slot, within, parity) = raid_parity_mapping(chunk, relative, parity_count)?;
        if within
            .checked_add(data.len() as u64)
            .map_or(true, |end| end > chunk.stripe_len)
        {
            return Err(AxError::InvalidInput);
        }
        let stripe_len = usize::try_from(chunk.stripe_len).map_err(|_| AxError::Io)?;
        let physical_offset = set.checked_mul(chunk.stripe_len).ok_or(AxError::Io)?;
        let mut stripes = Vec::new();
        stripes
            .try_reserve_exact(width)
            .map_err(|_| AxError::NoMemory)?;
        for stripe in &chunk.stripes {
            let physical = stripe
                .physical
                .checked_add(physical_offset)
                .ok_or(AxError::Io)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(stripe_len)
                .map_err(|_| AxError::NoMemory)?;
            bytes.resize(stripe_len, 0);
            self.read_one(
                StripeRead {
                    device: stripe.device,
                    physical,
                    len: chunk.stripe_len,
                },
                &mut bytes,
            )?;
            stripes.push(bytes);
        }
        let target = raid_data_physical(width, parity_count, parity, data_slot)?;
        let start = usize::try_from(within).map_err(|_| AxError::Io)?;
        let end = start.checked_add(data.len()).ok_or(AxError::Io)?;
        stripes[target][start..end].copy_from_slice(data);
        let q_parity = (parity_count == 2).then(|| (parity + width - 1) % width);
        let mut p = Vec::new();
        p.try_reserve_exact(stripe_len)
            .map_err(|_| AxError::NoMemory)?;
        p.resize(stripe_len, 0);
        let mut q = Vec::new();
        if q_parity.is_some() {
            q.try_reserve_exact(stripe_len)
                .map_err(|_| AxError::NoMemory)?;
            q.resize(stripe_len, 0);
        }
        for slot in 0..width - parity_count {
            let physical = raid_data_physical(width, parity_count, parity, slot)?;
            for (index, value) in stripes[physical].iter().copied().enumerate() {
                p[index] ^= value;
                if q_parity.is_some() {
                    q[index] ^= gf_mul(value, gf_pow(slot as u8));
                }
            }
        }
        stripes[parity] = p;
        if let Some(q_parity) = q_parity {
            stripes[q_parity] = q;
        }
        for (index, stripe) in chunk.stripes.iter().enumerate() {
            self.write_one(
                StripeWrite {
                    device: stripe.device,
                    physical: stripe
                        .physical
                        .checked_add(physical_offset)
                        .ok_or(AxError::Io)?,
                    len: chunk.stripe_len,
                },
                &stripes[index],
            )?;
        }
        Ok(())
    }

    /// Reconstructs an unavailable parity data stripe.  RAID6 consumes both
    /// native P and Q equations, so one requested data failure plus one
    /// additional failed data/parity member remains recoverable.  Bytes are
    /// accepted only after every surviving member has been read; an
    /// underdetermined stripe set is an I/O error rather than guessed data.
    fn reconstruct_parity_data(
        &self,
        chunk: &Chunk,
        relative: u64,
        out: &mut [u8],
    ) -> AxResult<()> {
        let parity_count = if chunk.profile == ChunkProfile::Raid5 {
            1usize
        } else {
            2usize
        };
        let width = chunk.stripes.len();
        let (set, data_slot, within, parity) = raid_parity_mapping(chunk, relative, parity_count)?;
        if within
            .checked_add(out.len() as u64)
            .map_or(true, |end| end > chunk.stripe_len)
        {
            return Err(AxError::InvalidInput);
        }
        let stripe_len = usize::try_from(chunk.stripe_len).map_err(|_| AxError::Io)?;
        let target = raid_data_physical(width, parity_count, parity, data_slot)?;
        let q_parity = (parity_count == 2).then(|| (parity + width - 1) % width);
        let physical_offset = set.checked_mul(chunk.stripe_len).ok_or(AxError::Io)?;
        let mut stripes = Vec::new();
        stripes
            .try_reserve_exact(width)
            .map_err(|_| AxError::NoMemory)?;
        let mut missing = Vec::new();
        missing
            .try_reserve_exact(2)
            .map_err(|_| AxError::NoMemory)?;
        for (index, stripe) in chunk.stripes.iter().enumerate() {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(stripe_len)
                .map_err(|_| AxError::NoMemory)?;
            bytes.resize(stripe_len, 0);
            if index == target
                || self
                    .read_one(
                        StripeRead {
                            device: stripe.device,
                            physical: stripe
                                .physical
                                .checked_add(physical_offset)
                                .ok_or(AxError::Io)?,
                            len: chunk.stripe_len,
                        },
                        &mut bytes,
                    )
                    .is_err()
            {
                missing.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                missing.push(index);
            }
            stripes.push(bytes);
        }
        if missing.first().copied() != Some(target) || missing.len() > parity_count {
            return Err(AxError::Io);
        }
        let p = parity;
        let q = q_parity;
        let mut p_residual = Vec::new();
        p_residual
            .try_reserve_exact(stripe_len)
            .map_err(|_| AxError::NoMemory)?;
        p_residual.resize(stripe_len, 0);
        let mut q_residual = Vec::new();
        if q.is_some() {
            q_residual
                .try_reserve_exact(stripe_len)
                .map_err(|_| AxError::NoMemory)?;
            q_residual.resize(stripe_len, 0);
        }
        // Build P/Q residuals by cancelling every known data column from its
        // stored parity.  A missing parity member simply leaves that equation
        // unavailable; RAID6 needs only the remaining independent equation.
        if !missing.contains(&p) {
            p_residual.copy_from_slice(&stripes[p]);
        }
        if let Some(q) = q {
            if !missing.contains(&q) {
                q_residual.copy_from_slice(&stripes[q]);
            }
        }
        for slot in 0..width - parity_count {
            let physical = raid_data_physical(width, parity_count, parity, slot)?;
            if missing.contains(&physical) {
                continue;
            }
            let coefficient = gf_pow(slot as u8);
            for index in 0..stripe_len {
                p_residual[index] ^= stripes[physical][index];
                if q.is_some() {
                    q_residual[index] ^= gf_mul(stripes[physical][index], coefficient);
                }
            }
        }
        let target_slot = raid_physical_data_slot(width, parity_count, parity, target)?;
        let recovered = &mut stripes[target];
        match missing.len() {
            1 if !missing.contains(&p) => recovered.copy_from_slice(&p_residual),
            1 if q.is_some() && !missing.contains(&q.unwrap()) => {
                let coefficient = gf_pow(target_slot as u8);
                for (destination, value) in recovered.iter_mut().zip(q_residual) {
                    *destination = gf_div(value, coefficient)?;
                }
            }
            2 if q.is_some() => {
                let other = missing[1];
                if other == p {
                    let coefficient = gf_pow(target_slot as u8);
                    for (destination, value) in recovered.iter_mut().zip(q_residual) {
                        *destination = gf_div(value, coefficient)?;
                    }
                } else if other == q.unwrap() {
                    recovered.copy_from_slice(&p_residual);
                } else {
                    let other_slot = raid_physical_data_slot(width, parity_count, parity, other)?;
                    let a = gf_pow(target_slot as u8);
                    let b = gf_pow(other_slot as u8);
                    let divisor = a ^ b;
                    if divisor == 0 {
                        return Err(AxError::Io);
                    }
                    for index in 0..stripe_len {
                        recovered[index] =
                            gf_div(q_residual[index] ^ gf_mul(b, p_residual[index]), divisor)?;
                    }
                }
            }
            _ => return Err(AxError::Io),
        }
        let start = usize::try_from(within).map_err(|_| AxError::Io)?;
        out.copy_from_slice(&recovered[start..start + out.len()]);
        Ok(())
    }

    fn read_one(&self, read: StripeRead, out: &mut [u8]) -> AxResult<()> {
        let device = self
            .volume
            .devices()
            .find(|device| device.index == read.device)
            .ok_or(AxError::Io)?;
        let block_size = self.volume.geometry().block_size as u64;
        if read.physical % block_size != 0 || read.len != out.len() as u64 {
            return Err(AxError::InvalidInput);
        }
        let start = device
            .volume_start
            .checked_add(read.physical / block_size)
            .ok_or(AxError::Io)?;
        self.volume.read_blocks(start, out).map_err(|_| AxError::Io)
    }

    fn write_one(&self, write: StripeWrite, data: &[u8]) -> AxResult<()> {
        let device = self
            .volume
            .devices()
            .find(|device| device.index == write.device)
            .ok_or(AxError::Io)?;
        let block_size = self.volume.geometry().block_size as u64;
        if write.physical % block_size != 0 || write.len != data.len() as u64 {
            return Err(AxError::InvalidInput);
        }
        let start = device
            .volume_start
            .checked_add(write.physical / block_size)
            .ok_or(AxError::Io)?;
        self.volume
            .write_blocks(start, data)
            .map_err(|_| AxError::Io)
    }

    fn find_chunk(&self, logical: u64, len: u64) -> AxResult<&Chunk> {
        let end = logical.checked_add(len).ok_or(AxError::InvalidInput)?;
        self.chunks
            .iter()
            .find(|chunk| logical >= chunk.logical && end <= chunk.logical + chunk.length)
            .ok_or(AxError::InvalidInput)
    }
}

fn validate_chunk(chunk: &Chunk, devices: &[BlockVolumeDevice]) -> AxResult<()> {
    if chunk.length == 0
        || chunk.stripe_len == 0
        || !chunk.stripe_len.is_power_of_two()
        || chunk.stripes.is_empty()
    {
        return Err(AxError::InvalidInput);
    }
    if matches!(chunk.profile, ChunkProfile::Raid10)
        && (chunk.sub_stripes < 2 || chunk.stripes.len() % usize::from(chunk.sub_stripes) != 0)
    {
        return Err(AxError::InvalidInput);
    }
    if matches!(
        chunk.profile,
        ChunkProfile::Raid0 | ChunkProfile::Raid1 | ChunkProfile::Dup
    ) && chunk.stripes.len() < 2
    {
        return Err(AxError::InvalidInput);
    }
    if matches!(chunk.profile, ChunkProfile::Raid5) && chunk.stripes.len() < 3 {
        return Err(AxError::InvalidInput);
    }
    if matches!(chunk.profile, ChunkProfile::Raid6) && chunk.stripes.len() < 4 {
        return Err(AxError::InvalidInput);
    }
    if matches!(chunk.profile, ChunkProfile::Raid5 | ChunkProfile::Raid6) {
        let parity = if chunk.profile == ChunkProfile::Raid5 {
            1usize
        } else {
            2usize
        };
        let data_columns = chunk
            .stripes
            .len()
            .checked_sub(parity)
            .ok_or(AxError::InvalidInput)?;
        let set_len = chunk
            .stripe_len
            .checked_mul(u64::try_from(data_columns).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        if chunk.length % set_len != 0 {
            return Err(AxError::InvalidInput);
        }
    }
    for stripe in &chunk.stripes {
        let device = devices
            .iter()
            .find(|device| device.index == stripe.device)
            .ok_or(AxError::InvalidInput)?;
        if stripe.physical % device.geometry.block_size as u64 != 0
            || chunk.stripe_len % device.geometry.block_size as u64 != 0
        {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(())
}

fn decode_profile(bits: u64) -> AxResult<ChunkProfile> {
    match bits {
        0 => Ok(ChunkProfile::Single),
        8 => Ok(ChunkProfile::Raid0),
        16 => Ok(ChunkProfile::Raid1),
        32 => Ok(ChunkProfile::Dup),
        64 => Ok(ChunkProfile::Raid10),
        128 => Ok(ChunkProfile::Raid5),
        256 => Ok(ChunkProfile::Raid6),
        _ => Err(AxError::Unsupported),
    }
}
// Writer-side profile encoding kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn profile_bits(profile: ChunkProfile) -> u64 {
    match profile {
        ChunkProfile::Single => 0,
        ChunkProfile::Raid0 => 1 << 3,
        ChunkProfile::Raid1 => 1 << 4,
        ChunkProfile::Dup => 1 << 5,
        ChunkProfile::Raid10 => 1 << 6,
        ChunkProfile::Raid5 => 1 << 7,
        ChunkProfile::Raid6 => 1 << 8,
    }
}
fn le16(bytes: &[u8], offset: usize) -> AxResult<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(AxError::Io)
}
fn le64(bytes: &[u8], offset: usize) -> AxResult<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::Io)
}

fn map_primary(chunk: &Chunk, relative: u64, len: u64) -> AxResult<StripeRead> {
    if len == 0
        || relative
            .checked_add(len)
            .map_or(true, |end| end > chunk.length)
        || relative / chunk.stripe_len != (relative + len - 1) / chunk.stripe_len
    {
        return Err(AxError::InvalidInput);
    }
    let (index, offset) = match chunk.profile {
        ChunkProfile::Single | ChunkProfile::Dup | ChunkProfile::Raid1 => (0, relative),
        ChunkProfile::Raid0 => {
            let stripe = relative / chunk.stripe_len;
            (
                (stripe % chunk.stripes.len() as u64) as usize,
                stripe / chunk.stripes.len() as u64 * chunk.stripe_len
                    + relative % chunk.stripe_len,
            )
        }
        ChunkProfile::Raid10 => {
            let group = raid10_group(chunk, relative)?;
            (
                group * usize::from(chunk.sub_stripes),
                raid10_offset(chunk, relative)?,
            )
        }
        ChunkProfile::Raid5 | ChunkProfile::Raid6 => {
            let parity_count = if chunk.profile == ChunkProfile::Raid5 {
                1
            } else {
                2
            };
            let (set, data_slot, offset, parity) =
                raid_parity_mapping(chunk, relative, parity_count)?;
            let physical =
                raid_data_physical(chunk.stripes.len(), parity_count, parity, data_slot)?;
            (
                physical,
                set.checked_mul(chunk.stripe_len)
                    .and_then(|base| base.checked_add(offset))
                    .ok_or(AxError::InvalidInput)?,
            )
        }
    };
    let stripe = chunk.stripes.get(index).ok_or(AxError::InvalidInput)?;
    Ok(StripeRead {
        device: stripe.device,
        physical: stripe
            .physical
            .checked_add(offset)
            .ok_or(AxError::InvalidInput)?,
        len,
    })
}

fn raid_parity_mapping(
    chunk: &Chunk,
    relative: u64,
    parity_count: usize,
) -> AxResult<(u64, usize, u64, usize)> {
    let width = chunk.stripes.len();
    let data_width = width
        .checked_sub(parity_count)
        .ok_or(AxError::InvalidInput)?;
    if data_width == 0 || chunk.stripe_len == 0 {
        return Err(AxError::InvalidInput);
    }
    let set_width = chunk
        .stripe_len
        .checked_mul(data_width as u64)
        .ok_or(AxError::InvalidInput)?;
    let set = relative / set_width;
    let in_set = relative % set_width;
    let data_slot =
        usize::try_from(in_set / chunk.stripe_len).map_err(|_| AxError::InvalidInput)?;
    let offset = in_set % chunk.stripe_len;
    let parity = (width - 1)
        .checked_sub(usize::try_from(set % width as u64).map_err(|_| AxError::InvalidInput)?)
        .ok_or(AxError::InvalidInput)?;
    Ok((set, data_slot, offset, parity))
}

fn raid_data_physical(
    width: usize,
    parity_count: usize,
    p: usize,
    data_slot: usize,
) -> AxResult<usize> {
    let q = (parity_count == 2).then(|| (p + width - 1) % width);
    let mut seen = 0usize;
    for physical in 0..width {
        if physical == p || q == Some(physical) {
            continue;
        }
        if seen == data_slot {
            return Ok(physical);
        }
        seen += 1;
    }
    Err(AxError::InvalidInput)
}

fn raid_physical_data_slot(
    width: usize,
    parity_count: usize,
    p: usize,
    physical: usize,
) -> AxResult<usize> {
    for slot in 0..width.saturating_sub(parity_count) {
        if raid_data_physical(width, parity_count, p, slot)? == physical {
            return Ok(slot);
        }
    }
    Err(AxError::InvalidInput)
}

fn gf_pow(power: u8) -> u8 {
    let mut value = 1u8;
    for _ in 0..power {
        value = gf_mul(value, 2);
    }
    value
}

fn gf_mul(mut left: u8, mut right: u8) -> u8 {
    let mut result = 0u8;
    while right != 0 {
        if right & 1 != 0 {
            result ^= left;
        }
        let high = left & 0x80;
        left <<= 1;
        if high != 0 {
            left ^= 0x1d;
        }
        right >>= 1;
    }
    result
}

fn gf_div(value: u8, divisor: u8) -> AxResult<u8> {
    if divisor == 0 {
        return Err(AxError::Io);
    }
    // GF(2^8)'s multiplicative group has order 255.  Fermat inversion keeps
    // the recovery path allocation-free and uses the same polynomial as the
    // RAID6 Q generator above.
    let mut power = 254u16;
    let mut base = divisor;
    let mut inverse = 1u8;
    while power != 0 {
        if power & 1 != 0 {
            inverse = gf_mul(inverse, base);
        }
        base = gf_mul(base, base);
        power >>= 1;
    }
    Ok(gf_mul(value, inverse))
}
fn raid10_group(chunk: &Chunk, relative: u64) -> AxResult<usize> {
    let groups = chunk.stripes.len() / usize::from(chunk.sub_stripes);
    if groups == 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(((relative / chunk.stripe_len) % groups as u64) as usize)
}
fn raid10_offset(chunk: &Chunk, relative: u64) -> AxResult<u64> {
    let groups = (chunk.stripes.len() / usize::from(chunk.sub_stripes)) as u64;
    (relative / chunk.stripe_len)
        .checked_div(groups)
        .and_then(|stripe| stripe.checked_mul(chunk.stripe_len))
        .and_then(|base| base.checked_add(relative % chunk.stripe_len))
        .ok_or(AxError::InvalidInput)
}
