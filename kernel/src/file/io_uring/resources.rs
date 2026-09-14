//! Registered files, pinned buffers, and their accounting and leases.

use super::*;

pub(super) struct RequestSlotCharge(usize);

impl RequestSlotCharge {
    pub(super) fn try_new(slots: usize) -> AxResult<Self> {
        IO_URING_REQUEST_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_REQUEST_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENOSPC))?;
        Ok(Self(slots))
    }
}

impl Drop for RequestSlotCharge {
    fn drop(&mut self) {
        IO_URING_REQUEST_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

pub(super) struct FixedFileSlotCharge(usize);

impl FixedFileSlotCharge {
    pub(super) fn try_new(slots: usize) -> AxResult<Self> {
        if slots > crate::task::AX_FILE_LIMIT {
            return Err(AxError::from(LinuxError::EMFILE));
        }
        IO_URING_FIXED_FILE_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_FIXED_FILE_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENFILE))?;
        Ok(Self(slots))
    }
}

impl Drop for FixedFileSlotCharge {
    fn drop(&mut self) {
        IO_URING_FIXED_FILE_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

pub(super) struct RegisteredBufferSlotCharge(usize);

impl RegisteredBufferSlotCharge {
    pub(super) fn try_new(slots: usize) -> AxResult<Self> {
        IO_URING_REGISTERED_BUFFER_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_REGISTERED_BUFFER_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENFILE))?;
        Ok(Self(slots))
    }
}

impl Drop for RegisteredBufferSlotCharge {
    fn drop(&mut self) {
        IO_URING_REGISTERED_BUFFER_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

pub(super) struct RegisteredBufferPinBudget {
    pub(super) pages: AtomicUsize,
    pub(super) bytes: AtomicUsize,
}

impl RegisteredBufferPinBudget {
    pub(super) const fn new() -> Self {
        Self {
            pages: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        }
    }

    pub(super) fn try_reserve(&self, pages: usize, bytes: usize, page_limit: usize) -> bool {
        if self
            .pages
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(pages).filter(|next| *next <= page_limit)
            })
            .is_err()
        {
            return false;
        }
        let byte_limit = page_limit.saturating_mul(PAGE_BYTES);
        if self
            .bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= byte_limit)
            })
            .is_err()
        {
            self.pages.fetch_sub(pages, Ordering::AcqRel);
            return false;
        }
        true
    }

    pub(super) fn release(&self, pages: usize, bytes: usize) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.pages.fetch_sub(pages, Ordering::AcqRel);
    }

    pub(super) fn try_charge(
        self: &Arc<Self>,
        pages: usize,
    ) -> AxResult<RegisteredBufferPinCharge> {
        let bytes = pages.checked_mul(PAGE_BYTES).ok_or(AxError::NoMemory)?;
        if !self.try_reserve(pages, bytes, IO_URING_RING_REGISTERED_BUFFER_PAGES) {
            return Err(AxError::ResourceBusy);
        }
        if !try_reserve_global_registered_buffer_pin(pages, bytes) {
            self.release(pages, bytes);
            return Err(AxError::ResourceBusy);
        }
        Ok(RegisteredBufferPinCharge {
            budget: Arc::clone(self),
            pages,
            bytes,
        })
    }
}

pub(super) struct RegisteredBufferPinCharge {
    pub(super) budget: Arc<RegisteredBufferPinBudget>,
    pub(super) pages: usize,
    pub(super) bytes: usize,
}

impl Drop for RegisteredBufferPinCharge {
    fn drop(&mut self) {
        self.budget.release(self.pages, self.bytes);
        IO_URING_REGISTERED_BUFFER_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        IO_URING_REGISTERED_BUFFER_PAGES.fetch_sub(self.pages, Ordering::AcqRel);
    }
}

pub(super) struct PinBeforeCharge<P, C> {
    pub(super) pin: Option<P>,
    pub(super) _charge: C,
}

impl<P, C> PinBeforeCharge<P, C> {
    pub(super) fn new(pin: P, charge: C) -> Self {
        Self {
            pin: Some(pin),
            _charge: charge,
        }
    }
}

impl<P, C> Drop for PinBeforeCharge<P, C> {
    fn drop(&mut self) {
        drop(self.pin.take());
    }
}

pub(super) fn try_reserve_global_registered_buffer_pin(pages: usize, bytes: usize) -> bool {
    if IO_URING_REGISTERED_BUFFER_PAGES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(pages)
                .filter(|next| *next <= IO_URING_GLOBAL_REGISTERED_BUFFER_PAGES)
        })
        .is_err()
    {
        return false;
    }
    let byte_limit = IO_URING_GLOBAL_REGISTERED_BUFFER_PAGES.saturating_mul(PAGE_BYTES);
    if IO_URING_REGISTERED_BUFFER_BYTES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= byte_limit)
        })
        .is_err()
    {
        IO_URING_REGISTERED_BUFFER_PAGES.fetch_sub(pages, Ordering::AcqRel);
        return false;
    }
    true
}

pub(super) struct RegisteredFiles {
    pub(super) table: RegisteredFileTable<FileDescription>,
    pub(super) _charge: FixedFileSlotCharge,
}

