use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    AddressSpaceId, ExpectedMapping, InvalidationRange, MappingKind, MappingSnapshot, MmError,
    PageRange, PageSize, PinOwner, UserRange,
};

/// Requested memory access of a pin.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PinAccess {
    /// The consumer only reads memory.
    Read,
    /// The consumer may write memory; COW must be broken before publication.
    Write,
}

/// Lifetime class of a pin.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PinDuration {
    /// Operation completes synchronously before returning to the caller.
    Synchronous,
    /// Pin survives until an asynchronous operation completes or is cancelled.
    AsyncIo,
    /// Long-term pin subject to stricter reclaim and file constraints.
    LongTerm,
}

/// Purpose of a pin, without importing a driver or syscall type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PinUse {
    /// Fault-in or bounded userspace copy preparation.
    UserCopy,
    /// Block I/O scatter/gather request.
    BlockIo,
    /// Network I/O scatter/gather request.
    NetworkIo,
    /// Device DMA mapping.
    Dma,
    /// Other consumer-owned use with the same lifetime contract.
    Other,
}

fn try_atomic_update(
    counter: &AtomicU64,
    set_order: Ordering,
    load_order: Ordering,
    mut update: impl FnMut(u64) -> Option<u64>,
) -> Result<u64, u64> {
    let mut current = counter.load(load_order);
    loop {
        let Some(next) = update(current) else {
            return Err(current);
        };
        match counter.compare_exchange_weak(current, next, set_order, load_order) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

/// Fully typed pin intent. Fields remain private so intent cannot be partial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinRequest {
    range: UserRange,
    access: PinAccess,
    duration: PinDuration,
    use_kind: PinUse,
    owner: PinOwner,
}

impl PinRequest {
    /// Builds a pin request from checked values.
    pub const fn new(
        range: UserRange,
        access: PinAccess,
        duration: PinDuration,
        use_kind: PinUse,
        owner: PinOwner,
    ) -> Self {
        Self {
            range,
            access,
            duration,
            use_kind,
            owner,
        }
    }

    /// Raw-value adapter for syscall/kernel consumers.
    pub fn from_raw(
        start: usize,
        length: usize,
        access: PinAccess,
        duration: PinDuration,
        use_kind: PinUse,
        owner: u64,
    ) -> Result<Self, MmError> {
        Ok(Self::new(
            UserRange::new(start, length)?,
            access,
            duration,
            use_kind,
            PinOwner::new(owner)?,
        ))
    }

    /// Exact requested byte range before page covering.
    pub const fn range(self) -> UserRange {
        self.range
    }

    /// Requested access.
    pub const fn access(self) -> PinAccess {
        self.access
    }

    /// Requested lifetime.
    pub const fn duration(self) -> PinDuration {
        self.duration
    }

    /// Requested use.
    pub const fn use_kind(self) -> PinUse {
        self.use_kind
    }

    /// Accounting owner.
    pub const fn owner(self) -> PinOwner {
        self.owner
    }
}

/// Finite pages, bytes, and live-token quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinQuota {
    max_pages: u64,
    max_bytes: u64,
    max_tokens: u64,
}

impl PinQuota {
    /// Builds an explicit quota. Zero fields intentionally deny that resource.
    pub const fn new(max_pages: u64, max_bytes: u64, max_tokens: u64) -> Self {
        Self {
            max_pages,
            max_bytes,
            max_tokens,
        }
    }

    /// Maximum charged pages.
    pub const fn max_pages(self) -> u64 {
        self.max_pages
    }

    /// Maximum charged bytes.
    pub const fn max_bytes(self) -> u64 {
        self.max_bytes
    }

    /// Maximum simultaneous reservations and active pins.
    pub const fn max_tokens(self) -> u64 {
        self.max_tokens
    }

    const fn admits(self, accounting: PinAccounting) -> bool {
        accounting.pages <= self.max_pages
            && accounting.bytes <= self.max_bytes
            && accounting.tokens <= self.max_tokens
    }
}

