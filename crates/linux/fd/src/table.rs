#[cfg(not(feature = "alloc"))]
use core::array;

use crate::{DescriptorFlags, FdNumber, FdTableId};

#[cfg(feature = "alloc")]
use alloc::{sync::Arc, vec::Vec};

#[cfg(feature = "alloc")]
use core::sync::atomic::{AtomicU8, Ordering};

/// Opaque generation-tagged identity for one published descriptor slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DescriptorToken {
    table: FdTableId,
    fd: FdNumber,
    generation: u64,
}

impl DescriptorToken {
    /// Returns the owning table identity.
    pub const fn table(self) -> FdTableId {
        self.table
    }

    /// Returns the numeric descriptor.
    pub const fn fd(self) -> FdNumber {
        self.fd
    }

    /// Returns the opaque generation for diagnostics and adapter indexes.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Opaque unpublished reservation for one numeric descriptor.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a descriptor reservation must be published or explicitly cancelled"]
pub struct ReservationToken {
    table: FdTableId,
    fd: FdNumber,
    generation: u64,
}

#[cfg(feature = "alloc")]
const PUBLICATION_PENDING: u8 = 0;
#[cfg(feature = "alloc")]
const PUBLICATION_VISIBLE: u8 = 1;
#[cfg(feature = "alloc")]
const PUBLICATION_ABORTED: u8 = 2;

#[cfg(feature = "alloc")]
#[derive(Debug)]
struct PublicationState {
    phase: AtomicU8,
}

#[cfg(feature = "alloc")]
impl PublicationState {
    fn pending() -> Self {
        Self {
            phase: AtomicU8::new(PUBLICATION_PENDING),
        }
    }

    fn is_visible(&self) -> bool {
        self.phase.load(Ordering::Acquire) == PUBLICATION_VISIBLE
    }
}

/// Exact unpublished descriptor whose fallible table preparation is complete.
///
/// The token is branded by shared state installed in one table slot. Committing
/// it does not receive a table, allocate, validate a caller-selected target, or
/// return an error: it only makes that exact prepared slot visible. Dropping an
/// uncommitted token marks the slot aborted; adapters should normally call
/// [`FdTable::cancel_prepared`] first so the invisible entry is detached too.
#[cfg(feature = "alloc")]
#[derive(Debug)]
#[must_use = "a prepared descriptor must be committed or explicitly cancelled"]
pub struct PreparedPublication {
    table: FdTableId,
    fd: FdNumber,
    generation: u64,
    state: Arc<PublicationState>,
    active: bool,
}

#[cfg(feature = "alloc")]
impl PreparedPublication {
    /// Returns the exact prepared descriptor number.
    pub const fn fd(&self) -> FdNumber {
        self.fd
    }

    /// Returns the exact owning table identity.
    pub const fn table(&self) -> FdTableId {
        self.table
    }

    /// Makes the already prepared descriptor visible without allocation or a
    /// fallible table operation.
    pub fn commit(mut self) -> DescriptorToken {
        self.state
            .phase
            .store(PUBLICATION_VISIBLE, Ordering::Release);
        self.active = false;
        DescriptorToken {
            table: self.table,
            fd: self.fd,
            generation: self.generation,
        }
    }
}

#[cfg(feature = "alloc")]
impl Drop for PreparedPublication {
    fn drop(&mut self) {
        if self.active {
            self.state
                .phase
                .store(PUBLICATION_ABORTED, Ordering::Release);
        }
    }
}

impl ReservationToken {
    /// Returns the reserved number, which remains invisible to lookup.
    pub const fn fd(&self) -> FdNumber {
        self.fd
    }

    /// Returns the owning table identity.
    pub const fn table(&self) -> FdTableId {
        self.table
    }
}

/// One descriptor-local entry pointing at a shared OFD handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorEntry<D> {
    description: D,
    flags: DescriptorFlags,
}

impl<D> DescriptorEntry<D> {
    /// Creates descriptor-local state around a shared OFD handle.
    pub const fn new(description: D, flags: DescriptorFlags) -> Self {
        Self { description, flags }
    }

    /// Returns the shared OFD handle.
    pub const fn description(&self) -> &D {
        &self.description
    }

    /// Returns descriptor-local flags.
    pub const fn flags(&self) -> DescriptorFlags {
        self.flags
    }

    /// Mutates descriptor-local flags without changing OFD state.
    pub fn set_flags(&mut self, flags: DescriptorFlags) {
        self.flags = flags;
    }

    /// Decomposes the entry for lock-external destruction or handoff.
    pub fn into_parts(self) -> (D, DescriptorFlags) {
        (self.description, self.flags)
    }
}

enum Slot<D> {
    Vacant,
    Reserved {
        generation: u64,
        flags: DescriptorFlags,
    },
    #[cfg(feature = "alloc")]
    Prepared {
        generation: u64,
        entry: DescriptorEntry<D>,
        state: Arc<PublicationState>,
    },
    Occupied {
        generation: u64,
        entry: DescriptorEntry<D>,
    },
}