/// Registered-buffer owner. The pin is retained until the table owner and
/// every request lease have retired. Actual file I/O still uses the existing
/// direct-or-copy fallback; this pin establishes lifetime and mapping fences,
/// not a claim of hardware DMA support.
pub(super) struct RegisteredBuffer {
    pub(super) world: crate::task::WorldId,
    pub(super) address: usize,
    pub(super) length: usize,
    pub(super) capability: UserMemoryCapability,
    pub(super) pin_start: usize,
    pub(super) pin_len: usize,
    pub(super) segment_ends: Vec<usize>,
    pub(super) pin_segments_disjoint: bool,
    // Release the lower VM/frame/page-cache pin before making this ring's
    // admission charge reusable. A large unpin can yield enough observable
    // time for another registration to consume the io_uring budget while the
    // shared lower pin budget is still occupied.
    pub(super) _pin_owner: PinBeforeCharge<PinnedUserSegmentsMut, RegisteredBufferPinCharge>,
}

// The owner retains the explicit address-space capability alongside the
// kernel-side pin/fence state and opaque userspace address. It never relies on
// current-task state; the address-space pin registry serializes mapping
// changes for the selected capability.
unsafe impl Send for RegisteredBuffer {}
unsafe impl Sync for RegisteredBuffer {}

pub(super) struct RegisteredBuffers {
    pub(super) table: RegisteredBufferTable<RegisteredBuffer>,
    pub(super) _charge: RegisteredBufferSlotCharge,
}

/// A ring-owned user region used exclusively for v6.18 registered wait
/// records.  Retaining the original capability keeps the region bound to its
/// registration address space rather than whichever task later enters it.
pub(super) struct RegisteredWaitRegion {
    /// Kernel-owned, zero-filled backing exported at the fixed v6.18
    /// parameter-region offset.  Keeping both the mapping policy and its
    /// backing here makes final-close/unmap lifetime purely Arc based.
    pub(super) backing: RegisteredWaitBacking,
    pub(super) length: usize,
    pub(super) wait_arguments: bool,
}

pub(super) enum RegisteredWaitBacking {
    Kernel {
        region: FixedSharedMmapRegion,
        pages: Arc<SharedPages>,
    },
    User {
        capability: UserMemoryCapability,
        address: usize,
        _pin: PinnedUserSegments,
    },
}

/// `IORING_MAP_OFF_PARAM_REGION` is an internal v6.18 mmap selector.  It is
/// deliberately distinct from the three public SQ/CQ/SQE offsets and the
/// provided-buffer selector range.
pub(super) const IORING_MAP_OFF_PARAM_REGION: u64 = 0x2000_0000;

pub(super) struct IoUringFinalizer {
    pub(super) ring: Option<Arc<IoUring>>,
}

impl Drop for IoUringFinalizer {
    fn drop(&mut self) {
        let Some(ring) = self.ring.take() else {
            return;
        };
        ring.request_final_close();
    }
}

pub(crate) enum IoUringFileLease {
    Descriptor(Arc<FileDescription>),
    Registered {
        ring: Weak<IoUring>,
        lease: Option<RegisteredFileLease<FileDescription>>,
    },
}

pub(crate) struct IoUringBufferLease {
    pub(super) ring: Arc<IoUring>,
    pub(super) owner: IoUringBufferLeaseOwner,
    pub(super) provided_return_on_drop: bool,
}

pub(super) enum IoUringBufferLeaseOwner {
    Registered(Option<RegisteredBufferLease<RegisteredBuffer>>),
    Provided {
        group: u16,
        id: u16,
        address: u64,
        length: u32,
        capability: UserMemoryCapability,
    },
}

impl Drop for IoUringBufferLease {
    fn drop(&mut self) {
        match &mut self.owner {
            IoUringBufferLeaseOwner::Registered(lease) => {
                if let Some(lease) = lease.take() {
                    self.ring.release_registered_buffer(lease);
                }
            }
            IoUringBufferLeaseOwner::Provided { group, id, .. } => {
                // `consume_provided` disarms this lease before the caller
                // can publish a CQE.  A later userspace PROVIDE_BUFFERS may
                // legitimately reuse the same bid, so an old consumed lease
                // must never look that bid up again on Drop.
                if self.provided_return_on_drop {
                    self.ring.release_provided_buffer(*group, *id);
                }
            }
        }
    }
}

