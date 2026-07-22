use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt,
    ops::DerefMut,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, ax_bail};
use axhal::{
    mem::phys_to_virt,
    paging::{MappingFlags, PageSize, PageTable, PagingError},
    trap::PageFaultFlags,
};
use axsync::Mutex;
use hashbrown::{HashMap, hash_map::Entry};
use kspin::SpinNoIrq;
use memory_addr::{
    MemoryAddr, PAGE_SIZE_4K, PageIter4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k,
};
use memory_set::{MappingLineage, MappingResult, MemoryArea, MemorySet};
use thekernel_linux_mm::{
    AddressSpaceId, ExpectedMapping, FaultDisposition, FaultHandlerId, InvalidationRange,
    InvalidationReason, MappingAccess, MappingGeneration, MappingId, MappingKind, MappingSnapshot,
    MmError, PageRange, PinBudget, PinBudgetCharge, PinOwner, PinQuota, PinRegistry, PinRequest,
    PinReservation, PinToken, UffdRegistration,
};

use super::{
    DeferredUffdWake, LockExternalUffdOutcome, OptionalUffdPlan, PreparedRemapUffd,
    PreparedUffdMutation, RemapUffdOutcome, UffdFaultLeafState, UffdIcacheSynchronization,
    UffdPagePublication, UffdRemapKind, UffdResolverLease,
    asid::{AddressSpaceToken, HardwareAddressSpaceId, reserve_hardware_address_space_id},
    checked_align_up_4k,
};

mod backend;
mod mapping;

pub use self::backend::*;
pub(crate) use self::mapping::{FileLikeMappingLease, FileMappingLease, FileMappingSharing};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultResult {
    Handled,
    Failed(PageFaultFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultFailure {
    AddressNotMapped,
    AccessDenied,
    BackingUnavailable,
    InternalInconsistency,
    OutOfMemory,
}

fn classify_page_population(result: AxResult<usize>) -> PageFaultResult {
    match result {
        Ok(0) => PageFaultResult::Failed(PageFaultFailure::InternalInconsistency),
        Ok(_) => PageFaultResult::Handled,
        Err(err) if err.canonicalize() == AxError::NoMemory => {
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        }
        Err(err)
            if matches!(
                err.canonicalize(),
                AxError::BadAddress | AxError::BadState | AxError::InvalidInput
            ) =>
        {
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        }
        Err(_) => PageFaultResult::Failed(PageFaultFailure::BackingUnavailable),
    }
}

#[inline]
fn synchronize_executable_publication(flags: MappingFlags) {
    if flags.contains(MappingFlags::EXECUTE) {
        drop(super::synchronize_icache());
    }
}

fn adds_execute_permission(old_flags: MappingFlags, new_flags: MappingFlags) -> bool {
    new_flags.contains(MappingFlags::EXECUTE) && !old_flags.contains(MappingFlags::EXECUTE)
}

fn present_leaf_satisfies_fault(page_flags: MappingFlags, access_flags: PageFaultFlags) -> bool {
    page_flags.contains(access_flags)
}

/// Clamps one userfaultfd ioctl range to the address space that may contain
/// live VMAs. Current Linux permits the raw range to begin below
/// `mmap_min_addr`; only actual VMA intersections reach registration policy.
fn uffd_vma_scan_range(
    range: PageRange,
    address_space_start: VirtAddr,
    address_space_end: VirtAddr,
) -> AxResult<VirtAddrRange> {
    let start = range.start().max(address_space_start.as_usize());
    let end = range.end().min(address_space_end.as_usize());
    if start >= end {
        return Err(AxError::InvalidInput);
    }
    Ok(VirtAddrRange::new(
        VirtAddr::from(start),
        VirtAddr::from(end),
    ))
}

/// The first MISSING-only profile does not advertise
/// `UFFD_FEATURE_MISSING_HUGETLBFS`.
///
/// Mapping snapshots deliberately use 4 KiB Linux policy geometry, so
/// accepting a larger backend leaf here would let a resolver or ordinary
/// fault populate outside one registered 4 KiB page. Reject it before any
/// registration-table mutation.
fn validate_uffd_missing_backend_granule(page_size: PageSize) -> AxResult {
    if page_size == PageSize::Size4K {
        Ok(())
    } else {
        Err(AxError::InvalidInput)
    }
}

const USER_IO_PIN_MAX_TOKENS: u64 = 64;
const USER_IO_PIN_MAX_BYTES: u64 = 64 * 1024 * 1024;
const USER_IO_PIN_MAX_PAGES: u64 = USER_IO_PIN_MAX_BYTES / PAGE_SIZE_4K as u64;
/// Internal live logical-mapping limit. Fragments sharing one lineage count
/// once, so protection splits do not consume additional slots.
const MAX_MAPPING_LINEAGES: usize = 65_536;
/// Independent live VMA-fragment limit. One logical lineage may be split by
/// protection, unmap, fork policy, or remap geometry, so bounding only the
/// lineage sidecar does not bound the area tree itself.
const MAX_VMA_FRAGMENTS: usize = 65_536;
type UserIoPinRegistry = PinRegistry<1, { USER_IO_PIN_MAX_TOKENS as usize }>;
type UserIoPinBudget = PinBudget<{ USER_IO_PIN_MAX_TOKENS as usize }>;
type UserIoPolicy = (
    AddressSpaceId,
    MappingId,
    MappingGeneration,
    UserIoPinRegistry,
);

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);
// MappingLineage reserves raw value 1 for compatibility-only untracked areas.
static NEXT_MAPPING_ID: AtomicU64 = AtomicU64::new(2);
static USER_IO_PIN_BUDGET: SpinNoIrq<Option<UserIoPinBudget>> = SpinNoIrq::new(None);

fn allocate_nonwrapping_id(sequence: &AtomicU64) -> AxResult<u64> {
    sequence
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::ResourceBusy)
}

fn mm_error(error: MmError) -> AxError {
    match error {
        MmError::ZeroLength
        | MmError::Overflow
        | MmError::InvalidPageSize
        | MmError::Unaligned
        | MmError::InvalidIdentity
        | MmError::InvalidRemap => AxError::InvalidInput,
        MmError::RangeNotMapped | MmError::StaleGeneration => AxError::BadAddress,
        MmError::AccessDenied => AxError::PermissionDenied,
        MmError::QuotaExceeded
        | MmError::CapacityExceeded
        | MmError::OwnerBusy
        | MmError::PinOverlap
        | MmError::MappingPinned
        | MmError::IdExhausted
        | MmError::Closing
        | MmError::TearingDown
        | MmError::Closed
        | MmError::Busy => AxError::ResourceBusy,
        MmError::UnsupportedPin
        | MmError::OwnerNotConfigured
        | MmError::UnknownToken
        | MmError::InvalidTokenState
        | MmError::UnknownFault
        | MmError::MemlockDenied => AxError::InvalidInput,
        _ => AxError::InvalidInput,
    }
}

fn allocate_mapping_id() -> AxResult<MappingId> {
    MappingId::new(allocate_nonwrapping_id(&NEXT_MAPPING_ID)?).map_err(mm_error)
}

fn new_user_io_policy() -> AxResult<UserIoPolicy> {
    let address_space_id =
        AddressSpaceId::new(allocate_nonwrapping_id(&NEXT_ADDRESS_SPACE_ID)?).map_err(mm_error)?;
    let topology_mapping_id = allocate_mapping_id()?;
    let topology_generation = MappingGeneration::new(1).map_err(mm_error)?;
    let pin_quota = PinQuota::new(
        USER_IO_PIN_MAX_PAGES,
        USER_IO_PIN_MAX_BYTES,
        USER_IO_PIN_MAX_TOKENS,
    );
    let mut user_io_pins = UserIoPinRegistry::new(PAGE_SIZE_4K, pin_quota, 1).map_err(mm_error)?;
    user_io_pins
        .configure_owner(
            PinOwner::new(address_space_id.get()).map_err(mm_error)?,
            pin_quota,
        )
        .map_err(mm_error)?;
    Ok((
        address_space_id,
        topology_mapping_id,
        topology_generation,
        user_io_pins,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MappingIdentityState {
    id: MappingId,
    generation: MappingGeneration,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MappingIdentityEntry {
    lineage: MappingLineage,
    state: MappingIdentityState,
}

/// Bounded, fallibly-grown sidecar for logical mapping identities.
///
/// Mapping lineages are kernel-allocated monotonic values, so they are not an
/// attacker-chosen hash input. Keeping them in a hash index avoids the linear
/// compaction that the old ordered `Vec` paid whenever a mapping was retired,
/// while `reserve_slot` preserves the pre-publication allocation boundary.
#[derive(Debug, Default)]
struct MappingIdentityIndex {
    states: HashMap<MappingLineage, MappingIdentityState>,
}

impl MappingIdentityIndex {
    fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.states.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.states.capacity()
    }

    fn reserve_slot(&mut self, limit: usize) -> AxResult {
        if self.states.len() >= limit {
            return Err(AxError::NoMemory);
        }
        self.states.try_reserve(1).map_err(|_| AxError::NoMemory)
    }

    /// Publishes one identity after `reserve_slot` has admitted its storage.
    fn insert_reserved(
        &mut self,
        lineage: MappingLineage,
        state: MappingIdentityState,
    ) -> AxResult {
        match self.states.entry(lineage) {
            Entry::Vacant(entry) => {
                entry.insert(state);
                Ok(())
            }
            Entry::Occupied(_) => Err(AxError::BadState),
        }
    }

    fn get(&self, lineage: MappingLineage) -> Option<MappingIdentityState> {
        self.states.get(&lineage).copied()
    }

    fn get_mut(&mut self, lineage: MappingLineage) -> Option<&mut MappingIdentityState> {
        self.states.get_mut(&lineage)
    }

    fn remove(&mut self, lineage: MappingLineage) -> Option<MappingIdentityState> {
        self.states.remove(&lineage)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingIdentityMutation {
    Advance {
        lineage: MappingLineage,
        generation: MappingGeneration,
    },
    Retire {
        lineage: MappingLineage,
    },
}

impl MappingIdentityMutation {
    const fn lineage(self) -> MappingLineage {
        match self {
            Self::Advance { lineage, .. } | Self::Retire { lineage } => lineage,
        }
    }
}

fn mapping_identity(
    identities: &MappingIdentityIndex,
    lineage: MappingLineage,
) -> AxResult<MappingIdentityState> {
    identities.get(lineage).ok_or(AxError::BadState)
}

fn reserve_mapping_identity_slot(identities: &mut MappingIdentityIndex, limit: usize) -> AxResult {
    identities.reserve_slot(limit)
}

fn lineage_covers_range<B>(
    areas: &MemorySet<B>,
    lineage: MappingLineage,
    mut start: VirtAddr,
    size: usize,
) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    if size == 0 {
        return false;
    }
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    while start < end {
        let Some(area) = areas.find(start) else {
            return false;
        };
        if area.start() > start || area.lineage() != lineage {
            return false;
        }
        start = area.end().min(end);
    }
    true
}

fn range_is_fully_mapped<B>(areas: &MemorySet<B>, mut start: VirtAddr, size: usize) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    if size == 0 {
        return false;
    }
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    while start < end {
        let Some(area) = areas.find(start) else {
            return false;
        };
        if area.start() > start {
            return false;
        }
        start = area.end().min(end);
    }
    true
}

fn lineage_is_contained_in_range<B>(
    areas: &MemorySet<B>,
    lineage: MappingLineage,
    start: VirtAddr,
    size: usize,
) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    if size == 0 {
        return false;
    }
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    areas
        .iter()
        .all(|area| area.lineage() != lineage || (area.start() >= start && area.end() <= end))
}

fn lineage_exactly_covers_range<B>(
    areas: &MemorySet<B>,
    lineage: MappingLineage,
    start: VirtAddr,
    size: usize,
) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    lineage_covers_range(areas, lineage, start, size)
        && lineage_is_contained_in_range(areas, lineage, start, size)
}

fn range_is_empty<B>(areas: &MemorySet<B>, start: VirtAddr, size: usize) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    !range.is_empty() && !areas.overlaps(range)
}

fn range_is_owned_by_lineage<B>(
    areas: &MemorySet<B>,
    lineage: MappingLineage,
    start: VirtAddr,
    size: usize,
) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let Some(end) = start.checked_add(size) else {
        return false;
    };
    size != 0
        && lineage_is_contained_in_range(areas, lineage, start, size)
        && areas
            .iter()
            .all(|area| area.end() <= start || area.start() >= end || area.lineage() == lineage)
}

fn normalize_ranges(ranges: &[VirtAddrRange]) -> AxResult<Vec<VirtAddrRange>> {
    let mut sorted_ranges = Vec::new();
    sorted_ranges
        .try_reserve(ranges.len())
        .map_err(|_| AxError::NoMemory)?;
    sorted_ranges.extend(ranges.iter().copied().filter(|range| !range.is_empty()));
    sorted_ranges.sort_unstable_by_key(|range| range.start);

    let mut normalized_len = 0usize;
    for index in 0..sorted_ranges.len() {
        let range = sorted_ranges[index];
        if normalized_len != 0 {
            let previous = &mut sorted_ranges[normalized_len - 1];
            if range.start <= previous.end {
                if range.end > previous.end {
                    previous.end = range.end;
                }
                continue;
            }
        }
        sorted_ranges[normalized_len] = range;
        normalized_len += 1;
    }
    sorted_ranges.truncate(normalized_len);
    Ok(sorted_ranges)
}

fn projected_fragment_count_after_unmaps<B>(
    areas: &MemorySet<B>,
    ranges: &[VirtAddrRange],
) -> AxResult<usize>
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let ranges = normalize_ranges(ranges)?;

    let mut count = 0usize;
    let mut range_index = 0usize;
    for area in areas.iter() {
        while range_index < ranges.len() && ranges[range_index].end <= area.start() {
            range_index += 1;
        }
        let mut cursor = area.start();
        let mut current_range = range_index;
        while let Some(range) = ranges.get(current_range) {
            if range.end <= cursor {
                current_range += 1;
                continue;
            }
            if range.start >= area.end() {
                break;
            }
            if cursor < range.start {
                count = count.checked_add(1).ok_or(AxError::NoMemory)?;
            }
            cursor = cursor.max(range.end);
            if cursor >= area.end() {
                break;
            }
            current_range += 1;
        }
        if cursor < area.end() {
            count = count.checked_add(1).ok_or(AxError::NoMemory)?;
        }
    }
    Ok(count)
}

fn admit_staged_fragments_after_unmaps<B>(
    areas: &MemorySet<B>,
    ranges: &[VirtAddrRange],
    staged_fragments: usize,
    limit: usize,
) -> AxResult
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let remaining = projected_fragment_count_after_unmaps(areas, ranges)?;
    let projected = remaining
        .checked_add(staged_fragments)
        .ok_or(AxError::NoMemory)?;
    if areas.len().max(projected) > limit {
        return Err(AxError::NoMemory);
    }
    Ok(())
}

fn prepare_mapping_generation_advances_for_range<B>(
    areas: &MemorySet<B>,
    identities: &MappingIdentityIndex,
    start: VirtAddr,
    size: usize,
) -> AxResult<Vec<MappingIdentityMutation>>
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    if size == 0 {
        return Ok(Vec::new());
    }
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let mut lineages = Vec::new();
    lineages
        .try_reserve(areas.len())
        .map_err(|_| AxError::NoMemory)?;
    for area in areas.iter_overlapping(VirtAddrRange::new(start, end)) {
        lineages.push(area.lineage());
    }
    lineages.sort_unstable();
    lineages.dedup();

    let mut mutations = Vec::new();
    mutations
        .try_reserve(lineages.len())
        .map_err(|_| AxError::NoMemory)?;
    for lineage in lineages {
        let generation = mapping_identity(identities, lineage)?
            .generation
            .next()
            .map_err(mm_error)?;
        mutations.push(MappingIdentityMutation::Advance {
            lineage,
            generation,
        });
    }
    Ok(mutations)
}

fn area_is_fully_covered_by_ranges(
    start: VirtAddr,
    end: VirtAddr,
    ranges: &[VirtAddrRange],
    range_index: &mut usize,
) -> (bool, bool) {
    while *range_index < ranges.len() && ranges[*range_index].end <= start {
        *range_index += 1;
    }

    let mut cursor = start;
    let mut affected = false;
    let mut current_range = *range_index;
    while let Some(range) = ranges.get(current_range) {
        if range.end <= cursor {
            current_range += 1;
            continue;
        }
        if range.start >= end {
            break;
        }
        affected = true;
        if range.start > cursor {
            break;
        }
        cursor = cursor.max(range.end.min(end));
        if cursor >= end {
            break;
        }
        current_range += 1;
    }
    (affected, affected && cursor >= end)
}

#[derive(Clone, Copy, Default)]
struct UnmapLineageCoverage {
    survives: bool,
}

fn prepare_unmap_mapping_mutations<B>(
    areas: &MemorySet<B>,
    identities: &MappingIdentityIndex,
    start: VirtAddr,
    size: usize,
) -> AxResult<Vec<MappingIdentityMutation>>
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    if size == 0 {
        return Ok(Vec::new());
    }
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    prepare_unmap_mapping_mutations_for_ranges(areas, identities, &[VirtAddrRange::new(start, end)])
}