/// Current pin resource charges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PinAccounting {
    pages: u64,
    bytes: u64,
    tokens: u64,
}

impl PinAccounting {
    /// Charged pages.
    pub const fn pages(self) -> u64 {
        self.pages
    }

    /// Charged bytes.
    pub const fn bytes(self) -> u64 {
        self.bytes
    }

    /// Charged reservations plus active pins.
    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    fn checked_add(self, charge: Self) -> Result<Self, MmError> {
        Ok(Self {
            pages: self
                .pages
                .checked_add(charge.pages)
                .ok_or(MmError::Overflow)?,
            bytes: self
                .bytes
                .checked_add(charge.bytes)
                .ok_or(MmError::Overflow)?,
            tokens: self
                .tokens
                .checked_add(charge.tokens)
                .ok_or(MmError::Overflow)?,
        })
    }

    fn checked_sub(self, charge: Self) -> Result<Self, MmError> {
        Ok(Self {
            pages: self
                .pages
                .checked_sub(charge.pages)
                .ok_or(MmError::Overflow)?,
            bytes: self
                .bytes
                .checked_sub(charge.bytes)
                .ok_or(MmError::Overflow)?,
            tokens: self
                .tokens
                .checked_sub(charge.tokens)
                .ok_or(MmError::Overflow)?,
        })
    }

    fn for_range(range: PageRange) -> Result<Self, MmError> {
        Ok(Self {
            pages: u64::try_from(range.page_count()).map_err(|_| MmError::Overflow)?,
            bytes: u64::try_from(range.len()).map_err(|_| MmError::Overflow)?,
            tokens: 1,
        })
    }
}

/// Opaque token for one live system-wide pin-budget charge.
///
/// The budget identity prevents a token from one system domain from refunding
/// an unrelated domain that happens to use the same local token sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[must_use = "a system pin-budget charge must be released by its originating budget"]
pub struct PinBudgetCharge {
    budget: NonZeroU64,
    token: NonZeroU64,
}