impl<D> Slot<D> {
    fn visible_entry(&self) -> Option<&DescriptorEntry<D>> {
        match self {
            Self::Occupied { entry, .. } => Some(entry),
            #[cfg(feature = "alloc")]
            Self::Prepared { entry, state, .. } if state.is_visible() => Some(entry),
            _ => None,
        }
    }

    fn visible_entry_mut(&mut self) -> Option<&mut DescriptorEntry<D>> {
        match self {
            Self::Occupied { entry, .. } => Some(entry),
            #[cfg(feature = "alloc")]
            Self::Prepared { entry, state, .. } if state.is_visible() => Some(entry),
            _ => None,
        }
    }

    fn visible_generation(&self) -> Option<u64> {
        match self {
            Self::Occupied { generation, .. } => Some(*generation),
            #[cfg(feature = "alloc")]
            Self::Prepared {
                generation, state, ..
            } if state.is_visible() => Some(*generation),
            _ => None,
        }
    }

    fn blocks_replacement(&self) -> bool {
        match self {
            Self::Reserved { .. } => true,
            #[cfg(feature = "alloc")]
            Self::Prepared { state, .. } => !state.is_visible(),
            _ => false,
        }
    }

    fn into_visible_entry(self) -> Result<DescriptorEntry<D>, Self> {
        match self {
            Self::Occupied { entry, .. } => Ok(entry),
            #[cfg(feature = "alloc")]
            Self::Prepared { entry, state, .. } if state.is_visible() => Ok(entry),
            other => Err(other),
        }
    }
}

/// Bounded caller-owned Linux files-table state.
///
/// The consumer supplies external synchronization and chooses `N`. Every
/// user-visible mutation uses `&mut self`; callbacks, destructors, and
/// allocation therefore never run under a crate-owned hidden lock.
pub struct FdTable<D, const N: usize> {
    id: FdTableId,
    next_generation: u64,
    #[cfg(not(feature = "alloc"))]
    slots: [Slot<D>; N],
    #[cfg(feature = "alloc")]
    slots: Vec<Slot<D>>,
}

/// Descriptor-table operation failure before errno mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FdTableError {
    /// The numeric descriptor is absent or outside this table.
    BadDescriptor,
    /// No slot exists in the requested bounded range.
    TableFull,
    /// A dup/replace target is reserved by another publication transaction.
    Busy,
    /// The token belongs to another table or an older slot generation.
    StaleToken,
    /// No future unique generation can be allocated.
    GenerationExhausted,
    /// Caller-provided close storage cannot hold the whole transaction.
    InsufficientCloseStorage,
    /// Fallible owned storage could not be reserved.
    NoMemory,
    /// The requested capacity was effectively unbounded or could not be
    /// represented by the crate's Linux descriptor-number type.
    Unbounded,
}

/// Publication failure which returns ownership of the unpublished OFD handle.
#[derive(Debug)]
pub struct PublishError<D> {
    /// Typed table error.
    pub error: FdTableError,
    /// Description handle that was never published.
    pub description: D,
}

/// Preparation failure which preserves both reservation authority and OFD
/// ownership so the caller can roll back the exact original table.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct PreparePublicationError<D> {
    /// Typed table error.
    pub error: FdTableError,
    /// Reservation that was not consumed by preparation.
    pub reservation: ReservationToken,
    /// Description handle that was never installed in a prepared slot.
    pub description: D,
}

/// Cancellation failure which returns the still-active exact publication
/// authority so it can be retried against its owning table.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct CancelPreparedError {
    /// Typed table error.
    pub error: FdTableError,
    /// Prepared publication that was not detached.
    pub publication: PreparedPublication,
}

impl<D, const N: usize> FdTable<D, N> {
    /// Fallibly creates an empty table with an explicit stable identity.
    ///
    /// With the `alloc` feature, the complete slot array is reserved and
    /// initialized directly in heap-backed storage. The table value itself
    /// therefore stays small even for a kernel-sized descriptor ceiling, and
    /// moving it into an `Arc` or returning it from `fork_copy` does not create
    /// an `N * size_of::<Slot<D>>()` stack temporary. No later table mutation
    /// grows this storage.
    ///
    /// Without `alloc`, the caller deliberately selects inline fixed storage;
    /// construction remains fallible in the public contract so consumers do
    /// not need a feature-dependent call site.
    pub fn try_new(id: FdTableId) -> Result<Self, FdTableError> {
        if N == usize::MAX
            || u64::try_from(N)
                .ok()
                .is_none_or(|capacity| capacity > u64::from(u32::MAX) + 1)
        {
            return Err(FdTableError::Unbounded);
        }
        #[cfg(feature = "alloc")]
        let slots = {
            let mut slots = Vec::new();
            slots
                .try_reserve_exact(N)
                .map_err(|_| FdTableError::NoMemory)?;
            slots.resize_with(N, || Slot::Vacant);
            slots
        };
        #[cfg(not(feature = "alloc"))]
        let slots = array::from_fn(|_| Slot::Vacant);

        Ok(Self {
            id,
            next_generation: 1,
            slots,
        })
    }

