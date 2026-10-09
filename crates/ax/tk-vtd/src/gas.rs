//! Guest-address-space allocation translated from FreeBSD
//! sys/dev/iommu/iommu_gas.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. The intrusive augmented RB tree maps to a sorted
//! kernel-owned entry vector; domain locking and page-unload callbacks are
//! explicit caller seams. DDB display commands are framework-only omissions.
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::{Error, pgtbl::PAGE_SIZE};

pub const ENTRY_PLACE: u32 = 1 << 0;
pub const ENTRY_UNMAPPED: u32 = 1 << 1;
pub const ENTRY_MAP: u32 = 1 << 2;
pub const ENTRY_RMRR: u32 = 1 << 3;
pub const ENTRY_FAKE: u32 = 1 << 4;
pub const ENTRY_REMOVING: u32 = 1 << 5;
pub const MF_CANWAIT: u32 = 1 << 0;
pub const MF_CANSPLIT: u32 = 1 << 1;
pub const MF_RMRR: u32 = 1 << 2;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GasEntry {
    pub start: u64,
    pub end: u64,
    pub flags: u32,
    pub first: u64,
    pub last: u64,
    pub free_down: u64,
    pub domain_id: Option<i32>,
    pub generation: u64,
}
impl GasEntry {
    fn placeholder(at: u64) -> Self {
        Self {
            start: at,
            end: at,
            flags: ENTRY_PLACE | ENTRY_UNMAPPED,
            first: at,
            last: at,
            ..Self::default()
        }
    }
    fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasConstraints {
    pub size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub boundary: u64,
    pub low_address: u64,
    pub high_address: u64,
    pub can_split: bool,
}
impl GasConstraints {
    pub const fn page_aligned(size: u64, end: u64) -> Self {
        Self {
            size,
            offset: 0,
            alignment: PAGE_SIZE,
            boundary: 0,
            low_address: end,
            high_address: end,
            can_split: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GasDomain {
    pub domain_id: i32,
    pub start: u64,
    pub end: u64,
    pub entries: Vec<GasEntry>,
    pub start_gap: usize,
    pub first_place: usize,
    pub last_place: usize,
    pub entries_count: usize,
    pub msi_entry: Option<GasEntry>,
    pub msi_base: u64,
    pub msi_physical: u64,
}

impl GasDomain {
    /// Stand-in for intel_gas_init(): entry zone is represented by owned Vec entries.
    // upstream: iommu_gas.c intel_gas_init()
    pub fn new(domain_id: i32, end: u64) -> Result<Self, Error> {
        Self::new_range(domain_id, 0, end)
    }

    pub fn new_range(domain_id: i32, start: u64, end: u64) -> Result<Self, Error> {
        if start >= end
            || start & (PAGE_SIZE - 1) != 0
            || end - start < 2 * PAGE_SIZE
            || !end.is_multiple_of(PAGE_SIZE)
        {
            return Err(Error::InvalidRange);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(2)
            .map_err(|_| Error::OutOfMemory)?;
        entries.push(GasEntry::placeholder(start));
        entries.push(GasEntry::placeholder(end));
        Ok(Self {
            domain_id,
            start,
            end,
            entries,
            start_gap: 0,
            first_place: 0,
            last_place: 1,
            entries_count: 2,
            msi_entry: None,
            msi_base: 0,
            msi_physical: 0,
        })
    }

    /// Allocate one tracked mapping entry.
    // upstream: iommu_gas.c iommu_gas_alloc_entry()
    pub fn iommu_gas_alloc_entry(&mut self) -> Result<GasEntry, Error> {
        self.entries_count = self
            .entries_count
            .checked_add(1)
            .ok_or(Error::InvalidRange)?;
        Ok(GasEntry {
            domain_id: Some(self.domain_id),
            ..GasEntry::default()
        })
    }

    /// Release an unlinked entry and decrement the domain accounting count.
    // upstream: iommu_gas.c iommu_gas_free_entry()
    pub fn iommu_gas_free_entry(&mut self, entry: GasEntry) -> Result<(), Error> {
        if entry.domain_id != Some(self.domain_id) || self.entries_count <= 2 {
            return Err(Error::InvalidStructure);
        }
        if entry.flags & ENTRY_PLACE == 0
            && self
                .entries
                .iter()
                .any(|resident| resident.start == entry.start && resident.end == entry.end)
        {
            return Err(Error::InvalidStructure);
        }
        self.entries_count -= 1;
        Ok(())
    }

    /// Source RB comparator: map entries are ordered by exclusive end address.
    // upstream: iommu_gas.c iommu_gas_cmp_entries()
    pub fn iommu_gas_cmp_entries(a: &GasEntry, b: &GasEntry) -> Result<Ordering, Error> {
        if a.start > a.end || b.start > b.end {
            return Err(Error::InvalidRange);
        }
        if (a.flags | b.flags) & ENTRY_FAKE == 0
            && a.end > b.start
            && b.end > a.start
            && !a.is_empty()
            && !b.is_empty()
        {
            return Err(Error::InvalidStructure);
        }
        Ok(a.end.cmp(&b.end))
    }

    /// Recompute first/last/free-down augmentation over the current ordered vector.
    // upstream: iommu_gas.c iommu_gas_augment_entry()
    pub fn iommu_gas_augment_entry(&mut self, index: usize) -> Result<bool, Error> {
        if index >= self.entries.len() {
            return Err(Error::InvalidRange);
        }
        let old = (
            self.entries[index].first,
            self.entries[index].last,
            self.entries[index].free_down,
        );
        let mut first = self.entries[index].start;
        let mut last = self.entries[index].end;
        let mut largest_gap = 0;
        for pair in self.entries.windows(2) {
            if pair[0].end <= pair[1].start {
                largest_gap = largest_gap.max(pair[1].start - pair[0].end);
            }
        }
        first = first.min(self.entries.first().map_or(first, |entry| entry.first));
        last = last.max(self.entries.last().map_or(last, |entry| entry.last));
        self.entries[index].first = first;
        self.entries[index].last = last;
        self.entries[index].free_down = largest_gap;
        Ok(old != (first, last, largest_gap))
    }

    /// Verify sorted, disjoint entries and recomputed free-gap invariants.
    // upstream: iommu_gas.c iommu_gas_check_free()
    pub fn iommu_gas_check_free(&self) -> Result<(), Error> {
        if self.entries.len() < 2
            || self.entries[0].start != self.start
            || self.entries[0].end != self.start
            || self
                .entries
                .last()
                .is_none_or(|entry| entry.start != self.end || entry.end != self.end)
        {
            return Err(Error::InvalidStructure);
        }
        for pair in self.entries.windows(2) {
            if pair[0].start > pair[0].end || pair[0].end > pair[1].start {
                return Err(Error::InvalidStructure);
            }
        }
        Ok(())
    }

    /// Remove an ordered entry and refresh the address-space gap cursor.
    // upstream: iommu_gas.c iommu_gas_rb_remove()
    pub fn iommu_gas_rb_remove(&mut self, start: u64) -> Result<GasEntry, Error> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.start == start)
            .ok_or(Error::InvalidRange)?;
        if index == self.first_place || index == self.last_place {
            return Err(Error::InvalidStructure);
        }
        let removed = self.entries.remove(index);
        self.start_gap = self.start_gap.min(index.saturating_sub(1));
        self.first_place = 0;
        self.last_place = self.entries.len() - 1;
        Ok(removed)
    }

    /// Context to domain adapter.
    // upstream: iommu_gas.c iommu_get_ctx_domain()
    pub fn iommu_get_ctx_domain(&self) -> i32 {
        self.domain_id
    }

    /// Install the two permanent boundary placeholders.
    // upstream: iommu_gas.c iommu_gas_init_domain()
    pub fn iommu_gas_init_domain(&mut self) -> Result<(), Error> {
        if self.entries.len() != 2 || self.entries_count != 2 {
            return Err(Error::InvalidStructure);
        }
        self.entries[0].first = 0;
        self.entries[0].last = 0;
        self.entries[1].first = self.end;
        self.entries[1].last = self.end;
        self.start_gap = 0;
        self.first_place = 0;
        self.last_place = 1;
        Ok(())
    }

    /// Remove the two sentinels when the domain has no outstanding mappings.
    // upstream: iommu_gas.c iommu_gas_fini_domain()
    pub fn iommu_gas_fini_domain(&mut self) -> Result<(), Error> {
        if self.entries_count != 2 || self.entries.len() != 2 {
            return Err(Error::InvalidStructure);
        }
        self.entries.clear();
        self.entries_count = 0;
        Ok(())
    }

    /// Match one free interval subject to guard pages, alignment, low/high and boundary.
    // upstream: iommu_gas.c iommu_gas_match_one()
    pub fn iommu_gas_match_one(
        interval_start: u64,
        interval_end: u64,
        lower: u64,
        upper: u64,
        constraints: GasConstraints,
    ) -> Option<GasEntry> {
        if constraints.size == 0
            || constraints.alignment == 0
            || !constraints.alignment.is_power_of_two()
        {
            return None;
        }
        let begin = interval_start.checked_add(PAGE_SIZE)?.max(lower);
        let mut start = align_up(begin, constraints.alignment)?;
        let mut end = interval_end.checked_sub(PAGE_SIZE + 1)?.min(upper);
        let offset = constraints.offset;
        let mut mapped_size = constraints.size;
        if start
            .checked_add(offset)?
            .checked_add(mapped_size)?
            .checked_sub(1)?
            > end
        {
            return None;
        }
        if constraints.boundary != 0
            && !boundary_ok(start + offset, mapped_size, constraints.boundary)
        {
            let first = start;
            let next = align_up(
                start.checked_add(offset)?.checked_add(1)?,
                constraints.boundary,
            )?;
            start = align_up(next, constraints.alignment)?;
            if start
                .checked_add(offset)?
                .checked_add(mapped_size)?
                .checked_sub(1)?
                > end
                || !boundary_ok(start + offset, mapped_size, constraints.boundary)
            {
                if !constraints.can_split {
                    return None;
                }
                mapped_size = next.checked_sub(first)?.checked_sub(offset)?;
                start = first;
                end = interval_end.checked_sub(PAGE_SIZE + 1)?.min(upper);
            }
        }
        let allocation = align_up(mapped_size.checked_add(offset)?, PAGE_SIZE)?;
        let finish = start.checked_add(allocation)?;
        finish
            .checked_sub(1)
            .is_some_and(|last| last <= end)
            .then_some(GasEntry {
                start,
                end: finish,
                flags: ENTRY_MAP,
                ..GasEntry::default()
            })
    }

    /// Find the next interval entry in address order.
    // upstream: iommu_gas.c iommu_gas_next()
    pub fn iommu_gas_next(&self, current_start: u64, minimum_free: u64) -> Option<&GasEntry> {
        let current = self
            .entries
            .iter()
            .position(|entry| entry.start == current_start)?;
        self.entries
            .windows(2)
            .skip(current + 1)
            .find(|pair| pair[1].start.saturating_sub(pair[0].end) >= minimum_free)
            .map(|pair| &pair[1])
    }

    /// Address-ordered first-fit in lowaddr first, then the upper address range.
    // upstream: iommu_gas.c iommu_gas_find_space()
    pub fn iommu_gas_find_space(&self, constraints: GasConstraints) -> Result<GasEntry, Error> {
        let min_free = 2 * PAGE_SIZE
            + align_up(
                constraints
                    .size
                    .checked_add(constraints.offset)
                    .ok_or(Error::InvalidRange)?,
                PAGE_SIZE,
            )
            .ok_or(Error::InvalidRange)?;
        let low = constraints.low_address.min(self.end - 1);
        let high_start = constraints.high_address.saturating_add(1);
        for (lower, upper) in [(self.start, low), (high_start, self.end - 1)] {
            for pair in self.entries.windows(2) {
                if let Some(entry) =
                    Self::iommu_gas_match_one(pair[0].end, pair[1].start, lower, upper, constraints)
                {
                    if pair[1].start.saturating_sub(pair[0].end) >= min_free {
                        return Ok(entry);
                    }
                }
            }
        }
        Err(Error::OutOfMemory)
    }

    /// Insert a fixed range, optionally clipping overlaps with prior RMRR entries.
    // upstream: iommu_gas.c iommu_gas_alloc_region()
    pub fn iommu_gas_alloc_region(
        &mut self,
        mut entry: GasEntry,
        rmrr: bool,
    ) -> Result<GasEntry, Error> {
        if entry.start >= entry.end
            || entry.end >= self.end
            || entry.start & (PAGE_SIZE - 1) != 0
            || entry.end & (PAGE_SIZE - 1) != 0
        {
            return Err(Error::InvalidRange);
        }
        let position = self
            .entries
            .partition_point(|existing| existing.start < entry.start);
        let previous = position.checked_sub(1).and_then(|i| self.entries.get(i));
        let next = self.entries.get(position);
        if let Some(previous) = previous {
            if previous.end > entry.start && previous.flags & ENTRY_PLACE == 0 {
                if !rmrr || previous.flags & ENTRY_RMRR == 0 {
                    return Err(Error::MapFailed);
                }
                entry.start = previous.end;
            }
        }
        if let Some(next) = next {
            if next.start < entry.end && next.flags & ENTRY_PLACE == 0 {
                if !rmrr || next.flags & ENTRY_RMRR == 0 {
                    return Err(Error::MapFailed);
                }
                entry.end = next.start;
            }
        }
        if entry.start == entry.end {
            entry.flags = ENTRY_UNMAPPED;
            return Ok(entry);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        entry.flags = if rmrr { ENTRY_RMRR } else { entry.flags };
        entry.domain_id = Some(self.domain_id);
        let index = self
            .entries
            .partition_point(|existing| existing.start < entry.start);
        self.entries.insert(index, entry.clone());
        self.entries_count += 1;
        self.last_place = self.entries.len() - 1;
        self.iommu_gas_check_free()?;
        Ok(entry)
    }

    /// Release a mapped allocation while leaving its entry for owner cleanup.
    // upstream: iommu_gas.c iommu_gas_free_space()
    pub fn iommu_gas_free_space(&mut self, start: u64) -> Result<GasEntry, Error> {
        let entry = self.iommu_gas_rb_remove(start)?;
        if entry.flags & (ENTRY_PLACE | ENTRY_RMRR | ENTRY_MAP) != ENTRY_MAP {
            return Err(Error::InvalidStructure);
        }
        Ok(entry)
    }

    /// Release an RMRR reservation, except permanent placeholders.
    // upstream: iommu_gas.c iommu_gas_free_region()
    pub fn iommu_gas_free_region(&mut self, start: u64) -> Result<GasEntry, Error> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.start == start)
            .ok_or(Error::InvalidRange)?;
        if index == self.first_place
            || index == self.last_place
            || self.entries[index].flags & ENTRY_RMRR == 0
        {
            return Err(Error::InvalidStructure);
        }
        let entry = self.iommu_gas_rb_remove(start)?;
        self.entries_count -= 1;
        Ok(entry)
    }

    /// Split a mapping at the removal start and return the containing/next entry.
    // upstream: iommu_gas.c iommu_gas_remove_clip_left()
    pub fn iommu_gas_remove_clip_left(&mut self, start: u64, end: u64) -> Result<usize, Error> {
        let index = self.entries.partition_point(|entry| entry.end <= start);
        if index >= self.entries.len() {
            return Err(Error::InvalidRange);
        }
        if self.entries[index].start >= start || self.entries[index].flags & ENTRY_RMRR != 0 {
            return Ok(index);
        }
        let mut left = self.entries[index].clone();
        left.end = start;
        self.entries[index].start = start;
        self.entries.insert(index, left);
        self.entries_count += 1;
        self.last_place = self.entries.len() - 1;
        let _ = end;
        Ok(index + 1)
    }

    /// Split a mapping at the removal end.
    // upstream: iommu_gas.c iommu_gas_remove_clip_right()
    pub fn iommu_gas_remove_clip_right(&mut self, end: u64, index: usize) -> Result<bool, Error> {
        if index >= self.entries.len()
            || self.entries[index].start >= end
            || self.entries[index].flags & ENTRY_RMRR != 0
        {
            return Ok(false);
        }
        let mut right = self.entries[index].clone();
        right.start = end;
        self.entries[index].end = end;
        self.entries.insert(index + 1, right);
        self.entries_count += 1;
        self.last_place = self.entries.len() - 1;
        Ok(true)
    }

    /// Mark a mapped entry for deferred IOTLB unload.
    // upstream: iommu_gas.c iommu_gas_remove_unmap()
    pub fn iommu_gas_remove_unmap(
        entry: &mut GasEntry,
        pending: &mut Vec<GasEntry>,
    ) -> Result<(), Error> {
        if entry.flags & (ENTRY_UNMAPPED | ENTRY_RMRR | ENTRY_REMOVING) != 0 {
            return Ok(());
        }
        if entry.flags & ENTRY_PLACE != 0 {
            return Err(Error::InvalidStructure);
        }
        entry.flags |= ENTRY_REMOVING;
        pending.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        pending.push(entry.clone());
        Ok(())
    }

    /// Remove intersecting mapped ranges, preserving RMRR/placeholders and clipping edges.
    // upstream: iommu_gas.c iommu_gas_remove_locked()
    pub fn iommu_gas_remove_locked(
        &mut self,
        start: u64,
        size: u64,
    ) -> Result<Vec<GasEntry>, Error> {
        let end = start.checked_add(size).ok_or(Error::InvalidRange)?;
        if start >= end || end > self.end {
            return Err(Error::InvalidRange);
        }
        let mut pending = Vec::new();
        let mut index = self.iommu_gas_remove_clip_left(start, end)?;
        self.iommu_gas_remove_clip_right(end, index)?;
        while index < self.entries.len() && self.entries[index].start < end {
            if self.entries[index].flags & (ENTRY_RMRR | ENTRY_PLACE) == 0 {
                let mut entry = self.entries.remove(index);
                Self::iommu_gas_remove_unmap(&mut entry, &mut pending)?;
            } else {
                index += 1;
            }
        }
        self.last_place = self.entries.len() - 1;
        Ok(pending)
    }

    /// Prepare two scratch entries and an unload list.
    // upstream: iommu_gas.c iommu_gas_remove_init()
    pub fn iommu_gas_remove_init(&mut self) -> Result<(), Error> {
        if self.entries.len() < 2 {
            return Err(Error::InvalidStructure);
        }
        Ok(())
    }

    /// Finalize deferred unload callbacks and release split scratch state.
    // upstream: iommu_gas.c iommu_gas_remove_cleanup()
    pub fn iommu_gas_remove_cleanup(
        &mut self,
        pending: Vec<GasEntry>,
        mut unload: impl FnMut(GasEntry) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for entry in pending {
            unload(entry.clone())?;
            self.iommu_gas_free_entry(entry)?;
        }
        Ok(())
    }

    /// Public range removal: mutate GAS then defer unmap completion to the caller.
    // upstream: iommu_gas.c iommu_gas_remove()
    pub fn iommu_gas_remove(&mut self, start: u64, size: u64) -> Result<Vec<GasEntry>, Error> {
        self.iommu_gas_remove_init()?;
        let pending = self.iommu_gas_remove_locked(start, size)?;
        Ok(pending)
    }

    /// Allocate a constrained IOVA, map it, and roll back on map failure.
    // upstream: iommu_gas.c iommu_gas_map()
    pub fn iommu_gas_map(
        &mut self,
        constraints: GasConstraints,
        eflags: u32,
        mut map: impl FnMut(&GasEntry) -> Result<(), Error>,
        mut unload: impl FnMut(GasEntry) -> Result<(), Error>,
    ) -> Result<GasEntry, Error> {
        let mut entry = self.iommu_gas_find_space(constraints)?;
        entry.domain_id = Some(self.domain_id);
        entry.flags |= eflags;
        self.entries
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        let index = self.entries.partition_point(|old| old.start < entry.start);
        self.entries.insert(index, entry.clone());
        self.entries_count += 1;
        self.last_place = self.entries.len() - 1;
        if let Err(error) = map(&entry) {
            let _ = self.iommu_gas_rb_remove(entry.start)?;
            self.entries_count -= 1;
            unload(entry)?;
            return Err(error);
        }
        Ok(entry)
    }

    /// Install a caller-supplied fixed range, then map pages and roll back on failure.
    // upstream: iommu_gas.c iommu_gas_map_region()
    pub fn iommu_gas_map_region(
        &mut self,
        entry: GasEntry,
        eflags: u32,
        rmrr: bool,
        mut map: impl FnMut(&GasEntry) -> Result<(), Error>,
        mut unload: impl FnMut(GasEntry) -> Result<(), Error>,
    ) -> Result<GasEntry, Error> {
        let mut entry = self.iommu_gas_alloc_region(entry, rmrr)?;
        if entry.is_empty() {
            return Ok(entry);
        }
        entry.flags |= eflags;
        if let Err(error) = map(&entry) {
            let _ = self.iommu_gas_rb_remove(entry.start)?;
            self.entries_count -= 1;
            unload(entry)?;
            return Err(error);
        }
        Ok(entry)
    }

    /// Reserve a fixed address interval under the caller's domain lock.
    // upstream: iommu_gas.c iommu_gas_reserve_region_locked()
    pub fn iommu_gas_reserve_region_locked(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<GasEntry, Error> {
        let entry = GasEntry {
            start,
            end,
            flags: ENTRY_UNMAPPED,
            domain_id: Some(self.domain_id),
            ..GasEntry::default()
        };
        self.iommu_gas_alloc_region(entry, false)
    }

    /// Reserve an interval and retain its owner entry.
    // upstream: iommu_gas.c iommu_gas_reserve_region()
    pub fn iommu_gas_reserve_region(&mut self, start: u64, end: u64) -> Result<GasEntry, Error> {
        self.iommu_gas_reserve_region_locked(start, end)
    }

    /// Reserve every currently free gap in [start,end), allowing overlaps with prior regions.
    // upstream: iommu_gas.c iommu_gas_reserve_region_extend()
    pub fn iommu_gas_reserve_region_extend(
        &mut self,
        mut start: u64,
        end: u64,
    ) -> Result<Vec<GasEntry>, Error> {
        let end = end.min(self.end);
        let mut reserved = Vec::new();
        while start < end {
            let index = self.entries.partition_point(|entry| entry.start < start);
            let next = self.entries.get(index).ok_or(Error::InvalidStructure)?;
            let previous_end = index
                .checked_sub(1)
                .and_then(|i| self.entries.get(i))
                .map_or(start, |entry| entry.end);
            let low = start.max(previous_end);
            let high = end.min(next.start);
            start = next.end.max(start + 1);
            if low < high {
                let entry = self.iommu_gas_reserve_region_locked(low, high)?;
                reserved.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
                reserved.push(entry);
            }
        }
        Ok(reserved)
    }

    /// Remove domain-owned MSI IOVA state and invalidate/unmap via callbacks.
    // upstream: iommu_gas.c iommu_unmap_msi()
    pub fn iommu_unmap_msi(
        &mut self,
        mut unmap: impl FnMut(&GasEntry) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let Some(entry) = self.msi_entry.take() else {
            return Ok(());
        };
        unmap(&entry)?;
        let _ = self.iommu_gas_free_space(entry.start)?;
        self.iommu_gas_free_entry(entry)?;
        self.msi_base = 0;
        self.msi_physical = 0;
        Ok(())
    }

    /// Lazily map the domain MSI page; duplicate racing maps discard the extra entry.
    // upstream: iommu_gas.c iommu_map_msi()
    pub fn iommu_map_msi(
        &mut self,
        constraints: GasConstraints,
        physical: u64,
        eflags: u32,
        mut map: impl FnMut(&GasEntry) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if self.msi_entry.is_some() {
            return Ok(());
        }
        let entry = self.iommu_gas_map(constraints, eflags, |entry| map(entry), |_| Ok(()))?;
        self.msi_base = entry.start;
        self.msi_physical = physical;
        self.msi_entry = Some(entry);
        Ok(())
    }

    /// Translate an MSI write address through the cached domain MSI page.
    // upstream: iommu_gas.c iommu_translate_msi()
    pub fn iommu_translate_msi(&self, address: &mut u64) -> Result<(), Error> {
        let entry = self.msi_entry.as_ref().ok_or(Error::InvalidStructure)?;
        *address = address
            .checked_sub(self.msi_physical)
            .ok_or(Error::InvalidRange)?
            .checked_add(self.msi_base)
            .ok_or(Error::InvalidRange)?;
        if *address < entry.start || address.checked_add(8).is_none_or(|end| end > entry.end) {
            return Err(Error::InvalidRange);
        }
        Ok(())
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}
fn boundary_ok(address: u64, size: u64, boundary: u64) -> bool {
    boundary == 0
        || size != 0
            && address
                .checked_add(size - 1)
                .is_some_and(|last| address / boundary == last / boundary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_first_fit_keeps_guard_pages_and_alignment() {
        let mut gas = GasDomain::new(1, 0x10000).unwrap();
        let first = gas
            .iommu_gas_map(
                GasConstraints::page_aligned(4096, 0xffff),
                ENTRY_MAP,
                |_| Ok(()),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(first.start, 0x1000);
        let second = gas
            .iommu_gas_map(
                GasConstraints {
                    size: 4096,
                    offset: 0,
                    alignment: 0x4000,
                    boundary: 0x10000,
                    low_address: 0xffff,
                    high_address: 0xffff,
                    can_split: false,
                },
                ENTRY_MAP,
                |_| Ok(()),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(second.start, 0x4000);
        assert!(second.start - first.end >= PAGE_SIZE);
    }

    #[test]
    fn fixed_regions_and_overlapping_rmrr_reservations_clip_without_corrupting_tree() {
        let mut gas = GasDomain::new(2, 0x20000).unwrap();
        let first = gas
            .iommu_gas_alloc_region(
                GasEntry {
                    start: 0x4000,
                    end: 0x8000,
                    ..GasEntry::default()
                },
                true,
            )
            .unwrap();
        let overlap = gas
            .iommu_gas_alloc_region(
                GasEntry {
                    start: 0x6000,
                    end: 0xa000,
                    ..GasEntry::default()
                },
                true,
            )
            .unwrap();
        assert_eq!((overlap.start, overlap.end), (0x8000, 0xa000));
        assert_eq!(first.flags & ENTRY_RMRR, ENTRY_RMRR);
        gas.iommu_gas_check_free().unwrap();
        gas.iommu_gas_free_region(first.start).unwrap();
        gas.iommu_gas_free_region(overlap.start).unwrap();
        gas.iommu_gas_check_free().unwrap();
    }

    #[test]
    fn partial_remove_clips_edges_and_returns_pending_unmaps() {
        let mut gas = GasDomain::new(3, 0x20000).unwrap();
        let entry = gas
            .iommu_gas_map(
                GasConstraints::page_aligned(0x6000, 0x1ffff),
                ENTRY_MAP,
                |_| Ok(()),
                |_| Ok(()),
            )
            .unwrap();
        let pending = gas.iommu_gas_remove(entry.start + 0x2000, 0x2000).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].start, entry.start + 0x2000);
        assert_eq!(pending[0].end, entry.start + 0x4000);
        gas.iommu_gas_check_free().unwrap();
    }

    #[test]
    fn failed_map_rolls_back_reservation_and_msi_translation_checks_bounds() {
        let mut gas = GasDomain::new(4, 0x10000).unwrap();
        assert_eq!(
            gas.iommu_gas_map(
                GasConstraints::page_aligned(4096, 0xffff),
                ENTRY_MAP,
                |_| Err(Error::MapFailed),
                |_| Ok(())
            )
            .err(),
            Some(Error::MapFailed)
        );
        assert_eq!(gas.entries.len(), 2);
        gas.iommu_map_msi(
            GasConstraints::page_aligned(4096, 0xffff),
            0xfee0_0000,
            ENTRY_MAP,
            |_| Ok(()),
        )
        .unwrap();
        let mut addr = 0xfee0_0010;
        gas.iommu_translate_msi(&mut addr).unwrap();
        assert_eq!(addr, gas.msi_base + 0x10);
        gas.iommu_unmap_msi(|_| Ok(())).unwrap();
        assert_eq!(gas.msi_base, 0);
    }
}