impl PinBudgetCharge {
    /// Consumer-visible opaque token value for diagnostics.
    pub const fn get(self) -> u64 {
        self.token.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PinBudgetRecord {
    charge: PinBudgetCharge,
    range: PageRange,
    accounting: PinAccounting,
}

/// Fixed-capacity system-wide pin accounting shared by address spaces.
///
/// This type performs no allocation and owns no lock. A kernel places one
/// instance behind its system accounting lock, reserves a charge before any
/// lower pin mechanism work, and holds the opaque charge until that work has
/// been cancelled or completed. Per-address-space [`PinRegistry`] accounting
/// remains responsible for mapping mutation and owner policy; this budget
/// supplies the aggregate bound those independent registries cannot enforce.
pub struct PinBudget<const CHARGE_CAPACITY: usize> {
    identity: NonZeroU64,
    page_size: PageSize,
    quota: PinQuota,
    usage: PinAccounting,
    records: [Option<PinBudgetRecord>; CHARGE_CAPACITY],
    next_charge: Option<NonZeroU64>,
}

static NEXT_PIN_BUDGET_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn allocate_pin_budget_identity() -> Result<NonZeroU64, MmError> {
    let identity = try_atomic_update(
        &NEXT_PIN_BUDGET_IDENTITY,
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| match current {
            0 => None,
            u64::MAX => Some(0),
            _ => Some(current + 1),
        },
    )
    .map_err(|_| MmError::IdExhausted)?;
    NonZeroU64::new(identity).ok_or(MmError::IdExhausted)
}

impl<const CHARGE_CAPACITY: usize> PinBudget<CHARGE_CAPACITY> {
    /// Builds an empty uniquely identified budget with a consumer-selected
    /// nonzero first charge token. Budget identities and charge tokens never
    /// wrap or get reused.
    pub fn new(page_size: usize, quota: PinQuota, first_charge: u64) -> Result<Self, MmError> {
        let page_size = PageSize::new(page_size)?;
        let next_charge = Some(NonZeroU64::new(first_charge).ok_or(MmError::InvalidIdentity)?);
        let identity = allocate_pin_budget_identity()?;
        Ok(Self {
            identity,
            page_size,
            quota,
            usage: PinAccounting {
                pages: 0,
                bytes: 0,
                tokens: 0,
            },
            records: [None; CHARGE_CAPACITY],
            next_charge,
        })
    }

    /// Page size used to cover byte requests before accounting.
    pub const fn page_size(&self) -> PageSize {
        self.page_size
    }

    /// Aggregate finite quota.
    pub const fn quota(&self) -> PinQuota {
        self.quota
    }

    /// Aggregate live accounting, including pre-publication work.
    pub const fn accounting(&self) -> PinAccounting {
        self.usage
    }

    /// Number of live system charges.
    pub fn live_charges(&self) -> usize {
        self.records.iter().flatten().count()
    }

    /// Reserves aggregate quota before any lower pin mechanism is acquired.
    /// Every failure is mutation-free.
    pub fn reserve(&mut self, request: PinRequest) -> Result<PinBudgetCharge, MmError> {
        let range = PageRange::covering(request.range(), self.page_size)?;
        let record_index = self
            .records
            .iter()
            .position(Option::is_none)
            .ok_or(MmError::CapacityExceeded)?;
        let accounting = PinAccounting::for_range(range)?;
        let new_usage = self.usage.checked_add(accounting)?;
        if !self.quota.admits(new_usage) {
            return Err(MmError::QuotaExceeded);
        }
        let token = self.next_charge.ok_or(MmError::IdExhausted)?;
        let charge = PinBudgetCharge {
            budget: self.identity,
            token,
        };

        self.next_charge = token.get().checked_add(1).and_then(NonZeroU64::new);
        self.usage = new_usage;
        self.records[record_index] = Some(PinBudgetRecord {
            charge,
            range,
            accounting,
        });
        Ok(charge)
    }

    /// Exact page-covering range owned by a live charge.
    pub fn range(&self, charge: PinBudgetCharge) -> Result<PageRange, MmError> {
        self.record(charge).map(|record| record.range)
    }

    /// Exact aggregate accounting owned by a live charge.
    pub fn charge_accounting(&self, charge: PinBudgetCharge) -> Result<PinAccounting, MmError> {
        self.record(charge).map(|record| record.accounting)
    }

    /// Releases one charge after all lower pin ownership has ended.
    pub fn release(&mut self, charge: PinBudgetCharge) -> Result<(), MmError> {
        let index = self.record_index(charge)?;
        let record = self.records[index].expect("located budget charge");
        let new_usage = self.usage.checked_sub(record.accounting)?;
        self.records[index] = None;
        self.usage = new_usage;
        Ok(())
    }

    fn record(&self, charge: PinBudgetCharge) -> Result<PinBudgetRecord, MmError> {
        let index = self.record_index(charge)?;
        Ok(self.records[index].expect("located budget charge"))
    }

    fn record_index(&self, charge: PinBudgetCharge) -> Result<usize, MmError> {
        if charge.budget != self.identity {
            return Err(MmError::BudgetMismatch);
        }
        self.records
            .iter()
            .position(|record| record.is_some_and(|record| record.charge == charge))
            .ok_or(MmError::UnknownToken)
    }
}

/// ABA-safe, nonzero pin token. Registry sequences never wrap or reuse it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PinToken(NonZeroU64);

impl PinToken {
    /// Integer representation useful for opaque consumer maps and diagnostics.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Opaque handle for quota reserved before fault/pin mechanism work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "a pin reservation must be committed, cancelled, or torn down"]
pub struct PinReservation {
    token: PinToken,
}

impl PinReservation {
    /// Token identifying the reserved registry record.
    pub const fn token(self) -> PinToken {
        self.token
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordState {
    Reserved,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PinRecord {
    token: PinToken,
    request: PinRequest,
    range: PageRange,
    snapshot: PinSnapshot,
    validated_until: usize,
    validated_mappings: usize,
    charge: PinAccounting,
    state: RecordState,
}

/// Read-only view of one live reservation or active pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinLeaseView(PinRecord);

impl PinLeaseView {
    /// Stable token.
    pub const fn token(self) -> PinToken {
        self.0.token
    }

    /// Original typed request.
    pub const fn request(self) -> PinRequest {
        self.0.request
    }

    /// Exact page-covering range charged and protected from mutation.
    pub const fn range(self) -> PageRange {
        self.0.range
    }

    /// Address-space/range snapshot frozen at admission.
    pub const fn snapshot(self) -> PinSnapshot {
        self.0.snapshot
    }

    /// Number of contiguous mapping segments revalidated before publication.
    pub const fn validated_mappings(self) -> usize {
        self.0.validated_mappings
    }

    /// Exact resource charge.
    pub const fn accounting(self) -> PinAccounting {
        self.0.charge
    }

    /// Whether mechanism pins have been revalidated and published.
    pub const fn is_active(self) -> bool {
        matches!(self.0.state, RecordState::Active)
    }
}

/// Address-space and full page-covering range frozen for one pin operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinSnapshot {
    address_space: AddressSpaceId,
    range: PageRange,
}

impl PinSnapshot {
    /// Address space whose mappings must cover the pin.
    pub const fn address_space(self) -> AddressSpaceId {
        self.address_space
    }

    /// Full page-covering range protected by the registry.
    pub const fn range(self) -> PageRange {
        self.range
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OwnerAccount {
    owner: PinOwner,
    quota: PinQuota,
    usage: PinAccounting,
}

/// Pin registry admission lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinRegistryState {
    /// New reservations are admitted.
    Open,
    /// New work is rejected while existing work drains normally.
    Closing,
    /// Reserved work is cancelled and active pins must release.
    TearingDown,
    /// No live work remains and the registry cannot reopen.
    Closed,
}

/// Current bounded work counts returned by lifecycle operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleProgress {
    active: usize,
    reserved: usize,
}

impl LifecycleProgress {
    /// Published pins still owned by consumers or devices.
    pub const fn active(self) -> usize {
        self.active
    }

    /// Pre-publication reservations still in flight.
    pub const fn reserved(self) -> usize {
        self.reserved
    }

    /// Total live registry records.
    pub const fn total(self) -> usize {
        self.active + self.reserved
    }
}

/// Result of beginning forced teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeardownReport {
    cancelled_reservations: usize,
    active_remaining: usize,
}

impl TeardownReport {
    /// Reservations rolled back before publication.
    pub const fn cancelled_reservations(self) -> usize {
        self.cancelled_reservations
    }

    /// Active pins that still require verified release/cancellation.
    pub const fn active_remaining(self) -> usize {
        self.active_remaining
    }
}

/// First live pin preventing a mapping mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MutationBlocker(PinLeaseView);

impl MutationBlocker {
    /// Blocking token.
    pub const fn token(self) -> PinToken {
        self.0.token()
    }

    /// Blocking range.
    pub const fn range(self) -> PageRange {
        self.0.range()
    }

    /// Blocking request.
    pub const fn request(self) -> PinRequest {
        self.0.request()
    }

    /// Frozen mapping snapshot.
    pub const fn snapshot(self) -> PinSnapshot {
        self.0.snapshot()
    }
}

/// Caller-owned, fixed-capacity pin accounting and lifecycle core.
///
/// `OWNER_CAPACITY` bounds distinct configured owners and `TOKEN_CAPACITY`
/// bounds reserved plus active pins. The registry performs no allocation and
/// invokes no callbacks. A consumer must explicitly cancel every abandoned
/// reservation; forced teardown cancels all such reservations automatically.
pub struct PinRegistry<const OWNER_CAPACITY: usize, const TOKEN_CAPACITY: usize> {
    page_size: PageSize,
    global_quota: PinQuota,
    global_usage: PinAccounting,
    owners: [Option<OwnerAccount>; OWNER_CAPACITY],
    records: [Option<PinRecord>; TOKEN_CAPACITY],
    next_token: Option<NonZeroU64>,
    state: PinRegistryState,
}

impl<const OWNER_CAPACITY: usize, const TOKEN_CAPACITY: usize>
    PinRegistry<OWNER_CAPACITY, TOKEN_CAPACITY>
{
    /// Builds an empty registry with a consumer-selected nonzero token seed.
    pub fn new(
        page_size: usize,
        global_quota: PinQuota,
        first_token: u64,
    ) -> Result<Self, MmError> {
        Ok(Self {
            page_size: PageSize::new(page_size)?,
            global_quota,
            global_usage: PinAccounting::default(),
            owners: [None; OWNER_CAPACITY],
            records: [None; TOKEN_CAPACITY],
            next_token: Some(NonZeroU64::new(first_token).ok_or(MmError::InvalidIdentity)?),
            state: PinRegistryState::Open,
        })
    }

    /// Current lifecycle state.
    pub const fn state(&self) -> PinRegistryState {
        self.state
    }

    /// Registry page size.
    pub const fn page_size(&self) -> PageSize {
        self.page_size
    }

    /// Global finite quota.
    pub const fn global_quota(&self) -> PinQuota {
        self.global_quota
    }

    /// Current global accounting, including reservations.
    pub const fn global_accounting(&self) -> PinAccounting {
        self.global_usage
    }

    /// Current accounting for one configured owner.
    pub fn owner_accounting(&self, owner: PinOwner) -> Result<PinAccounting, MmError> {
        self.owner_index(owner)
            .map(|index| self.owners[index].expect("located owner").usage)
            .ok_or(MmError::OwnerNotConfigured)
    }

    /// Adds or updates one owner quota without dropping existing charges.
    pub fn configure_owner(&mut self, owner: PinOwner, quota: PinQuota) -> Result<(), MmError> {
        self.ensure_open()?;
        if let Some(index) = self.owner_index(owner) {
            let account = self.owners[index].as_mut().expect("located owner");
            if !quota.admits(account.usage) {
                return Err(MmError::OwnerBusy);
            }
            account.quota = quota;
            return Ok(());
        }
        let index = self
            .owners
            .iter()
            .position(Option::is_none)
            .ok_or(MmError::CapacityExceeded)?;
        self.owners[index] = Some(OwnerAccount {
            owner,
            quota,
            usage: PinAccounting::default(),
        });
        Ok(())
    }

    /// Removes an unused owner quota.
    pub fn remove_owner(&mut self, owner: PinOwner) -> Result<(), MmError> {
        self.ensure_open()?;
        let index = self.owner_index(owner).ok_or(MmError::OwnerNotConfigured)?;
        let account = self.owners[index].expect("located owner");
        if account.usage != PinAccounting::default() {
            return Err(MmError::OwnerBusy);
        }
        self.owners[index] = None;
        Ok(())
    }

    /// Reserves quota and one token before blocking fault/pin work begins.
    ///
    /// Every failure is mutation-free. After success, callers must invoke
    /// `commit`, `cancel_reservation`, or `begin_teardown`.
    ///
    /// A live reservation is also an address-range mutation fence: it is
    /// returned by [`Self::first_mutation_blocker`] and rejected by
    /// [`Self::admit_mutation`] just like an active pin. Consumers that route
    /// every overlapping mapping mutation through this registry may therefore
    /// revalidate a large request in bounded publication-lock windows. The
    /// reservation must remain live between windows, and each lower-level pin
    /// owner must be acquired before its corresponding window is revalidated.
    pub fn reserve(
        &mut self,
        request: PinRequest,
        address_space: AddressSpaceId,
    ) -> Result<PinReservation, MmError> {
        self.ensure_open()?;
        let range = PageRange::covering(request.range, self.page_size)?;
        self.validate_overlap(request, range, address_space)?;

        let owner_index = self
            .owner_index(request.owner)
            .ok_or(MmError::OwnerNotConfigured)?;
        let record_index = self
            .records
            .iter()
            .position(Option::is_none)
            .ok_or(MmError::CapacityExceeded)?;
        let charge = PinAccounting::for_range(range)?;
        let owner = self.owners[owner_index].expect("located owner");
        let new_owner_usage = owner.usage.checked_add(charge)?;
        let new_global_usage = self.global_usage.checked_add(charge)?;
        if !owner.quota.admits(new_owner_usage) || !self.global_quota.admits(new_global_usage) {
            return Err(MmError::QuotaExceeded);
        }
        let token = self.allocate_token()?;

        self.owners[owner_index]
            .as_mut()
            .expect("located owner")
            .usage = new_owner_usage;
        self.global_usage = new_global_usage;
        self.records[record_index] = Some(PinRecord {
            token,
            request,
            range,
            snapshot: PinSnapshot {
                address_space,
                range,
            },
            validated_until: range.start(),
            validated_mappings: 0,
            charge,
            state: RecordState::Reserved,
        });
        Ok(PinReservation { token })
    }

    /// Revalidates the next contiguous mapping segment covering a reservation.
    ///
    /// Consumers call this once for each covered VMA segment after faulting and
    /// pinning its mechanism pages. Segments must arrive in ascending order
    /// with no gaps or overlap. Revalidation may use either one continuous
    /// topology-publication critical section, or multiple bounded sections
    /// while the live reservation is the mutation fence. The latter is sound
    /// only when every overlapping mapping mutation is admitted through this
    /// registry; unrelated ranges may continue to mutate between windows.
    ///
    /// The consumer must drop any lower partial pins if this method fails. Any
    /// stale/access failure automatically rolls back the policy reservation
    /// and its accounting, which also removes the mutation fence.
    pub fn revalidate_next(
        &mut self,
        reservation: PinReservation,
        expected: ExpectedMapping,
        current: MappingSnapshot,
        covered: PageRange,
    ) -> Result<(), MmError> {
        let index = self
            .record_index(reservation.token)
            .ok_or(MmError::UnknownToken)?;
        let record = self.records[index].expect("located record");
        if record.state != RecordState::Reserved {
            return Err(MmError::InvalidTokenState);
        }
        let validation = (|| {
            if covered.page_size() != self.page_size
                || covered.start() != record.validated_until
                || !record.range.contains(covered)
            {
                return Err(MmError::NonContiguousCoverage);
            }
            if expected.address_space() != record.snapshot.address_space {
                return Err(MmError::StaleGeneration);
            }
            expected.revalidate_range(current, covered)?;
            self.validate_mapping(record.request, covered, current)
        })();
        if let Err(error) = validation {
            self.remove_record(index)?;
            return Err(error);
        }
        let record = self.records[index].as_mut().expect("located record");
        record.validated_until = covered.end();
        record.validated_mappings = record
            .validated_mappings
            .checked_add(1)
            .ok_or(MmError::Overflow)?;
        Ok(())
    }

    /// Publishes a pin only after mapping revalidation covers its full range.
    ///
    /// This is a constant-time state transition; all per-mapping work belongs
    /// in bounded [`Self::revalidate_next`] calls.
    pub fn commit(&mut self, reservation: PinReservation) -> Result<PinToken, MmError> {
        let index = self
            .record_index(reservation.token)
            .ok_or(MmError::UnknownToken)?;
        let record = self.records[index].expect("located record");
        if record.state != RecordState::Reserved {
            return Err(MmError::InvalidTokenState);
        }
        if record.validated_until != record.range.end() || record.validated_mappings == 0 {
            return Err(MmError::IncompleteRevalidation);
        }
        self.records[index].as_mut().expect("located record").state = RecordState::Active;
        Ok(record.token)
    }

    /// Explicitly rolls back an unpublished reservation.
    pub fn cancel_reservation(&mut self, reservation: PinReservation) -> Result<(), MmError> {
        let index = self
            .record_index(reservation.token)
            .ok_or(MmError::UnknownToken)?;
        if self.records[index].expect("located record").state != RecordState::Reserved {
            return Err(MmError::InvalidTokenState);
        }
        self.remove_record(index)
    }

    /// Releases one active pin and refunds owner/global accounting.
    pub fn release(&mut self, token: PinToken) -> Result<(), MmError> {
        let index = self.record_index(token).ok_or(MmError::UnknownToken)?;
        if self.records[index].expect("located record").state != RecordState::Active {
            return Err(MmError::InvalidTokenState);
        }
        self.remove_record(index)
    }

    /// Looks up immutable request/range/snapshot facts for a live token.
    pub fn view(&self, token: PinToken) -> Result<PinLeaseView, MmError> {
        self.record_index(token)
            .map(|index| PinLeaseView(self.records[index].expect("located record")))
            .ok_or(MmError::UnknownToken)
    }

    /// Number of active published pins.
    pub fn active_count(&self) -> usize {
        self.records
            .iter()
            .flatten()
            .filter(|record| record.state == RecordState::Active)
            .count()
    }

    /// Number of pre-publication reservations.
    pub fn reserved_count(&self) -> usize {
        self.records
            .iter()
            .flatten()
            .filter(|record| record.state == RecordState::Reserved)
            .count()
    }

    /// Current active/reserved work counts.
    pub fn progress(&self) -> LifecycleProgress {
        LifecycleProgress {
            active: self.active_count(),
            reserved: self.reserved_count(),
        }
    }

    /// Whether cloning the address-space topology would cross an unpublished
    /// reservation or a short-lived active mapping fence.
    ///
    /// Active long-term pins retain exact lower owners independently of the
    /// source VMA topology and therefore do not block a fork clone.
    pub fn has_clone_blocker(&self) -> bool {
        self.records.iter().flatten().any(|record| {
            record.state == RecordState::Reserved
                || record.request.duration != PinDuration::LongTerm
        })
    }

    /// Finds the first reservation or short-lived active pin overlapping an
    /// invalidation.
    ///
    /// Every reservation remains a mutation fence until publication. An
    /// active long-term pin has already transferred stability to exact lower
    /// owners, so mapping topology and permissions may subsequently change
    /// without invalidating that owner.
    pub fn first_mutation_blocker(
        &self,
        invalidation: InvalidationRange,
    ) -> Option<MutationBlocker> {
        self.records.iter().flatten().find_map(|record| {
            let expected = invalidation.expected();
            let fences_mutation = record.state == RecordState::Reserved
                || record.request.duration != PinDuration::LongTerm;
            (fences_mutation
                && record.snapshot.address_space == expected.address_space()
                && record.range.overlaps(invalidation.range()))
            .then_some(MutationBlocker(PinLeaseView(*record)))
        })
    }

    /// Admits a mapping mutation when no reservation or short-lived active pin
    /// overlaps it.
    pub fn admit_mutation(&self, invalidation: InvalidationRange) -> Result<(), MmError> {
        if self.first_mutation_blocker(invalidation).is_some() {
            return Err(MmError::MappingPinned);
        }
        Ok(())
    }

    /// Stops admitting new reservations and lets current work drain.
    pub fn begin_close(&mut self) -> Result<LifecycleProgress, MmError> {
        match self.state {
            PinRegistryState::Open => self.state = PinRegistryState::Closing,
            PinRegistryState::Closing => {}
            PinRegistryState::TearingDown => return Err(MmError::TearingDown),
            PinRegistryState::Closed => return Err(MmError::Closed),
        }
        Ok(self.progress())
    }

    /// Begins forced teardown and rolls back every unpublished reservation.
    pub fn begin_teardown(&mut self) -> Result<TeardownReport, MmError> {
        if self.state == PinRegistryState::Closed {
            return Err(MmError::Closed);
        }
        self.state = PinRegistryState::TearingDown;
        let mut cancelled = 0usize;
        for index in 0..TOKEN_CAPACITY {
            if self.records[index].is_some_and(|record| record.state == RecordState::Reserved) {
                self.remove_record(index)?;
                cancelled = cancelled.checked_add(1).ok_or(MmError::Overflow)?;
            }
        }
        Ok(TeardownReport {
            cancelled_reservations: cancelled,
            active_remaining: self.active_count(),
        })
    }

    /// Finalizes close/teardown only after every token has been released.
    pub fn finish_teardown(&mut self) -> Result<(), MmError> {
        match self.state {
            PinRegistryState::Open => return Err(MmError::Busy),
            PinRegistryState::Closing | PinRegistryState::TearingDown => {}
            PinRegistryState::Closed => return Ok(()),
        }
        if self.progress().total() != 0 {
            return Err(MmError::Busy);
        }
        self.state = PinRegistryState::Closed;
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), MmError> {
        match self.state {
            PinRegistryState::Open => Ok(()),
            PinRegistryState::Closing => Err(MmError::Closing),
            PinRegistryState::TearingDown => Err(MmError::TearingDown),
            PinRegistryState::Closed => Err(MmError::Closed),
        }
    }

    fn owner_index(&self, owner: PinOwner) -> Option<usize> {
        self.owners
            .iter()
            .position(|account| account.is_some_and(|account| account.owner == owner))
    }

    fn record_index(&self, token: PinToken) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.is_some_and(|record| record.token == token))
    }

    fn allocate_token(&mut self) -> Result<PinToken, MmError> {
        let raw = self.next_token.ok_or(MmError::IdExhausted)?;
        let token = PinToken(raw);
        self.next_token = raw.get().checked_add(1).and_then(NonZeroU64::new);
        Ok(token)
    }

    fn validate_mapping(
        &self,
        request: PinRequest,
        range: PageRange,
        snapshot: MappingSnapshot,
    ) -> Result<(), MmError> {
        if snapshot.range().page_size() != self.page_size || !snapshot.range().contains(range) {
            return Err(MmError::RangeNotMapped);
        }
        match request.access {
            PinAccess::Read if !snapshot.access().readable() => return Err(MmError::AccessDenied),
            PinAccess::Write if !snapshot.access().writable() => {
                return Err(MmError::AccessDenied);
            }
            PinAccess::Read | PinAccess::Write => {}
        }
        if request.duration == PinDuration::LongTerm && !snapshot.long_term_pinnable() {
            return Err(MmError::UnsupportedPin);
        }
        if request.use_kind == PinUse::Dma && request.duration != PinDuration::LongTerm {
            return Err(MmError::UnsupportedPin);
        }
        if request.duration == PinDuration::LongTerm
            && request.access == PinAccess::Write
            && snapshot.kind() == MappingKind::FileShared
            && !snapshot.writable_file_pin_supported()
        {
            return Err(MmError::UnsupportedPin);
        }
        if matches!(snapshot.kind(), MappingKind::Device | MappingKind::Special)
            && request.duration == PinDuration::LongTerm
        {
            return Err(MmError::UnsupportedPin);
        }
        Ok(())
    }

    fn validate_overlap(
        &self,
        request: PinRequest,
        range: PageRange,
        address_space: AddressSpaceId,
    ) -> Result<(), MmError> {
        for record in self.records.iter().flatten() {
            if record.snapshot.address_space == address_space
                && record.range.overlaps(range)
                && (record.request.access == PinAccess::Write || request.access == PinAccess::Write)
                && (record.state == RecordState::Reserved
                    || record.request.duration != PinDuration::LongTerm)
            {
                return Err(MmError::PinOverlap);
            }
        }
        Ok(())
    }

    fn remove_record(&mut self, index: usize) -> Result<(), MmError> {
        let record = self.records[index].take().ok_or(MmError::UnknownToken)?;
        let owner_index = self
            .owner_index(record.request.owner)
            .ok_or(MmError::OwnerNotConfigured)?;
        let owner = self.owners[owner_index].as_mut().expect("located owner");
        owner.usage = owner.usage.checked_sub(record.charge)?;
        self.global_usage = self.global_usage.checked_sub(record.charge)?;
        Ok(())
    }
}
