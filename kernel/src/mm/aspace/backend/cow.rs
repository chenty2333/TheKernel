use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    mem::MaybeUninit,
    slice,
    sync::atomic::{AtomicBool, Ordering},
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
    dealloc_frame, page_table_flags, pages_in, preflight_sparse_unmap,
};

struct FrameRefCnt(u32);

impl FrameRefCnt {
    // This function may lock FRAME_TABLE again, so the caller should drop the lock first.
    fn drop_frame(&mut self, paddr: PhysAddr, page_size: PageSize) {
        assert!(self.0 > 0, "dropping unreferenced frame");
        self.0 -= 1;
        if self.0 == 0 {
            // Remove the frame from FRAME_TABLE before deallocating it to avoid a race:
            // if we dealloc the frame first, another thread could allocate the same
            // physical frame before we remove the table entry. This function assumes
            // the caller is not holding the FRAME_TABLE lock, so it is safe to lock
            // FRAME_TABLE here and perform the removal.
            FRAME_TABLE.lock().remove_frame(paddr);
            dealloc_frame(paddr, page_size);
        }
    }
}

struct FrameTableRefCount {
    table: BTreeMap<PhysAddr, Arc<SpinNoIrq<FrameRefCnt>>>,
}

impl FrameTableRefCount {
    const INITIAL_CNT: u32 = 1;

    const fn new() -> Self {
        Self {
            table: BTreeMap::new(),
        }
    }

    fn get_frame_ref(&mut self, paddr: PhysAddr) -> Option<Arc<SpinNoIrq<FrameRefCnt>>> {
        self.table.get(&paddr).cloned()
    }

    fn get_or_init_frame(&mut self, paddr: PhysAddr) -> Arc<SpinNoIrq<FrameRefCnt>> {
        self.table
            .entry(paddr)
            .or_insert_with(|| Arc::new(SpinNoIrq::new(FrameRefCnt(Self::INITIAL_CNT))))
            .clone()
    }

    fn remove_frame(&mut self, paddr: PhysAddr) {
        assert!(
            self.table.contains_key(&paddr),
            "removing unreferenced frame"
        );
        self.table.remove(&paddr);
    }
}

static FRAME_TABLE: SpinNoIrq<FrameTableRefCount> = SpinNoIrq::new(FrameTableRefCount::new());

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
            let frame_ref = { FRAME_TABLE.lock().get_frame_ref(frame) };
            if let Some(frame_ref) = frame_ref {
                frame_ref.lock().drop_frame(frame, page_size);
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
}