fn prepare_unmap_mapping_mutations_for_ranges<B>(
    areas: &MemorySet<B>,
    identities: &MappingIdentityIndex,
    ranges: &[VirtAddrRange],
) -> AxResult<Vec<MappingIdentityMutation>>
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let ranges = normalize_ranges(ranges)?;
    if ranges.is_empty() {
        return Ok(Vec::new());
    }

    let mut coverage = HashMap::<MappingLineage, UnmapLineageCoverage>::new();
    coverage
        .try_reserve(ranges.len().min(areas.len()))
        .map_err(|_| AxError::NoMemory)?;
    let mut range_index = 0usize;
    for area in areas.iter() {
        let (affected, _) =
            area_is_fully_covered_by_ranges(area.start(), area.end(), &ranges, &mut range_index);
        if affected && !coverage.contains_key(&area.lineage()) {
            coverage.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            coverage.insert(area.lineage(), UnmapLineageCoverage::default());
        }
    }

    // The first sweep discovers only affected lineages. A second linear sweep
    // determines whether any fragment of those lineages survives, including a
    // fragment outside the first/last invalidation range. This replaces the
    // old full-VMA coverage allocation and lineage sort without reintroducing
    // a VMA-by-range nested scan.
    range_index = 0;
    for area in areas.iter() {
        let (_, fully_covered) =
            area_is_fully_covered_by_ranges(area.start(), area.end(), &ranges, &mut range_index);
        if let Some(entry) = coverage.get_mut(&area.lineage()) {
            entry.survives |= !fully_covered;
        }
    }

    let mut mutations = Vec::new();
    mutations
        .try_reserve(coverage.len())
        .map_err(|_| AxError::NoMemory)?;
    for (lineage, entry) in coverage {
        let identity = mapping_identity(identities, lineage)?;
        if entry.survives {
            let generation = identity.generation.next().map_err(mm_error)?;
            mutations.push(MappingIdentityMutation::Advance {
                lineage,
                generation,
            });
        } else {
            mutations.push(MappingIdentityMutation::Retire { lineage });
        }
    }
    mutations.sort_unstable_by_key(|mutation| mutation.lineage());
    Ok(mutations)
}

fn commit_mapping_identity_mutations(
    identities: &mut MappingIdentityIndex,
    mutations: &[MappingIdentityMutation],
) {
    for mutation in mutations.iter().copied() {
        match mutation {
            MappingIdentityMutation::Advance {
                lineage,
                generation,
            } => {
                identities
                    .get_mut(lineage)
                    .expect("prepared mapping lineage disappeared before commit")
                    .generation = generation;
            }
            MappingIdentityMutation::Retire { lineage } => {
                identities
                    .remove(lineage)
                    .expect("prepared mapping lineage disappeared before commit");
            }
        }
    }
}

fn allocate_mapping_identity() -> AxResult<(MappingLineage, MappingIdentityState)> {
    let id = allocate_mapping_id()?;
    let raw = id.get();
    let lineage = MappingLineage::new(raw).ok_or(AxError::ResourceBusy)?;
    Ok((
        lineage,
        MappingIdentityState {
            id,
            generation: MappingGeneration::new(1).map_err(mm_error)?,
        },
    ))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UserIoMappingExpectation {
    expected: ExpectedMapping,
    covered: PageRange,
    needs_frame_registry: bool,
}

impl UserIoMappingExpectation {
    pub(crate) const fn needs_frame_registry(&self) -> bool {
        self.needs_frame_registry
    }
}

/// System-wide accounting ownership held across the complete lower pin
/// transaction. The aggregate charge is refunded only after frame/page-cache
/// ownership has been released.
pub(crate) struct UserIoSystemPinCharge {
    charge: Option<PinBudgetCharge>,
}

impl UserIoSystemPinCharge {
    fn reserve(request: PinRequest) -> AxResult<Self> {
        let mut budget = USER_IO_PIN_BUDGET.lock();
        if budget.is_none() {
            *budget = Some(
                UserIoPinBudget::new(
                    PAGE_SIZE_4K,
                    PinQuota::new(
                        USER_IO_PIN_MAX_PAGES,
                        USER_IO_PIN_MAX_BYTES,
                        USER_IO_PIN_MAX_TOKENS,
                    ),
                    1,
                )
                .map_err(mm_error)?,
            );
        }
        let charge = budget
            .as_mut()
            .expect("initialized system user-I/O pin budget")
            .reserve(request)
            .map_err(mm_error)?;
        Ok(Self {
            charge: Some(charge),
        })
    }
}

impl Drop for UserIoSystemPinCharge {
    fn drop(&mut self) {
        let Some(charge) = self.charge.take() else {
            return;
        };
        USER_IO_PIN_BUDGET
            .lock()
            .as_mut()
            .expect("initialized system user-I/O pin budget")
            .release(charge)
            .expect("live system user-I/O pin charge disappeared");
    }
}

/// The virtual memory address space.
pub struct AddrSpace {
    va_range: VirtAddrRange,
    address_space_id: AddressSpaceId,
    hardware_asid: HardwareAddressSpaceId,
    topology_mapping_id: MappingId,
    topology_generation: MappingGeneration,
    areas: MemorySet<Backend>,
    mapping_identities: MappingIdentityIndex,
    growdown_starts: BTreeSet<VirtAddr>,
    wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
    user_io_pins: UserIoPinRegistry,
    pub(super) uffd: Option<Box<super::userfaultfd::UffdAddressSpaceState>>,
    lock_future_mappings: bool,
    lock_future_on_fault: bool,
    pt: PageTable,
}

/// The generic, testable core of one linear protection transaction.
///
/// This value owns the only mutable access to both the area tree and its page
/// table until it is either committed or dropped.
struct PreparedAreaProtect<'a, B: memory_set::MappingBackend> {
    areas: &'a mut MemorySet<B>,
    page_table: &'a mut B::PageTable,
    start: B::Addr,
    end: B::Addr,
    flags: B::Flags,
    max_areas: usize,
}

impl<'a, B: memory_set::MappingBackend> PreparedAreaProtect<'a, B> {
    fn segments(&self) -> impl Iterator<Item = (&MemoryArea<B>, B::Addr, B::Addr)> + '_ {
        let start = self.start;
        let end = self.end;
        self.areas.iter().filter_map(move |area| {
            let affected_start = area.start().max(start);
            let affected_end = area.end().min(end);
            (affected_start < affected_end).then_some((area, affected_start, affected_end))
        })
    }

    fn commit(self) -> MappingResult<&'a mut MemorySet<B>> {
        let Self {
            areas,
            page_table,
            start,
            end,
            flags,
            max_areas,
        } = self;
        areas.protect_with_limit(
            start,
            end.sub_addr(start),
            |_| Some(flags),
            page_table,
            max_areas,
        )?;
        Ok(areas)
    }
}

/// Commits the main area transaction before handing a prepared sidecar back
/// to its caller for publication.
///
/// The synchronization callback runs after the page-table attempt on both the
/// success and failure paths. If the main transaction fails, `?` drops the
/// sidecar in this function; an RAII sidecar can therefore abort its own
/// preflight authority before the error escapes. On success, the caller gets
/// the still-armed sidecar back and may publish it only after any infallible
/// main-MM bookkeeping that must precede sidecar visibility.
fn commit_area_before_sidecar<'a, B, S>(
    transaction: PreparedAreaProtect<'a, B>,
    sidecar: S,
    synchronize: impl FnOnce(),
) -> MappingResult<(&'a mut MemorySet<B>, S)>
where
    B: memory_set::MappingBackend,
{
    let areas = transaction.commit();
    synchronize();
    let areas = areas?;
    Ok((areas, sidecar))
}

#[derive(Clone, Copy)]
struct ProjectedProtectPiece<'a, B: memory_set::MappingBackend> {
    area: &'a MemoryArea<B>,
    start: B::Addr,
    end: B::Addr,
    flags: B::Flags,
}

struct ProjectedProtectRun<'a, B: memory_set::MappingBackend> {
    left_area: &'a MemoryArea<B>,
    start: B::Addr,
    end: B::Addr,
    flags: B::Flags,
}

fn projected_protect_pieces_share_structure<B: memory_set::MappingBackend>(
    left: &ProjectedProtectPiece<'_, B>,
    right: &ProjectedProtectPiece<'_, B>,
) -> bool {
    left.end == right.start
        && left.flags == right.flags
        && left.area.lineage() == right.area.lineage()
}

fn projected_protect_pieces_merge<B: memory_set::MappingBackend>(
    left: &ProjectedProtectPiece<'_, B>,
    right: &ProjectedProtectPiece<'_, B>,
) -> bool {
    projected_protect_pieces_share_structure(left, right)
        && memory_set::MappingBackend::can_merge(left.area.backend(), right.area.backend())
}

/// One immutable, pre-change VMA view in a prepared protection transaction.
///
/// The full area bounds identify the VMA that future policy hooks must inspect;
/// the affected bounds identify the subrange this transaction will change.
/// Neither the view nor its backend reference permits mutation.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct PreparedProtectSegment<'a> {
    area: &'a MemoryArea<Backend>,
    affected: VirtAddrRange,
}

#[allow(dead_code)]
impl<'a> PreparedProtectSegment<'a> {
    #[cfg(test)]
    pub(crate) const fn for_test(area: &'a MemoryArea<Backend>, affected: VirtAddrRange) -> Self {
        Self { area, affected }
    }

    pub(crate) const fn area_start(self) -> VirtAddr {
        self.area.start()
    }

    pub(crate) const fn area_end(self) -> VirtAddr {
        self.area.end()
    }

    pub(crate) const fn affected(self) -> VirtAddrRange {
        self.affected
    }

    pub(crate) const fn flags(self) -> MappingFlags {
        self.area.flags()
    }

    pub(crate) const fn backend(self) -> &'a Backend {
        self.area.backend()
    }

    pub(crate) fn file_mapping(self) -> Option<&'a FileMappingLease> {
        self.area.backend().file_mapping()
    }

    pub(crate) fn area_file_offset(self) -> Option<u64> {
        self.file_mapping()?.file_offset_at(self.area.start())
    }

    pub(crate) fn affected_file_offset(self) -> Option<u64> {
        self.file_mapping()?.file_offset_at(self.affected.start)
    }
}

/// Linear admission for one fully preflighted `mprotect` transaction.
///
/// Construction validates every target VMA without changing the area tree,
/// page table, pin state, or backend state. Dropping the value aborts with no
/// side effects; only [`Self::commit`] starts the existing transactional
/// split/protect/merge path.
#[must_use = "a prepared protection must be committed explicitly or dropped to abort"]
pub(crate) struct PreparedProtect<'a> {
    transaction: PreparedAreaProtect<'a, Backend>,
    growdown_starts: &'a mut BTreeSet<VirtAddr>,
    topology_generation: &'a mut MappingGeneration,
    next_topology_generation: MappingGeneration,
    mapping_identities: &'a mut MappingIdentityIndex,
    mapping_mutations: Vec<MappingIdentityMutation>,
    uffd_mutation: Option<PreparedUffdMutation<'a>>,
    synchronize_instruction_stream: bool,
}

enum RemapDestination {
    Empty,
    Replace,
}

/// Fixed-remap failure classification used by syscall glue to invalidate
/// per-range policy only when the transaction changed visible mappings.
#[derive(Debug)]
pub(crate) enum ReplaceMappingError {
    AddressSpacePreserved(AxError),
    AddressSpaceChanged(AxError),
}

impl ReplaceMappingError {
    pub(crate) const fn mapping_changed(&self) -> bool {
        matches!(self, Self::AddressSpaceChanged(_))
    }

