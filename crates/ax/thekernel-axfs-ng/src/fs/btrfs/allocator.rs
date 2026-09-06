use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axsync::Mutex;

use super::transaction::LogicalLease;
use super::{BtrfsCore, ChunkProfile};

/// A concrete free-space reservation on one Btrfs device.  The owner must
/// either commit it into the free-space/extent trees or release it; dropping a
/// reservation never silently claims physical media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// Writer-side reservation API kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct AllocatedStripe {
    pub device: usize,
    pub physical: u64,
    pub len: u64,
}

#[derive(Clone, Debug)]
// Writer-side reservation API kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct ChunkReservation {
    pub stripes: Vec<AllocatedStripe>,
    committed: bool,
}

/// Checked relationship between a logical Btrfs chunk and one physical
/// member reservation.  For RAID5/6 `data_stripes` excludes P/Q, so callers
/// cannot accidentally reserve only the logical length on every device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// Writer-side reservation API kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct ChunkReservationGeometry {
    pub stripe_count: usize,
    pub data_stripes: usize,
    pub sub_stripes: u16,
    pub stripe_len: u64,
    pub physical_len: u64,
}

/// Transactional free-space allocator for already-discovered device extents.
/// Every returned RAID profile has a complete physical stripe set; parity
/// chunks reserve data plus P (and Q for RAID6) members in the same mutable
/// free-map transaction as mirrored/striped chunks.
// Writer-side allocator kept for the gated Btrfs COW writer.
#[allow(dead_code)]
pub struct BtrfsAllocator {
    free: Mutex<BTreeMap<usize, BTreeMap<u64, u64>>>,
}

/// Logical-address allocator populated exclusively from checked free-space
/// tree records.  It is separate from physical stripe allocation: a Btrfs
/// writer first reserves a logical extent, then resolves/allocates backing
/// chunk stripes in the same transaction.
pub struct BtrfsLogicalAllocator {
    free: Mutex<BTreeMap<u64, u64>>,
    lease: Option<(Arc<BtrfsCore>, LogicalLease)>,
}