struct CowCloneJournalEntry {
    source_vaddr: VirtAddr,
    destination_vaddr: VirtAddr,
    paddr: PhysAddr,
    source_flags: MappingFlags,
    page_size: PageSize,
    frame_ref: Arc<SpinNoIrq<FrameRefCnt>>,
    frame_retained: bool,
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
            frame_ref,
            frame_retained: false,
            source_protected: false,
            destination_mapped: false,
        });
        let entry = self.journal.last_mut().unwrap();

        {
            let mut frame = entry.frame_ref.lock();
            assert!(frame.0 > 0, "referencing unreferenced frame");
            let Some(next_refcnt) = frame.0.checked_add(1) else {
                warn!("frame reference count overflow");
                return Err(AxError::BadAddress);
            };
            frame.0 = next_refcnt;
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
                let mut frame = entry.frame_ref.lock();
                assert!(frame.0 > 1, "COW rollback lost the source frame reference");
                frame.drop_frame(entry.paddr, entry.page_size);
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
    expected_page_size: PageSize,
    ops: &mut Ops,
    mut frame_ref: FrameRef,
) -> AxResult
where
    Ops: CowClonePageTableOps,
    Pages: ExactSizeIterator<Item = CowClonePage>,
    FrameRef: FnMut(PhysAddr) -> Arc<SpinNoIrq<FrameRefCnt>>,
{
    let mut transaction = CowCloneTransaction::try_new(ops, pages.len())?;
    for page in pages {
        if page.page_size != expected_page_size {
            return Err(AxError::BadAddress);
        }
        transaction.share_page(page, frame_ref(page.paddr))?;
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

    fn get_or_track_frame_ref(&self, paddr: PhysAddr) -> Arc<SpinNoIrq<FrameRefCnt>> {
        FRAME_TABLE.lock().get_or_init_frame(paddr)
    }

    pub(super) fn is_4k_anonymous(&self) -> bool {
        self.size == PageSize::Size4K && self.file.is_none()
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
                    return Err(err.into());
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
        flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult {
        let frame = { FRAME_TABLE.lock().get_frame_ref(paddr) };
        let Some(frame_ref) = frame else {
            pt.protect(vaddr, page_table_flags(flags))?;
            pt.flush();
            drop(crate::mm::synchronize_tlb());
            self.mark_materialized();
            return Ok(());
        };
        let references = frame_ref.lock().0;
        assert!(references > 0, "invalid frame reference count");
        match references {
            1 => {
                // Only one reference, just upgrade the permissions.
                pt.protect(vaddr, page_table_flags(flags))?;
                pt.flush();
                drop(crate::mm::synchronize_tlb());
                self.mark_materialized();
                return Ok(());
            }
            _ => {
                // Multiple references, need to copy the frame.
                let new_frame = self.alloc_new_frame(false)?;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        phys_to_virt(paddr).as_ptr(),
                        phys_to_virt(new_frame).as_mut_ptr(),
                        self.size as _,
                    );
                }
                if let Err(err) = pt.remap(vaddr, new_frame, page_table_flags(flags)) {
                    dealloc_frame(new_frame, self.size);
                    return Err(err.into());
                }
                pt.flush();
                drop(crate::mm::synchronize_tlb());
                frame_ref.lock().drop_frame(paddr, self.size);
            }
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
                    },
                );
        let mut ops = SingleCursorCowCloneOps { pt };
        clone_pages_transactionally(pages, self.size, &mut ops, |paddr| {
            self.get_or_track_frame_ref(paddr)
        })
    }

    pub(crate) fn is_private_anonymous(&self) -> bool {
        self.file.is_none()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

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

    struct MockCowCloneOps {
        parents: BTreeMap<VirtAddr, (MappingFlags, PageSize)>,
        children: BTreeMap<VirtAddr, (PhysAddr, MappingFlags, PageSize)>,
        fail_map_call: usize,
        map_calls: usize,
        unmapped: Vec<(VirtAddr, PhysAddr, PageSize)>,
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
    }

    fn tracked_frames(pages: &[CowClonePage]) -> BTreeMap<PhysAddr, Arc<SpinNoIrq<FrameRefCnt>>> {
        pages
            .iter()
            .map(|page| {
                (
                    page.paddr,
                    Arc::new(SpinNoIrq::new(FrameRefCnt(FrameTableRefCount::INITIAL_CNT))),
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
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x5000),
                destination_vaddr: VirtAddr::from(0x15_000),
                paddr: PhysAddr::from(0x2000_1000),
                source_flags: MappingFlags::USER | MappingFlags::READ,
                destination_flags: MappingFlags::USER | MappingFlags::READ,
                page_size,
                protect_source: false,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x6000),
                destination_vaddr: VirtAddr::from(0x16_000),
                paddr: PhysAddr::from(0x2000_2000),
                source_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE,
                destination_flags: MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE,
                page_size,
                protect_source: false,
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
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x40_0000),
                destination_vaddr: VirtAddr::from(0x40_0000),
                paddr: PhysAddr::from(0x1020_0000),
                source_flags: original_flags[1],
                destination_flags: MappingFlags::USER | MappingFlags::READ,
                page_size,
                protect_source: true,
            },
            CowClonePage {
                source_vaddr: VirtAddr::from(0x60_0000),
                destination_vaddr: VirtAddr::from(0x60_0000),
                paddr: PhysAddr::from(0x1040_0000),
                source_flags: original_flags[2],
                destination_flags: MappingFlags::READ,
                page_size,
                protect_source: true,
            },
        ];
        let frame_refs = tracked_frames(&pages);
        let mut ops = mock_clone_ops(&pages, 3);
        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr| {
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
                frame_refs[&page.paddr].lock().0,
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
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::NoMemory)
        );

        assert_eq!(ops.parents, source_snapshot);
        assert!(ops.children.is_empty());
        for page in pages {
            assert_eq!(
                frame_refs[&page.paddr].lock().0,
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
        frame_refs[&pages[2].paddr].lock().0 = u32::MAX;
        let mut ops = mock_clone_ops(&pages, usize::MAX);
        let source_snapshot = ops.parents.clone();

        assert_eq!(
            clone_pages_transactionally(pages.into_iter(), page_size, &mut ops, |paddr| {
                frame_refs.get(&paddr).unwrap().clone()
            }),
            Err(AxError::BadAddress)
        );

        assert_eq!(ops.map_calls, 2);
        assert_eq!(ops.parents, source_snapshot);
        assert!(ops.children.is_empty());
        assert_eq!(
            frame_refs[&pages[0].paddr].lock().0,
            FrameTableRefCount::INITIAL_CNT
        );
        assert_eq!(
            frame_refs[&pages[1].paddr].lock().0,
            FrameTableRefCount::INITIAL_CNT
        );
        assert_eq!(frame_refs[&pages[2].paddr].lock().0, u32::MAX);
        assert_eq!(
            ops.unmapped,
            vec![
                (pages[1].destination_vaddr, pages[1].paddr, page_size),
                (pages[0].destination_vaddr, pages[0].paddr, page_size),
            ]
        );
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
        pages_in(range, self.size)?;
        if !self.is_materialized() {
            return Ok(BackendRetirement::empty());
        }
        let materialized = pt.drain_present_leaves(range.start, range.size())?;
        for (_, _, _, page_size) in &materialized {
            assert_eq!(*page_size, self.size);
        }
        Ok(BackendRetirement::cow(CowUnmapRetirement {
            leaves: materialized,
        }))
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        preflight_sparse_unmap(range, self.size, pt)
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
            for addr in pages_in(range, self.size)? {
                match pt.query(addr) {
                    Ok((paddr, page_flags, page_size)) => {
                        if self.size != page_size {
                            return Err(AxError::BadAddress);
                        }
                        if access_flags.contains(MappingFlags::WRITE)
                            && !page_flags.contains(MappingFlags::WRITE)
                        {
                            self.handle_cow_fault(addr, paddr, flags, pt)?;
                            pages += 1;
                        } else if page_flags.contains(access_flags) {
                            pages += 1;
                        }
                    }
                    // If the page is not mapped, try map it.
                    Err(PagingError::NotMapped) => {
                        self.alloc_new_at(addr, flags, pt)?;
                        pages += 1;
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
    ) -> AxResult<Backend> {
        let cow_flags = page_table_flags(flags) - MappingFlags::WRITE;
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
            .map(|(vaddr, paddr, source_flags, page_size)| CowClonePage {
                source_vaddr: vaddr,
                destination_vaddr: vaddr,
                paddr,
                source_flags,
                destination_flags: cow_flags,
                page_size,
                protect_source: source_flags.contains(MappingFlags::WRITE),
            });
        let mut ops = CursorCowCloneOps { old_pt, new_pt };
        clone_pages_transactionally(pages, self.size, &mut ops, |paddr| {
            self.get_or_track_frame_ref(paddr)
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
