use alloc::vec::Vec;
use core::{
    fmt,
    marker::PhantomData,
    ops::Deref,
    sync::atomic::{Ordering, compiler_fence, fence},
};

use arrayvec::ArrayVec;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr};
use page_table_entry::x86_64::{Pkey, PkeyPTE};

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

    /// Ensures that this reservation retains at least `frame_count` frames.
    ///
    /// Callers may reuse one reservation across a sequence of prepared leaf
    /// commits. Frames consumed by earlier commits can be replenished here
    /// after leaving the page-table critical section, avoiding a fresh
    /// maximum-size reservation for every leaf.
    ///
    /// Allocation is transactional: an error leaves the pre-existing
    /// reservation unchanged and reclaims every frame allocated by this call.
    /// This method must not be invoked while holding a page-table lock.
    pub fn try_reserve_to(&mut self, frame_count: usize) -> Result<(), PrepareTableFramesError> {
        if frame_count > MAX_PREPARED_TABLE_FRAMES_64 {
            return Err(PrepareTableFramesError::TooMany {
                requested: frame_count,
                maximum: MAX_PREPARED_TABLE_FRAMES_64,
            });
        }
        let additional = frame_count.saturating_sub(self.frames.len());
        if additional == 0 {
            return Ok(());
        }

        let mut fresh = ArrayVec::<PhysAddr, MAX_PREPARED_TABLE_FRAMES_64>::new();
        for _ in 0..additional {
            let Ok(frame) = alloc_zeroed_table_frame::<H>() else {
                for frame in fresh {
                    H::dealloc_frame(frame);
                }
                return Err(PrepareTableFramesError::NoMemory);
            };
            fresh
                .try_push(frame)
                .expect("validated prepared-frame capacity");
        }
        for frame in fresh {
            self.frames
                .try_push(frame)
                .expect("validated prepared-frame capacity");
        }
        Ok(())
    }

    /// Replenishes this reusable reservation to the largest supported
    /// 64-bit page-table path.
    pub fn try_reserve_max(&mut self) -> Result<(), PrepareTableFramesError> {
        self.try_reserve_to(MAX_PREPARED_TABLE_FRAMES_64)
    }

    /// Number of frames currently retained by this reservation.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Returns whether this reservation retains no frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Transfers one preallocated, zeroed table frame to a caller building an
    /// unreachable table.  The caller must either publish the table or retain
    /// responsibility for returning it through the normal handler path.
    fn take_one(&mut self) -> Option<PhysAddr> {
        self.frames.pop()
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
    owns_root: bool,
    #[cfg(feature = "copy-from")]
    borrowed_entries: bitmaps::Bitmap<ENTRY_COUNT>,
    _phantom: PhantomData<(M, PTE, H)>,
}

/// A detached, fully populated P1 table replaced by a caller-owned 2 MiB leaf.
///
/// The replacement frame is deliberately *not* owned by this value: its
/// allocator and the code that copied the source contents retain that
/// responsibility. The detached P1 frame remains owned by this value after
/// replacement. Dropping it releases that table frame; passing it to
/// [`PageTable64Cursor::rollback_2m_pte_replacement`] makes it reachable
/// again.
/// `leaves` is an exact pre-publication snapshot, including architecture
/// private bits such as x86 accessed and dirty state.
pub struct ReplacedPteRun<PTE: GenericPTE, H: PagingHandler> {
    p1_table: PhysAddr,
    leaves: ArrayVec<PTE, ENTRY_COUNT>,
    replacement: PTE,
    _handler: PhantomData<H>,
}

impl<PTE: GenericPTE, H: PagingHandler> ReplacedPteRun<PTE, H> {
    /// Physical address of the detached P1 table frame.
    pub const fn p1_table(&self) -> PhysAddr {
        self.p1_table
    }

    /// Exact old PTEs in increasing virtual-address order.
    pub fn leaves(&self) -> &[PTE] {
        &self.leaves
    }

    /// Exact huge leaf published by the replacement transaction.
    pub const fn replacement(&self) -> PTE {
        self.replacement
    }
}

impl<PTE: GenericPTE, H: PagingHandler> Drop for ReplacedPteRun<PTE, H> {
    fn drop(&mut self) {
        H::dealloc_frame(self.p1_table);
    }
}

impl<M: PagingMetaData, PTE: GenericPTE, H: PagingHandler> PageTable64<M, PTE, H> {
    /// Creates a new page table instance or returns the error.
    ///
    /// It will allocate a new page for the root page table.
    pub fn try_new() -> PagingResult<Self> {
        let root_paddr = Self::alloc_table()?;
        Ok(Self {
            root_paddr,
            owns_root: true,
            #[cfg(feature = "copy-from")]
            borrowed_entries: bitmaps::Bitmap::new(),
            _phantom: PhantomData,
        })
    }

