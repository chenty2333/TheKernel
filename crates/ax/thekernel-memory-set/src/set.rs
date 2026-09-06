use alloc::collections::BTreeMap;
#[allow(unused_imports)] // this is a weird false alarm
use alloc::vec::Vec;
use core::{
    fmt, mem,
    ops::Bound::{Excluded, Included, Unbounded},
};

use memory_addr::{AddrRange, MemoryAddr};

use crate::{
    DeferredUnmapBackend, MappingBackend, MappingError, MappingLineage, MappingResult, MemoryArea,
};

struct ProtectAction<A, F> {
    area_start: A,
    start: A,
    end: A,
    old_end: A,
    old_flags: F,
    new_flags: F,
    lineage: MappingLineage,
}

/// One boundary-preserving backend metadata replacement staged by
/// [`MemorySet::prepare_metadata_update_with_limit`].
///
/// This remains private deliberately: callers own policy through the
/// predicate and replacement closure, while the range container owns all
/// boundary arithmetic and lineage preservation.
struct PreparedMetadataAction<A> {
    area_start: A,
    start: A,
    end: A,
}

/// A fully allocated, not-yet-visible metadata update.
///
/// Preparation clones every affected backend and builds a complete replacement
/// area tree. It neither changes the live set nor touches page tables. Commit
/// is therefore just an infallible tree swap after a revision check.
#[must_use = "prepared metadata updates must be committed or dropped"]
pub struct PreparedMetadataUpdate<B: MappingBackend> {
    revision: u64,
    replacement: BTreeMap<B::Addr, MemoryArea<B>>,
    changed: bool,
}

impl<B: MappingBackend> PreparedMetadataUpdate<B> {
    /// Returns whether the prepared tree changes any backend metadata.
    pub const fn changed(&self) -> bool {
        self.changed
    }

    /// Atomically installs this prepared tree and returns a transaction guard.
    ///
    /// The guard retains the complete old tree. Use
    /// [`CommittedMetadataUpdate::finish`] only after every coupled operation
    /// (for example a PMD publication) has succeeded. Dropping the guard or
    /// calling [`CommittedMetadataUpdate::rollback`] swaps the old tree back
    /// without allocation.
    pub fn commit<'a>(
        self,
        set: &'a mut MemorySet<B>,
    ) -> MappingResult<CommittedMetadataUpdate<'a, B>> {
        if self.revision != set.revision {
            return Err(MappingError::BadState);
        }
        let old_areas = mem::replace(&mut set.areas, self.replacement);
        set.bump_revision();
        Ok(CommittedMetadataUpdate {
            set,
            old_areas: Some(old_areas),
        })
    }
}

/// An installed metadata update whose old tree is still available for rollback.
///
/// The exclusive borrow prevents an intervening metadata operation from
/// invalidating rollback. Neither [`Self::rollback`] nor the drop rollback
/// allocates or invokes backend update callbacks; both only swap prebuilt
/// trees. The old tree retains the exact pre-commit area boundaries and
/// lineages, so no best-effort reconstruction is required on failure.
#[must_use = "finish commits the metadata update; drop or rollback restores the old tree"]
pub struct CommittedMetadataUpdate<'a, B: MappingBackend> {
    set: &'a mut MemorySet<B>,
    old_areas: Option<BTreeMap<B::Addr, MemoryArea<B>>>,
}

impl<B: MappingBackend> CommittedMetadataUpdate<'_, B> {
    /// Rolls the metadata tree back without allocation.
    pub fn rollback(mut self) {
        self.rollback_inner();
    }

    /// Makes the prepared replacement permanent and releases the old tree.
    pub fn finish(mut self) {
        let _ = self.old_areas.take();
    }

    fn rollback_inner(&mut self) {
        let Some(old_areas) = self.old_areas.take() else {
            return;
        };
        let replacement = mem::replace(&mut self.set.areas, old_areas);
        self.set.bump_revision();
        drop(replacement);
    }
}

impl<B: MappingBackend> Drop for CommittedMetadataUpdate<'_, B> {
    fn drop(&mut self) {
        self.rollback_inner();
    }
}

/// Outcome of a metadata-only VMA update that may have committed a prefix.
/// Unlike protection changes, Linux `mseal` retains earlier VMA changes if a
/// later split reaches the VMA-fragment limit.
#[derive(Debug)]
pub struct MetadataUpdateError {
    error: MappingError,
    changed: bool,
}

impl MetadataUpdateError {
    pub const fn changed(&self) -> bool {
        self.changed
    }

    pub fn into_parts(self) -> (MappingError, bool) {
        (self.error, self.changed)
    }
}

/// Resources retired by a deferred unmap or clear operation.
///
/// The value owns every backend retirement token produced by the operation and
/// every [`MemoryArea`] removed in full. It must remain alive until the caller
/// has completed the architecture-specific translation fence.
#[must_use = "retired mappings must be held until the translation fence completes"]
pub struct UnmapRetirement<B: DeferredUnmapBackend> {
    backend_retirements: Vec<B::Retirement>,
    retired_areas: Vec<MemoryArea<B>>,
}

impl<B: DeferredUnmapBackend> UnmapRetirement<B> {
    fn new() -> Self {
        Self {
            backend_retirements: Vec::new(),
            retired_areas: Vec::new(),
        }
    }

    fn try_reserve(&mut self, retirements: usize, areas: usize) -> MappingResult {
        self.backend_retirements
            .try_reserve(retirements)
            .map_err(|_| MappingError::NoMemory)?;
        self.retired_areas
            .try_reserve(areas)
            .map_err(|_| MappingError::NoMemory)?;
        Ok(())
    }

    /// Returns whether the operation retired no backend or area resources.
    pub fn is_empty(&self) -> bool {
        self.backend_retirements.is_empty() && self.retired_areas.is_empty()
    }

    /// Returns the backend retirement tokens retained until release.
    pub fn backend_retirements(&self) -> &[B::Retirement] {
        &self.backend_retirements
    }

    /// Returns the fully removed memory areas retained until release.
    pub fn retired_areas(&self) -> &[MemoryArea<B>] {
        &self.retired_areas
    }

    /// Releases all retained resources after the caller's fence has completed.
    pub fn release(self) {}
}

fn clone_area<B: MappingBackend>(area: &MemoryArea<B>) -> MemoryArea<B> {
    MemoryArea::new_with_lineage(
        area.start(),
        area.size(),
        area.flags(),
        area.backend().clone(),
        area.lineage(),
    )
}

struct FixedReplacementRestore<B: MappingBackend> {
    start: B::Addr,
    size: usize,
    backend: B,
}

/// The old ownership retained by a completed fixed replacement.
///
/// Keep this value until the architecture-specific translation grace period
/// has elapsed. In addition to backend retirement tokens, it owns the exact
/// pre-replacement VMA tree, including original boundaries and lineages.
#[must_use = "fixed-replacement retirement must survive the translation fence"]
pub struct FixedReplacementRetirement<B: DeferredUnmapBackend> {
    retirement: UnmapRetirement<B>,
    old_areas: BTreeMap<B::Addr, MemoryArea<B>>,
}

impl<B: DeferredUnmapBackend> FixedReplacementRetirement<B> {
    /// Returns the deferred backend retirements for diagnostics.
    pub fn backend_retirements(&self) -> &[B::Retirement] {
        self.retirement.backend_retirements()
    }

    /// Releases the old mapping ownership after the caller's translation
    /// fence. Dropping this value has the same ownership effect; the explicit
    /// spelling documents the required ordering at call sites.
    pub fn release(self) {
        let Self {
            retirement,
            old_areas,
        } = self;
        retirement.release();
        drop(old_areas);
    }
}

