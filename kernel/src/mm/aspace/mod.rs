use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    fmt,
    ops::DerefMut,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering, fence},
};

use axerrno::{AxError, AxResult, ax_bail};
use axhal::{
    mem::phys_to_virt,
    paging::{
        MappingFlags, PageSize, PageTable, PagingError, PagingHandlerImpl, Pkey,
        PrepareTableFramesError, PreparedPageTableFrames,
    },
    trap::PageFaultFlags,
};
use axsync::Mutex;
use hashbrown::{HashMap, hash_map::Entry};
use kernel_guard::NoPreemptIrqSave;
use kspin::SpinNoIrq;
use memory_addr::{
    MemoryAddr, PAGE_SIZE_4K, PageIter4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k,
};
use memory_set::{MappingLineage, MappingResult, MemoryArea, MemorySet};
use page_table_multiarch::{ReplacedPteRun, x86_64::X64PTE};
use thekernel_linux_mm::{
    AddressSpaceId, ExpectedMapping, FaultDisposition, FaultHandlerId, InvalidationRange,
    InvalidationReason, MappingAccess, MappingGeneration, MappingId, MappingKind, MappingSnapshot,
    MmError, PageRange, PinBudget, PinBudgetCharge, PinOwner, PinQuota, PinRegistry, PinRequest,
    PinReservation, PinToken, UffdRegisterMode, UffdRegistration,
};

use super::{
    DeferredUffdWake, LockExternalUffdOutcome, OptionalUffdPlan, PreparedRemapUffd,
    PreparedUffdMutation, RemapUffdOutcome, UffdFaultLeafState, UffdIcacheSynchronization,
    UffdPagePublication, UffdRemapKind, UffdResolverLease,
    asid::{AddressSpaceToken, HardwareAddressSpaceId, reserve_hardware_address_space_id},
    checked_align_up_4k,
    ldt::{ENTRIES, Ldt, UserDesc},
};
use crate::task::{AsThread, has_pending_sigkill};

mod alias_registry;
mod backend;
mod mapping;

pub use self::backend::*;
pub(crate) use self::{
    alias_registry::{AliasLease, PendingAliasLease, SharedBackingKey, reserve_alias_mutation},
    mapping::{FileLikeMappingLease, FileMappingLease, FileMappingSharing},
};

type SharedFolioPteRun = ReplacedPteRun<X64PTE, PagingHandlerImpl>;

/// A detached P1 run held until every alias has published its matching PMD.
/// Dropping it commits the page-table half of the transaction; passing it to
/// rollback restores the exact PTE bytes, including accessed/dirty state.
pub(crate) struct SharedFolioPteReplacement {
    start: VirtAddr,
    run: SharedFolioPteRun,
}

/// One alias switched from a shared compound PMD back to protected 4 KiB
/// leaves.  The old PMD can be restored until backing ownership commits.
pub(crate) struct SharedFolioDemotionReplacement {
    pub(crate) start: VirtAddr,
    folio: PhysAddr,
    pub(crate) flags: MappingFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultResult {
    Handled,
    Failed(PageFaultFailure),
}

/// Linux rusage classification for one successful missing-page resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageFaultKind {
    /// No backing-device read was needed.
    Minor,
    /// The backing file had to supply page contents.
    Major,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultFailure {
    AddressNotMapped,
    AccessDenied,
    BackingUnavailable,
    InternalInconsistency,
    OutOfMemory,
}

/// A retained software swap PTE captured outside swapoff's final MM lock
/// phase. The extra slot reference keeps backing stable while page-in frames
/// and page-table reservations are prepared.
pub(crate) struct SwapoffPage {
    page: VirtAddr,
    entry: crate::mm::SwapPte,
}

pub(crate) struct PreparedSwapoffPage {
    page: VirtAddr,
    entry: crate::mm::SwapPte,
    prepared: PreparedCowPage,
}

impl SwapoffPage {
    pub(crate) fn prepare(self) -> AxResult<PreparedSwapoffPage> {
        let mut prepared = PreparedCowPage::try_new()?;
        prepared.reserve_max_table_frames()?;
        unsafe {
            prepared.prepare_uninitialized(|bytes| {
                let page = core::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast(), PAGE_SIZE_4K);
                crate::mm::read(self.entry, page)
            })?;
        }
        let result = PreparedSwapoffPage {
            page: self.page,
            entry: self.entry,
            prepared,
        };
        core::mem::forget(self);
        Ok(result)
    }
}

impl Drop for SwapoffPage {
    fn drop(&mut self) {
        let _ = crate::mm::release(self.entry);
    }
}

impl Drop for PreparedSwapoffPage {
    fn drop(&mut self) {
        // This is the temporary preflight pin, distinct from the software
        // PTE reference which the commit path drops after publication.
        let _ = crate::mm::release(self.entry);
    }
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

fn wipe_on_fork_backend(start: VirtAddr, page_size: PageSize, sealed: bool) -> Backend {
    let mut backend = Backend::new_alloc(start, page_size);
    if sealed {
        backend.set_sealed();
    }
    backend
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
/// One PMD-sized anonymous promotion unit on x86_64.
///
/// Keep the eligibility test separate from the eventual page-table
/// transaction.  The latter may allocate and fail; this predicate must be a
/// side-effect-free proof that no VMA sidecar contract is crossed.
pub(crate) const COLLAPSE_2M_SIZE: usize = 2 * 1024 * 1024;

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
    Box<UserIoPinRegistry>,
);

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);
// MappingLineage reserves raw value 1 for compatibility-only untracked areas.
static NEXT_MAPPING_ID: AtomicU64 = AtomicU64::new(2);
static USER_IO_PIN_BUDGET: SpinNoIrq<Option<UserIoPinBudget>> = SpinNoIrq::new(None);

/// Exact private-COW frames retained by one active long-term writable pin.
///
/// The physical identity, rather than the registration-time VA, is retained:
/// active long-term pins deliberately allow later remap/unmap operations while
/// their lower owner remains live.  Fork can therefore ask whether the frame
/// currently present at a private-COW leaf is owned by this address
/// space's pin, without conservatively copying unrelated globally pinned
/// frames.
struct ActiveLongTermCowPin {
    token: PinToken,
    frames: Vec<PhysAddr>,
}

fn allocate_nonwrapping_id(sequence: &AtomicU64) -> AxResult<u64> {
    sequence
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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

/// Derives long-term user-I/O admission from the concrete lower owner that
/// keeps the mapping's pages stable. Device/linear mappings have no such
/// owner. A writable file-backed shared mapping additionally needs an owner
/// that records dirty/writeback state; currently only `FileBackend` provides
/// that contract through `CachedFilePagePin`.
fn mapping_user_io_pin_policy(backend: &Backend) -> (bool, bool) {
    match backend {
        Backend::Linear(_) => (false, false),
        Backend::Cow(_) => (true, false),
        // SharedPages has an exact frame owner, but it does not itself carry
        // the dirty/writeback contract required for writable FileShared pins.
        Backend::Shared(_) => (true, false),
        Backend::File(_) => (true, true),
    }
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

/// Registration and generation state for private expedited membarriers.
///
/// The registration bits are process-image state: ordinary fork copies them,
/// while exec starts with a fresh address space and therefore a fresh state.
/// The generation and acknowledgements are deliberately separate from the
/// address-space TLB generation; a barrier must never be mistaken for a page
/// table shootdown acknowledgement.
pub(crate) struct MembarrierState {
    registrations: AtomicU32,
    generation: AtomicU64,
    ack_generations: [AtomicU64; axconfig::plat::MAX_CPU_NUM],
}

impl MembarrierState {
    const REGISTER_PRIVATE: u32 = 1 << 4;
    const REGISTER_SYNC_CORE: u32 = 1 << 6;

    pub(crate) const fn new() -> Self {
        Self::with_registrations(0)
    }

    const fn with_registrations(registrations: u32) -> Self {
        Self {
            registrations: AtomicU32::new(registrations),
            generation: AtomicU64::new(0),
            ack_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
        }
    }

    pub(crate) fn fork_clone(&self) -> Self {
        Self::with_registrations(self.registrations.load(Ordering::Acquire))
    }

    pub(crate) fn register_private(&self) {
        self.registrations
            .fetch_or(Self::REGISTER_PRIVATE, Ordering::AcqRel);
    }

    pub(crate) fn register_sync_core(&self) {
        // Linux keeps the ordinary and sync-core private expedited
        // registrations independent: registering sync-core alone must not
        // authorize MEMBARRIER_CMD_PRIVATE_EXPEDITED.
        self.registrations
            .fetch_or(Self::REGISTER_SYNC_CORE, Ordering::AcqRel);
    }

    pub(crate) fn registrations(&self) -> u32 {
        self.registrations.load(Ordering::Acquire)
            & (Self::REGISTER_PRIVATE | Self::REGISTER_SYNC_CORE)
    }

    pub(crate) fn private_registered(&self) -> bool {
        self.registrations() & Self::REGISTER_PRIVATE != 0
    }

    pub(crate) fn sync_core_registered(&self) -> bool {
        self.registrations() & Self::REGISTER_SYNC_CORE != 0
    }

    /// Completes a barrier generation for a CPU that is entering this image
    /// after an issuer took its resident snapshot. Entry hooks use the same
    /// generation as the IPI path, and conservatively execute the x86
    /// serializing primitive even when the original command was the cheaper
    /// ordinary private barrier. This closes the admission/snapshot race
    /// without taking a scheduler or address-space lock.
    pub(crate) fn synchronize_entering_cpu(&self, cpu: usize) {
        let generation = self.generation.load(Ordering::SeqCst);
        if generation == 0 || self.acknowledged(cpu, generation) {
            return;
        }
        fence(Ordering::SeqCst);
        #[cfg(target_arch = "x86_64")]
        {
            let _ = core::arch::x86_64::__cpuid(0);
        }
        fence(Ordering::SeqCst);
        self.acknowledge(cpu, generation);
    }

    pub(crate) fn next_generation(&self) -> AxResult<u64> {
        self.generation
            .try_update(Ordering::SeqCst, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|_| AxError::from(axerrno::LinuxError::EOVERFLOW))
    }

    pub(crate) fn acknowledged(&self, cpu: usize, generation: u64) -> bool {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        self.ack_generations[cpu].load(Ordering::Acquire) >= generation
    }

    pub(crate) fn acknowledge(&self, cpu: usize, generation: u64) {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        let _ = self.ack_generations[cpu].try_update(
            Ordering::Release,
            Ordering::Acquire,
            |previous| Some(previous.max(generation)),
        );
    }
}

/// The virtual memory address space.
pub(crate) struct TlbState {
    generation: AtomicU64,
    resident_cpus: [AtomicBool; axconfig::plat::MAX_CPU_NUM],
    seen_generations: [AtomicU64; axconfig::plat::MAX_CPU_NUM],
    membarrier: MembarrierState,
    ldt: SpinNoIrq<Option<Arc<Ldt>>>,
}

impl TlbState {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            resident_cpus: [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM],
            seen_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
            membarrier: MembarrierState::new(),
            ldt: SpinNoIrq::new(None),
        }
    }

    fn fork_clone(&self) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            generation: AtomicU64::new(0),
            resident_cpus: [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM],
            seen_generations: [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM],
            membarrier: self.membarrier.fork_clone(),
            ldt: SpinNoIrq::new(None),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Publishes membership before sampling the generation. The returned
    /// generation is acknowledged only after the caller has completed the
    /// local flush, so a writer cannot mistake an entering CPU for one that
    /// already repaired its translations.
    fn admit_cpu(&self, cpu: usize) -> Option<u64> {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "address-space TLB CPU index exceeds fixed capacity"
        );
        self.resident_cpus[cpu].store(true, Ordering::SeqCst);
        let generation = self.generation.load(Ordering::SeqCst);
        let seen = self.seen_generations[cpu].load(Ordering::SeqCst);
        (seen < generation).then_some(generation)
    }

    pub(crate) fn enter_current(&self) {
        let _guard = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(generation) = self.admit_cpu(cpu) {
            axhal::asm::flush_tlb(None);
            self.seen_generations[cpu].store(generation, Ordering::SeqCst);
        }
        self.membarrier.synchronize_entering_cpu(cpu);
        self.reload_current_ldt();
    }

    /// Reloads the current CPU's descriptor. Callers keep IRQs/preemption
    /// disabled so the per-CPU GDT cannot be concurrently changed.
    pub(crate) fn reload_current_ldt(&self) {
        let ldt = self.ldt.lock();
        let (base, len) = ldt.as_ref().map_or((core::ptr::null(), 0), |table| {
            (table.bytes().as_ptr(), table.bytes().len())
        });
        unsafe { axhal::asm::load_user_ldt(base, len) };
    }

    fn replace_ldt(&self, new: Option<Arc<Ldt>>) -> Option<Arc<Ldt>> {
        core::mem::replace(&mut *self.ldt.lock(), new)
    }

    fn snapshot_ldt(&self) -> Option<Arc<Ldt>> {
        self.ldt.lock().clone()
    }

    pub(crate) fn membarrier_state(&self) -> &MembarrierState {
        &self.membarrier
    }

    pub(crate) fn membarrier_resident_on(&self, cpu: usize) -> bool {
        assert!(
            cpu < axconfig::plat::MAX_CPU_NUM,
            "membarrier CPU index exceeds fixed capacity"
        );
        self.resident_cpus[cpu].load(Ordering::SeqCst)
    }

    fn synchronize_after_mutation(&self) -> impl Drop {
        super::synchronize_tlb_for_addr_space(
            &self.generation,
            &self.resident_cpus,
            &self.seen_generations,
        )
    }
}

pub struct AddrSpace {
    va_range: VirtAddrRange,
    address_space_id: AddressSpaceId,
    hardware_asid: HardwareAddressSpaceId,
    /// Historical peak of resident user pages for this memory image.
    ///
    /// The mark belongs to the mm rather than a particular task: CLONE_VM
    /// owners and remote memory operations all observe the same Linux
    /// process-wide high-water mark.
    maxrss_kb: AtomicU64,
    /// Shared mm-level OOM-reaper ownership.  This is part of the address
    /// space rather than ProcessData because separate CLONE_VM process groups
    /// must never concurrently retire the same PTE/backing ownership.
    oom_reap_state: AtomicU8,
    /// Monotonic PTE invalidation generation and bounded per-CPU residency.
    /// The state is shared with scheduler hooks so they can publish residency
    /// without taking the address-space mutex.
    tlb: Arc<TlbState>,
    topology_mapping_id: MappingId,
    topology_generation: MappingGeneration,
    areas: MemorySet<Backend>,
    mapping_identities: MappingIdentityIndex,
    growdown_starts: BTreeSet<VirtAddr>,
    wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
    /// CET default shadow stacks are owned by Linux tasks, but the ownership
    /// lives in the mm that owns the VMA.  In particular, CLONE_VM peers may
    /// have distinct ProcessData objects, so keeping this in ProcessData (or
    /// walking a thread group) loses owners belonging to another sharer.
    ///
    /// This is deliberately a fallibly-grown vector rather than a global
    /// side table: it is protected by the same address-space mutex as the
    /// VMAs it names, and a task id occurs at most once in one mm.
    cet_default_shadow_stacks: Vec<CetDefaultShadowStackOwner>,
    /// One weak reverse-map lease per live shared-memory backing referenced by
    /// this mm.  Keeping leases on the address space, rather than individual
    /// VMAs, makes split/merge/unmap lifecycle handling explicit and avoids a
    /// backend-to-mm ownership cycle.
    alias_bindings: BTreeMap<SharedBackingKey, AliasLease>,