    /// Returns the physical address of the root page table.
    pub const fn root_paddr(&self) -> PhysAddr {
        self.root_paddr
    }

    /// Borrows an already-installed page-table root.  The returned object
    /// never frees the root (nor its existing hierarchy); callers must keep
    /// the root active and serialize every mutation externally.
    pub unsafe fn from_existing_root(root_paddr: PhysAddr) -> Self {
        Self {
            root_paddr,
            owns_root: false,
            #[cfg(feature = "copy-from")]
            borrowed_entries: bitmaps::Bitmap::new(),
            _phantom: PhantomData,
        }
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

    /// Replaces the huge leaf covering `vaddr` with lower-level leaves, using
    /// only frames already owned by `prepared`. A 2 MiB leaf consumes one P1
    /// frame; a 1 GiB leaf consumes a P2 frame and then one P1 frame for the
    /// selected 2 MiB child. All child entries are initialized before their
    /// parent is published, so no fallible operation follows publication.
    fn demote_leaf_to_4k_prepared(
        &mut self,
        vaddr: M::VirtAddr,
        prepared: &mut PreparedPageTableFrames<H>,
    ) -> PagingResult<PageSize> {
        let vaddr_usize: usize = vaddr.into();
        let p3_paddr = if M::LEVELS == 3 {
            self.root_paddr()
        } else if M::LEVELS == 4 {
            let p4 = self.table_of(self.root_paddr());
            let p4e = &p4[p4_index(vaddr_usize)];
            if p4e.is_unused() || p4e.is_huge() {
                return Err(PagingError::NotMapped);
            }
            p4e.paddr()
        } else {
            unreachable!("PageTable64 only supports three or four levels")
        };

        let p3_index = p3_index(vaddr_usize);
        let (p3_paddr_leaf, p3_flags, is_1g) = {
            let p3 = self.table_of(p3_paddr);
            let entry = &p3[p3_index];
            if entry.is_unused() || !entry.is_present() {
                return Err(PagingError::NotMapped);
            }
            (entry.paddr(), entry.flags(), entry.is_huge())
        };
        let required_frames = if is_1g { 2 } else { 1 };
        if prepared.frames.len() < required_frames {
            return Err(PagingError::NoMemory);
        }
        if is_1g {
            let p2_frame = prepared.frames.pop().ok_or(PagingError::NoMemory)?;
            let p2 = self.table_of_mut(p2_frame);
            for (index, entry) in p2.iter_mut().enumerate() {
                *entry = PTE::new_page(
                    p3_paddr_leaf.add(index * PageSize::Size2M as usize),
                    p3_flags,
                    true,
                );
            }
            // The fully initialized child is now reachable. No operation
            // below this point can fail except an internal invariant break.
            self.table_of_mut(p3_paddr)[p3_index] = PTE::new_table(p2_frame);
        }

        let p2_paddr = {
            let p3 = self.table_of(p3_paddr);
            let p3e = &p3[p3_index];
            if p3e.is_huge() || p3e.is_unused() {
                return Err(PagingError::NotMapped);
            }
            p3e.paddr()
        };
        let p2_index = p2_index(vaddr_usize);
        let (p2_paddr_leaf, p2_flags, is_2m) = {
            let p2 = self.table_of(p2_paddr);
            let entry = &p2[p2_index];
            if entry.is_unused() || !entry.is_present() {
                return Err(PagingError::NotMapped);
            }
            (entry.paddr(), entry.flags(), entry.is_huge())
        };
        if !is_2m {
            return Ok(PageSize::Size4K);
        }
        let p1_frame = prepared.frames.pop().ok_or(PagingError::NoMemory)?;
        let p1 = self.table_of_mut(p1_frame);
        for (index, entry) in p1.iter_mut().enumerate() {
            *entry = PTE::new_page(
                p2_paddr_leaf.add(index * PageSize::Size4K as usize),
                p2_flags,
                false,
            );
        }
        self.table_of_mut(p2_paddr)[p2_index] = PTE::new_table(p1_frame);
        Ok(if is_1g { PageSize::Size1G } else { PageSize::Size2M })
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

    /// Locates the P2 entry covering one 2 MiB-aligned address.
    fn p2_entry_location(&self, vaddr: M::VirtAddr) -> PagingResult<(PhysAddr, usize)> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of(self.root_paddr());
            self.next_table(&p4[p4_index(vaddr)])?
        } else {
            unreachable!()
        };
        let p3e = &p3[p3_index(vaddr)];
        if p3e.is_huge() {
            return Err(PagingError::MappedToHugePage);
        }
        let _ = self.next_table(p3e)?;
        Ok((p3e.paddr(), p2_index(vaddr)))
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
        if !self.owns_root {
            return;
        }
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

    /// Atomically replaces a homogeneous P1 run with one
    /// caller-preallocated 2 MiB P2 leaf.
    ///
    /// Validation completes before the live P2 entry is changed. The only
    /// publication is one release-ordered aligned entry store, so a walker
    /// can observe either the old P1 table or the new huge leaf, never an
    /// unmapped interval. The caller must have copied all source content into
    /// `replacement` before this call; this operation only changes translation
    /// state. The returned value owns the detached P1 frame and an exact PTE
    /// snapshot for rollback/accounting.
    ///
    /// Present source leaves need not be physically contiguous; all present
    /// 4 KiB leaves must have identical flags. Absent leaves are demand-zero
    /// slots already initialized in `replacement`. `replacement` must name
    /// one aligned, preallocated 2 MiB frame; its ownership remains with the
    /// caller on both success and failure.
    pub fn replace_2m_pte_run(
        &mut self,
        vaddr: M::VirtAddr,
        replacement: PhysAddr,
        flags: MappingFlags,
    ) -> PagingResult<ReplacedPteRun<PTE, H>> {
        let vaddr_usize: usize = vaddr.into();
        if !PageSize::Size2M.is_aligned(vaddr_usize)
            || !PageSize::Size2M.is_aligned(replacement.as_usize())
        {
            return Err(PagingError::NotAligned);
        }

        let (p2_table, p2_index) = self.inner.p2_entry_location(vaddr)?;
        let p1_table = {
            let p2e = &self.inner.table_of(p2_table)[p2_index];
            if p2e.is_huge() {
                return Err(PagingError::NotPromotable);
            }
            if p2e.paddr().as_usize() == 0 {
                return Err(PagingError::NotMapped);
            }
            p2e.paddr()
        };

        let mut leaves = ArrayVec::<PTE, ENTRY_COUNT>::new();
        let p1 = self.inner.table_of(p1_table);
        let mut source_flags = None;
        for leaf in p1.iter().copied() {
            if !leaf.is_present() {
                leaves.push(leaf);
                continue;
            }
            if leaf.is_huge()
                || source_flags.is_some_and(|source_flags| leaf.flags() != source_flags)
            {
                return Err(PagingError::NotPromotable);
            }
            source_flags = Some(leaf.flags());
            // Capacity is exactly the P1 fanout and no allocation occurs.
            leaves.push(leaf);
        }
        if source_flags.is_some_and(|source_flags| source_flags != flags) {
            return Err(PagingError::NotPromotable);
        }

        let replacement_entry = PTE::new_page(replacement, flags, true);
        let p2e = &mut self.inner.table_of_mut(p2_table)[p2_index];
        // The cursor is exclusive. Recheck the link anyway so malformed page
        // tables cannot detach a frame different from the one validated.
        if p2e.is_huge() || p2e.paddr() != p1_table {
            return Err(PagingError::RollbackMismatch);
        }
        publish_prepared_entry(p2e, replacement_entry);
        // A single INVLPG cannot invalidate arbitrary old 4 KiB translations
        // elsewhere in this 2 MiB span. Defer a full flush on cursor drop.
        self.flusher = TlbFlusher::Full;

        Ok(ReplacedPteRun {
            p1_table,
            leaves,
            replacement: replacement_entry,
            _handler: PhantomData,
        })
    }

    /// Atomically replaces one 2 MiB leaf with a caller-prepared, fully
    /// populated P1 table.  All new 4 KiB leaves and the P1 frame are checked
    /// and initialized before the live PDE is published, so failure leaves
    /// the huge mapping intact.
    pub fn replace_2m_huge_leaf_with_pte_run(
        &mut self,
        vaddr: M::VirtAddr,
        replacements: &[PhysAddr],
        flags: MappingFlags,
        prepared: &mut PreparedPageTableFrames<H>,
    ) -> PagingResult<PhysAddr> {
        let vaddr_usize: usize = vaddr.into();
        if !PageSize::Size2M.is_aligned(vaddr_usize) || replacements.len() != ENTRY_COUNT {
            return Err(PagingError::NotAligned);
        }
        if replacements
            .iter()
            .any(|paddr| !PageSize::Size4K.is_aligned(paddr.as_usize()))
        {
            return Err(PagingError::NotAligned);
        }
        let (p2_table, p2_index) = self.inner.p2_entry_location(vaddr)?;
        let original = self.inner.table_of(p2_table)[p2_index];
        if !original.is_present() || !original.is_huge() || original.flags() != flags {
            return Err(PagingError::NotPromotable);
        }
        let p1_table = prepared.take_one().ok_or(PagingError::NoMemory)?;
        let p1 = self.inner.table_of_mut(p1_table);
        for (entry, paddr) in p1.iter_mut().zip(replacements.iter().copied()) {
            *entry = PTE::new_page(paddr, flags, false);
        }
        let p2e = &mut self.inner.table_of_mut(p2_table)[p2_index];
        if !p2e.is_huge() || p2e.bits() != original.bits() {
            H::dealloc_frame(p1_table);
            return Err(PagingError::RollbackMismatch);
        }
        publish_prepared_entry(p2e, PTE::new_table(p1_table));
        self.flusher = TlbFlusher::Full;
        Ok(original.paddr())
    }

    /// Restores a replacement returned by [`Self::replace_2m_pte_run`].
    ///
    /// Like promotion, rollback has one publication store and therefore never
    /// exposes a hole. The exact promoted leaf must still be installed.
    pub fn rollback_2m_pte_replacement(
        &mut self,
        vaddr: M::VirtAddr,
        replacement: ReplacedPteRun<PTE, H>,
    ) -> PagingResult {
        let vaddr_usize: usize = vaddr.into();
        if !PageSize::Size2M.is_aligned(vaddr_usize) {
            return Err(PagingError::NotAligned);
        }
        let (p2_table, p2_index) = self.inner.p2_entry_location(vaddr)?;
        let p2e = &mut self.inner.table_of_mut(p2_table)[p2_index];
        if !p2e.is_huge() || p2e.bits() != replacement.replacement.bits() {
            return Err(PagingError::RollbackMismatch);
        }
        publish_prepared_entry(p2e, PTE::new_table(replacement.p1_table));
        self.flusher = TlbFlusher::Full;
        // The P1 table is reachable again. Its PTE payload was never touched,
        // so only relinquish this transaction's detached-table ownership.
        core::mem::forget(replacement);
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
        // No TLB flush for fresh mappings: the entry was unused, so no CPU can
        // hold a stale TLB entry for this VA.
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

    /// Returns the protection key of the present leaf mapping at `vaddr`.
    ///
    /// Only x86 PTE implementations provide [`PkeyPTE`].
    pub fn pkey(&self, vaddr: M::VirtAddr) -> PagingResult<(Pkey, PageSize)>
    where
        PTE: PkeyPTE,
    {
        let (entry, size) = self.inner.get_entry(vaddr)?;
        if !entry.is_present() {
            return Err(PagingError::NotMapped);
        }
        Ok((entry.pkey(), size))
    }

    /// Changes the protection key of the present leaf mapping at `vaddr`.
    ///
    /// This retains the address and ordinary mapping flags, then records the
    /// leaf for the same TLB invalidation performed by permission changes.
    pub fn set_pkey(&mut self, vaddr: M::VirtAddr, pkey: Pkey) -> PagingResult<PageSize>
    where
        PTE: PkeyPTE,
    {
        let (entry, size) = self.inner.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PagingError::NotMapped);
        }
        entry.set_pkey(pkey);
        self.push(vaddr);
        Ok(size)
    }