    /// Returns the table identity used in every token.
    pub const fn id(&self) -> FdTableId {
        self.id
    }

    /// Returns the compile-time descriptor ceiling.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of visible published descriptors.
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.visible_entry().is_some())
            .count()
    }

    /// Returns whether no descriptor is visible.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterates visible descriptors in ascending numeric order.
    ///
    /// Reserved slots remain unpublished and are intentionally skipped. The
    /// iterator borrows the table, so the embedding kernel retains its chosen
    /// external read lock for the duration without exposing crate-owned
    /// synchronization.
    pub fn iter(&self) -> impl Iterator<Item = (FdNumber, &DescriptorEntry<D>)> {
        self.slots.iter().enumerate().filter_map(|(fd, slot)| {
            let entry = slot.visible_entry()?;
            let fd = u32::try_from(fd).ok()?;
            Some((FdNumber::new(fd), entry))
        })
    }

    fn allocate_generation(&mut self) -> Result<u64, FdTableError> {
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(FdTableError::GenerationExhausted)?;
        Ok(generation)
    }

    fn bounded_range(minimum: usize, limit: usize) -> core::ops::Range<usize> {
        minimum.min(N)..limit.min(N)
    }

    /// Reserves the lowest free number in `[minimum, limit)`.
    ///
    /// Reserved slots are excluded from concurrent allocation but invisible to
    /// lookup. The caller must publish or cancel the returned token on every
    /// path; adapters normally wrap it in an RAII rollback owner.
    pub fn reserve(
        &mut self,
        minimum: usize,
        limit: usize,
        flags: DescriptorFlags,
    ) -> Result<ReservationToken, FdTableError> {
        let fd = Self::bounded_range(minimum, limit)
            .find(|&fd| matches!(self.slots[fd], Slot::Vacant))
            .ok_or(FdTableError::TableFull)?;
        let generation = self.allocate_generation()?;
        self.slots[fd] = Slot::Reserved { generation, flags };
        Ok(ReservationToken {
            table: self.id,
            fd: FdNumber::new(fd as u32),
            generation,
        })
    }

    /// Cancels the exact unpublished reservation.
    pub fn cancel_reservation(
        &mut self,
        reservation: ReservationToken,
    ) -> Result<(), FdTableError> {
        let fd = self.validate_reservation(&reservation)?;
        self.slots[fd] = Slot::Vacant;
        Ok(())
    }

    fn validate_reservation(&self, reservation: &ReservationToken) -> Result<usize, FdTableError> {
        if reservation.table != self.id || reservation.fd.index() >= N {
            return Err(FdTableError::StaleToken);
        }
        let fd = reservation.fd.index();
        if matches!(
            self.slots[fd],
            Slot::Reserved { generation, .. } if generation == reservation.generation
        ) {
            Ok(fd)
        } else {
            Err(FdTableError::StaleToken)
        }
    }

    /// Publishes a completely constructed OFD handle into its reserved slot.
    pub fn publish(
        &mut self,
        reservation: ReservationToken,
        description: D,
    ) -> Result<DescriptorToken, PublishError<D>> {
        let fd = match self.validate_reservation(&reservation) {
            Ok(fd) => fd,
            Err(error) => return Err(PublishError { error, description }),
        };
        let flags = match self.slots[fd] {
            Slot::Reserved { flags, .. } => flags,
            _ => {
                return Err(PublishError {
                    error: FdTableError::StaleToken,
                    description,
                });
            }
        };
        let token = DescriptorToken {
            table: self.id,
            fd: reservation.fd,
            generation: reservation.generation,
        };
        self.slots[fd] = Slot::Occupied {
            generation: reservation.generation,
            entry: DescriptorEntry::new(description, flags),
        };
        Ok(token)
    }

    /// Completes every fallible table-side step for a later infallible
    /// publication.
    ///
    /// The prepared entry remains invisible and keeps the numeric slot busy.
    /// [`PreparedPublication::commit`] is the sole operation that can make this
    /// exact generation visible, and it does not need the table again.
    #[cfg(feature = "alloc")]
    pub fn prepare_publication(
        &mut self,
        reservation: ReservationToken,
        description: D,
    ) -> Result<PreparedPublication, PreparePublicationError<D>> {
        let fd = match self.validate_reservation(&reservation) {
            Ok(fd) => fd,
            Err(error) => {
                return Err(PreparePublicationError {
                    error,
                    reservation,
                    description,
                });
            }
        };
        let flags = match self.slots[fd] {
            Slot::Reserved { flags, .. } => flags,
            _ => {
                return Err(PreparePublicationError {
                    error: FdTableError::StaleToken,
                    reservation,
                    description,
                });
            }
        };
        let state = Arc::new(PublicationState::pending());
        let publication = PreparedPublication {
            table: self.id,
            fd: reservation.fd,
            generation: reservation.generation,
            state: Arc::clone(&state),
            active: true,
        };
        self.slots[fd] = Slot::Prepared {
            generation: reservation.generation,
            entry: DescriptorEntry::new(description, flags),
            state,
        };
        Ok(publication)
    }

    /// Aborts and detaches the exact still-pending prepared publication.
    ///
    /// On a foreign, stale, or already committed token, the authority is
    /// returned unchanged so the caller cannot accidentally lose ownership of
    /// the real prepared slot.
    #[cfg(feature = "alloc")]
    pub fn cancel_prepared(
        &mut self,
        mut publication: PreparedPublication,
    ) -> Result<DescriptorEntry<D>, CancelPreparedError> {
        if publication.table != self.id || publication.fd.index() >= N {
            return Err(CancelPreparedError {
                error: FdTableError::StaleToken,
                publication,
            });
        }
        let fd = publication.fd.index();
        let matches = matches!(
            &self.slots[fd],
            Slot::Prepared {
                generation,
                state,
                ..
            } if *generation == publication.generation
                && Arc::ptr_eq(state, &publication.state)
                && state.phase.load(Ordering::Acquire) == PUBLICATION_PENDING
        );
        if !matches {
            return Err(CancelPreparedError {
                error: FdTableError::StaleToken,
                publication,
            });
        }

        let previous = core::mem::replace(&mut self.slots[fd], Slot::Vacant);
        match previous {
            Slot::Prepared { entry, .. } => {
                publication
                    .state
                    .phase
                    .store(PUBLICATION_ABORTED, Ordering::Release);
                publication.active = false;
                Ok(entry)
            }
            other => {
                self.slots[fd] = other;
                Err(CancelPreparedError {
                    error: FdTableError::StaleToken,
                    publication,
                })
            }
        }
    }

    /// Looks up a visible descriptor by number.
    pub fn get(&self, fd: FdNumber) -> Result<&DescriptorEntry<D>, FdTableError> {
        self.slots
            .get(fd.index())
            .and_then(Slot::visible_entry)
            .ok_or(FdTableError::BadDescriptor)
    }

    /// Mutably looks up descriptor-local state.
    pub fn get_mut(&mut self, fd: FdNumber) -> Result<&mut DescriptorEntry<D>, FdTableError> {
        self.slots
            .get_mut(fd.index())
            .and_then(Slot::visible_entry_mut)
            .ok_or(FdTableError::BadDescriptor)
    }

    /// Returns a generation token for the currently visible descriptor.
    pub fn token(&self, fd: FdNumber) -> Result<DescriptorToken, FdTableError> {
        let generation = self
            .slots
            .get(fd.index())
            .and_then(Slot::visible_generation)
            .ok_or(FdTableError::BadDescriptor)?;
        Ok(DescriptorToken {
            table: self.id,
            fd,
            generation,
        })
    }

    /// Revalidates a previously observed descriptor without returning a newer
    /// object that reused the same number.
    pub fn get_token(&self, token: DescriptorToken) -> Result<&DescriptorEntry<D>, FdTableError> {
        if token.table != self.id || token.fd.index() >= N {
            return Err(FdTableError::StaleToken);
        }
        let slot = &self.slots[token.fd.index()];
        if slot.visible_generation() != Some(token.generation) {
            return Err(FdTableError::StaleToken);
        }
        slot.visible_entry().ok_or(FdTableError::StaleToken)
    }

    /// Removes a visible descriptor and returns it for lock-external cleanup.
    pub fn close(&mut self, fd: FdNumber) -> Result<DescriptorEntry<D>, FdTableError> {
        let slot = self
            .slots
            .get_mut(fd.index())
            .ok_or(FdTableError::BadDescriptor)?;
        match core::mem::replace(slot, Slot::Vacant).into_visible_entry() {
            Ok(entry) => Ok(entry),
            Err(other) => {
                *slot = other;
                Err(FdTableError::BadDescriptor)
            }
        }
    }

    /// Closes only the exact generation previously observed.
    pub fn close_token(
        &mut self,
        token: DescriptorToken,
    ) -> Result<DescriptorEntry<D>, FdTableError> {
        self.get_token(token)?;
        self.close(token.fd)
    }

    /// Sets descriptor-local close-on-exec state.
    pub fn set_close_on_exec(&mut self, fd: FdNumber, enabled: bool) -> Result<(), FdTableError> {
        self.get_mut(fd)?
            .flags
            .set(DescriptorFlags::CLOSE_ON_EXEC, enabled);
        Ok(())
    }

    /// Marks every visible descriptor in an inclusive range close-on-exec.
    pub fn mark_close_on_exec_range(&mut self, first: FdNumber, last: FdNumber) {
        if N == 0 || last < first || first.index() >= N {
            return;
        }
        let end = last.index().min(N.saturating_sub(1));
        for fd in first.index().min(N)..=end {
            if let Some(entry) = self.slots[fd].visible_entry_mut() {
                entry.flags.set(DescriptorFlags::CLOSE_ON_EXEC, true);
            }
        }
    }
}

