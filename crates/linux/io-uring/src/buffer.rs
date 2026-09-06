use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroU64;

use crate::{IORING_MAX_CQ_ENTRIES, IORING_MAX_REGISTERED_BUFFERS, IoUringError, RingId};

/// Caller-allocated nonzero identity for one registered-buffer table epoch.
///
/// A ring must allocate a fresh identity after every unregister/register pair;
/// this keeps an old request lease from becoming valid for a rebuilt slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BufferTableId(NonZeroU64);

impl BufferTableId {
    /// Builds a table identity, rejecting the reserved zero value.
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

/// Bounded registered-buffer slot index copied from an SQE.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BufferSlot(u32);

impl BufferSlot {
    /// Builds a slot value; lookup performs table-capacity validation.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Raw Linux fixed-buffer index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Generation-safe identity for one registered-buffer slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegisteredBufferToken {
    ring: RingId,
    table: BufferTableId,
    slot: BufferSlot,
    generation: NonZeroU64,
}

impl RegisteredBufferToken {
    const fn new(
        ring: RingId,
        table: BufferTableId,
        slot: BufferSlot,
        generation: NonZeroU64,
    ) -> Self {
        Self {
            ring,
            table,
            slot,
            generation,
        }
    }

    /// Ring which owns this identity.
    pub const fn ring(self) -> RingId {
        self.ring
    }

    /// Registration epoch which owns this slot.
    pub const fn table(self) -> BufferTableId {
        self.table
    }

    /// Fixed-buffer slot.
    pub const fn slot(self) -> BufferSlot {
        self.slot
    }

    /// Nonzero slot generation.
    pub const fn generation(self) -> u64 {
        self.generation.get()
    }
}

/// Exact virtual subrange selected by one fixed-buffer request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredBufferRange {
    address: u64,
    length: u32,
}

impl RegisteredBufferRange {
    /// Checked range constructor.
    pub const fn new(address: u64, length: u32) -> Result<Self, IoUringError> {
        if address.checked_add(length as u64).is_none() {
            return Err(IoUringError::InvalidBufferRange);
        }
        Ok(Self { address, length })
    }

    /// Selected userspace address.
    pub const fn address(self) -> u64 {
        self.address
    }

    /// Selected byte length.
    pub const fn length(self) -> u32 {
        self.length
    }

    /// Exclusive selected address.
    pub const fn end(self) -> u64 {
        self.address + self.length as u64
    }
}

/// Table publication and retirement phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferTableLifecycle {
    /// Slots may be populated but are not lookup-visible.
    Building,
    /// Installed slots are visible to fixed-buffer SQEs.
    Published,
    /// Lookups are stopped while outstanding leases release.
    Retiring,
    /// Every owner and lease has left the table.
    Closed,
}

/// Finite table snapshot for registration and close progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferTableProgress {
    lifecycle: BufferTableLifecycle,
    capacity: u32,
    installed: u32,
    leases: u32,
    lease_capacity: u32,
}

impl BufferTableProgress {
    /// Current publication/retirement phase.
    pub const fn lifecycle(self) -> BufferTableLifecycle {
        self.lifecycle
    }

    /// Fixed-buffer slot capacity.
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

    /// Maximum simultaneous request-owned leases.
    pub const fn lease_capacity(self) -> u32 {
        self.lease_capacity
    }

    /// No slot owner or external lease remains.
    pub const fn empty(self) -> bool {
        self.installed == 0 && self.leases == 0
    }
}

#[derive(Debug)]
struct BufferSlotState<F> {
    generation: Option<NonZeroU64>,
    address: u64,
    length: u64,
    owner: Option<Arc<F>>,
    leases: u32,
}

/// Exact registered-buffer ownership retained by one accepted request.
#[derive(Debug)]
#[must_use = "a registered-buffer lease must be released through its table"]
pub struct RegisteredBufferLease<F> {
    token: RegisteredBufferToken,
    range: RegisteredBufferRange,
    owner: Arc<F>,
}

