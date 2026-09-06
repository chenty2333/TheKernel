use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroU64;

use crate::{IORING_MAX_CQ_ENTRIES, IORING_MAX_FIXED_FILES, IoUringError, RingId};

/// Caller-allocated nonzero identity for one fixed-file registration epoch.
///
/// A ring must never reuse an identity, including after unregister followed by
/// a new `REGISTER_FILES`, so old lease tokens cannot match a rebuilt table.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileTableId(NonZeroU64);

impl FileTableId {
    /// Builds a registration-epoch identity, rejecting zero.
    pub const fn new(raw: u64) -> Result<Self, IoUringError> {
        match NonZeroU64::new(raw) {
            Some(raw) => Ok(Self(raw)),
            None => Err(IoUringError::InvalidIdentity),
        }
    }

    /// Raw consumer identity value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Bounded fixed-file table index copied from an SQE.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileSlot(u32);

impl FileSlot {
    /// Builds a slot value; table lookup performs capacity validation.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Raw Linux fixed-file index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Generation-safe identity of one installed registered file.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegisteredFileToken {
    ring: RingId,
    table: FileTableId,
    slot: FileSlot,
    generation: NonZeroU64,
}

impl RegisteredFileToken {
    const fn new(ring: RingId, table: FileTableId, slot: FileSlot, generation: NonZeroU64) -> Self {
        Self {
            ring,
            table,
            slot,
            generation,
        }
    }

    /// Ring which owns this resource identity.
    pub const fn ring(self) -> RingId {
        self.ring
    }

    /// Non-reused registration epoch which owns the slot.
    pub const fn table(self) -> FileTableId {
        self.table
    }

    /// Fixed-file slot.
    pub const fn slot(self) -> FileSlot {
        self.slot
    }

    /// Nonzero slot generation.
    pub const fn generation(self) -> u64 {
        self.generation.get()
    }
}

/// Table publication and retirement phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTableLifecycle {
    /// Slots may be populated but are not lookup-visible.
    Building,
    /// Installed slots are visible to fixed-file SQEs.
    Published,
    /// Lookups are stopped while outstanding leases release.
    Retiring,
    /// Every owner and lease has left the table.
    Closed,
}

/// Finite table snapshot for registration and close progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileTableProgress {
    lifecycle: FileTableLifecycle,
    capacity: u32,
    installed: u32,
    leases: u32,
    lease_capacity: u32,
}

impl FileTableProgress {
    /// Current publication/retirement phase.
    pub const fn lifecycle(self) -> FileTableLifecycle {
        self.lifecycle
    }

    /// Fixed slot capacity.
    pub const fn capacity(self) -> u32 {
        self.capacity
    }

    /// Owners still retained by slots, including retiring owners.
    pub const fn installed(self) -> u32 {
        self.installed
    }

    /// Outstanding execution leases.
    pub const fn leases(self) -> u32 {
        self.leases
    }

    /// Maximum simultaneous request-owned file leases.
    pub const fn lease_capacity(self) -> u32 {
        self.lease_capacity
    }

    /// No slot owner or external lease remains.
    pub const fn empty(self) -> bool {
        self.installed == 0 && self.leases == 0
    }
}

#[derive(Debug)]
struct ResourceSlot<F> {
    generation: Option<NonZeroU64>,
    owner: Option<Arc<F>>,
    leases: u32,
}

/// Exact registered-file ownership retained by one accepted request.
#[derive(Debug)]
#[must_use = "a registered-file lease must be released through its table"]
pub struct RegisteredFileLease<F> {
    token: RegisteredFileToken,
    owner: Arc<F>,
}

impl<F> RegisteredFileLease<F> {
    /// Generation-safe installed resource identity.
    pub const fn token(&self) -> RegisteredFileToken {
        self.token
    }

    /// Exact retained file description, independent of numeric FD reuse.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }
}

/// File owner removed from lookup and proven free of request leases.
#[derive(Debug)]
#[must_use = "retired file destruction should occur outside policy/adapter locks"]
pub struct RetiredFile<F> {
    token: RegisteredFileToken,
    owner: Arc<F>,
}

/// Failed slot installation with ownership returned for lock-external cleanup.
#[derive(Debug)]
pub struct FileInstallError<F> {
    error: IoUringError,
    owner: Arc<F>,
}

impl<F> FileInstallError<F> {
    /// Typed policy failure.
    pub const fn error(&self) -> IoUringError {
        self.error
    }