    pub(crate) fn into_error(self) -> AxError {
        match self {
            Self::AddressSpacePreserved(error) | Self::AddressSpaceChanged(error) => error,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExistingLineageMapError {
    Preserved(AxError),
    Published(AxError),
}

impl ExistingLineageMapError {
    pub(crate) const fn published(self) -> bool {
        matches!(self, Self::Published(_))
    }

    pub(crate) const fn into_error(self) -> AxError {
        match self {
            Self::Preserved(error) | Self::Published(error) => error,
        }
    }
}

const fn classify_existing_lineage_population_failure(
    error: AxError,
    rollback_failed: bool,
) -> ExistingLineageMapError {
    if rollback_failed {
        ExistingLineageMapError::Published(error)
    } else {
        ExistingLineageMapError::Preserved(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemapTransactionEffect {
    Preserved,
    Destructive,
}

const fn classify_failed_remap_effect(
    destination_changed: bool,
    rollback_failed: bool,
) -> RemapTransactionEffect {
    if destination_changed || rollback_failed {
        RemapTransactionEffect::Destructive
    } else {
        RemapTransactionEffect::Preserved
    }
}

#[derive(Debug)]
struct MappingTransactionFailure {
    error: AxError,
    effect: RemapTransactionEffect,
}

impl MappingTransactionFailure {
    fn preserved(error: AxError) -> Self {
        Self {
            error,
            effect: RemapTransactionEffect::Preserved,
        }
    }

    fn into_replace_error(self) -> ReplaceMappingError {
        match self.effect {
            RemapTransactionEffect::Preserved => {
                ReplaceMappingError::AddressSpacePreserved(self.error)
            }
            RemapTransactionEffect::Destructive => {
                ReplaceMappingError::AddressSpaceChanged(self.error)
            }
        }
    }
}

struct RelativePolicyRange {
    offset: usize,
    size: usize,
}

struct RemapPolicyPlan {
    growdown: bool,
    wipe_on_fork: Vec<RelativePolicyRange>,
    dontfork: Vec<RelativePolicyRange>,
    locked: Vec<RelativePolicyRange>,
}

impl PreparedProtect<'_> {
    /// Iterates the exact pre-change VMAs in increasing virtual-address order.
    #[allow(dead_code)]
    pub(crate) fn segments(&self) -> impl Iterator<Item = PreparedProtectSegment<'_>> + '_ {
        self.transaction
            .segments()
            .map(
                |(area, affected_start, affected_end)| PreparedProtectSegment {
                    area,
                    affected: VirtAddrRange::new(affected_start, affected_end),
                },
            )
    }

    /// Commits the already-preflighted request through MemorySet's staged
    /// split/protect/rollback/merge transaction.
    pub(crate) fn commit(self) -> AxResult<DeferredUffdWake> {
        let Self {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            mapping_identities,
            mapping_mutations,
            uffd_mutation,
            synchronize_instruction_stream,
        } = self;
        let (areas, uffd_mutation) =
            commit_area_before_sidecar(transaction, uffd_mutation, || {
                if synchronize_instruction_stream {
                    let _ = super::synchronize_tlb_and_icache();
                } else {
                    let _ = super::synchronize_tlb();
                }
            })?;
        Self::refresh_growdown_starts(areas, growdown_starts);
        let wake = uffd_mutation.map_or_else(DeferredUffdWake::empty, |mutation| mutation.commit());
        commit_mapping_identity_mutations(mapping_identities, &mapping_mutations);
        *topology_generation = next_topology_generation;
        Ok(wake)
    }

    fn refresh_growdown_starts(
        areas: &MemorySet<Backend>,
        growdown_starts: &mut BTreeSet<VirtAddr>,
    ) {
        let starts: Vec<_> = growdown_starts.iter().copied().collect();
        growdown_starts.clear();
        for start in starts {
            if areas.find(start).is_some_and(|area| area.start() == start) {
                growdown_starts.insert(start);
            }
        }
    }
}

impl AddrSpace {
    const STACK_GUARD_GAP_PAGES: usize = 256;

    /// Returns the address space base.
    pub const fn base(&self) -> VirtAddr {
        self.va_range.start
    }

    /// Returns the address space end.
    pub const fn end(&self) -> VirtAddr {
        self.va_range.end
    }

    /// Returns the stable policy identity of this address space.
    pub(super) const fn address_space_id(&self) -> AddressSpaceId {
        self.address_space_id
    }

    /// Returns the address space size.
    pub fn size(&self) -> usize {
        self.va_range.size()
    }

    /// Returns the reference to the inner page table.
    pub const fn page_table(&self) -> &PageTable {
        &self.pt
    }

    /// Returns a mutable reference to the inner page table.
    pub const fn page_table_mut(&mut self) -> &mut PageTable {
        &mut self.pt
    }

    /// Returns the root physical address of the inner page table.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pt.root_paddr()
    }

    /// Returns the page-table root and bounded hardware-ASID identity.
    pub const fn address_space_token(&self) -> AddressSpaceToken {
        AddressSpaceToken::new(self.pt.root_paddr(), self.hardware_asid)
    }

    /// Checks if the address space contains the given address range.
    pub fn contains_range(&self, start: VirtAddr, size: usize) -> bool {
        self.va_range.contains(start) && (self.va_range.end - start) >= size
    }

    /// Creates a new empty address space.
    pub fn new_empty(base: VirtAddr, size: usize) -> AxResult<Self> {
        let va_range = VirtAddrRange::try_from_start_size(base, size).ok_or(AxError::NoMemory)?;
        let (address_space_id, topology_mapping_id, topology_generation, user_io_pins) =
            new_user_io_policy()?;
        let hardware_asid = reserve_hardware_address_space_id();
        Ok(Self {
            va_range,
            address_space_id,
            hardware_asid,
            topology_mapping_id,
            topology_generation,
            areas: MemorySet::new(),
            mapping_identities: MappingIdentityIndex::new(),
            growdown_starts: BTreeSet::new(),
            wipe_on_fork_ranges: BTreeMap::new(),
            dontfork_ranges: BTreeMap::new(),
            locked_ranges: BTreeMap::new(),
            user_io_pins,
            uffd: None,
            lock_future_mappings: false,
            lock_future_on_fault: false,
            pt: PageTable::try_new().map_err(|_| AxError::NoMemory)?,
        })
    }

    fn prepare_fresh_mapping_lineage(&mut self) -> AxResult<MappingLineage> {
        reserve_mapping_identity_slot(&mut self.mapping_identities, MAX_MAPPING_LINEAGES)?;
        let (lineage, identity) = allocate_mapping_identity()?;
        debug_assert_eq!(lineage.get(), identity.id.get());
        debug_assert_ne!(identity.id, self.topology_mapping_id);
        self.mapping_identities.insert_reserved(lineage, identity)?;
        Ok(lineage)
    }

    fn remove_mapping_lineage_if_unused(&mut self, lineage: MappingLineage) -> bool {
        if self.areas.iter().any(|area| area.lineage() == lineage) {
            return false;
        }
        self.mapping_identities.remove(lineage).is_some()
    }

    fn mapping_identity(&self, lineage: MappingLineage) -> AxResult<MappingIdentityState> {
        mapping_identity(&self.mapping_identities, lineage)
    }

    fn commit_mapping_generation(
        &mut self,
        lineage: MappingLineage,
        generation: MappingGeneration,
    ) {
        self.mapping_identities
            .get_mut(lineage)
            .expect("existing mapping lineage disappeared")
            .generation = generation;
    }

    fn refresh_growdown_starts(&mut self) {
        PreparedProtect::refresh_growdown_starts(&self.areas, &mut self.growdown_starts);
    }

    pub fn mark_growdown(&mut self, start: VirtAddr) {
        self.growdown_starts.insert(start);
        self.refresh_growdown_starts();
    }

    fn move_growdown_start(&mut self, old_start: VirtAddr, new_start: VirtAddr) {
        if self.growdown_starts.remove(&old_start) {
            self.growdown_starts.insert(new_start);
        }
    }

    fn insert_interval(ranges: &mut BTreeMap<VirtAddr, VirtAddr>, start: VirtAddr, end: VirtAddr) {
        if start >= end {
            return;
        }

        let mut new_start = start;
        let mut new_end = end;
        let overlaps: Vec<_> = ranges
            .range(..=end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end >= start && range_start <= end).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            ranges.remove(&range_start);
            new_start = new_start.min(range_start);
            new_end = new_end.max(range_end);
        }
        ranges.insert(new_start, new_end);
    }

    fn clear_interval(ranges: &mut BTreeMap<VirtAddr, VirtAddr>, start: VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let overlaps: Vec<_> = ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end > start).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            ranges.remove(&range_start);
            if range_start < start {
                ranges.insert(range_start, start);
            }
            if range_end > end {
                ranges.insert(end, range_end);
            }
        }
    }

    fn interval_end_covering(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        addr: VirtAddr,
    ) -> Option<VirtAddr> {
        ranges
            .range(..=addr)
            .last()
            .and_then(|(&range_start, &range_end)| {
                (range_start <= addr && range_end > addr).then_some(range_end)
            })
    }

    fn next_interval_start(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        addr: VirtAddr,
        limit: VirtAddr,
    ) -> Option<VirtAddr> {
        ranges
            .range(addr..)
            .filter_map(|(&range_start, _)| {
                (range_start > addr && range_start < limit).then_some(range_start)
            })
            .next()
    }

    fn interval_overlaps(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        start: VirtAddr,
        end: VirtAddr,
    ) -> bool {
        ranges
            .range(..end)
            .any(|(&range_start, &range_end)| range_end > start && range_start < end)
    }

    fn fork_fragment_count(&self) -> AxResult<usize> {
        let mut count = 0usize;
        for area in self.areas.iter() {
            let mut cursor = area.start();
            while cursor < area.end() {
                if let Some(dontfork_end) =
                    Self::interval_end_covering(&self.dontfork_ranges, cursor)
                {
                    cursor = dontfork_end.min(area.end());
                    continue;
                }

                let mut segment_end = area.end();
                if let Some(wipe_end) =
                    Self::interval_end_covering(&self.wipe_on_fork_ranges, cursor)
                {
                    segment_end = segment_end.min(wipe_end);
                } else if let Some(next_wipe) =
                    Self::next_interval_start(&self.wipe_on_fork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_wipe);
                }
                if let Some(next_dontfork) =
                    Self::next_interval_start(&self.dontfork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_dontfork);
                }

                if cursor >= segment_end {
                    return Err(AxError::BadState);
                }
                count = count.checked_add(1).ok_or(AxError::NoMemory)?;
                cursor = segment_end;
            }
        }
        Ok(count)
    }

    fn collect_relative_policy_ranges(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        source_start: VirtAddr,
        preserve_size: usize,
    ) -> AxResult<Vec<RelativePolicyRange>> {
        let source_end = source_start
            .checked_add(preserve_size)
            .ok_or(AxError::InvalidInput)?;
        let mut relative = Vec::new();
        relative
            .try_reserve(ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        for (&range_start, &range_end) in ranges.range(..source_end) {
            let start = range_start.max(source_start);
            let end = range_end.min(source_end);
            if start < end {
                relative.push(RelativePolicyRange {
                    offset: start.sub_addr(source_start),
                    size: end.sub_addr(start),
                });
            }
        }
        Ok(relative)
    }

    fn prepare_remap_policy(
        &self,
        source_start: VirtAddr,
        source_size: usize,
        destination_size: usize,
    ) -> AxResult<RemapPolicyPlan> {
        let preserve_size = source_size.min(destination_size);
        if preserve_size == 0 {
            return Err(AxError::InvalidInput);
        }
        let source_end = source_start
            .checked_add(source_size)
            .ok_or(AxError::InvalidInput)?;
        let mut wipe_on_fork = Self::collect_relative_policy_ranges(
            &self.wipe_on_fork_ranges,
            source_start,
            preserve_size,
        )?;
        let mut dontfork = Self::collect_relative_policy_ranges(
            &self.dontfork_ranges,
            source_start,
            preserve_size,
        )?;
        let mut locked =
            Self::collect_relative_policy_ranges(&self.locked_ranges, source_start, preserve_size)?;

        let growth = destination_size.saturating_sub(source_size);
        if growth != 0 {
            let last_source_byte = source_end - 1;
            if Self::interval_end_covering(&self.wipe_on_fork_ranges, last_source_byte).is_some() {
                wipe_on_fork.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                wipe_on_fork.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
            if Self::interval_end_covering(&self.dontfork_ranges, last_source_byte).is_some() {
                dontfork.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                dontfork.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
            if self.range_is_fully_locked(source_start, source_size) {
                locked.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                locked.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
        }

        Ok(RemapPolicyPlan {
            growdown: self.growdown_starts.contains(&source_start),
            wipe_on_fork,
            dontfork,
            locked,
        })
    }

    fn apply_remap_policy(&mut self, destination_start: VirtAddr, plan: &RemapPolicyPlan) {
        if plan.growdown {
            self.growdown_starts.insert(destination_start);
        }
        for range in &plan.wipe_on_fork {
            let start = destination_start + range.offset;
            Self::insert_interval(&mut self.wipe_on_fork_ranges, start, start + range.size);
        }
        for range in &plan.dontfork {
            let start = destination_start + range.offset;
            Self::insert_interval(&mut self.dontfork_ranges, start, start + range.size);
        }
        for range in &plan.locked {
            let start = destination_start + range.offset;
            self.insert_locked_range(start, start + range.size);
        }
    }

    pub fn set_wipe_on_fork(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        if enabled {
            Self::insert_interval(&mut self.wipe_on_fork_ranges, start, start + size);
        }
        Ok(())
    }

    pub fn set_dontfork(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        if !enabled {
            Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        }
        if enabled {
            Self::insert_interval(&mut self.dontfork_ranges, start, start + size);
        }
        Ok(())
    }

    fn insert_locked_range(&mut self, start: VirtAddr, end: VirtAddr) {
        if start >= end {
            return;
        }

        let mut new_start = start;
        let mut new_end = end;
        let overlaps: Vec<_> = self
            .locked_ranges
            .range(..=end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end >= start && range_start <= end).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            self.locked_ranges.remove(&range_start);
            new_start = new_start.min(range_start);
            new_end = new_end.max(range_end);
        }
        self.locked_ranges.insert(new_start, new_end);
    }

    fn clear_locked_range(&mut self, start: VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let overlaps: Vec<_> = self
            .locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end > start).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            self.locked_ranges.remove(&range_start);
            if range_start < start {
                self.locked_ranges.insert(range_start, start);
            }
            if range_end > end {
                self.locked_ranges.insert(end, range_end);
            }
        }
    }

    pub fn set_locked(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        self.clear_locked_range(start, size);
        if enabled {
            self.insert_locked_range(start, start + size);
        }
        Ok(())
    }

    pub fn range_is_locked(&self, start: VirtAddr, size: usize) -> bool {
        if size == 0 {
            return false;
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .any(|(&range_start, &range_end)| range_end > start && range_start < end)
    }

    pub fn locked_bytes(&self) -> usize {
        self.locked_ranges
            .iter()
            .map(|(start, end)| end.sub_addr(*start))
            .sum()
    }

    pub fn locked_bytes_in_range(&self, start: VirtAddr, size: usize) -> usize {
        if size == 0 {
            return 0;
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                if range_end <= start {
                    return None;
                }
                let overlap_start = range_start.max(start);
                let overlap_end = range_end.min(end);
                (overlap_start < overlap_end).then_some(overlap_end.sub_addr(overlap_start))
            })
            .sum()
    }

    pub fn locked_segments_in_range(&self, start: VirtAddr, size: usize) -> Vec<(VirtAddr, usize)> {
        if size == 0 {
            return Vec::new();
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                if range_end <= start {
                    return None;
                }
                let overlap_start = range_start.max(start);
                let overlap_end = range_end.min(end);
                (overlap_start < overlap_end)
                    .then_some((overlap_start, overlap_end.sub_addr(overlap_start)))
            })
            .collect()
    }

    pub fn range_is_fully_locked(&self, start: VirtAddr, size: usize) -> bool {
        size > 0 && self.locked_bytes_in_range(start, size) == size
    }

    pub(crate) fn user_io_pin_owner(&self) -> PinOwner {
        PinOwner::new(self.address_space_id.get()).expect("address-space IDs are nonzero")
    }

    pub(crate) fn begin_user_io_pin(
        &mut self,
        request: PinRequest,
    ) -> AxResult<(PinReservation, UserIoSystemPinCharge)> {
        if request.owner() != self.user_io_pin_owner() {
            return Err(AxError::InvalidInput);
        }
        let system_charge = UserIoSystemPinCharge::reserve(request)?;
        let reservation = self
            .user_io_pins
            .reserve(request, self.address_space_id)
            .map_err(mm_error)?;
        Ok((reservation, system_charge))
    }

    pub(crate) fn cancel_user_io_pin(&mut self, reservation: PinReservation) {
        // `revalidate_next` removes a stale reservation itself. Treat that
        // already-rolled-back state as successful cancellation so adapter RAII
        // can use one cleanup path for every preparation failure.
        if self.user_io_pins.view(reservation.token()).is_err() {
            return;
        }
        if let Err(error) = self.user_io_pins.cancel_reservation(reservation) {
            warn!(
                "AddrSpace::cancel_user_io_pin: token {}: {error:?}",
                reservation.token().get()
            );
        }
    }

    /// Revalidates one caller-bounded window of a reserved user-I/O pin.
    ///
    /// The reservation was published before any expectation or lower owner was
    /// collected, and every overlapping mapping mutation consults
    /// `user_io_pins`. It is therefore the range mutation fence between these
    /// short address-space lock acquisitions. `expectations` must begin at
    /// `start`, remain inside this window, and cover it without a gap; the
    /// return value tells the caller how many entries to remove from its
    /// remaining prefix.
    pub(crate) fn revalidate_user_io_pin_window(
        &mut self,
        reservation: PinReservation,
        expectations: &[UserIoMappingExpectation],
        start: VirtAddr,
        size: usize,
    ) -> AxResult<usize> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        let mut consumed = 0usize;
        for expectation in expectations {
            if VirtAddr::from(expectation.covered.start()) != cursor
                || expectation.covered.end() > end.as_usize()
            {
                return Err(AxError::BadState);
            }
            let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
            let current = self.mapping_snapshot(area)?;
            self.user_io_pins
                .revalidate_next(
                    reservation,
                    expectation.expected,
                    current,
                    expectation.covered,
                )
                .map_err(mm_error)?;
            cursor = VirtAddr::from(expectation.covered.end());
            consumed = consumed.checked_add(1).ok_or(AxError::NoMemory)?;
            if cursor == end {
                break;
            }
        }
        if cursor != end {
            return Err(AxError::BadState);
        }
        Ok(consumed)
    }

    /// Turns a fully revalidated reservation into an active lease.
    ///
    /// Every per-VMA operation has already completed in bounded windows, so
    /// this final address-space critical section is constant-time.
    pub(crate) fn commit_user_io_pin(&mut self, reservation: PinReservation) -> AxResult<PinToken> {
        self.user_io_pins.commit(reservation).map_err(mm_error)
    }

    pub(crate) fn end_user_io_pin(&mut self, token: PinToken) {
        if let Err(error) = self.user_io_pins.release(token) {
            warn!(
                "AddrSpace::end_user_io_pin: token {}: {error:?}",
                token.get()
            );
        }
    }

    pub fn user_io_pin_overlaps(&self, start: VirtAddr, size: usize) -> bool {
        self.invalidation(start, size, InvalidationReason::Unmap)
            .map_or(true, |invalidation| {
                self.user_io_pins
                    .first_mutation_blocker(invalidation)
                    .is_some()
            })
    }

    fn check_no_user_io_pin_overlap(
        &self,
        start: VirtAddr,
        size: usize,
        reason: InvalidationReason,
    ) -> AxResult {
        let invalidation = self.invalidation(start, size, reason)?;
        self.user_io_pins
            .admit_mutation(invalidation)
            .map_err(mm_error)
    }

    /// Appends mapping expectations for one caller-bounded scan window.
    ///
    /// `snapshots` must reserve at least one slot per covered page before the
    /// caller takes the address-space lock. VMAs are page-aligned, so this
    /// method can then split a VMA at the window boundary and push without any
    /// allocation. Partial output on error remains owned by unpublished-pin
    /// RAII and must be dropped only after the caller releases the lock.
    pub(crate) fn append_user_io_mapping_expectations(
        &self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
        snapshots: &mut Vec<UserIoMappingExpectation>,
    ) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let page_count = size / PAGE_SIZE_4K;
        if snapshots.capacity().saturating_sub(snapshots.len()) < page_count {
            return Err(AxError::NoMemory);
        }

        let mut cursor = start;
        let end = start + size;
        while cursor < end {
            let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
            if area.start() > cursor || !area.flags().contains(access_flags) {
                return Err(AxError::BadAddress);
            }
            let segment_end = area.end().min(end);
            let covered = PageRange::new(
                cursor.as_usize(),
                segment_end.sub_addr(cursor),
                PAGE_SIZE_4K,
            )
            .map_err(mm_error)?;
            snapshots.push(UserIoMappingExpectation {
                expected: self.mapping_snapshot(area)?.expected(),
                covered,
                needs_frame_registry: area.backend().supports_user_io_frame_pin(),
            });
            cursor = segment_end;
        }
        Ok(())
    }

    fn mapping_snapshot(&self, area: &MemoryArea<Backend>) -> AxResult<MappingSnapshot> {
        Self::mapping_snapshot_from_parts(self.address_space_id, &self.mapping_identities, area)
    }

    fn uffd_resolver_area_in(
        areas: &MemorySet<Backend>,
        destination: PageRange,
    ) -> AxResult<&MemoryArea<Backend>> {
        if destination.page_size().bytes() != PAGE_SIZE_4K {
            return Err(AxError::InvalidInput);
        }
        let start = VirtAddr::from(destination.start());
        let area = areas
            .find(start)
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?;
        if area.start() > start
            || area.end().as_usize() < destination.end()
            || !area.backend().supports_uffd_missing_resolver()
        {
            return Err(AxError::from(axerrno::LinuxError::ENOENT));
        }
        Ok(area)
    }

    /// Freezes whole-range COPY/ZEROPAGE authority before any page or
    /// page-table allocation.
    ///
    /// Linux requires the complete destination to remain within one
    /// compatible registered VMA. The returned lease is revalidated for every
    /// page publication, allowing the long-running ioctl to release the
    /// address-space mutex between pages without weakening that rule.
    pub(crate) fn preflight_uffd_resolver_range(
        &self,
        destination: PageRange,
    ) -> AxResult<UffdResolverLease> {
        let area = Self::uffd_resolver_area_in(&self.areas, destination)?;
        let mapping = self.mapping_snapshot(area)?;
        self.uffd
            .as_ref()
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?
            .prepare_resolver(mapping, destination)
    }

    /// Publishes one fully initialized resolver page with a short
    /// address-space critical section.
    ///
    /// All allocation, source usercopy, unused-owner reclamation, executable
    /// synchronization, signed-result copyout, and waiter wake remain outside
    /// this method. Under the mutex it only revalidates VMA/registration/PTE
    /// state, publishes one prepared leaf, and records an immutable deferred
    /// broker completion.
    pub(crate) fn publish_prepared_uffd_page(
        &mut self,
        lease: UffdResolverLease,
        page: PageRange,
        disposition: FaultDisposition,
        prepared: &mut PreparedCowPage,
        icache_synchronization: Option<UffdIcacheSynchronization>,
    ) -> AxResult<UffdPagePublication> {
        if page.len() != PAGE_SIZE_4K || !lease.destination().contains(page) {
            return Err(AxError::InvalidInput);
        }

        let Self {
            address_space_id,
            areas,
            mapping_identities,
            uffd,
            pt,
            ..
        } = self;
        let area = Self::uffd_resolver_area_in(areas, lease.destination())?;
        let mapping =
            Self::mapping_snapshot_from_parts(*address_space_id, mapping_identities, area)?;
        let state = uffd
            .as_mut()
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?;
        let current = state.revalidate_resolver(lease, mapping)?;
        let flags = area.flags();
        if flags.contains(MappingFlags::EXECUTE) && icache_synchronization.is_none() {
            return Ok(UffdPagePublication::NeedsIcacheSynchronization);
        }
        let completions = state.validate_resolver_completion(lease, current, page, disposition)?;
        area.backend().publish_prepared_cow_page(
            VirtAddr::from(page.start()),
            flags,
            pt,
            prepared,
        )?;
        state.defer_resolver_completions(completions);
        Ok(UffdPagePublication::Published)
    }

    /// Applies one handler-scoped UFFDIO_WAKE transition.
    ///
    /// The returned receipt owns every PollSet wake and must be finished only
    /// after the caller releases the address-space mutex.
    pub(crate) fn wake_uffd_handler_range(
        &mut self,
        handler: FaultHandlerId,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        self.uffd
            .as_mut()
            .ok_or(AxError::BadState)?
            .wake_handler_range(handler, range)
    }

    /// Returns the current mapping snapshot only when `address` is covered by
    /// a VMA and its 4 KiB page-table leaf is still absent.
    ///
    /// This is a generic, read-only admission primitive. It does not interpret
    /// Linux userfaultfd registration or allocate page-table state; the
    /// adapter performs that policy check separately while retaining the same
    /// address-space lock.
    pub(super) fn missing_mapping_snapshot_at(
        &self,
        address: VirtAddr,
    ) -> AxResult<Option<MappingSnapshot>> {
        if !self.va_range.contains(address) {
            return Ok(None);
        }
        let Some(area) = self.areas.find(address) else {
            return Ok(None);
        };
        let page = address.align_down(PAGE_SIZE_4K);
        match self.pt.query(page) {
            Ok(_) => Ok(None),
            Err(PagingError::NotMapped) => self.mapping_snapshot(area).map(Some),
            Err(_) => Err(AxError::BadState),
        }
    }

    fn mapping_snapshot_from_parts(
        address_space_id: AddressSpaceId,
        mapping_identities: &MappingIdentityIndex,
        area: &MemoryArea<Backend>,
    ) -> AxResult<MappingSnapshot> {
        let flags = area.flags();
        let identity = mapping_identity(mapping_identities, area.lineage())?;
        let range =
            PageRange::new(area.start().as_usize(), area.size(), PAGE_SIZE_4K).map_err(mm_error)?;
        Ok(MappingSnapshot::new(
            address_space_id,
            identity.id,
            identity.generation,
            range,
            MappingAccess::new(
                flags.contains(MappingFlags::READ),
                flags.contains(MappingFlags::WRITE),
                flags.contains(MappingFlags::EXECUTE),
            ),
            area.backend().linux_mapping_kind(),
            false,
            false,
        ))
    }

    fn projected_protect_piece_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        new_flags: B::Flags,
        address: B::Addr,
    ) -> Option<ProjectedProtectPiece<'a, B>> {
        let area = areas.find(address)?;
        let (start, end, flags) = if address < protect.start {
            (area.start(), area.end().min(protect.start), area.flags())
        } else if address < protect.end {
            (
                area.start().max(protect.start),
                area.end().min(protect.end),
                new_flags,
            )
        } else {
            (area.start().max(protect.end), area.end(), area.flags())
        };
        (start < end).then_some(ProjectedProtectPiece {
            area,
            start,
            end,
            flags,
        })
    }

    /// Projects MemorySet's exact post-mprotect merge law without mutating the
    /// area tree or allocating. The scan replays the retained-left merge law
    /// for the final structurally compatible run containing `address`.
    fn projected_protect_run_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        new_flags: B::Flags,
        address: B::Addr,
    ) -> Option<ProjectedProtectRun<'a, B>> {
        let mut anchor = Self::projected_protect_piece_at(areas, protect, new_flags, address)?;

        // Backend compatibility is deliberately not a left-scan barrier.
        // MemorySet processes protection actions in ascending address order,
        // retains the left backend after a merge, and starts a new run after
        // an incompatible pair. Replaying from the first structurally
        // compatible piece is therefore necessary because `can_merge` is not
        // required to be transitive.
        while Into::<usize>::into(anchor.start) != 0 {
            let previous_address = B::Addr::from(Into::<usize>::into(anchor.start) - 1);
            let Some(previous) =
                Self::projected_protect_piece_at(areas, protect, new_flags, previous_address)
            else {
                break;
            };
            if !projected_protect_pieces_share_structure(&previous, &anchor) {
                break;
            }
            anchor = previous;
        }

        let mut run = ProjectedProtectRun {
            left_area: anchor.area,
            start: anchor.start,
            end: anchor.end,
            flags: anchor.flags,
        };
        loop {
            let Some(next) = Self::projected_protect_piece_at(areas, protect, new_flags, run.end)
            else {
                return (run.start <= address && address < run.end).then_some(run);
            };
            // MemorySet retains the left/current area when it absorbs a right
            // neighbor. Keep comparing that surviving backend against every
            // later neighbor; `can_merge` is not required to be transitive.
            let survivor = ProjectedProtectPiece {
                area: run.left_area,
                start: run.start,
                end: run.end,
                flags: run.flags,
            };
            if projected_protect_pieces_merge(&survivor, &next) {
                run.end = next.end;
                continue;
            }
            if run.start <= address && address < run.end {
                return Some(run);
            }
            if !projected_protect_pieces_share_structure(&survivor, &next) {
                return None;
            }
            run = ProjectedProtectRun {
                left_area: next.area,
                start: next.start,
                end: next.end,
                flags: next.flags,
            };
        }
    }

    fn projected_uffd_protect_snapshot(
        address_space_id: AddressSpaceId,
        areas: &MemorySet<Backend>,
        mapping_identities: &MappingIdentityIndex,
        protect: VirtAddrRange,
        new_flags: MappingFlags,
        registration: UffdRegistration,
        fragment: PageRange,
    ) -> AxResult<Option<MappingSnapshot>> {
        let current = Self::uffd_snapshot_for_registration(
            address_space_id,
            areas,
            mapping_identities,
            registration,
        )?;
        let run = Self::projected_protect_run_at(
            areas,
            protect,
            new_flags,
            VirtAddr::from(fragment.start()),
        )
        .ok_or(AxError::BadState)?;
        let post_range = PageRange::new(
            run.start.as_usize(),
            run.end.sub_addr(run.start),
            PAGE_SIZE_4K,
        )
        .map_err(mm_error)?;
        if !post_range.contains(fragment) {
            return Err(AxError::BadState);
        }
        // This is the only `None` proof accepted by the UFFD planner: the
        // complete source remains in one VMA with exactly its old boundaries.
        // Access may change, but it is not registration/fault authority.
        if fragment == registration.range() && post_range == current.range() {
            return Ok(None);
        }

        let identity = mapping_identity(mapping_identities, run.left_area.lineage())?;
        let post = MappingSnapshot::new(
            address_space_id,
            identity.id,
            identity.generation,
            post_range,
            MappingAccess::new(
                run.flags.contains(MappingFlags::READ),
                run.flags.contains(MappingFlags::WRITE),
                run.flags.contains(MappingFlags::EXECUTE),
            ),
            run.left_area.backend().linux_mapping_kind(),
            false,
            false,
        );
        if post.address_space() != registration.address_space()
            || post.mapping() != registration.mapping()
        {
            return Err(AxError::BadState);
        }
        Ok(Some(post))
    }

    fn uffd_snapshot_for_registration(
        address_space_id: AddressSpaceId,
        areas: &MemorySet<Backend>,
        mapping_identities: &MappingIdentityIndex,
        registration: UffdRegistration,
    ) -> AxResult<MappingSnapshot> {
        let start = VirtAddr::from(registration.range().start());
        let area = areas.find(start).ok_or(AxError::BadState)?;
        let snapshot =
            Self::mapping_snapshot_from_parts(address_space_id, mapping_identities, area)?;
        if snapshot.address_space() != registration.address_space()
            || snapshot.mapping() != registration.mapping()
            || !snapshot.range().contains(registration.range())
        {
            return Err(AxError::BadState);
        }
        Ok(snapshot)
    }

    /// Appends every mapped VMA intersecting a userfaultfd ioctl range.
    ///
    /// Holes are deliberately skipped. The caller owns a fixed-capacity
    /// scratch vector prepared before this address-space lock was acquired;
    /// this scan never grows it. Compatibility and registration ownership are
    /// validated by the Linux-MM policy layer after the complete scan.
    pub(super) fn append_uffd_mapping_snapshots(
        &self,
        range: PageRange,
        snapshots: &mut Vec<MappingSnapshot>,
    ) -> AxResult {
        let scan_range = uffd_vma_scan_range(range, self.base(), self.end())?;
        for area in self.areas.iter_overlapping(scan_range) {
            validate_uffd_missing_backend_granule(area.backend().page_size())?;
            if snapshots.len() == snapshots.capacity() {
                return Err(AxError::NoMemory);
            }
            snapshots.push(self.mapping_snapshot(area)?);
        }
        if snapshots.is_empty() {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    fn topology_snapshot(&self) -> AxResult<MappingSnapshot> {
        let range =
            PageRange::new(self.base().as_usize(), self.size(), PAGE_SIZE_4K).map_err(mm_error)?;
        Ok(MappingSnapshot::new(
            self.address_space_id,
            self.topology_mapping_id,
            self.topology_generation,
            range,
            MappingAccess::new(true, true, true),
            MappingKind::Special,
            false,
            false,
        ))
    }

    fn invalidation(
        &self,
        start: VirtAddr,
        size: usize,
        reason: InvalidationReason,
    ) -> AxResult<InvalidationRange> {
        InvalidationRange::from_raw(self.topology_snapshot()?, start.as_usize(), size, reason)
            .map_err(mm_error)
    }

    fn next_topology_generation(&self) -> AxResult<MappingGeneration> {
        self.topology_generation.next().map_err(mm_error)
    }

    fn commit_topology_generation(&mut self, next: MappingGeneration) {
        self.topology_generation = next;
    }

    pub fn current_mapping_bytes(&self) -> usize {
        self.areas.iter().map(MemoryArea::size).sum()
    }

    pub fn resident_user_bytes(&self) -> usize {
        self.areas
            .iter()
            .filter(|area| area.flags().contains(MappingFlags::USER))
            .map(|area| {
                let page_size = area.backend().page_size() as usize;
                let mut resident_bytes = 0usize;
                let mut cursor = area.start();
                while cursor < area.end() {
                    let step = page_size.min(area.end().sub_addr(cursor));
                    if self.pt.query(cursor).is_ok() {
                        resident_bytes = resident_bytes.saturating_add(step);
                    }
                    cursor += page_size;
                }
                resident_bytes
            })
            .sum()
    }

    pub fn lock_current_mappings(&mut self) {
        let ranges: Vec<_> = self
            .areas
            .iter()
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in ranges {
            self.insert_locked_range(start, end);
        }
    }

    pub fn set_lock_future_mappings(&mut self, enabled: bool, on_fault: bool) {
        self.lock_future_mappings = enabled;
        self.lock_future_on_fault = enabled && on_fault;
    }

    pub fn locks_future_mappings(&self) -> bool {
        self.lock_future_mappings
    }

    pub fn locks_future_mappings_on_fault(&self) -> bool {
        self.lock_future_on_fault
    }

    pub fn clear_locked_mappings(&mut self) {
        self.locked_ranges.clear();
        self.lock_future_mappings = false;
        self.lock_future_on_fault = false;
    }

    fn validate_region(&self, start: VirtAddr, size: usize) -> AxResult {
        if !self.contains_range(start, size) {
            ax_bail!(NoMemory, "address out of range");
        }
        if !start.is_aligned_4k() || !is_aligned_4k(size) {
            ax_bail!(InvalidInput, "address is not aligned");
        }
        Ok(())
    }

    /// Finds a free area that can accommodate the given size.
    ///
    /// The search starts from the given hint address, and the area should be
    /// within the given limit range.
    ///
    /// Returns the start address of the free area. Returns None if no such area
    /// is found.
    pub fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        self.areas.find_free_area(hint, size, limit, align)
    }

    /// Finds a free area for kernel-chosen placement.
    ///
    /// If the caller provides an explicit hint above the base, that hint is
    /// still tried first. Otherwise, or if the explicit hint fails, the search
    /// first tries an append-biased placement near the current high-water mark
    /// before falling back to the full first-fit scan from the address-space
    /// base.
    pub fn find_kernel_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        if hint > limit.start {
            self.find_free_area(hint, size, limit, align)
                .or_else(|| self.areas.find_append_area(size, limit, align))
                .or_else(|| self.find_free_area(limit.start, size, limit, align))
        } else {
            self.areas
                .find_append_area(size, limit, align)
                .or_else(|| self.find_free_area(limit.start, size, limit, align))
        }
    }

    pub fn find_area(&self, vaddr: VirtAddr) -> Option<&MemoryArea<Backend>> {
        self.areas.find(vaddr)
    }

    pub fn brk_growth_collides(&self, start: VirtAddr, end: VirtAddr, heap_base: VirtAddr) -> bool {
        if start >= end {
            return false;
        }

        for area in self.areas.iter() {
            if area.end() <= start {
                continue;
            }
            if area.start() >= end {
                break;
            }

            let is_heap_area = area.start() == heap_base
                && area.backend().is_private_anonymous()
                && area.flags().contains(MappingFlags::USER);
            if !is_heap_area {
                return true;
            }
        }

        false
    }

    /// Add a new linear mapping.
    ///
    /// See [`Backend`] for more details about the mapping backends.
    ///
    /// The `flags` parameter indicates the mapping permissions and attributes.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub fn map_linear(
        &mut self,
        start_vaddr: VirtAddr,
        start_paddr: PhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        self.validate_region(start_vaddr, size)?;
        let next_generation = self.next_topology_generation()?;
        let lineage = self.prepare_fresh_mapping_lineage()?;

        if !start_paddr.is_aligned_4k() {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            ax_bail!(InvalidInput, "address is not aligned");
        }

        let area = MemoryArea::new_with_lineage(
            start_vaddr,
            size,
            flags,
            Backend::new_linear(start_vaddr, start_paddr, size),
            lineage,
        );
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(error.into());
        }
        self.commit_topology_generation(next_generation);
        Ok(())
    }

    pub fn map(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
    ) -> AxResult {
        self.map_with_lock_state(
            start,
            size,
            flags,
            populate,
            backend,
            self.lock_future_mappings,
        )
    }

    pub fn map_with_lock_state(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
    ) -> AxResult {
        self.validate_region(start, size)?;
        let next_generation = self.next_topology_generation()?;
        let lineage = self.prepare_fresh_mapping_lineage()?;

        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(error.into());
        }
        // Population or its rollback may partially change resident state, so
        // publish the new topology generation as soon as the VMA is visible.
        self.commit_topology_generation(next_generation);
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            if let Err(unmap_err) = self.unmap_areas_with_tlb_grace(start, size) {
                warn!(
                    "AddrSpace::map: failed to roll back {start:?}+{size:#x} after populate \
                     error: {unmap_err:?}"
                );
            }
            self.refresh_growdown_starts();
            self.clear_locked_range(start, size);
            // A fail-stop backend can leave the area visible after rollback
            // failure. Keep its sidecar identity in that case; removing it
            // would make later snapshots silently lose their lineage state.
            self.remove_mapping_lineage_if_unused(lineage);
            return Err(err);
        }
        Ok(())
    }

    fn preflight_existing_lineage_tail_uffd(
        &mut self,
        lineage: MappingLineage,
        old_end: VirtAddr,
        new_end: VirtAddr,
    ) -> Result<Option<OptionalUffdPlan>, ExistingLineageMapError> {
        let identity = self
            .mapping_identity(lineage)
            .map_err(ExistingLineageMapError::Preserved)?;
        let Some(state) = self.uffd.as_deref_mut() else {
            return Ok(None);
        };
        state
            .preflight_tail_extension(
                0,
                self.address_space_id,
                identity.id,
                old_end.as_usize(),
                new_end.as_usize(),
            )
            .map(Some)
            .map_err(ExistingLineageMapError::Preserved)
    }

    fn preflight_existing_lineage_head_uffd(
        &mut self,
        lineage: MappingLineage,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> Result<Option<OptionalUffdPlan>, ExistingLineageMapError> {
        let identity = self
            .mapping_identity(lineage)
            .map_err(ExistingLineageMapError::Preserved)?;
        let Some(state) = self.uffd.as_deref_mut() else {
            return Ok(None);
        };
        state
            .preflight_head_extension(
                0,
                self.address_space_id,
                identity.id,
                old_start.as_usize(),
                new_start.as_usize(),
            )
            .map(Some)
            .map_err(ExistingLineageMapError::Preserved)
    }

    fn resolve_existing_lineage_uffd(&mut self, plan: Option<OptionalUffdPlan>, published: bool) {
        let Some(plan) = plan else {
            return;
        };
        let state = self
            .uffd
            .as_deref_mut()
            .expect("armed UFFD lineage-extension plan lost its address-space state");
        if published {
            let wake = state.commit_plan(plan);
            assert!(
                wake.is_empty(),
                "authority-preserving UFFD lineage growth invalidated a live request"
            );
        } else {
            state.abort_plan(plan);
        }
    }

    fn map_with_existing_lineage_transaction(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
        uffd_plan: Option<OptionalUffdPlan>,
    ) -> Result<(), ExistingLineageMapError> {
        if let Err(error) = self.validate_region(start, size) {
            self.resolve_existing_lineage_uffd(uffd_plan, false);
            return Err(ExistingLineageMapError::Preserved(error));
        }
        let next_topology_generation = match self.next_topology_generation() {
            Ok(generation) => generation,
            Err(error) => {
                self.resolve_existing_lineage_uffd(uffd_plan, false);
                return Err(ExistingLineageMapError::Preserved(error));
            }
        };
        // If population rollback fails and the new range remains visible,
        // publishing a conservative generation is mandatory and must not
        // introduce a new fallible step after mutation.
        let next_mapping_generation = match self.mapping_identity(lineage) {
            Ok(identity) => identity.generation.next().map_err(mm_error),
            Err(error) => Err(error),
        };
        let next_mapping_generation = match next_mapping_generation {
            Ok(generation) => generation,
            Err(error) => {
                self.resolve_existing_lineage_uffd(uffd_plan, false);
                return Err(ExistingLineageMapError::Preserved(error));
            }
        };

        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            self.resolve_existing_lineage_uffd(uffd_plan, false);
            return Err(ExistingLineageMapError::Preserved(error.into()));
        }
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            let rollback = self.unmap_areas_with_tlb_grace(start, size);
            self.refresh_growdown_starts();
            match rollback {
                Ok(()) => {
                    self.clear_locked_range(start, size);
                    self.resolve_existing_lineage_uffd(uffd_plan, false);
                    return Err(classify_existing_lineage_population_failure(err, false));
                }
                Err(unmap_err) => {
                    warn!(
                        "AddrSpace::map_with_existing_lineage: failed to roll back \
                         {start:?}+{size:#x} after populate error: {unmap_err:?}"
                    );
                    self.resolve_existing_lineage_uffd(uffd_plan, true);
                    self.commit_mapping_generation(lineage, next_mapping_generation);
                    self.commit_topology_generation(next_topology_generation);
                    return Err(classify_existing_lineage_population_failure(err, true));
                }
            }
        }
        self.resolve_existing_lineage_uffd(uffd_plan, true);
        self.commit_mapping_generation(lineage, next_mapping_generation);
        self.commit_topology_generation(next_topology_generation);
        Ok(())
    }

    /// Extends an already identified logical mapping without extending any
    /// userfaultfd registration. `brk` uses this deliberately: a VMA/backend
    /// merge must not grant the new heap tail old range authority.
    pub(crate) fn map_with_existing_lineage(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        self.map_with_existing_lineage_transaction(
            start, size, flags, populate, backend, locked, lineage, None,
        )
    }

    pub(crate) fn extend_mapping_tail_with_existing_lineage(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        let new_end = start
            .checked_add(size)
            .ok_or(ExistingLineageMapError::Preserved(AxError::InvalidInput))?;
        let uffd_plan = self.preflight_existing_lineage_tail_uffd(lineage, start, new_end)?;
        self.map_with_existing_lineage_transaction(
            start, size, flags, populate, backend, locked, lineage, uffd_plan,
        )
    }

    fn extend_mapping_head_with_existing_lineage(
        &mut self,
        old_start: VirtAddr,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        let uffd_plan = self.preflight_existing_lineage_head_uffd(lineage, old_start, start)?;
        self.map_with_existing_lineage_transaction(
            start, size, flags, false, backend, locked, lineage, uffd_plan,
        )
    }

    /// Stages one fragment under an already reserved lineage without
    /// publishing topology or generation state. The caller must hold the
    /// address-space lock and finish through one of the transaction helpers
    /// below, which owns both commit and rollback.
    pub(crate) fn stage_mapping_fragment(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> AxResult {
        self.validate_region(start, size)?;
        self.mapping_identity(lineage)?;
        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        self.areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)?;
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate {
            // Leave the fragment visible on failure. The enclosing transaction
            // owns rollback for the complete destination, including any prefix
            // populated before the error.
            self.populate_area(start, size, flags)?;
        }
        Ok(())
    }

    fn preflight_transaction_unmap(&self, start: VirtAddr, size: usize) -> AxResult {
        self.areas
            .preflight_unmap(start, size, &self.pt)
            .map_err(Into::into)
    }

    fn unmap_areas_with_tlb_grace(&mut self, start: VirtAddr, size: usize) -> MappingResult {
        let retirement =
            self.areas
                .unmap_deferred_with_limit(start, size, &mut self.pt, MAX_VMA_FRAGMENTS)?;
        let grace = super::synchronize_tlb();
        retirement.release();
        drop(grace);
        Ok(())
    }

    fn clear_areas_with_tlb_grace(&mut self) -> MappingResult {
        let retirement = self.areas.clear_deferred(&mut self.pt)?;
        let grace = super::synchronize_tlb();
        retirement.release();
        drop(grace);
        Ok(())
    }

    fn rollback_staged_mapping(
        &mut self,
        start: VirtAddr,
        size: usize,
        lineage: MappingLineage,
    ) -> AxResult {
        if !range_is_owned_by_lineage(&self.areas, lineage, start, size) {
            // Do not retire a sidecar while an out-of-transaction fragment is
            // visible, or unmap a pre-existing mapping that the transaction
            // never owned. The enclosing transaction publishes its prepared
            // topology fence before returning this fail-closed result.
            return Err(AxError::BadState);
        }
        let rollback = self.unmap_areas_with_tlb_grace(start, size);
        if let Err(error) = rollback {
            // The fresh mapping incarnation remains visible. Its generation 1
            // sidecar remains valid; the caller must publish the topology
            // fence before exposing the rollback failure.
            return Err(error.into());
        }
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        self.clear_locked_range(start, size);
        if !self.remove_mapping_lineage_if_unused(lineage) {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    fn destroy_remap_destination(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.unmap_areas_with_tlb_grace(start, size)?;
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        self.clear_locked_range(start, size);
        Ok(())
    }

    fn preflight_remap_uffd(
        &mut self,
        kind: UffdRemapKind,
        fixed: bool,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
    ) -> AxResult<PreparedRemapUffd> {
        let source =
            PageRange::new(source_start.as_usize(), source_size, PAGE_SIZE_4K).map_err(mm_error)?;
        let destination =
            PageRange::new(destination_start.as_usize(), destination_size, PAGE_SIZE_4K)
                .map_err(mm_error)?;
        let AddrSpace {
            address_space_id,
            areas,
            mapping_identities,
            uffd,
            ..
        } = self;
        let Some(state) = uffd.as_deref_mut() else {
            return Ok(PreparedRemapUffd::None);
        };
        let address_space_id = *address_space_id;
        state.preflight_remap(kind, fixed, source, destination, |registration| {
            Self::uffd_snapshot_for_registration(
                address_space_id,
                areas,
                mapping_identities,
                registration,
            )
        })
    }

    fn resolve_remap_uffd(
        &mut self,
        prepared: PreparedRemapUffd,
        outcome: RemapUffdOutcome,
    ) -> DeferredUffdWake {
        if let Some(state) = self.uffd.as_deref_mut() {
            state.resolve_remap(prepared, outcome)
        } else {
            assert!(
                matches!(prepared, PreparedRemapUffd::None),
                "armed UFFD remap plan lost its address-space state"
            );
            DeferredUffdWake::empty()
        }
    }

    fn finish_failed_mapping_transaction(
        &mut self,
        operation_error: AxError,
        destination_start: VirtAddr,
        destination_size: usize,
        destination_lineage: MappingLineage,
        destination_mutations: &[MappingIdentityMutation],
        destination_changed: bool,
        next_topology_generation: MappingGeneration,
        uffd_plan: PreparedRemapUffd,
        wake: &mut DeferredUffdWake,
    ) -> MappingTransactionFailure {
        let rollback =
            self.rollback_staged_mapping(destination_start, destination_size, destination_lineage);
        let rollback_error = rollback.err();
        let effect = classify_failed_remap_effect(destination_changed, rollback_error.is_some());
        wake.merge(self.resolve_remap_uffd(
            uffd_plan,
            match effect {
                RemapTransactionEffect::Preserved => RemapUffdOutcome::Preserved,
                RemapTransactionEffect::Destructive => RemapUffdOutcome::DestructiveFailure,
            },
        ));
        if destination_changed {
            commit_mapping_identity_mutations(&mut self.mapping_identities, destination_mutations);
        }
        if effect == RemapTransactionEffect::Destructive {
            self.commit_topology_generation(next_topology_generation);
        }
        MappingTransactionFailure {
            error: rollback_error.unwrap_or(operation_error),
            effect,
        }
    }

    fn duplicate_mapping_transaction<T>(
        &mut self,
        destination: RemapDestination,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        fragment_limit: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, MappingTransactionFailure> {
        let mut wake = DeferredUffdWake::empty();
        let outcome = (|| {
            self.validate_region(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            self.validate_region(destination_start, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            if staged_fragments == 0 {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            let source_range = VirtAddrRange::from_start_size(source_start, source_size);
            let destination_range =
                VirtAddrRange::from_start_size(destination_start, destination_size);
            if source_range.overlaps(destination_range) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if !range_is_fully_mapped(&self.areas, source_start, source_size) {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }

            let replacing = matches!(destination, RemapDestination::Replace);
            if !replacing && !range_is_empty(&self.areas, destination_start, destination_size) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if replacing {
                self.check_no_user_io_pin_overlap(
                    destination_start,
                    destination_size,
                    InvalidationReason::Remap,
                )
                .map_err(MappingTransactionFailure::preserved)?;
            }

            let destination_mutations = if replacing {
                prepare_unmap_mapping_mutations(
                    &self.areas,
                    &self.mapping_identities,
                    destination_start,
                    destination_size,
                )
                .map_err(MappingTransactionFailure::preserved)?
            } else {
                Vec::new()
            };
            let policy = self
                .prepare_remap_policy(source_start, source_size, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_unmaps = [destination_range];
            let unmaps = if replacing {
                destination_unmaps.as_slice()
            } else {
                &[]
            };
            admit_staged_fragments_after_unmaps(
                &self.areas,
                &unmaps,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if replacing {
                self.preflight_transaction_unmap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
            }
            let next_topology_generation = self
                .next_topology_generation()
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_lineage = self
                .prepare_fresh_mapping_lineage()
                .map_err(MappingTransactionFailure::preserved)?;
            let uffd_plan = match self.preflight_remap_uffd(
                UffdRemapKind::Duplicate,
                replacing,
                source_start,
                source_size,
                destination_start,
                destination_size,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                    debug_assert!(removed);
                    return Err(MappingTransactionFailure::preserved(error));
                }
            };

            let destination_changed = !destination_mutations.is_empty();
            if replacing
                && let Err(error) =
                    self.destroy_remap_destination(destination_start, destination_size)
            {
                let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                debug_assert!(removed);
                wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(MappingTransactionFailure::preserved(error));
            }

            let staged = stage(self, destination_lineage);
            match staged {
                Ok(value)
                    if lineage_exactly_covers_range(
                        &self.areas,
                        destination_lineage,
                        destination_start,
                        destination_size,
                    ) =>
                {
                    self.apply_remap_policy(destination_start, &policy);
                    wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed));
                    commit_mapping_identity_mutations(
                        &mut self.mapping_identities,
                        &destination_mutations,
                    );
                    self.commit_topology_generation(next_topology_generation);
                    Ok(value)
                }
                Ok(_) => Err(self.finish_failed_mapping_transaction(
                    AxError::BadState,
                    destination_start,
                    destination_size,
                    destination_lineage,
                    &destination_mutations,
                    destination_changed,
                    next_topology_generation,
                    uffd_plan,
                    &mut wake,
                )),
                Err(operation_error) => Err(self.finish_failed_mapping_transaction(
                    operation_error,
                    destination_start,
                    destination_size,
                    destination_lineage,
                    &destination_mutations,
                    destination_changed,
                    next_topology_generation,
                    uffd_plan,
                    &mut wake,
                )),
            }
        })();
        LockExternalUffdOutcome::new(outcome, wake)
    }

    fn move_mapping_transaction<T>(
        &mut self,
        destination: RemapDestination,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        fragment_limit: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, MappingTransactionFailure> {
        let mut wake = DeferredUffdWake::empty();
        let outcome = (|| {
            self.validate_region(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            self.validate_region(destination_start, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            if staged_fragments == 0 {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            self.check_no_user_io_pin_overlap(source_start, source_size, InvalidationReason::Remap)
                .map_err(MappingTransactionFailure::preserved)?;
            let source_range = VirtAddrRange::from_start_size(source_start, source_size);
            let destination_range =
                VirtAddrRange::from_start_size(destination_start, destination_size);
            if source_range.overlaps(destination_range) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }

            let replacing = matches!(destination, RemapDestination::Replace);
            if !replacing && !range_is_empty(&self.areas, destination_start, destination_size) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if replacing {
                self.check_no_user_io_pin_overlap(
                    destination_start,
                    destination_size,
                    InvalidationReason::Remap,
                )
                .map_err(MappingTransactionFailure::preserved)?;
            }

            if !range_is_fully_mapped(&self.areas, source_start, source_size) {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }
            let destination_mutations = if replacing {
                prepare_unmap_mapping_mutations(
                    &self.areas,
                    &self.mapping_identities,
                    destination_start,
                    destination_size,
                )
                .map_err(MappingTransactionFailure::preserved)?
            } else {
                Vec::new()
            };
            let replacement_success_ranges = [destination_range, source_range];
            let source_success_ranges = [source_range];
            let success_ranges = if replacing {
                replacement_success_ranges.as_slice()
            } else {
                source_success_ranges.as_slice()
            };
            let success_mutations = prepare_unmap_mapping_mutations_for_ranges(
                &self.areas,
                &self.mapping_identities,
                success_ranges,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if success_mutations.is_empty() {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }

            let policy = self
                .prepare_remap_policy(source_start, source_size, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let replacement_destination_unmaps = [destination_range];
            let destination_unmaps = if replacing {
                replacement_destination_unmaps.as_slice()
            } else {
                &[]
            };
            admit_staged_fragments_after_unmaps(
                &self.areas,
                &destination_unmaps,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            admit_staged_fragments_after_unmaps(
                &self.areas,
                success_ranges,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if replacing {
                self.preflight_transaction_unmap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
            }
            self.preflight_transaction_unmap(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let next_topology_generation = self
                .next_topology_generation()
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_lineage = self
                .prepare_fresh_mapping_lineage()
                .map_err(MappingTransactionFailure::preserved)?;
            let uffd_plan = match self.preflight_remap_uffd(
                UffdRemapKind::Move,
                replacing,
                source_start,
                source_size,
                destination_start,
                destination_size,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                    debug_assert!(removed);
                    return Err(MappingTransactionFailure::preserved(error));
                }
            };

            let destination_changed = !destination_mutations.is_empty();
            if replacing
                && let Err(error) =
                    self.destroy_remap_destination(destination_start, destination_size)
            {
                let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                debug_assert!(removed);
                wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(MappingTransactionFailure::preserved(error));
            }

            let staged = stage(self, destination_lineage);
            let operation: AxResult<T> = match staged {
                Ok(value)
                    if lineage_exactly_covers_range(
                        &self.areas,
                        destination_lineage,
                        destination_start,
                        destination_size,
                    ) =>
                {
                    self.apply_remap_policy(destination_start, &policy);
                    let source_commit = self.unmap_areas_with_tlb_grace(source_start, source_size);
                    match source_commit {
                        Ok(()) => {
                            self.refresh_growdown_starts();
                            Self::clear_interval(
                                &mut self.wipe_on_fork_ranges,
                                source_start,
                                source_size,
                            );
                            Self::clear_interval(
                                &mut self.dontfork_ranges,
                                source_start,
                                source_size,
                            );
                            self.clear_locked_range(source_start, source_size);
                            wake.merge(
                                self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed),
                            );
                            commit_mapping_identity_mutations(
                                &mut self.mapping_identities,
                                &success_mutations,
                            );
                            self.commit_topology_generation(next_topology_generation);
                            return Ok(value);
                        }
                        Err(error) => Err(error.into()),
                    }
                }
                Ok(_) => Err(AxError::BadState),
                Err(error) => Err(error),
            };

            Err(self.finish_failed_mapping_transaction(
                operation
                    .err()
                    .expect("failed remap operation lost its error"),
                destination_start,
                destination_size,
                destination_lineage,
                &destination_mutations,
                destination_changed,
                next_topology_generation,
                uffd_plan,
                &mut wake,
            ))
        })();
        LockExternalUffdOutcome::new(outcome, wake)
    }

    pub(crate) fn duplicate_mapping_into_empty_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, AxError> {
        self.duplicate_mapping_transaction(
            RemapDestination::Empty,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(|failure| failure.error)
    }

    pub(crate) fn replace_and_duplicate_mapping_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, ReplaceMappingError> {
        self.duplicate_mapping_transaction(
            RemapDestination::Replace,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(MappingTransactionFailure::into_replace_error)
    }

    pub(crate) fn move_mapping_into_empty_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, AxError> {
        self.move_mapping_transaction(
            RemapDestination::Empty,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(|failure| failure.error)
    }

    pub(crate) fn replace_and_move_mapping_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, ReplaceMappingError> {
        self.move_mapping_transaction(
            RemapDestination::Replace,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(MappingTransactionFailure::into_replace_error)
    }

    /// Populates the area with physical frames, returning false if the area
    /// contains unmapped area.
    pub fn populate_area(
        &mut self,
        mut start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> AxResult {
        self.validate_region(start, size)?;
        let end = start + size;

        while let Some(area) = self.areas.find(start) {
            let area_end = area.end();
            let range = VirtAddrRange::new(start, area_end.min(end));
            let area_flags = area.flags();
            let outcome =
                area.backend()
                    .populate(range, area_flags, access_flags, &mut self.pt.cursor());
            let result = outcome.finish(self);
            // A backend may have published a valid executable prefix before a
            // later page fails, so synchronize before propagating the error.
            synchronize_executable_publication(area_flags);
            result?;
            start = area_end;
            if !start.is_aligned_4k() {
                return Err(AxError::BadAddress);
            }
            if start >= end {
                break;
            }
        }

        if start < end {
            // If the area is not fully mapped, we return ENOMEM.
            ax_bail!(NoMemory);
        }

        Ok(())
    }

    pub fn discard_pages(&mut self, mut start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Discard)?;
        let next_generation = self.next_topology_generation()?;
        // Backend discard can make partial progress before reporting a later
        // hole or backend error. Advance the legacy address-space admission
        // fence first, but deliberately keep per-lineage VMA generations
        // stable: residency is not a mapping-contract change.
        self.commit_topology_generation(next_generation);
        let end = start + size;

        let retirement_capacity = self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .count();
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(retirement_capacity)
            .map_err(|_| AxError::NoMemory)?;
        let result = {
            let mut modify = self.pt.cursor();
            (|| {
                while let Some(area) = self.areas.find(start) {
                    if area.start() > start {
                        break;
                    }

                    let range = VirtAddrRange::new(start, area.end().min(end));
                    retired.push(area.backend().unmap(range, &mut modify)?);
                    start = range.end;
                    if start >= end {
                        break;
                    }
                }

                if start < end {
                    ax_bail!(NoMemory);
                }

                Ok(())
            })()
        };
        super::retire_after_tlb_grace(retired);
        result
    }

    /// Drops resident private anonymous pages while keeping the VMA layout.
    pub fn discard_private_anonymous_pages(&mut self) {
        let ranges = self
            .areas
            .iter()
            .filter(|area| area.backend().is_private_anonymous())
            .map(|area| (area.start(), area.size()))
            .collect::<Vec<_>>();

        for (start, size) in ranges {
            if let Err(err) = self.discard_pages(start, size) {
                warn!("AddrSpace::discard_private_anonymous_pages: {start:?}+{size:#x}: {err:?}");
            }
        }
    }

    pub fn sync_backends_in_range(
        &self,
        mut start: VirtAddr,
        size: usize,
        fail_on_first_unmapped: bool,
    ) -> AxResult<(Vec<Backend>, bool)> {
        self.validate_region(start, size)?;
        let end = start + size;
        let mut backends = Vec::new();
        let mut saw_unmapped = false;

        for area in self.areas.iter() {
            if area.end() <= start {
                continue;
            }
            if area.start() >= end {
                break;
            }
            if area.start() > start {
                if fail_on_first_unmapped {
                    ax_bail!(NoMemory);
                }
                saw_unmapped = true;
            }
            backends.push(area.backend().clone());
            start = area.end().min(end);
            if start >= end {
                break;
            }
        }

        if start < end {
            if fail_on_first_unmapped {
                ax_bail!(NoMemory);
            }
            saw_unmapped = true;
        }

        Ok((backends, saw_unmapped))
    }

    /// Removes mappings within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub(crate) fn unmap(&mut self, start: VirtAddr, size: usize) -> AxResult<DeferredUffdWake> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(DeferredUffdWake::empty());
        }
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
        let next_generation = self.next_topology_generation()?;
        let mapping_mutations =
            prepare_unmap_mapping_mutations(&self.areas, &self.mapping_identities, start, size)?;
        let unmap_range = PageRange::new(start.as_usize(), size, PAGE_SIZE_4K).map_err(mm_error)?;
        let uffd_plan = {
            let AddrSpace {
                address_space_id,
                areas,
                mapping_identities,
                uffd,
                ..
            } = self;
            let address_space_id = *address_space_id;
            if let Some(state) = uffd.as_deref_mut() {
                match state.preflight_unmap(0, unmap_range, |registration| {
                    Self::uffd_snapshot_for_registration(
                        address_space_id,
                        areas,
                        mapping_identities,
                        registration,
                    )
                })? {
                    OptionalUffdPlan::Noop => None,
                    plan @ OptionalUffdPlan::Armed(_) => Some(plan),
                }
            } else {
                None
            }
        };

        if let Err(error) = self.unmap_areas_with_tlb_grace(start, size) {
            if let Some(plan) = uffd_plan {
                self.uffd
                    .as_mut()
                    .expect("armed UFFD unmap plan lost its address-space state")
                    .abort_plan(plan);
            }
            return Err(error.into());
        }
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        self.clear_locked_range(start, size);
        let wake = if let Some(plan) = uffd_plan {
            self.uffd
                .as_mut()
                .expect("armed UFFD unmap plan lost its address-space state")
                .commit_plan(plan)
        } else {
            DeferredUffdWake::empty()
        };
        commit_mapping_identity_mutations(&mut self.mapping_identities, &mapping_mutations);
        self.commit_topology_generation(next_generation);
        Ok(wake)
    }

    /// To process data in this area with the given function.
    ///
    /// Now it supports reading and writing data in the given interval.
    fn process_area_data<F>(&self, start: VirtAddr, size: usize, mut f: F) -> AxResult
    where
        F: FnMut(VirtAddr, usize, usize),
    {
        if !self.contains_range(start, size) {
            ax_bail!(InvalidInput, "address out of range");
        }
        let mut cnt = 0;
        // If start is aligned to 4K, start_align_down will be equal to start_align_up.
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let end_align_up =
            VirtAddr::from(checked_align_up_4k(end.as_usize()).ok_or(AxError::InvalidInput)?);
        let pages =
            PageIter4K::new(start.align_down_4k(), end_align_up).ok_or(AxError::InvalidInput)?;
        for vaddr in pages {
            let (mut paddr, ..) = self.pt.query(vaddr).map_err(|_| AxError::BadAddress)?;

            let mut copy_size = (size - cnt).min(PAGE_SIZE_4K);

            if copy_size == 0 {
                break;
            }
            if vaddr == start.align_down_4k() && start.align_offset_4k() != 0 {
                let align_offset = start.align_offset_4k();
                copy_size = copy_size.min(PAGE_SIZE_4K - align_offset);
                paddr += align_offset;
            }
            f(phys_to_virt(paddr), cnt, copy_size);
            cnt += copy_size;
        }
        Ok(())
    }

    /// To read data from the address space.
    ///
    /// # Arguments
    ///
    /// * `start` - The start virtual address to read.
    /// * `buf` - The buffer to store the data.
    pub fn read(&self, start: VirtAddr, buf: &mut [u8]) -> AxResult {
        self.process_area_data(start, buf.len(), |src, offset, read_size| unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), buf.as_mut_ptr().add(offset), read_size);
        })
    }

    /// To write data to the address space.
    ///
    /// # Arguments
    ///
    /// * `start_vaddr` - The start virtual address to write.
    /// * `buf` - The buffer to write to the address space.
    pub fn write(&self, start: VirtAddr, buf: &[u8]) -> AxResult {
        let synchronize_instruction_stream = start.checked_add(buf.len()).is_some_and(|end| {
            self.areas.iter().any(|area| {
                area.start() < end
                    && start < area.end()
                    && area.flags().contains(MappingFlags::EXECUTE)
            })
        });
        let result = self.process_area_data(start, buf.len(), |dst, offset, write_size| unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), dst.as_mut_ptr(), write_size);
        });
        // Direct address-space writers include ptrace and process_vm_writev.
        // Synchronize even after partial failure because a prefix may already
        // have changed executable memory.
        if synchronize_instruction_stream {
            synchronize_executable_publication(MappingFlags::EXECUTE);
        }
        result
    }

    /// Updates mapping within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub(crate) fn prepare_protect(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult<PreparedProtect<'_>> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Protect)?;
        self.check_protect_range(start, size, flags)?;
        let next_topology_generation = self.next_topology_generation()?;
        let mapping_mutations = prepare_mapping_generation_advances_for_range(
            &self.areas,
            &self.mapping_identities,
            start,
            size,
        )?;

        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let protect = VirtAddrRange::new(start, end);
        let protect_range =
            PageRange::new(start.as_usize(), size, PAGE_SIZE_4K).map_err(mm_error)?;

        let AddrSpace {
            address_space_id,
            areas,
            mapping_identities,
            growdown_starts,
            uffd,
            topology_generation,
            pt,
            ..
        } = self;
        let address_space_id = *address_space_id;
        let uffd_mutation = if let Some(state) = uffd.as_deref_mut() {
            let plan = state.preflight_protect(0, protect_range, |registration, fragment| {
                Self::projected_uffd_protect_snapshot(
                    address_space_id,
                    areas,
                    mapping_identities,
                    protect,
                    flags,
                    registration,
                    fragment,
                )
            })?;
            match plan {
                OptionalUffdPlan::Noop => None,
                OptionalUffdPlan::Armed(_) => Some(PreparedUffdMutation::new(state, plan)),
            }
        } else {
            None
        };

        let transaction = PreparedAreaProtect {
            areas,
            page_table: pt,
            start,
            end,
            flags,
            max_areas: MAX_VMA_FRAGMENTS,
        };
        let synchronize_instruction_stream = transaction
            .segments()
            .any(|(area, ..)| adds_execute_permission(area.flags(), flags));

        Ok(PreparedProtect {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            mapping_identities,
            mapping_mutations,
            uffd_mutation,
            synchronize_instruction_stream,
        })
    }

    fn check_protect_range(
        &self,
        mut start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;

        while start < end {
            let Some(area) = self.areas.find(start) else {
                ax_bail!(NoMemory);
            };
            if area.start() > start {
                ax_bail!(NoMemory);
            }
            area.backend().check_protect_flags(flags)?;
            start = area.end().min(end);
        }

        Ok(())
    }

    /// Removes all mappings and starts a fresh identity generation for exec.
    pub fn clear(&mut self) -> AxResult {
        if self.user_io_pins.progress().total() != 0 {
            return Err(AxError::ResourceBusy);
        }
        // Image reset is currently used only on a fresh address space. Do not
        // silently recycle an mm which still owns UFFD registrations,
        // terminal results, or waiter credits; that lifecycle needs an
        // explicit lock-external detach receipt.
        if self.uffd.is_some() {
            return Err(AxError::ResourceBusy);
        }
        // Reserve the replacement identity before destroying the current
        // image. A sequence-exhaustion failure therefore leaves it untouched.
        let new_policy = new_user_io_policy()?;
        self.clear_areas_with_tlb_grace()?;
        drop(core::mem::take(&mut self.mapping_identities));
        self.growdown_starts.clear();
        self.wipe_on_fork_ranges.clear();
        self.dontfork_ranges.clear();
        self.locked_ranges.clear();
        self.user_io_pins.begin_teardown().map_err(mm_error)?;
        self.user_io_pins.finish_teardown().map_err(mm_error)?;
        (
            self.address_space_id,
            self.topology_mapping_id,
            self.topology_generation,
            self.user_io_pins,
        ) = new_policy;
        Ok(())
    }

    fn try_handle_growdown_fault(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
        user_sp: Option<VirtAddr>,
    ) -> PageFaultResult {
        let Some(user_sp) = user_sp else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };

        // Linux grows MAP_GROWSDOWN mappings when the fault lands on the guard
        // page immediately below the current lowest mapped page and SP is still
        // within that guard page.
        let Some((current_start, fault_page, page_size, flags, lineage, backend)) = self
            .growdown_starts
            .iter()
            .copied()
            .find_map(|current_start| {
                let area = self.areas.find(current_start)?;
                if area.start() != current_start {
                    return None;
                }
                let page_size = area.backend().page_size();
                let fault_page = vaddr.align_down(page_size);
                if fault_page.checked_add(page_size as usize)? != current_start {
                    return None;
                }
                if !(user_sp >= fault_page && user_sp < current_start) {
                    return None;
                }
                match area.backend() {
                    Backend::Cow(_) => Some((
                        current_start,
                        fault_page,
                        page_size,
                        area.flags(),
                        area.lineage(),
                        area.backend().clone(),
                    )),
                    Backend::Linear(_) | Backend::Shared(_) | Backend::File(_) => None,
                }
            })
        else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };
        if !flags.contains(access_flags) {
            return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
        }

        let Some(gap_start) =
            current_start.checked_sub(page_size as usize * Self::STACK_GUARD_GAP_PAGES)
        else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };
        if self.areas.overlaps(VirtAddrRange::from_start_size(
            gap_start,
            current_start.sub_addr(gap_start),
        )) {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }

        let locked = self.range_is_fully_locked(current_start, page_size as usize);
        if let Err(error) = self.extend_mapping_head_with_existing_lineage(
            current_start,
            fault_page,
            page_size as usize,
            flags,
            backend,
            locked,
            lineage,
        ) {
            if error.published() {
                self.move_growdown_start(current_start, fault_page);
                // Mapping and UFFD sidecar authority are now visible. Return
                // to the user-fault dispatcher instead of populating under
                // this recursive lock-held path; the retried instruction must
                // pass through delegated-fault admission for the new page.
                return PageFaultResult::Handled;
            }
            let err = error.into_error();
            warn!(
                "Failed to extend MAP_GROWSDOWN mapping from {current_start:?} to {fault_page:?}: \
                 {err}"
            );
            return if err.canonicalize() == AxError::NoMemory {
                PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
            } else {
                PageFaultResult::Failed(PageFaultFailure::AddressNotMapped)
            };
        }
        self.move_growdown_start(current_start, fault_page);
        // Growth is one committed transition. A second hardware fault either
        // delegates the inherited UFFD MISSING registration or performs the
        // ordinary population path. `Handled` already means retry the same
        // userspace instruction at the trap boundary.
        PageFaultResult::Handled
    }

    /// Checks whether an access to the specified memory region is valid.
    ///
    /// Returns `true` if the memory region given by `range` is all mapped and
    /// has proper permission flags (i.e. containing `access_flags`).
    pub fn can_access_range(
        &self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> bool {
        let Some(mut range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        for area in self.areas.iter() {
            if area.end() <= range.start {
                continue;
            }
            if area.start() > range.start {
                return false;
            }

            // This area overlaps with the memory region
            if !area.flags().contains(access_flags) {
                return false;
            }

            range.start = area.end();
            if range.is_empty() {
                return true;
            }
        }

        false
    }

    /// Handles a page fault at the given address.
    ///
    /// `access_flags` indicates the access type that caused the page fault.
    ///
    /// Returns the outcome of the page fault handling.
    pub fn handle_page_fault_result(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
        user_sp: Option<VirtAddr>,
    ) -> PageFaultResult {
        if !self.va_range.contains(vaddr) {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }
        if let Some(area) = self.areas.find(vaddr) {
            let flags = area.flags();
            if flags.contains(access_flags) {
                let page_size = area.backend().page_size();
                let start = vaddr.align_down(page_size);
                if area.backend().faults_with_sigbus(start) {
                    return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
                }
                let fault_around = area.backend().fault_around_size(access_flags);
                let fault_around_len = area
                    .end()
                    .sub_addr(start)
                    .min(fault_around.max(page_size as usize));
                let leaf_state = match self.pt.query(vaddr.align_down(PAGE_SIZE_4K)) {
                    Ok((_paddr, page_flags, _page_size))
                        if present_leaf_satisfies_fault(page_flags, access_flags) =>
                    {
                        // A remote resolver/fault may have published this
                        // formerly absent leaf after the hardware cached an
                        // invalid translation. Repair only the fault-receiving
                        // CPU; a global shootdown on every fresh map would put
                        // the wrong ownership and cost on the publisher.
                        super::repair_local_spurious_fault(vaddr);
                        return PageFaultResult::Handled;
                    }
                    Ok(_) => UffdFaultLeafState::Present,
                    Err(PagingError::NotMapped) => UffdFaultLeafState::Missing,
                    Err(_) => {
                        return PageFaultResult::Failed(PageFaultFailure::InternalInconsistency);
                    }
                };
                let len = self.ordinary_fault_prefix_before_uffd(
                    vaddr,
                    start,
                    page_size as usize,
                    fault_around_len,
                    leaf_state,
                );
                if len == 0 {
                    // User-originated registered faults are intercepted by
                    // `FaultSession`; kernel-originated USER_MODE_ONLY faults
                    // are rejected by `handle_page_fault` below. Reaching the
                    // ordinary population path for the registered page would
                    // violate both boundaries, so fail closed.
                    return PageFaultResult::Failed(PageFaultFailure::InternalInconsistency);
                }
                let populate_outcome = area.backend().populate(
                    VirtAddrRange::from_start_size(start, len),
                    flags,
                    access_flags,
                    &mut self.pt.cursor(),
                );
                let populate_result = populate_outcome.finish(self);
                // Synchronize even on error: a multi-page populate may have
                // installed a valid executable prefix before the failure.
                synchronize_executable_publication(flags);
                match populate_result {
                    Ok(n) => {
                        if n == 0 {
                            warn!("No pages populated for {vaddr:?} ({flags:?})");
                        }
                    }
                    Err(err) => {
                        warn!("Failed to populate pages for {vaddr:?} ({flags:?}): {err}");
                    }
                }
                return classify_page_population(populate_result);
            }
            return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
        }
        self.try_handle_growdown_fault(vaddr, access_flags, user_sp)
    }

    /// Returns `true` if the page fault is handled successfully (not a real
    /// fault).
    pub fn handle_page_fault(&mut self, vaddr: VirtAddr, access_flags: PageFaultFlags) -> bool {
        if self.blocks_kernel_usercopy_missing(vaddr) {
            return false;
        }
        matches!(
            self.handle_page_fault_result(vaddr, access_flags, None),
            PageFaultResult::Handled
        )
    }

    /// Attempts to clone the current address space into a new one.
    ///
    /// This method creates a new empty address space with the same base and
    /// size, then iterates over all memory areas in the original address
    /// space to copy or share their mappings into the new one.
    pub fn try_clone(&mut self) -> AxResult<Arc<Mutex<Self>>> {
        if self.user_io_pins.progress().total() != 0 {
            return Err(AxError::ResourceBusy);
        }
        if self.fork_fragment_count()? > MAX_VMA_FRAGMENTS {
            return Err(AxError::NoMemory);
        }

        let new_aspace = Arc::new(Mutex::new(Self::new_empty(self.base(), self.size())?));
        let next_topology_generation = self.next_topology_generation()?;
        let new_aspace_clone = new_aspace.clone();
        let wipe_on_fork_ranges = self.wipe_on_fork_ranges.clone();
        let dontfork_ranges = self.dontfork_ranges.clone();

        // Reserve every child identity before clone_map can COW-protect a
        // parent PTE. Fork does not change the parent's VMA contract, so its
        // per-lineage generation remains stable; the legacy topology
        // generation below remains the conservative PTE/COW admission fence.
        let mut child_parent_lineages = Vec::new();
        child_parent_lineages
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            let mut cursor = area.start();
            let mut has_child_segment = false;
            while cursor < area.end() {
                if let Some(dontfork_end) = Self::interval_end_covering(&dontfork_ranges, cursor) {
                    cursor = dontfork_end.min(area.end());
                } else {
                    has_child_segment = true;
                    break;
                }
            }
            if has_child_segment {
                child_parent_lineages.push(area.lineage());
            }
        }
        child_parent_lineages.sort_unstable();
        child_parent_lineages.dedup();

        let mut guard = new_aspace.lock();
        guard.growdown_starts = self.growdown_starts.clone();
        let mut child_lineages = Vec::new();
        child_lineages
            .try_reserve(child_parent_lineages.len())
            .map_err(|_| AxError::NoMemory)?;
        for parent_lineage in child_parent_lineages {
            let child_lineage = guard.prepare_fresh_mapping_lineage()?;
            child_lineages.push((parent_lineage, child_lineage));
        }

        self.commit_topology_generation(next_topology_generation);

        let mut self_modify = self.pt.cursor();
        for area in self.areas.iter() {
            let child_lineage = child_lineages
                .binary_search_by_key(&area.lineage(), |(parent, _)| *parent)
                .ok()
                .map(|index| child_lineages[index].1);
            let page_size = area.backend().page_size();
            let mut cursor = area.start();
            while cursor < area.end() {
                if let Some(dontfork_end) = Self::interval_end_covering(&dontfork_ranges, cursor) {
                    cursor = dontfork_end.min(area.end());
                    continue;
                }

                if let Some(wipe_end) = Self::interval_end_covering(&wipe_on_fork_ranges, cursor) {
                    let segment_end = wipe_end.min(area.end());
                    let wipe_size = segment_end.sub_addr(cursor);
                    debug_assert!(page_size.is_aligned(wipe_size));
                    let new_area = MemoryArea::new_with_lineage(
                        cursor,
                        wipe_size,
                        area.flags(),
                        Backend::new_alloc(cursor, page_size),
                        child_lineage.expect("included parent lineage was not prepared"),
                    );
                    let aspace = guard.deref_mut();
                    aspace.areas.map_with_limit(
                        new_area,
                        &mut aspace.pt,
                        false,
                        MAX_VMA_FRAGMENTS,
                    )?;
                    Self::insert_interval(&mut aspace.wipe_on_fork_ranges, cursor, segment_end);
                    cursor = segment_end;
                    continue;
                }

                let mut segment_end = area.end();
                if let Some(next_start) =
                    Self::next_interval_start(&dontfork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_start);
                }
                if let Some(next_start) =
                    Self::next_interval_start(&wipe_on_fork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_start);
                }

                if cursor < segment_end {
                    let segment_size = segment_end.sub_addr(cursor);
                    let new_backend = {
                        let mut new_modify = guard.pt.cursor_no_flush();
                        area.backend().clone_map(
                            VirtAddrRange::from_start_size(cursor, segment_size),
                            area.flags(),
                            &mut self_modify,
                            &mut new_modify,
                            &new_aspace_clone,
                        )?
                    };
                    // Fork keeps the segment at the same virtual address. In
                    // particular, a suffix after MADV_DONTFORK must retain the
                    // original backend origin and file-offset relation.
                    let new_area = MemoryArea::new_with_lineage(
                        cursor,
                        segment_size,
                        area.flags(),
                        new_backend,
                        child_lineage.expect("included parent lineage was not prepared"),
                    );
                    let aspace = guard.deref_mut();
                    aspace.areas.map_with_limit(
                        new_area,
                        &mut aspace.pt,
                        false,
                        MAX_VMA_FRAGMENTS,
                    )?;
                    if Self::interval_overlaps(&wipe_on_fork_ranges, cursor, segment_end) {
                        Self::insert_interval(&mut aspace.wipe_on_fork_ranges, cursor, segment_end);
                    }
                    cursor = segment_end;
                } else {
                    cursor += page_size as usize;
                }
            }
        }
        guard.refresh_growdown_starts();
        debug_assert!(
            guard.areas.iter().all(|area| mapping_identity(
                &guard.mapping_identities,
                area.lineage()
            )
            .is_ok())
        );
        drop(self_modify);
        drop(super::synchronize_tlb());
        drop(guard);

        Ok(new_aspace)
    }

    /// Returns an iterator over the memory areas.
    ///
    /// This is required for `procfs` to generate `/proc/pid/maps`.
    /// Exposing internal state for system introspection is a standard practice.
    pub fn areas(&self) -> impl Iterator<Item = &MemoryArea<Backend>> {
        self.areas.iter()
    }

    /// Returns only VMAs intersecting `range`, starting from the crossing
    /// predecessor instead of walking the complete address-space prefix.
    pub(crate) fn areas_overlapping(
        &self,
        range: VirtAddrRange,
    ) -> impl Iterator<Item = &MemoryArea<Backend>> {
        self.areas.iter_overlapping(range)
    }
}

impl fmt::Debug for AddrSpace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AddrSpace")
            .field("va_range", &self.va_range)
            .field("page_table_root", &self.pt.root_paddr())
            .field("hardware_asid", &self.hardware_asid.asid())
            .field("hardware_asid_generation", &self.hardware_asid.generation())
            .field("areas", &self.areas)
            .finish()
    }
}