impl<F> RegisteredBufferLease<F> {
    /// Generation-safe installed resource identity.
    pub const fn token(&self) -> RegisteredBufferToken {
        self.token
    }

    /// Exact subrange validated against the registered iovec.
    pub const fn range(&self) -> RegisteredBufferRange {
        self.range
    }

    /// Exact retained pin/resource owner.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }
}

/// Buffer owner removed from lookup and proven free of request leases.
#[derive(Debug)]
#[must_use = "retired buffer destruction should occur outside policy locks"]
pub struct RetiredBuffer<F> {
    token: RegisteredBufferToken,
    owner: Arc<F>,
}

impl<F> RetiredBuffer<F> {
    /// Exact retired slot generation.
    pub const fn token(&self) -> RegisteredBufferToken {
        self.token
    }

    /// Retained pin/resource owner.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }

    /// Transfers the final table owner for lock-external destruction.
    pub fn into_owner(self) -> Arc<F> {
        self.owner
    }
}

/// Failed slot installation with ownership returned for rollback.
#[derive(Debug)]
pub struct BufferInstallError<F> {
    error: IoUringError,
    owner: Arc<F>,
}

impl<F> BufferInstallError<F> {
    /// Typed policy failure.
    pub const fn error(&self) -> IoUringError {
        self.error
    }

    /// Owner which was not installed.
    pub fn owner(&self) -> &Arc<F> {
        &self.owner
    }

    /// Recovers ownership for destruction or rollback.
    pub fn into_owner(self) -> Arc<F> {
        self.owner
    }
}

/// Result of releasing one registered-buffer execution lease.
#[derive(Debug)]
pub enum BufferLeaseRelease<F> {
    /// The published slot still retains its owner.
    Active,
    /// Retirement was waiting for this last lease and returned the owner.
    Retired(RetiredBuffer<F>),
}

/// Failed lease release with the exact lease returned to its caller.
#[derive(Debug)]
pub struct BufferLeaseReleaseError<F> {
    error: IoUringError,
    lease: RegisteredBufferLease<F>,
}

impl<F> BufferLeaseReleaseError<F> {
    /// Typed policy failure.
    pub const fn error(&self) -> IoUringError {
        self.error
    }

    /// Recovers the lease for retry against its originating table.
    pub fn into_lease(self) -> RegisteredBufferLease<F> {
        self.lease
    }
}

/// Fixed-capacity registered-buffer ownership and retirement state.
#[derive(Debug)]
pub struct RegisteredBufferTable<F> {
    ring: RingId,
    id: BufferTableId,
    lifecycle: BufferTableLifecycle,
    slots: Vec<BufferSlotState<F>>,
    retire_cursor: usize,
    lease_capacity: u32,
    leases: u32,
}