/// Incoming VMA/backend ownership withdrawn by a rolled-back fixed
/// replacement.
///
/// Restoring the old PTE leaves does not invalidate translations which other
/// CPUs may have cached for the provisional incoming mapping.  Keep the exact
/// replacement tree alive until the caller completes its architecture-specific
/// translation fence; dropping it earlier can release backend/finalizer state
/// while a stale incoming translation is still usable.
#[must_use = "rolled-back replacement ownership must survive the translation fence"]
pub struct FixedReplacementRollbackRetirement<B: DeferredUnmapBackend> {
    replacement_areas: BTreeMap<B::Addr, MemoryArea<B>>,
}

impl<B: DeferredUnmapBackend> FixedReplacementRollbackRetirement<B> {
    /// Releases the provisional replacement ownership after the caller's
    /// translation fence.  The explicit method documents the required order
    /// at the transaction boundary.
    pub fn release(self) {
        drop(self.replacement_areas);
    }
}

/// A fully preflighted, still-invisible MAP_FIXED-style replacement.
///
/// Preparation allocates a complete post-replacement VMA tree and records
/// every old PTE range that rollback may restore. It also admits the incoming
/// map, every old unmap, and every old exact-leaf restore before any live state
/// changes.
#[must_use = "prepared fixed replacements must be committed or dropped"]
pub struct PreparedFixedReplacement<B: DeferredUnmapBackend> {
    revision: u64,
    replacement_start: B::Addr,
    replacement_size: usize,
    replacement_flags: B::Flags,
    incoming_map: B::Retirement,
    replacement_tree: BTreeMap<B::Addr, MemoryArea<B>>,
    restores: Vec<FixedReplacementRestore<B>>,
    // Both Vec capacities are admitted while preparation is still private.
    // Commit only fills this preallocated token vector; it must not allocate
    // after withdrawing any old PTE.
    retirement: UnmapRetirement<B>,
}

impl<B: DeferredUnmapBackend> PreparedFixedReplacement<B> {
    /// Withdraws old PTEs and atomically makes the replacement VMA topology
    /// visible. The returned guard owns the old topology until `finish`, and
    /// may restore its exact old leaves without allocation if installing the new mapping fails
    /// before publication.
    pub fn commit(
        self,
        set: &mut MemorySet<B>,
        page_table: &mut B::PageTable,
    ) -> MappingResult<CommittedFixedReplacement<B>> {
        if self.revision != set.revision {
            return Err(MappingError::BadState);
        }

        let mut retirement = self.retirement;
        assert_eq!(
            retirement.backend_retirements.len(),
            self.restores.len(),
            "fixed replacement lost a prepared deferred-unmap token"
        );
        for (restore, token) in self
            .restores
            .iter()
            .zip(retirement.backend_retirements.iter_mut())
        {
            if !restore.backend.unmap_deferred_prepared(
                token,
                restore.start,
                restore.size,
                page_table,
            ) {
                // Preparation reserved every token while topology and page
                // tables were serialized. Continuing would expose a partially
                // withdrawn fixed replacement, so this is fail-stop and the
                // already-detached ownership must not be released.
                mem::forget(retirement);
                panic!("mapping backend failed after successful fixed-replace prepared-unmap admission");
            }
        }

        let old_areas = mem::replace(&mut set.areas, self.replacement_tree);
        set.bump_revision();
        Ok(CommittedFixedReplacement {
            replacement_start: self.replacement_start,
            replacement_size: self.replacement_size,
            replacement_flags: self.replacement_flags,
            incoming_map: Some(self.incoming_map),
            restores: self.restores,
            old_areas: Some(old_areas),
            retirement: Some(retirement),
            installed: false,
        })
    }
}

/// A committed fixed replacement awaiting installation or rollback.
///
/// The exclusive borrow prevents unrelated VMA changes from invalidating the
/// exact old tree retained for rollback. `install` and `rollback` allocate
/// nothing; backend failure after preflight is a fail-stop invariant breach.
#[must_use = "a committed fixed replacement requires explicit finish or rollback"]
pub struct CommittedFixedReplacement<B: DeferredUnmapBackend> {
    replacement_start: B::Addr,
    replacement_size: usize,
    replacement_flags: B::Flags,
    incoming_map: Option<B::Retirement>,
    restores: Vec<FixedReplacementRestore<B>>,
    old_areas: Option<BTreeMap<B::Addr, MemoryArea<B>>>,
    retirement: Option<UnmapRetirement<B>>,
    installed: bool,
}

impl<B: DeferredUnmapBackend> CommittedFixedReplacement<B> {
    /// Installs the preflighted incoming PTEs without altering VMA metadata.
    pub fn install(&mut self, set: &mut MemorySet<B>, page_table: &mut B::PageTable) {
        assert!(!self.installed, "fixed replacement installed twice");
        let incoming_map = self
            .incoming_map
            .take()
            .expect("fixed replacement incoming-map token already consumed");
        let replacement = set
            .areas
            .get(&self.replacement_start)
            .expect("fixed replacement live VMA disappeared before install");
        if replacement.start() != self.replacement_start
            || replacement.size() != self.replacement_size
            || replacement.flags() != self.replacement_flags
            || !replacement.backend().map_fixed_prepared(
                incoming_map,
                self.replacement_start,
                self.replacement_size,
                self.replacement_flags,
                page_table,
            )
        {
            if let Some(retirement) = self.retirement.take() {
                mem::forget(retirement);
            }
            if let Some(old_areas) = self.old_areas.take() {
                mem::forget(old_areas);
            }
            panic!("mapping backend failed after successful fixed-replace prepared-map admission");
        }
        self.installed = true;
    }

    /// Keeps the replacement and transfers old ownership to the caller.
    ///
    /// The returned value must remain alive until the caller's external TLB
    /// grace period is complete.
    pub fn finish(mut self) -> FixedReplacementRetirement<B> {
        assert!(self.installed, "fixed replacement finished before install");
        FixedReplacementRetirement {
            retirement: self
                .retirement
                .take()
                .expect("fixed replacement retirement already consumed"),
            old_areas: self
                .old_areas
                .take()
                .expect("fixed replacement old tree already consumed"),
        }
    }

    /// Removes the newly installed mapping, remaps all admitted old PTE
    /// leaf state, and swaps the exact original VMA tree back without allocation.
    pub fn rollback(
        mut self,
        set: &mut MemorySet<B>,
        page_table: &mut B::PageTable,
    ) -> FixedReplacementRollbackRetirement<B> {
        self.rollback_inner(set, page_table)
    }

    fn rollback_inner(
        &mut self,
        set: &mut MemorySet<B>,
        page_table: &mut B::PageTable,
    ) -> FixedReplacementRollbackRetirement<B> {
        let old_areas = self
            .old_areas
            .take()
            .expect("fixed replacement rolled back after ownership was consumed");

        if self.installed {
            let replacement = set
                .areas
                .get(&self.replacement_start)
                .expect("fixed replacement live VMA disappeared before rollback");
            if !replacement.backend().unmap(
                self.replacement_start,
                self.replacement_size,
                page_table,
            ) {
                if let Some(retirement) = self.retirement.take() {
                    mem::forget(retirement);
                }
                mem::forget(old_areas);
                panic!("mapping backend failed while rolling back fixed replacement");
            }
            self.installed = false;
        }
        let mut retirement = self
            .retirement
            .take()
            .expect("fixed replacement retirement already consumed");
        assert_eq!(
            retirement.backend_retirements.len(),
            self.restores.len(),
            "fixed replacement lost an old-PTE retirement token"
        );
        let mut tokens = mem::take(&mut retirement.backend_retirements).into_iter();
        for restore in &self.restores {
            let token = tokens
                .next()
                .expect("fixed replacement lost an old-PTE retirement token");
            if !restore
                .backend
                .restore_deferred(token, restore.start, restore.size, page_table)
            {
                // `restore_deferred` consumes its token. On a post-admission
                // failure it is responsible for retaining that token's state;
                // the still-unconsumed tokens and old tree must likewise not
                // be dropped while the page table is partially restored.
                mem::forget(tokens);
                mem::forget(retirement);
                mem::forget(old_areas);
                panic!("mapping backend failed after successful fixed-replace deferred-restore preflight");
            }
        }

        let replacement_tree = mem::replace(&mut set.areas, old_areas);
        set.bump_revision();
        drop(retirement);
        FixedReplacementRollbackRetirement {
            replacement_areas: replacement_tree,
        }
    }
}