    /// Owner which was not installed.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }

    /// Recovers ownership for destruction or rollback outside the table lock.
    pub fn into_owner(self) -> Arc<F> {
        self.owner
    }
}

impl<F> RetiredFile<F> {
    /// Exact retired slot generation.
    pub const fn token(&self) -> RegisteredFileToken {
        self.token
    }

    /// Retained owner to inspect before destruction.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }

    /// Transfers the final table owner for lock-external destruction.
    pub fn into_owner(self) -> Arc<F> {
        self.owner
    }
}

/// Result of releasing one registered-file execution lease.
#[derive(Debug)]
pub enum LeaseRelease<F> {
    /// The published slot still retains its owner.
    Active,
    /// Retirement was waiting for this last lease and returned the owner.
    Retired(RetiredFile<F>),
}

/// Failed lease release with the exact lease returned to its caller.
#[derive(Debug)]
pub struct LeaseReleaseError<F> {
    error: IoUringError,
    lease: RegisteredFileLease<F>,
}

impl<F> LeaseReleaseError<F> {
    /// Typed policy failure.
    pub const fn error(&self) -> IoUringError {
        self.error
    }

    /// Lease which was not consumed.
    pub fn lease(&self) -> &RegisteredFileLease<F> {
        &self.lease
    }

    /// Recovers the lease for retry against its originating table.
    pub fn into_lease(self) -> RegisteredFileLease<F> {
        self.lease
    }
}

/// Fixed-capacity registered-file ownership and retirement state.
///
/// Slots own real `Arc<F>` values. Acquiring a lease clones only the `Arc`, so
/// accepted work remains bound to the exact open file description after the
/// source numeric descriptor is closed or reused. Construction is the only
/// allocation performed by this table.
#[derive(Debug)]
pub struct RegisteredFileTable<F> {
    ring: RingId,
    id: FileTableId,
    lifecycle: FileTableLifecycle,
    slots: Vec<ResourceSlot<F>>,
    retire_cursor: usize,
    lease_capacity: u32,
    leases: u32,
}