impl Drop for BtrfsLogicalAllocator {
    fn drop(&mut self) {
        if let Some((core, lease)) = &self.lease {
            let _ = core.end_logical_lease(*lease);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalReservation {
    pub logical: u64,
    pub len: u64,
    committed: bool,
}

impl BtrfsLogicalAllocator {
    pub fn new() -> Self {
        Self {
            free: Mutex::new(BTreeMap::new()),
            lease: None,
        }
    }

    /// A mount-owned allocator is lease-aware.  The lease closes the gap
    /// between independent FreeSpace snapshots: a second writer cannot pick
    /// bytes reserved by an uncommitted balance or ordinary COW operation.
    pub fn with_core_lease(core: Arc<BtrfsCore>) -> AxResult<Self> {
        let lease = core.begin_logical_lease()?;
        Ok(Self {
            free: Mutex::new(BTreeMap::new()),
            lease: Some((core, lease)),
        })
    }

    pub fn add_free(&self, logical: u64, len: u64) -> AxResult<()> {
        if logical == 0 || len == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = logical.checked_add(len).ok_or(AxError::Io)?;
        let mut free = self.free.lock();
        let previous = free
            .range(..=logical)
            .next_back()
            .map(|(&start, &size)| (start, size));
        let next = free
            .range(logical..)
            .next()
            .map(|(&start, &size)| (start, size));
        if previous.map_or(false, |(start, size)| {
            start
                .checked_add(size)
                .map_or(true, |old_end| old_end > logical)
        }) || next.map_or(false, |(start, _)| start < end)
        {
            return Err(AxError::Io);
        }
        let (mut start, mut size) = (logical, len);
        if let Some((previous_start, previous_size)) = previous {
            if previous_start.checked_add(previous_size) == Some(logical) {
                free.remove(&previous_start);
                start = previous_start;
                size = size.checked_add(previous_size).ok_or(AxError::Io)?;
            }
        }
        if let Some((next_start, next_size)) = next {
            if end == next_start {
                free.remove(&next_start);
                size = size.checked_add(next_size).ok_or(AxError::Io)?;
            }
        }
        free.insert(start, size);
        Ok(())
    }

    /// Exact free-space-tree image after reservations.  Values are sorted and
    /// non-overlapping, making this suitable for atomically rebuilding the
    /// v2 free-space tree in a surrounding COW transaction.
    pub fn free_extents(&self) -> Vec<(u64, u64)> {
        self.free
            .lock()
            .iter()
            .map(|(&logical, &len)| (logical, len))
            .collect()
    }

    // Writer-side reservation API kept for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn reserve(&self, len: u64, alignment: u64) -> AxResult<LogicalReservation> {
        self.reserve_where(len, alignment, |_, _| true)
    }

    /// Removes one already-chosen logical range from the free-space image.
    /// Tree-log replay uses this for extents which were written before the
    /// crash but whose home-tree reference was not yet published.  It is not
    /// an allocation policy: the caller has already decoded the native
    /// `EXTENT_DATA` record and merely needs to make the FreeSpace tree stop
    /// advertising exactly those sectors in the same root switch.
    pub fn consume_exact(&self, logical: u64, len: u64) -> AxResult<()> {
        if logical == 0 || len == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = logical.checked_add(len).ok_or(AxError::Io)?;
        let mut free = self.free.lock();
        let (&start, &span) = free.range(..=logical).next_back().ok_or(AxError::Io)?;
        let span_end = start.checked_add(span).ok_or(AxError::Io)?;
        if logical < start || end > span_end {
            return Err(AxError::Io);
        }
        free.remove(&start);
        if logical > start {
            free.insert(start, logical - start);
        }
        if end < span_end {
            free.insert(end, span_end - end);
        }
        Ok(())
    }

    /// Reserves only from an extent accepted by the caller's checked chunk
    /// policy (for example metadata/system chunks for tree nodes).
    pub fn reserve_where(
        &self,
        len: u64,
        alignment: u64,
        mut admit: impl FnMut(u64, u64) -> bool,
    ) -> AxResult<LogicalReservation> {
        if len == 0 || alignment == 0 || !alignment.is_power_of_two() {
            return Err(AxError::InvalidInput);
        }
        let mut free = self.free.lock();
        let candidate = free
            .iter()
            .find_map(|(&start, &size)| {
                let aligned = start.checked_add(alignment - 1)? & !(alignment - 1);
                (aligned.checked_add(len)? <= start.checked_add(size)? && admit(aligned, len))
                    .then_some((start, size, aligned))
            })
            .ok_or(AxError::StorageFull)?;
        let (start, size, logical) = candidate;
        let end = start.checked_add(size).ok_or(AxError::Io)?;
        let allocated_end = logical.checked_add(len).ok_or(AxError::Io)?;
        free.remove(&start);
        if logical > start {
            free.insert(start, logical - start);
        }
        if allocated_end < end {
            free.insert(allocated_end, end - allocated_end);
        }
        if let Some((core, lease)) = &self.lease {
            if let Err(error) = core.claim_logical_range(*lease, logical, len) {
                // Restore the exact split we just removed before reporting a
                // concurrent lease/generation conflict.
                free.remove(&start);
                free.insert(start, size);
                return Err(error);
            }
        }
        Ok(LogicalReservation {
            logical,
            len,
            committed: false,
        })
    }

    pub fn release(&self, reservation: LogicalReservation) -> AxResult<()> {
        if reservation.committed {
            return Err(AxError::BadState);
        }
        if let Some((core, lease)) = &self.lease {
            core.release_logical_range(*lease, reservation.logical, reservation.len)?;
        }
        self.add_free(reservation.logical, reservation.len)
    }

    pub fn commit(&self, reservation: &mut LogicalReservation) -> AxResult<()> {
        if reservation.committed {
            return Err(AxError::BadState);
        }
        if let Some((core, lease)) = &self.lease {
            core.release_logical_range(*lease, reservation.logical, reservation.len)?;
        }
        reservation.committed = true;
        Ok(())
    }

    /// Marks a range irrevocably consumed without releasing its cross-writer
    /// lease.  Balance uses this after the first data write if a later
    /// metadata/root publication fails: keeping the lease is conservative
    /// but prevents any live writer from reusing possibly written sectors.
    pub fn seal(&self, reservation: &mut LogicalReservation) -> AxResult<()> {
        if reservation.committed {
            return Err(AxError::BadState);
        }
        reservation.committed = true;
        Ok(())
    }
}

// Writer-side allocator kept for the gated Btrfs COW writer.
#[allow(dead_code)]
impl BtrfsAllocator {
    pub fn new() -> Self {
        Self {
            free: Mutex::new(BTreeMap::new()),
        }
    }

    /// Inserts a free extent discovered from a checked free-space tree.
    /// Overlapping records indicate metadata corruption and are refused.
    pub fn add_free(&self, device: usize, physical: u64, len: u64) -> AxResult<()> {
        if len == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = physical.checked_add(len).ok_or(AxError::InvalidInput)?;
        let mut free = self.free.lock();
        let extents = free.entry(device).or_default();
        if extents
            .range(..=physical)
            .next_back()
            .map_or(false, |(&start, &size)| {
                start
                    .checked_add(size)
                    .map_or(true, |existing_end| existing_end > physical)
            })
            || extents
                .range(physical..)
                .next()
                .map_or(false, |(&start, _)| start < end)
        {
            return Err(AxError::Io);
        }
        extents.insert(physical, len);
        Ok(())
    }

    /// Reserves all physical members required by a profile.  The
    /// requested physical length is explicit because callers deriving it from
    /// a chunk item must account for stripe geometry before allocation.
    pub fn reserve(
        &self,
        profile: ChunkProfile,
        members: usize,
        physical_len: u64,
        alignment: u64,
    ) -> AxResult<ChunkReservation> {
        if physical_len == 0 || alignment == 0 || !alignment.is_power_of_two() {
            return Err(AxError::InvalidInput);
        }
        let required = match profile {
            ChunkProfile::Single => 1,
            ChunkProfile::Dup => 2,
            ChunkProfile::Raid0 | ChunkProfile::Raid1 => members.max(2),
            ChunkProfile::Raid10 => members.max(4),
            // Btrfs RAID5/6 stripe counts include the P/Q columns.  Requiring
            // distinct devices is essential: a parity column sharing a
            // backing device with a data column does not tolerate the loss it
            // claims to tolerate.
            ChunkProfile::Raid5 => members.max(3),
            ChunkProfile::Raid6 => members.max(4),
        };
        let mut free = self.free.lock();
        let mut selected: Vec<AllocatedStripe> = Vec::new();
        selected
            .try_reserve_exact(required)
            .map_err(|_| AxError::NoMemory)?;
        // DUP intentionally allows two extents on one device; RAID mirrors
        // require different device indices to avoid claiming redundancy that
        // is not physically present.
        for ordinal in 0..required {
            let distinct = !matches!(profile, ChunkProfile::Dup);
            let same_device = matches!(profile, ChunkProfile::Dup)
                .then(|| selected.first().map(|stripe| stripe.device))
                .flatten();
            let candidate = match find_fit(
                &free,
                physical_len,
                alignment,
                if distinct { Some(&selected) } else { None },
                same_device,
            ) {
                Some(candidate) => candidate,
                None => {
                    // Reservation is all-or-nothing.  Returning earlier
                    // stripes while holding the same free-map lock prevents
                    // a partially satisfiable RAID set from leaking space.
                    for stripe in selected.iter().copied() {
                        restore_consumed(&mut free, stripe)?;
                    }
                    return Err(AxError::StorageFull);
                }
            };
            consume(&mut free, candidate, physical_len)?;
            selected.push(candidate);
            let _ = ordinal;
        }
        Ok(ChunkReservation {
            stripes: selected,
            committed: false,
        })
    }

    /// Reserves a complete chunk from its logical length.  This is the
    /// preferred entry point for balance/device code: it derives the native
    /// per-member extent length from data-column geometry and therefore keeps
    /// RAID5/6 allocation, write rotation and on-media `CHUNK_ITEM` shape in
    /// agreement.
    pub fn reserve_chunk(
        &self,
        profile: ChunkProfile,
        requested_stripes: usize,
        logical_len: u64,
        stripe_len: u64,
        alignment: u64,
    ) -> AxResult<(ChunkReservationGeometry, ChunkReservation)> {
        if logical_len == 0 || stripe_len == 0 || !stripe_len.is_power_of_two() {
            return Err(AxError::InvalidInput);
        }
        let stripe_count = match profile {
            ChunkProfile::Single => 1,
            ChunkProfile::Dup => 2,
            ChunkProfile::Raid0 | ChunkProfile::Raid1 => requested_stripes.max(2),
            ChunkProfile::Raid10 => requested_stripes.max(4),
            ChunkProfile::Raid5 => requested_stripes.max(3),
            ChunkProfile::Raid6 => requested_stripes.max(4),
        };
        let (data_stripes, sub_stripes) = match profile {
            ChunkProfile::Single | ChunkProfile::Dup | ChunkProfile::Raid1 => (1, 0),
            ChunkProfile::Raid0 => (stripe_count, 0),
            ChunkProfile::Raid10 => (stripe_count / 2, 2),
            ChunkProfile::Raid5 => (stripe_count - 1, 0),
            ChunkProfile::Raid6 => (stripe_count - 2, 0),
        };
        let set_len = stripe_len
            .checked_mul(u64::try_from(data_stripes).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        if data_stripes == 0 || logical_len % set_len != 0 {
            return Err(AxError::InvalidInput);
        }
        let physical_len =
            logical_len / u64::try_from(data_stripes).map_err(|_| AxError::InvalidInput)?;
        if physical_len % stripe_len != 0 {
            return Err(AxError::InvalidInput);
        }
        let geometry = ChunkReservationGeometry {
            stripe_count,
            data_stripes,
            sub_stripes,
            stripe_len,
            physical_len,
        };
        let reservation = self.reserve(profile, stripe_count, physical_len, alignment)?;
        Ok((geometry, reservation))
    }

    /// Returns an uncommitted reservation to the in-memory free map.  The
    /// caller uses this only before publishing extent-tree records.
    pub fn release(&self, mut reservation: ChunkReservation) -> AxResult<()> {
        if reservation.committed {
            return Err(AxError::BadState);
        }
        for stripe in reservation.stripes.drain(..) {
            self.add_free(stripe.device, stripe.physical, stripe.len)?;
        }
        Ok(())
    }

    /// Marks the reservation consumed after its extent/chunk-tree records
    /// reached the transaction commit point.
    pub fn commit(&self, reservation: &mut ChunkReservation) -> AxResult<()> {
        if reservation.committed {
            return Err(AxError::BadState);
        }
        reservation.committed = true;
        Ok(())
    }
}

// Writer-side allocator internals kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn find_fit(
    free: &BTreeMap<usize, BTreeMap<u64, u64>>,
    len: u64,
    align: u64,
    exclude_devices: Option<&Vec<AllocatedStripe>>,
    only_device: Option<usize>,
) -> Option<AllocatedStripe> {
    for (&device, extents) in free {
        if only_device.map_or(false, |wanted| wanted != device)
            || exclude_devices.map_or(false, |selected| {
                selected.iter().any(|stripe| stripe.device == device)
            })
        {
            continue;
        }
        for (&start, &size) in extents {
            let aligned = start.checked_add(align - 1)? & !(align - 1);
            if aligned.checked_add(len)? <= start.checked_add(size)? {
                return Some(AllocatedStripe {
                    device,
                    physical: aligned,
                    len,
                });
            }
        }
    }
    None
}
// Writer-side allocator internals kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn consume(
    free: &mut BTreeMap<usize, BTreeMap<u64, u64>>,
    stripe: AllocatedStripe,
    len: u64,
) -> AxResult<()> {
    let extents = free.get_mut(&stripe.device).ok_or(AxError::Io)?;
    let (&start, &size) = extents
        .range(..=stripe.physical)
        .next_back()
        .ok_or(AxError::Io)?;
    let end = start.checked_add(size).ok_or(AxError::Io)?;
    let allocated_end = stripe.physical.checked_add(len).ok_or(AxError::Io)?;
    if stripe.physical < start || allocated_end > end {
        return Err(AxError::Io);
    }
    extents.remove(&start);
    if stripe.physical > start {
        extents.insert(start, stripe.physical - start);
    }
    if allocated_end < end {
        extents.insert(allocated_end, end - allocated_end);
    }
    Ok(())
}

// Writer-side allocator internals kept for the gated Btrfs COW writer.
#[allow(dead_code)]
fn restore_consumed(
    free: &mut BTreeMap<usize, BTreeMap<u64, u64>>,
    stripe: AllocatedStripe,
) -> AxResult<()> {
    let extents = free.entry(stripe.device).or_default();
    let end = stripe.physical.checked_add(stripe.len).ok_or(AxError::Io)?;
    let previous = extents
        .range(..=stripe.physical)
        .next_back()
        .map(|(&start, &len)| (start, len));
    let next = extents
        .range(stripe.physical..)
        .next()
        .map(|(&start, &len)| (start, len));
    if previous.map_or(false, |(start, len)| {
        start
            .checked_add(len)
            .map_or(true, |old_end| old_end > stripe.physical)
    }) || next.map_or(false, |(start, _)| start < end)
    {
        return Err(AxError::Io);
    }
    let mut start = stripe.physical;
    let mut len = stripe.len;
    if let Some((old_start, old_len)) = previous {
        if old_start.checked_add(old_len) == Some(start) {
            extents.remove(&old_start);
            start = old_start;
            len = len.checked_add(old_len).ok_or(AxError::Io)?;
        }
    }
    if let Some((next_start, next_len)) = next {
        if end == next_start {
            extents.remove(&next_start);
            len = len.checked_add(next_len).ok_or(AxError::Io)?;
        }
    }
    extents.insert(start, len);
    Ok(())
}