impl<D: Clone, const N: usize> FdTable<D, N> {
    /// Atomically replaces one exact descriptor number with `description`.
    ///
    /// This is the primitive used by cross-table descriptor injection: unlike
    /// `duplicate_replace`, the source description does not need to be
    /// present in this table.
    pub fn replace(
        &mut self,
        target: FdNumber,
        description: D,
        flags: DescriptorFlags,
    ) -> Result<(DescriptorToken, Option<DescriptorEntry<D>>), FdTableError> {
        if target.index() >= N {
            return Err(FdTableError::BadDescriptor);
        }
        if self.slots[target.index()].blocks_replacement() {
            return Err(FdTableError::Busy);
        }
        let generation = self.allocate_generation()?;
        let replacement = Slot::Occupied {
            generation,
            entry: DescriptorEntry::new(description, flags),
        };
        let previous = core::mem::replace(&mut self.slots[target.index()], replacement);
        let removed = match previous.into_visible_entry() {
            Ok(entry) => Some(entry),
            Err(Slot::Vacant) => None,
            Err(other) => {
                self.slots[target.index()] = other;
                return Err(FdTableError::Busy);
            }
        };
        Ok((
            DescriptorToken {
                table: self.id,
                fd: target,
                generation,
            },
            removed,
        ))
    }

