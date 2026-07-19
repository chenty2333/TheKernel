use alloc::vec::Vec;
use core::{
    fmt,
    marker::PhantomData,
    ops::Deref,
    sync::atomic::{Ordering, compiler_fence, fence},
};

use arrayvec::ArrayVec;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr};

use crate::{
    GenericPTE, MappingFlags, PageSize, PagingError, PagingHandler, PagingMetaData, PagingResult,
    TlbFlusher,
};

const ENTRY_COUNT: usize = 512;

/// Maximum number of intermediate table frames needed to install one leaf in
/// a supported 64-bit page table.
///
/// A four-level page table needs at most three frames below its root. A
/// three-level page table needs at most two, so one fixed-capacity reservation
/// serves both layouts without heap allocation.
pub const MAX_PREPARED_TABLE_FRAMES_64: usize = 3;

/// Failure while allocating an out-of-lock page-table frame reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareTableFramesError {
    /// The requested reservation exceeds the fixed 64-bit path bound.
    TooMany {
        /// Number of frames requested by the caller.
        requested: usize,
        /// Maximum number of frames a reservation can contain.
        maximum: usize,
    },
    /// The paging handler could not allocate another frame.
    NoMemory,
}

/// Failure while committing a leaf with preallocated page-table frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PreparedMapError {
    /// The path changed and now requires more frames than were reserved.
    NeedMore {
        /// Number of intermediate frames required by the rechecked path.
        required: usize,
        /// Number of frames retained by the reservation.
        available: usize,
    },
    /// A regular page-table error detected before publication.
    Paging(PagingError),
}

impl From<PagingError> for PreparedMapError {
    fn from(error: PagingError) -> Self {
        Self::Paging(error)
    }
}

/// Result of one successful prepared mapping commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparedMapCommit {
    consumed_frames: usize,
}

impl PreparedMapCommit {
    /// Number of reserved intermediate table frames consumed by this commit.
    pub const fn consumed_frames(self) -> usize {
        self.consumed_frames
    }
}

/// Preallocated and pre-zeroed frames for one 64-bit page-table path.
///
/// Allocate this value before entering a page-table critical section, pass it
/// by mutable reference to [`PageTable64Cursor::map_prepared`], and move/drop
/// it only after leaving that critical section. Failed commits retain every
/// frame. Successful commits remove only frames made reachable from the page
/// table; unused frames remain owned by this reservation.
pub struct PreparedPageTableFrames<H: PagingHandler> {
    frames: ArrayVec<PhysAddr, MAX_PREPARED_TABLE_FRAMES_64>,
    _handler: PhantomData<H>,
}

impl<H: PagingHandler> PreparedPageTableFrames<H> {
    /// Allocates and zeroes exactly `frame_count` table frames.
    ///
    /// If allocation fails, already allocated frames are reclaimed before the
    /// error is returned. Callers must invoke this outside page-table locks.
    pub fn try_new(frame_count: usize) -> Result<Self, PrepareTableFramesError> {
        if frame_count > MAX_PREPARED_TABLE_FRAMES_64 {
            return Err(PrepareTableFramesError::TooMany {
                requested: frame_count,
                maximum: MAX_PREPARED_TABLE_FRAMES_64,
            });
        }

        let mut prepared = Self {
            frames: ArrayVec::new(),
            _handler: PhantomData,
        };
        for _ in 0..frame_count {
            let frame =
                alloc_zeroed_table_frame::<H>().map_err(|_| PrepareTableFramesError::NoMemory)?;
            prepared
                .frames
                .try_push(frame)
                .expect("validated prepared-frame capacity");
        }
        Ok(prepared)
    }

    /// Allocates the maximum reservation needed by any supported 64-bit path.
    pub fn try_max() -> Result<Self, PrepareTableFramesError> {
        Self::try_new(MAX_PREPARED_TABLE_FRAMES_64)
    }

    /// Number of frames currently retained by this reservation.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Returns whether this reservation retains no frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl<H: PagingHandler> fmt::Debug for PreparedPageTableFrames<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedPageTableFrames")
            .field("frame_count", &self.frames.len())
            .finish()
    }
}

impl<H: PagingHandler> Drop for PreparedPageTableFrames<H> {
    fn drop(&mut self) {
        while let Some(frame) = self.frames.pop() {
            H::dealloc_frame(frame);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PreparedMapPath {
    publish_table: PhysAddr,
    publish_index: usize,
    publish_level: usize,
    target_level: usize,
    missing_tables: usize,
}

fn alloc_zeroed_table_frame<H: PagingHandler>() -> PagingResult<PhysAddr> {
    let paddr = H::alloc_frame().ok_or(PagingError::NoMemory)?;
    let ptr = H::phys_to_virt(paddr).as_mut_ptr() as *mut u64;
    // u64 store loop instead of write_bytes: under QEMU TCG the emitted
    // memset for write_bytes is ~4x slower here (it issues ~4x more stores).
    for i in 0..(PAGE_SIZE_4K / 8) {
        unsafe { *ptr.add(i) = 0 };
    }
    Ok(paddr)
}

fn publish_prepared_entry<PTE: GenericPTE>(entry: &mut PTE, value: PTE) {
    // Child tables, leaf data, and caller-prepared metadata must all become
    // observable before the live page-table entry. Linux's equivalent
    // table-install path uses a write barrier for the same pointer-chasing
    // publication contract.
    fence(Ordering::Release);
    unsafe { core::ptr::write_volatile(entry, value) };
    // Keep subsequent ownership bookkeeping after the externally visible
    // store even though it only mutates the reservation object.
    compiler_fence(Ordering::SeqCst);
}

const fn p4_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 27)) & (ENTRY_COUNT - 1)
}

