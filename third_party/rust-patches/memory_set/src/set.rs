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
        }
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
        self.protect_with_limit(start, size, update_flags, page_table, usize::MAX)
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
        update_flags: impl Fn(B::Flags) -> Option<B::Flags>,
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
                let Some(new_flags) = update_flags(area.flags()) else {
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