impl Drop for AddrSpace {
    fn drop(&mut self) {
        debug_assert_eq!(self.user_io_pins.progress().total(), 0);
        if let Err(err) = self.clear_areas_with_tlb_grace() {
            warn!("AddrSpace::drop: failed to unmap all areas: {err:?}");
        }
        let _ = self.user_io_pins.begin_teardown();
        let _ = self.user_io_pins.finish_teardown();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::Cell;

    use thekernel_linux_mm::{MappingKind, UFFD_API, UffdApiState, UffdRegisterMode};

    use super::*;
    use crate::mm::{UffdAddressSpaceState, UffdPollSet};

    const TEST_SPACE_SIZE: usize = 0x6000;

    fn mock_lineage(raw: u64) -> MappingLineage {
        MappingLineage::new(raw).unwrap()
    }

    fn mock_identity(raw: u64, generation: u64) -> MappingIdentityEntry {
        MappingIdentityEntry {
            lineage: mock_lineage(raw),
            state: MappingIdentityState {
                id: MappingId::new(raw).unwrap(),
                generation: MappingGeneration::new(generation).unwrap(),
            },
        }
    }

    fn mock_identities(
        entries: impl IntoIterator<Item = MappingIdentityEntry>,
    ) -> MappingIdentityIndex {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut identities = MappingIdentityIndex::new();
        identities.states.reserve(entries.len());
        for entry in entries {
            identities
                .insert_reserved(entry.lineage, entry.state)
                .unwrap();
        }
        identities
    }

    fn map_mock_area(
        areas: &mut MemorySet<MockBackend>,
        page_table: &mut Vec<u8>,
        start: usize,
        size: usize,
        flags: u8,
        backend: u8,
        lineage: MappingLineage,
    ) {
        areas
            .map(
                MemoryArea::new_with_lineage(
                    VirtAddr::from(start),
                    size,
                    flags,
                    MockBackend(backend),
                    lineage,
                ),
                page_table,
                false,
            )
            .unwrap();
    }

    fn initialized_uffd_api() -> UffdApiState {
        let mut api = UffdApiState::new();
        let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
        api.commit(negotiation).unwrap();
        api
    }

    fn uffd_test_snapshot(start: usize, size: usize) -> MappingSnapshot {
        MappingSnapshot::from_raw(
            1,
            2,
            17,
            start,
            size,
            PAGE_SIZE_4K,
            MappingAccess::new(true, true, false).bits(),
            MappingKind::AnonymousPrivate,
            true,
            false,
        )
        .unwrap()
    }

    fn uffd_test_fragment(mapping: MappingSnapshot, range: PageRange) -> MappingSnapshot {
        MappingSnapshot::new(
            mapping.address_space(),
            mapping.mapping(),
            mapping.generation(),
            range,
            mapping.access(),
            mapping.kind(),
            mapping.long_term_pinnable(),
            mapping.writable_file_pin_supported(),
        )
    }

    #[test]
    fn uffd_vma_scan_clamps_a_low_leading_hole_to_the_address_space() {
        let range = PageRange::new(0, 0x4000, PAGE_SIZE_4K).unwrap();
        let scan = uffd_vma_scan_range(
            range,
            VirtAddr::from(0x1000),
            VirtAddr::from(TEST_SPACE_SIZE),
        )
        .unwrap();
        assert_eq!(scan.start, VirtAddr::from(0x1000));
        assert_eq!(scan.end, VirtAddr::from(0x4000));

        let wholly_below = PageRange::new(0, 0x1000, PAGE_SIZE_4K).unwrap();
        assert_eq!(
            uffd_vma_scan_range(
                wholly_below,
                VirtAddr::from(0x1000),
                VirtAddr::from(TEST_SPACE_SIZE),
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn first_uffd_profile_rejects_huge_backend_granules() {
        assert_eq!(
            validate_uffd_missing_backend_granule(PageSize::Size4K),
            Ok(())
        );
        assert_eq!(
            validate_uffd_missing_backend_granule(PageSize::Size2M),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_uffd_missing_backend_granule(PageSize::Size1G),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn failed_remap_effect_marks_visible_destination_residue_as_changed() {
        assert_eq!(
            classify_failed_remap_effect(false, false),
            RemapTransactionEffect::Preserved
        );
        for (destination_changed, rollback_failed) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                classify_failed_remap_effect(destination_changed, rollback_failed),
                RemapTransactionEffect::Destructive
            );
        }
    }

    #[test]
    fn existing_lineage_population_failure_reports_visible_residue_as_published() {
        let preserved = classify_existing_lineage_population_failure(AxError::NoMemory, false);
        assert!(!preserved.published());
        assert_eq!(preserved.into_error(), AxError::NoMemory);

        let published = classify_existing_lineage_population_failure(AxError::NoMemory, true);
        assert!(published.published());
        assert_eq!(published.into_error(), AxError::NoMemory);
    }

    #[test]
    fn topology_and_area_mapping_ids_share_one_namespace() {
        let (_, topology_mapping_id, topology_generation, _) = new_user_io_policy().unwrap();
        let (lineage, area_identity) = allocate_mapping_identity().unwrap();
        let (_, next_topology_mapping_id, ..) = new_user_io_policy().unwrap();

        assert_ne!(topology_mapping_id, area_identity.id);
        assert_ne!(area_identity.id, next_topology_mapping_id);
        assert_ne!(topology_mapping_id, next_topology_mapping_id);
        assert_eq!(lineage.get(), area_identity.id.get());
        assert_eq!(topology_generation.get(), 1);
        assert_eq!(area_identity.generation.get(), 1);
    }

    #[test]
    fn fork_preparation_keeps_parent_identity_stable_and_allocates_child_identity() {
        let parent = mock_identity(u64::MAX - 1, 17);
        let parent_before = parent;
        let (child_lineage, child) = allocate_mapping_identity().unwrap();

        assert_eq!(parent, parent_before);
        assert_ne!(child.id, parent.state.id);
        assert_ne!(child_lineage, parent.lineage);
        assert_eq!(child.generation.get(), 1);
    }

    #[test]
    fn advancing_one_lineage_does_not_stale_an_unrelated_mapping() {
        let lineage_a = mock_lineage(2);
        let lineage_b = mock_lineage(3);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage_a);
        map_mock_area(&mut areas, &mut page_table, 0x3000, 0x1000, 1, 2, lineage_b);
        let mut identities = mock_identities([mock_identity(2, 7), mock_identity(3, 11)]);
        let before_a = mapping_identity(&identities, lineage_a).unwrap();

        let mutations = prepare_mapping_generation_advances_for_range(
            &areas,
            &identities,
            VirtAddr::from(0x3000),
            0x1000,
        )
        .unwrap();
        assert_eq!(
            mutations,
            vec![MappingIdentityMutation::Advance {
                lineage: lineage_b,
                generation: MappingGeneration::new(12).unwrap(),
            }]
        );
        commit_mapping_identity_mutations(&mut identities, &mutations);

        assert_eq!(mapping_identity(&identities, lineage_a).unwrap(), before_a);
        assert_eq!(
            mapping_identity(&identities, lineage_b)
                .unwrap()
                .generation
                .get(),
            12
        );
    }

    #[test]
    fn resident_only_discard_keeps_mapping_identity_and_generation() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
        let identities = mock_identities([mock_identity(2, 9)]);
        let before = mapping_identity(&identities, lineage).unwrap();

        // Model MADV_DONTNEED's residency-only PTE teardown: the VMA and its
        // sidecar are deliberately untouched.
        page_table[0x1000..0x2000].fill(0);

        assert_eq!(mapping_identity(&identities, lineage).unwrap(), before);
        assert_eq!(
            areas.find(VirtAddr::from(0x1000)).unwrap().lineage(),
            lineage
        );
    }

    #[test]
    fn protect_split_keeps_lineage_and_advances_once() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x3000, 1, 1, lineage);
        let mut identities = mock_identities([mock_identity(2, 1)]);
        let mutations = prepare_mapping_generation_advances_for_range(
            &areas,
            &identities,
            VirtAddr::from(0x2000),
            0x1000,
        )
        .unwrap();

        areas
            .protect(VirtAddr::from(0x2000), 0x1000, |_| Some(3), &mut page_table)
            .unwrap();
        commit_mapping_identity_mutations(&mut identities, &mutations);

        assert_eq!(areas.len(), 3);
        assert!(areas.iter().all(|area| area.lineage() == lineage));
        assert_eq!(
            mapping_identity(&identities, lineage)
                .unwrap()
                .generation
                .get(),
            2
        );
    }