const fn p3_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 18)) & (ENTRY_COUNT - 1)
}

const fn p2_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 9)) & (ENTRY_COUNT - 1)
}

const fn p1_index(vaddr: usize) -> usize {
    (vaddr >> 12) & (ENTRY_COUNT - 1)
}

/// A generic page table struct for 64-bit platform.
///
/// It also tracks all intermediate level tables. They will be deallocated
/// When the [`PageTable64`] itself is dropped.
pub struct PageTable64<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> {
    root_paddr: PhysAddr,
    #[cfg(feature = "copy-from")]
    borrowed_entries: bitmaps::Bitmap<ENTRY_COUNT>,
    _phantom: PhantomData<(M, PTE, H)>,
}

impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> PageTable64<M, PTE, H> {
    /// Creates a new page table instance or returns the error.
    ///
    /// It will allocate a new page for the root page table.
    pub fn try_new() -> PagingResult<Self> {
        let root_paddr = Self::alloc_table()?;
        Ok(Self {
            root_paddr,
            #[cfg(feature = "copy-from")]
            borrowed_entries: bitmaps::Bitmap::new(),
            _phantom: PhantomData,
        })
    }

    /// Returns the physical address of the root page table.
    pub const fn root_paddr(&self) -> PhysAddr {
        self.root_paddr
    }

    /// Queries the result of the mapping starting at `vaddr`.
    ///
    /// Returns the physical address of the target frame, mapping flags, and
    /// the page size.
    ///
    /// Returns [`Err(PagingError::NotMapped)`](PagingError::NotMapped) if the
    /// mapping is not present.
    pub fn query(&self, vaddr: M::VirtAddr) -> PagingResult<(PhysAddr, MappingFlags, PageSize)> {
        let (entry, size) = self.get_entry(vaddr)?;
        if !entry.is_present() {
            return Err(PagingError::NotMapped);
        }
        let off = size.align_offset(vaddr.into());
        Ok((entry.paddr().add(off), entry.flags(), size))
    }

    /// Returns the number of intermediate table frames currently needed to
    /// install a mapping at `vaddr`.
    ///
    /// This is an advisory, read-only planning query. A caller may release its
    /// address-space lock, allocate that many [`PreparedPageTableFrames`], and
    /// later call [`PageTable64Cursor::map_prepared`]. The commit rechecks the
    /// path and reports [`PreparedMapError::NeedMore`] if it changed.
    pub fn required_prepared_frames(
        &self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PagingResult<usize> {
        Ok(self.prepared_map_path(vaddr, page_size)?.missing_tables)
    }

    /// Collects present leaf mappings within the given virtual range.
    ///
    /// Unlike repeated [`Self::query`] calls, this walks the page-table tree
    /// structurally and skips absent subtrees instead of probing every 4K
    /// slot in the range.
    pub fn collect_present_leaves(
        &self,
        start: M::VirtAddr,
        size: usize,
    ) -> PagingResult<Vec<(M::VirtAddr, PhysAddr, MappingFlags, PageSize)>> {
        let start_usize: usize = start.into();
        let end_usize = start_usize
            .checked_add(size)
            .ok_or(PagingError::NotAligned)?;
        if !PageSize::Size4K.is_aligned(start_usize) || !PageSize::Size4K.is_aligned(size) {
            return Err(PagingError::NotAligned);
        }

        let leaf_count = self.validate_and_count_present_leaves_recursive(
            self.table_of(self.root_paddr()),
            0,
            0,
            start_usize,
            end_usize,
        )?;
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(leaf_count)
            .map_err(|_| PagingError::NoMemory)?;
        self.collect_present_leaves_recursive(
            self.table_of(self.root_paddr()),
            0,
            0,
            start_usize,
            end_usize,
            &mut leaves,
        )?;
        debug_assert_eq!(leaves.len(), leaf_count);
        Ok(leaves)
    }

    /// Walk the page table recursively.
    ///
    /// When reaching a page table entry, call `pre_func` and `post_func` on the
    /// entry if they are provided. The max number of enumerations in one table
    /// is limited by `limit`. `pre_func` and `post_func` are called before and
    /// after recursively walking the page table.
    ///
    /// The arguments of `*_func` are:
    /// - Current level (starts with `0`): `usize`
    /// - The index of the entry in the current-level table: `usize`
    /// - The virtual address that is mapped to the entry: `M::VirtAddr`
    /// - The reference of the entry: [`&PTE`](GenericPTE)
    pub fn walk<F>(&self, limit: usize, pre_func: Option<&F>, post_func: Option<&F>)
    where
        F: Fn(usize, usize, M::VirtAddr, &PTE),
    {
        self.walk_recursive(
            self.table_of(self.root_paddr()),
            0,
            0.into(),
            limit,
            pre_func,
            post_func,
        )
    }

    /// Gets a cursor to modify the page table.
    ///
    /// The TLB will be flushed automatically when the cursor is dropped.
    pub fn cursor(&mut self) -> PageTable64Cursor<'_, M, PTE, H> {
        PageTable64Cursor::new(self)
    }

    /// Gets a cursor to modify an inactive page table.
    ///
    /// Callers must ensure the page table is not active on any CPU while the
    /// cursor exists; otherwise required TLB invalidations may be skipped.
    pub fn cursor_no_flush(&mut self) -> PageTable64Cursor<'_, M, PTE, H> {
        PageTable64Cursor::new_no_flush(self)
    }
}