impl<F> RegisteredFileTable<F> {
    /// Allocates an unpublished fixed-capacity table.
    pub fn new(
        ring: RingId,
        id: FileTableId,
        capacity: u32,
        lease_capacity: u32,
    ) -> Result<Self, IoUringError> {
        if capacity == 0 || capacity > IORING_MAX_FIXED_FILES {
            return Err(IoUringError::InvalidFileTableCapacity);
        }
        if lease_capacity == 0 || lease_capacity > IORING_MAX_CQ_ENTRIES {
            return Err(IoUringError::InvalidFileLeaseCapacity);
        }
        let capacity = usize::try_from(capacity).map_err(|_| IoUringError::Overflow)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| IoUringError::AllocationFailed)?;
        for _ in 0..capacity {
            slots.push(ResourceSlot {
                generation: NonZeroU64::new(1),
                owner: None,
                leases: 0,
            });
        }
        Ok(Self {
            ring,
            id,
            lifecycle: FileTableLifecycle::Building,
            slots,
            retire_cursor: 0,
            lease_capacity,
            leases: 0,
        })
    }

    /// Non-reused registration epoch carried by every table token.
    pub const fn id(&self) -> FileTableId {
        self.id
    }

    /// Installs one exact file owner before table publication.
    pub fn install(
        &mut self,
        slot: FileSlot,
        owner: Arc<F>,
    ) -> Result<RegisteredFileToken, FileInstallError<F>> {
        if self.lifecycle != FileTableLifecycle::Building {
            let error = match self.lifecycle {
                FileTableLifecycle::Building => unreachable!(),
                FileTableLifecycle::Published => IoUringError::Busy,
                FileTableLifecycle::Retiring => IoUringError::Closing,
                FileTableLifecycle::Closed => IoUringError::Closed,
            };
            return Err(FileInstallError { error, owner });
        }
        let ring = self.ring;
        let id = self.id;
        let table_slot = match self.slot_mut(slot) {
            Ok(table_slot) => table_slot,
            Err(error) => return Err(FileInstallError { error, owner }),
        };
        if table_slot.owner.is_some() {
            return Err(FileInstallError {
                error: IoUringError::FileSlotOccupied,
                owner,
            });
        }
        let generation = match table_slot.generation {
            Some(generation) => generation,
            None => {
                return Err(FileInstallError {
                    error: IoUringError::GenerationExhausted,
                    owner,
                });
            }
        };
        table_slot.owner = Some(owner);
        Ok(RegisteredFileToken::new(ring, id, slot, generation))
    }

    /// Makes every installed owner lookup-visible in one policy transition.
    pub fn publish(&mut self) -> Result<FileTableProgress, IoUringError> {
        match self.lifecycle {
            FileTableLifecycle::Building => self.lifecycle = FileTableLifecycle::Published,
            FileTableLifecycle::Published => {}
            FileTableLifecycle::Retiring => return Err(IoUringError::Closing),
            FileTableLifecycle::Closed => return Err(IoUringError::Closed),
        }
        self.progress()
    }

    /// Returns the current exact token for an installed, published slot.
    pub fn token(&self, slot: FileSlot) -> Result<RegisteredFileToken, IoUringError> {
        if self.lifecycle != FileTableLifecycle::Published {
            return Err(match self.lifecycle {
                FileTableLifecycle::Building => IoUringError::FileTableNotPublished,
                FileTableLifecycle::Published => unreachable!(),
                FileTableLifecycle::Retiring => IoUringError::Closing,
                FileTableLifecycle::Closed => IoUringError::Closed,
            });
        }
        let table_slot = self.slot(slot)?;
        if table_slot.owner.is_none() {
            return Err(IoUringError::FileSlotEmpty);
        }
        let generation = table_slot
            .generation
            .ok_or(IoUringError::GenerationExhausted)?;
        Ok(RegisteredFileToken::new(
            self.ring, self.id, slot, generation,
        ))
    }

    /// Acquires a request-owned lease by slot.
    pub fn acquire(&mut self, slot: FileSlot) -> Result<RegisteredFileLease<F>, IoUringError> {
        let token = self.token(slot)?;
        self.acquire_token(token)
    }

    /// Acquires the current slot in one exclusive table transaction. Unlike
    /// acquire_token, no externally supplied token needs revalidation after
    /// token() has just checked this same table under the exclusive borrow.
    pub fn acquire_current(
        &mut self,
        slot: FileSlot,
    ) -> Result<RegisteredFileLease<F>, IoUringError> {
        let token = self.token(slot)?;
        self.acquire_validated(token)
    }

    pub fn acquire_token(
        &mut self,
        token: RegisteredFileToken,
    ) -> Result<RegisteredFileLease<F>, IoUringError> {
        if self.lifecycle != FileTableLifecycle::Published {
            return Err(match self.lifecycle {
                FileTableLifecycle::Building => IoUringError::FileTableNotPublished,
                FileTableLifecycle::Published => unreachable!(),
                FileTableLifecycle::Retiring => IoUringError::Closing,
                FileTableLifecycle::Closed => IoUringError::Closed,
            });
        }
        self.validate_token(token)?;
        self.acquire_validated(token)
    }

    fn acquire_validated(
        &mut self,
        token: RegisteredFileToken,
    ) -> Result<RegisteredFileLease<F>, IoUringError> {
        if self.leases >= self.lease_capacity {
            return Err(IoUringError::FileLeaseCapacityExceeded);
        }
        let table_slot = self.slot_mut(token.slot)?;
        let owner = Arc::clone(
            table_slot
                .owner
                .as_ref()
                .ok_or(IoUringError::FileSlotEmpty)?,
        );
        table_slot.leases = table_slot
            .leases
            .checked_add(1)
            .ok_or(IoUringError::Overflow)?;
        self.leases = self.leases.checked_add(1).ok_or(IoUringError::Overflow)?;
        Ok(RegisteredFileLease { token, owner })
    }

    /// Stops new lookups and marks every installed slot for retirement.
    pub fn begin_retire(&mut self) -> Result<FileTableProgress, IoUringError> {
        match self.lifecycle {
            FileTableLifecycle::Building | FileTableLifecycle::Published => {
                self.lifecycle = FileTableLifecycle::Retiring;
                self.retire_cursor = 0;
            }
            FileTableLifecycle::Retiring => {}
            FileTableLifecycle::Closed => return self.progress(),
        }
        self.progress()
    }

    /// Removes one retirement-ready owner without destroying it in the table.
    ///
    /// A slot with outstanding leases reports `Busy`; its last lease release
    /// will instead return the retired owner.
    pub fn retire(&mut self, token: RegisteredFileToken) -> Result<RetiredFile<F>, IoUringError> {
        if self.lifecycle != FileTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        self.validate_token(token)?;
        let table_slot = self.slot_mut(token.slot)?;
        if table_slot.leases != 0 {
            return Err(IoUringError::Busy);
        }
        let owner = table_slot.owner.take().ok_or(IoUringError::FileSlotEmpty)?;
        advance_generation(table_slot, token.generation);
        Ok(RetiredFile { token, owner })
    }

    /// Returns the next installed slot that can retire immediately.
    ///
    /// Each slot is inspected at most once across the retirement pass. Slots
    /// skipped for live leases are completed by their last `release` call.
    pub fn next_retirable(&mut self) -> Result<Option<RegisteredFileToken>, IoUringError> {
        if self.lifecycle != FileTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        while self.retire_cursor < self.slots.len() {
            let index = self.retire_cursor;
            self.retire_cursor += 1;
            let slot = &self.slots[index];
            if slot.owner.is_some() && slot.leases == 0 {
                let generation = slot.generation.ok_or(IoUringError::GenerationExhausted)?;
                return Ok(Some(RegisteredFileToken::new(
                    self.ring,
                    self.id,
                    FileSlot::new(u32::try_from(index).map_err(|_| IoUringError::Overflow)?),
                    generation,
                )));
            }
        }
        Ok(None)
    }

    /// Releases one execution lease and, when last, transfers a retiring owner.
    pub fn release(
        &mut self,
        lease: RegisteredFileLease<F>,
    ) -> Result<LeaseRelease<F>, LeaseReleaseError<F>> {
        let token = lease.token;
        if let Err(error) = self.validate_token(token) {
            return Err(LeaseReleaseError { error, lease });
        }
        if self.leases == 0 {
            return Err(LeaseReleaseError {
                error: IoUringError::UnknownFileLease,
                lease,
            });
        }
        let retiring = self.lifecycle == FileTableLifecycle::Retiring;
        let table_slot = match self.slot_mut(token.slot) {
            Ok(table_slot) => table_slot,
            Err(error) => return Err(LeaseReleaseError { error, lease }),
        };
        if table_slot.leases == 0 {
            return Err(LeaseReleaseError {
                error: IoUringError::UnknownFileLease,
                lease,
            });
        }
        let retired_owner = if retiring && table_slot.leases == 1 {
            match table_slot.owner.take() {
                Some(owner) => {
                    table_slot.leases = 0;
                    advance_generation(table_slot, token.generation);
                    Some(owner)
                }
                None => {
                    return Err(LeaseReleaseError {
                        error: IoUringError::FileSlotEmpty,
                        lease,
                    });
                }
            }
        } else {
            table_slot.leases -= 1;
            None
        };
        self.leases -= 1;

        if let Some(owner) = retired_owner {
            // The table owner keeps the object alive while the lease's Arc is
            // dropped here. Return that owner for lock-external destruction.
            drop(lease);
            Ok(LeaseRelease::Retired(RetiredFile { token, owner }))
        } else {
            drop(lease);
            Ok(LeaseRelease::Active)
        }
    }

    /// Completes unregister/close after all owners and leases have retired.
    pub fn finish_retire(&mut self) -> Result<(), IoUringError> {
        if self.lifecycle == FileTableLifecycle::Closed {
            return Ok(());
        }
        if self.lifecycle != FileTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        if !self.progress()?.empty() {
            return Err(IoUringError::Busy);
        }
        self.lifecycle = FileTableLifecycle::Closed;
        Ok(())
    }

    /// Returns finite resource-retirement progress.
    pub fn progress(&self) -> Result<FileTableProgress, IoUringError> {
        let mut installed = 0_u32;
        for slot in &self.slots {
            if slot.owner.is_some() {
                installed = installed.checked_add(1).ok_or(IoUringError::Overflow)?;
            }
        }
        Ok(FileTableProgress {
            lifecycle: self.lifecycle,
            capacity: u32::try_from(self.slots.len()).map_err(|_| IoUringError::Overflow)?,
            installed,
            leases: self.leases,
            lease_capacity: self.lease_capacity,
        })
    }

    fn slot(&self, slot: FileSlot) -> Result<&ResourceSlot<F>, IoUringError> {
        self.slots
            .get(usize::try_from(slot.0).map_err(|_| IoUringError::InvalidFileSlot)?)
            .ok_or(IoUringError::InvalidFileSlot)
    }

    fn slot_mut(&mut self, slot: FileSlot) -> Result<&mut ResourceSlot<F>, IoUringError> {
        self.slots
            .get_mut(usize::try_from(slot.0).map_err(|_| IoUringError::InvalidFileSlot)?)
            .ok_or(IoUringError::InvalidFileSlot)
    }

    fn validate_token(&self, token: RegisteredFileToken) -> Result<(), IoUringError> {
        if token.ring != self.ring || token.table != self.id {
            return Err(IoUringError::UnknownFileLease);
        }
        let slot = self.slot(token.slot)?;
        if slot.generation != Some(token.generation) || slot.owner.is_none() {
            return Err(IoUringError::UnknownFileLease);
        }
        Ok(())
    }
}

