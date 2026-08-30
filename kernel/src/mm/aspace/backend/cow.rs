use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    mem::MaybeUninit,
    ops::Range,
    slice,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::CachedFile;
use axfs_ng_vfs::Location;
use axhal::{
    mem::phys_to_virt,
    paging::{
        MappingFlags, PageSize, PageTable, PageTableCursor, PagingError, PrepareTableFramesError,
        PreparedMapError, PreparedPageTableFrames,
    },
};
use axsync::Mutex;
use kspin::SpinNoIrq;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, BackendRetirement, MappingStatus, PopulateOutcome, alloc_frame,
    dealloc_frame, page_table_flags, pages_in, preflight_sparse_leaves,
};
use crate::mm::swap::{self, SwapPte};

struct FrameRefCnt {
    references: u32,
    backing: Option<Arc<DemotedHugeBacking>>,
}

impl FrameRefCnt {
    // This function may lock FRAME_TABLE again, so the caller should drop the lock first.
    fn drop_frame(&mut self, paddr: PhysAddr, page_size: PageSize) {
        assert!(self.references > 0, "dropping unreferenced frame");
        self.references -= 1;
        if self.references == 0 {
            // Remove the frame from FRAME_TABLE before deallocating it to avoid a race:
            // if we dealloc the frame first, another thread could allocate the same
            // physical frame before we remove the table entry. This function assumes
            // the caller is not holding the FRAME_TABLE lock, so it is safe to lock
            // FRAME_TABLE here and perform the removal.
            FRAME_TABLE.lock().remove_frame(paddr, page_size);
            if let Some(backing) = self.backing.take() {
                backing.retire(page_size);
            } else {
                dealloc_frame(paddr, page_size);
            }
        }
    }
}

struct FrameTableRefCount {
    table: BTreeMap<(PhysAddr, usize), Arc<SpinNoIrq<FrameRefCnt>>>,
}

impl FrameTableRefCount {
    const INITIAL_CNT: u32 = 1;

    const fn new() -> Self {
        Self {
            table: BTreeMap::new(),
        }
    }

    fn get_frame_ref(
        &mut self,
        paddr: PhysAddr,
        page_size: PageSize,
    ) -> Option<Arc<SpinNoIrq<FrameRefCnt>>> {
        self.table.get(&(paddr, page_size as usize)).cloned()
    }

    fn get_or_init_frame(
        &mut self,
        paddr: PhysAddr,
        page_size: PageSize,
    ) -> Arc<SpinNoIrq<FrameRefCnt>> {
        self.table
            .entry((paddr, page_size as usize))
            .or_insert_with(|| {
                Arc::new(SpinNoIrq::new(FrameRefCnt {
                    references: Self::INITIAL_CNT,
                    backing: demoted_huge_backing(paddr),
                }))
            })
            .clone()
    }

    fn remove_frame(&mut self, paddr: PhysAddr, page_size: PageSize) {
        assert!(
            self.table.contains_key(&(paddr, page_size as usize)),
            "removing unreferenced frame"
        );
        self.table.remove(&(paddr, page_size as usize));
    }
}

static FRAME_TABLE: SpinNoIrq<FrameTableRefCount> = SpinNoIrq::new(FrameTableRefCount::new());

/// One original huge allocation which may simultaneously have unsplit huge
/// PTEs (fork siblings) and demoted P1 children.  Both domains retain the
/// same allocation; the final releaser returns it with its original size.
struct DemotedHugeBacking {
    base: PhysAddr,
    size: PageSize,
    huge_references: AtomicUsize,
    subpage_references: AtomicUsize,
    released: AtomicBool,
}

static DEMOTED_HUGE_BACKINGS: SpinNoIrq<BTreeMap<PhysAddr, Arc<DemotedHugeBacking>>> =
    SpinNoIrq::new(BTreeMap::new());

impl DemotedHugeBacking {
    fn contains(&self, paddr: PhysAddr) -> bool {
        paddr >= self.base && paddr.sub_addr(self.base) < self.size as usize
    }

    fn retire(&self, page_size: PageSize) {
        let references = match page_size {
            PageSize::Size4K => &self.subpage_references,
            PageSize::Size2M | PageSize::Size1G => &self.huge_references,
            _ => panic!("invalid demoted huge leaf size"),
        };
        assert!(
            references.fetch_sub(1, Ordering::AcqRel) > 0,
            "dropping unreferenced demoted huge leaf"
        );
        if self.huge_references.load(Ordering::Acquire) == 0
            && self.subpage_references.load(Ordering::Acquire) == 0
            && self
                .released
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            DEMOTED_HUGE_BACKINGS.lock().remove(&self.base);
            dealloc_frame(self.base, self.size);
        }
    }
}

fn demoted_huge_backing(paddr: PhysAddr) -> Option<Arc<DemotedHugeBacking>> {
    DEMOTED_HUGE_BACKINGS
        .lock()
        .range(..=paddr)
        .next_back()
        .and_then(|(_, backing)| backing.contains(paddr).then(|| backing.clone()))
}

/// Transfers one huge-PTE ownership unit into all of its P1 children.  If
/// fork created sibling huge mappings first, their aggregate reference count
/// remains attached to the original base frame and is retired independently.
pub(crate) fn register_demoted_huge_backing(base: PhysAddr, size: PageSize) -> AxResult {
    if !matches!(size, PageSize::Size2M | PageSize::Size1G) {
        return Err(AxError::InvalidInput);
    }
    let subpages = size as usize / PAGE_SIZE_4K;
    let existing = demoted_huge_backing(base);
    let was_already_demoted = existing.is_some();
    let backing = if let Some(backing) = existing {
        if backing.base != base || backing.size != size {
            return Err(AxError::BadState);
        }
        backing
    } else {
        let huge_references = FRAME_TABLE
            .lock()
            .get_frame_ref(base, size)
            .map_or(0, |reference| reference.lock().references.saturating_sub(1));
        let backing = Arc::try_new(DemotedHugeBacking {
            base,
            size,
            huge_references: AtomicUsize::new(huge_references as usize),
            subpage_references: AtomicUsize::new(0),
            released: AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory)?;
        DEMOTED_HUGE_BACKINGS.lock().insert(base, backing.clone());
        backing
    };

    // The converting PTE ceases to be a huge reference. Keep the remaining
    // fork siblings in FRAME_TABLE, but route their eventual final release to
    // the same backing state.
    if let Some(reference) = FRAME_TABLE.lock().get_frame_ref(base, size) {
        let mut reference = reference.lock();
        assert!(reference.references > 0, "invalid huge COW reference count");
        reference.references -= 1;
        reference.backing = Some(backing.clone());
        if reference.references == 0 {
            drop(reference);
            FRAME_TABLE.lock().remove_frame(base, size);
        }
    }
    backing
        .subpage_references
        .fetch_add(subpages, Ordering::AcqRel);
    if was_already_demoted {
        backing.retire(size);
    }
    Ok(())
}

/// Data-frame and page-table-frame ownership prepared before an anonymous
/// leaf publication enters the address-space critical section.
///
/// Publication borrows this value and transfers only the data frame plus any
/// table frames made reachable from the page table. Every unused resource
/// remains here so the caller can drop it after releasing the address-space
/// lock.
#[must_use = "prepared COW page ownership must be published or dropped outside the address-space \
              lock"]
pub(crate) struct PreparedCowPage {
    frame: PreparedCowFrame,
    tables: PreparedPageTableFrames,
}

/// One privately owned 2 MiB frame prepared for an anonymous COW collapse.
///
/// The frame is allocated and filled before the page-table transaction starts.
/// Until [`Self::commit_frame`] is called it remains owned by this guard, so a
/// failed PMD replacement cannot leak the new frame.  The source 4 KiB frames
/// are deliberately *not* touched here: their ownership is returned as a
/// [`CowUnmapRetirement`] only after a successful replacement and must remain
/// live through the TLB grace period.
#[must_use = "prepared huge-frame ownership must be committed or dropped"]
pub(crate) struct PreparedCowHugeFrame {
    frame: PreparedCowFrame,
}

/// Privately owned 4 KiB frames prepared to demote one materialized PMD.
#[must_use = "prepared demotion frames must be committed or dropped"]
pub(crate) struct PreparedCowDemotionFrames {
    frames: Vec<PhysAddr>,
}

impl PreparedCowDemotionFrames {
    pub(crate) fn copy_from_2m_frame(source: PhysAddr) -> AxResult<Self> {
        if !PageSize::Size2M.is_aligned(source.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        let mut prepared = Self { frames: Vec::new() };
        prepared
            .frames
            .try_reserve_exact(PageSize::Size2M as usize / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        for offset in (0..PageSize::Size2M as usize).step_by(PAGE_SIZE_4K) {
            let frame = alloc_frame(false, PageSize::Size4K)?;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(source).as_ptr().add(offset),
                    phys_to_virt(frame).as_mut_ptr(),
                    PAGE_SIZE_4K,
                );
            }
            prepared.frames.push(frame);
        }
        Ok(prepared)
    }

    pub(crate) fn frames(&self) -> &[PhysAddr] {
        &self.frames
    }

    pub(crate) fn commit_frames(&mut self) {
        self.frames.clear();
    }
}

impl Drop for PreparedCowDemotionFrames {
    fn drop(&mut self) {
        for frame in self.frames.drain(..) {
            dealloc_frame(frame, PageSize::Size4K);
        }
    }
}

impl PreparedCowHugeFrame {
    /// Allocates a PMD-sized frame and copies exactly its 512 source 4 KiB
    /// pages in virtual-address order.
    ///
    /// This is intentionally separate from page-table publication.  In
    /// particular it does not install a PTE, mutate a frame reference count,
    /// or make any source frame reclaimable.
    pub(crate) fn copy_from_4k_frames(sources: &[Option<PhysAddr>]) -> AxResult<Self> {
        validate_collapse_2m_source_frames(sources)?;
        let frame = alloc_frame(true, PageSize::Size2M)?;
        let mut prepared = Self {
            frame: PreparedCowFrame::Incomplete(frame),
        };
        for (index, source) in sources.iter().copied().enumerate() {
            let Some(source) = source else {
                continue;
            };
            let offset = index * PAGE_SIZE_4K;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(source).as_ptr(),
                    phys_to_virt(frame).as_mut_ptr().add(offset),
                    PAGE_SIZE_4K,
                );
            }
        }
        prepared.frame = PreparedCowFrame::Ready(frame);
        Ok(prepared)
    }

    pub(crate) fn frame(&self) -> AxResult<PhysAddr> {
        match self.frame {
            PreparedCowFrame::Ready(frame) => Ok(frame),
            PreparedCowFrame::Empty | PreparedCowFrame::Incomplete(_) => Err(AxError::BadState),
        }
    }

    /// Transfers the prepared frame to the successful page-table replacement.
    pub(crate) fn commit_frame(&mut self) {
        debug_assert!(matches!(self.frame, PreparedCowFrame::Ready(_)));
        self.frame = PreparedCowFrame::Empty;
    }
}

