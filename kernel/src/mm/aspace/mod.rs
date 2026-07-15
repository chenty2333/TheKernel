use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::{fmt, ops::DerefMut};

use axerrno::{AxError, AxResult, ax_bail};
use axhal::{
    mem::phys_to_virt,
    paging::{MappingFlags, PageTable},
    trap::PageFaultFlags,
};
use axsync::Mutex;
use memory_addr::{
    MemoryAddr, PAGE_SIZE_4K, PageIter4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k,
};
use memory_set::{MappingResult, MemoryArea, MemorySet};

use super::checked_align_up_4k;

mod backend;

pub use self::backend::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultResult {
    Handled,
    SigBus,
    Unhandled,
}

#[derive(Clone, Copy, Debug)]
struct UserIoPinRange {
    start: VirtAddr,
    end: VirtAddr,
}

/// The virtual memory address space.
pub struct AddrSpace {
    va_range: VirtAddrRange,
    areas: MemorySet<Backend>,
    growdown_starts: BTreeSet<VirtAddr>,
    wipe_on_fork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    dontfork_ranges: BTreeMap<VirtAddr, VirtAddr>,
    locked_ranges: BTreeMap<VirtAddr, VirtAddr>,
    user_io_pins: BTreeMap<u64, UserIoPinRange>,
    next_user_io_pin: u64,
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
        } = self;
        areas.protect(start, end.sub_addr(start), |_| Some(flags), page_table)?;
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
        } = self;
        let areas = transaction.commit()?;
        Self::refresh_growdown_starts(areas, growdown_starts);
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
        Ok(Self {
            va_range,
            areas: MemorySet::new(),
            growdown_starts: BTreeSet::new(),
            wipe_on_fork_ranges: BTreeMap::new(),
            dontfork_ranges: BTreeMap::new(),
            locked_ranges: BTreeMap::new(),
            user_io_pins: BTreeMap::new(),
            next_user_io_pin: 1,
            lock_future_mappings: false,
            lock_future_on_fault: false,
            pt: PageTable::try_new().map_err(|_| AxError::NoMemory)?,
        })
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

    pub fn begin_user_io_pin(&mut self, start: VirtAddr, size: usize) -> AxResult<u64> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        for _ in 0..u64::MAX {
            let token = self.next_user_io_pin;
            self.next_user_io_pin = self.next_user_io_pin.wrapping_add(1).max(1);
            if let alloc::collections::btree_map::Entry::Vacant(entry) =
                self.user_io_pins.entry(token)
            {
                entry.insert(UserIoPinRange { start, end });
                return Ok(token);
            }
        }
        Err(AxError::ResourceBusy)
    }

    pub fn end_user_io_pin(&mut self, token: u64) {
        if self.user_io_pins.remove(&token).is_none() {
            warn!("AddrSpace::end_user_io_pin: unknown token {token}");
        }
    }

    pub fn user_io_pin_overlaps(&self, start: VirtAddr, size: usize) -> bool {
        if size == 0 {
            return false;
        }
        let Some(end) = start.checked_add(size) else {
            return true;
        };
        self.user_io_pins
            .values()
            .any(|range| range.start < end && range.end > start)
    }

    fn check_no_user_io_pin_overlap(&self, start: VirtAddr, size: usize) -> AxResult {
        if self.user_io_pin_overlaps(start, size) {
            Err(AxError::ResourceBusy)
        } else {
            Ok(())
        }
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

        if !start_paddr.is_aligned_4k() {
            ax_bail!(InvalidInput, "address is not aligned");
        }

        let area = MemoryArea::new(
            start_vaddr,
            size,
            flags,
            Backend::new_linear(start_vaddr, start_paddr, size),
        );
        self.areas.map(area, &mut self.pt, false)?;
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

        let area = MemoryArea::new(start, size, flags, backend);
        self.areas.map(area, &mut self.pt, false)?;
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            if let Err(unmap_err) = self.areas.unmap(start, size, &mut self.pt) {
                warn!(
                    "AddrSpace::map: failed to roll back {start:?}+{size:#x} after populate \
                     error: {unmap_err:?}"
                );
            }
            self.refresh_growdown_starts();
            self.clear_locked_range(start, size);
            return Err(err);
        }
        Ok(())
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

        let mut modify = self.pt.cursor();
        while let Some(area) = self.areas.find(start) {
            let range = VirtAddrRange::new(start, area.end().min(end));
            area.backend()
                .populate(range, area.flags(), access_flags, &mut modify)?;
            start = area.end();
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
        self.check_no_user_io_pin_overlap(start, size)?;
        let end = start + size;

        let mut modify = self.pt.cursor();
        while let Some(area) = self.areas.find(start) {
            if area.start() > start {
                break;
            }

            let range = VirtAddrRange::new(start, area.end().min(end));
            area.backend().unmap(range, &mut modify)?;
            start = range.end;
            if start >= end {
                break;
            }
        }

        if start < end {
            ax_bail!(NoMemory);
        }

        Ok(())
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
        self.check_no_user_io_pin_overlap(start, size)?;

        self.areas.unmap(start, size, &mut self.pt)?;
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        self.clear_locked_range(start, size);
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
        self.process_area_data(start, buf.len(), |dst, offset, write_size| unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), dst.as_mut_ptr(), write_size);
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
        self.validate_region(start, size)?;
        self.check_no_user_io_pin_overlap(start, size)?;
        self.check_protect_range(start, size, flags)?;

        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;

        Ok(PreparedProtect {
            transaction: PreparedAreaProtect {
                areas: &mut self.areas,
                page_table: &mut self.pt,
                start,
                end,
                flags,
            },
            growdown_starts: &mut self.growdown_starts,
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

    /// Removes all mappings in the address space.
    pub fn clear(&mut self) {
        if !self.user_io_pins.is_empty() {
            warn!(
                "AddrSpace::clear: clearing address space with {} active user I/O pins",
                self.user_io_pins.len()
            );
        }
        if let Err(err) = self.areas.clear(&mut self.pt) {
            warn!("AddrSpace::clear: failed to unmap all areas: {err:?}");
        }
        self.growdown_starts.clear();
        self.wipe_on_fork_ranges.clear();
        self.dontfork_ranges.clear();
        self.locked_ranges.clear();
        self.user_io_pins.clear();
    }

    fn try_handle_growdown_fault(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
        user_sp: Option<VirtAddr>,
    ) -> PageFaultResult {
        let Some(user_sp) = user_sp else {
            return PageFaultResult::Unhandled;
        };

        // Linux grows MAP_GROWSDOWN mappings when the fault lands on the guard
        // page immediately below the current lowest mapped page and SP is still
        // within that guard page.
        let Some((current_start, fault_page, page_size, flags)) = self
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
                    Backend::Cow(_) => Some((current_start, fault_page, page_size, area.flags())),
                    Backend::Linear(_) | Backend::Shared(_) | Backend::File(_) => None,
                }
            })
        else {
            return PageFaultResult::Unhandled;
        };

        let Some(gap_start) =
            current_start.checked_sub(page_size as usize * Self::STACK_GUARD_GAP_PAGES)
        else {
            return PageFaultResult::Unhandled;
        };
        if self.areas.overlaps(VirtAddrRange::from_start_size(
            gap_start,
            current_start.sub_addr(gap_start),
        )) {
            return PageFaultResult::Unhandled;
        }

        if let Err(err) = self.map(
            fault_page,
            page_size as usize,
            flags,
            false,
            Backend::new_alloc(fault_page, page_size),
        ) {
            warn!(
                "Failed to extend MAP_GROWSDOWN mapping from {current_start:?} to {fault_page:?}: \
                 {err}"
            );
            return PageFaultResult::Unhandled;
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
            return PageFaultResult::Unhandled;
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
                let populate_result = area.backend().populate(
                    VirtAddrRange::from_start_size(start, len),
                    flags,
                    access_flags,
                    &mut self.pt.cursor(),
                );
                return match populate_result {
                    Ok((n, callback)) => {
                        if let Some(cb) = callback {
                            cb(self);
                        }
                        if n == 0 {
                            warn!("No pages populated for {vaddr:?} ({flags:?})");
                            PageFaultResult::Unhandled
                        } else {
                            PageFaultResult::Handled
                        }
                    }
                    Err(err) => {
                        warn!("Failed to populate pages for {vaddr:?} ({flags:?}): {err}");
                        PageFaultResult::Unhandled
                    }
                };
            }
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
        if !self.user_io_pins.is_empty() {
            return Err(AxError::ResourceBusy);
        }

        let new_aspace = Arc::new(Mutex::new(Self::new_empty(self.base(), self.size())?));
        let new_aspace_clone = new_aspace.clone();

        let mut guard = new_aspace.lock();
        guard.growdown_starts = self.growdown_starts.clone();

        let wipe_on_fork_ranges = self.wipe_on_fork_ranges.clone();
        let dontfork_ranges = self.dontfork_ranges.clone();
        let mut self_modify = self.pt.cursor();
        for area in self.areas.iter() {
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
                    let new_area = MemoryArea::new(
                        cursor,
                        wipe_size,
                        area.flags(),
                        Backend::new_alloc(cursor, page_size),
                    );
                    let aspace = guard.deref_mut();
                    aspace.areas.map(new_area, &mut aspace.pt, false)?;
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
                    let new_backend =
                        new_backend.relocate(area.start(), cursor, &new_aspace_clone)?;
                    let new_area = MemoryArea::new(cursor, segment_size, area.flags(), new_backend);
                    let aspace = guard.deref_mut();
                    aspace.areas.map(new_area, &mut aspace.pt, false)?;
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
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const TEST_SPACE_SIZE: usize = 0x6000;

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
                MemoryArea::new(VirtAddr::from(0x1000), 0x1000, 1, MockBackend(1)),
                &mut page_table,
                false,
            )
            .unwrap();
        areas
            .map(
                MemoryArea::new(VirtAddr::from(0x2000), 0x1000, 3, MockBackend(2)),
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
                MemoryArea::new(VirtAddr::from(0x1000), 0x3000, 1, MockBackend(1)),
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
        }
        .commit()
        .unwrap();
        assert_eq!(area_snapshot(&areas), vec![(0x1000, 0x4000, 1, 1)]);
        assert!(page_table[0x1000..0x4000].iter().all(|entry| *entry == 1));
    }
}