    /// Non-present anonymous leaves.  The hardware table deliberately has no
    /// encoding for them; this owner-side registry is the authoritative
    /// software PTE and keeps swap entries out of physical-address APIs.
    swapped: BTreeMap<VirtAddr, crate::mm::SwapPte>,
    user_io_pins: Box<UserIoPinRegistry>,
    active_long_term_cow_pins: Vec<ActiveLongTermCowPin>,
    pub(super) uffd: Option<Box<super::userfaultfd::UffdAddressSpaceState>>,
    lock_future_mappings: bool,
    lock_future_on_fault: bool,
    pt: PageTable,
}

/// Kernel-only ownership for one automatically allocated CET shadow stack.
/// Explicit `map_shadow_stack(2)` mappings never enter this registry.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CetDefaultShadowStackOwner {
    pub task_id: u32,
    pub start: VirtAddr,
    pub size: usize,
    pub ownership: CetDefaultShadowStackOwnership,
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CetDefaultShadowStackOwnership {
    Owned,
    Borrowed,
}

// `AddrSpace` is constructed by value during early boot and fork preparation.
// Keep the long-lived object below one base page so fixed-capacity policy
// sidecars cannot silently turn those call chains into large stack frames.
const _: () = assert!(core::mem::size_of::<AddrSpace>() <= PAGE_SIZE_4K);

/// The generic, testable core of one linear protection transaction.
///
/// This value owns the only mutable access to both the area tree and its page
/// table until it is either committed or dropped.
struct PreparedAreaProtect<'a, B: memory_set::MappingBackend> {
    areas: &'a mut MemorySet<B>,
    page_table: &'a mut B::PageTable,
    start: B::Addr,
    end: B::Addr,
    ranges: Vec<PreparedProtectRange<B::Addr, B::Flags>>,
    max_areas: usize,
}

/// One already-admitted portion of a protection transaction. Ranges are
/// disjoint and cover the complete transaction interval.
#[derive(Clone, Copy)]
struct PreparedProtectRange<A, F> {
    start: A,
    end: A,
    flags: F,
}

impl<'a, B: memory_set::MappingBackend> PreparedAreaProtect<'a, B> {
    fn flags_at(&self, address: B::Addr) -> B::Flags {
        self.ranges
            .iter()
            .find(|range| range.start <= address && address < range.end)
            .expect("prepared protection ranges cover every affected VMA")
            .flags
    }

    fn segments(&self) -> impl Iterator<Item = (&MemoryArea<B>, B::Addr, B::Addr, B::Flags)> + '_ {
        let start = self.start;
        let end = self.end;
        self.areas.iter().filter_map(move |area| {
            let affected_start = area.start().max(start);
            let affected_end = area.end().min(end);
            (affected_start < affected_end).then_some((
                area,
                affected_start,
                affected_end,
                self.flags_at(affected_start),
            ))
        })
    }

    fn commit(self) -> MappingResult<&'a mut MemorySet<B>> {
        let Self {
            areas,
            page_table,
            start,
            end,
            ranges,
            max_areas,
        } = self;
        areas.protect_with_limit(
            start,
            end.sub_addr(start),
            |affected_start, _| {
                Some(
                    ranges
                        .iter()
                        .find(|range| range.start <= affected_start && affected_start < range.end)
                        .expect("prepared protection ranges cover every affected VMA")
                        .flags,
                )
            },
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
    new_flags: MappingFlags,
}