    /// Duplicates a descriptor into the lowest free number in a bounded range.
    pub fn duplicate(
        &mut self,
        source: FdNumber,
        minimum: usize,
        limit: usize,
        flags: DescriptorFlags,
    ) -> Result<DescriptorToken, FdTableError> {
        let description = self.get(source)?.description.clone();
        let reservation = self.reserve(minimum, limit, flags)?;
        self.publish(reservation, description)
            .map_err(|error| error.error)
    }

    /// Atomically implements the table part of `dup2`/`dup3` replacement.
    ///
    /// The adapter handles the `old == new` difference between the two
    /// syscalls. A reserved target reports `Busy` and is never stolen.
    pub fn duplicate_replace(
        &mut self,
        source: FdNumber,
        target: FdNumber,
        flags: DescriptorFlags,
    ) -> Result<(DescriptorToken, Option<DescriptorEntry<D>>), FdTableError> {
        if source == target {
            return Ok((self.token(source)?, None));
        }
        if target.index() >= N {
            return Err(FdTableError::BadDescriptor);
        }
        if self.slots[target.index()].blocks_replacement() {
            return Err(FdTableError::Busy);
        }
        let description = self.get(source)?.description.clone();
        let generation = self.allocate_generation()?;
        let replacement = Slot::Occupied {
            generation,
            entry: DescriptorEntry::new(description, flags),
        };
        let previous = core::mem::replace(&mut self.slots[target.index()], replacement);
        let removed = match previous.into_visible_entry() {
            Ok(entry) => Some(entry),
            Err(Slot::Vacant) => None,
            Err(other) => {
                self.slots[target.index()] = other;
                return Err(FdTableError::Busy);
            }
        };
        Ok((
            DescriptorToken {
                table: self.id,
                fd: target,
                generation,
            },
            removed,
        ))
    }

    /// Copies visible descriptor entries for fork while sharing every OFD
    /// handle. Unpublished reservations are intentionally not inherited.
    pub fn fork_copy(&self, new_id: FdTableId) -> Result<Self, FdTableError> {
        let mut copy = Self::try_new(new_id)?;
        for (fd, slot) in self.slots.iter().enumerate() {
            if let Some(entry) = slot.visible_entry() {
                let generation = copy.allocate_generation()?;
                copy.slots[fd] = Slot::Occupied {
                    generation,
                    entry: entry.clone(),
                };
            }
        }
        Ok(copy)
    }
}

/// Preallocated owner for descriptors detached by one range/exec transaction.
#[cfg(feature = "alloc")]
pub struct CloseBatch<D> {
    entries: Vec<DescriptorEntry<D>>,
    limit: usize,
}

/// Full-capacity ownership prepared for one allocation-free close-on-exec
/// transaction on an `N`-slot table.
#[cfg(feature = "alloc")]
#[must_use = "a prepared close-on-exec batch should be committed or dropped"]
pub struct PreparedCloseOnExec<D, const N: usize> {
    batch: CloseBatch<D>,
}

/// Descriptors detached by an infallible full-capacity close-on-exec commit.
#[cfg(feature = "alloc")]
pub struct CommittedCloseOnExec<D> {
    batch: CloseBatch<D>,
}

#[cfg(feature = "alloc")]
impl<D> CommittedCloseOnExec<D> {
    /// Returns every detached descriptor in ascending table order.
    pub fn entries(&self) -> &[DescriptorEntry<D>] {
        self.batch.entries()
    }

    /// Consumes the transaction and returns detached descriptor ownership.
    pub fn into_entries(self) -> Vec<DescriptorEntry<D>> {
        self.batch.into_entries()
    }
}

#[cfg(feature = "alloc")]
impl<D> CloseBatch<D> {
    /// Fallibly reserves exact transaction storage before table mutation.
    pub fn try_with_capacity(capacity: usize) -> Result<Self, FdTableError> {
        if capacity == usize::MAX {
            return Err(FdTableError::Unbounded);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| FdTableError::NoMemory)?;
        Ok(Self {
            entries,
            limit: capacity,
        })
    }

