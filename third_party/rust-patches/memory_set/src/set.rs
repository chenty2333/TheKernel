use alloc::collections::BTreeMap;
#[allow(unused_imports)] // this is a weird false alarm
use alloc::vec::Vec;
use core::{
    fmt,
    ops::Bound::{Excluded, Included, Unbounded},
};

use memory_addr::{AddrRange, MemoryAddr};

use crate::{MappingBackend, MappingError, MappingResult, MemoryArea};

struct ProtectAction<A, F> {
    area_start: A,
    start: A,
    end: A,
    old_end: A,
    old_flags: F,
    new_flags: F,
}

/// A container that maintains memory mappings ([`MemoryArea`]).
pub struct MemorySet<B: MappingBackend> {
    areas: BTreeMap<B::Addr, MemoryArea<B>>,
}

impl<B: MappingBackend> MemorySet<B> {
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
        if area.va_range().is_empty() {
            return Err(MappingError::InvalidParam);
        }

        if self.overlaps(area.va_range()) {
            if unmap_overlap {
                self.unmap(area.start(), area.size(), page_table)?;
            } else {
                return Err(MappingError::AlreadyExists);
            }
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

        // Unmap entire areas that are contained by the range.
        let fully_covered: Vec<_> = self
            .areas
            .range((Included(start), Excluded(end)))
            .filter_map(|(&area_start, area)| {
                area.va_range().contained_in(range).then_some(area_start)
            })
            .collect();
        for area_start in fully_covered {
            let area = self.areas.remove(&area_start).unwrap();
            area.unmap_area(page_table)
                .expect("mapping backend failed after successful unmap preflight");
        }

        // Shrink right if the area intersects with the left boundary.
        if let Some((&before_start, before)) = self.areas.range_mut(..start).last() {
            let before_end = before.end();
            if before_end > start {
                if before_end <= end {
                    // the unmapped area is at the end of `before`.
                    before
                        .shrink_right(start.sub_addr(before_start), page_table)
                        .expect("mapping backend failed after successful unmap preflight");
                } else {
                    // the unmapped area is in the middle `before`, need to split.
                    let right_part = before.split(end).unwrap();
                    before
                        .shrink_right(start.sub_addr(before_start), page_table)
                        .expect("mapping backend failed after successful unmap preflight");
                    assert_eq!(right_part.start().into(), Into::<usize>::into(end));
                    self.areas.insert(end, right_part);
                }
            }
        }

        // Shrink left if the area intersects with the right boundary.
        if let Some((&after_start, after)) = self.areas.range_mut(start..).next() {
            let after_end = after.end();
            if after_start < end {
                // the unmapped area is at the start of `after`.
                let mut new_area = self.areas.remove(&after_start).unwrap();
                new_area
                    .shrink_left(after_end.sub_addr(end), page_table)
                    .expect("mapping backend failed after successful unmap preflight");
                assert_eq!(new_area.start().into(), Into::<usize>::into(end));
                self.areas.insert(end, new_area);
            }
        }

        self.merge_adjacent_at(start);
        self.merge_adjacent_at(end);

        Ok(())
    }

    /// Remove all memory areas and the underlying mappings.
    pub fn clear(&mut self, page_table: &mut B::PageTable) -> MappingResult {
        for area in self.areas.values() {
            if !area
                .backend()
                .preflight_unmap(area.start(), area.size(), page_table)
            {
                return Err(MappingError::BadState);
            }
        }
        for (_, area) in self.areas.iter() {
            area.unmap_area(page_table)
                .expect("mapping backend failed after successful clear preflight");
        }
        self.areas.clear();
        Ok(())
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
                });
            }
        }

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
                let middle = MemoryArea::new(
                    action.start,
                    action.end.sub_addr(action.start),
                    action.new_flags,
                    backend,
                );
                assert!(self.areas.insert(middle.start(), middle).is_none());
            }
            if let Some(backend) = right_backend {
                let right = MemoryArea::new(
                    action.end,
                    action.old_end.sub_addr(action.end),
                    action.old_flags,
                    backend,
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