impl Drop for PreparedCowHugeFrame {
    fn drop(&mut self) {
        let frame = match self.frame {
            PreparedCowFrame::Empty => None,
            PreparedCowFrame::Incomplete(frame) | PreparedCowFrame::Ready(frame) => Some(frame),
        };
        self.frame = PreparedCowFrame::Empty;
        if let Some(frame) = frame {
            dealloc_frame(frame, PageSize::Size2M);
        }
    }
}

fn validate_collapse_2m_source_frames(sources: &[Option<PhysAddr>]) -> AxResult {
    const COLLAPSE_2M_PAGES: usize = PageSize::Size2M as usize / PAGE_SIZE_4K;
    if sources.len() != COLLAPSE_2M_PAGES
        || sources
            .iter()
            .any(|frame| frame.is_some_and(|frame| !PageSize::Size4K.is_aligned(frame.as_usize())))
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

enum PreparedCowFrame {
    Empty,
    /// Owned storage whose initializer failed and which must never become
    /// user-visible.
    Incomplete(PhysAddr),
    Ready(PhysAddr),
}

impl PreparedCowPage {
    pub(crate) fn try_new() -> AxResult<Self> {
        Ok(Self {
            frame: PreparedCowFrame::Empty,
            tables: PreparedPageTableFrames::try_new(0).map_err(Self::prepare_table_error)?,
        })
    }

    fn prepare_table_error(error: PrepareTableFramesError) -> AxError {
        match error {
            PrepareTableFramesError::NoMemory => AxError::NoMemory,
            PrepareTableFramesError::TooMany { .. } => AxError::BadState,
        }
    }

    /// Replenishes enough table ownership for any supported 4 KiB leaf path.
    ///
    /// Reusing the reservation means this allocates only frames consumed by
    /// earlier publications. The caller invokes it outside the address-space
    /// lock, after which `NeedMore` is an internal consistency failure.
    pub(crate) fn reserve_max_table_frames(&mut self) -> AxResult {
        self.tables
            .try_reserve_max()
            .map_err(Self::prepare_table_error)
    }

    /// Allocates one data frame whose entire contents are initialized to zero.
    pub(crate) fn prepare_zeroed(&mut self) -> AxResult {
        if !matches!(self.frame, PreparedCowFrame::Empty) {
            return Err(AxError::BadState);
        }
        let frame = alloc_frame(true, PageSize::Size4K)?;
        self.frame = PreparedCowFrame::Ready(frame);
        Ok(())
    }

    /// Allocates one uninitialized data frame and lets the caller initialize
    /// every byte.
    ///
    /// `fill` runs outside the address-space lock. A failed fill leaves an
    /// explicitly non-publishable frame owned by this value for lock-external
    /// reclamation.
    ///
    /// # Safety
    ///
    /// Returning `Ok(())` from `fill` must mean every byte in the supplied
    /// slice was initialized. The production caller uses the checked VM
    /// usercopy primitive, which has exactly that success contract.
    pub(crate) unsafe fn prepare_uninitialized(
        &mut self,
        fill: impl FnOnce(&mut [MaybeUninit<u8>]) -> AxResult,
    ) -> AxResult {
        if !matches!(self.frame, PreparedCowFrame::Empty) {
            return Err(AxError::BadState);
        }
        let frame = alloc_frame(false, PageSize::Size4K)?;
        self.frame = PreparedCowFrame::Incomplete(frame);
        let bytes = unsafe {
            slice::from_raw_parts_mut(
                phys_to_virt(frame).as_mut_ptr().cast::<MaybeUninit<u8>>(),
                PAGE_SIZE_4K,
            )
        };
        fill(bytes)?;
        self.frame = PreparedCowFrame::Ready(frame);
        Ok(())
    }

    fn frame(&self) -> AxResult<PhysAddr> {
        match self.frame {
            PreparedCowFrame::Ready(frame) => Ok(frame),
            PreparedCowFrame::Empty | PreparedCowFrame::Incomplete(_) => Err(AxError::BadState),
        }
    }

    fn commit_frame(&mut self) {
        debug_assert!(matches!(self.frame, PreparedCowFrame::Ready(_)));
        self.frame = PreparedCowFrame::Empty;
    }
}

impl Drop for PreparedCowPage {
    fn drop(&mut self) {
        let frame = match self.frame {
            PreparedCowFrame::Empty => None,
            PreparedCowFrame::Incomplete(frame) | PreparedCowFrame::Ready(frame) => Some(frame),
        };
        self.frame = PreparedCowFrame::Empty;
        if let Some(frame) = frame {
            dealloc_frame(frame, PageSize::Size4K);
        }
    }
}

/// Materialized COW leaves detached from a page table but not yet reclaimed.
pub(super) struct CowUnmapRetirement {
    leaves: Vec<(VirtAddr, PhysAddr, MappingFlags, PageSize)>,
}

impl Drop for CowUnmapRetirement {
    fn drop(&mut self) {
        for (_vaddr, frame, _flags, page_size) in self.leaves.drain(..) {
            let frame_ref = { FRAME_TABLE.lock().get_frame_ref(frame, page_size) };
            if let Some(frame_ref) = frame_ref {
                frame_ref.lock().drop_frame(frame, page_size);
            } else if let Some(backing) = demoted_huge_backing(frame) {
                backing.retire(page_size);
            } else {
                dealloc_frame(frame, page_size);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct CowClonePage {
    source_vaddr: VirtAddr,
    destination_vaddr: VirtAddr,
    paddr: PhysAddr,
    source_flags: MappingFlags,
    destination_flags: MappingFlags,
    page_size: PageSize,
    protect_source: bool,
    eager_copy: bool,
}

trait CowClonePageTableOps {
    fn protect_source(
        &mut self,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> Result<PageSize, PagingError>;

    fn map_destination(
        &mut self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
    ) -> Result<(), PagingError>;

    fn unmap_destination(
        &mut self,
        vaddr: VirtAddr,
    ) -> Result<(PhysAddr, MappingFlags, PageSize), PagingError>;

    fn copy_frame(&mut self, source: PhysAddr, page_size: PageSize) -> AxResult<PhysAddr>;

    fn reclaim_copied_frame(&mut self, frame: PhysAddr, page_size: PageSize);
}

fn copy_cow_frame(source: PhysAddr, page_size: PageSize) -> AxResult<PhysAddr> {
    let copied = alloc_frame(false, page_size)?;
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(source).as_ptr(),
            phys_to_virt(copied).as_mut_ptr(),
            page_size as usize,
        );
    }
    Ok(copied)
}

struct CursorCowCloneOps<'a, 'old, 'new> {
    old_pt: &'a mut PageTableCursor<'old>,
    new_pt: &'a mut PageTableCursor<'new>,
}

struct SingleCursorCowCloneOps<'a, 'pt> {
    pt: &'a mut PageTableCursor<'pt>,
}

impl CowClonePageTableOps for CursorCowCloneOps<'_, '_, '_> {
    fn protect_source(
        &mut self,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> Result<PageSize, PagingError> {
        self.old_pt.protect(vaddr, flags)
    }

    fn map_destination(
        &mut self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
    ) -> Result<(), PagingError> {
        self.new_pt.map(vaddr, paddr, page_size, flags)
    }

    fn unmap_destination(
        &mut self,
        vaddr: VirtAddr,
    ) -> Result<(PhysAddr, MappingFlags, PageSize), PagingError> {
        self.new_pt.unmap(vaddr)
    }

    fn copy_frame(&mut self, source: PhysAddr, page_size: PageSize) -> AxResult<PhysAddr> {
        copy_cow_frame(source, page_size)
    }

    fn reclaim_copied_frame(&mut self, frame: PhysAddr, page_size: PageSize) {
        dealloc_frame(frame, page_size);
    }
}

impl CowClonePageTableOps for SingleCursorCowCloneOps<'_, '_> {
    fn protect_source(
        &mut self,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> Result<PageSize, PagingError> {
        self.pt.protect(vaddr, flags)
    }

    fn map_destination(
        &mut self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
    ) -> Result<(), PagingError> {
        self.pt.map(vaddr, paddr, page_size, flags)
    }

    fn unmap_destination(
        &mut self,
        vaddr: VirtAddr,
    ) -> Result<(PhysAddr, MappingFlags, PageSize), PagingError> {
        self.pt.unmap(vaddr)
    }

    fn copy_frame(&mut self, source: PhysAddr, page_size: PageSize) -> AxResult<PhysAddr> {
        copy_cow_frame(source, page_size)
    }

    fn reclaim_copied_frame(&mut self, frame: PhysAddr, page_size: PageSize) {
        dealloc_frame(frame, page_size);
    }
}

struct CowCloneJournalEntry {
    source_vaddr: VirtAddr,
    destination_vaddr: VirtAddr,
    paddr: PhysAddr,
    source_flags: MappingFlags,
    page_size: PageSize,
    frame_ref: Option<Arc<SpinNoIrq<FrameRefCnt>>>,
    frame_retained: bool,
    eager_frame_owned: bool,
    source_protected: bool,
    destination_mapped: bool,
}

// The transaction owns only the extra destination PTE and frame reference.
// The source mapping remains caller-owned throughout fork and remap migration.
struct CowCloneTransaction<'a, Ops: CowClonePageTableOps> {
    ops: &'a mut Ops,
    journal: Vec<CowCloneJournalEntry>,
    committed: bool,
}

impl<'a, Ops: CowClonePageTableOps> CowCloneTransaction<'a, Ops> {
    fn try_new(ops: &'a mut Ops, page_count: usize) -> AxResult<Self> {
        let mut journal = Vec::new();
        journal
            .try_reserve(page_count)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            ops,
            journal,
            committed: false,
        })
    }

    fn share_page(
        &mut self,
        page: CowClonePage,
        frame_ref: Arc<SpinNoIrq<FrameRefCnt>>,
    ) -> AxResult {
        self.journal.push(CowCloneJournalEntry {
            source_vaddr: page.source_vaddr,
            destination_vaddr: page.destination_vaddr,
            paddr: page.paddr,
            source_flags: page.source_flags,
            page_size: page.page_size,
            frame_ref: Some(frame_ref),
            frame_retained: false,
            eager_frame_owned: false,
            source_protected: false,
            destination_mapped: false,
        });
        let entry = self.journal.last_mut().unwrap();

        {
            let mut frame = entry
                .frame_ref
                .as_ref()
                .expect("shared COW page lost its frame reference")
                .lock();
            assert!(frame.references > 0, "referencing unreferenced frame");
            let Some(next_refcnt) = frame.references.checked_add(1) else {
                warn!("frame reference count overflow");
                return Err(AxError::BadAddress);
            };
            frame.references = next_refcnt;
            entry.frame_retained = true;
        }

        if page.protect_source {
            let protected_size = self
                .ops
                .protect_source(page.source_vaddr, page.destination_flags)?;
            entry.source_protected = true;
            if protected_size != page.page_size {
                return Err(AxError::BadAddress);
            }
        }

        self.ops.map_destination(
            page.destination_vaddr,
            page.paddr,
            page.page_size,
            page.destination_flags,
        )?;
        entry.destination_mapped = true;
        Ok(())
    }

    fn copy_page(&mut self, page: CowClonePage) -> AxResult {
        let copied = self.ops.copy_frame(page.paddr, page.page_size)?;
        self.journal.push(CowCloneJournalEntry {
            source_vaddr: page.source_vaddr,
            destination_vaddr: page.destination_vaddr,
            paddr: copied,
            source_flags: page.source_flags,
            page_size: page.page_size,
            frame_ref: None,
            frame_retained: false,
            eager_frame_owned: true,
            source_protected: false,
            destination_mapped: false,
        });
        let entry = self.journal.last_mut().unwrap();
        self.ops.map_destination(
            page.destination_vaddr,
            copied,
            page.page_size,
            page.destination_flags,
        )?;
        entry.destination_mapped = true;
        Ok(())
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl<Ops: CowClonePageTableOps> Drop for CowCloneTransaction<'_, Ops> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        // A rollback mismatch means PTE ownership and frame accounting have
        // diverged. Continuing after that point could free a still-mapped frame.
        for entry in self.journal.iter().rev() {
            if entry.destination_mapped {
                let (paddr, _flags, page_size) = self
                    .ops
                    .unmap_destination(entry.destination_vaddr)
                    .unwrap_or_else(|err| {
                        panic!(
                            "failed to roll back COW destination mapping at {:?}: {:?}",
                            entry.destination_vaddr, err
                        )
                    });
                assert_eq!(paddr, entry.paddr, "COW rollback unmapped another frame");
                assert_eq!(
                    page_size, entry.page_size,
                    "COW rollback changed the destination leaf size"
                );
            }

            if entry.frame_retained {
                let mut frame = entry
                    .frame_ref
                    .as_ref()
                    .expect("retained COW page lost its frame reference")
                    .lock();
                assert!(frame.references > 1, "COW rollback lost the source frame reference");
                frame.drop_frame(entry.paddr, entry.page_size);
            }

            if entry.eager_frame_owned {
                self.ops.reclaim_copied_frame(entry.paddr, entry.page_size);
            }

            if entry.source_protected {
                let page_size = self
                    .ops
                    .protect_source(entry.source_vaddr, entry.source_flags)
                    .unwrap_or_else(|err| {
                        panic!(
                            "failed to restore COW source mapping at {:?}: {:?}",
                            entry.source_vaddr, err
                        )
                    });
                assert_eq!(
                    page_size, entry.page_size,
                    "COW rollback changed the source leaf size"
                );
            }
        }
    }
}

fn clone_pages_transactionally<Ops, Pages, FrameRef>(
    pages: Pages,
    _expected_page_size: PageSize,
    ops: &mut Ops,
    mut frame_ref: FrameRef,
) -> AxResult
where
    Ops: CowClonePageTableOps,
    Pages: ExactSizeIterator<Item = CowClonePage>,
    FrameRef: FnMut(PhysAddr, PageSize) -> Arc<SpinNoIrq<FrameRefCnt>>,
{
    let mut transaction = CowCloneTransaction::try_new(ops, pages.len())?;
    for page in pages {
        if page.eager_copy {
            transaction.copy_page(page)?;
        } else {
            transaction.share_page(page, frame_ref(page.paddr, page.page_size))?;
        }
    }
    transaction.commit();
    Ok(())
}

fn advance_file_start(file_start: u64, backing_advance: usize) -> AxResult<u64> {
    let backing_advance = u64::try_from(backing_advance).map_err(|_| AxError::InvalidInput)?;
    file_start
        .checked_add(backing_advance)
        .ok_or(AxError::InvalidInput)
}

/// Copy-on-write mapping backend.
///
/// This corresponds to the `MAP_PRIVATE` flag.
#[derive(Clone)]
pub struct CowBackend {
    start: VirtAddr,
    size: PageSize,
    file: Option<(CachedFile, u64, Option<u64>, bool)>,
    map_id: Arc<()>,
    materialized: Arc<AtomicBool>,
    status: MappingStatus,
}

impl CowBackend {
    const ANON_FAULT_AROUND_PAGES: usize = 4;

    pub(super) const fn mapping_status(&self) -> &MappingStatus {
        &self.status
    }

    pub(super) fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        &mut self.status
    }

    fn alloc_new_frame(&self, zeroed: bool) -> AxResult<PhysAddr> {
        alloc_frame(zeroed, self.size)
    }

    fn is_materialized(&self) -> bool {
        self.materialized.load(Ordering::Relaxed)
    }

    fn mark_materialized(&self) {
        self.materialized.store(true, Ordering::Relaxed);
    }

    fn get_or_track_frame_ref(
        &self,
        paddr: PhysAddr,
        page_size: PageSize,
    ) -> Arc<SpinNoIrq<FrameRefCnt>> {
        FRAME_TABLE.lock().get_or_init_frame(paddr, page_size)
    }

    pub(super) fn is_4k_anonymous(&self) -> bool {
        self.size == PageSize::Size4K && self.file.is_none()
    }

    pub(crate) fn has_file_backing(&self) -> bool {
        self.file.is_some()
    }

    /// Prepares a replacement PMD frame for a private 4 KiB COW mapping.
    ///
    /// Materialized leaves are copied verbatim. For sparse MAP_PRIVATE file
    /// mappings, absent leaves are loaded directly into the unpublished huge
    /// frame, avoiding transient 4 KiB mappings and anonymous COW frames.
    pub(crate) fn prepare_collapse_2m_frame(
        &self,
        start: VirtAddr,
        sources: &[Option<PhysAddr>],
    ) -> AxResult<PreparedCowHugeFrame> {
        if self.size != PageSize::Size4K {
            return Err(AxError::InvalidInput);
        }
        let prepared = PreparedCowHugeFrame::copy_from_4k_frames(sources)?;
        let Some((file, file_start, file_end, sigbus_on_eof)) = &self.file else {
            return Ok(prepared);
        };
        let frame = prepared.frame()?;
        for (index, source) in sources.iter().enumerate() {
            if source.is_some() {
                continue;
            }
            let vaddr = start + index * PAGE_SIZE_4K;
            if self.faults_with_sigbus(vaddr) {
                return Err(AxError::BadAddress);
            }
            let page_file_start = file_start
                .checked_add(vaddr.as_usize().saturating_sub(self.start.as_usize()) as u64)
                .ok_or(AxError::InvalidInput)?;
            let current_end = if *sigbus_on_eof {
                Some(file.location().len()?)
            } else {
                *file_end
            };
            let max_read = current_end
                .map_or(u64::MAX, |end| end.saturating_sub(page_file_start))
                .min(PAGE_SIZE_4K as u64) as usize;
            let destination = unsafe {
                slice::from_raw_parts_mut(
                    phys_to_virt(frame).as_mut_ptr().add(index * PAGE_SIZE_4K),
                    max_read,
                )
            };
            let read = file.read_at_sync(&mut &mut *destination, page_file_start)?;
            if read > max_read {
                return Err(AxError::Io);
            }
            // The prepared frame is zeroed, so short reads keep the normal
            // fault path's zero-filled tail.
        }
        Ok(prepared)
    }

    /// Returns the otherwise-identical COW backend used after a successful
    /// 4 KiB-to-PMD replacement.  This must be installed only after the PTE
    /// transaction commits; changing the backend first would make rollback
    /// unable to interpret the old leaves.
    pub(crate) fn collapsed_2m_backend(&self) -> AxResult<Self> {
        if self.size != PageSize::Size4K {
            return Err(AxError::InvalidInput);
        }
        let mut collapsed = self.clone();
        collapsed.size = PageSize::Size2M;
        collapsed.mark_materialized();
        Ok(collapsed)
    }

    pub(crate) fn prepare_demote_2m_frames(
        &self,
        source: PhysAddr,
    ) -> AxResult<PreparedCowDemotionFrames> {
        if self.size != PageSize::Size2M {
            return Err(AxError::InvalidInput);
        }
        PreparedCowDemotionFrames::copy_from_2m_frame(source)
    }

    pub(crate) fn demoted_4k_backend(&self) -> AxResult<Self> {
        if self.size != PageSize::Size2M {
            return Err(AxError::InvalidInput);
        }
        let mut demoted = self.clone();
        demoted.size = PageSize::Size4K;
        demoted.mark_materialized();
        Ok(demoted)
    }

    pub(crate) fn retire_demoted_2m_source(
        &self,
        vaddr: VirtAddr,
        frame: PhysAddr,
        flags: MappingFlags,
    ) -> AxResult<BackendRetirement> {
        if self.size != PageSize::Size2M || !PageSize::Size2M.is_aligned(frame.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        Ok(BackendRetirement::cow(CowUnmapRetirement {
            leaves: alloc::vec![(vaddr, frame, flags, PageSize::Size2M)],
        }))
    }

    /// Takes ownership of the detached 4 KiB leaves after a successful
    /// collapse.  Dropping the returned retirement decrements existing COW
    /// reference counts (or frees unshared frames) only after its owner has
    /// waited for TLB grace.
    pub(crate) fn retire_collapsed_2m_sources(
        &self,
        start: VirtAddr,
        leaves: Vec<(VirtAddr, PhysAddr, MappingFlags, PageSize)>,
    ) -> AxResult<BackendRetirement> {
        if self.size != PageSize::Size4K {
            return Err(AxError::InvalidInput);
        }
        let end = start + PageSize::Size2M as usize;
        let mut previous = None;
        for (vaddr, paddr, _flags, page_size) in &leaves {
            if *vaddr < start
                || *vaddr >= end
                || previous.is_some_and(|previous| *vaddr <= previous)
                || *page_size != PageSize::Size4K
                || !PageSize::Size4K.is_aligned(paddr.as_usize())
            {
                return Err(AxError::InvalidInput);
            }
            previous = Some(*vaddr);
        }
        Ok(BackendRetirement::cow(CowUnmapRetirement { leaves }))
    }

    /// Whether a present anonymous leaf has a single MM owner and can be
    /// replaced by a swap entry without invalidating a fork sibling.
    pub(super) fn swap_reclaimable(&self, paddr: PhysAddr) -> bool {
        if !self.is_4k_anonymous() {
            return false;
        }
        FRAME_TABLE
            .lock()
            .get_frame_ref(paddr, PageSize::Size4K)
            .is_none_or(|frame| frame.lock().references == 1)
    }

    /// Installs a resident page from an already-owned software swap entry.
    /// The entry is deliberately released only after `map` publishes the PTE.
    pub(super) fn restore_swapped_page(
        &self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        entry: SwapPte,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        if !self.is_4k_anonymous() {
            return Err(AxError::InvalidInput);
        }
        let frame = self.alloc_new_frame(false)?;
        let page =
            unsafe { slice::from_raw_parts_mut(phys_to_virt(frame).as_mut_ptr(), PAGE_SIZE_4K) };
        if let Err(error) = swap::read(entry, page) {
            dealloc_frame(frame, self.size);
            return Err(error);
        }
        if let Err(error) = pt.map(vaddr, frame, self.size, page_table_flags(flags)) {
            dealloc_frame(frame, self.size);
            return Err(error.into());
        }
        swap::release(entry)?;
        self.mark_materialized();
        Ok(())
    }

    /// Drops the former resident ownership after a swap PTE has been
    /// published and the TLB grace period has elapsed.
    pub(super) fn release_swapped_frame(&self, paddr: PhysAddr) {
        if let Some(frame) = FRAME_TABLE.lock().get_frame_ref(paddr, self.size) {
            frame.lock().drop_frame(paddr, self.size);
        } else {
            dealloc_frame(paddr, self.size);
        }
    }

    /// Atomically publishes one fully initialized anonymous page.
    ///
    /// This path performs no allocation or deallocation. On every error all
    /// resource ownership remains in `prepared`; on success the data frame and
    /// only the consumed table frames become page-table/backend ownership.
    pub(super) fn publish_prepared_page(
        &self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        pt: &mut PageTable,
        prepared: &mut PreparedCowPage,
    ) -> AxResult {
        if !self.is_4k_anonymous() {
            return Err(AxError::InvalidInput);
        }
        let frame = prepared.frame()?;
        let committed = pt.cursor().map_prepared(
            vaddr,
            frame,
            PageSize::Size4K,
            page_table_flags(flags),
            &mut prepared.tables,
        );
        match committed {
            Ok(_) => {
                prepared.commit_frame();
                self.mark_materialized();
                Ok(())
            }
            Err(PreparedMapError::NeedMore { .. }) => Err(AxError::BadState),
            Err(PreparedMapError::Paging(PagingError::AlreadyMapped))
            | Err(PreparedMapError::Paging(PagingError::MappedToHugePage)) => {
                Err(AxError::AlreadyExists)
            }
            Err(PreparedMapError::Paging(PagingError::NoMemory)) => Err(AxError::NoMemory),
            Err(PreparedMapError::Paging(_)) => Err(AxError::BadAddress),
        }
    }

    fn alloc_new_at(
        &self,
        vaddr: VirtAddr,
        flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        // For file-backed faults the file read fills the page, so a full-page
        // pre-zero is wasted work (and the zero path is the dominant fault
        // cost). Only anonymous pages need the frame pre-zeroed; for file
        // pages we zero just the gap before and the tail after the file data.
        let file_window = if let Some((file, file_start, file_end, sigbus_on_eof)) = &self.file {
            let page_size = self.size as usize;
            // vaddr can be smaller than self.start (at most 1 page) due to
            // non-aligned mappings, we need to keep the gap clean.
            let start = self.start.as_usize().saturating_sub(vaddr.as_usize());
            if start >= page_size {
                return Err(AxError::InvalidInput);
            }
            let file_start = file_start
                .checked_add(vaddr.as_usize().saturating_sub(self.start.as_usize()) as u64)
                .ok_or(AxError::InvalidInput)?;
            let file_end = if *sigbus_on_eof {
                Some(file.location().len()?)
            } else {
                *file_end
            };
            let max_read = file_end
                .map_or(u64::MAX, |end| end.saturating_sub(file_start))
                .min((page_size - start) as u64) as usize;
            Some((file, file_start, start, max_read))
        } else {
            None
        };

        let frame = self.alloc_new_frame(file_window.is_none())?;

        if let Some((file, file_start, start, max_read)) = file_window {
            let buf = unsafe {
                slice::from_raw_parts_mut(phys_to_virt(frame).as_mut_ptr(), self.size as _)
            };

            if start > 0 {
                unsafe { core::ptr::write_bytes(buf.as_mut_ptr(), 0, start) };
            }
            let destination = &mut &mut buf[start..start + max_read];
            // Page-fault and fork population both run inside the owning
            // address-space transaction. Until MM can snapshot, drop the lock,
            // and range-revalidate, it must not suspend after an async submit.
            let read_result = file.read_at_sync(destination, file_start);
            let read = match read_result {
                Ok(read) if read <= max_read => read,
                Ok(_) => {
                    dealloc_frame(frame, self.size);
                    return Err(AxError::Io);
                }
                Err(err) => {
                    dealloc_frame(frame, self.size);
                    return Err(err);
                }
            };
            let tail_start = start + read;
            if tail_start < buf.len() {
                unsafe {
                    core::ptr::write_bytes(
                        buf.as_mut_ptr().add(tail_start),
                        0,
                        buf.len() - tail_start,
                    )
                };
            }
        }
        if let Err(err) = pt.map(vaddr, frame, self.size, page_table_flags(flags)) {
            dealloc_frame(frame, self.size);
            return Err(err.into());
        }
        self.mark_materialized();
        Ok(())
    }

    fn handle_cow_fault(
        &self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        page_size: PageSize,
        flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let frame = { FRAME_TABLE.lock().get_frame_ref(paddr, page_size) };
        let backing = (page_size == PageSize::Size4K)
            .then(|| demoted_huge_backing(paddr))
            .flatten();
        // An unsplit fork sibling is one logical owner of every P1 child.
        // It is not represented by a per-P1 FRAME_TABLE entry, so retain it
        // explicitly in the COW decision until that sibling is also demoted
        // or unmapped.
        let huge_sibling = backing.as_ref().is_some_and(|backing| {
            backing.huge_references.load(Ordering::Acquire) != 0
        });
        let references = frame.as_ref().map_or(1, |frame| frame.lock().references);
        assert!(references > 0, "invalid frame reference count");
        if !huge_sibling && references == 1 {
            pt.protect(vaddr, page_table_flags(flags))?;
            pt.flush();
            drop(crate::mm::synchronize_tlb());
            self.mark_materialized();
            return Ok(());
        }

        let new_frame = alloc_frame(false, page_size)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(paddr).as_ptr(),
                phys_to_virt(new_frame).as_mut_ptr(),
                page_size as _,
            );
        }
        if let Err(err) = pt.remap(vaddr, new_frame, page_table_flags(flags)) {
            dealloc_frame(new_frame, page_size);
            return Err(err.into());
        }
        pt.flush();
        drop(crate::mm::synchronize_tlb());
        if let Some(frame) = frame {
            frame.lock().drop_frame(paddr, page_size);
        } else if let Some(backing) = backing {
            backing.retire(PageSize::Size4K);
        }

        self.mark_materialized();
        Ok(())
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: Arc<()>,
    ) -> AxResult<Self> {
        let (start, backing_advance) =
            super::relocate_affine_origin(self.start, old_start, new_start)?;
        let file = self
            .file
            .as_ref()
            .map(
                |(file, file_start, file_end, sigbus_on_eof)| -> AxResult<_> {
                    Ok((
                        file.clone(),
                        advance_file_start(*file_start, backing_advance)?,
                        *file_end,
                        *sigbus_on_eof,
                    ))
                },
            )
            .transpose()?;
        Ok(Self {
            start,
            size: self.size,
            file,
            map_id,
            materialized: self.materialized.clone(),
            status: self.status.relocated(old_start, new_start)?,
        })
    }

    pub(crate) fn faults_with_sigbus(&self, vaddr: VirtAddr) -> bool {
        let Some((file, file_start, Some(file_end), true)) = &self.file else {
            return false;
        };
        let page_start = vaddr.align_down(self.size);
        // The fault handler may align non-page-aligned file mappings down to the
        // backing page size. `alloc_new_at` treats the bytes before `self.start`
        // as a zero-filled gap, so keep the SIGBUS check on the mapped file
        // offset rather than underflowing here.
        let page_delta = page_start.as_usize().saturating_sub(self.start.as_usize()) as u64;
        let page_file_start = file_start.saturating_add(page_delta);
        let current_end = file.location().len().unwrap_or(*file_end);
        page_file_start >= current_end
    }

    pub(crate) fn cached_page_resident(&self, vaddr: VirtAddr) -> bool {
        let Some((file, file_start, ..)) = &self.file else {
            return false;
        };
        let page_start = vaddr.align_down(self.size);
        if page_start < self.start {
            return false;
        }

        let file_offset = *file_start + page_start.sub_addr(self.start) as u64;
        let file_page = file_offset / PAGE_SIZE_4K as u64;
        if file_page > u32::MAX as u64 {
            return false;
        }

        let mut resident = false;
        file.with_page(file_page as u32, |page| {
            resident = page.is_some();
        });
        resident
    }

    /// Converts a virtual subrange of this private file mapping into 4 KiB
    /// page-cache indices.  Private COW leaves are not page-cache aliases,
    /// but their untouched file contents are still backed by this cache.
    fn cache_page_range(&self, range: VirtAddrRange) -> AxResult<Range<u32>> {
        let Some((_, file_start, ..)) = &self.file else {
            return Err(AxError::OperationNotSupported);
        };
        if range.is_empty() {
            return Ok(0..0);
        }
        let start = range
            .start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        if !start.is_multiple_of(PAGE_SIZE_4K) || !range.size().is_multiple_of(PAGE_SIZE_4K) {
            return Err(AxError::InvalidInput);
        }
        let first = file_start
            .checked_add(u64::try_from(start).map_err(|_| AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
        if !first.is_multiple_of(PAGE_SIZE_4K as u64) {
            return Err(AxError::InvalidInput);
        }
        let first =
            u32::try_from(first / PAGE_SIZE_4K as u64).map_err(|_| AxError::InvalidInput)?;
        let count =
            u32::try_from(range.size() / PAGE_SIZE_4K).map_err(|_| AxError::InvalidInput)?;
        let end = first.checked_add(count).ok_or(AxError::InvalidInput)?;
        Ok(first..end)
    }

    /// Brings the untouched file portion of a MAP_PRIVATE mapping into the
    /// inode cache without allocating an anonymous COW leaf or installing a
    /// PTE.  Once a private page is materialized it remains private and is
    /// intentionally not treated as a cache alias.
    pub(crate) fn prefetch_file_pages(&self, range: VirtAddrRange) -> AxResult<usize> {
        let Some((file, ..)) = &self.file else {
            return Ok(0);
        };
        let pages = self.cache_page_range(range)?;
        let mut prefetched = 0usize;
        file.with_direct_io_excluded(|| {
            for (vaddr, pn) in pages_in(range, PageSize::Size4K)?.zip(pages) {
                // MAP_PRIVATE mappings preserve the same SIGBUS-at-EOF
                // boundary as their fault path; there is no backing cache
                // page to prefetch beyond it.
                if self.faults_with_sigbus(vaddr) {
                    continue;
                }
                file.with_page_or_insert(pn, |_, evicted| {
                    drop(evicted);
                    Ok(())
                })?;
                prefetched = prefetched.checked_add(1).ok_or(AxError::InvalidInput)?;
            }
            Ok::<(), AxError>(())
        })?;
        Ok(prefetched)
    }

    /// Demotes resident source-file cache pages for an unmaterialized private
    /// mapping.  This cannot affect anonymous COW pages, which have no safe
    /// reclaim representation without swap.
    pub(crate) fn cold_file_pages(&self, range: VirtAddrRange) -> AxResult<usize> {
        let Some((file, ..)) = &self.file else {
            return Err(AxError::OperationNotSupported);
        };
        Ok(file.cold_pages(self.cache_page_range(range)?)?)
    }

    /// Writes back and evicts resident source-file cache pages.  Private COW
    /// leaves keep their data independently, so eviction is safe and does not
    /// discard a process-private modification.
    pub(crate) fn pageout_file_pages(&self, range: VirtAddrRange) -> AxResult<usize> {
        let Some((file, ..)) = &self.file else {
            return Err(AxError::OperationNotSupported);
        };
        Ok(file.pageout_pages(self.cache_page_range(range)?)?)
    }

    pub(crate) fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        self.clone_for_range_with_id(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Self> {
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        self.clone_for_range_with_id(old_start, new_start, map_id)
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        if !Arc::ptr_eq(&self.map_id, &other.map_id) {
            return false;
        }
        if self.size != other.size {
            return false;
        }
        if self.start != other.start {
            return false;
        }
        match (&self.file, &other.file) {
            (None, None) => true,
            (
                Some((lhs_backend, lhs_start, lhs_end, lhs_sigbus)),
                Some((rhs_backend, rhs_start, rhs_end, rhs_sigbus)),
            ) => {
                lhs_start == rhs_start
                    && lhs_end == rhs_end
                    && lhs_sigbus == rhs_sigbus
                    && lhs_backend.ptr_eq(rhs_backend)
            }
            _ => false,
        }
    }

    pub(crate) fn mergeable_with(&self, other: &Self) -> bool {
        if self.size != other.size {
            return false;
        }
        match (&self.file, &other.file) {
            (None, None) => {
                (!self.is_materialized() && !other.is_materialized())
                    || Arc::ptr_eq(&self.materialized, &other.materialized)
            }
            _ => self.compatible_with(other),
        }
    }

    pub(crate) fn fault_around_size(&self, access_flags: MappingFlags) -> usize {
        if self.file.is_none()
            && self.size == PageSize::Size4K
            && access_flags.contains(MappingFlags::WRITE)
        {
            self.size as usize * Self::ANON_FAULT_AROUND_PAGES
        } else {
            self.size as usize
        }
    }

    pub(crate) fn clone_materialized_pages(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        size: usize,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let old_range =
            VirtAddrRange::try_from_start_size(old_start, size).ok_or(AxError::InvalidInput)?;
        let new_range =
            VirtAddrRange::try_from_start_size(new_start, size).ok_or(AxError::InvalidInput)?;
        pages_in(old_range, self.size)?;
        pages_in(new_range, self.size)?;
        let materialized = pt.collect_present_leaves(old_start, size)?;
        if !materialized.is_empty() {
            self.mark_materialized();
        }
        let pages =
            materialized
                .into_iter()
                .map(
                    |(source_vaddr, paddr, source_flags, page_size)| CowClonePage {
                        source_vaddr,
                        destination_vaddr: new_start + source_vaddr.sub_addr(old_start),
                        paddr,
                        source_flags,
                        destination_flags: page_table_flags(source_flags),
                        page_size,
                        protect_source: false,
                        eager_copy: false,
                    },
                );
        let mut ops = SingleCursorCowCloneOps { pt };
        clone_pages_transactionally(pages, self.size, &mut ops, |paddr, page_size| {
            self.get_or_track_frame_ref(paddr, page_size)
        })
    }

    pub(crate) fn is_private_anonymous(&self) -> bool {
        self.file.is_none()
    }

    fn needs_eager_fork_copy(
        &self,
        paddr: PhysAddr,
        active_long_term_cow_frames: &[PhysAddr],
    ) -> bool {
        self.size == PageSize::Size4K && active_long_term_cow_frames.binary_search(&paddr).is_ok()
    }
}

impl BackendOps for CowBackend {
    fn page_size(&self) -> PageSize {
        self.size
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        debug!("Cow::map: {range:?} {flags:?}",);
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult<BackendRetirement> {
        debug!("Cow::unmap: {range:?}");
        // A resident huge COW leaf may have been demoted to P1 entries by
        // pkey_mprotect. `drain_present_leaves` validates that the requested
        // VMA range contains complete leaves before changing anything.
        if !self.is_materialized() {
            return Ok(BackendRetirement::empty());
        }
        let materialized = pt.drain_present_leaves(range.start, range.size())?;
        Ok(BackendRetirement::cow(CowUnmapRetirement {
            leaves: materialized,
        }))
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        preflight_sparse_leaves(range, pt)
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> PopulateOutcome {
        PopulateOutcome::immediate((|| {
            let mut pages = 0;
            let mut addr = range.start;
            while addr < range.end {
                match pt.query(addr) {
                    Ok((paddr, page_flags, page_size)) => {
                        let leaf_start = addr.align_down(page_size);
                        let leaf_end = leaf_start
                            .checked_add(page_size as usize)
                            .ok_or(AxError::BadAddress)?;
                        if leaf_start < range.start || leaf_end > range.end {
                            return Err(AxError::BadAddress);
                        }
                        if access_flags.contains(MappingFlags::WRITE)
                            && !page_flags.contains(MappingFlags::WRITE)
                        {
                            self.handle_cow_fault(addr, paddr, page_size, flags, pt)?;
                            pages += 1;
                        } else if page_flags.contains(access_flags) {
                            pages += 1;
                        }
                        addr = leaf_end;
                    }
                    // If the page is not mapped, try map it.
                    Err(PagingError::NotMapped) => {
                        self.alloc_new_at(addr, flags, pt)?;
                        pages += 1;
                        addr = addr
                            .checked_add(self.size as usize)
                            .ok_or(AxError::BadAddress)?;
                    }
                    Err(_) => return Err(AxError::BadAddress),
                }
            }
            Ok(pages)
        })())
    }

    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pt: &mut PageTableCursor,
        new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
        active_long_term_cow_frames: &[PhysAddr],
    ) -> AxResult<Backend> {
        let cow_flags = page_table_flags(flags) - MappingFlags::WRITE;
        let eager_copy_flags = page_table_flags(flags);
        pages_in(range, self.size)?;
        if self.file.is_some() && flags.contains(MappingFlags::WRITE) {
            // Fork must snapshot the parent's current private data image, not the
            // original ELF file contents. Populate writable file-backed pages in
            // the parent before sharing them read-only with the child.
            for vaddr in pages_in(range, self.size)? {
                if matches!(old_pt.query(vaddr), Err(PagingError::NotMapped)) {
                    self.alloc_new_at(vaddr, cow_flags, old_pt)?;
                }
            }
        }
        let materialized = old_pt.collect_present_leaves(range.start, range.size())?;
        if !materialized.is_empty() {
            self.mark_materialized();
        }
        let pages = materialized
            .into_iter()
            .map(|(vaddr, paddr, source_flags, page_size)| {
                let eager_copy = self.needs_eager_fork_copy(paddr, active_long_term_cow_frames);
                CowClonePage {
                    source_vaddr: vaddr,
                    destination_vaddr: vaddr,
                    paddr,
                    source_flags,
                    destination_flags: if eager_copy {
                        eager_copy_flags
                    } else {
                        cow_flags
                    },
                    page_size,
                    // A pin-aware copy gives the child an independent frame at
                    // the VMA's current permissions. The parent and its
                    // escaped I/O owner keep the original physical identity,
                    // even if mprotect has since reduced the parent PTE.
                    protect_source: !eager_copy && source_flags.contains(MappingFlags::WRITE),
                    eager_copy,
                }
            });
        let mut ops = CursorCowCloneOps { old_pt, new_pt };
        clone_pages_transactionally(pages, self.size, &mut ops, |paddr, page_size| {
            self.get_or_track_frame_ref(paddr, page_size)
        })?;

        Ok(Backend::Cow(self.clone()))
    }
}

impl Backend {
    pub fn new_cow(
        start: VirtAddr,
        size: PageSize,
        file: Location,
        file_start: u64,
        file_end: Option<u64>,
        sigbus_on_eof: bool,
    ) -> Self {
        Self::Cow(CowBackend {
            start,
            size,
            file: Some((
                CachedFile::get_or_create(file),
                file_start,
                file_end,
                sigbus_on_eof,
            )),
            map_id: Arc::new(()),
            materialized: Arc::new(AtomicBool::new(false)),
            status: MappingStatus::default(),
        })
    }

    pub fn new_alloc(start: VirtAddr, size: PageSize) -> Self {
        Self::Cow(CowBackend {
            start,
            size,
            file: None,
            map_id: Arc::new(()),
            materialized: Arc::new(AtomicBool::new(false)),
            status: MappingStatus::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    #[test]
    fn incomplete_prepared_frame_is_never_publishable() {
        let mut prepared = PreparedCowPage {
            frame: PreparedCowFrame::Incomplete(PhysAddr::from(0x4000)),
            tables: PreparedPageTableFrames::try_new(0).unwrap(),
        };
        assert_eq!(prepared.frame(), Err(AxError::BadState));
        // This test uses a synthetic physical address to exercise only the
        // ownership state machine; restore Empty so Drop does not reclaim it.
        prepared.frame = PreparedCowFrame::Empty;
    }

    #[test]
    fn collapse_huge_frame_requires_exactly_aligned_4k_sources() {
        let sources = [Some(PhysAddr::from(0x2000)); PageSize::Size2M as usize / PAGE_SIZE_4K];
        assert_eq!(validate_collapse_2m_source_frames(&sources), Ok(()));
        let mut sparse = sources;
        sparse[17] = None;
        assert_eq!(validate_collapse_2m_source_frames(&sparse), Ok(()));
        assert_eq!(
            validate_collapse_2m_source_frames(&sources[..sources.len() - 1]),
            Err(AxError::InvalidInput)
        );

        let mut unaligned = sources;
        unaligned[17] = Some(PhysAddr::from(0x2001));
        assert_eq!(
            validate_collapse_2m_source_frames(&unaligned),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn collapse_huge_frame_is_not_publishable_until_copy_completes() {
        let mut prepared = PreparedCowHugeFrame {
            frame: PreparedCowFrame::Incomplete(PhysAddr::from(0x20_0000)),
        };
        assert_eq!(prepared.frame(), Err(AxError::BadState));
        // Synthetic physical address: prevent Drop from reclaiming it.
        prepared.frame = PreparedCowFrame::Empty;
    }

    #[test]
    fn collapse_promotes_private_anonymous_and_file_4k_cow_backends() {
        let Backend::Cow(anonymous) =
            Backend::new_alloc(VirtAddr::from(0x20_0000), PageSize::Size4K)
        else {
            unreachable!()
        };
        let collapsed = anonymous.collapsed_2m_backend().unwrap();
        assert_eq!(collapsed.page_size(), PageSize::Size2M);
        assert!(collapsed.is_private_anonymous());
        assert!(collapsed.is_materialized());

        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "collapse-private-file-cow",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let Backend::Cow(file_private) = Backend::new_cow(
            VirtAddr::from(0x40_0000),
            PageSize::Size4K,
            location,
            0,
            None,
            false,
        ) else {
            unreachable!()
        };
        let collapsed_file = file_private.collapsed_2m_backend().unwrap();
        assert_eq!(collapsed_file.page_size(), PageSize::Size2M);
        assert!(collapsed_file.has_file_backing());

        let Backend::Cow(already_huge) =
            Backend::new_alloc(VirtAddr::from(0x20_0000), PageSize::Size2M)
        else {
            unreachable!()
        };
        assert!(matches!(
            already_huge.collapsed_2m_backend(),
            Err(AxError::InvalidInput)
        ));
    }

    #[test]
    fn materialized_growdown_must_clone_backend_identity_to_remerge() {
        let Backend::Cow(original) = Backend::new_alloc(VirtAddr::from(0x4000), PageSize::Size4K)
        else {
            unreachable!()
        };
        let cloned = original.clone();
        let Backend::Cow(fresh) = Backend::new_alloc(VirtAddr::from(0x3000), PageSize::Size4K)
        else {
            unreachable!()
        };

        original.mark_materialized();
        assert!(original.mergeable_with(&cloned));
        assert!(!original.mergeable_with(&fresh));
    }

    #[test]
    fn fork_eager_copy_survives_mprotect_down_for_anon_and_file_private_cow() {
        let pinned = PhysAddr::from(0x2200_0000);
        let other = PhysAddr::from(0x2200_1000);
        let Backend::Cow(anonymous) = Backend::new_alloc(VirtAddr::from(0x4000), PageSize::Size4K)
        else {
            unreachable!()
        };
        let Backend::Cow(huge) = Backend::new_alloc(VirtAddr::from(0x20_0000), PageSize::Size2M)
        else {
            unreachable!()
        };
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "pin-aware-private-file-cow",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let Backend::Cow(file_private) = Backend::new_cow(
            VirtAddr::from(0x8000),
            PageSize::Size4K,
            location,
            0,
            None,
            false,
        ) else {
            unreachable!()
        };
        // Membership in this owner-aware set was published only by an active
        // long-term WRITE pin. A later mprotect(PROT_READ/PROT_NONE) changes
        // current PTE permissions, not that historical COW/pin obligation.
        // CowBackend's predicate intentionally does not inspect `self.file`:
        // both anonymous pages and materialized MAP_PRIVATE file pages become
        // private COW frames before a FOLL_WRITE-equivalent pin is published.
        assert!(anonymous.needs_eager_fork_copy(pinned, &[pinned]));
        assert!(file_private.needs_eager_fork_copy(pinned, &[pinned]));
        assert!(!anonymous.needs_eager_fork_copy(other, &[pinned]));
        assert!(!huge.needs_eager_fork_copy(pinned, &[pinned]));
    }

    #[test]
    fn private_file_cow_madvise_uses_source_page_cache_without_cow_faulting() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "private-cow-madvise-cache",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let Backend::Cow(backend) = Backend::new_cow(
            VirtAddr::from(0x8000),
            PageSize::Size4K,
            location,
            0,
            None,
            false,
        ) else {
            unreachable!()
        };
        let address = VirtAddr::from(0x8000);
        let range = VirtAddrRange::new(address, address + PAGE_SIZE_4K);

        assert!(!backend.cached_page_resident(address));
        assert_eq!(backend.prefetch_file_pages(range), Ok(1));
        assert!(backend.cached_page_resident(address));
        assert_eq!(backend.cold_file_pages(range), Ok(1));
        // Memory files are not writeback-backed, so PAGEOUT correctly keeps
        // the resident cache page while still taking the real cache path.
        assert_eq!(backend.pageout_file_pages(range), Ok(0));
    }

    #[test]
    fn anonymous_cow_rejects_unimplementable_cold_and_pageout() {
        let Backend::Cow(backend) = Backend::new_alloc(VirtAddr::from(0x8000), PageSize::Size4K)
        else {
            unreachable!()
        };
        let range = VirtAddrRange::new(VirtAddr::from(0x8000), VirtAddr::from(0x9000));

        assert_eq!(
            backend.cold_file_pages(range),
            Err(AxError::OperationNotSupported)
        );
        assert_eq!(
            backend.pageout_file_pages(range),
            Err(AxError::OperationNotSupported)
        );
    }

    struct MockCowCloneOps {
        parents: BTreeMap<VirtAddr, (MappingFlags, PageSize)>,
        children: BTreeMap<VirtAddr, (PhysAddr, MappingFlags, PageSize)>,
        fail_map_call: usize,
        map_calls: usize,
        unmapped: Vec<(VirtAddr, PhysAddr, PageSize)>,
        copied: Vec<(PhysAddr, PhysAddr, PageSize)>,
        reclaimed: Vec<(PhysAddr, PageSize)>,
    }

    impl CowClonePageTableOps for MockCowCloneOps {
        fn protect_source(
            &mut self,
            vaddr: VirtAddr,
            flags: MappingFlags,
        ) -> Result<PageSize, PagingError> {
            let Some((parent_flags, page_size)) = self.parents.get_mut(&vaddr) else {
                return Err(PagingError::NotMapped);
            };
            *parent_flags = flags;
            Ok(*page_size)
        }

        fn map_destination(
            &mut self,
            vaddr: VirtAddr,
            paddr: PhysAddr,
            page_size: PageSize,
            flags: MappingFlags,
        ) -> Result<(), PagingError> {
            self.map_calls += 1;
            if self.map_calls == self.fail_map_call {
                return Err(PagingError::NoMemory);
            }
            if self
                .children
                .insert(vaddr, (paddr, flags, page_size))
                .is_some()
            {
                return Err(PagingError::AlreadyMapped);
            }
            Ok(())
        }

        fn unmap_destination(
            &mut self,
            vaddr: VirtAddr,
        ) -> Result<(PhysAddr, MappingFlags, PageSize), PagingError> {
            let Some((paddr, flags, page_size)) = self.children.remove(&vaddr) else {
                return Err(PagingError::NotMapped);
            };
            self.unmapped.push((vaddr, paddr, page_size));
            Ok((paddr, flags, page_size))
        }

        fn copy_frame(&mut self, source: PhysAddr, page_size: PageSize) -> AxResult<PhysAddr> {
            let copied = PhysAddr::from(source.as_usize() + 0x1000_0000);
            self.copied.push((source, copied, page_size));
            Ok(copied)
        }

        fn reclaim_copied_frame(&mut self, frame: PhysAddr, page_size: PageSize) {
            self.reclaimed.push((frame, page_size));
        }
    }

    fn tracked_frames(pages: &[CowClonePage]) -> BTreeMap<PhysAddr, Arc<SpinNoIrq<FrameRefCnt>>> {
        pages
            .iter()
            .map(|page| {
                (
                    page.paddr,
                    Arc::new(SpinNoIrq::new(FrameRefCnt {
                        references: FrameTableRefCount::INITIAL_CNT,
                        backing: None,
                    })),
                )
            })
            .collect()
    }

    fn mock_clone_ops(pages: &[CowClonePage], fail_map_call: usize) -> MockCowCloneOps {
        MockCowCloneOps {
            parents: pages
                .iter()
                .map(|page| (page.source_vaddr, (page.source_flags, page.page_size)))
                .collect(),
            children: BTreeMap::new(),
            fail_map_call,
            map_calls: 0,
            unmapped: Vec::new(),
            copied: Vec::new(),
            reclaimed: Vec::new(),
        }
    }

    fn migrate_test_pages() -> [CowClonePage; 3] {
        let page_size = PageSize::Size4K;
        [
            CowClonePage {
                source_vaddr: VirtAddr::from(0x4000),
                destination_vaddr: VirtAddr::from(0x14_000),
                paddr: PhysAddr::from(0x2000_0000),
                source_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                destination_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                page_size,
                protect_source: false,
                eager_copy: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x5000),
                destination_vaddr: VirtAddr::from(0x15_000),
                paddr: PhysAddr::from(0x2000_1000),
                source_flags: MappingFlags::USER | MappingFlags::READ,
                destination_flags: MappingFlags::USER | MappingFlags::READ,
                page_size,
                protect_source: false,
                eager_copy: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x6000),
                destination_vaddr: VirtAddr::from(0x16_000),
                paddr: PhysAddr::from(0x2000_2000),
                source_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE,
                destination_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE,
                page_size,
                protect_source: false,
                eager_copy: false,
            },
        ]
    }

    #[test]
    fn low_address_cow_suffix_advances_the_file_cursor() {
        let (start, backing_advance) = super::super::relocate_affine_origin(
            VirtAddr::from(0x4000),
            VirtAddr::from(0x8000),
            VirtAddr::from(0x1000),
        )
        .unwrap();

        assert_eq!(start, VirtAddr::from(0x1000));
        assert_eq!(backing_advance, 0x4000);
        assert_eq!(advance_file_start(0x20_000, backing_advance), Ok(0x24_000));
        assert_eq!(
            advance_file_start(u64::MAX - 0x1000, backing_advance),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn late_child_map_failure_rolls_back_huge_cow_clone_transaction() {
        let page_size = PageSize::Size2M;
        let original_flags = [
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            MappingFlags::READ | MappingFlags::WRITE,
        ];
        let pages = [
            CowClonePage {
                source_vaddr: VirtAddr::from(0x20_0000),
                destination_vaddr: VirtAddr::from(0x20_0000),
                paddr: PhysAddr::from(0x1000_0000),
                source_flags: original_flags[0],
                destination_flags: MappingFlags::USER | MappingFlags::READ,
                page_size,
                protect_source: true,
                eager_copy: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x40_0000),
                destination_vaddr: VirtAddr::from(0x40_0000),
                paddr: PhysAddr::from(0x1020_0000),
                source_flags: original_flags[1],
                destination_flags: MappingFlags::USER | MappingFlags::READ,
                page_size,
                protect_source: true,
                eager_copy: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x60_0000),
                destination_vaddr: VirtAddr::from(0x60_0000),
                paddr: PhysAddr::from(0x1040_0000),
                source_flags: original_flags[2],
                destination_flags: MappingFlags::READ,
                page_size,
                protect_source: true,
                eager_copy: false,
            },
        ];
        let frame_refs = tracked_frames(&pages);
        let mut ops = mock_clone_ops(&pages, 3);
        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::NoMemory)
        );

        assert!(ops.children.is_empty());
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(
                ops.parents[&page.source_vaddr],
                (original_flags[index], page_size)
            );
            assert_eq!(
                frame_refs[&page.paddr].lock().references,
                FrameTableRefCount::INITIAL_CNT
            );
        }
        assert_eq!(
            ops.unmapped,
            vec![
                (pages[1].destination_vaddr, pages[1].paddr, page_size),
                (pages[0].destination_vaddr, pages[0].paddr, page_size),
            ]
        );
    }

    #[test]
    fn eager_child_copy_failure_reclaims_copy_and_restores_prior_lazy_cow() {
        let page_size = PageSize::Size4K;
        let writable = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let readonly = MappingFlags::USER | MappingFlags::READ;
        let pages = [
            CowClonePage {
                source_vaddr: VirtAddr::from(0x4000),
                destination_vaddr: VirtAddr::from(0x4000),
                paddr: PhysAddr::from(0x2100_0000),
                source_flags: writable,
                destination_flags: readonly,
                page_size,
                protect_source: true,
                eager_copy: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x5000),
                destination_vaddr: VirtAddr::from(0x5000),
                paddr: PhysAddr::from(0x2100_1000),
                source_flags: writable,
                destination_flags: writable,
                page_size,
                protect_source: false,
                eager_copy: true,
            },
        ];
        let frame_refs = tracked_frames(&pages);
        let copied = PhysAddr::from(pages[1].paddr.as_usize() + 0x1000_0000);
        let mut ops = mock_clone_ops(&pages, 2);

        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::NoMemory)
        );
        assert!(ops.children.is_empty());
        assert_eq!(ops.parents[&pages[0].source_vaddr], (writable, page_size));
        assert_eq!(
            frame_refs[&pages[0].paddr].lock().references,
            FrameTableRefCount::INITIAL_CNT
        );
        assert_eq!(ops.copied, vec![(pages[1].paddr, copied, page_size)]);
        assert_eq!(ops.reclaimed, vec![(copied, page_size)]);
    }

    #[test]
    fn ordinary_unpinned_clone_remains_lazy_cow() {
        let page_size = PageSize::Size4K;
        let writable = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let readonly = MappingFlags::USER | MappingFlags::READ;
        let pages = [CowClonePage {
            source_vaddr: VirtAddr::from(0x4000),
            destination_vaddr: VirtAddr::from(0x4000),
            paddr: PhysAddr::from(0x2300_0000),
            source_flags: writable,
            destination_flags: readonly,
            page_size,
            protect_source: true,
            eager_copy: false,
        }];
        let frame_refs = tracked_frames(&pages);
        let mut ops = mock_clone_ops(&pages, usize::MAX);

        clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
            frame_refs.get(&paddr).unwrap().clone()
        })
        .unwrap();

        assert_eq!(ops.parents[&pages[0].source_vaddr], (readonly, page_size));
        assert_eq!(
            ops.children[&pages[0].destination_vaddr],
            (pages[0].paddr, readonly, page_size)
        );
        assert_eq!(frame_refs[&pages[0].paddr].lock().references, 2);
        assert!(ops.copied.is_empty());
    }

    #[test]
    fn mprotect_down_keeps_child_copy_at_current_readonly_permissions() {
        let page_size = PageSize::Size4K;
        let readonly = MappingFlags::USER | MappingFlags::READ;
        let pages = [CowClonePage {
            source_vaddr: VirtAddr::from(0x7000),
            destination_vaddr: VirtAddr::from(0x7000),
            paddr: PhysAddr::from(0x2400_0000),
            source_flags: readonly,
            destination_flags: readonly,
            page_size,
            protect_source: false,
            eager_copy: true,
        }];
        let frame_refs = tracked_frames(&pages);
        let copied = PhysAddr::from(pages[0].paddr.as_usize() + 0x1000_0000);
        let mut ops = mock_clone_ops(&pages, usize::MAX);

        clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
            frame_refs.get(&paddr).unwrap().clone()
        })
        .unwrap();

        assert_eq!(ops.parents[&pages[0].source_vaddr], (readonly, page_size));
        assert_eq!(
            ops.children[&pages[0].destination_vaddr],
            (copied, readonly, page_size)
        );
        assert_eq!(ops.copied, vec![(pages[0].paddr, copied, page_size)]);
        assert!(ops.reclaimed.is_empty());
        assert_eq!(
            frame_refs[&pages[0].paddr].lock().references,
            FrameTableRefCount::INITIAL_CNT
        );
    }

    #[test]
    fn late_migrate_map_failure_unmaps_destinations_without_touching_sources() {
        let page_size = PageSize::Size4K;
        let pages = migrate_test_pages();
        let frame_refs = tracked_frames(&pages);
        let source_snapshot: BTreeMap<_, _> = pages
            .iter()
            .map(|page| (page.source_vaddr, (page.source_flags, page.page_size)))
            .collect();
        let mut ops = mock_clone_ops(&pages, 3);

        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::NoMemory)
        );

        assert_eq!(ops.parents, source_snapshot);
        assert!(ops.children.is_empty());
        for page in pages {
            assert_eq!(
                frame_refs[&page.paddr].lock().references,
                FrameTableRefCount::INITIAL_CNT
            );
        }
        assert_eq!(
            ops.unmapped,
            vec![
                (pages[1].destination_vaddr, pages[1].paddr, page_size),
                (pages[0].destination_vaddr, pages[0].paddr, page_size),
            ]
        );
    }

    #[test]
    fn late_migrate_refcount_overflow_rolls_back_published_destinations() {
        let page_size = PageSize::Size4K;
        let pages = migrate_test_pages();
        let frame_refs = tracked_frames(&pages);
        frame_refs[&pages[2].paddr].lock().references = u32::MAX;
        let mut ops = mock_clone_ops(&pages, usize::MAX);
        let source_snapshot = ops.parents.clone();

        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr, _| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::BadAddress)
        );

        assert_eq!(ops.map_calls, 2);
        assert_eq!(ops.parents, source_snapshot);
        assert!(ops.children.is_empty());
        assert_eq!(
            frame_refs[&pages[0].paddr].lock().references,
            FrameTableRefCount::INITIAL_CNT
        );
        assert_eq!(
            frame_refs[&pages[1].paddr].lock().references,
            FrameTableRefCount::INITIAL_CNT
        );
        assert_eq!(frame_refs[&pages[2].paddr].lock().references, u32::MAX);
        assert_eq!(
            ops.unmapped,
            vec![
                (pages[1].destination_vaddr, pages[1].paddr, page_size),
                (pages[0].destination_vaddr, pages[0].paddr, page_size),
            ]
        );
    }
}