    /// Returns detached descriptors for cleanup after releasing table locks.
    pub fn entries(&self) -> &[DescriptorEntry<D>] {
        &self.entries
    }

    /// Consumes the batch and returns all detached ownership.
    pub fn into_entries(self) -> Vec<DescriptorEntry<D>> {
        self.entries
    }

    fn remaining(&self) -> usize {
        self.limit - self.entries.len()
    }

    fn push(&mut self, entry: DescriptorEntry<D>) {
        self.entries.push(entry);
    }
}

#[cfg(feature = "alloc")]
impl<D, const N: usize> FdTable<D, N> {
    /// Preallocates the complete descriptor ownership required by a later
    /// close-on-exec commit, independent of how flags change before commit.
    pub fn prepare_close_on_exec(&self) -> Result<PreparedCloseOnExec<D, N>, FdTableError> {
        Ok(PreparedCloseOnExec {
            batch: CloseBatch::try_with_capacity(N)?,
        })
    }

    /// Detaches all descriptors currently marked close-on-exec using the
    /// full-capacity ownership prepared earlier. This operation allocates
    /// nothing and has no capacity failure.
    pub fn commit_close_on_exec(
        &mut self,
        mut prepared: PreparedCloseOnExec<D, N>,
    ) -> CommittedCloseOnExec<D> {
        for fd in 0..N {
            let should_remove = self.slots[fd]
                .visible_entry()
                .is_some_and(|entry| entry.flags.contains(DescriptorFlags::CLOSE_ON_EXEC));
            if should_remove {
                let previous = core::mem::replace(&mut self.slots[fd], Slot::Vacant);
                if let Ok(entry) = previous.into_visible_entry() {
                    prepared.batch.push(entry);
                }
            }
        }
        CommittedCloseOnExec {
            batch: prepared.batch,
        }
    }

    fn count_matching(
        &self,
        mut predicate: impl FnMut(usize, &DescriptorEntry<D>) -> bool,
    ) -> usize {
        self.slots
            .iter()
            .enumerate()
            .filter(|(fd, slot)| {
                slot.visible_entry()
                    .is_some_and(|entry| predicate(*fd, entry))
            })
            .count()
    }

    fn detach_matching(
        &mut self,
        batch: &mut CloseBatch<D>,
        mut predicate: impl FnMut(usize, &DescriptorEntry<D>) -> bool,
    ) -> Result<(), FdTableError> {
        let count = self.count_matching(|fd, entry| predicate(fd, entry));
        if count > batch.remaining() {
            return Err(FdTableError::InsufficientCloseStorage);
        }
        for fd in 0..N {
            let should_remove = self.slots[fd]
                .visible_entry()
                .is_some_and(|entry| predicate(fd, entry));
            if should_remove {
                let previous = core::mem::replace(&mut self.slots[fd], Slot::Vacant);
                if let Ok(entry) = previous.into_visible_entry() {
                    batch.push(entry);
                }
            }
        }
        Ok(())
    }

    /// Detaches an inclusive numeric range after proving batch capacity.
    pub fn close_range(
        &mut self,
        first: FdNumber,
        last: FdNumber,
        batch: &mut CloseBatch<D>,
    ) -> Result<(), FdTableError> {
        if last < first {
            return Err(FdTableError::BadDescriptor);
        }
        self.detach_matching(batch, |fd, _| fd >= first.index() && fd <= last.index())
    }

    /// Transactionally detaches every close-on-exec descriptor.
    pub fn close_on_exec(&mut self, batch: &mut CloseBatch<D>) -> Result<(), FdTableError> {
        self.detach_matching(batch, |_, entry| {
            entry.flags.contains(DescriptorFlags::CLOSE_ON_EXEC)
        })
    }

