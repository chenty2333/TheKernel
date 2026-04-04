use alloc::collections::BTreeMap;
#[allow(unused_imports)] // this is a weird false alarm
use alloc::vec::Vec;
use core::{
    fmt,
    ops::Bound::{Excluded, Included, Unbounded},
};

use memory_addr::{AddrRange, MemoryAddr};

use crate::{MappingBackend, MappingError, MappingResult, MemoryArea};

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
            area.unmap_area(page_table)?;
        }

        // Shrink right if the area intersects with the left boundary.
        if let Some((&before_start, before)) = self.areas.range_mut(..start).last() {
            let before_end = before.end();
            if before_end > start {
                if before_end <= end {
                    // the unmapped area is at the end of `before`.
                    before.shrink_right(start.sub_addr(before_start), page_table)?;
                } else {
                    // the unmapped area is in the middle `before`, need to split.
                    let right_part = before.split(end).unwrap();
                    before.shrink_right(start.sub_addr(before_start), page_table)?;
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
                new_area.shrink_left(after_end.sub_addr(end), page_table)?;
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
        for (_, area) in self.areas.iter() {
            area.unmap_area(page_table)?;
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
        let mut to_insert = Vec::new();
        for (&area_start, area) in self.areas.iter_mut() {
            let area_end = area.end();

            if let Some(new_flags) = update_flags(area.flags()) {
                if area_start >= end {
                    // [ prot ]
                    //          [ area ]
                    break;
                } else if area_end <= start {
                    //          [ prot ]
                    // [ area ]
                    // Do nothing
                } else if area_start >= start && area_end <= end {
                    // [   prot   ]
                    //   [ area ]
                    area.protect_area(new_flags, page_table)?;
                    area.set_flags(new_flags);
                } else if area_start < start && area_end > end {
                    //        [ prot ]
                    // [ left | area | right ]
                    let right_part = area.split(end).unwrap();
                    area.set_end(start);

                    let mut middle_part =
                        MemoryArea::new(start, size, area.flags(), area.backend().clone());
                    middle_part.protect_area(new_flags, page_table)?;
                    middle_part.set_flags(new_flags);

                    to_insert.push((right_part.start(), right_part));
                    to_insert.push((middle_part.start(), middle_part));
                } else if area_end > end {
                    // [    prot ]
                    //   [  area | right ]
                    let right_part = area.split(end).unwrap();
                    area.protect_area(new_flags, page_table)?;
                    area.set_flags(new_flags);

                    to_insert.push((right_part.start(), right_part));
                } else {
                    //        [ prot    ]
                    // [ left |  area ]
                    let mut right_part = area.split(start).unwrap();
                    right_part.protect_area(new_flags, page_table)?;
                    right_part.set_flags(new_flags);

                    to_insert.push((right_part.start(), right_part));
                }
            }
        }
        self.areas.extend(to_insert);
        self.merge_adjacent_at(start);
        self.merge_adjacent_at(end);
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