    /// Demotes the huge leaf covering `vaddr` to 4 KiB leaves using the
    /// caller's preallocated page-table reservation. See
    /// [`PreparedPageTableFrames::try_new`] for lock-external preparation.
    pub fn demote_leaf_to_4k_prepared(
        &mut self,
        vaddr: M::VirtAddr,
        prepared: &mut PreparedPageTableFrames<H>,
    ) -> PagingResult<PageSize> {
        self.inner.demote_leaf_to_4k_prepared(vaddr, prepared)
    }

    /// Changes the protection key of every present leaf fully covered by the
    /// 4 KiB-aligned range.
    ///
    /// A range that would partially update a huge-page leaf is rejected. The
    /// caller must split that leaf first or update its complete extent.
    pub fn set_pkey_region(&mut self, vaddr: M::VirtAddr, size: usize, pkey: Pkey) -> PagingResult
    where
        PTE: PkeyPTE,
    {
        let mut current: usize = vaddr.into();
        if !PageSize::Size4K.is_aligned(current) || !PageSize::Size4K.is_aligned(size) {
            return Err(PagingError::NotAligned);
        }
        let end = current.checked_add(size).ok_or(PagingError::NotAligned)?;
        while current < end {
            let leaf_vaddr = current.into();
            let (_, page_size) = self.pkey(leaf_vaddr)?;
            if !page_size.is_aligned(current) || current + page_size as usize > end {
                return Err(PagingError::NotAligned);
            }
            self.set_pkey(leaf_vaddr, pkey)?;
            current += page_size as usize;
        }
        Ok(())
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