    /// Transactionally detaches every visible descriptor.
    pub fn close_all(&mut self, batch: &mut CloseBatch<D>) -> Result<(), FdTableError> {
        self.detach_matching(batch, |_, _| true)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    fn table_id(raw: u64) -> FdTableId {
        FdTableId::new(raw).unwrap()
    }

    #[test]
    fn reservation_is_invisible_and_publish_is_generation_tagged() {
        let mut table = FdTable::<Arc<u32>, 4>::try_new(table_id(1)).unwrap();
        let reservation = table.reserve(0, 4, DescriptorFlags::EMPTY).unwrap();
        assert_eq!(reservation.fd(), FdNumber::new(0));
        assert_eq!(
            table.get(FdNumber::new(0)),
            Err(FdTableError::BadDescriptor)
        );
        let token = table.publish(reservation, Arc::new(7)).unwrap();
        assert_eq!(**table.get_token(token).unwrap().description(), 7);
    }

    #[test]
    fn prepared_publication_is_invisible_until_infallible_commit() {
        let mut table = FdTable::<Arc<u32>, 2>::try_new(table_id(1)).unwrap();
        let reservation = table.reserve(0, 2, DescriptorFlags::CLOSE_ON_EXEC).unwrap();
        let publication = table.prepare_publication(reservation, Arc::new(7)).unwrap();
        assert_eq!(publication.fd(), FdNumber::new(0));
        assert_eq!(table.len(), 0);
        assert_eq!(
            table.get(FdNumber::new(0)),
            Err(FdTableError::BadDescriptor)
        );

        let token = publication.commit();
        let entry = table.get_token(token).unwrap();
        assert_eq!(**entry.description(), 7);
        assert_eq!(entry.flags(), DescriptorFlags::CLOSE_ON_EXEC);
    }

    #[test]
    fn prepared_cancellation_returns_entry_and_exact_slot() {
        let mut table = FdTable::<Arc<u32>, 1>::try_new(table_id(1)).unwrap();
        let reservation = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let publication = table.prepare_publication(reservation, Arc::new(7)).unwrap();
        let entry = table.cancel_prepared(publication).unwrap();
        assert_eq!(**entry.description(), 7);

        let reused = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        assert_eq!(reused.fd(), FdNumber::new(0));
        let reused = table.publish(reused, Arc::new(9)).unwrap();
        assert_eq!(**table.get_token(reused).unwrap().description(), 9);
    }

    #[test]
    fn foreign_prepare_and_cancel_preserve_exact_authority() {
        let mut one = FdTable::<Arc<u32>, 1>::try_new(table_id(1)).unwrap();
        let mut two = FdTable::<Arc<u32>, 1>::try_new(table_id(2)).unwrap();
        let reservation = one.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let description = Arc::new(7);
        let error = two
            .prepare_publication(reservation, description.clone())
            .unwrap_err();
        assert_eq!(error.error, FdTableError::StaleToken);
        assert!(Arc::ptr_eq(&error.description, &description));

        let publication = one
            .prepare_publication(error.reservation, error.description)
            .unwrap();
        let error = two.cancel_prepared(publication).unwrap_err();
        assert_eq!(error.error, FdTableError::StaleToken);
        let entry = one.cancel_prepared(error.publication).unwrap();
        assert!(Arc::ptr_eq(entry.description(), &description));
    }

    #[test]
    fn prepared_slot_cannot_be_stolen_by_dup_replace() {
        let mut table = FdTable::<Arc<u32>, 2>::try_new(table_id(1)).unwrap();
        let source = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let source = table.publish(source, Arc::new(1)).unwrap();
        let target = table.reserve(1, 2, DescriptorFlags::CLOSE_ON_EXEC).unwrap();
        let publication = table.prepare_publication(target, Arc::new(2)).unwrap();

        assert_eq!(
            table.duplicate_replace(source.fd(), publication.fd(), DescriptorFlags::EMPTY),
            Err(FdTableError::Busy)
        );
        let target = publication.commit();
        assert_eq!(**table.get_token(target).unwrap().description(), 2);
    }

    #[test]
    fn committed_prepared_entries_follow_close_exec_and_fork_rules() {
        let mut table = FdTable::<Arc<u32>, 2>::try_new(table_id(1)).unwrap();
        let reservation = table.reserve(0, 2, DescriptorFlags::CLOSE_ON_EXEC).unwrap();
        let publication = table.prepare_publication(reservation, Arc::new(7)).unwrap();
        let token = publication.commit();

        let fork = table.fork_copy(table_id(2)).unwrap();
        assert_eq!(**fork.get(token.fd()).unwrap().description(), 7);

        let mut batch = CloseBatch::try_with_capacity(1).unwrap();
        table.close_on_exec(&mut batch).unwrap();
        assert!(table.is_empty());
        assert_eq!(**batch.entries()[0].description(), 7);
    }

    #[test]
    fn alloc_table_handle_stays_small_for_a_kernel_sized_ceiling() {
        assert!(core::mem::size_of::<FdTable<Arc<u32>, 1024>>() <= 64);
        let table = FdTable::<Arc<u32>, 1024>::try_new(table_id(1)).unwrap();
        assert_eq!(table.capacity(), 1024);
    }

    #[test]
    fn iterator_is_ordered_and_skips_unpublished_reservations() {
        let mut table = FdTable::<Arc<u32>, 8>::try_new(table_id(1)).unwrap();
        let unpublished = table.reserve(0, 8, DescriptorFlags::EMPTY).unwrap();
        let high = table.reserve(7, 8, DescriptorFlags::EMPTY).unwrap();
        table.publish(high, Arc::new(7)).unwrap();
        let middle = table.reserve(3, 7, DescriptorFlags::EMPTY).unwrap();
        table.publish(middle, Arc::new(3)).unwrap();

        let visible = table
            .iter()
            .map(|(fd, entry)| (fd.get(), **entry.description()))
            .collect::<alloc::vec::Vec<_>>();
        assert_eq!(visible, alloc::vec![(3, 3), (7, 7)]);
        table.cancel_reservation(unpublished).unwrap();
    }

    #[test]
    fn effectively_unbounded_table_is_rejected_before_allocation() {
        assert!(matches!(
            FdTable::<Arc<u32>, { usize::MAX }>::try_new(table_id(1)),
            Err(FdTableError::Unbounded)
        ));
    }

    #[test]
    fn foreign_publication_returns_unpublished_ownership() {
        let mut one = FdTable::<Arc<u32>, 2>::try_new(table_id(1)).unwrap();
        let mut two = FdTable::<Arc<u32>, 2>::try_new(table_id(2)).unwrap();
        let reservation = one.reserve(0, 2, DescriptorFlags::EMPTY).unwrap();
        let description = Arc::new(9);
        let error = two.publish(reservation, description.clone()).unwrap_err();
        assert_eq!(error.error, FdTableError::StaleToken);
        assert!(Arc::ptr_eq(&error.description, &description));
    }

    #[test]
    fn stale_token_cannot_close_a_reused_number() {
        let mut table = FdTable::<Arc<u32>, 1>::try_new(table_id(1)).unwrap();
        let first = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let first = table.publish(first, Arc::new(1)).unwrap();
        drop(table.close_token(first).unwrap());
        let second = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let second = table.publish(second, Arc::new(2)).unwrap();
        assert_eq!(table.close_token(first), Err(FdTableError::StaleToken));
        assert_eq!(**table.get_token(second).unwrap().description(), 2);
    }

    #[test]
    fn dup_and_fork_share_ofd_handle_but_copy_descriptor_flags() {
        let mut table = FdTable::<Arc<u32>, 4>::try_new(table_id(1)).unwrap();
        let source = table.reserve(0, 4, DescriptorFlags::EMPTY).unwrap();
        let source = table.publish(source, Arc::new(3)).unwrap();
        let duplicate = table
            .duplicate(source.fd(), 0, 4, DescriptorFlags::CLOSE_ON_EXEC)
            .unwrap();
        assert!(Arc::ptr_eq(
            table.get_token(source).unwrap().description(),
            table.get_token(duplicate).unwrap().description(),
        ));
        assert_ne!(
            table.get_token(source).unwrap().flags(),
            table.get_token(duplicate).unwrap().flags(),
        );

        let fork = table.fork_copy(table_id(2)).unwrap();
        assert_eq!(fork.iter().count(), 2);
        assert!(Arc::ptr_eq(
            table.get(source.fd()).unwrap().description(),
            fork.get(source.fd()).unwrap().description(),
        ));
    }

    #[test]
    fn close_batch_capacity_failure_is_atomic() {
        let mut table = FdTable::<Arc<u32>, 4>::try_new(table_id(1)).unwrap();
        for _ in 0..2 {
            let reservation = table.reserve(0, 4, DescriptorFlags::CLOSE_ON_EXEC).unwrap();
            table.publish(reservation, Arc::new(1)).unwrap();
        }
        let mut too_small = CloseBatch::try_with_capacity(1).unwrap();
        assert_eq!(
            table.close_on_exec(&mut too_small),
            Err(FdTableError::InsufficientCloseStorage)
        );
        assert_eq!(table.len(), 2);

        let mut exact = CloseBatch::try_with_capacity(2).unwrap();
        table.close_on_exec(&mut exact).unwrap();
        assert_eq!(exact.entries().len(), 2);
        assert!(table.is_empty());
    }

    #[test]
    fn full_cloexec_preparation_covers_flags_and_entries_added_before_commit() {
        let mut table = FdTable::<Arc<u32>, 3>::try_new(table_id(1)).unwrap();
        let keep = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        table.publish(keep, Arc::new(1)).unwrap();
        let prepared = table.prepare_close_on_exec().unwrap();

        table.set_close_on_exec(FdNumber::new(0), true).unwrap();
        let later = table.reserve(1, 3, DescriptorFlags::CLOSE_ON_EXEC).unwrap();
        table.publish(later, Arc::new(2)).unwrap();

        let committed = table.commit_close_on_exec(prepared);
        let values = committed
            .entries()
            .iter()
            .map(|entry| **entry.description())
            .collect::<alloc::vec::Vec<_>>();
        assert_eq!(values, alloc::vec![1, 2]);
        assert!(table.is_empty());
    }

    #[test]
    fn close_batch_rejects_an_effectively_unbounded_request() {
        assert!(matches!(
            CloseBatch::<Arc<u32>>::try_with_capacity(usize::MAX),
            Err(FdTableError::Unbounded)
        ));
    }

    #[test]
    fn dup_replace_never_steals_a_reserved_target() {
        let mut table = FdTable::<Arc<u32>, 2>::try_new(table_id(1)).unwrap();
        let source = table.reserve(0, 1, DescriptorFlags::EMPTY).unwrap();
        let source = table.publish(source, Arc::new(1)).unwrap();
        let target = table.reserve(1, 2, DescriptorFlags::EMPTY).unwrap();
        assert_eq!(
            table.duplicate_replace(source.fd(), target.fd(), DescriptorFlags::EMPTY),
            Err(FdTableError::Busy)
        );
        table.cancel_reservation(target).unwrap();
    }
}