fn advance_generation<F>(slot: &mut ResourceSlot<F>, generation: NonZeroU64) {
    slot.generation = generation.get().checked_add(1).and_then(NonZeroU64::new);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct File(u32);

    fn ring(raw: u64) -> RingId {
        RingId::new(raw).unwrap()
    }

    fn table(raw: u64) -> FileTableId {
        FileTableId::new(raw).unwrap()
    }

    #[test]
    fn table_is_not_visible_until_atomic_publication() {
        let mut table = RegisteredFileTable::new(ring(1), table(1), 2, 2).unwrap();
        let token = table.install(FileSlot::new(0), Arc::new(File(7))).unwrap();
        assert!(matches!(
            table.acquire_token(token),
            Err(IoUringError::FileTableNotPublished)
        ));
        table.publish().unwrap();
        let lease = table.acquire_token(token).unwrap();
        assert_eq!(lease.owner().0, 7);
        assert!(matches!(
            table.release(lease).unwrap(),
            LeaseRelease::Active
        ));
    }

    #[test]
    fn current_slot_acquisition_keeps_independent_leases_and_retirement() {
        let mut files = RegisteredFileTable::new(ring(1), table(1), 1, 2).unwrap();
        let token = files.install(FileSlot::new(0), Arc::new(File(7))).unwrap();
        files.publish().unwrap();
        let first = files.acquire_current(FileSlot::new(0)).unwrap();
        let second = files.acquire_current(FileSlot::new(0)).unwrap();
        assert_eq!(first.token(), token);
        assert!(matches!(
            files.acquire_current(FileSlot::new(0)),
            Err(IoUringError::FileLeaseCapacityExceeded)
        ));
        files.begin_retire().unwrap();
        assert!(matches!(
            files.acquire_current(FileSlot::new(0)),
            Err(IoUringError::Closing)
        ));
        files.release(first).unwrap();
        assert!(matches!(files.retire(token), Err(IoUringError::Busy)));
        assert!(matches!(
            files.release(second).unwrap(),
            LeaseRelease::Retired(_)
        ));
        files.finish_retire().unwrap();
    }

    #[test]
    fn lease_retains_exact_owner_after_retirement_starts() {
        let mut table = RegisteredFileTable::new(ring(1), table(1), 1, 1).unwrap();
        let token = table.install(FileSlot::new(0), Arc::new(File(7))).unwrap();
        table.publish().unwrap();
        let lease = table.acquire(FileSlot::new(0)).unwrap();
        table.begin_retire().unwrap();
        assert!(matches!(
            table.acquire(FileSlot::new(0)),
            Err(IoUringError::Closing)
        ));
        assert!(matches!(table.retire(token), Err(IoUringError::Busy)));
        let LeaseRelease::Retired(retired) = table.release(lease).unwrap() else {
            panic!("last retiring lease must return the owner");
        };
        assert_eq!(retired.token(), token);
        assert_eq!(retired.owner().0, 7);
        table.finish_retire().unwrap();
    }

    #[test]
    fn empty_and_sparse_slots_retire_incrementally() {
        let mut table = RegisteredFileTable::new(ring(1), table(1), 3, 3).unwrap();
        let first = table.install(FileSlot::new(0), Arc::new(File(1))).unwrap();
        let last = table.install(FileSlot::new(2), Arc::new(File(3))).unwrap();
        table.publish().unwrap();
        table.begin_retire().unwrap();
        assert_eq!(table.next_retirable().unwrap(), Some(first));
        assert_eq!(table.retire(first).unwrap().owner().0, 1);
        assert_eq!(table.next_retirable().unwrap(), Some(last));
        assert_eq!(table.retire(last).unwrap().owner().0, 3);
        assert_eq!(table.next_retirable().unwrap(), None);
        table.finish_retire().unwrap();
    }

    #[test]
    fn tokens_are_ring_and_generation_scoped() {
        let mut first = RegisteredFileTable::new(ring(1), table(1), 1, 1).unwrap();
        let mut second = RegisteredFileTable::new(ring(2), table(1), 1, 1).unwrap();
        let token = first.install(FileSlot::new(0), Arc::new(File(1))).unwrap();
        first.publish().unwrap();
        second.install(FileSlot::new(0), Arc::new(File(2))).unwrap();
        second.publish().unwrap();
        assert!(matches!(
            second.acquire_token(token),
            Err(IoUringError::UnknownFileLease)
        ));
    }

    #[test]
    fn construction_rejects_unbounded_or_zero_tables() {
        assert!(matches!(
            RegisteredFileTable::<File>::new(ring(1), table(1), 0, 1),
            Err(IoUringError::InvalidFileTableCapacity)
        ));
        assert!(matches!(
            RegisteredFileTable::<File>::new(ring(1), table(1), IORING_MAX_FIXED_FILES + 1, 1),
            Err(IoUringError::InvalidFileTableCapacity)
        ));
        assert!(matches!(
            RegisteredFileTable::<File>::new(ring(1), table(1), 1, 0),
            Err(IoUringError::InvalidFileLeaseCapacity)
        ));
        assert!(matches!(
            RegisteredFileTable::<File>::new(ring(1), table(1), 1, IORING_MAX_CQ_ENTRIES + 1),
            Err(IoUringError::InvalidFileLeaseCapacity)
        ));
    }

    #[test]
    fn lease_admission_obeys_the_table_level_budget() {
        let mut table = RegisteredFileTable::new(ring(1), table(1), 1, 1).unwrap();
        table.install(FileSlot::new(0), Arc::new(File(1))).unwrap();
        table.publish().unwrap();
        let lease = table.acquire(FileSlot::new(0)).unwrap();
        assert!(matches!(
            table.acquire(FileSlot::new(0)),
            Err(IoUringError::FileLeaseCapacityExceeded)
        ));
        assert_eq!(table.progress().unwrap().leases(), 1);
        assert_eq!(table.progress().unwrap().lease_capacity(), 1);
        assert!(matches!(table.release(lease), Ok(LeaseRelease::Active)));
    }

    #[test]
    fn failed_install_and_wrong_table_release_return_ownership() {
        let mut first = RegisteredFileTable::new(ring(1), table(1), 1, 1).unwrap();
        first.install(FileSlot::new(0), Arc::new(File(1))).unwrap();
        let rejected = Arc::new(File(2));
        let rejected_ptr = Arc::as_ptr(&rejected);
        let install_error = first
            .install(FileSlot::new(0), rejected)
            .expect_err("occupied slot must reject install");
        assert_eq!(install_error.error(), IoUringError::FileSlotOccupied);
        assert_eq!(Arc::as_ptr(install_error.owner()), rejected_ptr);
        drop(install_error.into_owner());

        first.publish().unwrap();
        let lease = first.acquire(FileSlot::new(0)).unwrap();
        let mut other = RegisteredFileTable::new(ring(1), table(2), 1, 1).unwrap();
        other.install(FileSlot::new(0), Arc::new(File(3))).unwrap();
        other.publish().unwrap();
        let release_error = other
            .release(lease)
            .expect_err("foreign table must return the lease");
        assert_eq!(release_error.error(), IoUringError::UnknownFileLease);
        assert!(matches!(
            first.release(release_error.into_lease()),
            Ok(LeaseRelease::Active)
        ));
    }

    #[test]
    fn registration_epoch_prevents_rebuild_aba_within_one_ring() {
        let mut first = RegisteredFileTable::new(ring(1), table(1), 1, 1).unwrap();
        let token = first.install(FileSlot::new(0), Arc::new(File(1))).unwrap();
        first.publish().unwrap();

        let mut rebuilt = RegisteredFileTable::new(ring(1), table(2), 1, 1).unwrap();
        rebuilt
            .install(FileSlot::new(0), Arc::new(File(2)))
            .unwrap();
        rebuilt.publish().unwrap();
        assert!(matches!(
            rebuilt.acquire_token(token),
            Err(IoUringError::UnknownFileLease)
        ));
    }
}
