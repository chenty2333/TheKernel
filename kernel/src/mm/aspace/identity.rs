//! User I/O pin budgets, mapping identities and lineage range helpers.

use super::*;

pub(super) const USER_IO_PIN_MAX_TOKENS: u64 = 64;
pub(super) const USER_IO_PIN_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const USER_IO_PIN_MAX_PAGES: u64 = USER_IO_PIN_MAX_BYTES / PAGE_SIZE_4K as u64;
/// One PMD-sized anonymous promotion unit on x86_64.
///
/// Keep the eligibility test separate from the eventual page-table
/// transaction.  The latter may allocate and fail; this predicate must be a
/// side-effect-free proof that no VMA sidecar contract is crossed.
pub(crate) const COLLAPSE_2M_SIZE: usize = 2 * 1024 * 1024;

/// Linux MMF_DISABLE_THP state.  It belongs to the address space: separate
/// CLONE_VM process groups must observe the same setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ThpDisableMode {
    #[default]
    Enabled,
    Disabled,
    ExceptAdvised,
}

impl ThpDisableMode {
    pub(crate) const fn prctl_value(self) -> usize {
        match self {
            Self::Enabled => 0,
            Self::Disabled => 1,
            Self::ExceptAdvised => 1 | 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Collapse2MCandidateFacts {
    pub(crate) start: usize,
    pub(crate) length: usize,
    pub(crate) vma_covers_range: bool,
    pub(crate) private_cow: bool,
    /// A write-protect registration changes the permission contract of a
    /// present leaf.  MISSING-only registrations do not: once every source
    /// PTE is resident, promotion can retain their fault-free semantics.
    pub(crate) has_uffd_write_protect: bool,
    pub(crate) has_locked_pages: bool,
    /// This is an exact physical-frame fact, not an address-space-wide pin
    /// count.  An unrelated long-term pin must not prevent promotion.
    pub(crate) has_exact_long_term_cow_pin: bool,
    pub(crate) has_fork_policy: bool,
}

/// Returns whether a single 2 MiB MADV_COLLAPSE unit can be promoted without
/// crossing an address-space policy boundary.
///
/// A page-table implementation still has to prove that its 512 source leaves
/// are present and suitably contiguous.  This deliberately only classifies
/// VMA/sidecar eligibility, so it remains pure and can be checked before any
/// allocation or PTE change.
pub(crate) const fn collapse_2m_candidate_eligible(facts: Collapse2MCandidateFacts) -> bool {
    facts.start & (COLLAPSE_2M_SIZE - 1) == 0
        && facts.length == COLLAPSE_2M_SIZE
        && facts.vma_covers_range
        && facts.private_cow
        && !facts.has_uffd_write_protect
        && !facts.has_locked_pages
        && !facts.has_exact_long_term_cow_pin
        && !facts.has_fork_policy
}
/// Internal live logical-mapping limit. Fragments sharing one lineage count
/// once, so protection splits do not consume additional slots.
pub(super) const MAX_MAPPING_LINEAGES: usize = 65_536;
/// Independent live VMA-fragment limit. One logical lineage may be split by
/// protection, unmap, fork policy, or remap geometry, so bounding only the
/// lineage sidecar does not bound the area tree itself.
pub(super) const MAX_VMA_FRAGMENTS: usize = 65_536;
pub(super) type UserIoPinRegistry = PinRegistry<1, { USER_IO_PIN_MAX_TOKENS as usize }>;
pub(super) type UserIoPinBudget = PinBudget<{ USER_IO_PIN_MAX_TOKENS as usize }>;
pub(super) type UserIoPolicy = (
    AddressSpaceId,
    MappingId,
    MappingGeneration,
    Box<UserIoPinRegistry>,
);

pub(super) static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);
// MappingLineage reserves raw value 1 for compatibility-only untracked areas.
pub(super) static NEXT_MAPPING_ID: AtomicU64 = AtomicU64::new(2);
pub(super) static USER_IO_PIN_BUDGET: SpinNoIrq<Option<UserIoPinBudget>> = SpinNoIrq::new(None);

/// Exact private-COW frames retained by one active long-term writable pin.
///
/// The physical identity, rather than the registration-time VA, is retained:
/// active long-term pins deliberately allow later remap/unmap operations while
/// their lower owner remains live.  Fork can therefore ask whether the frame
/// currently present at a private-COW leaf is owned by this address
/// space's pin, without conservatively copying unrelated globally pinned
/// frames.
pub(super) struct ActiveLongTermCowPin {
    pub(super) token: PinToken,
    pub(super) frames: Vec<PhysAddr>,
}

pub(super) fn allocate_nonwrapping_id(sequence: &AtomicU64) -> AxResult<u64> {
    sequence
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::ResourceBusy)
}

pub(super) fn mm_error(error: MmError) -> AxError {
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

pub(super) fn allocate_mapping_id() -> AxResult<MappingId> {
    MappingId::new(allocate_nonwrapping_id(&NEXT_MAPPING_ID)?).map_err(mm_error)
}

pub(super) fn new_user_io_policy() -> AxResult<UserIoPolicy> {
    let address_space_id =
        AddressSpaceId::new(allocate_nonwrapping_id(&NEXT_ADDRESS_SPACE_ID)?).map_err(mm_error)?;
    let topology_mapping_id = allocate_mapping_id()?;
    let topology_generation = MappingGeneration::new(1).map_err(mm_error)?;
    let pin_quota = PinQuota::new(
        USER_IO_PIN_MAX_PAGES,
        USER_IO_PIN_MAX_BYTES,
        USER_IO_PIN_MAX_TOKENS,
    );
    // Keep the fixed-capacity pin ledger out of `AddrSpace` itself.  The
    // registry is intentionally allocation-free internally, but its 64
    // records are cold policy state and keeping them inline made every
    // address-space value roughly 10 KiB.  Early boot retains one such value
    // across the deep ELF/filesystem loader call chain, which can exhaust the
    // fixed BSP stack.  The one allocation belongs to address-space creation,
    // before the object is published or any pin can exist.
    let mut user_io_pins =
        Box::try_new(UserIoPinRegistry::new(PAGE_SIZE_4K, pin_quota, 1).map_err(mm_error)?)
            .map_err(|_| AxError::NoMemory)?;
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
pub(super) struct MappingIdentityState {
    pub(super) id: MappingId,
    pub(super) generation: MappingGeneration,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MappingIdentityEntry {
    pub(super) lineage: MappingLineage,
    pub(super) state: MappingIdentityState,
}

/// Bounded, fallibly-grown sidecar for logical mapping identities.
///
/// Mapping lineages are kernel-allocated monotonic values, so they are not an
/// attacker-chosen hash input. Keeping them in a hash index avoids the linear
/// compaction that the old ordered `Vec` paid whenever a mapping was retired,
/// while `reserve_slot` preserves the pre-publication allocation boundary.
#[derive(Debug, Default)]
pub(super) struct MappingIdentityIndex {
    pub(super) states: HashMap<MappingLineage, MappingIdentityState>,
}

impl MappingIdentityIndex {
    pub(super) fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.states.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.states.capacity()
    }

    pub(super) fn reserve_slot(&mut self, limit: usize) -> AxResult {
        if self.states.len() >= limit {
            return Err(AxError::NoMemory);
        }
        self.states.try_reserve(1).map_err(|_| AxError::NoMemory)
    }

    /// Publishes one identity after `reserve_slot` has admitted its storage.
    pub(super) fn insert_reserved(
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

    pub(super) fn get(&self, lineage: MappingLineage) -> Option<MappingIdentityState> {
        self.states.get(&lineage).copied()
    }

    pub(super) fn get_mut(&mut self, lineage: MappingLineage) -> Option<&mut MappingIdentityState> {
        self.states.get_mut(&lineage)
    }

    pub(super) fn remove(&mut self, lineage: MappingLineage) -> Option<MappingIdentityState> {
        self.states.remove(&lineage)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MappingIdentityMutation {
    Advance {
        lineage: MappingLineage,
        generation: MappingGeneration,
    },
    Retire {
        lineage: MappingLineage,
    },
}

impl MappingIdentityMutation {
    pub(super) const fn lineage(self) -> MappingLineage {
        match self {
            Self::Advance { lineage, .. } | Self::Retire { lineage } => lineage,
        }
    }
}

pub(super) fn mapping_identity(
    identities: &MappingIdentityIndex,
    lineage: MappingLineage,
) -> AxResult<MappingIdentityState> {
    identities.get(lineage).ok_or(AxError::BadState)
}

/// Derives long-term user-I/O admission from the concrete lower owner that
/// keeps the mapping's pages stable. Device/linear mappings have no such
/// owner. A writable file-backed shared mapping additionally needs an owner
/// that records dirty/writeback state; currently only `FileBackend` provides
/// that contract through `CachedFilePagePin`.
pub(super) fn mapping_user_io_pin_policy(backend: &Backend) -> (bool, bool) {
    match backend {
        Backend::Linear(_) => (false, false),
        Backend::Cow(_) => (true, false),
        // SharedPages has an exact frame owner, but it does not itself carry
        // the dirty/writeback contract required for writable FileShared pins.
        Backend::Shared(_) => (true, false),
        Backend::File(_) => (true, true),
    }
}

pub(super) fn reserve_mapping_identity_slot(
    identities: &mut MappingIdentityIndex,
    limit: usize,
) -> AxResult {
    identities.reserve_slot(limit)
}

pub(super) fn lineage_covers_range<B>(
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

pub(super) fn range_is_fully_mapped<B>(
    areas: &MemorySet<B>,
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
        if area.start() > start {
            return false;
        }
        start = area.end().min(end);
    }
    true
}

/// Recognises the process heap VMA that `brk` owns.
///
/// Linux `SYSCALL_DEFINE1(brk)` never has to name this VMA: the iterator that
/// both the stack-guard-gap probe and `do_brk_flags()` use starts at the old
/// break, so a VMA that ends there is already behind it.  This kernel's heap
/// can also be *entirely* absent (the range was `munmap`ed), which is why the
/// growth path has to look for a mergeable tail separately, and its address
/// space is a flat area list, so the exemption has to be spelled out.  The
/// area is identified by the same three properties the merge path requires:
/// it starts at the heap base, it is private anonymous storage, and it belongs
/// to user space.
pub(super) fn is_brk_heap_area(area: &MemoryArea<Backend>, heap_base: VirtAddr) -> bool {
    area.start() == heap_base
        && area.backend().is_private_anonymous()
        && area.flags().contains(MappingFlags::USER)
}

/// Linux `mm/vma.h:is_data_mapping_vma_flags()` over one installed mapping.
///
/// `VMA_SHARED_BIT` is `VM_SHARED`.  The generic VMA flags of this kernel carry
/// only hardware permission bits, so sharing comes from the backend that owns
/// the pages: anonymous shared storage is `Backend::Shared`, and a file mapping
/// records `FileMappingSharing` on its lease.  A device mapping
/// (`Backend::Linear`) does not record which of `MAP_SHARED`/`MAP_PRIVATE`
/// created it and is treated as shared, so a writable `MAP_PRIVATE` device
/// mapping is the one shape that under-counts.
pub(super) fn is_data_mapping_area(
    flags: MappingFlags,
    backend: &Backend,
    start: VirtAddr,
    growdown_starts: &BTreeSet<VirtAddr>,
) -> bool {
    let shared = match backend {
        Backend::Shared(_) | Backend::Linear(_) => true,
        Backend::Cow(_) | Backend::File(_) => backend
            .file_mapping()
            .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Shared),
    };
    tk_linux_mm::is_data_mapping(
        flags.contains(MappingFlags::WRITE),
        shared,
        growdown_starts.contains(&start),
    )
}

pub(super) fn lineage_is_contained_in_range<B>(
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

pub(super) fn lineage_exactly_covers_range<B>(
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

pub(super) fn range_is_empty<B>(areas: &MemorySet<B>, start: VirtAddr, size: usize) -> bool
where
    B: memory_set::MappingBackend<Addr = VirtAddr>,
{
    let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    !range.is_empty() && !areas.overlaps(range)
}

pub(super) fn range_is_owned_by_lineage<B>(
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

pub(super) fn normalize_ranges(ranges: &[VirtAddrRange]) -> AxResult<Vec<VirtAddrRange>> {
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

pub(super) fn projected_fragment_count_after_unmaps<B>(
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

pub(super) fn admit_staged_fragments_after_unmaps<B>(
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

pub(super) fn prepare_mapping_generation_advances_for_range<B>(
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

pub(super) fn area_is_fully_covered_by_ranges(
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
pub(super) struct UnmapLineageCoverage {
    pub(super) survives: bool,
}

pub(super) fn prepare_unmap_mapping_mutations<B>(
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

pub(super) fn prepare_unmap_mapping_mutations_for_ranges<B>(
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

pub(super) fn commit_mapping_identity_mutations(
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

pub(super) fn allocate_mapping_identity() -> AxResult<(MappingLineage, MappingIdentityState)> {
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
    pub(super) expected: ExpectedMapping,
    pub(super) covered: PageRange,
    pub(super) needs_frame_registry: bool,
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
    pub(super) charge: Option<PinBudgetCharge>,
}

impl UserIoSystemPinCharge {
    pub(super) fn reserve(request: PinRequest) -> AxResult<Self> {
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