pub(super) fn clip_registered_physical_segments(
    segments: &[UserIoPinSegment],
    offset: usize,
    length: usize,
    output: &mut [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
) -> AxResult<usize> {
    if length == 0 || length > IO_URING_PHYSICAL_MAX_BYTES {
        return Err(AxError::InvalidInput);
    }
    let end = offset.checked_add(length).ok_or(AxError::BadAddress)?;
    let mut logical = 0usize;
    let mut count = 0usize;
    for segment in segments.iter().copied() {
        let segment_end = logical
            .checked_add(segment.len)
            .ok_or(AxError::BadAddress)?;
        let clip_start = offset.max(logical);
        let clip_end = end.min(segment_end);
        if clip_start < clip_end {
            let paddr = segment
                .paddr
                .checked_add(clip_start.checked_sub(logical).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            let clipped_len = clip_end - clip_start;
            if let Some(previous) = count.checked_sub(1).and_then(|index| output.get_mut(index))
                && previous.paddr.checked_add(previous.len) == Some(paddr)
            {
                previous.len = previous
                    .len
                    .checked_add(clipped_len)
                    .ok_or(AxError::BadAddress)?;
            } else {
                if count == output.len() {
                    return Err(AxError::InvalidInput);
                }
                output[count] = PhysicalIoSegment::new(paddr, clipped_len);
                count += 1;
            }
        }
        logical = segment_end;
        if logical >= end {
            break;
        }
    }
    if logical < end || count == 0 {
        return Err(AxError::BadAddress);
    }
    Ok(count)
}

pub(super) fn locate_physical_segment(
    segment_ends: &[usize],
    offset: usize,
) -> AxResult<(usize, usize)> {
    let segment_index = segment_ends.partition_point(|segment_end| *segment_end <= offset);
    let preceding = segment_index
        .checked_sub(1)
        .map_or(0, |index| segment_ends[index]);
    let segment_offset = offset.checked_sub(preceding).ok_or(AxError::BadAddress)?;
    Ok((segment_index, segment_offset))
}

impl IoUringBufferLease {
    pub(super) fn validate_world(&self, world: crate::task::WorldId) -> AxResult<()> {
        let owner_world = match &self.owner {
            IoUringBufferLeaseOwner::Registered(Some(lease)) => lease.owner().world,
            IoUringBufferLeaseOwner::Registered(None) => return Err(AxError::BadState),
            IoUringBufferLeaseOwner::Provided { .. } => self.ring.world,
        };
        if world.admits(owner_world) && world.admits(self.ring.world) {
            Ok(())
        } else {
            Err(AxError::PermissionDenied)
        }
    }

    pub(crate) fn consume_provided(&mut self) {
        if self.provided_return_on_drop
            && let IoUringBufferLeaseOwner::Provided { group, id, .. } = &self.owner
        {
            self.ring.consume_provided_buffer(*group, *id);
            self.provided_return_on_drop = false;
        }
    }

    pub(crate) fn rollback_consumed_provided(&mut self) {
        if !self.provided_return_on_drop
            && let IoUringBufferLeaseOwner::Provided { group, id, .. } = &self.owner
        {
            self.ring.restore_provided_buffer(*group, *id);
            self.provided_return_on_drop = true;
        }
    }
    /// Derives the physical descriptor array from this exact registered
    /// buffer lease. Callers can only provide operation metadata; the SG
    /// addresses themselves are never accepted from an unowned tuple.
    pub(crate) fn prepared_physical_plan(
        &self,
        operation: PreparedPhysicalIoOperation,
        offset: u64,
        address: usize,
        requested_len: usize,
        allowed_len: usize,
    ) -> AxResult<PreparedPhysicalIoPlan> {
        let (lease_address, lease_length) = self.range()?;
        if usize::try_from(lease_address).map_err(|_| AxError::BadAddress)? != address
            || usize::try_from(lease_length).map_err(|_| AxError::BadAddress)? != requested_len
        {
            return Err(AxError::BadAddress);
        }
        if allowed_len == 0 || allowed_len > requested_len {
            return Err(AxError::BadAddress);
        }
        let (segments, offset_in_segments, fixed_len, _) = self.physical_range()?;
        if allowed_len > fixed_len {
            return Err(AxError::BadAddress);
        }
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_PHYSICAL_MAX_SEGMENTS];
        let physical_len = clip_registered_physical_segments(
            segments,
            offset_in_segments,
            allowed_len,
            &mut physical,
        )?;
        Ok(PreparedPhysicalIoPlan::new(
            operation,
            offset,
            address,
            requested_len,
            allowed_len,
            physical,
            physical_len,
        ))
    }

    /// Returns the address-space capability captured when this buffer was
    /// registered. Fixed I/O must never substitute the caller's current
    /// capability: the ring may be submitted through a shared descriptor by
    /// another task or address space.
    pub(crate) fn capability(&self) -> AxResult<UserMemoryCapability> {
        self.validate_world(self.ring.world)?;
        match &self.owner {
            IoUringBufferLeaseOwner::Registered(Some(lease)) => {
                Ok(lease.owner().capability.clone())
            }
            IoUringBufferLeaseOwner::Provided { capability, .. } => Ok(capability.clone()),
            IoUringBufferLeaseOwner::Registered(None) => Err(AxError::BadState),
        }
    }

    /// Returns the registered capability and exact request range together.
    ///
    /// The submission path needs both values for fixed resources. Keeping the
    /// world/lifetime check at this combined lease boundary avoids validating
    /// the same retained lease once for the capability and again for its
    /// range. The lease remains the authority for both values; this does not
    /// cache a table entry or weaken retirement checks.
    #[cfg(feature = "io-submit-batch")]
    pub(crate) fn capability_and_range(&self) -> AxResult<(UserMemoryCapability, (u64, u32))> {
        self.validate_world(self.ring.world)?;
        match &self.owner {
            IoUringBufferLeaseOwner::Registered(Some(lease)) => {
                let range = lease.range();
                Ok((
                    lease.owner().capability.clone(),
                    (range.address(), range.length()),
                ))
            }
            IoUringBufferLeaseOwner::Provided {
                address,
                length,
                capability,
                ..
            } => Ok((capability.clone(), (*address, *length))),
            IoUringBufferLeaseOwner::Registered(None) => Err(AxError::BadState),
        }
    }

    /// Returns the exact subrange validated by the table lookup. Fixed I/O
    /// must derive its address and length from this lease rather than reuse
    /// the caller's raw SQE geometry after admission.
    pub(crate) fn range(&self) -> AxResult<(u64, u32)> {
        self.validate_world(self.ring.world)?;
        match &self.owner {
            IoUringBufferLeaseOwner::Registered(Some(lease)) => {
                let range = lease.range();
                Ok((range.address(), range.length()))
            }
            IoUringBufferLeaseOwner::Provided {
                address, length, ..
            } => Ok((*address, *length)),
            IoUringBufferLeaseOwner::Registered(None) => Err(AxError::BadState),
        }
    }

    /// Returns the selected fixed-buffer bytes from the physical SG captured
    /// at registration. The lease must remain alive while the returned view is
    /// consumed; it is the owner of the underlying pin.
    pub(crate) fn physical_range(&self) -> AxResult<(&[UserIoPinSegment], usize, usize, bool)> {
        let IoUringBufferLeaseOwner::Registered(Some(lease)) = &self.owner else {
            return Err(AxError::OperationNotSupported);
        };
        let range = lease.range();
        let owner = lease.owner();
        let address = usize::try_from(range.address()).map_err(|_| AxError::BadAddress)?;
        let length = usize::try_from(range.length()).map_err(|_| AxError::BadAddress)?;
        let offset = address
            .checked_sub(owner.pin_start)
            .ok_or(AxError::BadAddress)?;
        let pin = owner._pin_owner.pin.as_ref().ok_or(AxError::BadState)?;
        let end = offset.checked_add(length).ok_or(AxError::BadAddress)?;
        if end > owner.pin_len {
            return Err(AxError::BadAddress);
        }
        let (segment_index, segment_offset) = locate_physical_segment(&owner.segment_ends, offset)?;
        let segments = pin
            .segments()
            .get(segment_index..)
            .ok_or(AxError::BadAddress)?;
        Ok((
            segments,
            segment_offset,
            length,
            owner.pin_segments_disjoint,
        ))
    }

    /// Validates that an already clipped physical plan remains covered by
    /// this exact registered-buffer lease. The returned lease is retained by
    /// the prepared token; callers never retain this borrowed tuple.
    pub(crate) fn physical_segments_for_plan(
        &self,
        expected: &[PhysicalIoSegment],
        length: usize,
    ) -> AxResult<()> {
        let (segments, offset, fixed_len, _) = self.physical_range()?;
        if length == 0 || length > fixed_len {
            return Err(AxError::BadAddress);
        }
        let _ = self.physical_provenance()?;
        let mut actual = [PhysicalIoSegment::new(0, 0); IO_URING_PHYSICAL_MAX_SEGMENTS];
        let actual_len = clip_registered_physical_segments(segments, offset, length, &mut actual)?;
        if expected != &actual[..actual_len] {
            return Err(AxError::BadAddress);
        }
        Ok(())
    }

    /// Returns the provenance captured by the registered-buffer pin.  The
    /// direct physical-DMA path only accepts private anonymous pages; callers
    /// must continue to hold this lease while the lower filesystem call runs.
    pub(crate) fn physical_provenance(&self) -> AxResult<UserIoPinProvenance> {
        let IoUringBufferLeaseOwner::Registered(Some(lease)) = &self.owner else {
            return Err(AxError::OperationNotSupported);
        };
        let pin = lease
            .owner()
            ._pin_owner
            .pin
            .as_ref()
            .ok_or(AxError::BadState)?;
        Ok(pin.provenance())
    }

    pub(crate) fn provided_id(&self) -> Option<u16> {
        if !self.provided_return_on_drop {
            return None;
        }
        match &self.owner {
            IoUringBufferLeaseOwner::Provided { id, .. } => Some(*id),
            IoUringBufferLeaseOwner::Registered(_) => None,
        }
    }
}

impl IoUringFileLease {
    pub(crate) fn description(&self) -> AxResult<&Arc<FileDescription>> {
        match self {
            Self::Descriptor(description) => Ok(description),
            Self::Registered { lease, .. } => lease
                .as_ref()
                .map(RegisteredFileLease::owner)
                .ok_or(AxError::BadState),
        }
    }
}

impl Drop for IoUringFileLease {
    fn drop(&mut self) {
        let Self::Registered { ring, lease } = self else {
            return;
        };
        let Some(lease) = lease.take() else {
            return;
        };
        if let Some(ring) = ring.upgrade() {
            ring.release_registered_file(lease);
        }
    }
}

impl IoUring {
    pub(super) fn is_ring_description(description: &Arc<FileDescription>) -> bool {
        description.file_handle().downcast::<IoUring>().is_ok()
    }

    pub(crate) fn retain_descriptor(
        &self,
        description: Arc<FileDescription>,
    ) -> AxResult<IoUringFileLease> {
        if Self::is_ring_description(&description) {
            return Err(AxError::BadFileDescriptor);
        }
        Ok(IoUringFileLease::Descriptor(description))
    }

    pub(crate) fn acquire_registered_file(&self, slot: FileSlot) -> AxResult<IoUringFileLease> {
        let lease = {
            let mut state = self.state.lock();
            let table = &mut state
                .fixed_files
                .as_mut()
                .ok_or(AxError::BadFileDescriptor)?
                .table;
            #[cfg(feature = "io-submit-batch")]
            let lease = table.acquire_current(slot);
            #[cfg(not(feature = "io-submit-batch"))]
            let lease = table.acquire(slot);
            lease.map_err(map_core_error)?
        };
        let ring = self.self_weak.get().ok_or(AxError::BadState)?.clone();
        Ok(IoUringFileLease::Registered {
            ring,
            lease: Some(lease),
        })
    }

    pub(crate) fn acquire_registered_buffer(
        &self,
        world: crate::task::WorldId,
        slot: BufferSlot,
        address: u64,
        length: u32,
    ) -> AxResult<IoUringBufferLease> {
        self.admit_world(world)?;
        // Acquire the ring owner before taking a table lease. A failed weak
        // upgrade must not strand the table's lease counter during teardown.
        let ring = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        let lease = {
            let mut state = self.state.lock();
            let table = &mut state
                .registered_buffers
                .as_mut()
                .ok_or(AxError::BadFileDescriptor)?
                .table;
            #[cfg(feature = "io-submit-batch")]
            let lease = table.acquire_current(slot, address, length);
            #[cfg(not(feature = "io-submit-batch"))]
            let lease = table.acquire(slot, address, length);
            lease.map_err(map_buffer_lease_error)?
        };
        Ok(IoUringBufferLease {
            ring,
            owner: IoUringBufferLeaseOwner::Registered(Some(lease)),
            provided_return_on_drop: true,
        })
    }

    /// Atomically select one ready buffer.  The returned lease is the sole
    /// authority that can recycle the slot, preventing two SQEs (including a
    /// multishot executor) from observing the same userspace range.
    pub(crate) fn acquire_provided_buffer(&self, group: u16) -> AxResult<IoUringBufferLease> {
        let ring = self.arc_owner()?;
        let mut state = self.state.lock();
        let slot = state
            .provided_buffers
            .get_mut(&group)
            .and_then(|entry| {
                entry
                    .slots
                    .iter_mut()
                    .find(|slot| !slot.leased && !slot.retiring && !slot.consumed)
            })
            .ok_or_else(|| AxError::from(LinuxError::ENOBUFS))?;
        slot.leased = true;
        Ok(IoUringBufferLease {
            ring,
            owner: IoUringBufferLeaseOwner::Provided {
                group,
                id: slot.id,
                address: u64::try_from(slot.address).map_err(|_| AxError::BadAddress)?,
                length: u32::try_from(slot.length).map_err(|_| AxError::BadAddress)?,
                capability: slot.capability.clone(),
            },
            provided_return_on_drop: true,
        })
    }

    pub(super) fn release_provided_buffer(&self, group: u16, id: u16) {
        let mut state = self.state.lock();
        let Some(entry) = state.provided_buffers.get_mut(&group) else {
            return;
        };
        if let Some(slot) = entry
            .slots
            .iter_mut()
            .find(|slot| slot.id == id && slot.leased)
        {
            slot.leased = false;
        }
        entry.slots.retain(|slot| !slot.retiring || slot.leased);
    }

    pub(super) fn consume_provided_buffer(&self, group: u16, id: u16) {
        let mut state = self.state.lock();
        let Some(entry) = state.provided_buffers.get_mut(&group) else {
            return;
        };
        if let Some(slot) = entry
            .slots
            .iter_mut()
            .find(|slot| slot.id == id && slot.leased)
        {
            slot.leased = false;
            slot.consumed = true;
        }
    }

    pub(super) fn restore_provided_buffer(&self, group: u16, id: u16) {
        let mut state = self.state.lock();
        let Some(entry) = state.provided_buffers.get_mut(&group) else {
            return;
        };
        if let Some(slot) = entry
            .slots
            .iter_mut()
            .find(|slot| slot.id == id && slot.consumed)
        {
            slot.consumed = false;
            slot.leased = true;
        }
    }

    pub(super) fn release_registered_file(&self, lease: RegisteredFileLease<FileDescription>) {
        let (retired, closed) = {
            let mut state = self.state.lock();
            let Some(files) = state.fixed_files.as_mut() else {
                drop(state);
                drop(lease);
                return;
            };
            let retired = match files.table.release(lease) {
                Ok(LeaseRelease::Active) => None,
                Ok(LeaseRelease::Retired(retired)) => Some(retired),
                Err(error) => {
                    let kind = error.error();
                    core::mem::forget(error.into_lease());
                    error!("io_uring registered-file release lost ownership: {kind:?}");
                    return;
                }
            };
            let should_close = files
                .table
                .progress()
                .is_ok_and(|progress| progress.empty());
            if should_close {
                if let Err(error) = files.table.finish_retire() {
                    error!("io_uring fixed-file retirement did not finish: {error:?}");
                    (retired, None)
                } else {
                    (retired, state.fixed_files.take())
                }
            } else {
                (retired, None)
            }
        };
        drop(retired);
        drop(closed);
    }

    pub(super) fn release_registered_buffer(&self, lease: RegisteredBufferLease<RegisteredBuffer>) {
        let (retired, closed) = {
            let mut state = self.state.lock();
            let Some(buffers) = state.registered_buffers.as_mut() else {
                drop(state);
                drop(lease);
                return;
            };
            let retired = match buffers.table.release(lease) {
                Ok(BufferLeaseRelease::Active) => None,
                Ok(BufferLeaseRelease::Retired(retired)) => Some(retired),
                Err(error) => {
                    let kind = error.error();
                    core::mem::forget(error.into_lease());
                    error!("io_uring registered-buffer release lost ownership: {kind:?}");
                    return;
                }
            };
            let should_close = buffers
                .table
                .progress()
                .is_ok_and(|progress| progress.empty());
            if should_close {
                if let Err(error) = buffers.table.finish_retire() {
                    error!("io_uring registered-buffer retirement did not finish: {error:?}");
                    (retired, None)
                } else {
                    (retired, state.registered_buffers.take())
                }
            } else {
                (retired, None)
            }
        };
        drop(retired);
        drop(closed);
    }

    /// Publishes the one completion-eventfd owner.  The descriptor handle is
    /// retained directly so close/reuse of the numeric fd cannot redirect
    /// completion notifications.
    pub(crate) fn register_completion_eventfd(&self, eventfd: FileHandle<EventFd>) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        let mut state = self.state.lock();
        if state.final_close.phase != FinalClosePhase::Begin {
            return Err(AxError::BadFileDescriptor);
        }
        if state.completion_eventfd.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.completion_eventfd = Some(eventfd);
        Ok(())
    }

    pub(crate) fn unregister_completion_eventfd(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        let eventfd = self
            .state
            .lock()
            .completion_eventfd
            .take()
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
        drop(eventfd);
        Ok(())
    }

    pub(crate) fn provide_buffers(
        &self,
        group: u16,
        first_id: u16,
        buffers: Vec<(usize, usize, UserMemoryCapability)>,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let entry = state
            .provided_buffers
            .entry(group)
            .or_insert_with(|| ProvidedBufferGroup { slots: Vec::new() });
        // Validate the complete batch before modifying the group.  A failed
        // PROVIDE_BUFFERS CQE must never leave a prefix unexpectedly usable.
        if buffers.iter().any(|(_, length, _)| *length == 0)
            || buffers.len() > usize::from(u16::MAX) + 1
            || first_id
                .checked_add(
                    u16::try_from(buffers.len().saturating_sub(1))
                        .map_err(|_| AxError::InvalidInput)?,
                )
                .is_none()
            || (0..buffers.len()).any(|offset| {
                let Some(id) = first_id.checked_add(offset as u16) else {
                    return true;
                };
                entry
                    .slots
                    .iter()
                    .any(|slot| slot.id == id && !slot.retiring && !slot.consumed)
            })
        {
            return Err(AxError::InvalidInput);
        }
        entry
            .slots
            .try_reserve(buffers.len())
            .map_err(|_| AxError::NoMemory)?;
        for (offset, (address, length, capability)) in buffers.into_iter().enumerate() {
            let id = first_id
                .checked_add(offset as u16)
                .ok_or(AxError::InvalidInput)?;
            entry
                .slots
                .retain(|slot| slot.id != id || !slot.consumed || slot.leased);
            entry.slots.push(ProvidedBuffer {
                id,
                address,
                length,
                capability,
                leased: false,
                consumed: false,
                retiring: false,
            });
        }
        Ok(())
    }

    pub(crate) fn remove_buffers(&self, group: u16, count: usize) -> AxResult<usize> {
        let mut state = self.state.lock();
        let entry = state
            .provided_buffers
            .get_mut(&group)
            .ok_or(AxError::NotFound)?;
        let mut removed = 0;
        for slot in entry.slots.iter_mut().rev() {
            if removed == count {
                break;
            }
            if !slot.leased && !slot.retiring {
                slot.retiring = true;
                removed += 1;
            }
        }
        entry.slots.retain(|slot| !slot.retiring || slot.leased);
        Ok(removed)
    }

    pub(crate) fn register_files(&self, files: Vec<Option<Arc<FileDescription>>>) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        if files.is_empty() {
            return Err(AxError::InvalidInput);
        }
        if files.iter().flatten().any(Self::is_ring_description) {
            return Err(AxError::BadFileDescriptor);
        }
        let charge = FixedFileSlotCharge::try_new(files.len())?;
        let table_id = {
            let mut state = self.state.lock();
            if state.fixed_files.is_some() {
                return Err(AxError::ResourceBusy);
            }
            let raw = state.next_file_table_id;
            state.next_file_table_id = raw.checked_add(1).ok_or(AxError::OutOfRange)?;
            FileTableId::new(raw).map_err(map_core_error)?
        };
        let capacity = u32::try_from(files.len()).map_err(|_| AxError::InvalidInput)?;
        let mut table =
            RegisteredFileTable::new(self.id, table_id, capacity, self.layout.sq_entries())
                .map_err(map_core_error)?;
        for (slot, file) in files.into_iter().enumerate() {
            if let Some(file) = file
                && let Err(error) = table.install(
                    FileSlot::new(u32::try_from(slot).map_err(|_| AxError::InvalidInput)?),
                    file,
                )
            {
                let kind = error.error();
                drop(error.into_owner());
                return Err(map_core_error(kind));
            }
        }
        table.publish().map_err(map_core_error)?;
        let mut state = self.state.lock();
        if state.fixed_files.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.fixed_files = Some(RegisteredFiles {
            table,
            _charge: charge,
        });
        Ok(())
    }

    pub(crate) fn unregister_files(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        {
            let mut state = self.state.lock();
            let files = state
                .fixed_files
                .as_mut()
                .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
            files.table.begin_retire().map_err(map_core_error)?;
        }
        self.drain_registered_files_after_retire()
    }

    /// Constructs, publishes, and acknowledges the single parameter region
    /// under one registration transaction.  In particular, `ENABLE_RINGS`
    /// cannot observe a region whose in/out descriptor has not been copied
    /// back yet, and a failed acknowledgement retracts that same publication
    /// before releasing `registration_serial`.
    pub(crate) fn register_wait_region_transaction(
        &self,
        prepare: impl FnOnce() -> AxResult<(
            usize,
            bool,
            Option<(UserMemoryCapability, usize, PinnedUserSegments)>,
        )>,
        copyout: impl FnOnce(u32, u64) -> AxResult<()>,
    ) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        // Reject a duplicate parameter region before any caller-controlled
        // usercopy or long-term pin.  The same serial is held through the
        // remainder of preparation and publication, making concurrent
        // registrations observe this exact ordering.
        if self.state.lock().registered_wait_region.is_some() {
            return Err(AxError::ResourceBusy);
        }
        let (length, wait_arguments, user_backing) = prepare()?;
        if wait_arguments && !self.disabled.load(Ordering::Acquire) {
            return Err(AxError::InvalidInput);
        }
        if length < 64 {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        debug_assert!(state.registered_wait_region.is_none());
        let (backing, mmap_offset) = if let Some((capability, address, pin)) = user_backing {
            (
                RegisteredWaitBacking::User {
                    capability,
                    address,
                    _pin: pin,
                },
                0,
            )
        } else {
            let pages = Arc::try_new(SharedPages::new_fixed(length, PageSize::Size4K)?)
                .map_err(|_| AxError::NoMemory)?;
            let region = FixedSharedMmapRegion::try_new_detached(
                IORING_MAP_OFF_PARAM_REGION,
                Arc::clone(&pages),
                FileMmapProtection::READ | FileMmapProtection::WRITE,
            )?;
            (
                RegisteredWaitBacking::Kernel { region, pages },
                IORING_MAP_OFF_PARAM_REGION,
            )
        };
        // There is one v6.18 parameter region per ring.  Its zero id is part
        // of the ABI copyout and no ID is published until the complete object
        // has been constructed under the registration lock.
        state.registered_wait_region = Some(RegisteredWaitRegion {
            backing,
            length,
            wait_arguments,
        });
        drop(state);

        // The kernel-generated mapping id/offset are not externally visible
        // until this copyout succeeds.  Keep the serial held across it so a
        // concurrent REGISTER_ENABLE_RINGS cannot commit a half-registered
        // parameter region.  Rollback uses the lock already held above;
        // calling the public unregister helper here would recursively lock.
        if let Err(error) = copyout(0, mmap_offset) {
            self.state.lock().registered_wait_region.take();
            return Err(error);
        }
        Ok(())
    }

    /// Copies an exact 64-byte registered wait record.  The returned bytes
    /// have no transient user pointer and may safely outlive the enter call.
    pub(crate) fn copy_registered_wait(&self, offset: usize) -> AxResult<[u8; 64]> {
        let state = self.state.lock();
        let region = state
            .registered_wait_region
            .as_ref()
            .ok_or(AxError::BadAddress)?;
        if !region.wait_arguments {
            return Err(AxError::BadAddress);
        }
        if !offset.is_multiple_of(core::mem::size_of::<usize>()) {
            return Err(AxError::BadAddress);
        }
        let end = offset.checked_add(64).ok_or(AxError::BadAddress)?;
        if end > region.length {
            return Err(AxError::BadAddress);
        }
        let mut bytes = [0u8; 64];
        match &region.backing {
            RegisteredWaitBacking::Kernel { pages, .. } => pages.read_bytes(offset, &mut bytes)?,
            RegisteredWaitBacking::User {
                capability,
                address,
                ..
            } => {
                let address = address.checked_add(offset).ok_or(AxError::BadAddress)?;
                let value = capability
                    .read_value_uninit(address as *const [u8; 64])
                    .map_err(crate::mm::map_usercopy_error)?;
                bytes = unsafe { value.assume_init() };
            }
        }
        Ok(bytes)
    }

    pub(crate) fn copy_registered_signal_mask(
        &self,
        capability: &UserMemoryCapability,
        address: usize,
    ) -> AxResult<SignalSet> {
        // The wait record itself is kernel-backed, but `sigmask` retains the
        // UAPI user pointer semantics.  Copy it against this enter caller's
        // current capability after the record has been copied by value.
        let value = capability
            .read_value_uninit(address as *const SignalSet)
            .map_err(crate::mm::map_usercopy_error)?;
        // SAFETY: the complete fixed signal set was copied before returning.
        Ok(unsafe { value.assume_init() })
    }

    pub(super) fn drain_registered_files_after_retire(&self) -> AxResult<()> {
        loop {
            let retired = {
                let mut state = self.state.lock();
                let Some(files) = state.fixed_files.as_mut() else {
                    break;
                };
                let Some(token) = files.table.next_retirable().map_err(map_core_error)? else {
                    break;
                };
                files.table.retire(token).map_err(map_core_error)?
            };
            drop(retired);
        }
        let closed = {
            let mut state = self.state.lock();
            if let Some(files) = state.fixed_files.as_mut() {
                if files.table.progress().map_err(map_core_error)?.empty() {
                    files.table.finish_retire().map_err(map_core_error)?;
                    state.fixed_files.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(closed);
        Ok(())
    }

    pub(crate) fn register_buffers(
        &self,
        world: crate::task::WorldId,
        capability: &UserMemoryCapability,
        buffers: Vec<(usize, usize)>,
    ) -> AxResult<()> {
        self.admit_world(world)?;
        let _registration = self.registration_serial.lock();
        if buffers.is_empty()
            || buffers.len() > self.world.profile().limits().registered_buffers as usize
        {
            return Err(AxError::InvalidInput);
        }
        let capacity = u32::try_from(buffers.len()).map_err(|_| AxError::InvalidInput)?;
        // Validate every descriptor and its page-cover arithmetic before
        // publishing or pinning any owner. The syscall adapter has already
        // checked user write access; this second pass keeps the ring API
        // transactional for future callers too.
        for &(address, length) in &buffers {
            if length == 0 {
                return Err(AxError::InvalidInput);
            }
            let end = address.checked_add(length).ok_or(AxError::BadAddress)?;
            let page_start = address & !(PAGE_BYTES - 1);
            let page_end = end
                .checked_add(PAGE_BYTES - 1)
                .map(|value| value & !(PAGE_BYTES - 1))
                .ok_or(AxError::BadAddress)?;
            if page_end <= page_start || page_end - page_start < PAGE_BYTES {
                return Err(AxError::InvalidInput);
            }
        }
        let charge = RegisteredBufferSlotCharge::try_new(buffers.len())?;
        let table_id = {
            let mut state = self.state.lock();
            if state.final_close.phase != FinalClosePhase::Begin {
                return Err(AxError::BadFileDescriptor);
            }
            if state.registered_buffers.is_some() {
                return Err(AxError::ResourceBusy);
            }
            let raw = state.next_buffer_table_id;
            state.next_buffer_table_id = raw.checked_add(1).ok_or(AxError::OutOfRange)?;
            BufferTableId::new(raw).map_err(map_core_error)?
        };
        let mut table =
            RegisteredBufferTable::new(self.id, table_id, capacity, self.layout.sq_entries())
                .map_err(map_core_error)?;
        for (slot, (address, length)) in buffers.into_iter().enumerate() {
            let end = address.checked_add(length).ok_or(AxError::BadAddress)?;
            let page_start = address & !(PAGE_BYTES - 1);
            let page_end = end
                .checked_add(PAGE_BYTES - 1)
                .map(|value| value & !(PAGE_BYTES - 1))
                .ok_or(AxError::BadAddress)?;
            let page_len = page_end - page_start;
            let page_count = page_len / PAGE_BYTES;
            let pin_charge = self.registered_buffer_budget.try_charge(page_count)?;
            let pin = match try_pin_user_segments_to_user_longterm_with(
                capability,
                page_start as *mut u8,
                page_len,
            ) {
                Some(pin) => pin,
                None => {
                    drop(pin_charge);
                    // A rejected user pin is an invalid registered-buffer
                    // address (not ring-table contention). In particular,
                    // secretmem deliberately cannot supply durable DMA/GUP
                    // segments and Linux reports EFAULT for this admission.
                    return Err(AxError::BadAddress);
                }
            };
            let mut segment_ends = Vec::new();
            segment_ends
                .try_reserve_exact(pin.segments().len())
                .map_err(|_| AxError::NoMemory)?;
            let mut segment_end = 0usize;
            for segment in pin.segments() {
                segment_end = segment_end
                    .checked_add(segment.len)
                    .ok_or(AxError::BadAddress)?;
                segment_ends.push(segment_end);
            }
            if segment_end != page_len {
                return Err(AxError::BadState);
            }
            let owner = Arc::try_new(RegisteredBuffer {
                world: self.world,
                address,
                length,
                pin_start: page_start,
                pin_len: page_len,
                segment_ends,
                pin_segments_disjoint: physical_segments_are_disjoint(pin.segments()),
                capability: capability.clone(),
                _pin_owner: PinBeforeCharge::new(pin, pin_charge),
            })
            .map_err(|_| AxError::NoMemory)?;
            if let Err(error) = table.install(
                BufferSlot::new(u32::try_from(slot).map_err(|_| AxError::InvalidInput)?),
                address as u64,
                length as u64,
                owner,
            ) {
                let kind = error.error();
                drop(error.into_owner());
                return Err(map_core_error(kind));
            }
        }
        table.publish().map_err(map_core_error)?;
        let mut state = self.state.lock();
        if state.final_close.phase != FinalClosePhase::Begin {
            return Err(AxError::BadFileDescriptor);
        }
        if state.registered_buffers.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.registered_buffers = Some(RegisteredBuffers {
            table,
            _charge: charge,
        });
        Ok(())
    }

    pub(crate) fn unregister_buffers(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        {
            let mut state = self.state.lock();
            let buffers = state
                .registered_buffers
                .as_mut()
                .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
            buffers.table.begin_retire().map_err(map_core_error)?;
        }
        loop {
            let retired = {
                let mut state = self.state.lock();
                let Some(buffers) = state.registered_buffers.as_mut() else {
                    break;
                };
                let Some(token) = buffers.table.next_retirable().map_err(map_core_error)? else {
                    break;
                };
                buffers.table.retire(token).map_err(map_core_error)?
            };
            drop(retired);
        }
        let closed = {
            let mut state = self.state.lock();
            if let Some(buffers) = state.registered_buffers.as_mut() {
                if buffers.table.progress().map_err(map_core_error)?.empty() {
                    buffers.table.finish_retire().map_err(map_core_error)?;
                    state.registered_buffers.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(closed);
        Ok(())
    }
}