#[allow(dead_code)]
impl<'a> PreparedProtectSegment<'a> {
    #[cfg(test)]
    pub(crate) const fn for_test(area: &'a MemoryArea<Backend>, affected: VirtAddrRange) -> Self {
        Self {
            area,
            affected,
            new_flags: area.flags(),
        }
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

    pub(crate) const fn new_flags(self) -> MappingFlags {
        self.new_flags
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
    tlb: &'a TlbState,
    mapping_identities: &'a mut MappingIdentityIndex,
    mapping_mutations: Vec<MappingIdentityMutation>,
    uffd_mutation: Option<PreparedUffdMutation<'a>>,
    synchronize_instruction_stream: bool,
}

/// Resources reserved before pkey protection splits a resident huge leaf.
/// Keeping these tables outside the VMA transaction means allocation failure
/// leaves both the PTEs and mapping metadata untouched.
pub(crate) struct PreparedPkeyDemotion {
    leaves: Vec<PreparedPkeyLeaf>,
}

struct PreparedPkeyLeaf {
    vaddr: VirtAddr,
    paddr: PhysAddr,
    size: PageSize,
    cow_backing: bool,
    tables: PreparedPageTableFrames,
}

impl PreparedPkeyDemotion {
    fn prepare_table_error(error: PrepareTableFramesError) -> AxError {
        match error {
            PrepareTableFramesError::NoMemory => AxError::NoMemory,
            PrepareTableFramesError::TooMany { .. } => AxError::BadState,
        }
    }

    pub(crate) fn commit(&mut self, pt: &mut PageTable) -> AxResult {
        let mut cursor = pt.cursor();
        for leaf in &mut self.leaves {
            if leaf.cow_backing {
                backend::register_demoted_huge_backing(leaf.paddr, leaf.size)?;
            }
            cursor
                .demote_leaf_to_4k_prepared(leaf.vaddr, &mut leaf.tables)
                .map_err(AxError::from)?;
        }
        Ok(())
    }

    pub(crate) fn apply_key(&self, pt: &mut PageTable, key: Pkey) -> AxResult {
        let mut cursor = pt.cursor();
        for leaf in &self.leaves {
            for index in 0..(leaf.size as usize / PAGE_SIZE_4K) {
                cursor
                    .set_pkey(leaf.vaddr + index * PAGE_SIZE_4K, key)
                    .map_err(AxError::from)?;
            }
        }
        Ok(())
    }
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
                |(area, affected_start, affected_end, flags)| PreparedProtectSegment {
                    area,
                    affected: VirtAddrRange::new(affected_start, affected_end),
                    new_flags: flags,
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
            tlb,
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
                    let _ = tlb.synchronize_after_mutation();
                }
            })?;
        Self::refresh_growdown_starts(areas, growdown_starts);
        let wake = uffd_mutation.map_or_else(DeferredUffdWake::empty, |mutation| mutation.commit());
        commit_mapping_identity_mutations(mapping_identities, &mapping_mutations);
        *topology_generation = next_topology_generation;
        Ok(wake)
    }

    /// Commits pkey protection after prepared huge-leaf tables have been
    /// published. The reservation is consumed before MemorySet performs its
    /// now-preflighted PTE protection pass, and no allocation occurs in the
    /// interval.
    pub(crate) fn commit_with_pkey_demotion(
        self,
        demotion: &mut PreparedPkeyDemotion,
        key: Pkey,
    ) -> AxResult<DeferredUffdWake> {
        demotion.commit(self.transaction.page_table)?;
        demotion.apply_key(self.transaction.page_table, key)?;
        self.commit()
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

    /// Assigns an x86 protection key to resident PTE leaves in a fully
    /// validated VMA range. Missing pages deliberately need no PTE update;
    /// their key is installed by the mapping metadata on first population.
    pub(crate) fn set_pkey(&mut self, start: VirtAddr, size: usize, key: u8) -> AxResult {
        let pkey = Pkey::new(key).ok_or(AxError::InvalidInput)?;
        let leaves = self.preflight_set_pkey(start, size)?;
        // The key is VMA state, not merely a currently-resident PTE bit.
        // MemorySet's protected-range transaction splits boundary VMAs before
        // publishing the replacement flags, so later demand faults, COW and
        // fork cloning receive the same key through `MappingFlags`.
        self.areas
            .protect_with_limit(
                start,
                size,
                |_, flags| Some(flags.with_pkey(key)),
                &mut self.pt,
                MAX_VMA_FRAGMENTS,
            )
            .map_err(AxError::from)?;
        let mut cursor = self.pt.cursor();
        for (vaddr, ..) in leaves {
            cursor
                .set_pkey(vaddr, pkey)
                .expect("preflighted pkey leaf must remain mapped");
        }
        drop(cursor);
        let _ = self.tlb.synchronize_after_mutation();
        Ok(())
    }

    /// Validates that changing a key cannot require an unsupported huge-leaf
    /// demotion.  It allocates every resident-leaf record before any VMA or
    /// PTE mutation, allowing callers to reject a pkey_mprotect request
    /// before beginning its ordinary protection transaction.
    pub(crate) fn preflight_set_pkey(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<Vec<(VirtAddr, PhysAddr, MappingFlags, PageSize)>> {
        self.validate_region(start, size)?;
        // Validate every resident leaf before publishing VMA metadata. A
        // partial huge leaf is handled by `prepare_pkey_demotion`.
        let leaves = self.pt.collect_present_leaves(start, size)?;
        Ok(leaves)
    }

    /// Reserves lower-level page tables for huge leaves touched partially by
    /// a pkey range. Fully covered huge leaves retain their original size.
    pub(crate) fn prepare_pkey_demotion(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<PreparedPkeyDemotion> {
        self.validate_region(start, size)?;
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let present = self.pt.collect_present_leaves(start, size)?;
        let mut leaves = Vec::new();
        leaves
            .try_reserve(present.len())
            .map_err(|_| AxError::NoMemory)?;
        for (vaddr, paddr, _, page_size) in present {
            if page_size == PageSize::Size4K {
                continue;
            }
            let leaf_end = vaddr
                .checked_add(page_size as usize)
                .ok_or(AxError::InvalidInput)?;
            if vaddr >= start && leaf_end <= end {
                continue;
            }
            let frames = match page_size {
                PageSize::Size2M => 1,
                PageSize::Size1G => 2,
                PageSize::Size4K | PageSize::Size1M => return Err(AxError::InvalidInput),
            };
            leaves.push(PreparedPkeyLeaf {
                vaddr,
                paddr,
                size: page_size,
                cow_backing: matches!(
                    self.areas.find(vaddr).map(|area| area.backend()),
                    Some(Backend::Cow(_))
                ),
                tables: PreparedPageTableFrames::try_new(frames)
                    .map_err(PreparedPkeyDemotion::prepare_table_error)?,
            });
        }
        Ok(PreparedPkeyDemotion { leaves })
    }

    /// Returns the stable policy identity of this address space.
    pub(crate) const fn address_space_id(&self) -> AddressSpaceId {
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

    /// Completes a direct leaf-PTE mutation that happened outside a backend
    /// operation.  The caller holds this address space's write lock.
    pub(crate) fn synchronize_pte_mutation(&self) {
        let _ = self.tlb.synchronize_after_mutation();
    }

    /// Returns the root physical address of the inner page table.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pt.root_paddr()
    }

    /// Returns the page-table root and bounded hardware-ASID identity.
    pub const fn address_space_token(&self) -> AddressSpaceToken {
        AddressSpaceToken::new(self.pt.root_paddr(), self.hardware_asid)
    }

    /// Publishes this address space as active on the current CPU at a
    /// scheduler context-switch boundary.
    ///
    /// Residency is published before sampling the generation. The bit is a
    /// conservative, monotonic upper bound because task-extension hooks run
    /// before the hardware page-table switch; clearing it there could let a
    /// writer release mappings while the old CR3 is still live. If a page
    /// table writer snapshots the CPU before this store, the generation load
    /// observes the writer's subsequent publication and performs the local
    /// repair itself; if it snapshots after the store, the CPU is included in
    /// the targeted shootdown.
    pub(crate) fn tlb_state(&self) -> Arc<TlbState> {
        self.tlb.clone()
    }

    pub(crate) fn ldt_snapshot(&self) -> Option<Arc<Ldt>> {
        self.tlb.snapshot_ldt()
    }

    pub(crate) fn replace_ldt_entry(&mut self, info: UserDesc, oldmode: bool) -> AxResult {
        let index = info.entry_number as usize;
        if index >= ENTRIES {
            return Err(AxError::InvalidInput);
        }
        let old = self.tlb.snapshot_ldt();
        let mut next = Ldt::new(core::cmp::max(
            index + 1,
            old.as_ref().map_or(0, |table| table.len()),
        ))?;
        if let Some(old) = old.as_ref() {
            old.copy_into(&mut next);
        }
        next.set(index, Ldt::descriptor(info, oldmode)?);
        let retired = self
            .tlb
            .replace_ldt(Some(Arc::try_new(next).map_err(|_| AxError::NoMemory)?));

        // Publish the new descriptor locally before remote acknowledgements
        // make the old backing allocation reclaimable.
        {
            let _guard = NoPreemptIrqSave::new();
            self.tlb.reload_current_ldt();
        }
        let grace = self.synchronize_tlb_after_mutation();
        drop(grace);
        drop(retired);
        Ok(())
    }

    /// Completes one PTE mutation's full local flush and targeted grace.
    ///
    /// The helper advances the generation before taking the active snapshot;
    /// callers must invoke it after publishing page-table stores but before
    /// releasing any retired mapping/frame ownership.
    pub(crate) fn synchronize_tlb_after_mutation(&self) -> impl Drop {
        self.tlb.synchronize_after_mutation()
    }

    /// Checks if the address space contains the given address range.
    pub fn contains_range(&self, start: VirtAddr, size: usize) -> bool {
        self.va_range.contains(start) && (self.va_range.end - start) >= size
    }

    /// Creates a new empty address space.
    pub fn new_empty(base: VirtAddr, size: usize) -> AxResult<Self> {
        #[cfg(test)]
        crate::test_support::ensure_host_memory();

        let va_range = VirtAddrRange::try_from_start_size(base, size).ok_or(AxError::NoMemory)?;
        let (address_space_id, topology_mapping_id, topology_generation, user_io_pins) =
            new_user_io_policy()?;
        let hardware_asid = reserve_hardware_address_space_id();
        let mut active_long_term_cow_pins = Vec::new();
        active_long_term_cow_pins
            .try_reserve_exact(USER_IO_PIN_MAX_TOKENS as usize)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            va_range,
            address_space_id,
            hardware_asid,
            maxrss_kb: AtomicU64::new(0),
            oom_reap_state: AtomicU8::new(0),
            tlb: Arc::try_new(TlbState::new()).map_err(|_| AxError::NoMemory)?,
            topology_mapping_id,
            topology_generation,
            areas: MemorySet::new(),
            mapping_identities: MappingIdentityIndex::new(),
            growdown_starts: BTreeSet::new(),
            wipe_on_fork_ranges: BTreeMap::new(),
            dontfork_ranges: BTreeMap::new(),
            locked_ranges: BTreeMap::new(),
            cet_default_shadow_stacks: Vec::new(),
            alias_bindings: BTreeMap::new(),
            swapped: BTreeMap::new(),
            user_io_pins,
            active_long_term_cow_pins,
            uffd: None,
            lock_future_mappings: false,
            lock_future_on_fault: false,
            pt: PageTable::try_new().map_err(|_| AxError::NoMemory)?,
        })
    }

    /// The mmap-lock equivalent used by operations that Linux specifies as
    /// killable.  The sleeping mutex registers an event listener before its
    /// SeqCst owner recheck, so unlock notification cannot be lost; only a
    /// pending SIGKILL cancels the wait, matching fatal_signal_pending().
    pub(crate) fn lock_interruptibly(
        handle: &Arc<Mutex<Self>>,
    ) -> AxResult<axsync::MutexGuard<'_, Self>> {
        axsync::lock_interruptible(handle, || {
            let current = axtask::current();
            let thread = current.as_thread();
            has_pending_sigkill(thread)
                || thread.proc_data.should_exit_for_exec(thread.kernel_tid())
        })
        .ok_or(AxError::Interrupted)
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

    /// Synchronizes this mm's reverse-map leases after a shared mapping
    /// topology change.  The caller supplies the owning Arc at syscall/fork
    /// boundaries; ordinary `map` remains a pure address-space operation.
    pub(crate) fn sync_shared_alias_bindings(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult {
        let mut keys = Vec::new();
        keys.try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if let Some(key) = area.backend().shared_backing_key() {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        keys.dedup();

        self.bind_shared_alias_keys(&keys, aspace)?;
        self.alias_bindings
            .retain(|key, _| keys.binary_search(key).is_ok());
        Ok(())
    }

    fn bind_shared_alias_keys(
        &mut self,
        keys: &[SharedBackingKey],
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult {
        let mut inserted = Vec::new();
        inserted
            .try_reserve(keys.len())
            .map_err(|_| AxError::NoMemory)?;
        for key in keys.iter().copied() {
            if self.alias_bindings.contains_key(&key) {
                continue;
            }
            match AliasLease::try_new(key, aspace, self.address_space_id) {
                Ok(lease) => {
                    self.alias_bindings.insert(key, lease);
                    inserted.push(key);
                }
                Err(error) => {
                    for key in inserted {
                        self.alias_bindings.remove(&key);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Reserves the reverse-map generation required for one newly published
    /// shared VMA.  The outer syscall owns the returned guard across map and
    /// must either commit it immediately after VMA publication or let Drop
    /// abort it on every failure path.
    pub(crate) fn prepare_shared_alias_binding(
        &self,
        key: SharedBackingKey,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Option<PendingAliasLease>> {
        if self.alias_bindings.contains_key(&key) {
            return Ok(None);
        }
        PendingAliasLease::prepare(key, aspace, self.address_space_id).map(Some)
    }

    pub(crate) fn commit_shared_alias_binding(&mut self, pending: PendingAliasLease) {
        let lease = pending.commit();
        let key = lease.key();
        let previous = self.alias_bindings.insert(key, lease);
        debug_assert!(previous.is_none(), "duplicate shared alias commit");
    }

    fn prune_shared_alias_bindings(&mut self) {
        self.alias_bindings.retain(|key, _| {
            self.areas
                .iter()
                .any(|area| area.backend().shared_backing_key() == Some(*key))
        });
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

    /// Linux mseal requires a mapped, gap-free range. The metadata transaction
    /// splits boundary VMAs before setting VM_SEALED, with no PTE mutation.
    pub(crate) fn seal(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }
        if !range_is_fully_mapped(&self.areas, start, size) {
            return Err(AxError::NoMemory);
        }
        if self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, start + size))
            .all(|area| area.backend().is_sealed())
        {
            return Ok(());
        }
        let next_topology_generation = self.next_topology_generation()?;
        let updated = self.areas.update_metadata_with_limit(
            start,
            size,
            |backend| !backend.is_sealed(),
            Backend::set_sealed,
            MAX_VMA_FRAGMENTS,
        );
        match updated {
            Ok(()) => {
                self.commit_topology_generation(next_topology_generation);
                Ok(())
            }
            Err(error) => {
                let (error, changed) = error.into_parts();
                if changed {
                    self.commit_topology_generation(next_topology_generation);
                }
                Err(AxError::from(error))
            }
        }
    }

    pub(crate) fn check_no_seal_overlap(&self, start: VirtAddr, size: usize) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        if self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .any(|area| area.backend().is_sealed())
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    /// mremap checks the initially looked-up VMA's seal before validating the
    /// rest of its source geometry. Leave an unmapped address to the normal
    /// source-range validator so it retains Linux's EFAULT/ENOMEM mapping.
    pub(crate) fn check_vma_at_not_sealed(&self, address: VirtAddr) -> AxResult {
        if self
            .find_area(address)
            .is_some_and(|area| area.backend().is_sealed())
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    /// `can_modify_vma_madv()` in Linux 6.12.103 only rejects discard-style
    /// advice for sealed, read-only private anonymous VMAs.
    pub(crate) fn sealed_ro_anon_in_range(&self, start: VirtAddr, size: usize) -> bool {
        let Some(end) = start.checked_add(size) else {
            return false;
        };
        self.areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .any(|area| {
                area.backend().is_sealed()
                    && area.backend().is_private_anonymous()
                    && !area.flags().contains(MappingFlags::WRITE)
            })
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
        } else {
            let range = VirtAddrRange::from_start_size(start, size);
            let secret_ranges: Vec<_> = self
                .areas_overlapping(range)
                .filter(|area| area.backend().is_secret())
                .map(|area| {
                    let range_start = area.start().max(start);
                    let range_end = area.end().min(start + size);
                    (range_start, range_end)
                })
                .collect();
            for (range_start, range_end) in secret_ranges {
                self.insert_locked_range(range_start, range_end);
            }
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

    /// Classifies one PMD-sized private-anonymous range for MADV_COLLAPSE.
    ///
    /// This is intentionally a conservative VMA-side proof. Long-term COW
    /// pins retain physical frames rather than virtual ranges, so their exact
    /// intersection is checked after the source leaves are collected. The
    /// caller still must validate all 4 KiB leaves and commit the replacement
    /// atomically.
    pub(crate) fn collapse_2m_candidate_eligible(&self, start: VirtAddr, length: usize) -> bool {
        let Some(end_raw) = start.as_usize().checked_add(length) else {
            return false;
        };
        let end = VirtAddr::from(end_raw);
        let area = self.find_area(start);
        let vma_covers_range = area.is_some_and(|area| area.start() <= start && area.end() >= end);
        let private_cow = area.is_some_and(|area| area.backend().is_private_cow());
        let range = PageRange::new(start.as_usize(), length, PAGE_SIZE_4K).ok();
        let has_uffd_write_protect = range.is_some_and(|range| {
            self.uffd.as_ref().is_some_and(|state| {
                state
                    .registrations
                    .intersecting(self.address_space_id, range)
                    .any(|registration| {
                        registration.mode().bits() & UffdRegisterMode::WP.bits() != 0
                    })
            })
        });
        let has_fork_policy = Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end);
        collapse_2m_candidate_eligible(Collapse2MCandidateFacts {
            start: start.as_usize(),
            length,
            vma_covers_range,
            private_cow,
            has_uffd_write_protect,
            has_locked_pages: self.range_is_locked(start, length),
            // Exact pin ownership is established from the PTE source frames
            // below; never reject a PMD merely because another PMD is pinned.
            has_exact_long_term_cow_pin: false,
            has_fork_policy,
        })
    }

    fn uffd_missing_registered_at(&self, vaddr: VirtAddr) -> bool {
        let Ok(page) = PageRange::new(vaddr.as_usize(), PAGE_SIZE_4K, PAGE_SIZE_4K) else {
            return false;
        };
        self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, page)
                .any(|registration| {
                    registration.mode().bits() & UffdRegisterMode::MISSING.bits() != 0
                })
        })
    }

    /// Collapses one private COW PMD into a privately owned 2 MiB leaf,
    /// materializing absent anonymous or file-backed leaves directly in the
    /// prepared frame.
    ///
    /// The source leaves are first made read-only and observed through a TLB
    /// grace period, so copying cannot race a stale writable translation.  A
    /// new huge frame is then copied before the single PDE publication.  The
    /// VMA fragment keeps its lineage while changing only its COW granule;
    /// every detached 4 KiB frame and the former PTE table remain owned until
    /// the replacement's TLB grace completes.
    pub(crate) fn collapse_private_cow_2m(&mut self, start: VirtAddr) -> AxResult {
        if !self.collapse_2m_candidate_eligible(start, COLLAPSE_2M_SIZE) {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        // This is the only fallible bookkeeping step after PDE publication;
        // admit it before changing either metadata or translations.
        let next_topology_generation = self.next_topology_generation()?;

        let source_backend = self
            .find_area(start)
            .ok_or(AxError::NoMemory)?
            .backend()
            .clone();
        let vma_flags = self.find_area(start).ok_or(AxError::NoMemory)?.flags();
        let collapsed_backend = source_backend.collapsed_2m_backend()?;
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        let mut source_slots = Vec::new();
        source_slots
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;

        let mut source_flags = None;
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            let vaddr = start + offset;
            match self.pt.query(vaddr) {
                Ok((paddr, flags, page_size)) => {
                    if page_size != PageSize::Size4K
                        || !PageSize::Size4K.is_aligned(paddr.as_usize())
                        || source_flags.is_some_and(|expected| expected != flags)
                    {
                        return Err(AxError::InvalidInput);
                    }
                    source_flags = Some(flags);
                    source_slots.push(Some(paddr));
                    leaves.push((vaddr, paddr, flags, page_size));
                }
                Err(PagingError::NotMapped) => {
                    if self.uffd_missing_registered_at(vaddr) {
                        return Err(AxError::ResourceBusy);
                    }
                    source_slots.push(None);
                }
                Err(_) => return Err(AxError::BadAddress),
            }
        }
        // For an entirely untouched anonymous VMA there is no PTE flag to
        // inherit; its VMA access contract becomes the new PMD leaf flags.
        let source_flags = source_flags.unwrap_or_else(|| {
            if vma_flags.contains(MappingFlags::WRITE) {
                vma_flags | MappingFlags::READ
            } else {
                vma_flags
            }
        });
        if !source_flags.contains(MappingFlags::WRITE) {
            return Err(AxError::InvalidInput);
        }
        let protected_flags = source_flags - MappingFlags::WRITE;

        // Long-term writable pins are tracked by physical frame precisely so
        // virtual remaps do not turn an unrelated pin into a global barrier.
        // Reject only this PMD's own source frames, before write-protecting
        // them or allocating the replacement frame.
        let pinned_frames = self.active_long_term_cow_frames()?;
        if leaves
            .iter()
            .any(|(_, frame, ..)| pinned_frames.binary_search(frame).is_ok())
            || any_frame_pinned(leaves.iter().map(|(_, frame, ..)| *frame))
        {
            return Err(AxError::ResourceBusy);
        }

        // The address-space lock prevents ordinary page-table mutation, but a
        // running CPU can still hold a writable translation. Revoke it before
        // taking the source snapshot.
        let write_protection_failed = {
            let mut cursor = self.pt.cursor();
            let mut failed = false;
            for (vaddr, ..) in &leaves {
                match cursor.protect(*vaddr, protected_flags) {
                    Ok(PageSize::Size4K) => {}
                    Ok(_) | Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            failed
        };
        if write_protection_failed {
            self.restore_collapse_2m_source_permissions(&leaves)?;
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());

        let mut prepared = match source_backend.prepare_collapse_2m_frame(start, &source_slots) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.restore_collapse_2m_source_permissions(&leaves)?;
                return Err(error);
            }
        };

        // Split and update the one affected VMA before publishing the PDE.
        // A failure here still has the old PTE run installed, so restoring
        // permissions is sufficient rollback.
        if let Err(error) =
            self.replace_collapse_2m_backend_metadata(start, collapsed_backend.clone())
        {
            self.restore_collapse_2m_source_permissions(&leaves)?;
            return Err(error);
        }

        let replacement = prepared.frame()?;
        let replaced = {
            let mut cursor = self.pt.cursor();
            match cursor.replace_2m_pte_run(start, replacement, source_flags) {
                Ok(replaced) => Ok(Some(replaced)),
                // A completely untouched VMA need not have a P1 table yet.
                // Publish the prepared PMD directly in that case; the map
                // path constructs only unreachable intermediate tables before
                // linking the huge leaf.
                Err(PagingError::NotMapped) => cursor
                    .map(start, replacement, PageSize::Size2M, source_flags)
                    .map(|_| None)
                    .map_err(|_| AxError::BadState),
                Err(_) => Err(AxError::BadState),
            }
        };
        let replaced = match replaced {
            Ok(replaced) => replaced,
            Err(error) => {
                // The metadata update is now an exact 2 MiB fragment, so this
                // cannot allocate or split. Preserve the original lineage and
                // restore the 4 KiB backend before returning the old PTEs to
                // userspace.
                self.replace_collapse_2m_backend_metadata(start, source_backend.clone())?;
                self.restore_collapse_2m_source_permissions(&leaves)?;
                return Err(error);
            }
        };
        prepared.commit_frame();

        // `leaves` was checked above and belongs to this exact 4 KiB COW
        // backend, so retirement is now an infallible ownership conversion.
        let retired = source_backend
            .retire_collapsed_2m_sources(start, leaves)
            .expect("validated COW collapse leaves must be retireable");
        self.commit_topology_generation(next_topology_generation);

        // `replaced` owns the detached P1 table and `retired` owns the old
        // COW frames. Neither can be released before the PDE publication has
        // reached every CPU which could retain a former 4 KiB translation.
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(replaced);
        drop(grace);
        Ok(())
    }

    /// Promotes a fully resident, physically contiguous shared/file 4 KiB
    /// run without changing its backing ownership.
    ///
    /// Unlike private anonymous collapse this must not copy into a new frame:
    /// doing so would sever MAP_SHARED visibility or the file-cache's
    /// writeback and eviction identity.  A naturally contiguous cache/shmem
    /// run can instead be represented directly by a PDE referring to the
    /// exact same frames.  Non-contiguous or sparse runs remain ineligible.
    pub(crate) fn collapse_alias_preserving_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !matches!(area.backend(), Backend::Shared(_) | Backend::File(_))
            || self.range_is_locked(start, COLLAPSE_2M_SIZE)
            || Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end)
        {
            return Err(AxError::InvalidInput);
        }
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }

        let (base, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size4K || !PageSize::Size2M.is_aligned(base.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            let (paddr, leaf_flags, leaf_size) =
                self.pt.query(start + offset).map_err(|error| match error {
                    PagingError::NotMapped => AxError::NoMemory,
                    _ => AxError::BadAddress,
                })?;
            let expected = base + offset;
            if leaf_size != PageSize::Size4K || leaf_flags != flags || paddr != expected {
                return Err(AxError::InvalidInput);
            }
        }
        let replaced = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(start, base, flags)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        // The detached P1 table is the only retired object.  The data frames
        // remain owned by SharedPages/CachedFile throughout this transaction.
        let grace = self.synchronize_tlb_after_mutation();
        drop(replaced);
        drop(grace);
        Ok(())
    }

    /// Finds every PMD-aligned mapping of one shared backing folio in this
    /// address space.  A non-PMD alias of the same 2 MiB backing must make the
    /// whole promotion ineligible: otherwise its old PTEs would keep pointing
    /// at the pre-folio frames after the shared backing is switched.
    pub(crate) fn shared_folio_alias_starts(
        &self,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<Vec<VirtAddr>> {
        let backing_start = start_index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AxError::InvalidInput)?;
        let backing_end = backing_start
            .checked_add(COLLAPSE_2M_SIZE)
            .ok_or(AxError::InvalidInput)?;
        let mut starts = Vec::new();
        starts
            .try_reserve_exact(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            let Some(shared) = area.backend().shared_pages() else {
                continue;
            };
            if !Arc::ptr_eq(shared, pages) {
                continue;
            }
            let backend_start = match area.backend() {
                Backend::Shared(shared) => shared
                    .backing_offset(area.start().as_usize())
                    .ok_or(AxError::BadState)?,
                _ => unreachable!("shared pages originate only from SharedBackend"),
            };
            let backend_end = backend_start
                .checked_add(area.size())
                .ok_or(AxError::BadState)?;
            if backing_start >= backend_end || backing_end <= backend_start {
                continue;
            }
            // A partial overlap is still an alias of pages we are about to
            // replace, but cannot be made into a PMD without changing its VMA
            // geometry. Reject before any folio or PTE publication.
            if backing_start < backend_start || backing_end > backend_end {
                return Err(AxError::InvalidInput);
            }
            let start = area.start() + (backing_start - backend_start);
            if !PageSize::Size2M.is_aligned(start.as_usize()) {
                return Err(AxError::InvalidInput);
            }
            starts.push(start);
        }
        Ok(starts)
    }

    /// Validates an alias P1 run against the shared backing before its folio
    /// is promoted.  It performs every fallible VMA/UFFD/pin/PTE check while
    /// the old mapping remains live; publication below is then one PMD store.
    pub(crate) fn preflight_shared_folio_collapse_2m(
        &self,
        start: VirtAddr,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<MappingFlags> {
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if self.range_is_locked(start, COLLAPSE_2M_SIZE)
            || Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end)
        {
            return Err(AxError::InvalidInput);
        }
        let Some(mapped_pages) = area.backend().shared_pages() else {
            return Err(AxError::InvalidInput);
        };
        if !Arc::ptr_eq(mapped_pages, pages) {
            return Err(AxError::BadState);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }
        let (first, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size4K || first != pages.paddr_at(start_index)? {
            return Err(AxError::InvalidInput);
        }
        for page in 0..(COLLAPSE_2M_SIZE / PAGE_SIZE_4K) {
            let address = start + page * PAGE_SIZE_4K;
            let (paddr, leaf_flags, leaf_size) =
                self.pt.query(address).map_err(|error| match error {
                    PagingError::NotMapped => AxError::NoMemory,
                    _ => AxError::BadAddress,
                })?;
            if leaf_size != PageSize::Size4K
                || leaf_flags != flags
                || paddr != pages.paddr_at(start_index + page)?
            {
                return Err(AxError::InvalidInput);
            }
        }
        Ok(flags)
    }

    pub(crate) fn publish_shared_folio_collapse_2m(
        &mut self,
        start: VirtAddr,
        folio: PhysAddr,
        flags: MappingFlags,
    ) -> AxResult<SharedFolioPteReplacement> {
        let source_flags = if flags.contains(MappingFlags::WRITE) {
            flags - MappingFlags::WRITE
        } else {
            flags
        };
        let run = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(start, folio, source_flags)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        if source_flags != flags {
            let restored = {
                let mut cursor = self.pt.cursor();
                cursor.protect(start, flags)
            };
            if !matches!(restored, Ok(PageSize::Size2M)) {
                let rollback = {
                    let mut cursor = self.pt.cursor();
                    cursor.rollback_2m_pte_replacement(start, run)
                };
                return match rollback {
                    Ok(()) => Err(AxError::BadState),
                    Err(_) => Err(AxError::BadState),
                };
            }
        }
        Ok(SharedFolioPteReplacement { start, run })
    }

    /// Revokes writable 4 KiB translations before a shared-folio snapshot.
    /// The address-space mutex alone does not evict translations that a CPU
    /// installed before this transaction started.
    pub(crate) fn write_protect_shared_folio_collapse_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let protected = flags - MappingFlags::WRITE;
        let result = {
            let mut cursor = self.pt.cursor();
            let mut result = Ok(());
            for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
                if !matches!(
                    cursor.protect(start + offset, protected),
                    Ok(PageSize::Size4K)
                ) {
                    result = Err(AxError::BadState);
                    break;
                }
            }
            result
        };
        if result.is_err() {
            self.restore_shared_folio_permissions_2m(start, flags)?;
            return result;
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn restore_shared_folio_permissions_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        {
            let mut cursor = self.pt.cursor();
            for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
                if !matches!(cursor.protect(start + offset, flags), Ok(PageSize::Size4K)) {
                    return Err(AxError::BadState);
                }
            }
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn rollback_shared_folio_collapse_2m(
        &mut self,
        replacement: SharedFolioPteReplacement,
    ) -> AxResult {
        {
            let mut cursor = self.pt.cursor();
            cursor.rollback_2m_pte_replacement(replacement.start, replacement.run)
        }
        .map_err(|_| AxError::BadState)?;
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn commit_shared_folio_collapse_2m(
        &mut self,
        replacement: SharedFolioPteReplacement,
    ) {
        let grace = self.synchronize_tlb_after_mutation();
        drop(replacement);
        drop(grace);
    }

    fn replace_collapse_2m_backend_metadata(
        &mut self,
        start: VirtAddr,
        replacement: Backend,
    ) -> AxResult {
        self.areas
            .update_metadata_with_limit(
                start,
                COLLAPSE_2M_SIZE,
                |_| true,
                |backend| *backend = replacement.clone(),
                MAX_VMA_FRAGMENTS,
            )
            .map_err(|error| AxError::from(error.into_parts().0))
    }

    fn restore_collapse_2m_source_permissions(
        &mut self,
        leaves: &[(VirtAddr, PhysAddr, MappingFlags, PageSize)],
    ) -> AxResult {
        {
            let mut cursor = self.pt.cursor();
            for (vaddr, _, flags, _) in leaves {
                match cursor.protect(*vaddr, *flags) {
                    Ok(PageSize::Size4K) => {}
                    Ok(_) | Err(_) => return Err(AxError::BadState),
                }
            }
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    /// Demotes one private anonymous COW PMD into a prepared P1 run.
    ///
    /// A writable PMD is first revoked and flushed from the CPUs currently
    /// running this address space.  Only after that grace period is its
    /// content copied.  Thus no stale writable translation can modify the
    /// source while the replacement frames are being made.  Every failure
    /// before the PDE publication restores the exact original PMD flags;
    /// after publication the old PMD frame stays owned through a second,
    /// targeted TLB grace period.
    pub(crate) fn demote_private_cow_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let end = start + COLLAPSE_2M_SIZE;
        let source_backend = self
            .find_area(start)
            .filter(|area| area.start() == start && area.end() == end)
            .ok_or(AxError::NoMemory)?
            .backend()
            .clone();
        if !source_backend.is_private_cow() || source_backend.page_size() != PageSize::Size2M {
            return Err(AxError::InvalidInput);
        }
        let (source_frame, source_flags, source_size) =
            self.pt.query(start).map_err(|error| match error {
                PagingError::NotMapped if self.uffd_missing_registered_at(start) => {
                    AxError::ResourceBusy
                }
                PagingError::NotMapped => AxError::NoMemory,
                _ => AxError::BadAddress,
            })?;
        if source_size != PageSize::Size2M || !PageSize::Size2M.is_aligned(source_frame.as_usize())
        {
            return Err(AxError::BadState);
        }
        let next_topology_generation = self.next_topology_generation()?;
        let demoted_backend = source_backend.demoted_4k_backend()?;
        let mut tables = PreparedPageTableFrames::try_new(1).map_err(|_| AxError::NoMemory)?;

        // The address-space mutex serializes page-table writers, but CPUs
        // which ran this mm before we acquired it can retain a writable PMD
        // translation. Revoke WRITE and wait for precisely those CPUs before
        // sampling the source frame.
        let protected_flags = source_flags - MappingFlags::WRITE;
        let protected = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, protected_flags)
        };
        if !matches!(protected, Ok(PageSize::Size2M)) {
            // `protect` is expected to be all-or-nothing for one PMD. Still
            // restore the observed PMD exactly in case a malformed table made
            // the operation report after changing it.
            self.restore_demote_2m_source_permissions(start, source_flags)?;
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());

        let mut prepared = match source_backend.prepare_demote_2m_frames(source_frame) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.restore_demote_2m_source_permissions(start, source_flags)?;
                return Err(error);
            }
        };

        // All metadata allocation/splitting is admitted before the PDE store.
        if let Err(error) = self.replace_collapse_2m_backend_metadata(start, demoted_backend) {
            self.restore_demote_2m_source_permissions(start, source_flags)?;
            return Err(error);
        }
        let published = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(
                start,
                prepared.frames(),
                source_flags,
                &mut tables,
            )
        };
        let published = match published {
            Ok(frame) => frame,
            Err(error) => {
                self.replace_collapse_2m_backend_metadata(start, source_backend)?;
                self.restore_demote_2m_source_permissions(start, source_flags)?;
                return Err(match error {
                    PagingError::NoMemory => AxError::NoMemory,
                    _ => AxError::BadState,
                });
            }
        };
        debug_assert_eq!(published, source_frame);
        prepared.commit_frames();
        let retired = source_backend
            .retire_demoted_2m_source(start, source_frame, source_flags)
            .expect("validated huge COW leaf must be retireable");
        self.commit_topology_generation(next_topology_generation);
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(grace);
        Ok(())
    }

    /// Expands an alias-preserving shared/file PDE back into 4 KiB leaves
    /// referring to the same backing/cache frames.  No data frame changes
    /// ownership, so this is safe for all MAP_SHARED aliases and cache pins.
    pub(crate) fn demote_alias_preserving_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !matches!(area.backend(), Backend::Shared(_) | Backend::File(_)) {
            return Err(AxError::InvalidInput);
        }
        let (source, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size2M || !PageSize::Size2M.is_aligned(source.as_usize()) {
            return Err(AxError::BadState);
        }
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            leaves.push(source + offset);
        }
        let mut tables = PreparedPageTableFrames::try_new(1).map_err(|_| AxError::NoMemory)?;
        let published = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(start, &leaves, flags, &mut tables)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        debug_assert_eq!(published, source);
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn preflight_shared_folio_demotion_2m(
        &self,
        start: VirtAddr,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<MappingFlags> {
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !Arc::ptr_eq(
            area.backend().shared_pages().ok_or(AxError::InvalidInput)?,
            pages,
        ) {
            return Err(AxError::BadState);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }
        let (folio, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size2M || folio != pages.paddr_at(start_index)? {
            return Err(AxError::BadState);
        }
        Ok(flags)
    }

    pub(crate) fn write_protect_shared_folio_demotion_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let result = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, flags - MappingFlags::WRITE)
        };
        if !matches!(result, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn restore_shared_folio_demotion_pmd_permissions(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let result = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, flags)
        };
        if !matches!(result, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn publish_shared_folio_demotion_2m(
        &mut self,
        start: VirtAddr,
        frames: &[PhysAddr],
        flags: MappingFlags,
        tables: &mut PreparedPageTableFrames,
    ) -> AxResult<SharedFolioDemotionReplacement> {
        let protected = flags - MappingFlags::WRITE;
        let folio = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(start, frames, protected, tables)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        Ok(SharedFolioDemotionReplacement {
            start,
            folio,
            flags,
        })
    }

    pub(crate) fn rollback_shared_folio_demotion_2m(
        &mut self,
        replacement: SharedFolioDemotionReplacement,
    ) -> AxResult {
        let protected = replacement.flags - MappingFlags::WRITE;
        let run = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(replacement.start, replacement.folio, protected)
        }
        .map_err(|_| AxError::BadState)?;
        let grace = self.synchronize_tlb_after_mutation();
        drop(run);
        drop(grace);
        self.restore_shared_folio_demotion_pmd_permissions(replacement.start, replacement.flags)
    }

    /// Restores the source PMD after a demotion failure which occurred before
    /// replacement publication.  The target is one exact leaf, so restoring
    /// its original hardware flags is a single page-table mutation followed
    /// by the same targeted TLB grace used for write revocation.
    fn restore_demote_2m_source_permissions(
        &mut self,
        start: VirtAddr,
        source_flags: MappingFlags,
    ) -> AxResult {
        let restored = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, source_flags)
        };
        if !matches!(restored, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    /// Ensures that mutations which operate at page granularity never leave a
    /// private anonymous huge COW mapping or an alias-preserving shared/file
    /// huge mapping behind them.
    ///
    /// Call this after the operation's non-MM admission gates, but before it
    /// prepares VMA/PTE mutations or observes individual PTEs.
    pub(crate) fn ensure_4k_granularity(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }

        let end = start + size;
        let mut candidate = VirtAddr::from(start.as_usize() & !(COLLAPSE_2M_SIZE - 1));
        while candidate < end {
            let demote_private = self.areas.find(candidate).is_some_and(|area| {
                area.start() == candidate
                    && area.size() == COLLAPSE_2M_SIZE
                    && area.backend().is_private_cow()
                    && area.backend().page_size() == PageSize::Size2M
            });
            if demote_private {
                self.demote_private_cow_2m(candidate)?;
            } else {
                let demote_alias = self.areas.find(candidate).is_some_and(|area| {
                    area.start() <= candidate
                        && area.end() >= candidate + COLLAPSE_2M_SIZE
                        && matches!(area.backend(), Backend::Shared(_) | Backend::File(_))
                }) && self
                    .pt
                    .query(candidate)
                    .is_ok_and(|(_, _, page_size)| page_size == PageSize::Size2M);
                if demote_alias {
                    if let (Some(pages), Some(offset)) = (
                        self.shared_pages_at(candidate),
                        self.shared_backing_offset_at(candidate),
                    ) {
                        if pages.page_size() == PageSize::Size4K
                            && offset.is_multiple_of(COLLAPSE_2M_SIZE)
                            && pages.has_4k_folio(offset / PAGE_SIZE_4K)
                        {
                            // A compound shmem folio owns one set of former
                            // 4 KiB frames for every mm alias.  Its caller
                            // must use the ordered cross-mm transaction.
                            return Err(AxError::BadState);
                        }
                    }
                    self.demote_alias_preserving_2m(candidate)?;
                }
            }
            candidate = candidate
                .checked_add(COLLAPSE_2M_SIZE)
                .ok_or(AxError::InvalidInput)?;
        }
        Ok(())
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
    pub(crate) fn commit_user_io_pin(
        &mut self,
        reservation: PinReservation,
        cow_frames: &mut Vec<PhysAddr>,
    ) -> AxResult<PinToken> {
        let request = self
            .user_io_pins
            .view(reservation.token())
            .map_err(mm_error)?
            .request();
        let tracks_cow_frames = request.duration() == thekernel_linux_mm::PinDuration::LongTerm
            && request.access() == thekernel_linux_mm::PinAccess::Write
            && !cow_frames.is_empty();
        if tracks_cow_frames
            && self.active_long_term_cow_pins.len() == self.active_long_term_cow_pins.capacity()
        {
            return Err(AxError::ResourceBusy);
        }

        let token = self.user_io_pins.commit(reservation).map_err(mm_error)?;
        if tracks_cow_frames {
            self.active_long_term_cow_pins.push(ActiveLongTermCowPin {
                token,
                frames: core::mem::take(cow_frames),
            });
        }
        Ok(token)
    }

    pub(crate) fn end_user_io_pin(&mut self, token: PinToken) -> Option<Vec<PhysAddr>> {
        if let Err(error) = self.user_io_pins.release(token) {
            warn!(
                "AddrSpace::end_user_io_pin: token {}: {error:?}",
                token.get()
            );
            return None;
        }
        self.active_long_term_cow_pins
            .iter()
            .position(|pin| pin.token == token)
            .map(|index| self.active_long_term_cow_pins.swap_remove(index).frames)
    }

    fn active_long_term_cow_frames(&self) -> AxResult<Vec<PhysAddr>> {
        let count = self
            .active_long_term_cow_pins
            .iter()
            .try_fold(0usize, |count, pin| count.checked_add(pin.frames.len()))
            .ok_or(AxError::NoMemory)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for pin in &self.active_long_term_cow_pins {
            frames.extend_from_slice(&pin.frames);
        }
        frames.sort_unstable();
        frames.dedup();
        Ok(frames)
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
        &mut self,
        destination: PageRange,
    ) -> AxResult<UffdResolverLease> {
        self.ensure_4k_granularity(VirtAddr::from(destination.start()), destination.len())?;
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
        // A swap PTE is a recoverable resident image, not a UFFD MISSING
        // hole.  Let normal fault handling page it in rather than allowing a
        // resolver to overwrite it with zeroes or unrelated copied bytes.
        if self.swapped.contains_key(&page) {
            return Ok(None);
        }
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
        let (long_term_pinnable, writable_file_pin_supported) =
            mapping_user_io_pin_policy(area.backend());
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
            long_term_pinnable,
            writable_file_pin_supported,
        ))
    }

    fn projected_protect_piece_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        ranges: &[PreparedProtectRange<B::Addr, B::Flags>],
        address: B::Addr,
    ) -> Option<ProjectedProtectPiece<'a, B>> {
        let area = areas.find(address)?;
        let (start, end, flags) = if address < protect.start {
            (area.start(), area.end().min(protect.start), area.flags())
        } else if address < protect.end {
            (
                area.start().max(protect.start),
                area.end().min(protect.end),
                ranges
                    .iter()
                    .find(|range| range.start <= address && address < range.end)
                    .expect("prepared protection ranges cover projected UFFD fragment")
                    .flags,
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
    fn projected_protect_run_at_ranges<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        ranges: &[PreparedProtectRange<B::Addr, B::Flags>],
        address: B::Addr,
    ) -> Option<ProjectedProtectRun<'a, B>> {
        let mut anchor = Self::projected_protect_piece_at(areas, protect, ranges, address)?;

        // Backend compatibility is deliberately not a left-scan barrier.
        // MemorySet processes protection actions in ascending address order,
        // retains the left backend after a merge, and starts a new run after
        // an incompatible pair. Replaying from the first structurally
        // compatible piece is therefore necessary because `can_merge` is not
        // required to be transitive.
        while Into::<usize>::into(anchor.start) != 0 {
            let previous_address = B::Addr::from(Into::<usize>::into(anchor.start) - 1);
            let Some(previous) =
                Self::projected_protect_piece_at(areas, protect, ranges, previous_address)
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
            let Some(next) = Self::projected_protect_piece_at(areas, protect, ranges, run.end)
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

    fn projected_protect_run_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        new_flags: B::Flags,
        address: B::Addr,
    ) -> Option<ProjectedProtectRun<'a, B>> {
        let ranges = [PreparedProtectRange {
            start: protect.start,
            end: protect.end,
            flags: new_flags,
        }];
        Self::projected_protect_run_at_ranges(areas, protect, &ranges, address)
    }

    fn projected_uffd_protect_snapshot(
        address_space_id: AddressSpaceId,
        areas: &MemorySet<Backend>,
        mapping_identities: &MappingIdentityIndex,
        protect: VirtAddrRange,
        ranges: &[PreparedProtectRange<VirtAddr, MappingFlags>],
        registration: UffdRegistration,
        fragment: PageRange,
    ) -> AxResult<Option<MappingSnapshot>> {
        let current = Self::uffd_snapshot_for_registration(
            address_space_id,
            areas,
            mapping_identities,
            registration,
        )?;
        let run = Self::projected_protect_run_at_ranges(
            areas,
            protect,
            ranges,
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
        let (long_term_pinnable, writable_file_pin_supported) =
            mapping_user_io_pin_policy(run.left_area.backend());
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
            long_term_pinnable,
            writable_file_pin_supported,
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

    /// Claims this shared mm's OOM reaper for one PTE generation. Completion
    /// returns to idle: later faults can populate new private pages which a
    /// later process_mrelease must be able to reclaim.
    pub(crate) fn begin_oom_reap(&self) -> AxResult<bool> {
        match self
            .oom_reap_state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(true),
            Err(_) => Err(AxError::ResourceBusy),
        }
    }

    /// Releases the reaper ownership claimed above.
    pub(crate) fn finish_oom_reap(&self) {
        self.oom_reap_state.store(0, Ordering::Release);
    }

    /// Merges an already-observed peak into this mm's high-water mark.
    ///
    /// Exec uses this to preserve the process lifetime mark while replacing
    /// the address space; fork uses the same operation for the child's
    /// inherited initial peak.
    pub(crate) fn merge_resident_highwater(&self, resident_kb: u64) -> u64 {
        let mut current = self.maxrss_kb.load(Ordering::Acquire);
        while resident_kb > current {
            match self.maxrss_kb.compare_exchange_weak(
                current,
                resident_kb,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return resident_kb,
                Err(observed) => current = observed,
            }
        }
        current
    }

    /// Publishes the current resident set before a mutation can remove PTEs.
    /// The caller already owns the address-space lock.
    fn publish_resident_highwater(&self) {
        self.merge_resident_highwater(self.resident_user_bytes() as u64 / 1024);
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
        let secret_ranges: Vec<_> = self
            .areas()
            .filter(|area| area.backend().is_secret())
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in secret_ranges {
            self.insert_locked_range(start, end);
        }
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

    /// Returns the strong backing identity and byte offset for a mapped
    /// process-shared futex word.  The caller must hold this address space's
    /// mutex while using the result for a queue operation; a later no-fault
    /// check compares the live mapping against this exact lease to reject
    /// remap/unmap ABA races.
    pub(crate) fn futex_shared_key_at(&self, address: usize) -> Option<SharedFutexKey> {
        let end = address.checked_add(size_of::<u32>())?;
        let area = self.find_area(VirtAddr::from_usize(address))?;
        if address < area.start().as_usize() || end > area.end().as_usize() {
            return None;
        }
        area.backend().futex_shared_key(address)
    }

    /// Gate-safe shared-futex identity lookup.  Unlike key derivation this
    /// returns only copyable identity data and never clones the backing lease.
    pub(crate) fn futex_shared_id_at(
        &self,
        address: usize,
    ) -> Option<(crate::mm::FutexBackingId, crate::mm::FutexWordOffset)> {
        let end = address.checked_add(size_of::<u32>())?;
        let area = self.find_area(VirtAddr::from_usize(address))?;
        if address < area.start().as_usize() || end > area.end().as_usize() {
            return None;
        }
        area.backend().futex_shared_id(address)
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
        let result = self.map_with_lock_state(
            start,
            size,
            flags,
            populate,
            backend,
            self.lock_future_mappings,
        );
        #[cfg(target_arch = "x86_64")]
        if result.is_ok() {
            let _invalidated = self.reconcile_cet_default_shadow_stacks();
        }
        result
    }

    /// Replace one complete shared VMA at the same virtual address with an
    /// alias of its backing at `page_offset`.  The caller holds the address
    /// space mutex for the entire prepared replacement; both the replacement
    /// and rollback backends are fully constructed before the old PTE/VMA is
    /// retired.  Deferred UFFD wakeup is deliberately returned to the caller
    /// and must be finished only after dropping that mutex.
    pub(crate) fn replace_shared_mapping_at_offset(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
        start: VirtAddr,
        size: usize,
        page_offset: usize,
        populate: bool,
    ) -> AxResult<DeferredUffdWake> {
        self.validate_region(start, size)?;
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let area = self.find_area(start).ok_or(AxError::InvalidInput)?;
        if area.start() != start || area.end() < end {
            return Err(AxError::InvalidInput);
        }
        let flags = area.flags();
        let locked = self.range_is_locked(start, size);
        let source = area.backend();
        let shared = source
            .file_mapping()
            .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Shared)
            || matches!(source, Backend::Shared(_));
        if !shared {
            return Err(AxError::InvalidInput);
        }
        // Build both candidates before retiring the old VMA.  Recreating the
        // original through relocate(start,start) retains every cache/lease
        // registration should publication of the replacement fail.
        let rollback = source.relocate(start, start, aspace)?;
        let replacement = match source {
            Backend::File(_) => source.clone_file_rebased(start, page_offset, aspace)?,
            Backend::Shared(_) => source.clone_shared_rebased(start, page_offset)?,
            Backend::Linear(_) | Backend::Cow(_) => return Err(AxError::InvalidInput),
        };
        let wake = self.unmap(start, size)?;
        match self.map_with_lock_state(start, size, flags, populate, replacement, locked) {
            Ok(()) => Ok(wake),
            Err(error) => {
                // The old mapping was retained as a prepared backend, so a
                // failed fixed replacement cannot leave a hole visible after
                // the address-space lock is released.
                self.map_with_lock_state(start, size, flags, false, rollback, locked)
                    .map_err(|_| AxError::BadState)?;
                Err(error)
            }
        }
    }

    /// Snapshot the complete Linux remap_file_pages VMA span.  Every fragment
    /// must be contiguous, shared, carry the same flags and pin the same OFD.
    pub(crate) fn remap_shared_span_snapshot(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<(MappingFlags, FileMappingLease)> {
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        let mut result: Option<(MappingFlags, FileMappingLease)> = None;
        for area in self.areas_overlapping(range) {
            if cursor >= range.end {
                break;
            }
            if area.start() > cursor {
                return Err(AxError::InvalidInput);
            }
            let lease = area.backend().file_mapping().ok_or(AxError::InvalidInput)?;
            if lease.sharing() != FileMappingSharing::Shared {
                return Err(AxError::InvalidInput);
            }
            if let Some((flags, first)) = &result {
                if *flags != area.flags() || first.ofd_key() != lease.ofd_key() {
                    return Err(AxError::InvalidInput);
                }
            } else {
                result = Some((area.flags(), lease.clone()));
            }
            cursor = area.end().min(range.end);
        }
        if cursor != range.end {
            return Err(AxError::InvalidInput);
        }
        result.ok_or(AxError::InvalidInput)
    }

    pub(crate) fn replace_shared_mapping_span_at_offset(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
        start: VirtAddr,
        size: usize,
        page_offset: usize,
        _populate: bool,
    ) -> LockExternalUffdOutcome<(), AxError> {
        let mut deferred_wake = DeferredUffdWake::empty();
        let outcome = (|| -> AxResult {
            let range =
                VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
            self.check_no_seal_overlap(start, size)?;
            self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
            let source_mutations = prepare_unmap_mapping_mutations(
                &self.areas,
                &self.mapping_identities,
                start,
                size,
            )?;
            let next_topology_generation = self.next_topology_generation()?;
            let policy = self.prepare_remap_policy(start, size, size)?;
            // `mlock` can cover only a prefix, suffix, or interior pages of a
            // VMA.  `prepare_remap_policy` snapshots those exact intervals before
            // unmap clears the ledger, so the replacement neither drops the
            // charge nor expands it to the whole VMA.
            let mut cursor = start;
            let mut fragments: Vec<(
                VirtAddr,
                usize,
                MappingFlags,
                MappingLineage,
                Backend,
                Backend,
            )> = Vec::new();
            for area in self.areas_overlapping(range) {
                if cursor >= range.end {
                    break;
                }
                if area.start() > cursor {
                    return Err(AxError::InvalidInput);
                }
                let end = area.end().min(range.end);
                let length = end.sub_addr(cursor);
                let offset = page_offset
                    .checked_add(cursor.sub_addr(start) / PAGE_SIZE_4K)
                    .ok_or(AxError::InvalidInput)?;
                let source = area.backend();
                let shared = source
                    .file_mapping()
                    .is_some_and(|lease| lease.sharing() == FileMappingSharing::Shared);
                if !shared {
                    return Err(AxError::InvalidInput);
                }
                let rollback = source.relocate(cursor, cursor, aspace)?;
                let replacement = match source {
                    Backend::File(_) => source.clone_file_rebased(cursor, offset, aspace)?,
                    Backend::Shared(_) => source.clone_shared_rebased(cursor, offset)?,
                    _ => return Err(AxError::InvalidInput),
                };
                fragments.push((
                    cursor,
                    length,
                    area.flags(),
                    area.lineage(),
                    rollback,
                    replacement,
                ));
                cursor = end;
            }
            if cursor != range.end {
                return Err(AxError::InvalidInput);
            }
            // Arm UFFD only after every backend and rollback candidate has been
            // built.  From here, every failure consumes this plan exactly once.
            // Do not use `unmap()` below: that helper commits its UFFD plan
            // immediately.  A nonlinear replacement must retain both outcomes
            // until the replacement has either fully published or been restored.
            let uffd_plan =
                self.preflight_remap_uffd(UffdRemapKind::Move, true, start, size, start, size)?;
            if let Err(error) = self.unmap_areas_with_tlb_grace(start, size) {
                deferred_wake
                    .merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(error.into());
            }
            self.refresh_growdown_starts();
            Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
            Self::clear_interval(&mut self.dontfork_ranges, start, size);
            self.clear_locked_range(start, size);
            let mut committed = 0usize;
            for (_, _, flags, _, _, replacement) in &fragments {
                let (address, length, ..) = fragments[committed];
                if let Err(error) = self.map_with_lock_state(
                    address,
                    length,
                    *flags,
                    false,
                    replacement.clone(),
                    false,
                ) {
                    let replacement_mutations = prepare_unmap_mapping_mutations(
                        &self.areas,
                        &self.mapping_identities,
                        start,
                        size,
                    )
                    .ok();
                    let restored = self.unmap_areas_with_tlb_grace(start, size).is_ok();
                    if restored {
                        if let Some(replacement_mutations) = replacement_mutations {
                            commit_mapping_identity_mutations(
                                &mut self.mapping_identities,
                                &replacement_mutations,
                            );
                        }
                    }
                    for (address, length, flags, lineage, rollback, _) in fragments.into_iter() {
                        if self
                            .map_with_existing_lineage(
                                address, length, flags, false, rollback, false, lineage,
                            )
                            .is_err()
                        {
                            deferred_wake.merge(self.resolve_remap_uffd(
                                uffd_plan,
                                RemapUffdOutcome::DestructiveFailure,
                            ));
                            self.commit_topology_generation(next_topology_generation);
                            return Err(AxError::BadState);
                        }
                    }
                    if restored {
                        self.apply_remap_policy(start, &policy);
                        deferred_wake
                            .merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                        return Err(error);
                    }
                    deferred_wake.merge(
                        self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::DestructiveFailure),
                    );
                    self.commit_topology_generation(next_topology_generation);
                    return Err(AxError::BadState);
                }
                committed += 1;
            }
            self.apply_remap_policy(start, &policy);
            commit_mapping_identity_mutations(&mut self.mapping_identities, &source_mutations);
            self.commit_topology_generation(next_topology_generation);
            deferred_wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed));
            Ok(())
        })();
        LockExternalUffdOutcome::new(outcome, deferred_wake)
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
        self.publish_resident_highwater();
        let retirement =
            self.areas
                .unmap_deferred_with_limit(start, size, &mut self.pt, MAX_VMA_FRAGMENTS)?;
        self.release_swapped_range(start, size);
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();
        Ok(())
    }

    fn clear_areas_with_tlb_grace(&mut self) -> MappingResult {
        self.publish_resident_highwater();
        let retirement = self.areas.clear_deferred(&mut self.pt)?;
        let swapped: Vec<_> = self.swapped.keys().copied().collect();
        for page in swapped {
            if let Some(entry) = self.swapped.remove(&page) {
                let _ = crate::mm::release(entry);
            }
        }
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();
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
        self.prune_shared_alias_bindings();
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
        self.prune_shared_alias_bindings();
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
            self.check_no_seal_overlap(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;

            let replacing = matches!(destination, RemapDestination::Replace);
            if !replacing && !range_is_empty(&self.areas, destination_start, destination_size) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if replacing {
                self.check_no_seal_overlap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
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
                unmaps,
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
            self.check_no_seal_overlap(source_start, source_size)
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
                self.check_no_seal_overlap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
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
                destination_unmaps,
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
        // All generic COW fault/populate users operate on 4 KiB spans.  Make
        // that representation explicit here, rather than relying on each
        // usercopy, process_vm, or task-fault caller to remember the huge-PMD
        // boundary rule.
        self.ensure_4k_granularity(start, size)?;
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
            // Publish even on a partial-population error: the valid prefix is
            // still a real resident peak and may be rolled back immediately.
            self.publish_resident_highwater();
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
        self.ensure_4k_granularity(start, size)?;
        self.publish_resident_highwater();
        let next_generation = self.next_topology_generation()?;
        // Backend discard can make partial progress before reporting a later
        // hole or backend error. Advance the legacy address-space admission
        // fence first, but deliberately keep per-lineage VMA generations
        // stable: residency is not a mapping-contract change.
        self.commit_topology_generation(next_generation);
        let discard_start = start;
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
        // A software swap PTE is already non-present, so backend `unmap`
        // cannot see it.  Discard is nevertheless an ownership drop.
        self.release_swapped_range(discard_start, start.sub_addr(discard_start));
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(grace);
        self.publish_resident_highwater();
        result
    }

    /// Moves already-resident leaves to the cold end of the local reclaim
    /// policy without faulting or detaching their backing frames.
    ///
    /// x86 records the accessed bit in the PTE rather than in
    /// [`MappingFlags`]. Reinstalling the exact translation clears that
    /// hardware-owned state while retaining all software permissions and the
    /// physical frame. This is the only safe anonymous/shmem COLD operation
    /// available before swap exists: unmapping a COW leaf would release its
    /// sole frame, and unmapping a shared leaf would not make its backing
    /// reclaimable. A later PAGEOUT may reclaim file-cache pages, but has the
    /// Linux no-swap outcome for these retained anonymous leaves.
    pub(crate) fn cold_resident_pages(&mut self, range: VirtAddrRange) -> AxResult<usize> {
        self.validate_region(range.start, range.size())?;
        // COLD/PAGEOUT walk individual translations in order to clear the
        // hardware accessed state.  Do not let a range which starts or ends
        // inside a collapsed shared/file PMD retain that compound leaf: the
        // next partial mprotect/munmap/mremap must see the same 4 KiB
        // geometry, and ensure_4k_granularity performs the alias-preserving
        // demotion before this page-by-page walk observes any PTE.
        self.ensure_4k_granularity(range.start, range.size())?;
        let mut cursor = range.start;
        let mut cooled = 0usize;
        let mut changed = false;
        let result = {
            let mut pt = self.pt.cursor();
            (|| {
                while cursor < range.end {
                    match pt.query(cursor) {
                        Ok((paddr, flags, page_size)) => {
                            let leaf_start = cursor.align_down(page_size);
                            let leaf_end = leaf_start + page_size as usize;
                            let leaf_paddr = paddr.align_down(page_size);
                            pt.remap(leaf_start, leaf_paddr, flags)
                                .map_err(|_| AxError::BadAddress)?;
                            changed = true;
                            let covered_end = leaf_end.min(range.end);
                            cooled = cooled
                                .checked_add(covered_end.sub_addr(cursor))
                                .ok_or(AxError::InvalidInput)?;
                            cursor = covered_end;
                        }
                        Err(PagingError::NotMapped) => cursor += PAGE_SIZE_4K,
                        Err(_) => return Err(AxError::BadAddress),
                    }
                }
                if changed {
                    pt.flush();
                }
                Ok(())
            })()
        };
        if changed {
            // The replacement PTEs retain their frame ownership, but remote
            // CPUs may retain an accessed translation. Finish the required
            // targeted invalidation before exposing the cold state.
            drop(self.synchronize_tlb_after_mutation());
        }
        result?;
        Ok(cooled)
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

    /// Reclaim the PTEs and backing frames the Linux OOM reaper may discard.
    ///
    /// The caller holds the address-space mmap-equivalent lock.  Do not race
    /// active user-I/O pins or userfaultfd registrations: this kernel has no
    /// nonblocking notifier protocol for either, so retaining the complete
    /// image and asking the caller to retry is the only safe outcome.
    /// Private file COW mappings are included, matching Linux's
    /// `vma_is_anonymous(vma) || !(vma->vm_flags & VM_SHARED)` rule.
    pub(crate) fn oom_reap_private_pages(&mut self) -> bool {
        if self.user_io_pins.progress().total() != 0 || self.uffd.is_some() {
            return false;
        }

        let ranges = self
            .areas
            .iter()
            .filter(|area| area.backend().is_oom_reapable_private())
            .map(|area| (area.start(), area.size()))
            .collect::<Vec<_>>();

        for (start, size) in ranges {
            // discard_pages drains PTEs through the backend, waits for the
            // TLB generation grace period, and only then retires frames.
            if self.discard_pages(start, size).is_err() {
                return false;
            }
        }
        true
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
        self.check_no_seal_overlap(start, size)?;
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
        self.ensure_4k_granularity(start, size)?;
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
        self.prune_shared_alias_bindings();
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

    /// Copies from this address space for the task that currently owns it.
    ///
    /// Ordinary pages retain the direct-map fast path. Secret shared pages
    /// have no direct alias, so each VMA/page-sized piece is populated and
    /// copied through its backing's CPU-local secret window instead.
    pub(crate) fn current_uaccess_read(&mut self, start: VirtAddr, buf: &mut [u8]) -> AxResult {
        self.current_uaccess(start, buf, MappingFlags::READ)
    }

    /// See [`Self::current_uaccess_read`].
    pub(crate) fn current_uaccess_write(&mut self, start: VirtAddr, buf: &[u8]) -> AxResult {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.contains_range(start, buf.len()) {
            return Err(AxError::BadAddress);
        }
        let end = start.checked_add(buf.len()).ok_or(AxError::BadAddress)?;
        let mut cursor = start;
        let mut copied = 0;
        while cursor < end {
            let (area_end, area_flags, backend) = {
                let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
                if area.start() > cursor || !area.flags().contains(MappingFlags::WRITE) {
                    return Err(AxError::PermissionDenied);
                }
                (area.end(), area.flags(), area.backend().clone())
            };
            let page_end = cursor + PAGE_SIZE_4K - cursor.align_offset_4k();
            let piece_end = area_end.min(end).min(page_end);
            self.populate_area(cursor.align_down_4k(), PAGE_SIZE_4K, MappingFlags::WRITE)?;
            let piece_len = piece_end - cursor;
            let piece = &buf[copied..copied + piece_len];
            match backend {
                Backend::Shared(shared) if shared.is_secret() => {
                    let offset = shared
                        .backing_offset(cursor.as_usize())
                        .ok_or(AxError::BadAddress)?;
                    shared.pages().write_bytes(offset, piece)?;
                }
                _ => self.write(cursor, piece)?,
            }
            if area_flags.contains(MappingFlags::EXECUTE) {
                synchronize_executable_publication(MappingFlags::EXECUTE);
            }
            cursor = piece_end;
            copied += piece_len;
        }
        Ok(())
    }

    fn current_uaccess(
        &mut self,
        start: VirtAddr,
        buf: &mut [u8],
        access_flags: MappingFlags,
    ) -> AxResult {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.contains_range(start, buf.len()) {
            return Err(AxError::BadAddress);
        }
        let end = start.checked_add(buf.len()).ok_or(AxError::BadAddress)?;
        let mut cursor = start;
        let mut copied = 0;
        while cursor < end {
            let (area_end, backend) = {
                let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
                if area.start() > cursor || !area.flags().contains(access_flags) {
                    return Err(AxError::PermissionDenied);
                }
                (area.end(), area.backend().clone())
            };
            let piece_end = area_end
                .min(end)
                .min(cursor + PAGE_SIZE_4K - cursor.align_offset_4k());
            let page_start = cursor.align_down_4k();
            self.populate_area(page_start, PAGE_SIZE_4K, access_flags)?;
            let piece_len = piece_end - cursor;
            let piece = &mut buf[copied..copied + piece_len];
            match backend {
                Backend::Shared(shared) if shared.is_secret() => {
                    let offset = shared
                        .backing_offset(cursor.as_usize())
                        .ok_or(AxError::BadAddress)?;
                    shared.pages().read_bytes(offset, piece)?;
                }
                _ => self.read(cursor, piece)?,
            }
            cursor = piece_end;
            copied += piece_len;
        }
        Ok(())
    }

    /// Returns whether a range overlaps a secret-memory VMA.  Such frames
    /// must never be accessed through the generic direct-map copy helpers.
    pub(crate) fn has_secret_mapping(&self, start: VirtAddr, len: usize) -> bool {
        len != 0
            && start.checked_add(len).is_some_and(|end| {
                self.areas.iter().any(|area| {
                    area.start() < end && start < area.end() && area.backend().is_secret()
                })
            })
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
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        self.prepare_protect_ranges(
            start,
            size,
            vec![PreparedProtectRange { start, end, flags }],
        )
    }

    /// Prepares one atomic pkey_mprotect transaction.  READ_IMPLIES_EXEC is
    /// evaluated against each source VMA because an executable personality is
    /// suppressed for file mappings on noexec mounts.
    pub(crate) fn prepare_pkey_protect(
        &mut self,
        start: VirtAddr,
        size: usize,
        requested: MappingFlags,
        key: u8,
        read_implies_exec: bool,
    ) -> AxResult<PreparedProtect<'_>> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut ranges = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.areas.find(cursor) else {
                return Err(AxError::NoMemory);
            };
            if area.start() > cursor {
                return Err(AxError::NoMemory);
            }
            ranges.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let may_execute = area
                .backend()
                .file_mapping()
                .is_none_or(|mapping| mapping.may_protect().contains(MappingFlags::EXECUTE));
            let mut flags = requested;
            if area.flags().contains(MappingFlags::SHADOW_STACK) {
                // pkey_mprotect may rekey a shadow stack, but never turns it
                // into a conventional writable/executable mapping.
                if requested != MappingFlags::READ {
                    return Err(AxError::InvalidInput);
                }
                flags = (flags - MappingFlags::EXECUTE) | MappingFlags::SHADOW_STACK;
            }
            if read_implies_exec && may_execute && flags.contains(MappingFlags::READ) {
                flags |= MappingFlags::EXECUTE;
            }
            if area.flags().contains(MappingFlags::SHADOW_STACK) {
                flags -= MappingFlags::EXECUTE;
            }
            let segment_end = area.end().min(end);
            ranges.push(PreparedProtectRange {
                start: cursor,
                end: segment_end,
                flags: flags.with_pkey(key),
            });
            cursor = segment_end;
        }
        self.prepare_protect_ranges(start, size, ranges)
    }

    fn prepare_protect_ranges(
        &mut self,
        start: VirtAddr,
        size: usize,
        ranges: Vec<PreparedProtectRange<VirtAddr, MappingFlags>>,
    ) -> AxResult<PreparedProtect<'_>> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Protect)?;
        for range in &ranges {
            self.check_protect_range(range.start, range.end.sub_addr(range.start), range.flags)?;
        }
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
            tlb,
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
                    ranges.as_slice(),
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
            ranges,
            max_areas: MAX_VMA_FRAGMENTS,
        };
        let synchronize_instruction_stream = transaction
            .segments()
            .any(|(area, _, _, flags)| adds_execute_permission(area.flags(), flags));

        Ok(PreparedProtect {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            tlb,
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
        // Keep the registry exact even when the Arc survives an exec image
        // replacement.  Otherwise a later cross-mm transaction needlessly
        // locks this unrelated mm (and an identity reset makes snapshots
        // impossible to revalidate).
        self.alias_bindings.clear();
        drop(core::mem::take(&mut self.mapping_identities));
        self.growdown_starts.clear();
        self.wipe_on_fork_ranges.clear();
        self.dontfork_ranges.clear();
        self.locked_ranges.clear();
        debug_assert!(self.active_long_term_cow_pins.is_empty());
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

    /// Reclaims one exclusively-owned 4 KiB anonymous leaf.  The present PTE
    /// is first invalidated and globally quiesced, so a concurrent CPU cannot
    /// modify bytes while they are copied to swap.  An I/O failure restores
    /// the original leaf before returning.
    pub(crate) fn reclaim_one_anonymous_page(&mut self) -> AxResult<bool> {
        let candidate = self.areas.iter().find_map(|area| {
            if area.backend().page_size() != PageSize::Size4K {
                return None;
            }
            let mut page = area.start();
            while page < area.end() {
                if let Ok((paddr, _, PageSize::Size4K)) = self.pt.query(page)
                    && area.backend().swap_reclaimable(paddr)
                {
                    return Some((page, paddr, area.flags(), area.backend().clone()));
                }
                page += PAGE_SIZE_4K;
            }
            None
        });
        let Some((page, paddr, _flags, backend)) = candidate else {
            return Ok(false);
        };
        // A pinned frame may still be modified by in-flight DMA.  Deferring
        // allocator reuse is insufficient: the persisted image would already
        // be stale, so reclaim must reject the victim before pageout.
        self.check_no_user_io_pin_overlap(page, PAGE_SIZE_4K, InvalidationReason::Discard)?;
        let (_, leaf_flags, leaf_size) = self.pt.cursor().unmap(page).map_err(AxError::from)?;
        if leaf_size != PageSize::Size4K {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        let bytes =
            unsafe { core::slice::from_raw_parts(phys_to_virt(paddr).as_ptr(), PAGE_SIZE_4K) };
        let entry = match crate::mm::pageout(bytes) {
            Ok(entry) => entry,
            Err(error) => {
                self.pt
                    .cursor()
                    .map(page, paddr, PageSize::Size4K, leaf_flags)
                    .map_err(AxError::from)?;
                drop(self.synchronize_tlb_after_mutation());
                return Err(error);
            }
        };
        self.swapped.insert(page, entry);
        let grace = self.synchronize_tlb_after_mutation();
        backend.release_swapped_frame(paddr);
        drop(grace);
        Ok(true)
    }

    /// Captures and pins all target entries while the caller holds this mm
    /// lock. Allocation and I/O are deliberately deferred to `prepare`.
    pub(crate) fn snapshot_swapoff_area(&self, area: u16) -> AxResult<Vec<SwapoffPage>> {
        let count = self
            .swapped
            .values()
            .filter(|entry| entry.area() == area)
            .count();
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for (page, entry) in self
            .swapped
            .iter()
            .filter(|(_, entry)| entry.area() == area)
        {
            let mapping = self.areas.find(*page).ok_or(AxError::BadState)?;
            if !mapping.backend().supports_uffd_missing_resolver() {
                return Err(AxError::BadState);
            }
            crate::mm::retain(*entry)?;
            pages.push(SwapoffPage {
                page: *page,
                entry: *entry,
            });
        }
        Ok(pages)
    }

    /// Validates the complete preflight set while all live MM locks are held.
    /// No page-table state changes here, allowing the caller to abandon every
    /// prepared page with zero migration on any mismatch.
    pub(crate) fn validate_swapoff_pages(&self, pages: &[PreparedSwapoffPage]) -> AxResult<()> {
        for page in pages {
            match self.swapped.get(&page.page) {
                Some(entry) if *entry == page.entry => {
                    let mapping = self.areas.find(page.page).ok_or(AxError::BadState)?;
                    if !mapping.backend().supports_uffd_missing_resolver() {
                        return Err(AxError::BadState);
                    }
                }
                // A fault may have restored the entry after snapshot. That
                // already satisfies swapoff and only leaves our temporary pin.
                None => {}
                Some(_) => return Err(AxError::BadState),
            }
        }
        Ok(())
    }

    /// Infallible half of the global swapoff transaction. Validation and all
    /// allocation precede this call while all MM locks remain held.
    pub(crate) fn commit_swapoff_pages(&mut self, pages: &mut [PreparedSwapoffPage]) {
        for page in pages {
            if self.swapped.get(&page.page) != Some(&page.entry) {
                continue;
            }
            let mapping = self
                .areas
                .find(page.page)
                .expect("validated swapoff VMA vanished");
            let backend = mapping.backend().clone();
            let flags = mapping.flags();
            backend
                .publish_prepared_cow_page(page.page, flags, &mut self.pt, &mut page.prepared)
                .expect("validated swapoff publication consumed preallocated resources");
            self.swapped.remove(&page.page);
            crate::mm::release(page.entry).expect("validated swapoff entry disappeared");
            self.publish_resident_highwater();
        }
    }

    fn release_swapped_range(&mut self, start: VirtAddr, size: usize) {
        let end = start + size;
        let pages: Vec<_> = self
            .swapped
            .range(start..end)
            .map(|(page, entry)| (*page, *entry))
            .collect();
        for (page, entry) in pages {
            self.swapped.remove(&page);
            let _ = crate::mm::release(entry);
        }
    }

    /// Stages non-present anonymous software PTEs at an mremap destination.
    /// This always takes a destination reference.  A moving transaction keeps
    /// its source reference until its normal source-unmap commit, making a
    /// failed staged move rollback-safe without a special restore path.
    pub(crate) fn relocate_swapped_entries(
        &mut self,
        source: VirtAddr,
        destination: VirtAddr,
        size: usize,
    ) -> AxResult {
        let end = source + size;
        let entries: Vec<_> = self
            .swapped
            .range(source..end)
            .map(|(page, entry)| (*page, *entry))
            .collect();
        for (page, entry) in entries {
            let destination_page = destination + page.sub_addr(source);
            crate::mm::retain(entry)?;
            if let Some(displaced) = self.swapped.insert(destination_page, entry) {
                // A destination is required to be empty by the remap
                // transaction.  Treat a violation as ownership corruption
                // rather than silently releasing an unrelated swap PTE.
                let _ = crate::mm::release(entry);
                self.swapped.insert(destination_page, displaced);
                return Err(AxError::AlreadyExists);
            }
        }
        Ok(())
    }

    /// Checks whether this trap still names a real missing/unsatisfied leaf.
    /// The eventual minor/major classification is deliberately deferred until
    /// backend population completes, when the task's backing-read counter can
    /// prove that storage I/O actually occurred.
    pub(crate) fn fault_needs_accounting(
        &self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
    ) -> bool {
        let Some(area) = self.areas.find(vaddr) else {
            return false;
        };
        if !area.flags().contains(access_flags) {
            return false;
        }
        if self.swapped.contains_key(&vaddr.align_down(PAGE_SIZE_4K)) {
            return true;
        }
        match self.pt.query(vaddr.align_down(PAGE_SIZE_4K)) {
            Ok((_paddr, page_flags, _page_size)) => {
                if present_leaf_satisfies_fault(page_flags, access_flags) {
                    return false;
                }
                true
            }
            Err(PagingError::NotMapped) => true,
            Err(_) => false,
        }
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
                // A CET access is permissioned by SHADOW_STACK, but still
                // has write semantics for private-COW allocation/copy.
                let populate_access = if access_flags.contains(MappingFlags::SHADOW_STACK) {
                    access_flags | MappingFlags::WRITE
                } else {
                    access_flags
                };
                let page = vaddr.align_down(PAGE_SIZE_4K);
                if let Some(entry) = self.swapped.get(&page).copied() {
                    let backend = area.backend().clone();
                    let restored = {
                        let mut cursor = self.pt.cursor();
                        backend.restore_swapped_page(page, flags, entry, &mut cursor)
                    };
                    match restored {
                        Ok(()) => {
                            self.swapped.remove(&page);
                            self.publish_resident_highwater();
                            return PageFaultResult::Handled;
                        }
                        Err(error) if error.canonicalize() == AxError::NoMemory => {
                            return PageFaultResult::Failed(PageFaultFailure::OutOfMemory);
                        }
                        Err(_) => {
                            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
                        }
                    }
                }
                let page_size = area.backend().page_size();
                let start = vaddr.align_down(page_size);
                if area.backend().faults_with_sigbus(start) {
                    return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
                }
                let fault_around = area.backend().fault_around_size(populate_access);
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
                    populate_access,
                    &mut self.pt.cursor(),
                );
                let populate_result = populate_outcome.finish(self);
                // Synchronize even on error: a multi-page populate may have
                // installed a valid executable prefix before the failure.
                synchronize_executable_publication(flags);
                self.publish_resident_highwater();
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
        let fault_candidate = self.fault_needs_accounting(vaddr, access_flags);
        let read_before = axtask::current()
            .try_as_thread()
            .map(|thread| thread.backing_read_bytes());
        let handled = matches!(
            self.handle_page_fault_result(vaddr, access_flags, None),
            PageFaultResult::Handled
        );
        if handled && fault_candidate {
            if let (Some(thread), Some(read_before)) =
                (axtask::current().try_as_thread(), read_before)
            {
                thread.account_resolved_page_fault(read_before);
            }
        }
        handled
    }

    /// Attempts to clone the current address space into a new one.
    ///
    /// This method creates a new empty address space with the same base and
    /// size, then iterates over all memory areas in the original address
    /// space to copy or share their mappings into the new one.
    pub fn try_clone(&mut self) -> AxResult<Arc<Mutex<Self>>> {
        if self.user_io_pins.has_clone_blocker() {
            return Err(AxError::ResourceBusy);
        }
        if self.fork_fragment_count()? > MAX_VMA_FRAGMENTS {
            return Err(AxError::NoMemory);
        }
        // Resolve owner-aware physical identities before any parent PTE is
        // COW-protected. Allocation failure therefore leaves fork entirely
        // unpublished and the parent untouched.
        let active_long_term_cow_frames = self.active_long_term_cow_frames()?;

        let new_aspace = Arc::new(Mutex::new(Self::new_empty(self.base(), self.size())?));
        crate::mm::register_pending_address_space(&new_aspace);
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
        // Bind every shared source backing before clone_map can make a parent
        // COW-visible change.  The child is not published yet, so a later
        // failure drops these leases with the unpublished child; a successful
        // final sync removes any DONTFORK-only provisional bindings.
        let mut fork_shared_keys = Vec::new();
        fork_shared_keys
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if let Some(key) = area.backend().shared_backing_key() {
                fork_shared_keys.push(key);
            }
        }
        fork_shared_keys.sort_unstable();
        fork_shared_keys.dedup();
        // The child has no VMA yet.  Reserve every potential shared backing
        // as pending so a cross-mm folio transaction cannot snapshot between
        // clone_map publication and the final child registry commit.
        let mut fork_pending_aliases = Vec::new();
        fork_pending_aliases
            .try_reserve_exact(fork_shared_keys.len())
            .map_err(|_| AxError::NoMemory)?;
        for key in fork_shared_keys {
            fork_pending_aliases.push(PendingAliasLease::try_prepare(
                key,
                &new_aspace_clone,
                guard.address_space_id,
            )?);
        }
        // Linux carries private membarrier registrations across an ordinary
        // fork, but the child starts with no CPUs resident and a fresh
        // barrier generation. CLONE_VM shares the address space (and hence
        // this state) through the existing Arc path instead.
        guard.tlb = self.tlb.fork_clone()?;
        if let Some(old) = self.tlb.snapshot_ldt() {
            guard.tlb.replace_ldt(Some(
                Arc::try_new(old.copy()?).map_err(|_| AxError::NoMemory)?,
            ));
        }
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
                    let child_backend =
                        wipe_on_fork_backend(cursor, page_size, area.backend().is_sealed());
                    let new_area = MemoryArea::new_with_lineage(
                        cursor,
                        wipe_size,
                        area.flags(),
                        child_backend,
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
                            &active_long_term_cow_frames,
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
        // Present private leaves were handled by `clone_map`; copy software
        // swap PTEs separately and take one slot reference for the child.
        // DONTFORK drops the child ownership and WIPEONFORK intentionally
        // starts absent/zero-filled.
        for (page, entry) in &self.swapped {
            if Self::interval_end_covering(&dontfork_ranges, *page).is_some()
                || Self::interval_end_covering(&wipe_on_fork_ranges, *page).is_some()
            {
                continue;
            }
            crate::mm::retain(*entry)?;
            guard.swapped.insert(*page, *entry);
        }
        // Secret VMAs are unconditionally mlocked.  Reconstruct this child
        // sidecar from VMAs that were actually cloned, rather than copying
        // the parent's ranges: MADV_DONTFORK holes have no child mapping and
        // ordinary mlock state is not inherited by fork.
        let child_secret_ranges: Vec<_> = guard
            .areas
            .iter()
            .filter(|area| area.backend().is_secret())
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in child_secret_ranges {
            guard.insert_locked_range(start, end);
        }
        guard.refresh_growdown_starts();
        for pending in fork_pending_aliases.drain(..) {
            guard.commit_shared_alias_binding(pending);
        }
        guard.sync_shared_alias_bindings(&new_aspace_clone)?;
        // A forked mm starts with the child's currently resident pages as its
        // initial peak; unlike CLONE_VM this is a distinct address space.
        guard.publish_resident_highwater();
        debug_assert!(
            guard.areas.iter().all(|area| mapping_identity(
                &guard.mapping_identities,
                area.lineage()
            )
            .is_ok())
        );
        drop(self_modify);
        drop(self.synchronize_tlb_after_mutation());
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

    #[cfg(target_arch = "x86_64")]
    fn cet_shadow_stack_extent_at(&self, address: VirtAddr) -> Option<(VirtAddr, usize)> {
        let area = self.find_area(address)?;
        area.flags()
            .contains(MappingFlags::SHADOW_STACK)
            .then_some((area.start(), area.size()))
    }

    /// Registers the task-owned default stack only after its VMA is live.
    /// The caller retains responsibility for undoing the just-created VMA if
    /// this fallible publication cannot reserve its registry slot.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_with_ownership(
            task_id,
            start,
            size,
            CetDefaultShadowStackOwnership::Owned,
        )
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_borrowed_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_with_ownership(
            task_id,
            start,
            size,
            CetDefaultShadowStackOwnership::Borrowed,
        )
    }

    #[cfg(target_arch = "x86_64")]
    fn register_cet_default_shadow_stack_with_ownership(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
        ownership: CetDefaultShadowStackOwnership,
    ) -> AxResult {
        if size == 0
            || self
                .cet_default_shadow_stacks
                .iter()
                .any(|owner| owner.task_id == task_id)
            || self.cet_shadow_stack_extent_at(start) != Some((start, size))
        {
            return Err(AxError::InvalidInput);
        }
        self.cet_default_shadow_stacks
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.cet_default_shadow_stacks
            .push(CetDefaultShadowStackOwner {
                task_id,
                start,
                size,
                ownership,
            });
        Ok(())
    }

    /// Removes one owner without touching VMAs.  Exec uses this before the
    /// image handoff; exit uses `detach_*` below to remove its private VMA.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn take_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
    ) -> Option<CetDefaultShadowStackOwner> {
        let index = self
            .cet_default_shadow_stacks
            .iter()
            .position(|owner| owner.task_id == task_id)?;
        Some(self.cet_default_shadow_stacks.swap_remove(index))
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_default_shadow_stack(
        &self,
        task_id: u32,
    ) -> Option<CetDefaultShadowStackOwner> {
        self.cet_default_shadow_stacks
            .iter()
            .copied()
            .find(|owner| owner.task_id == task_id)
    }

    /// Default CET stacks are task SSP leases.  Moving, resizing, or fixed
    /// replacement cannot safely update every CLONE_VM peer atomically, so
    /// callers reject any overlap before altering VMAs.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_default_shadow_stack_intersects(&self, start: VirtAddr, size: usize) -> bool {
        let Some(end) = start.checked_add(size) else {
            return true;
        };
        self.cet_default_shadow_stacks.iter().any(|owner| {
            owner
                .start
                .checked_add(owner.size)
                .is_none_or(|owner_end| start < owner_end && owner.start < end)
        })
    }

    /// Retires an owner only after its VMA transaction succeeds. Borrowed
    /// vfork aliases name the parent's VMA and therefore only lose the alias.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn retire_cet_default_shadow_stack(&mut self, task_id: u32) {
        let Some(owner) = self.cet_default_shadow_stack(task_id) else {
            return;
        };
        if owner.ownership == CetDefaultShadowStackOwnership::Owned {
            let Ok(wake) = self.unmap(owner.start, owner.size) else {
                return;
            };
            let _ = self.take_cet_default_shadow_stack(task_id);
            wake.finish();
        } else {
            let _ = self.take_cet_default_shadow_stack(task_id);
        }
    }

    /// Tests the current PL3_SSP against this task's still-live default CET
    /// VMA.  This is intentionally an MM-only lease check: callers perform
    /// task-local state retirement only after dropping the address-space lock.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_default_shadow_stack_contains(&self, task_id: u32, ssp: u64) -> bool {
        let Some(owner) = self.cet_default_shadow_stack(task_id) else {
            return false;
        };
        let Some(end) = owner.start.as_usize().checked_add(owner.size) else {
            return false;
        };
        let ssp = ssp as usize;
        ssp >= owner.start.as_usize()
            && ssp <= end
            && self.cet_shadow_stack_extent_at(owner.start) == Some((owner.start, owner.size))
    }

    /// Performs the all-or-nothing kernel side of a CET signal push.  The
    /// VMA and every touched leaf are validated before any word is written;
    /// SHADOW_STACK mappings are intentionally written through this narrow
    /// kernel authority rather than general usercopy.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn write_cet_signal_frame(
        &mut self,
        task_id: u32,
        saved_ssp: u64,
        words: [u64; 3],
    ) -> AxResult<u64> {
        let bytes = core::mem::size_of_val(&words);
        let start = (saved_ssp as usize)
            .checked_sub(bytes)
            .ok_or(AxError::BadAddress)?;
        if !saved_ssp.is_multiple_of(core::mem::size_of::<u64>() as u64)
            || !self.cet_default_shadow_stack_contains(task_id, saved_ssp)
            || !self.cet_default_shadow_stack_contains(task_id, start as u64)
        {
            return Err(AxError::BadAddress);
        }
        let first_page = start & !(PAGE_SIZE_4K - 1);
        let last_page = (saved_ssp as usize - 1) & !(PAGE_SIZE_4K - 1);
        let population = last_page
            .checked_sub(first_page)
            .and_then(|delta| delta.checked_add(PAGE_SIZE_4K))
            .ok_or(AxError::BadAddress)?;
        self.populate_area(VirtAddr::from(first_page), population, MappingFlags::READ)?;
        for address in [start, saved_ssp as usize - core::mem::size_of::<u64>()] {
            let (_, flags, _) = self.page_table().query(VirtAddr::from(address))?;
            if !flags.contains(MappingFlags::USER) || !flags.contains(MappingFlags::SHADOW_STACK) {
                return Err(AxError::BadAddress);
            }
        }
        let mut bytes = [0u8; core::mem::size_of::<[u64; 3]>()];
        for (index, word) in words.into_iter().enumerate() {
            bytes[index * 8..(index + 1) * 8].copy_from_slice(&word.to_ne_bytes());
        }
        self.write(VirtAddr::from(start), &bytes)?;
        Ok(start as u64)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn read_cet_signal_frame(
        &self,
        task_id: u32,
        shadow_start: u64,
    ) -> AxResult<[u64; 3]> {
        let end = shadow_start
            .checked_add(core::mem::size_of::<[u64; 3]>() as u64)
            .ok_or(AxError::BadAddress)?;
        if !shadow_start.is_multiple_of(core::mem::size_of::<u64>() as u64)
            || !self.cet_default_shadow_stack_contains(task_id, shadow_start)
            || !self.cet_default_shadow_stack_contains(task_id, end)
        {
            return Err(AxError::BadAddress);
        }
        let mut bytes = [0u8; core::mem::size_of::<[u64; 3]>()];
        self.read(VirtAddr::from(shadow_start as usize), &mut bytes)?;
        let mut words = [0u64; 3];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u64::from_ne_bytes(bytes[index * 8..(index + 1) * 8].try_into().unwrap());
        }
        Ok(words)
    }

    /// Reconciles every owner in this mm after an arbitrary successful VMA
    /// mutation.  The returned task ids are the only information callers may
    /// use to clear task-local CET signal metadata; this avoids taking task
    /// locks while holding the mm lock.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn reconcile_cet_default_shadow_stacks(&mut self) -> Vec<u32> {
        let mut invalidated = Vec::new();
        let mut index = 0;
        while index < self.cet_default_shadow_stacks.len() {
            let owner = self.cet_default_shadow_stacks[index];
            if let Some((start, size)) = self.cet_shadow_stack_extent_at(owner.start) {
                self.cet_default_shadow_stacks[index].start = start;
                self.cet_default_shadow_stacks[index].size = size;
                index += 1;
            } else {
                invalidated.push(owner.task_id);
                self.cet_default_shadow_stacks.swap_remove(index);
            }
        }
        invalidated
    }

    /// Reconciles stack records after mremap while still inside its VMA
    /// transaction. A moved owner's start follows its source offset; all
    /// unrelated owners are checked against the resulting live VMAs too,
    /// which covers MREMAP_FIXED replacement in a shared mm.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn reconcile_cet_default_shadow_stacks_after_mremap(
        &mut self,
        source: VirtAddr,
        old_size: usize,
        destination: VirtAddr,
    ) -> Vec<u32> {
        let source_end = source.checked_add(old_size);
        for owner in &mut self.cet_default_shadow_stacks {
            if source_end.is_some_and(|end| source <= owner.start && owner.start < end) {
                if let Some(offset) = owner.start.as_usize().checked_sub(source.as_usize())
                    && let Some(start) = destination.checked_add(offset)
                {
                    owner.start = start;
                }
            }
        }
        self.reconcile_cet_default_shadow_stacks()
    }

    pub(crate) fn shared_backing_key_at(&self, address: VirtAddr) -> Option<SharedBackingKey> {
        self.find_area(address)?.backend().shared_backing_key()
    }

    pub(crate) fn shared_pages_at(&self, address: VirtAddr) -> Option<Arc<SharedPages>> {
        self.find_area(address)?.backend().shared_pages().cloned()
    }

    pub(crate) fn shared_backing_offset_at(&self, address: VirtAddr) -> Option<usize> {
        match self.find_area(address)?.backend() {
            Backend::Shared(shared) => shared.backing_offset(address.as_usize()),
            Backend::Linear(_) | Backend::Cow(_) | Backend::File(_) => None,
        }
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
        debug_assert!(self.active_long_term_cow_pins.is_empty());
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

    #[test]
    fn address_space_keeps_the_fixed_pin_ledger_off_stack() {
        assert!(core::mem::size_of::<UserIoPinRegistry>() > PAGE_SIZE_4K);
        assert!(core::mem::size_of::<AddrSpace>() <= PAGE_SIZE_4K);
    }

    #[test]
    fn cold_resident_pages_preserves_anonymous_frames() {
        let start = VirtAddr::from(0x1000);
        let mut aspace =
            AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE - 0x1000).unwrap();
        let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        aspace
            .map(
                start,
                PAGE_SIZE_4K * 2,
                flags,
                true,
                Backend::new_alloc(start, PageSize::Size4K),
            )
            .unwrap();
        let before = aspace.page_table().query(start).unwrap().0;

        assert_eq!(
            aspace.cold_resident_pages(VirtAddrRange::from_start_size(start, PAGE_SIZE_4K * 2,)),
            Ok(PAGE_SIZE_4K * 2)
        );

        // No-swap advice must not detach private frames: both leaves stay
        // present and retain their original physical identity.
        assert_eq!(aspace.page_table().query(start).unwrap().0, before);
        assert!(aspace.page_table().query(start + PAGE_SIZE_4K).is_ok());
    }

    fn eligible_collapse_2m_facts() -> Collapse2MCandidateFacts {
        Collapse2MCandidateFacts {
            start: COLLAPSE_2M_SIZE,
            length: COLLAPSE_2M_SIZE,
            vma_covers_range: true,
            private_cow: true,
            has_uffd_write_protect: false,
            has_locked_pages: false,
            has_exact_long_term_cow_pin: false,
            has_fork_policy: false,
        }
    }

    #[test]
    fn collapse_2m_eligibility_requires_one_aligned_private_cow_vma() {
        let facts = eligible_collapse_2m_facts();
        assert!(collapse_2m_candidate_eligible(facts));

        let mut unaligned = facts;
        unaligned.start += PAGE_SIZE_4K;
        assert!(!collapse_2m_candidate_eligible(unaligned));

        let mut partial = facts;
        partial.length -= PAGE_SIZE_4K;
        assert!(!collapse_2m_candidate_eligible(partial));

        let mut crosses_vma = facts;
        crosses_vma.vma_covers_range = false;
        assert!(!collapse_2m_candidate_eligible(crosses_vma));

        let mut non_private = facts;
        non_private.private_cow = false;
        assert!(!collapse_2m_candidate_eligible(non_private));
    }

    #[test]
    fn collapse_2m_eligibility_rejects_each_vma_sidecar_boundary() {
        let facts = eligible_collapse_2m_facts();

        let mut uffd = facts;
        uffd.has_uffd_write_protect = true;
        assert!(!collapse_2m_candidate_eligible(uffd));

        let mut locked = facts;
        locked.has_locked_pages = true;
        assert!(!collapse_2m_candidate_eligible(locked));

        let mut pinned = facts;
        pinned.has_exact_long_term_cow_pin = true;
        assert!(!collapse_2m_candidate_eligible(pinned));

        let mut fork_policy = facts;
        fork_policy.has_fork_policy = true;
        assert!(!collapse_2m_candidate_eligible(fork_policy));
    }

    #[test]
    fn resident_highwater_is_mm_owned_and_monotonic() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE).unwrap();
        assert_eq!(aspace.merge_resident_highwater(12), 12);
        assert_eq!(aspace.merge_resident_highwater(7), 12);
        assert_eq!(aspace.merge_resident_highwater(19), 19);
    }

    #[test]
    fn oom_reap_drains_private_cow_ptes_but_keeps_vmas() {
        let start = VirtAddr::from(0x1000);
        let mut aspace = AddrSpace::new_empty(start, TEST_SPACE_SIZE).unwrap();
        aspace
            .map(
                start,
                PAGE_SIZE_4K * 2,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                true,
                Backend::new_alloc(start, PageSize::Size4K),
            )
            .unwrap();
        assert_eq!(aspace.current_mapping_bytes(), PAGE_SIZE_4K * 2);
        assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K * 2);

        assert!(aspace.oom_reap_private_pages());
        assert_eq!(aspace.current_mapping_bytes(), PAGE_SIZE_4K * 2);
        assert_eq!(aspace.resident_user_bytes(), 0);
        // A completed reaper pass is idempotent at the page-table layer.
        assert!(aspace.oom_reap_private_pages());

        // Completion is not a permanent OOM_SKIP: a later fault generation
        // can populate and then be drained by another pass.
        aspace
            .populate_area(
                start,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            )
            .unwrap();
        assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K);
        assert!(aspace.oom_reap_private_pages());
        assert_eq!(aspace.resident_user_bytes(), 0);
    }

    #[test]
    fn oom_reaper_mm_owner_is_exactly_once_and_retryable() {
        let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE).unwrap();
        assert_eq!(aspace.begin_oom_reap(), Ok(true));
        assert_eq!(aspace.begin_oom_reap(), Err(AxError::ResourceBusy));
        aspace.finish_oom_reap();
        assert_eq!(aspace.begin_oom_reap(), Ok(true));
        aspace.finish_oom_reap();
        // A new fault generation may be reaped by a subsequent caller.
        assert_eq!(aspace.begin_oom_reap(), Ok(true));
        aspace.finish_oom_reap();
    }

    #[test]
    fn cold_demotes_a_shared_huge_leaf_before_walking_partial_range() {
        let start = VirtAddr::from(COLLAPSE_2M_SIZE);
        let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let pages = Arc::new(SharedPages::new(COLLAPSE_2M_SIZE, PageSize::Size2M).unwrap());
        let mut aspace = AddrSpace::new_empty(start, COLLAPSE_2M_SIZE).unwrap();
        aspace
            .map(
                start,
                COLLAPSE_2M_SIZE,
                flags,
                true,
                Backend::new_shared(start, pages),
            )
            .unwrap();
        let source = aspace.page_table().query(start).unwrap().0;

        assert_eq!(
            aspace.cold_resident_pages(VirtAddrRange::from_start_size(
                start + PAGE_SIZE_4K,
                PAGE_SIZE_4K
            )),
            Ok(PAGE_SIZE_4K)
        );
        assert_eq!(
            aspace.page_table().query(start).unwrap().2,
            PageSize::Size4K
        );
        assert_eq!(
            aspace.page_table().query(start + PAGE_SIZE_4K).unwrap().0,
            source + PAGE_SIZE_4K
        );

        // This host test has no task context for SharedPages reclamation.
        core::mem::forget(aspace);
    }

    #[test]
    fn wipe_on_fork_child_keeps_parent_seal_without_its_backing() {
        let child = wipe_on_fork_backend(VirtAddr::from(0x4000), PageSize::Size4K, true);
        assert!(child.is_sealed());
        assert!(child.is_private_anonymous());
        assert!(child.file_mapping().is_none());
    }

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

    #[test]
    fn long_term_pin_policy_follows_exact_lower_owner_capability() {
        let start = VirtAddr::from(0x4000);
        let cow = Backend::new_alloc(start, PageSize::Size4K);
        let linear = Backend::new_linear(start, PhysAddr::from(0x8000), PAGE_SIZE_4K);

        assert_eq!(mapping_user_io_pin_policy(&cow), (true, false));
        assert_eq!(mapping_user_io_pin_policy(&linear), (false, false));
    }

    #[test]
    fn anonymous_shared_long_term_pin_does_not_claim_file_writeback() {
        let shared = Backend::new_shared(
            VirtAddr::from(0x4000),
            Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap()),
        );
        assert_eq!(mapping_user_io_pin_policy(&shared), (true, false));

        // SharedPages teardown takes the kernel mutex; the host unit-test
        // environment has no current task even for this zero-page fixture.
        core::mem::forget(shared);
    }

    #[test]
    fn tlb_state_admits_before_generation_repair_and_bounds_membership() {
        let state = TlbState::new();
        assert!(
            state
                .resident_cpus
                .iter()
                .all(|cpu| !cpu.load(Ordering::Relaxed))
        );
        assert!(
            state
                .seen_generations
                .iter()
                .all(|generation| { generation.load(Ordering::Relaxed) == 0 })
        );

        state.generation.store(4, Ordering::SeqCst);
        assert_eq!(state.admit_cpu(0), Some(4));
        assert!(state.resident_cpus[0].load(Ordering::SeqCst));
        assert_eq!(state.seen_generations[0].load(Ordering::SeqCst), 0);

        state.seen_generations[0].store(4, Ordering::SeqCst);
        assert_eq!(state.admit_cpu(0), None);
        assert!(state.resident_cpus[0].load(Ordering::SeqCst));
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
        let mut ranges = Vec::with_capacity(VMA_COUNT / 64);

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
                ranges: vec![PreparedProtectRange {
                    start: VirtAddr::from(0x2000),
                    end: VirtAddr::from(0x3000),
                    flags: 3,
                }],
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
        // AddrSpace PageTable; runtime architecture gates cover that final
        // wiring.
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
                ranges: vec![PreparedProtectRange {
                    start: VirtAddr::from(0x2000),
                    end: VirtAddr::from(0x3000),
                    flags: 3,
                }],
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
            ranges: vec![
                PreparedProtectRange {
                    start: VirtAddr::from(0x1800),
                    end: VirtAddr::from(0x2000),
                    flags: 5,
                },
                PreparedProtectRange {
                    start: VirtAddr::from(0x2000),
                    end: VirtAddr::from(0x2800),
                    flags: 7,
                },
            ],
            max_areas: usize::MAX,
        };
        let segments: Vec<_> = plan
            .segments()
            .map(|(area, affected_start, affected_end, flags)| {
                (
                    area.start().as_usize(),
                    area.end().as_usize(),
                    affected_start.as_usize(),
                    affected_end.as_usize(),
                    area.flags(),
                    area.backend().0,
                    flags,
                )
            })
            .collect();
        assert_eq!(
            segments,
            vec![
                (0x1000, 0x2000, 0x1800, 0x2000, 1, 1, 5),
                (0x2000, 0x3000, 0x2000, 0x2800, 3, 2, 7),
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
            ranges: vec![PreparedProtectRange {
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 3,
            }],
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
            ranges: vec![PreparedProtectRange {
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 1,
            }],
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
            ranges: vec![PreparedProtectRange {
                start: VirtAddr::from(0x1000),
                end: VirtAddr::from(0x4000),
                flags: 9,
            }],
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
                                    ranges: vec![PreparedProtectRange {
                                        start: protect.start,
                                        end: protect.end,
                                        flags: new_flags,
                                    }],
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

    #[cfg(target_arch = "x86_64")]
    fn test_cet_stack(aspace: &mut AddrSpace, start: VirtAddr, size: usize) {
        aspace
            .map(
                start,
                size,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK,
                false,
                Backend::new_alloc(start, PageSize::Size4K),
            )
            .unwrap();
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn cet_default_owners_are_mm_local_and_reconcile_move_shrink_and_replacement() {
        let page = PAGE_SIZE_4K;
        let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x20_000).unwrap();
        let first = VirtAddr::from(0x4000);
        let second = VirtAddr::from(0x8000);
        test_cet_stack(&mut mm, first, page * 2);
        test_cet_stack(&mut mm, second, page);
        // These model two distinct ProcessData values sharing CLONE_VM: both
        // owners are discoverable solely through this one address space.
        mm.register_cet_default_shadow_stack(101, first, page * 2)
            .unwrap();
        mm.register_cet_default_shadow_stack(202, second, page)
            .unwrap();

        let moved = VirtAddr::from(0xc000);
        mm.unmap(first, page * 2).unwrap();
        test_cet_stack(&mut mm, moved, page * 2);
        assert!(
            mm.reconcile_cet_default_shadow_stacks_after_mremap(first, page * 2, moved)
                .is_empty()
        );
        assert_eq!(mm.cet_default_shadow_stack(101).unwrap().start, moved);
        assert_eq!(mm.cet_default_shadow_stack(202).unwrap().start, second);

        // Shrinking leaves the low VMA fragment live and updates its exact
        // extent; replacing the other owner's VMA invalidates only it.
        mm.unmap(moved + page, page).unwrap();
        assert!(
            mm.reconcile_cet_default_shadow_stacks_after_mremap(moved, page * 2, moved)
                .is_empty()
        );
        assert_eq!(mm.cet_default_shadow_stack(101).unwrap().size, page);
        mm.unmap(second, page).unwrap();
        mm.map(
            second,
            page,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            false,
            Backend::new_alloc(second, PageSize::Size4K),
        )
        .unwrap();
        assert_eq!(mm.reconcile_cet_default_shadow_stacks(), vec![202]);
        assert!(mm.cet_default_shadow_stack(202).is_none());
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn cet_default_owner_detach_is_exactly_once() {
        let page = PAGE_SIZE_4K;
        let stack = VirtAddr::from(0x4000);
        let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
        test_cet_stack(&mut mm, stack, page);
        mm.register_cet_default_shadow_stack(101, stack, page)
            .unwrap();
        let owner = mm.take_cet_default_shadow_stack(101).unwrap();
        mm.unmap(owner.start, owner.size).unwrap();
        assert!(mm.take_cet_default_shadow_stack(101).is_none());
        assert!(mm.find_area(stack).is_none());
    }
}