impl<F> RegisteredBufferTable<F> {
    /// Allocates an unpublished fixed-capacity table.
    pub fn new(
        ring: RingId,
        id: BufferTableId,
        capacity: u32,
        lease_capacity: u32,
    ) -> Result<Self, IoUringError> {
        if capacity == 0 || capacity > IORING_MAX_REGISTERED_BUFFERS {
            return Err(IoUringError::InvalidBufferTableCapacity);
        }
        if lease_capacity == 0 || lease_capacity > IORING_MAX_CQ_ENTRIES {
            return Err(IoUringError::InvalidBufferLeaseCapacity);
        }
        let capacity = usize::try_from(capacity).map_err(|_| IoUringError::Overflow)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| IoUringError::AllocationFailed)?;
        for _ in 0..capacity {
            slots.push(BufferSlotState {
                generation: NonZeroU64::new(1),
                address: 0,
                length: 0,
                owner: None,
                leases: 0,
            });
        }
        Ok(Self {
            ring,
            id,
            lifecycle: BufferTableLifecycle::Building,
            slots,
            retire_cursor: 0,
            lease_capacity,
            leases: 0,
        })
    }

    /// Non-reused registration epoch carried by every table token.
    pub const fn id(&self) -> BufferTableId {
        self.id
    }

    /// Installs one exact registered range before publication.
    pub fn install(
        &mut self,
        slot: BufferSlot,
        address: u64,
        length: u64,
        owner: Arc<F>,
    ) -> Result<RegisteredBufferToken, BufferInstallError<F>> {
        let invalid_range = address.checked_add(length).is_none() || length == 0;
        if invalid_range {
            return Err(BufferInstallError {
                error: IoUringError::InvalidBufferRange,
                owner,
            });
        }
        if self.lifecycle != BufferTableLifecycle::Building {
            let error = match self.lifecycle {
                BufferTableLifecycle::Building => unreachable!(),
                BufferTableLifecycle::Published => IoUringError::Busy,
                BufferTableLifecycle::Retiring => IoUringError::Closing,
                BufferTableLifecycle::Closed => IoUringError::Closed,
            };
            return Err(BufferInstallError { error, owner });
        }
        let table_slot = match self.slot_mut(slot) {
            Ok(table_slot) => table_slot,
            Err(error) => return Err(BufferInstallError { error, owner }),
        };
        if table_slot.owner.is_some() {
            return Err(BufferInstallError {
                error: IoUringError::BufferSlotOccupied,
                owner,
            });
        }
        let generation = match table_slot.generation {
            Some(generation) => generation,
            None => {
                return Err(BufferInstallError {
                    error: IoUringError::GenerationExhausted,
                    owner,
                });
            }
        };
        table_slot.address = address;
        table_slot.length = length;
        table_slot.owner = Some(owner);
        Ok(RegisteredBufferToken::new(
            self.ring, self.id, slot, generation,
        ))
    }

    /// Makes every installed owner lookup-visible in one policy transition.
    pub fn publish(&mut self) -> Result<BufferTableProgress, IoUringError> {
        match self.lifecycle {
            BufferTableLifecycle::Building => self.lifecycle = BufferTableLifecycle::Published,
            BufferTableLifecycle::Published => {}
            BufferTableLifecycle::Retiring => return Err(IoUringError::Closing),
            BufferTableLifecycle::Closed => return Err(IoUringError::Closed),
        }
        self.progress()
    }

    /// Returns the current exact token for an installed published slot.
    pub fn token(&self, slot: BufferSlot) -> Result<RegisteredBufferToken, IoUringError> {
        if self.lifecycle != BufferTableLifecycle::Published {
            return Err(match self.lifecycle {
                BufferTableLifecycle::Building => IoUringError::BufferTableNotPublished,
                BufferTableLifecycle::Published => unreachable!(),
                BufferTableLifecycle::Retiring => IoUringError::Closing,
                BufferTableLifecycle::Closed => IoUringError::Closed,
            });
        }
        let table_slot = self.slot(slot)?;
        if table_slot.owner.is_none() {
            return Err(IoUringError::BufferSlotEmpty);
        }
        let generation = table_slot
            .generation
            .ok_or(IoUringError::GenerationExhausted)?;
        Ok(RegisteredBufferToken::new(
            self.ring, self.id, slot, generation,
        ))
    }

    /// Acquires a lease after validating the exact fixed-buffer subrange.
    pub fn acquire(
        &mut self,
        slot: BufferSlot,
        address: u64,
        length: u32,
    ) -> Result<RegisteredBufferLease<F>, IoUringError> {
        let token = self.token(slot)?;
        self.acquire_token(token, address, length)
    }

    /// Acquires the current slot in one exclusive table transaction. Unlike
    /// acquire_token, no externally supplied token needs revalidation after
    /// token() has just checked this same table under the exclusive borrow.
    pub fn acquire_current(
        &mut self,
        slot: BufferSlot,
        address: u64,
        length: u32,
    ) -> Result<RegisteredBufferLease<F>, IoUringError> {
        let token = self.token(slot)?;
        self.acquire_validated(token, address, length)
    }

    pub fn acquire_token(
        &mut self,
        token: RegisteredBufferToken,
        address: u64,
        length: u32,
    ) -> Result<RegisteredBufferLease<F>, IoUringError> {
        if self.lifecycle != BufferTableLifecycle::Published {
            return Err(match self.lifecycle {
                BufferTableLifecycle::Building => IoUringError::BufferTableNotPublished,
                BufferTableLifecycle::Published => unreachable!(),
                BufferTableLifecycle::Retiring => IoUringError::Closing,
                BufferTableLifecycle::Closed => IoUringError::Closed,
            });
        }
        self.validate_token(token)?;
        self.acquire_validated(token, address, length)
    }

    fn acquire_validated(
        &mut self,
        token: RegisteredBufferToken,
        address: u64,
        length: u32,
    ) -> Result<RegisteredBufferLease<F>, IoUringError> {
        let range = RegisteredBufferRange::new(address, length)?;
        if self.leases >= self.lease_capacity {
            return Err(IoUringError::BufferLeaseCapacityExceeded);
        }
        let table_slot = self.slot_mut(token.slot)?;
        let registered_end = table_slot
            .address
            .checked_add(table_slot.length)
            .ok_or(IoUringError::InvalidBufferRange)?;
        if address < table_slot.address || range.end() > registered_end {
            return Err(IoUringError::InvalidBufferRange);
        }
        let owner = Arc::clone(
            table_slot
                .owner
                .as_ref()
                .ok_or(IoUringError::BufferSlotEmpty)?,
        );
        table_slot.leases = table_slot
            .leases
            .checked_add(1)
            .ok_or(IoUringError::Overflow)?;
        self.leases = self.leases.checked_add(1).ok_or(IoUringError::Overflow)?;
        Ok(RegisteredBufferLease {
            token,
            range,
            owner,
        })
    }

    /// Stops new lookups and marks every installed slot for retirement.
    pub fn begin_retire(&mut self) -> Result<BufferTableProgress, IoUringError> {
        match self.lifecycle {
            BufferTableLifecycle::Building | BufferTableLifecycle::Published => {
                self.lifecycle = BufferTableLifecycle::Retiring;
                self.retire_cursor = 0;
            }
            BufferTableLifecycle::Retiring => {}
            BufferTableLifecycle::Closed => return self.progress(),
        }
        self.progress()
    }

    /// Removes one retirement-ready owner without destroying it in the table.
    pub fn retire(
        &mut self,
        token: RegisteredBufferToken,
    ) -> Result<RetiredBuffer<F>, IoUringError> {
        if self.lifecycle != BufferTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        self.validate_token(token)?;
        let table_slot = self.slot_mut(token.slot)?;
        if table_slot.leases != 0 {
            return Err(IoUringError::Busy);
        }
        let owner = table_slot
            .owner
            .take()
            .ok_or(IoUringError::BufferSlotEmpty)?;
        advance_generation(table_slot, token.generation);
        Ok(RetiredBuffer { token, owner })
    }

    /// Returns the next installed slot that can retire immediately.
    pub fn next_retirable(&mut self) -> Result<Option<RegisteredBufferToken>, IoUringError> {
        if self.lifecycle != BufferTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        while self.retire_cursor < self.slots.len() {
            let index = self.retire_cursor;
            self.retire_cursor += 1;
            let slot = &self.slots[index];
            if slot.owner.is_some() && slot.leases == 0 {
                let generation = slot.generation.ok_or(IoUringError::GenerationExhausted)?;
                return Ok(Some(RegisteredBufferToken::new(
                    self.ring,
                    self.id,
                    BufferSlot::new(u32::try_from(index).map_err(|_| IoUringError::Overflow)?),
                    generation,
                )));
            }
        }
        Ok(None)
    }

    /// Releases one execution lease and transfers a retiring owner when last.
    pub fn release(
        &mut self,
        lease: RegisteredBufferLease<F>,
    ) -> Result<BufferLeaseRelease<F>, BufferLeaseReleaseError<F>> {
        let token = lease.token;
        if let Err(error) = self.validate_token(token) {
            return Err(BufferLeaseReleaseError { error, lease });
        }
        if self.leases == 0 {
            return Err(BufferLeaseReleaseError {
                error: IoUringError::UnknownBufferLease,
                lease,
            });
        }
        let retiring = self.lifecycle == BufferTableLifecycle::Retiring;
        let table_slot = match self.slot_mut(token.slot) {
            Ok(table_slot) => table_slot,
            Err(error) => return Err(BufferLeaseReleaseError { error, lease }),
        };
        if table_slot.leases == 0 {
            return Err(BufferLeaseReleaseError {
                error: IoUringError::UnknownBufferLease,
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
                    return Err(BufferLeaseReleaseError {
                        error: IoUringError::BufferSlotEmpty,
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
            drop(lease);
            Ok(BufferLeaseRelease::Retired(RetiredBuffer { token, owner }))
        } else {
            drop(lease);
            Ok(BufferLeaseRelease::Active)
        }
    }

    /// Completes unregister/close after all owners and leases have retired.
    pub fn finish_retire(&mut self) -> Result<(), IoUringError> {
        if self.lifecycle == BufferTableLifecycle::Closed {
            return Ok(());
        }
        if self.lifecycle != BufferTableLifecycle::Retiring {
            return Err(IoUringError::InvalidLifecycleTransition);
        }
        if !self.progress()?.empty() {
            return Err(IoUringError::Busy);
        }
        self.lifecycle = BufferTableLifecycle::Closed;
        Ok(())
    }

    /// Returns finite resource-retirement progress.
    pub fn progress(&self) -> Result<BufferTableProgress, IoUringError> {
        let mut installed = 0_u32;
        for slot in &self.slots {
            if slot.owner.is_some() {
                installed = installed.checked_add(1).ok_or(IoUringError::Overflow)?;
            }
        }
        Ok(BufferTableProgress {
            lifecycle: self.lifecycle,
            capacity: u32::try_from(self.slots.len()).map_err(|_| IoUringError::Overflow)?,
            installed,
            leases: self.leases,
            lease_capacity: self.lease_capacity,
        })
    }

    fn slot(&self, slot: BufferSlot) -> Result<&BufferSlotState<F>, IoUringError> {
        self.slots
            .get(usize::try_from(slot.0).map_err(|_| IoUringError::InvalidBufferSlot)?)
            .ok_or(IoUringError::InvalidBufferSlot)
    }

    fn slot_mut(&mut self, slot: BufferSlot) -> Result<&mut BufferSlotState<F>, IoUringError> {
        self.slots
            .get_mut(usize::try_from(slot.0).map_err(|_| IoUringError::InvalidBufferSlot)?)
            .ok_or(IoUringError::InvalidBufferSlot)
    }

    fn validate_token(&self, token: RegisteredBufferToken) -> Result<(), IoUringError> {
        if token.ring != self.ring || token.table != self.id {
            return Err(IoUringError::UnknownBufferLease);
        }
        let slot = self.slot(token.slot)?;
        if slot.generation != Some(token.generation) || slot.owner.is_none() {
            return Err(IoUringError::UnknownBufferLease);
        }
        Ok(())
    }
}

fn advance_generation<F>(slot: &mut BufferSlotState<F>, generation: NonZeroU64) {
    slot.generation = generation.get().checked_add(1).and_then(NonZeroU64::new);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct Pin(u32);

    fn ring(raw: u64) -> RingId {
        RingId::new(raw).unwrap()
    }

    fn table(raw: u64) -> BufferTableId {
        BufferTableId::new(raw).unwrap()
    }

    #[test]
    fn range_is_checked_against_the_registered_iovec() {
        assert!(matches!(
            RegisteredBufferRange::new(u64::MAX, 1),
            Err(IoUringError::InvalidBufferRange)
        ));
        let mut table = RegisteredBufferTable::new(ring(1), table(1), 1, 2).unwrap();
        let owner = Arc::new(Pin(7));
        let token = table
            .install(BufferSlot::new(0), 0x1000, 0x1000, owner)
            .unwrap();
        table.publish().unwrap();
        assert!(matches!(
            table.acquire(BufferSlot::new(0), 0x0fff, 1),
            Err(IoUringError::InvalidBufferRange)
        ));
        let lease = table.acquire(BufferSlot::new(0), 0x1800, 0x400).unwrap();
        assert_eq!(lease.range().end(), 0x1c00);
        assert_eq!(lease.token(), token);
        assert!(matches!(
            table.release(lease),
            Ok(BufferLeaseRelease::Active)
        ));
    }

    #[test]
    fn current_buffer_acquisition_checks_range_without_leaking_retirement_credit() {
        let mut buffers = RegisteredBufferTable::new(ring(1), table(1), 1, 1).unwrap();
        let token = buffers
            .install(BufferSlot::new(0), 0x1000, 0x1000, Arc::new(Pin(7)))
            .unwrap();
        buffers.publish().unwrap();
        assert!(matches!(
            buffers.acquire_current(BufferSlot::new(0), 0x0fff, 1),
            Err(IoUringError::InvalidBufferRange)
        ));
        let lease = buffers
            .acquire_current(BufferSlot::new(0), 0x1800, 0x400)
            .unwrap();
        assert_eq!(lease.token(), token);
        assert_eq!(lease.range().end(), 0x1c00);
        assert!(matches!(
            buffers.acquire_current(BufferSlot::new(0), 0x1800, 1),
            Err(IoUringError::BufferLeaseCapacityExceeded)
        ));
        buffers.begin_retire().unwrap();
        assert!(matches!(
            buffers.acquire_current(BufferSlot::new(0), 0x1800, 1),
            Err(IoUringError::Closing)
        ));
        assert!(matches!(
            buffers.release(lease).unwrap(),
            BufferLeaseRelease::Retired(_)
        ));
        buffers.finish_retire().unwrap();
    }

    #[test]
    fn unregister_waits_for_the_last_generation_bound_lease() {
        let mut table = RegisteredBufferTable::new(ring(1), table(1), 1, 2).unwrap();
        let token = table
            .install(BufferSlot::new(0), 0x2000, 0x1000, Arc::new(Pin(1)))
            .unwrap();
        table.publish().unwrap();
        let lease = table.acquire(BufferSlot::new(0), 0x2000, 0x1000).unwrap();
        table.begin_retire().unwrap();
        assert!(matches!(
            table.acquire(BufferSlot::new(0), 0x2000, 1),
            Err(IoUringError::Closing)
        ));
        assert!(matches!(table.retire(token), Err(IoUringError::Busy)));
        let BufferLeaseRelease::Retired(retired) = table.release(lease).unwrap() else {
            panic!("last lease must return retired owner");
        };
        assert_eq!(retired.token(), token);
        table.finish_retire().unwrap();
    }

    #[test]
    fn table_epoch_and_generation_block_aba() {
        let mut first = RegisteredBufferTable::new(ring(1), table(1), 1, 1).unwrap();
        let token = first
            .install(BufferSlot::new(0), 0x3000, 0x1000, Arc::new(Pin(1)))
            .unwrap();
        first.publish().unwrap();
        first.begin_retire().unwrap();
        let retired = first.retire(token).unwrap();
        drop(retired);
        first.finish_retire().unwrap();

        let mut rebuilt = RegisteredBufferTable::new(ring(1), table(2), 1, 1).unwrap();
        rebuilt
            .install(BufferSlot::new(0), 0x3000, 0x1000, Arc::new(Pin(2)))
            .unwrap();
        rebuilt.publish().unwrap();
        assert!(matches!(
            rebuilt.acquire_token(token, 0x3000, 1),
            Err(IoUringError::UnknownBufferLease)
        ));
    }
}