// Private implements.
impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> PageTable64<M, PTE, H> {
    fn alloc_table() -> PagingResult<PhysAddr> {
        alloc_zeroed_table_frame::<H>()
    }

    fn table_of<'a>(&self, paddr: PhysAddr) -> &'a [PTE] {
        let ptr = H::phys_to_virt(paddr).as_ptr() as _;
        unsafe { core::slice::from_raw_parts(ptr, ENTRY_COUNT) }
    }

    fn table_of_mut<'a>(&mut self, paddr: PhysAddr) -> &'a mut [PTE] {
        let ptr = H::phys_to_virt(paddr).as_mut_ptr() as _;
        unsafe { core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
    }

    fn next_table<'a>(&self, entry: &PTE) -> PagingResult<&'a [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PagingError::NotMapped)
        } else if entry.is_huge() {
            Err(PagingError::MappedToHugePage)
        } else {
            Ok(self.table_of(entry.paddr()))
        }
    }

    fn next_table_mut<'a>(&mut self, entry: &PTE) -> PagingResult<&'a mut [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PagingError::NotMapped)
        } else if entry.is_huge() {
            Err(PagingError::MappedToHugePage)
        } else {
            Ok(self.table_of_mut(entry.paddr()))
        }
    }

    fn next_table_mut_or_create<'a>(&mut self, entry: &mut PTE) -> PagingResult<&'a mut [PTE]> {
        if entry.is_unused() {
            let paddr = Self::alloc_table()?;
            *entry = GenericPTE::new_table(paddr);
            Ok(self.table_of_mut(paddr))
        } else {
            self.next_table_mut(entry)
        }
    }

    fn get_entry(&self, vaddr: M::VirtAddr) -> PagingResult<(&PTE, PageSize)> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of(self.root_paddr());
            let p4e = &p4[p4_index(vaddr)];
            self.next_table(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &p3[p3_index(vaddr)];
        if p3e.is_huge() {
            return Ok((p3e, PageSize::Size1G));
        }

        let p2 = self.next_table(p3e)?;
        let p2e = &p2[p2_index(vaddr)];
        if p2e.is_huge() {
            return Ok((p2e, PageSize::Size2M));
        }

        let p1 = self.next_table(p2e)?;
        let p1e = &p1[p1_index(vaddr)];
        Ok((p1e, PageSize::Size4K))
    }

    fn get_entry_mut(&mut self, vaddr: M::VirtAddr) -> PagingResult<(&mut PTE, PageSize)> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of_mut(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of_mut(self.root_paddr());
            let p4e = &mut p4[p4_index(vaddr)];
            self.next_table_mut(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &mut p3[p3_index(vaddr)];
        if p3e.is_huge() {
            return Ok((p3e, PageSize::Size1G));
        }

        let p2 = self.next_table_mut(p3e)?;
        let p2e = &mut p2[p2_index(vaddr)];
        if p2e.is_huge() {
            return Ok((p2e, PageSize::Size2M));
        }

        let p1 = self.next_table_mut(p2e)?;
        let p1e = &mut p1[p1_index(vaddr)];
        Ok((p1e, PageSize::Size4K))
    }

    fn get_entry_mut_or_create(
        &mut self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PagingResult<&mut PTE> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of_mut(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of_mut(self.root_paddr());
            let p4e = &mut p4[p4_index(vaddr)];
            self.next_table_mut_or_create(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &mut p3[p3_index(vaddr)];
        if page_size == PageSize::Size1G {
            return Ok(p3e);
        }

        let p2 = self.next_table_mut_or_create(p3e)?;
        let p2e = &mut p2[p2_index(vaddr)];
        if page_size == PageSize::Size2M {
            return Ok(p2e);
        }

        let p1 = self.next_table_mut_or_create(p2e)?;
        let p1e = &mut p1[p1_index(vaddr)];
        Ok(p1e)
    }

    fn prepared_target_level(page_size: PageSize) -> PagingResult<usize> {
        assert!(
            matches!(M::LEVELS, 3 | 4),
            "PageTable64 only supports three or four levels"
        );
        match page_size {
            PageSize::Size1G => Ok(M::LEVELS - 3),
            PageSize::Size2M => Ok(M::LEVELS - 2),
            PageSize::Size4K => Ok(M::LEVELS - 1),
            // Preserve the existing `map()` behavior for this page size even
            // though current 64-bit architecture aliases do not use 1 MiB
            // leaves: it selects the last-level entry and sets the huge bit.
            PageSize::Size1M => Ok(M::LEVELS - 1),
        }
    }

    const fn index_at_level(vaddr: usize, level: usize) -> usize {
        let shift = 12 + (M::LEVELS - 1 - level) * 9;
        (vaddr >> shift) & (ENTRY_COUNT - 1)
    }

    fn prepared_map_path(
        &self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PagingResult<PreparedMapPath> {
        let target_level = Self::prepared_target_level(page_size)?;
        let vaddr: usize = vaddr.into();
        let mut table_paddr = self.root_paddr();

        for level in 0..=target_level {
            let index = Self::index_at_level(vaddr, level);
            let entry = &self.table_of(table_paddr)[index];
            if level == target_level {
                if !entry.is_unused() {
                    return Err(PagingError::AlreadyMapped);
                }
                return Ok(PreparedMapPath {
                    publish_table: table_paddr,
                    publish_index: index,
                    publish_level: level,
                    target_level,
                    missing_tables: 0,
                });
            }
            if entry.is_unused() {
                return Ok(PreparedMapPath {
                    publish_table: table_paddr,
                    publish_index: index,
                    publish_level: level,
                    target_level,
                    missing_tables: target_level - level,
                });
            }
            if entry.is_huge() {
                return Err(PagingError::MappedToHugePage);
            }
            if entry.paddr().as_usize() == 0 {
                return Err(PagingError::NotMapped);
            }
            table_paddr = entry.paddr();
        }
        unreachable!("target page-table level must terminate the path walk")
    }

    fn commit_prepared_map(
        &mut self,
        vaddr: M::VirtAddr,
        target: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
        prepared: &mut PreparedPageTableFrames<H>,
    ) -> Result<PreparedMapCommit, PreparedMapError> {
        let path = self.prepared_map_path(vaddr, page_size)?;
        let available = prepared.frames.len();
        if path.missing_tables > available {
            return Err(PreparedMapError::NeedMore {
                required: path.missing_tables,
                available,
            });
        }

        let page_entry = PTE::new_page(target.align_down(page_size), flags, page_size.is_huge());
        if path.missing_tables == 0 {
            let table = self.table_of_mut(path.publish_table);
            let entry = &mut table[path.publish_index];
            debug_assert!(entry.is_unused());
            publish_prepared_entry(entry, page_entry);
            return Ok(PreparedMapCommit { consumed_frames: 0 });
        }

        let first_reserved = available - path.missing_tables;
        let selected = &prepared.frames[first_reserved..];
        for (offset, table_paddr) in selected.iter().copied().enumerate() {
            let level = path.publish_level + 1 + offset;
            let index = Self::index_at_level(vaddr.into(), level);
            let table = self.table_of_mut(table_paddr);
            let entry = &mut table[index];
            debug_assert!(entry.is_unused());
            if level == path.target_level {
                *entry = page_entry;
            } else {
                *entry = PTE::new_table(selected[offset + 1]);
            }
        }

        let publish_entry = PTE::new_table(selected[0]);
        let table = self.table_of_mut(path.publish_table);
        let entry = &mut table[path.publish_index];
        debug_assert!(entry.is_unused());
        publish_prepared_entry(entry, publish_entry);
        unsafe {
            // This is the commit's only post-publication ownership operation.
            // The exact tail has already been validated and made reachable;
            // shortening ArrayVec's initialized prefix is non-fallible and
            // ensures its Drop cannot reclaim published table frames.
            prepared.frames.set_len(first_reserved);
        }
        Ok(PreparedMapCommit {
            consumed_frames: path.missing_tables,
        })
    }

    fn walk_recursive<F>(
        &self,
        table: &[PTE],
        level: usize,
        start_vaddr: M::VirtAddr,
        limit: usize,
        pre_func: Option<&F>,
        post_func: Option<&F>,
    ) where
        F: Fn(usize, usize, M::VirtAddr, &PTE),
    {
        let start_vaddr_usize: usize = start_vaddr.into();
        let mut n = 0;
        for (i, entry) in table.iter().enumerate() {
            let vaddr_usize = start_vaddr_usize + (i << (12 + (M::LEVELS - 1 - level) * 9));
            let vaddr = vaddr_usize.into();
            let is_leaf = level == M::LEVELS - 1 || entry.is_huge();

            if entry.is_unused() || (is_leaf && !entry.is_present()) {
                continue;
            }
            if let Some(func) = pre_func {
                func(level, i, vaddr, entry);
            }
            if !is_leaf && let Ok(table) = self.next_table(entry) {
                self.walk_recursive(table, level + 1, vaddr, limit, pre_func, post_func);
            }
            if let Some(func) = post_func {
                func(level, i, vaddr, entry);
            }
            n += 1;
            if n >= limit {
                break;
            }
        }
    }

    fn collect_present_leaves_recursive(
        &self,
        table: &[PTE],
        level: usize,
        table_start: usize,
        range_start: usize,
        range_end: usize,
        out: &mut Vec<(M::VirtAddr, PhysAddr, MappingFlags, PageSize)>,
    ) -> PagingResult {
        let shift = 12 + (M::LEVELS - 1 - level) * 9;
        let span = 1usize << shift;

        for (index, entry) in table.iter().enumerate() {
            let is_leaf = level == M::LEVELS - 1 || entry.is_huge();
            if entry.is_unused() || (is_leaf && !entry.is_present()) {
                continue;
            }

            let entry_start = table_start + (index << shift);
            let Some(entry_end) = entry_start.checked_add(span) else {
                return Err(PagingError::NotAligned);
            };
            if entry_end <= range_start || entry_start >= range_end {
                continue;
            }

            if !is_leaf {
                let child = self.next_table(entry)?;
                self.collect_present_leaves_recursive(
                    child,
                    level + 1,
                    entry_start,
                    range_start,
                    range_end,
                    out,
                )?;
                continue;
            }

            let page_size = match span {
                x if x == PageSize::Size4K as usize => PageSize::Size4K,
                x if x == PageSize::Size2M as usize => PageSize::Size2M,
                x if x == PageSize::Size1G as usize => PageSize::Size1G,
                x if x == PageSize::Size1M as usize => PageSize::Size1M,
                _ => return Err(PagingError::NotAligned),
            };
            if range_start > entry_start || range_end < entry_end {
                return Err(PagingError::NotAligned);
            }

            assert!(
                out.len() < out.capacity(),
                "preallocated present-leaf buffer exhausted"
            );
            out.push((entry_start.into(), entry.paddr(), entry.flags(), page_size));
        }

        Ok(())
    }

    fn validate_and_count_present_leaves_recursive(
        &self,
        table: &[PTE],
        level: usize,
        table_start: usize,
        range_start: usize,
        range_end: usize,
    ) -> PagingResult<usize> {
        let shift = 12 + (M::LEVELS - 1 - level) * 9;
        let span = 1usize << shift;
        let mut count = 0usize;

        for (index, entry) in table.iter().enumerate() {
            let is_leaf = level == M::LEVELS - 1 || entry.is_huge();
            if entry.is_unused() || (is_leaf && !entry.is_present()) {
                continue;
            }

            let entry_start = table_start + (index << shift);
            let Some(entry_end) = entry_start.checked_add(span) else {
                return Err(PagingError::NotAligned);
            };
            if entry_end <= range_start || entry_start >= range_end {
                continue;
            }

            if is_leaf {
                if range_start > entry_start || range_end < entry_end {
                    return Err(PagingError::NotAligned);
                }
                count = count.checked_add(1).ok_or(PagingError::NoMemory)?;
                continue;
            }

            let child = self.next_table(entry)?;
            let child_count = self.validate_and_count_present_leaves_recursive(
                child,
                level + 1,
                entry_start,
                range_start,
                range_end,
            )?;
            count = count
                .checked_add(child_count)
                .ok_or(PagingError::NoMemory)?;
        }

        Ok(count)
    }

    fn dealloc_tree(&self, table_paddr: PhysAddr, level: usize) {
        // don't free the entries in last level, they are not array.
        if level < M::LEVELS - 1 {
            for entry in self.table_of(table_paddr) {
                if self.next_table(entry).is_ok() {
                    self.dealloc_tree(entry.paddr(), level + 1);
                }
            }
        }
        H::dealloc_frame(table_paddr);
    }
}

impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> Drop for PageTable64<M, PTE, H> {
    fn drop(&mut self) {
        let root = self.table_of(self.root_paddr);
        #[allow(unused_variables)]
        for (i, entry) in root.iter().enumerate() {
            #[cfg(feature = "copy-from")]
            if self.borrowed_entries.get(i) {
                continue;
            }
            if self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
        }
        H::dealloc_frame(self.root_paddr());
    }
}

/// A cursor created by [`PageTable64::cursor`] to modify the page table.
pub struct PageTable64Cursor<'a, M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> {
    inner: &'a mut PageTable64<M, PTE, H>,
    flusher: TlbFlusher<M>,
    flush_on_drop: bool,
}

impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> Deref
    for PageTable64Cursor<'_, M, PTE, H>
{
    type Target = PageTable64<M, PTE, H>;

    fn deref(&self) -> &PageTable64<M, PTE, H> {
        self.inner
    }
}

impl<'a, M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> PageTable64Cursor<'a, M, PTE, H> {
    fn new(inner: &'a mut PageTable64<M, PTE, H>) -> Self {
        Self {
            inner,
            flusher: TlbFlusher::None,
            flush_on_drop: true,
        }
    }

    fn new_no_flush(inner: &'a mut PageTable64<M, PTE, H>) -> Self {
        Self {
            inner,
            flusher: TlbFlusher::None,
            flush_on_drop: false,
        }
    }

    fn push(&mut self, vaddr: M::VirtAddr) {
        match self.flusher {
            TlbFlusher::None => {
                let mut arr = ArrayVec::new();
                arr.push(vaddr);
                self.flusher = TlbFlusher::Array(arr);
            }
            TlbFlusher::Array(ref mut arr) => {
                if arr.try_push(vaddr).is_err() {
                    self.flusher = TlbFlusher::Full;
                }
            }
            TlbFlusher::Full => {}
        }
    }

    fn rollback_region_mappings(
        &mut self,
        mappings: &[(M::VirtAddr, PhysAddr, MappingFlags, PageSize)],
    ) {
        for &(vaddr, expected_paddr, expected_flags, expected_page_size) in mappings.iter().rev() {
            let vaddr_usize: usize = vaddr.into();
            let removed = self.unmap(vaddr).unwrap_or_else(|error| {
                panic!("map_region rollback failed at {vaddr_usize:#x}: {error:?}")
            });
            assert_eq!(
                removed,
                (expected_paddr, expected_flags, expected_page_size),
                "map_region rollback mismatch at {vaddr_usize:#x}"
            );
        }
    }

    fn drain_present_leaves_recursive(
        &mut self,
        table_paddr: PhysAddr,
        level: usize,
        table_start: usize,
        range_start: usize,
        range_end: usize,
        out: &mut Vec<(M::VirtAddr, PhysAddr, MappingFlags, PageSize)>,
    ) -> PagingResult {
        let shift = 12 + (M::LEVELS - 1 - level) * 9;
        let span = 1usize << shift;

        for index in 0..ENTRY_COUNT {
            let entry_start = table_start + (index << shift);
            let Some(entry_end) = entry_start.checked_add(span) else {
                return Err(PagingError::NotAligned);
            };

            enum DrainStep {
                Skip,
                Preserve,
                Leaf(PhysAddr, MappingFlags, PageSize),
                Child(PhysAddr),
            }

            let step = {
                let table = self.inner.table_of_mut(table_paddr);
                let entry = &mut table[index];
                let is_leaf = level == M::LEVELS - 1 || entry.is_huge();
                if entry.is_unused() || (is_leaf && !entry.is_present()) {
                    DrainStep::Skip
                } else if entry_end <= range_start || entry_start >= range_end {
                    DrainStep::Preserve
                } else {
                    if is_leaf {
                        if range_start > entry_start || range_end < entry_end {
                            return Err(PagingError::NotAligned);
                        }
                        let page_size = match span {
                            x if x == PageSize::Size4K as usize => PageSize::Size4K,
                            x if x == PageSize::Size2M as usize => PageSize::Size2M,
                            x if x == PageSize::Size1G as usize => PageSize::Size1G,
                            x if x == PageSize::Size1M as usize => PageSize::Size1M,
                            _ => return Err(PagingError::NotAligned),
                        };
                        assert!(
                            out.len() < out.capacity(),
                            "preallocated drain journal exhausted"
                        );
                        let leaf = DrainStep::Leaf(entry.paddr(), entry.flags(), page_size);
                        entry.clear();
                        leaf
                    } else {
                        DrainStep::Child(entry.paddr())
                    }
                }
            };

            match step {
                DrainStep::Skip => {}
                DrainStep::Preserve => {}
                DrainStep::Leaf(paddr, flags, page_size) => {
                    out.push((entry_start.into(), paddr, flags, page_size));
                    self.push(entry_start.into());
                }
                DrainStep::Child(child_paddr) => {
                    self.drain_present_leaves_recursive(
                        child_paddr,
                        level + 1,
                        entry_start,
                        range_start,
                        range_end,
                        out,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// Maps a virtual page to a physical frame with the given `page_size`
    /// and mapping `flags`.
    ///
    /// The virtual page starts at `vaddr`, and the physical frame starts at
    /// `target`. If the `target` is not aligned to the `page_size`, it will be
    /// aligned down automatically.
    ///
    /// Returns [`Err(PagingError::AlreadyMapped)`](PagingError::AlreadyMapped)
    /// if the mapping is already present.
    pub fn map(
        &mut self,
        vaddr: M::VirtAddr,
        target: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
    ) -> PagingResult {
        // `vaddr` does not need to be page-aligned here; `get_entry_mut_or_create`
        // internally maps `vaddr` to its corresponding page table entry (PTE).
        let entry = self.inner.get_entry_mut_or_create(vaddr, page_size)?;
        if !entry.is_unused() {
            return Err(PagingError::AlreadyMapped);
        }
        *entry = GenericPTE::new_page(target.align_down(page_size), flags, page_size.is_huge());
        #[cfg(target_arch = "loongarch64")]
        if flags.contains(MappingFlags::USER) {
            self.push(vaddr);
        }
        // No TLB flush for non-user fresh mappings: the entry was unused, so no
        // CPU can hold a stale TLB entry for this VA. LoongArch user mappings are
        // the exception: hardware page-walk may cache invalid user TLB entries
        // before the kernel populates the PTE, so flush only those faulted user
        // VAs while keeping the RV/non-user fresh-map optimization.
        Ok(())
    }

    /// Maps one leaf using table frames allocated and zeroed before entering
    /// the current page-table critical section.
    ///
    /// The path is rechecked before any mutation. If part of the path is
    /// absent, its complete child subtree is built in unreachable reserved
    /// frames and then made visible with one topmost entry publication. This
    /// method never allocates or deallocates. Every error leaves the live page
    /// table unchanged and retains all frames in `prepared`.
    ///
    /// The caller should move or drop `prepared` only after leaving the
    /// critical section so unused frames are reclaimed lock-external.
    pub fn map_prepared(
        &mut self,
        vaddr: M::VirtAddr,
        target: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
        prepared: &mut PreparedPageTableFrames<H>,
    ) -> Result<PreparedMapCommit, PreparedMapError> {
        let committed = self
            .inner
            .commit_prepared_map(vaddr, target, page_size, flags, prepared)?;
        #[cfg(target_arch = "loongarch64")]
        if flags.contains(MappingFlags::USER) {
            self.push(vaddr);
        }
        Ok(committed)
    }

    /// Remaps the mapping starting at `vaddr`, updates both the physical
    /// address and flags.
    ///
    /// Returns the page size of the mapping.
    ///
    /// Returns [`Err(PagingError::NotMapped)`](PagingError::NotMapped) if the
    /// intermediate level tables of the mapping is not present.
    pub fn remap(
        &mut self,
        vaddr: M::VirtAddr,
        paddr: PhysAddr,
        flags: MappingFlags,
    ) -> PagingResult<PageSize> {
        let (entry, size) = self.inner.get_entry_mut(vaddr)?;
        entry.set_paddr(paddr);
        entry.set_flags(flags, size.is_huge());
        self.push(vaddr);
        Ok(size)
    }

    /// Updates the flags of the mapping starting at `vaddr`.
    ///
    /// Returns the page size of the mapping.
    ///
    /// Returns [`Err(PagingError::NotMapped)`](PagingError::NotMapped) if the
    /// mapping is not present.
    pub fn protect(&mut self, vaddr: M::VirtAddr, flags: MappingFlags) -> PagingResult<PageSize> {
        let (entry, size) = self.inner.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PagingError::NotMapped);
        }
        entry.set_flags(flags, size.is_huge());
        self.push(vaddr);
        Ok(size)
    }

    /// Unmaps the mapping starting at `vaddr`.
    ///
    /// Returns [`Err(PagingError::NotMapped)`](PagingError::NotMapped) if the
    /// mapping is not present.
    pub fn unmap(
        &mut self,
        vaddr: M::VirtAddr,
    ) -> PagingResult<(PhysAddr, MappingFlags, PageSize)> {
        let (entry, size) = self.inner.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            entry.clear();
            return Err(PagingError::NotMapped);
        }
        let paddr = entry.paddr();
        let flags = entry.flags();
        entry.clear();
        self.push(vaddr);
        Ok((paddr, flags, size))
    }

    /// Maps a contiguous virtual memory region to a contiguous physical memory
    /// region with the given mapping `flags`.
    ///
    /// The virtual and physical memory regions start at `vaddr` and `paddr`
    /// respectively. The region size is `size`. The addresses and `size` must
    /// be aligned to 4K, otherwise it will return
    /// [`Err(PagingError::NotAligned)`].
    ///
    /// When `allow_huge` is true, it will try to map the region with huge pages
    /// if possible. Otherwise, it will map the region with 4K pages.
    /// If any mapping or journal allocation fails, mappings created by this
    /// call are removed before the error is returned.
    ///
    /// [`Err(PagingError::NotAligned)`]: PagingError::NotAligned
    pub fn map_region(
        &mut self,
        vaddr: M::VirtAddr,
        get_paddr: impl Fn(M::VirtAddr) -> PhysAddr,
        size: usize,
        flags: MappingFlags,
        allow_huge: bool,
    ) -> PagingResult {
        let mut vaddr_usize: usize = vaddr.into();
        let mut size = size;
        if !PageSize::Size4K.is_aligned(vaddr_usize) || !PageSize::Size4K.is_aligned(size) {
            return Err(PagingError::NotAligned);
        }
        trace!(
            "map_region({:#x}): [{:#x}, {:#x}) {:?}",
            self.root_paddr(),
            vaddr_usize,
            vaddr_usize + size,
            flags,
        );
        let mut mapped = Vec::new();
        while size > 0 {
            if mapped.try_reserve(1).is_err() {
                self.rollback_region_mappings(&mapped);
                return Err(PagingError::NoMemory);
            }

            let vaddr = vaddr_usize.into();
            let paddr = get_paddr(vaddr);
            let page_size = if allow_huge {
                if PageSize::Size1G.is_aligned(vaddr_usize)
                    && paddr.is_aligned(PageSize::Size1G)
                    && size >= PageSize::Size1G as usize
                {
                    PageSize::Size1G
                } else if PageSize::Size2M.is_aligned(vaddr_usize)
                    && paddr.is_aligned(PageSize::Size2M)
                    && size >= PageSize::Size2M as usize
                {
                    PageSize::Size2M
                } else {
                    PageSize::Size4K
                }
            } else {
                PageSize::Size4K
            };
            let represented =
                PTE::new_page(paddr.align_down(page_size), flags, page_size.is_huge());
            if let Err(error) = self.map(vaddr, paddr, page_size, flags) {
                error!(
                    "failed to map page: {vaddr_usize:#x?}({page_size:?}) -> {paddr:#x?}, \
                     {error:?}"
                );
                self.rollback_region_mappings(&mapped);
                return Err(error);
            }
            mapped.push((vaddr, represented.paddr(), represented.flags(), page_size));

            vaddr_usize += page_size as usize;
            size -= page_size as usize;
        }
        Ok(())
    }

    /// Unmaps a contiguous virtual memory region.
    ///
    /// The region must be mapped before using [`Self::map_region`], or
    /// unexpected behaviors may occur. It can deal with huge pages
    /// automatically.
    pub fn unmap_region(&mut self, vaddr: M::VirtAddr, size: usize) -> PagingResult {
        let mut vaddr_usize: usize = vaddr.into();
        let mut size = size;
        trace!(
            "unmap_region({:#x}) [{:#x}, {:#x})",
            self.root_paddr(),
            vaddr_usize,
            vaddr_usize + size,
        );
        while size > 0 {
            let vaddr = vaddr_usize.into();
            let (_, _, page_size) = self
                .unmap(vaddr)
                .inspect_err(|e| error!("failed to unmap page: {vaddr_usize:#x?}, {e:?}"))?;

            assert!(page_size.is_aligned(vaddr_usize));
            assert!(page_size as usize <= size);
            vaddr_usize += page_size as usize;
            size -= page_size as usize;
        }
        Ok(())
    }

    /// Removes and returns the present leaf mappings fully contained in the
    /// given range.
    ///
    /// This is a destructive counterpart to `collect_present_leaves()`: it
    /// first validates that the requested range does not partially overlap any
    /// present leaf, then clears matching leaves in place and returns them.
    pub fn drain_present_leaves(
        &mut self,
        start: M::VirtAddr,
        size: usize,
    ) -> PagingResult<Vec<(M::VirtAddr, PhysAddr, MappingFlags, PageSize)>> {
        let start_usize: usize = start.into();
        let end_usize = start_usize
            .checked_add(size)
            .ok_or(PagingError::NotAligned)?;
        if !PageSize::Size4K.is_aligned(start_usize) || !PageSize::Size4K.is_aligned(size) {
            return Err(PagingError::NotAligned);
        }

        let leaf_count = {
            let root = self.inner.table_of(self.inner.root_paddr());
            self.inner.validate_and_count_present_leaves_recursive(
                root,
                0,
                0,
                start_usize,
                end_usize,
            )?
        };
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(leaf_count)
            .map_err(|_| PagingError::NoMemory)?;
        if let Err(error) = self.drain_present_leaves_recursive(
            self.inner.root_paddr(),
            0,
            0,
            start_usize,
            end_usize,
            &mut leaves,
        ) {
            panic!("validated present-leaf drain failed: {error:?}");
        }
        debug_assert_eq!(leaves.len(), leaf_count);
        Ok(leaves)
    }

    /// Updates mapping flags of a contiguous virtual memory region.
    ///
    /// The region must be mapped before using [`Self::map_region`], or
    /// unexpected behaviors may occur. It can deal with huge pages
    /// automatically.
    pub fn protect_region(
        &mut self,
        vaddr: M::VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> PagingResult {
        let mut vaddr_usize: usize = vaddr.into();
        let mut size = size;
        trace!(
            "protect_region({:#x}) [{:#x}, {:#x}) {:?}",
            self.root_paddr(),
            vaddr_usize,
            vaddr_usize + size,
            flags,
        );
        while size > 0 {
            let vaddr = vaddr_usize.into();
            let page_size = match self.inner.get_entry_mut(vaddr) {
                Ok((entry, page_size)) => {
                    if entry.is_present() {
                        entry.set_flags(flags, page_size.is_huge());
                        self.push(vaddr);
                    }
                    // ignore if not present

                    page_size
                }
                Err(PagingError::NotMapped) => PageSize::Size4K,
                Err(e) => {
                    error!("failed to protect page: {vaddr_usize:#x?}, {e:?}");
                    return Err(e);
                }
            };

            assert!(page_size.is_aligned(vaddr_usize));
            assert!(page_size as usize <= size);
            vaddr_usize += page_size as usize;
            size -= page_size as usize;
        }
        Ok(())
    }

    /// Copy entries from another page table within the given virtual memory
    /// range.
    #[cfg(feature = "copy-from")]
    pub fn copy_from(&mut self, other: &PageTable64<M, PTE, H>, start: M::VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let src_table = self.table_of(other.root_paddr);
        let root_paddr = self.root_paddr;
        let dst_table = self.inner.table_of_mut(root_paddr);
        let index_fn = if M::LEVELS == 3 {
            p3_index
        } else if M::LEVELS == 4 {
            p4_index
        } else {
            unreachable!()
        };
        let start_idx = index_fn(start.into());
        let end_idx = index_fn(start.into() + size - 1) + 1;
        assert!(start_idx < ENTRY_COUNT);
        assert!(end_idx <= ENTRY_COUNT);
        for i in start_idx..end_idx {
            let entry = &mut dst_table[i];
            if !self.inner.borrowed_entries.set(i, true) && self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
            *entry = src_table[i];
        }
        self.flusher = TlbFlusher::Full;
    }

    /// Flushes the TLB according to the recorded flush requests.
    pub fn flush(&mut self) {
        #[cfg(not(docsrs))]
        match &self.flusher {
            TlbFlusher::None => {}
            TlbFlusher::Array(addrs) => {
                for vaddr in addrs.iter() {
                    M::flush_tlb(Some(*vaddr));
                }
            }
            TlbFlusher::Full => {
                M::flush_tlb(None);
            }
        }
        self.flusher = TlbFlusher::None;
    }
}

impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> Drop
    for PageTable64Cursor<'_, M, PTE, H>
{
    fn drop(&mut self) {
        if self.flush_on_drop {
            self.flush();
        }
    }
}