    #[test]
    fn full_unmap_retires_max_generation_but_partial_unmap_is_preflight_rejected() {
        let lineage = mock_lineage(2);
        let mut full_areas = MemorySet::new();
        let mut full_page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut full_areas,
            &mut full_page_table,
            0x1000,
            0x3000,
            1,
            1,
            lineage,
        );
        let mut full_identities = mock_identities([mock_identity(2, u64::MAX)]);
        let retire = prepare_unmap_mapping_mutations(
            &full_areas,
            &full_identities,
            VirtAddr::from(0x1000),
            0x3000,
        )
        .unwrap();
        assert_eq!(retire, vec![MappingIdentityMutation::Retire { lineage }]);
        full_areas
            .unmap(VirtAddr::from(0x1000), 0x3000, &mut full_page_table)
            .unwrap();
        commit_mapping_identity_mutations(&mut full_identities, &retire);
        assert!(full_areas.is_empty());
        assert!(full_identities.is_empty());

        let mut partial_areas = MemorySet::new();
        let mut partial_page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut partial_areas,
            &mut partial_page_table,
            0x1000,
            0x3000,
            1,
            1,
            lineage,
        );
        let partial_identities = mock_identities([mock_identity(2, u64::MAX)]);
        let before_areas = area_snapshot(&partial_areas);
        let before_page_table = partial_page_table.clone();
        assert_eq!(
            prepare_unmap_mapping_mutations(
                &partial_areas,
                &partial_identities,
                VirtAddr::from(0x2000),
                0x1000,
            ),
            Err(AxError::ResourceBusy)
        );
        assert_eq!(area_snapshot(&partial_areas), before_areas);
        assert_eq!(partial_page_table, before_page_table);
        assert_eq!(
            mapping_identity(&partial_identities, lineage).unwrap(),
            mock_identity(2, u64::MAX).state
        );
    }

    #[test]
    fn unmap_plan_is_deduplicated_and_validates_retired_sidecars_before_mutation() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
        map_mock_area(&mut areas, &mut page_table, 0x3000, 0x1000, 3, 1, lineage);
        let identities = mock_identities([mock_identity(2, 4)]);
        let mutations =
            prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x3000)
                .unwrap();
        assert_eq!(mutations, vec![MappingIdentityMutation::Retire { lineage }]);

        let before_areas = area_snapshot(&areas);
        let before_page_table = page_table.clone();
        let no_identities = MappingIdentityIndex::new();
        assert_eq!(
            prepare_unmap_mapping_mutations(&areas, &no_identities, VirtAddr::from(0x1000), 0x3000,),
            Err(AxError::BadState)
        );
        assert_eq!(area_snapshot(&areas), before_areas);
        assert_eq!(page_table, before_page_table);
    }

    #[test]
    fn unmap_plan_advances_a_lineage_that_survives_in_an_unaffected_fragment() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
        map_mock_area(&mut areas, &mut page_table, 0x4000, 0x1000, 3, 1, lineage);
        let identities = mock_identities([mock_identity(2, 8)]);

        let mutations = prepare_unmap_mapping_mutations_for_ranges(
            &areas,
            &identities,
            &[
                VirtAddrRange::new(VirtAddr::from(0x1800), VirtAddr::from(0x2000)),
                VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x1900)),
            ],
        )
        .unwrap();

        assert_eq!(
            mutations,
            [MappingIdentityMutation::Advance {
                lineage,
                generation: MappingGeneration::new(9).unwrap(),
            }]
        );
    }

    #[test]
    fn multi_range_planner_handles_thousands_of_vmas_without_nested_scans() {
        const VMA_COUNT: usize = 2_048;
        const STRIDE: usize = 0x2000;
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; (VMA_COUNT + 1) * STRIDE];
        let mut identities = MappingIdentityIndex::new();
        identities.states.reserve(VMA_COUNT);
        let mut ranges = Vec::new();
        ranges.reserve(VMA_COUNT / 64);

        for index in 0..VMA_COUNT {
            let start = index * STRIDE;
            let lineage = mock_lineage(index as u64 + 2);
            map_mock_area(
                &mut areas,
                &mut page_table,
                start,
                PAGE_SIZE_4K,
                1,
                1,
                lineage,
            );
            identities
                .insert_reserved(lineage, mock_identity(index as u64 + 2, 1).state)
                .unwrap();
            if index.is_multiple_of(64) {
                ranges.push(VirtAddrRange::from_start_size(
                    VirtAddr::from(start),
                    PAGE_SIZE_4K,
                ));
            }
        }
        ranges.reverse();

        let mutations =
            prepare_unmap_mapping_mutations_for_ranges(&areas, &identities, &ranges).unwrap();
        assert_eq!(mutations.len(), VMA_COUNT / 64);
        assert!(
            mutations
                .iter()
                .all(|mutation| matches!(mutation, MappingIdentityMutation::Retire { .. }))
        );
    }

    #[test]
    fn zero_length_plans_are_identity_noops() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
        let no_identities = MappingIdentityIndex::new();
        assert!(
            prepare_mapping_generation_advances_for_range(
                &areas,
                &no_identities,
                VirtAddr::from(0x1000),
                0,
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            prepare_unmap_mapping_mutations(&areas, &no_identities, VirtAddr::from(0x1000), 0,)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn move_uses_fresh_destination_identity_and_rollback_preserves_source() {
        let source = mock_lineage(2);
        let destination = mock_lineage(3);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, source);
        let source_state = mock_identity(2, 6);
        let mut identities = mock_identities([source_state, mock_identity(3, 1)]);
        let source_retire =
            prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x1000)
                .unwrap();
        map_mock_area(
            &mut areas,
            &mut page_table,
            0x3000,
            0x1000,
            1,
            1,
            destination,
        );
        areas
            .unmap(VirtAddr::from(0x1000), 0x1000, &mut page_table)
            .unwrap();
        commit_mapping_identity_mutations(&mut identities, &source_retire);
        assert!(mapping_identity(&identities, source).is_err());
        assert_eq!(
            mapping_identity(&identities, destination)
                .unwrap()
                .generation
                .get(),
            1
        );

        let mut rollback_areas = MemorySet::new();
        let mut rollback_page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut rollback_areas,
            &mut rollback_page_table,
            0x1000,
            0x1000,
            1,
            1,
            source,
        );
        map_mock_area(
            &mut rollback_areas,
            &mut rollback_page_table,
            0x3000,
            0x1000,
            1,
            1,
            destination,
        );
        let mut rollback_identities = mock_identities([source_state, mock_identity(3, 1)]);
        let destination_retire = prepare_unmap_mapping_mutations(
            &rollback_areas,
            &rollback_identities,
            VirtAddr::from(0x3000),
            0x1000,
        )
        .unwrap();
        rollback_areas
            .unmap(VirtAddr::from(0x3000), 0x1000, &mut rollback_page_table)
            .unwrap();
        commit_mapping_identity_mutations(&mut rollback_identities, &destination_retire);
        assert_eq!(
            mapping_identity(&rollback_identities, source).unwrap(),
            source_state.state
        );
        assert!(mapping_identity(&rollback_identities, destination).is_err());

        // old_size == 0 duplication keeps the source and installs one fresh
        // generation-1 destination incarnation.
        let duplicate_identities = mock_identities([source_state, mock_identity(3, 1)]);
        assert_eq!(
            mapping_identity(&duplicate_identities, source).unwrap(),
            source_state.state
        );
        assert_eq!(
            mapping_identity(&duplicate_identities, destination)
                .unwrap()
                .generation
                .get(),
            1
        );
    }

    #[test]
    fn staged_coverage_mismatch_rolls_back_and_out_of_range_lineage_is_rejected() {
        let lineage = mock_lineage(2);
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
        let mut identities = mock_identities([mock_identity(2, 1)]);
        assert!(!lineage_exactly_covers_range(
            &areas,
            lineage,
            VirtAddr::from(0x1000),
            0x2000,
        ));
        assert!(lineage_is_contained_in_range(
            &areas,
            lineage,
            VirtAddr::from(0x1000),
            0x2000,
        ));
        let rollback =
            prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x2000)
                .unwrap();
        areas
            .unmap(VirtAddr::from(0x1000), 0x2000, &mut page_table)
            .unwrap();
        commit_mapping_identity_mutations(&mut identities, &rollback);
        assert!(areas.is_empty());
        assert!(identities.is_empty());

        let mut outside = MemorySet::new();
        let mut outside_page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut outside,
            &mut outside_page_table,
            0x3000,
            0x1000,
            1,
            1,
            lineage,
        );
        assert!(!lineage_is_contained_in_range(
            &outside,
            lineage,
            VirtAddr::from(0x1000),
            0x2000,
        ));
        assert!(!lineage_exactly_covers_range(
            &outside,
            lineage,
            VirtAddr::from(0x1000),
            0x2000,
        ));
    }

    #[test]
    fn mapping_lineage_limit_is_admitted_before_growth_and_exec_releases_capacity() {
        let mut identities = mock_identities([mock_identity(2, 1), mock_identity(3, 1)]);
        assert_eq!(
            reserve_mapping_identity_slot(&mut identities, 2),
            Err(AxError::NoMemory)
        );
        assert_eq!(identities.len(), 2);

        identities.states.reserve(32);
        assert!(identities.capacity() > 2);
        drop(core::mem::take(&mut identities));
        assert_eq!(identities.len(), 0);
        assert_eq!(identities.capacity(), 0);
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct MockBackend(u8);

    impl memory_set::MappingBackend for MockBackend {
        type Addr = VirtAddr;
        type Flags = u8;
        type PageTable = Vec<u8>;

        fn map(
            &self,
            start: VirtAddr,
            size: usize,
            flags: u8,
            page_table: &mut Self::PageTable,
        ) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].iter().any(|entry| *entry != 0) {
                return false;
            }
            page_table[range].fill(flags);
            true
        }

        fn unmap(&self, start: VirtAddr, size: usize, page_table: &mut Self::PageTable) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].contains(&0) {
                return false;
            }
            page_table[range].fill(0);
            true
        }

        fn protect(
            &self,
            start: VirtAddr,
            size: usize,
            new_flags: u8,
            page_table: &mut Self::PageTable,
        ) -> bool {
            let range = start.as_usize()..start.as_usize() + size;
            if page_table[range.clone()].contains(&0) {
                return false;
            }
            page_table[range].fill(new_flags);
            true
        }

        fn can_merge(&self, other: &Self) -> bool {
            self == other || matches!((self.0, other.0), (10, 11) | (11, 10) | (11, 12) | (12, 11))
        }
    }

    fn area_snapshot(set: &MemorySet<MockBackend>) -> Vec<(usize, usize, u8, u8)> {
        set.iter()
            .map(|area| {
                (
                    area.start().as_usize(),
                    area.end().as_usize(),
                    area.flags(),
                    area.backend().0,
                )
            })
            .collect()
    }

    fn expect_mapping_error<T>(result: MappingResult<T>) -> memory_set::MappingError {
        match result {
            Ok(value) => {
                drop(value);
                panic!("mapping transaction unexpectedly committed")
            }
            Err(error) => error,
        }
    }

    #[test]
    fn prepared_area_failure_aborts_uffd_and_success_commits_after_mm() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut areas,
            &mut page_table,
            0x1000,
            0x3000,
            1,
            1,
            mock_lineage(2),
        );

        let mut uffd = *UffdAddressSpaceState::try_new_boxed().unwrap();
        let handler = uffd.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
        let mapping = uffd_test_snapshot(0x1000, 0x3000);
        let mut current = [mapping];
        uffd.register_range(
            &initialized_uffd_api(),
            handler,
            mapping.range(),
            UffdRegisterMode::MISSING,
            &mut current,
        )
        .unwrap();
        let before_registrations: Vec<_> = uffd.registrations.iter().collect();
        let before_areas = area_snapshot(&areas);
        let before_page_table = page_table.clone();

        let plan = uffd
            .preflight_protect(
                0,
                PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap(),
                |_, fragment| Ok(Some(uffd_test_fragment(mapping, fragment))),
            )
            .unwrap();
        let synchronized = Cell::new(false);
        let error = expect_mapping_error(commit_area_before_sidecar(
            PreparedAreaProtect {
                areas: &mut areas,
                page_table: &mut page_table,
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 3,
                max_areas: 1,
            },
            PreparedUffdMutation::new(&mut uffd, plan),
            || synchronized.set(true),
        ));
        assert_eq!(error, memory_set::MappingError::NoMemory);
        assert!(synchronized.get());
        assert_eq!(area_snapshot(&areas), before_areas);
        assert_eq!(page_table, before_page_table);
        assert_eq!(
            uffd.registrations.iter().collect::<Vec<_>>(),
            before_registrations
        );

        // A second admission proves that the failed main-MM transaction's
        // RAII drop released the bounded UFFD plan slot. The helper below is
        // the same coordinator used by PreparedProtect::commit. Host axhal's
        // dummy address translation cannot safely instantiate a real
        // AddrSpace PageTable; RV/LA runtime gates cover that final wiring.
        let plan = uffd
            .preflight_protect(
                0,
                PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap(),
                |_, fragment| Ok(Some(uffd_test_fragment(mapping, fragment))),
            )
            .unwrap();
        synchronized.set(false);
        let (committed_areas, mutation) = commit_area_before_sidecar(
            PreparedAreaProtect {
                areas: &mut areas,
                page_table: &mut page_table,
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 3,
                max_areas: usize::MAX,
            },
            PreparedUffdMutation::new(&mut uffd, plan),
            || synchronized.set(true),
        )
        .unwrap();
        assert!(synchronized.get());
        assert_eq!(
            area_snapshot(committed_areas),
            vec![
                (0x1000, 0x2000, 1, 1),
                (0x2000, 0x3000, 3, 1),
                (0x3000, 0x4000, 1, 1),
            ]
        );
        assert!(page_table[0x2000..0x3000].iter().all(|entry| *entry == 3));
        mutation.commit().finish();

        let mut registrations: Vec<_> = uffd.registrations.iter().collect();
        registrations.sort_by_key(|registration| registration.range().start());
        assert_eq!(registrations.len(), 3);
        assert_eq!(
            registrations[0].range(),
            PageRange::new(0x1000, 0x1000, PAGE_SIZE_4K).unwrap()
        );
        assert_eq!(
            registrations[1].range(),
            PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap()
        );
        assert_eq!(
            registrations[2].range(),
            PageRange::new(0x3000, 0x1000, PAGE_SIZE_4K).unwrap()
        );
        assert!(registrations.iter().all(|registration| {
            registration.mapping() == mapping.mapping()
                && registration.generation() == mapping.generation()
        }));
    }

    #[test]
    fn prepared_protect_exposes_all_segments_and_drop_aborts() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        areas
            .map(
                MemoryArea::new_with_lineage(
                    VirtAddr::from(0x1000),
                    0x1000,
                    1,
                    MockBackend(1),
                    mock_lineage(2),
                ),
                &mut page_table,
                false,
            )
            .unwrap();
        areas
            .map(
                MemoryArea::new_with_lineage(
                    VirtAddr::from(0x2000),
                    0x1000,
                    3,
                    MockBackend(2),
                    mock_lineage(3),
                ),
                &mut page_table,
                false,
            )
            .unwrap();
        let before_areas = area_snapshot(&areas);
        let before_page_table = page_table.clone();

        let plan = PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x1800),
            end: VirtAddr::from(0x2800),
            flags: 5,
            max_areas: usize::MAX,
        };
        let segments: Vec<_> = plan
            .segments()
            .map(|(area, affected_start, affected_end)| {
                (
                    area.start().as_usize(),
                    area.end().as_usize(),
                    affected_start.as_usize(),
                    affected_end.as_usize(),
                    area.flags(),
                    area.backend().0,
                )
            })
            .collect();
        assert_eq!(
            segments,
            vec![
                (0x1000, 0x2000, 0x1800, 0x2000, 1, 1),
                (0x2000, 0x3000, 0x2000, 0x2800, 3, 2),
            ]
        );

        // A future policy hook may reject after inspecting every segment.
        drop(plan);
        assert_eq!(area_snapshot(&areas), before_areas);
        assert_eq!(page_table, before_page_table);
    }

    #[test]
    fn prepared_protect_commit_splits_and_remerges_areas() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        areas
            .map(
                MemoryArea::new_with_lineage(
                    VirtAddr::from(0x1000),
                    0x3000,
                    1,
                    MockBackend(1),
                    mock_lineage(2),
                ),
                &mut page_table,
                false,
            )
            .unwrap();

        let protect = VirtAddrRange::new(VirtAddr::from(0x2000), VirtAddr::from(0x3000));
        for (address, expected_start, expected_end, expected_flags) in [
            (0x1000, 0x1000, 0x2000, 1),
            (0x2000, 0x2000, 0x3000, 3),
            (0x3000, 0x3000, 0x4000, 1),
        ] {
            let projected =
                AddrSpace::projected_protect_run_at(&areas, protect, 3, VirtAddr::from(address))
                    .unwrap();
            assert_eq!(projected.start.as_usize(), expected_start);
            assert_eq!(projected.end.as_usize(), expected_end);
            assert_eq!(projected.flags, expected_flags);
        }

        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            flags: 3,
            max_areas: usize::MAX,
        }
        .commit()
        .unwrap();
        assert_eq!(
            area_snapshot(&areas),
            vec![
                (0x1000, 0x2000, 1, 1),
                (0x2000, 0x3000, 3, 1),
                (0x3000, 0x4000, 1, 1),
            ]
        );
        assert!(page_table[0x1000..0x2000].iter().all(|entry| *entry == 1));
        assert!(page_table[0x2000..0x3000].iter().all(|entry| *entry == 3));
        assert!(page_table[0x3000..0x4000].iter().all(|entry| *entry == 1));

        for address in [0x1000, 0x2000, 0x3000] {
            let projected =
                AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(address))
                    .unwrap();
            assert_eq!(projected.start.as_usize(), 0x1000);
            assert_eq!(projected.end.as_usize(), 0x4000);
            assert_eq!(projected.flags, 1);
        }

        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            flags: 1,
            max_areas: usize::MAX,
        }
        .commit()
        .unwrap();
        assert_eq!(area_snapshot(&areas), vec![(0x1000, 0x4000, 1, 1)]);
        assert!(page_table[0x1000..0x4000].iter().all(|entry| *entry == 1));
    }

    #[test]
    fn projected_protect_run_respects_backend_and_lineage_barriers() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        map_mock_area(
            &mut areas,
            &mut page_table,
            0x1000,
            0x1000,
            1,
            1,
            mock_lineage(2),
        );
        map_mock_area(
            &mut areas,
            &mut page_table,
            0x2000,
            0x1000,
            3,
            2,
            mock_lineage(2),
        );
        map_mock_area(
            &mut areas,
            &mut page_table,
            0x3000,
            0x1000,
            1,
            2,
            mock_lineage(3),
        );

        let projected = AddrSpace::projected_protect_run_at(
            &areas,
            VirtAddrRange::new(VirtAddr::from(0x2000), VirtAddr::from(0x3000)),
            1,
            VirtAddr::from(0x2000),
        )
        .unwrap();
        assert_eq!(projected.start.as_usize(), 0x2000);
        assert_eq!(projected.end.as_usize(), 0x3000);
        assert_eq!(projected.flags, 1);
    }

    #[test]
    fn projected_protect_run_respects_holes_and_flag_barriers() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        for (start, flags) in [(0x1000, 1), (0x3000, 1), (0x4000, 2)] {
            map_mock_area(
                &mut areas,
                &mut page_table,
                start,
                0x1000,
                flags,
                1,
                mock_lineage(2),
            );
        }

        let protect = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x2000));
        let left = AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(0x1000))
            .unwrap();
        assert_eq!(left.start.as_usize(), 0x1000);
        assert_eq!(left.end.as_usize(), 0x2000);

        let right = AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(0x3000))
            .unwrap();
        assert_eq!(right.start.as_usize(), 0x3000);
        assert_eq!(right.end.as_usize(), 0x4000);
        assert_eq!(right.flags, 1);
    }

    #[test]
    fn projected_protect_run_keeps_memory_set_left_backend_across_right_merges() {
        let mut areas = MemorySet::new();
        let mut page_table = vec![0; TEST_SPACE_SIZE];
        for (start, flags, backend) in [(0x1000, 1, 10), (0x2000, 2, 11), (0x3000, 3, 12)] {
            map_mock_area(
                &mut areas,
                &mut page_table,
                start,
                0x1000,
                flags,
                backend,
                mock_lineage(2),
            );
        }

        let protect = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x4000));
        for (address, start, end, backend) in [
            (0x1000, 0x1000, 0x3000, 10),
            (0x2000, 0x1000, 0x3000, 10),
            (0x3000, 0x3000, 0x4000, 12),
        ] {
            let projected =
                AddrSpace::projected_protect_run_at(&areas, protect, 9, VirtAddr::from(address))
                    .unwrap();
            assert_eq!(projected.start.as_usize(), start);
            assert_eq!(projected.end.as_usize(), end);
            assert_eq!(projected.left_area.backend().0, backend);
        }

        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x1000),
            end: VirtAddr::from(0x4000),
            flags: 9,
            max_areas: usize::MAX,
        }
        .commit()
        .unwrap();
        assert_eq!(
            area_snapshot(&areas),
            vec![(0x1000, 0x3000, 9, 10), (0x3000, 0x4000, 9, 12)]
        );
    }

    #[test]
    fn projected_protect_run_matches_memory_set_for_nontransitive_backends() {
        let boundaries = [0x1000, 0x2000, 0x3000, 0x4000];
        for left_flags in 1..=3 {
            for middle_flags in 1..=3 {
                for right_flags in 1..=3 {
                    for protect_start in 0..3 {
                        for protect_end in (protect_start + 1)..=3 {
                            for new_flags in 1..=3 {
                                let mut areas = MemorySet::new();
                                let mut page_table = vec![0; TEST_SPACE_SIZE];
                                for (start, flags, backend) in [
                                    (0x1000, left_flags, 10),
                                    (0x2000, middle_flags, 11),
                                    (0x3000, right_flags, 12),
                                ] {
                                    map_mock_area(
                                        &mut areas,
                                        &mut page_table,
                                        start,
                                        0x1000,
                                        flags,
                                        backend,
                                        mock_lineage(2),
                                    );
                                }

                                let protect = VirtAddrRange::new(
                                    VirtAddr::from(boundaries[protect_start]),
                                    VirtAddr::from(boundaries[protect_end]),
                                );
                                let projected = [0x1000, 0x2000, 0x3000].map(|address| {
                                    let run = AddrSpace::projected_protect_run_at(
                                        &areas,
                                        protect,
                                        new_flags,
                                        VirtAddr::from(address),
                                    )
                                    .unwrap();
                                    (
                                        run.start.as_usize(),
                                        run.end.as_usize(),
                                        run.flags,
                                        run.left_area.backend().0,
                                    )
                                });

                                PreparedAreaProtect {
                                    areas: &mut areas,
                                    page_table: &mut page_table,
                                    start: protect.start,
                                    end: protect.end,
                                    flags: new_flags,
                                    max_areas: usize::MAX,
                                }
                                .commit()
                                .unwrap();
                                for (index, address) in
                                    [0x1000, 0x2000, 0x3000].into_iter().enumerate()
                                {
                                    let area = areas.find(VirtAddr::from(address)).unwrap();
                                    assert_eq!(
                                        projected[index],
                                        (
                                            area.start().as_usize(),
                                            area.end().as_usize(),
                                            area.flags(),
                                            area.backend().0,
                                        ),
                                        "flags={left_flags}/{middle_flags}/{right_flags}, \
                                         protect={protect_start}..{protect_end}, new={new_flags}, \
                                         address={address:#x}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn execute_publication_is_required_only_when_execute_is_added() {
        let read_write = MappingFlags::READ | MappingFlags::WRITE;
        let read_execute = MappingFlags::READ | MappingFlags::EXECUTE;

        assert!(adds_execute_permission(read_write, read_execute));
        assert!(!adds_execute_permission(read_execute, read_execute));
        assert!(!adds_execute_permission(read_execute, read_write));
        assert!(!adds_execute_permission(read_write, read_write));
    }

    #[test]
    fn present_leaf_repairs_only_faults_already_granted_by_the_pte() {
        let read_execute = MappingFlags::READ | MappingFlags::EXECUTE;
        assert!(present_leaf_satisfies_fault(
            read_execute,
            MappingFlags::READ
        ));
        assert!(present_leaf_satisfies_fault(
            read_execute,
            MappingFlags::EXECUTE
        ));
        assert!(!present_leaf_satisfies_fault(
            read_execute,
            MappingFlags::WRITE
        ));
    }

    #[test]
    fn page_population_failure_preserves_vma_failure_causes() {
        assert_eq!(classify_page_population(Ok(1)), PageFaultResult::Handled);
        assert_eq!(
            classify_page_population(Ok(0)),
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        );
        assert_eq!(
            classify_page_population(Err(AxError::NoMemory)),
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        );
        assert_eq!(
            classify_page_population(Err(AxError::BadAddress)),
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        );
        assert_eq!(
            classify_page_population(Err(AxError::Io)),
            PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)
        );
    }
}
