use alloc::{
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
    paging::{MappingFlags, PageTable},
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
    AddressSpaceId, ExpectedMapping, InvalidationRange, InvalidationReason, MappingAccess,
    MappingGeneration, MappingId, MappingKind, MappingSnapshot, MmError, PageRange, PinBudget,
    PinBudgetCharge, PinOwner, PinQuota, PinRegistry, PinRequest, PinReservation, PinToken,
};

use super::checked_align_up_4k;

mod backend;
mod mapping;

pub use self::backend::*;
pub(crate) use self::mapping::{FileLikeMappingLease, FileMappingLease, FileMappingSharing};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultResult {
    Handled,
    SigBus,
    SegmentationFault(SegmentationFaultReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentationFaultReason {
    AddressNotMapped,
    AccessDenied,
}

#[inline]
fn synchronize_executable_publication(flags: MappingFlags) {
    if flags.contains(MappingFlags::EXECUTE) {
        axhal::asm::flush_icache_all();
        drop(super::synchronize_after_local_icache());
    }
}

fn adds_execute_permission(old_flags: MappingFlags, new_flags: MappingFlags) -> bool {
    new_flags.contains(MappingFlags::EXECUTE) && !old_flags.contains(MappingFlags::EXECUTE)
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
    topology_mapping_id: MappingId,
    topology_generation: MappingGeneration,
    areas: MemorySet<Backend>,
    mapping_identities: MappingIdentityIndex,
    growdown_starts: BTreeSet<VirtAddr>,
    wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
    user_io_pins: UserIoPinRegistry,
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
    synchronize_instruction_stream: bool,
}

enum RemapDestination {
    Empty,
    Replace,
}

/// Fixed-remap failure classification used by syscall glue to invalidate
/// per-range policy only when replacement actually destroyed an old mapping.
#[derive(Debug)]
pub(crate) enum ReplaceMappingError {
    DestinationPreserved(AxError),
    DestinationDestroyed(AxError),
}

impl ReplaceMappingError {
    pub(crate) const fn destination_destroyed(&self) -> bool {
        matches!(self, Self::DestinationDestroyed(_))
    }

    pub(crate) fn into_error(self) -> AxError {
        match self {
            Self::DestinationPreserved(error) | Self::DestinationDestroyed(error) => error,
        }
    }
}

#[derive(Debug)]
struct MappingTransactionFailure {
    error: AxError,
    destination_destroyed: bool,
}

impl MappingTransactionFailure {
    fn preserved(error: AxError) -> Self {
        Self {
            error,
            destination_destroyed: false,
        }
    }

    fn into_replace_error(self) -> ReplaceMappingError {
        if self.destination_destroyed {
            ReplaceMappingError::DestinationDestroyed(self.error)
        } else {
            ReplaceMappingError::DestinationPreserved(self.error)
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
    pub(crate) fn commit(self) -> AxResult {
        let Self {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            mapping_identities,
            mapping_mutations,
            synchronize_instruction_stream,
        } = self;
        let areas = transaction.commit();
        if synchronize_instruction_stream {
            axhal::asm::flush_icache_all();
            drop(super::synchronize_after_local_tlb_and_icache());
        } else {
            drop(super::synchronize_after_local_flush());
        }
        let areas = areas?;
        Self::refresh_growdown_starts(areas, growdown_starts);
        commit_mapping_identity_mutations(mapping_identities, &mapping_mutations);
        *topology_generation = next_topology_generation;
        Ok(())
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

    /// Checks if the address space contains the given address range.
    pub fn contains_range(&self, start: VirtAddr, size: usize) -> bool {
        self.va_range.contains(start) && (self.va_range.end - start) >= size
    }

    /// Creates a new empty address space.
    pub fn new_empty(base: VirtAddr, size: usize) -> AxResult<Self> {
        let va_range = VirtAddrRange::try_from_start_size(base, size).ok_or(AxError::NoMemory)?;
        let (address_space_id, topology_mapping_id, topology_generation, user_io_pins) =
            new_user_io_policy()?;
        Ok(Self {
            va_range,
            address_space_id,
            topology_mapping_id,
            topology_generation,
            areas: MemorySet::new(),
            mapping_identities: MappingIdentityIndex::new(),
            growdown_starts: BTreeSet::new(),
            wipe_on_fork_ranges: BTreeMap::new(),
            dontfork_ranges: BTreeMap::new(),
            locked_ranges: BTreeMap::new(),
            user_io_pins,
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
        let flags = area.flags();
        let identity = self.mapping_identity(area.lineage())?;
        let range =
            PageRange::new(area.start().as_usize(), area.size(), PAGE_SIZE_4K).map_err(mm_error)?;
        Ok(MappingSnapshot::new(
            self.address_space_id,
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

    /// Extends an already identified logical mapping.
    ///
    /// A successful extension publishes exactly one lineage generation. Multi-
    /// fragment staging that must defer publication uses
    /// [`Self::stage_mapping_fragment`] inside an explicit transaction instead.
    pub(crate) fn map_with_existing_lineage(
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
        let next_topology_generation = self.next_topology_generation()?;
        // If population rollback fails and the new range remains visible,
        // publishing a conservative generation is mandatory and must not
        // introduce a new fallible step after mutation.
        let next_mapping_generation = self
            .mapping_identity(lineage)?
            .generation
            .next()
            .map_err(mm_error)?;

        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        self.areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)?;
        self.commit_topology_generation(next_topology_generation);
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            if let Err(unmap_err) = self.unmap_areas_with_tlb_grace(start, size) {
                warn!(
                    "AddrSpace::map_with_existing_lineage: failed to roll back \
                     {start:?}+{size:#x} after populate error: {unmap_err:?}"
                );
            }
            self.refresh_growdown_starts();
            self.clear_locked_range(start, size);
            let end = start + size;
            if self
                .areas
                .iter()
                .any(|area| area.lineage() == lineage && area.start() < end && start < area.end())
            {
                self.commit_mapping_generation(lineage, next_mapping_generation);
            }
            return Err(err);
        }
        self.commit_mapping_generation(lineage, next_mapping_generation);
        Ok(())
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
        let grace = super::synchronize_after_local_flush();
        retirement.release();
        drop(grace);
        Ok(())
    }

    fn clear_areas_with_tlb_grace(&mut self) -> MappingResult {
        let retirement = self.areas.clear_deferred(&mut self.pt)?;
        let grace = super::synchronize_after_local_flush();
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

    fn finish_failed_mapping_transaction(
        &mut self,
        operation_error: AxError,
        destination_start: VirtAddr,
        destination_size: usize,
        destination_lineage: MappingLineage,
        destination_mutations: &[MappingIdentityMutation],
        destination_destroyed: bool,
        next_topology_generation: MappingGeneration,
    ) -> MappingTransactionFailure {
        let rollback =
            self.rollback_staged_mapping(destination_start, destination_size, destination_lineage);
        if destination_destroyed {
            commit_mapping_identity_mutations(&mut self.mapping_identities, destination_mutations);
        }
        let rollback_error = rollback.err();
        let destructive_outcome = destination_destroyed || rollback_error.is_some();
        if destructive_outcome {
            self.commit_topology_generation(next_topology_generation);
        }
        MappingTransactionFailure {
            error: rollback_error.unwrap_or(operation_error),
            destination_destroyed: destructive_outcome,
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
    ) -> Result<T, MappingTransactionFailure> {
        self.validate_region(source_start, source_size)
            .map_err(MappingTransactionFailure::preserved)?;
        self.validate_region(destination_start, destination_size)
            .map_err(MappingTransactionFailure::preserved)?;
        if staged_fragments == 0 {
            return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
        }
        let source_range = VirtAddrRange::from_start_size(source_start, source_size);
        let destination_range = VirtAddrRange::from_start_size(destination_start, destination_size);
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
        admit_staged_fragments_after_unmaps(&self.areas, &unmaps, staged_fragments, fragment_limit)
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

        let destination_destroyed = !destination_mutations.is_empty();
        if replacing
            && let Err(error) = self.destroy_remap_destination(destination_start, destination_size)
        {
            let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
            debug_assert!(removed);
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
                destination_destroyed,
                next_topology_generation,
            )),
            Err(operation_error) => Err(self.finish_failed_mapping_transaction(
                operation_error,
                destination_start,
                destination_size,
                destination_lineage,
                &destination_mutations,
                destination_destroyed,
                next_topology_generation,
            )),
        }
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
    ) -> Result<T, MappingTransactionFailure> {
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
        let destination_range = VirtAddrRange::from_start_size(destination_start, destination_size);
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

        let destination_destroyed = !destination_mutations.is_empty();
        if replacing
            && let Err(error) = self.destroy_remap_destination(destination_start, destination_size)
        {
            let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
            debug_assert!(removed);
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
                        Self::clear_interval(&mut self.dontfork_ranges, source_start, source_size);
                        self.clear_locked_range(source_start, source_size);
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
            destination_destroyed,
            next_topology_generation,
        ))
    }

    pub(crate) fn duplicate_mapping_into_empty_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> AxResult<T> {
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
    ) -> Result<T, ReplaceMappingError> {
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
    ) -> AxResult<T> {
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
    ) -> Result<T, ReplaceMappingError> {
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
        super::retire_after_local_flush(retired);
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
    pub fn unmap(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
        let next_generation = self.next_topology_generation()?;
        let mapping_mutations =
            prepare_unmap_mapping_mutations(&self.areas, &self.mapping_identities, start, size)?;

        self.unmap_areas_with_tlb_grace(start, size)?;
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        self.clear_locked_range(start, size);
        commit_mapping_identity_mutations(&mut self.mapping_identities, &mapping_mutations);
        self.commit_topology_generation(next_generation);
        Ok(())
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

        let transaction = PreparedAreaProtect {
            areas: &mut self.areas,
            page_table: &mut self.pt,
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
            growdown_starts: &mut self.growdown_starts,
            topology_generation: &mut self.topology_generation,
            next_topology_generation,
            mapping_identities: &mut self.mapping_identities,
            mapping_mutations,
            synchronize_instruction_stream,
        })
    }

    pub fn protect(&mut self, start: VirtAddr, size: usize, flags: MappingFlags) -> AxResult {
        self.prepare_protect(start, size, flags)?.commit()
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
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        };

        // Linux grows MAP_GROWSDOWN mappings when the fault lands on the guard
        // page immediately below the current lowest mapped page and SP is still
        // within that guard page.
        let Some((current_start, fault_page, page_size, flags, lineage)) = self
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
                if !area.flags().contains(access_flags) {
                    return None;
                }
                match area.backend() {
                    Backend::Cow(_) => Some((
                        current_start,
                        fault_page,
                        page_size,
                        area.flags(),
                        area.lineage(),
                    )),
                    Backend::Linear(_) | Backend::Shared(_) | Backend::File(_) => None,
                }
            })
        else {
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        };

        let Some(gap_start) =
            current_start.checked_sub(page_size as usize * Self::STACK_GUARD_GAP_PAGES)
        else {
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        };
        if self.areas.overlaps(VirtAddrRange::from_start_size(
            gap_start,
            current_start.sub_addr(gap_start),
        )) {
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        }

        let locked = self.range_is_fully_locked(current_start, page_size as usize);
        if let Err(err) = self.map_with_existing_lineage(
            fault_page,
            page_size as usize,
            flags,
            false,
            Backend::new_alloc(fault_page, page_size),
            locked,
            lineage,
        ) {
            warn!(
                "Failed to extend MAP_GROWSDOWN mapping from {current_start:?} to {fault_page:?}: \
                 {err}"
            );
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        }
        self.move_growdown_start(current_start, fault_page);
        self.handle_page_fault_result(vaddr, access_flags, Some(user_sp))
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
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AddressNotMapped);
        }
        if let Some(area) = self.areas.find(vaddr) {
            let flags = area.flags();
            if flags.contains(access_flags) {
                let page_size = area.backend().page_size();
                let start = vaddr.align_down(page_size);
                if area.backend().faults_with_sigbus(start) {
                    return PageFaultResult::SigBus;
                }
                let fault_around = area.backend().fault_around_size(access_flags);
                let len = area
                    .end()
                    .sub_addr(start)
                    .min(fault_around.max(page_size as usize));
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
                return match populate_result {
                    Ok(n) => {
                        if n == 0 {
                            warn!("No pages populated for {vaddr:?} ({flags:?})");
                            PageFaultResult::SegmentationFault(
                                SegmentationFaultReason::AddressNotMapped,
                            )
                        } else {
                            PageFaultResult::Handled
                        }
                    }
                    Err(err) => {
                        warn!("Failed to populate pages for {vaddr:?} ({flags:?}): {err}");
                        PageFaultResult::SegmentationFault(
                            SegmentationFaultReason::AddressNotMapped,
                        )
                    }
                };
            }
            return PageFaultResult::SegmentationFault(SegmentationFaultReason::AccessDenied);
        }
        self.try_handle_growdown_fault(vaddr, access_flags, user_sp)
    }

    /// Returns `true` if the page fault is handled successfully (not a real
    /// fault).
    pub fn handle_page_fault(&mut self, vaddr: VirtAddr, access_flags: PageFaultFlags) -> bool {
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
        drop(super::synchronize_after_local_flush());
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

    use super::*;

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
            self == other
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
    fn execute_publication_is_required_only_when_execute_is_added() {
        let read_write = MappingFlags::READ | MappingFlags::WRITE;
        let read_execute = MappingFlags::READ | MappingFlags::EXECUTE;

        assert!(adds_execute_permission(read_write, read_execute));
        assert!(!adds_execute_permission(read_execute, read_execute));
        assert!(!adds_execute_permission(read_execute, read_write));
        assert!(!adds_execute_permission(read_write, read_write));
    }
}