impl<B: DeferredUnmapBackend> Drop for CommittedFixedReplacement<B> {
    fn drop(&mut self) {
        // Drop has no page-table argument and therefore cannot restore a
        // withdrawn mapping. Callers must explicitly finish or roll back
        // while they still hold page-table serialization.
        assert!(
            self.old_areas.is_none(),
            "committed fixed replacement dropped without finish or rollback"
        );
    }
}

trait UnmapMode<B: MappingBackend> {
    type Output;

    fn try_reserve(&mut self, unmaps: usize, complete_areas: usize) -> MappingResult;

    fn unmap(
        &mut self,
        backend: &B,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
    ) -> bool;

    fn retire_area(&mut self, area: MemoryArea<B>);

    fn finish(self) -> Self::Output;
}

struct ImmediateUnmap;

impl<B: MappingBackend> UnmapMode<B> for ImmediateUnmap {
    type Output = ();

    fn try_reserve(&mut self, _unmaps: usize, _complete_areas: usize) -> MappingResult {
        Ok(())
    }

    fn unmap(
        &mut self,
        backend: &B,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
    ) -> bool {
        backend.unmap(start, size, page_table)
    }

    fn retire_area(&mut self, _area: MemoryArea<B>) {}

    fn finish(self) {}
}

struct DeferredUnmap<B: DeferredUnmapBackend> {
    retirement: Option<UnmapRetirement<B>>,
}

impl<B: DeferredUnmapBackend> DeferredUnmap<B> {
    fn new() -> Self {
        Self {
            retirement: Some(UnmapRetirement::new()),
        }
    }

    fn retirement_mut(&mut self) -> &mut UnmapRetirement<B> {
        self.retirement
            .as_mut()
            .expect("deferred unmap retirement was already disarmed")
    }

    fn leak_retirement(&mut self) {
        // A post-preflight backend failure is fail-stop, but host tests and a
        // future unwinding kernel may still catch the panic. Never let stack
        // unwinding release resources that earlier commits detached from their
        // PTEs before the caller has established translation grace.
        if let Some(retirement) = self.retirement.take() {
            mem::forget(retirement);
        }
    }
}

impl<B: DeferredUnmapBackend> UnmapMode<B> for DeferredUnmap<B> {
    type Output = UnmapRetirement<B>;

    fn try_reserve(&mut self, unmaps: usize, complete_areas: usize) -> MappingResult {
        self.retirement_mut().try_reserve(unmaps, complete_areas)
    }

    fn unmap(
        &mut self,
        backend: &B,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
    ) -> bool {
        let Some(retirement) = backend.unmap_deferred(start, size, page_table) else {
            self.leak_retirement();
            return false;
        };
        self.retirement_mut().backend_retirements.push(retirement);
        true
    }

    fn retire_area(&mut self, area: MemoryArea<B>) {
        self.retirement_mut().retired_areas.push(area);
    }

    fn finish(mut self) -> Self::Output {
        self.retirement
            .take()
            .expect("deferred unmap retirement was already disarmed")
    }
}

/// A container that maintains memory mappings ([`MemoryArea`]).
pub struct MemorySet<B: MappingBackend> {
    areas: BTreeMap<B::Addr, MemoryArea<B>>,
    // Prepared metadata commits use this to reject a tree assembled from a
    // stale topology. The committing guard then keeps `&mut self`, making its
    // rollback immune to intervening normal MemorySet operations.
    revision: u64,
}

impl<B: MappingBackend> MemorySet<B> {
    fn check_area_limit(count: usize, max_areas: usize) -> MappingResult {
        if count > max_areas {
            Err(MappingError::NoMemory)
        } else {
            Ok(())
        }
    }

    /// Returns a conservative count of the VMA fragments that remain after
    /// removing `range`.
    ///
    /// Adjacent fragments that the commit may subsequently merge are counted
    /// separately. This makes the result suitable for capacity admission: it
    /// can reject early, but it can never undercount a live tree node.
    fn fragment_count_after_unmap(&self, range: AddrRange<B::Addr>) -> MappingResult<usize> {
        let mut overlapping = 0usize;
        let mut fragments = 0usize;
        for area in self.iter_overlapping(range) {
            // The iterator yields distinct entries from `areas`, so this
            // cannot exceed `self.len()` or overflow.
            overlapping += 1;
            if area.start() < range.start {
                fragments = fragments.checked_add(1).ok_or(MappingError::NoMemory)?;
            }
            if area.end() > range.end {
                fragments = fragments.checked_add(1).ok_or(MappingError::NoMemory)?;
            }
        }

        // Only the overlapping entries need inspection. All other live tree
        // nodes survive unchanged, while every residual side of an overlap is
        // conservatively counted as a separate fragment.
        let unaffected = self.len() - overlapping;
        unaffected
            .checked_add(fragments)
            .ok_or(MappingError::NoMemory)
    }

    /// Creates a new memory set.
    pub const fn new() -> Self {
        Self {
            areas: BTreeMap::new(),
            revision: 0,
        }
    }

    fn bump_revision(&mut self) {
        // Revision is an ABA guard, not a user-visible sequence number. A
        // wrap would require 2^64 serialized topology changes while a
        // prepared update is retained, which is not a reachable kernel
        // lifetime. Wrapping keeps mutation itself infallible.
        self.revision = self.revision.wrapping_add(1);
    }

    /// Returns the number of memory areas in the memory set.
    pub fn len(&self) -> usize {
        self.areas.len()
    }

    /// Returns `true` if the memory set contains no memory areas.
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty()
    }

    /// Returns the iterator over all memory areas.
    pub fn iter(&self) -> impl Iterator<Item = &MemoryArea<B>> {
        self.areas.values()
    }

    /// Returns the memory areas that overlap `range`, in address order.
    ///
    /// The cursor starts at the one predecessor that may cross the lower
    /// boundary and then walks only keys below the upper boundary. Adapters
    /// can therefore plan range transactions without scanning every VMA that
    /// precedes the target.
    pub fn iter_overlapping(
        &self,
        range: AddrRange<B::Addr>,
    ) -> impl Iterator<Item = &MemoryArea<B>> {
        let first_start = self
            .areas
            .range(..=range.start)
            .next_back()
            .filter(|(_, area)| area.end() > range.start)
            .map(|(&area_start, _)| area_start)
            .unwrap_or(range.start);
        self.areas
            .range(first_start..range.end)
            .map(|(_, area)| area)
            .filter(move |area| area.va_range().overlaps(range))
    }

    /// Returns whether the given address range overlaps with any existing area.
    pub fn overlaps(&self, range: AddrRange<B::Addr>) -> bool {
        if let Some((_, before)) = self.areas.range(..range.start).last() {
            if before.va_range().overlaps(range) {
                return true;
            }
        }
        if let Some((_, after)) = self.areas.range(range.start..).next() {
            if after.va_range().overlaps(range) {
                return true;
            }
        }
        false
    }

    /// Finds the memory area that contains the given address.
    pub fn find(&self, addr: B::Addr) -> Option<&MemoryArea<B>> {
        let candidate = self.areas.range(..=addr).last().map(|(_, a)| a);
        candidate.filter(|a| a.va_range().contains(addr))
    }

    fn merge_prev_into(&mut self, current_start: B::Addr) -> B::Addr {
        let Some((&prev_start, _)) = self.areas.range(..current_start).last() else {
            return current_start;
        };

        let can_merge = {
            let prev = self.areas.get(&prev_start).unwrap();
            let curr = self.areas.get(&current_start).unwrap();
            prev.end() == curr.start()
                && prev.flags() == curr.flags()
                && prev.lineage() == curr.lineage()
                && prev.backend().can_merge(curr.backend())
        };
        if !can_merge {
            return current_start;
        }

        let curr_end = self.areas.remove(&current_start).unwrap().end();
        self.areas.get_mut(&prev_start).unwrap().set_end(curr_end);
        prev_start
    }

    fn merge_next_into(&mut self, current_start: B::Addr) -> bool {
        let Some((&next_start, _)) = self
            .areas
            .range((Excluded(current_start), Unbounded))
            .next()
        else {
            return false;
        };

        let can_merge = {
            let curr = self.areas.get(&current_start).unwrap();
            let next = self.areas.get(&next_start).unwrap();
            curr.end() == next.start()
                && curr.flags() == next.flags()
                && curr.lineage() == next.lineage()
                && curr.backend().can_merge(next.backend())
        };
        if !can_merge {
            return false;
        }

        let next_end = self.areas.remove(&next_start).unwrap().end();
        self.areas
            .get_mut(&current_start)
            .unwrap()
            .set_end(next_end);
        true
    }

    fn merge_adjacent_at(&mut self, anchor: B::Addr) {
        let mut current_start = if self.areas.contains_key(&anchor) {
            anchor
        } else if let Some((&start, area)) = self.areas.range(..=anchor).last() {
            if area.end() == anchor || area.va_range().contains(anchor) {
                start
            } else {
                return;
            }
        } else {
            return;
        };

        loop {
            let merged_start = self.merge_prev_into(current_start);
            if merged_start == current_start {
                break;
            }
            current_start = merged_start;
        }
        while self.merge_next_into(current_start) {}
    }

    /// Finds a free area that can accommodate the given size.
    ///
    /// The search starts from the given `hint` address, and the area should be
    /// within the given `limit` range.
    ///
    /// # Notes
    /// The `align` parameter specifies the alignment of the start address and
    /// the size of the area. The start address of the resulting area will
    /// be aligned to this value. Also, the size of the area must be a multiple
    /// of this value.
    ///
    /// # Returns
    /// Returns the start address of the free area. Returns `None` if no such
    /// area is found.
    pub fn find_free_area(
        &self,
        hint: B::Addr,
        size: usize,
        limit: AddrRange<B::Addr>,
        align: usize,
    ) -> Option<B::Addr> {
        if size % align != 0 {
            // size must be a multiple of align.
            return None;
        }
        // brute force: try each area's end address as the start.
        let mut last_end: <B as MappingBackend>::Addr = hint.max(limit.start).align_up(align);
        if let Some((_, area)) = self.areas.range(..last_end).last() {
            last_end = last_end.max(area.end()).align_up(align);
        }
        for (&addr, area) in self.areas.range(last_end..) {
            if last_end.checked_add(size).is_some_and(|end| end <= addr) {
                return Some(last_end);
            }
            last_end = area.end().align_up(align);
        }
        if last_end
            .checked_add(size)
            .is_some_and(|end| end <= limit.end)
        {
            Some(last_end)
        } else {
            None
        }
    }

    /// Finds an append-biased free area at or after the highest occupied end
    /// within the given limit.
    ///
    /// This is intended for kernel-chosen placements that grow upward, not for
    /// exact first-fit semantics. Callers should still fall back to
    /// [`Self::find_free_area`] when this returns [`None`].
    pub fn find_append_area(
        &self,
        size: usize,
        limit: AddrRange<B::Addr>,
        align: usize,
    ) -> Option<B::Addr> {
        if size % align != 0 {
            return None;
        }

        let candidate = self
            .areas
            .range(..limit.end)
            .next_back()
            .map(|(_, area)| area.end())
            .unwrap_or(limit.start)
            .max(limit.start)
            .align_up(align);

        candidate
            .checked_add(size)
            .filter(|&end| end <= limit.end)
            .map(|_| candidate)
    }

    /// Add a new memory mapping.
    ///
    /// The mapping is represented by a [`MemoryArea`].
    ///
    /// If the new area overlaps with any existing area, the behavior is
    /// determined by the `unmap_overlap` parameter. If it is `true`, the
    /// overlapped regions will be unmapped first. Otherwise, it returns an
    /// error.
    pub fn map(
        &mut self,
        area: MemoryArea<B>,
        page_table: &mut B::PageTable,
        unmap_overlap: bool,
    ) -> MappingResult {
        self.map_with_limit(area, page_table, unmap_overlap, usize::MAX)
    }

    /// Adds a new mapping while bounding the peak number of live VMA
    /// fragments.
    ///
    /// Capacity admission completes before an overlapping mapping is removed
    /// or the backend/page table is changed. The ordinary [`Self::map`]
    /// interface remains source-compatible and uses no effective limit.
    pub fn map_with_limit(
        &mut self,
        area: MemoryArea<B>,
        page_table: &mut B::PageTable,
        unmap_overlap: bool,
        max_areas: usize,
    ) -> MappingResult {
        if area.va_range().is_empty() {
            return Err(MappingError::InvalidParam);
        }

        if self.overlaps(area.va_range()) {
            if unmap_overlap {
                let remaining = self.fragment_count_after_unmap(area.va_range())?;
                let peak = remaining.checked_add(1).ok_or(MappingError::NoMemory)?;
                Self::check_area_limit(self.len().max(peak), max_areas)?;
                self.unmap_with_limit(area.start(), area.size(), page_table, max_areas)?;
            } else {
                return Err(MappingError::AlreadyExists);
            }
        } else {
            let peak = self.len().checked_add(1).ok_or(MappingError::NoMemory)?;
            Self::check_area_limit(peak, max_areas)?;
        }

        let area_start = area.start();
        area.map_area(page_table)?;
        assert!(self.areas.insert(area_start, area).is_none());
        self.merge_adjacent_at(area_start);
        self.bump_revision();
        Ok(())
    }

    /// Remove memory mappings within the given address range.
    ///
    /// All memory areas that are fully contained in the range will be removed
    /// directly. If the area intersects with the boundary, it will be shrinked.
    /// If the unmapped range is in the middle of an existing area, it will be
    /// split into two areas.
    pub fn unmap(
        &mut self,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
    ) -> MappingResult {
        self.unmap_with_limit(start, size, page_table, usize::MAX)
    }

    /// Validates every backend touched by an unmap without changing the area
    /// tree or page table.
    ///
    /// A caller that keeps the page table and mapping topology serialized may
    /// use this to prepare a larger transaction. A later commit still checks
    /// the same invariant defensively.
    pub fn preflight_unmap(
        &self,
        start: B::Addr,
        size: usize,
        page_table: &B::PageTable,
    ) -> MappingResult {
        let range =
            AddrRange::try_from_start_size(start, size).ok_or(MappingError::InvalidParam)?;
        if range.is_empty() {
            return Ok(());
        }

        let end = range.end;

        // Admission is read-only and covers every backend before the first
        // VMA or PTE change. The mutable commit below runs under the caller's
        // page-table/topology serialization, so a backend failure after this
        // point is an invariant violation rather than a recoverable result.
        let first_start = self
            .areas
            .range(..=start)
            .next_back()
            .filter(|(_, area)| area.end() > start)
            .map(|(&area_start, _)| area_start)
            .unwrap_or(start);
        for area in self.areas.range(first_start..end).map(|(_, area)| area) {
            let unmap_start = area.start().max(start);
            let unmap_end = area.end().min(end);
            if unmap_start < unmap_end
                && !area.backend().preflight_unmap(
                    unmap_start,
                    unmap_end.sub_addr(unmap_start),
                    page_table,
                )
            {
                return Err(MappingError::BadState);
            }
        }

        Ok(())
    }

    /// Removes mappings while bounding the peak number of live VMA
    /// fragments.
    ///
    /// A middle unmap can turn one area into two. The resulting node count and
    /// every backend admission are checked before the first VMA/PTE mutation.
    pub fn unmap_with_limit(
        &mut self,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
        max_areas: usize,
    ) -> MappingResult {
        self.unmap_with_limit_inner(start, size, page_table, max_areas, ImmediateUnmap)
    }

    /// Removes mappings while retaining backend and complete-area ownership.
    ///
    /// The returned value must remain alive until the caller completes the
    /// translation fence that makes stale translations unreachable. Existing
    /// [`Self::unmap`] behavior remains available for immediate retirement.
    pub fn unmap_deferred(
        &mut self,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
    ) -> MappingResult<UnmapRetirement<B>>
    where
        B: DeferredUnmapBackend,
    {
        self.unmap_deferred_with_limit(start, size, page_table, usize::MAX)
    }

    /// Deferred unmap with an explicit live-area quota.
    ///
    /// Capacity for the address plan, backend retirements, and complete area
    /// owners is admitted before the first page-table or area-tree mutation.
    pub fn unmap_deferred_with_limit(
        &mut self,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
        max_areas: usize,
    ) -> MappingResult<UnmapRetirement<B>>
    where
        B: DeferredUnmapBackend,
    {
        self.unmap_with_limit_inner(start, size, page_table, max_areas, DeferredUnmap::new())
    }

    /// Removes every complete area selected by `should_unmap` while retaining
    /// all backend and area ownership until the caller establishes one
    /// translation grace period.
    ///
    /// Logical mapping owners such as SysV SHM can survive VMA splitting and
    /// hole punching. Selecting by backend metadata removes all surviving
    /// fragments as one transaction without requiring one large contiguous
    /// virtual range. Every address and retirement slot is reserved and every
    /// backend is preflighted before the first page-table or area-tree change.
    pub fn unmap_matching_deferred_with_limit(
        &mut self,
        should_unmap: impl Fn(&B) -> bool,
        page_table: &mut B::PageTable,
        max_areas: usize,
    ) -> MappingResult<UnmapRetirement<B>>
    where
        B: DeferredUnmapBackend,
    {
        Self::check_area_limit(self.len(), max_areas)?;
        let selected_count = self
            .areas
            .values()
            .filter(|area| should_unmap(area.backend()))
            .count();

        let mut selected = Vec::new();
        selected
            .try_reserve(selected_count)
            .map_err(|_| MappingError::NoMemory)?;
        selected.extend(
            self.areas.iter().filter_map(|(&area_start, area)| {
                should_unmap(area.backend()).then_some(area_start)
            }),
        );

        let mut mode = DeferredUnmap::new();
        mode.try_reserve(selected_count, selected_count)?;
        for area_start in &selected {
            let area = self
                .areas
                .get(area_start)
                .expect("selected mapping area disappeared during unmap preflight");
            if !area
                .backend()
                .preflight_unmap(area.start(), area.size(), page_table)
            {
                return Err(MappingError::BadState);
            }
        }

        for area_start in selected {
            let area = self
                .areas
                .get(&area_start)
                .expect("selected mapping area disappeared during unmap commit");
            assert!(
                mode.unmap(area.backend(), area.start(), area.size(), page_table),
                "mapping backend failed after successful matching-unmap preflight"
            );
            let area = self
                .areas
                .remove(&area_start)
                .expect("selected mapping area disappeared during retirement");
            mode.retire_area(area);
        }
        if selected_count != 0 {
            self.bump_revision();
        }
        Ok(mode.finish())
    }

    /// Removes an exact, sorted set of complete areas as one deferred
    /// retirement transaction.
    ///
    /// The caller supplies current area starts captured under the same
    /// topology lock. Every selected area must still satisfy `should_unmap`;
    /// any missing, duplicate, reordered, or stale selection is rejected
    /// before page tables change. This supports provenance-driven teardown
    /// where only a subset of one logical owner's fragments should disappear.
    pub fn unmap_selected_deferred_with_limit(
        &mut self,
        selected: &[B::Addr],
        should_unmap: impl Fn(&B) -> bool,
        page_table: &mut B::PageTable,
        max_areas: usize,
    ) -> MappingResult<UnmapRetirement<B>>
    where
        B: DeferredUnmapBackend,
    {
        Self::check_area_limit(self.len(), max_areas)?;
        if selected.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(MappingError::InvalidParam);
        }

        let mut mode = DeferredUnmap::new();
        mode.try_reserve(selected.len(), selected.len())?;
        for area_start in selected {
            let area = self.areas.get(area_start).ok_or(MappingError::BadState)?;
            if !should_unmap(area.backend())
                || !area
                    .backend()
                    .preflight_unmap(area.start(), area.size(), page_table)
            {
                return Err(MappingError::BadState);
            }
        }

        for area_start in selected {
            let area = self
                .areas
                .get(area_start)
                .expect("preflighted selected mapping area disappeared during commit");
            assert!(
                mode.unmap(area.backend(), area.start(), area.size(), page_table),
                "mapping backend failed after successful selected-unmap preflight"
            );
            let area = self
                .areas
                .remove(area_start)
                .expect("preflighted selected mapping area disappeared during retirement");
            mode.retire_area(area);
        }
        if !selected.is_empty() {
            self.bump_revision();
        }
        Ok(mode.finish())
    }

    /// Prepares an all-or-nothing MAP_FIXED-style replacement.
    ///
    /// `replacement` becomes the sole VMA covering its virtual range. Any
    /// old VMA may be split at either boundary; their exact pre-operation tree
    /// is retained by the committed guard, rather than reconstructed by
    /// best-effort merging on rollback. All allocation, fragment admission,
    /// incoming-map admission, old-unmap admission, and old exact-restore
    /// admission finish before the first live VMA or PTE mutation.
    pub fn prepare_fixed_replacement_with_limit(
        &self,
        replacement: MemoryArea<B>,
        page_table: &B::PageTable,
        max_areas: usize,
    ) -> MappingResult<PreparedFixedReplacement<B>>
    where
        B: DeferredUnmapBackend,
    {
        let range = replacement.va_range();
        if range.is_empty() {
            return Err(MappingError::InvalidParam);
        }

        let after_unmap = self.fragment_count_after_unmap(range)?;
        let projected = after_unmap.checked_add(1).ok_or(MappingError::NoMemory)?;
        Self::check_area_limit(self.len().max(projected), max_areas)?;

        let Some(incoming_map) = replacement.backend().prepare_fixed_map(
            replacement.start(),
            replacement.size(),
            replacement.flags(),
            page_table,
        ) else {
            return Err(MappingError::BadState);
        };

        let mut restores = Vec::new();
        restores
            .try_reserve(self.len())
            .map_err(|_| MappingError::NoMemory)?;
        let overlap_count = self.iter_overlapping(range).count();
        let mut retirement = UnmapRetirement::new();
        retirement.try_reserve(overlap_count, 0)?;
        for area in self.iter_overlapping(range) {
            let start = area.start().max(range.start);
            let end = area.end().min(range.end);
            debug_assert!(start < end);
            let size = end.sub_addr(start);
            if !area.backend().preflight_unmap(start, size, page_table)
                || !area
                    .backend()
                    .preflight_restore_deferred(start, size, page_table)
            {
                return Err(MappingError::BadState);
            }
            let Some(token) = area
                .backend()
                .prepare_deferred_unmap(start, size, page_table)
            else {
                return Err(MappingError::BadState);
            };
            restores.push(FixedReplacementRestore {
                start,
                size,
                backend: area.backend().clone(),
            });
            retirement.backend_retirements.push(token);
        }

        // Build the complete future tree while it is private. BTreeMap does
        // not provide fallible reserve/insert, so every node allocation is
        // necessarily completed before commit can withdraw a PTE.
        let mut replacement_tree = BTreeMap::new();
        for (&area_start, area) in &self.areas {
            if !area.va_range().overlaps(range) {
                let cloned = clone_area(area);
                assert!(replacement_tree.insert(area_start, cloned).is_none());
                continue;
            }
            if area.start() < range.start {
                let left = MemoryArea::new_with_lineage(
                    area.start(),
                    range.start.sub_addr(area.start()),
                    area.flags(),
                    area.backend().clone(),
                    area.lineage(),
                );
                assert!(replacement_tree.insert(left.start(), left).is_none());
            }
            if range.end < area.end() {
                let right = MemoryArea::new_with_lineage(
                    range.end,
                    area.end().sub_addr(range.end),
                    area.flags(),
                    area.backend().clone(),
                    area.lineage(),
                );
                assert!(replacement_tree.insert(right.start(), right).is_none());
            }
        }
        let replacement_start = replacement.start();
        let replacement_size = replacement.size();
        let replacement_flags = replacement.flags();
        assert!(replacement_tree
            .insert(replacement_start, replacement)
            .is_none());

        Ok(PreparedFixedReplacement {
            revision: self.revision,
            replacement_start,
            replacement_size,
            replacement_flags,
            incoming_map,
            // Keep the incoming VMA as its own node even when a backend says
            // it can merge with a neighbour. The PTEs on either side were not
            // installed by one operation, and preserving this boundary keeps
            // the replacement's backend and lineage exact until a later,
            // ordinary VMA operation deliberately coalesces it.
            replacement_tree,
            restores,
            retirement,
        })
    }

    /// Commits a prepared fixed replacement without allocating.
    pub fn commit_prepared_fixed_replacement(
        &mut self,
        prepared: PreparedFixedReplacement<B>,
        page_table: &mut B::PageTable,
    ) -> MappingResult<CommittedFixedReplacement<B>>
    where
        B: DeferredUnmapBackend,
    {
        prepared.commit(self, page_table)
    }

    fn unmap_with_limit_inner<M: UnmapMode<B>>(
        &mut self,
        start: B::Addr,
        size: usize,
        page_table: &mut B::PageTable,
        max_areas: usize,
        mut mode: M,
    ) -> MappingResult<M::Output> {
        let range =
            AddrRange::try_from_start_size(start, size).ok_or(MappingError::InvalidParam)?;
        if range.is_empty() {
            mode.try_reserve(0, 0)?;
            return Ok(mode.finish());
        }

        let remaining = self.fragment_count_after_unmap(range)?;
        Self::check_area_limit(self.len().max(remaining), max_areas)?;

        let mut unmap_count = 0;
        let mut fully_covered_count = 0;
        for area in self.iter_overlapping(range) {
            unmap_count += 1;
            fully_covered_count += usize::from(area.va_range().contained_in(range));
        }
        let mut fully_covered = Vec::new();
        fully_covered
            .try_reserve(fully_covered_count)
            .map_err(|_| MappingError::NoMemory)?;
        mode.try_reserve(unmap_count, fully_covered_count)?;
        fully_covered.extend(
            self.areas
                .range((Included(start), Excluded(range.end)))
                .filter_map(|(&area_start, area)| {
                    area.va_range().contained_in(range).then_some(area_start)
                }),
        );
        self.preflight_unmap(start, size, page_table)?;

        let end = range.end;

        // Unmap entire areas that are contained by the range.
        for area_start in fully_covered {
            let area = self.areas.get(&area_start).unwrap();
            assert!(
                mode.unmap(area.backend(), area.start(), area.size(), page_table),
                "mapping backend failed after successful unmap preflight"
            );
            let area = self.areas.remove(&area_start).unwrap();
            mode.retire_area(area);
        }

        // Shrink right if the area intersects with the left boundary.
        if let Some((_, before)) = self.areas.range_mut(..start).last() {
            let before_end = before.end();
            if before_end > start {
                if before_end <= end {
                    // the unmapped area is at the end of `before`.
                    assert!(
                        mode.unmap(
                            before.backend(),
                            start,
                            before_end.sub_addr(start),
                            page_table
                        ),
                        "mapping backend failed after successful unmap preflight"
                    );
                    before.set_end(start);
                } else {
                    // the unmapped area is in the middle `before`, need to split.
                    let right_part = MemoryArea::new_with_lineage(
                        end,
                        before_end.sub_addr(end),
                        before.flags(),
                        before.backend().clone(),
                        before.lineage(),
                    );
                    assert!(
                        mode.unmap(before.backend(), start, end.sub_addr(start), page_table),
                        "mapping backend failed after successful unmap preflight"
                    );
                    before.set_end(start);
                    assert_eq!(right_part.start().into(), Into::<usize>::into(end));
                    self.areas.insert(end, right_part);
                }
            }
        }

        // Shrink left if the area intersects with the right boundary.
        if let Some((&after_start, after)) = self.areas.range_mut(start..).next() {
            if after_start < end {
                // the unmapped area is at the start of `after`.
                assert!(
                    mode.unmap(
                        after.backend(),
                        after_start,
                        end.sub_addr(after_start),
                        page_table
                    ),
                    "mapping backend failed after successful unmap preflight"
                );
                after.set_start(end);
                let new_area = self.areas.remove(&after_start).unwrap();
                assert_eq!(new_area.start().into(), Into::<usize>::into(end));
                self.areas.insert(end, new_area);
            }
        }

        self.merge_adjacent_at(start);
        self.merge_adjacent_at(end);

        self.bump_revision();

        Ok(mode.finish())
    }

    /// Remove all memory areas and the underlying mappings.
    pub fn clear(&mut self, page_table: &mut B::PageTable) -> MappingResult {
        self.clear_inner(page_table, ImmediateUnmap)
    }

    fn clear_inner<M: UnmapMode<B>>(
        &mut self,
        page_table: &mut B::PageTable,
        mut mode: M,
    ) -> MappingResult<M::Output> {
        let area_count = self.len();
        mode.try_reserve(area_count, area_count)?;

        for area in self.areas.values() {
            if !area
                .backend()
                .preflight_unmap(area.start(), area.size(), page_table)
            {
                return Err(MappingError::BadState);
            }
        }
        for area in self.areas.values() {
            assert!(
                mode.unmap(area.backend(), area.start(), area.size(), page_table),
                "mapping backend failed after successful clear preflight"
            );
        }
        for area in mem::take(&mut self.areas).into_values() {
            mode.retire_area(area);
        }
        self.bump_revision();
        Ok(mode.finish())
    }

    /// Removes all mappings while retaining every backend token and area owner.
    ///
    /// Both output vectors reserve their complete capacity before backend
    /// preflight and before the first page-table or area-tree mutation.
    pub fn clear_deferred(
        &mut self,
        page_table: &mut B::PageTable,
    ) -> MappingResult<UnmapRetirement<B>>
    where
        B: DeferredUnmapBackend,
    {
        self.clear_inner(page_table, DeferredUnmap::new())
    }

    /// Change the flags of memory mappings within the given address range.
    ///
    /// `update_flags` is a function that receives old flags and processes
    /// new flags (e.g., some flags can not be changed through this interface).
    /// It returns [`None`] if there is no bit to change.
    ///
    /// Memory areas will be skipped according to `update_flags`. Memory areas
    /// that are fully contained in the range or contains the range or
    /// intersects with the boundary will be handled similarly to `munmap`.
    pub fn protect(
        &mut self,
        start: B::Addr,
        size: usize,
        update_flags: impl Fn(B::Flags) -> Option<B::Flags>,
        page_table: &mut B::PageTable,
    ) -> MappingResult {
        self.protect_with_limit(
            start,
            size,
            |_, flags| update_flags(flags),
            page_table,
            usize::MAX,
        )
    }

    /// Changes mapping flags while bounding the peak number of live VMA
    /// fragments.
    ///
    /// All left/middle/right split nodes are admitted before backend preflight
    /// and before the first tree or PTE mutation.
    pub fn protect_with_limit(
        &mut self,
        start: B::Addr,
        size: usize,
        update_flags: impl Fn(B::Addr, B::Flags) -> Option<B::Flags>,
        page_table: &mut B::PageTable,
        max_areas: usize,
    ) -> MappingResult {
        let end = start.checked_add(size).ok_or(MappingError::InvalidParam)?;
        if start == end {
            return Ok(());
        }
        let mut actions = Vec::new();

        // Include the one area that may start before the requested range, then
        // walk only the overlapping suffix instead of cloning the whole set.
        let first_start = self
            .areas
            .range(..=start)
            .next_back()
            .filter(|(_, area)| area.end() > start)
            .map(|(&area_start, _)| area_start)
            .unwrap_or(start);
        for (&area_start, area) in self.areas.range(first_start..end) {
            let area_end = area.end();
            if area_end > start {
                let Some(new_flags) = update_flags(area_start.max(start), area.flags()) else {
                    continue;
                };
                actions.try_reserve(1).map_err(|_| MappingError::NoMemory)?;
                actions.push(ProtectAction {
                    area_start,
                    start: area_start.max(start),
                    end: area_end.min(end),
                    old_end: area_end,
                    old_flags: area.flags(),
                    new_flags,
                    lineage: area.lineage(),
                });
            }
        }

        let mut peak = self.len();
        for action in &actions {
            let additional = usize::from(action.area_start < action.start)
                + usize::from(action.end < action.old_end);
            peak = peak.checked_add(additional).ok_or(MappingError::NoMemory)?;
        }
        Self::check_area_limit(peak, max_areas)?;

        // Complete every recoverable backend/PTE check before splitting the
        // first VMA. Once this succeeds, commit failures are consistency bugs:
        // restoring the old area tree cannot prove that a partially updated
        // page table was restored as well.
        for action in &actions {
            let backend = self.areas.get(&action.area_start).unwrap().backend();
            if !backend.preflight_protect(
                action.start,
                action.end.sub_addr(action.start),
                action.new_flags,
                page_table,
            ) {
                return Err(MappingError::BadState);
            }
        }

        // Pre-split only affected areas. Every BTreeMap insertion (and thus
        // every infallible node allocation imposed by alloc::BTreeMap) occurs
        // before the first backend/PTE mutation. The original node at
        // `area_start` is retained as an in-place rollback anchor.
        for action in &actions {
            let has_left = action.area_start < action.start;
            let has_right = action.end < action.old_end;
            let (middle_backend, right_backend) = {
                let area = self.areas.get(&action.area_start).unwrap();
                (
                    has_left.then(|| area.backend().clone()),
                    has_right.then(|| area.backend().clone()),
                )
            };
            self.areas
                .get_mut(&action.area_start)
                .unwrap()
                .set_end(if has_left { action.start } else { action.end });

            if let Some(backend) = middle_backend {
                let middle = MemoryArea::new_with_lineage(
                    action.start,
                    action.end.sub_addr(action.start),
                    action.new_flags,
                    backend,
                    action.lineage,
                );
                assert!(self.areas.insert(middle.start(), middle).is_none());
            }
            if let Some(backend) = right_backend {
                let right = MemoryArea::new_with_lineage(
                    action.end,
                    action.old_end.sub_addr(action.end),
                    action.old_flags,
                    backend,
                    action.lineage,
                );
                assert!(self.areas.insert(right.start(), right).is_none());
            }
        }

        for action in &actions {
            let backend = self.areas.get(&action.start).unwrap().backend();
            backend
                .protect(
                    action.start,
                    action.end.sub_addr(action.start),
                    action.new_flags,
                    page_table,
                )
                .then_some(())
                .expect("mapping backend failed after successful protect preflight");
        }

        for action in &actions {
            self.areas
                .get_mut(&action.start)
                .unwrap()
                .set_flags(action.new_flags);
        }
        for action in actions {
            self.merge_adjacent_at(action.start);
            self.merge_adjacent_at(action.end);
        }
        self.bump_revision();
        Ok(())
    }

    /// Prepares an all-or-nothing backend-metadata update for a fully mapped
    /// range.
    ///
    /// Unlike [`Self::update_metadata_with_limit`], this API never commits a
    /// prefix. Preparation first verifies that `start..start + size` contains
    /// no VMA gap, evaluates the predicate for every intersecting area, checks
    /// the post-split area limit, and constructs a complete replacement tree.
    /// Every affected backend is cloned and updated while that replacement is
    /// still private. Thus allocation or predicate/update failure cannot alter
    /// the live tree.
    ///
    /// `BTreeMap` exposes no fallible `reserve`; its complete replacement is
    /// instead constructed here, before commit. Both action and merge-anchor
    /// vectors use fallible reservation, and all BTreeMap node allocations are
    /// necessarily complete before the first live metadata change.
    pub fn prepare_metadata_update_with_limit(
        &self,
        start: B::Addr,
        size: usize,
        should_update: impl Fn(&B) -> bool,
        update: impl Fn(&mut B),
        max_areas: usize,
    ) -> MappingResult<PreparedMetadataUpdate<B>> {
        let end = start.checked_add(size).ok_or(MappingError::InvalidParam)?;

        let mut actions = Vec::new();
        actions
            .try_reserve(self.len())
            .map_err(|_| MappingError::NoMemory)?;

        // Start at the crossing predecessor. A prepared transaction has
        // stricter semantics than mseal's historical prefix API: a hole means
        // the caller did not prepare one complete logical range.
        let first_start = self
            .areas
            .range(..=start)
            .next_back()
            .filter(|(_, area)| area.end() > start)
            .map(|(&area_start, _)| area_start)
            .unwrap_or(start);
        let mut covered_until = start;
        for (&area_start, area) in self.areas.range(first_start..end) {
            if area.end() <= covered_until {
                continue;
            }
            let action_start = area_start.max(start);
            if action_start > covered_until {
                return Err(MappingError::InvalidParam);
            }
            let action_end = area.end().min(end);
            if action_end <= action_start {
                continue;
            }
            covered_until = action_end;
            if should_update(area.backend()) {
                actions.push(PreparedMetadataAction {
                    area_start,
                    start: action_start,
                    end: action_end,
                });
            }
            if covered_until == end {
                break;
            }
        }
        if covered_until != end {
            return Err(MappingError::InvalidParam);
        }

        let mut projected = self.len();
        for action in &actions {
            projected = projected
                .checked_add(usize::from(action.area_start < action.start))
                .and_then(|count| {
                    count.checked_add(usize::from(
                        action.end < self.areas[&action.area_start].end(),
                    ))
                })
                .ok_or(MappingError::NoMemory)?;
        }
        Self::check_area_limit(projected, max_areas)?;

        // Build every replacement node before changing live metadata. This
        // deliberately clones unaffected nodes too: commit can then swap the
        // complete BTreeMap and rollback can restore the exact old one.
        let mut replacement = BTreeMap::new();
        let mut action_index = 0usize;
        for (&area_start, area) in &self.areas {
            let action = actions
                .get(action_index)
                .filter(|action| action.area_start == area_start);
            if let Some(action) = action {
                action_index += 1;
                if area_start < action.start {
                    let left = MemoryArea::new_with_lineage(
                        area_start,
                        action.start.sub_addr(area_start),
                        area.flags(),
                        area.backend().clone(),
                        area.lineage(),
                    );
                    assert!(replacement.insert(left.start(), left).is_none());
                }

                let mut backend = area.backend().clone();
                update(&mut backend);
                let middle = MemoryArea::new_with_lineage(
                    action.start,
                    action.end.sub_addr(action.start),
                    area.flags(),
                    backend,
                    area.lineage(),
                );
                assert!(replacement.insert(middle.start(), middle).is_none());

                if action.end < area.end() {
                    let right = MemoryArea::new_with_lineage(
                        action.end,
                        area.end().sub_addr(action.end),
                        area.flags(),
                        area.backend().clone(),
                        area.lineage(),
                    );
                    assert!(replacement.insert(right.start(), right).is_none());
                }
            } else {
                let cloned = MemoryArea::new_with_lineage(
                    area_start,
                    area.size(),
                    area.flags(),
                    area.backend().clone(),
                    area.lineage(),
                );
                assert!(replacement.insert(cloned.start(), cloned).is_none());
            }
        }
        debug_assert_eq!(action_index, actions.len());

        // Preserve ordinary metadata-update coalescing in private storage.
        // Only changed boundaries are considered: unrelated pre-existing VMA
        // boundaries must not disappear merely because this transaction cloned
        // the full tree. The anchor list itself is fallibly reserved before
        // the live swap.
        let mut anchors = Vec::new();
        anchors
            .try_reserve(actions.len().checked_mul(2).ok_or(MappingError::NoMemory)?)
            .map_err(|_| MappingError::NoMemory)?;
        for action in &actions {
            anchors.push(action.start);
            anchors.push(action.end);
        }
        let mut staged = MemorySet {
            areas: replacement,
            revision: self.revision,
        };
        for anchor in anchors {
            staged.merge_adjacent_at(anchor);
        }

        Ok(PreparedMetadataUpdate {
            revision: self.revision,
            replacement: staged.areas,
            changed: !actions.is_empty(),
        })
    }

    /// Prepares an all-or-nothing backend-metadata update for every complete
    /// area selected by `should_update`.
    ///
    /// This is the sparse counterpart of
    /// [`Self::prepare_metadata_update_with_limit`]. It deliberately does not
    /// split areas: callers use it to replace ownership metadata that was
    /// cloned across an address space, even when the selected VMAs are
    /// separated by holes or unrelated mappings. The complete replacement
    /// tree and every merge anchor are allocated before commit, so a failed
    /// preparation cannot leave a prefix of the address space updated.
    pub fn prepare_matching_metadata_update_with_limit(
        &self,
        should_update: impl Fn(&B) -> bool,
        update: impl Fn(&mut B),
        max_areas: usize,
    ) -> MappingResult<PreparedMetadataUpdate<B>> {
        Self::check_area_limit(self.len(), max_areas)?;

        let anchor_capacity = self.len().checked_mul(2).ok_or(MappingError::NoMemory)?;
        let mut anchors = Vec::new();
        anchors
            .try_reserve(anchor_capacity)
            .map_err(|_| MappingError::NoMemory)?;

        let mut replacement = BTreeMap::new();
        let mut changed = false;
        for (&area_start, area) in &self.areas {
            let mut backend = area.backend().clone();
            if should_update(&backend) {
                update(&mut backend);
                anchors.push(area.start());
                anchors.push(area.end());
                changed = true;
            }
            let cloned = MemoryArea::new_with_lineage(
                area_start,
                area.size(),
                area.flags(),
                backend,
                area.lineage(),
            );
            assert!(replacement.insert(cloned.start(), cloned).is_none());
        }

        let mut staged = MemorySet {
            areas: replacement,
            revision: self.revision,
        };
        for anchor in anchors {
            staged.merge_adjacent_at(anchor);
        }

        Ok(PreparedMetadataUpdate {
            revision: self.revision,
            replacement: staged.areas,
            changed,
        })
    }

    /// Commits a [`PreparedMetadataUpdate`] without allocating.
    ///
    /// This spelling is useful when callers keep their transaction verbs on
    /// `MemorySet`; it is equivalent to [`PreparedMetadataUpdate::commit`].
    pub fn commit_prepared_metadata_update<'a>(
        &'a mut self,
        prepared: PreparedMetadataUpdate<B>,
    ) -> MappingResult<CommittedMetadataUpdate<'a, B>> {
        prepared.commit(self)
    }

    /// Applies one backend-metadata update to every VMA fragment intersecting
    /// `start..start + size`, splitting boundary VMAs first. No page-table
    /// operation occurs. VMAs are found, split, and updated one at a time in
    /// address order. A later fragment-limit or allocation failure retains the
    /// successfully updated prefix; the callback itself must be infallible.
    pub fn update_metadata_with_limit(
        &mut self,
        start: B::Addr,
        size: usize,
        should_update: impl Fn(&B) -> bool,
        update: impl Fn(&mut B),
        max_areas: usize,
    ) -> Result<(), MetadataUpdateError> {
        let end = start.checked_add(size).ok_or(MetadataUpdateError {
            error: MappingError::InvalidParam,
            changed: false,
        })?;
        if start == end {
            return Ok(());
        }
        let mut changed = false;
        let mut cursor = start;
        while cursor < end {
            // Start from the crossing predecessor; if the cursor is in a
            // hole, advance to the next VMA. Capture everything needed before
            // mutating because splitting/merging invalidates map references.
            let candidate = self
                .areas
                .range(..=cursor)
                .next_back()
                .filter(|(_, area)| area.end() > cursor)
                .map(|(&area_start, _)| area_start)
                .or_else(|| {
                    self.areas
                        .range(cursor..)
                        .next()
                        .map(|(&area_start, _)| area_start)
                });
            let Some(area_start) = candidate else {
                break;
            };
            let (action_start, action_end, old_end, flags, lineage, needs_update) = {
                let area = self.areas.get(&area_start).unwrap();
                if area.start() >= end {
                    break;
                }
                (
                    area_start.max(start),
                    area.end().min(end),
                    area.end(),
                    area.flags(),
                    area.lineage(),
                    should_update(area.backend()),
                )
            };
            // Advance using the pre-change end: the current action can split
            // or merge this VMA, but no later work may revisit its prefix.
            cursor = action_end;
            if !needs_update {
                continue;
            }

            let has_left = area_start < action_start;
            let has_right = action_end < old_end;
            let additional = usize::from(has_left) + usize::from(has_right);
            let Some(peak) = self.len().checked_add(additional) else {
                if changed {
                    self.bump_revision();
                }
                return Err(MetadataUpdateError {
                    error: MappingError::NoMemory,
                    changed,
                });
            };
            if peak > max_areas {
                if changed {
                    self.bump_revision();
                }
                return Err(MetadataUpdateError {
                    error: MappingError::NoMemory,
                    changed,
                });
            }

            // BTreeMap has no fallible insert, so the bounded fragment check
            // is the recoverable admission point. Each VMA then commits
            // independently, matching Linux mseal's prefix semantics.
            let (middle_backend, right_backend) = {
                let area = self.areas.get(&area_start).unwrap();
                (
                    has_left.then(|| area.backend().clone()),
                    has_right.then(|| area.backend().clone()),
                )
            };
            self.areas
                .get_mut(&area_start)
                .unwrap()
                .set_end(if has_left { action_start } else { action_end });
            if let Some(backend) = middle_backend {
                let area = MemoryArea::new_with_lineage(
                    action_start,
                    action_end.sub_addr(action_start),
                    flags,
                    backend,
                    lineage,
                );
                assert!(self.areas.insert(area.start(), area).is_none());
            }
            if let Some(backend) = right_backend {
                let area = MemoryArea::new_with_lineage(
                    action_end,
                    old_end.sub_addr(action_end),
                    flags,
                    backend,
                    lineage,
                );
                assert!(self.areas.insert(area.start(), area).is_none());
            }
            update(self.areas.get_mut(&action_start).unwrap().backend_mut());
            self.merge_adjacent_at(action_start);
            self.merge_adjacent_at(action_end);
            changed = true;
        }
        if changed {
            self.bump_revision();
        }
        Ok(())
    }
}

impl<B: MappingBackend> Default for MemorySet<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: MappingBackend> fmt::Debug for MemorySet<B>
where
    B::Addr: fmt::Debug,
    B::Flags: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.areas.values()).finish()
    }
}
