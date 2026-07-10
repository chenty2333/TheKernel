use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    slice,
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::CachedFile;
use axfs_ng_vfs::Location;
use axhal::{
    mem::phys_to_virt,
    paging::{MappingFlags, PageSize, PageTableCursor, PagingError},
};
use axsync::Mutex;
use kspin::SpinNoIrq;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, PopulateCallback, alloc_frame, dealloc_frame, page_table_flags,
    pages_in,
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

fn relocate_backend_start(
    backend_start: VirtAddr,
    old_start: VirtAddr,
    new_start: VirtAddr,
) -> VirtAddr {
    if backend_start >= old_start {
        new_start + backend_start.sub_addr(old_start)
    } else {
        new_start.sub(old_start.sub_addr(backend_start))
    }
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
}

impl CowBackend {
    const ANON_FAULT_AROUND_PAGES: usize = 4;

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
            let read = match file.read_at(&mut &mut buf[start..start + max_read], file_start) {
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
        let Some(frame) = frame else {
            pt.protect(vaddr, page_table_flags(flags))?;
            self.mark_materialized();
            return Ok(());
        };
        let mut frame = frame.lock();
        assert!(frame.0 > 0, "invalid frame reference count");
        match frame.0 {
            1 => {
                // Only one reference, just upgrade the permissions.
                pt.protect(vaddr, page_table_flags(flags))?;
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
                frame.drop_frame(paddr, self.size);
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
    ) -> Self {
        Self {
            start: relocate_backend_start(self.start, old_start, new_start),
            size: self.size,
            file: self.file.clone(),
            map_id,
            materialized: self.materialized.clone(),
        }
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

    pub(crate) fn clone_for_range(&self, old_start: VirtAddr, new_start: VirtAddr) -> Self {
        self.clone_for_range_with_id(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(&self, old_start: VirtAddr, new_start: VirtAddr) -> Self {
        self.clone_for_range_with_id(old_start, new_start, Arc::new(()))
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
        for (old_addr, paddr, flags, page_size) in materialized {
            if page_size != self.size {
                return Err(AxError::BadAddress);
            }
            let new_addr = new_start + old_addr.sub_addr(old_start);
            let frame = self.get_or_track_frame_ref(paddr);
            let mut frame = frame.lock();
            let Some(next_refcnt) = frame.0.checked_add(1) else {
                return Err(AxError::BadAddress);
            };
            frame.0 = next_refcnt;
            drop(frame);
            if let Err(err) = pt.map(new_addr, paddr, self.size, page_table_flags(flags)) {
                let frame = self.get_or_track_frame_ref(paddr);
                let mut frame = frame.lock();
                frame.drop_frame(paddr, self.size);
                return Err(err.into());
            }
        }
        Ok(())
    }

    pub(crate) fn is_private_anonymous(&self) -> bool {
        self.file.is_none()
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

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        debug!("Cow::unmap: {range:?}");
        pages_in(range, self.size)?;
        if !self.is_materialized() {
            return Ok(());
        }
        let materialized = pt.drain_present_leaves(range.start, range.size())?;
        for (_addr, frame, _flags, page_size) in materialized {
            assert_eq!(page_size, self.size);
            let frame_ref = { FRAME_TABLE.lock().get_frame_ref(frame) };
            if let Some(frame_ref) = frame_ref {
                let mut frame_ref = frame_ref.lock();
                frame_ref.drop_frame(frame, self.size);
            } else {
                dealloc_frame(frame, self.size);
            }
        }
        Ok(())
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pt: &mut PageTableCursor,
    ) -> AxResult<(usize, Option<PopulateCallback>)> {
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
        Ok((pages, None))
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
        for (vaddr, paddr, page_flags, page_size) in materialized {
            if page_size != self.size {
                return Err(AxError::BadAddress);
            }
            // If the page is mapped in the old page table:
            // - Update its permissions in the old page table using `flags`.
            // - Map the same physical page into the new page table at the same
            // virtual address, with the same page size and `flags`.
            let frame = self.get_or_track_frame_ref(paddr);
            let mut frame = frame.lock();
            let Some(next_refcnt) = frame.0.checked_add(1) else {
                warn!("frame reference count overflow");
                return Err(AxError::BadAddress);
            };
            assert!(frame.0 > 0, "referencing unreferenced frame");
            frame.0 = next_refcnt;
            drop(frame);
            let protected_parent = page_flags.contains(MappingFlags::WRITE);
            if protected_parent {
                if let Err(err) = old_pt.protect(vaddr, cow_flags) {
                    let frame = self.get_or_track_frame_ref(paddr);
                    let mut frame = frame.lock();
                    frame.drop_frame(paddr, self.size);
                    return Err(err.into());
                }
            }
            if let Err(err) = new_pt.map(vaddr, paddr, self.size, cow_flags) {
                if protected_parent {
                    let _ = old_pt.protect(vaddr, page_table_flags(flags));
                }
                let frame = self.get_or_track_frame_ref(paddr);
                let mut frame = frame.lock();
                frame.drop_frame(paddr, self.size);
                return Err(err.into());
            }
        }

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
        })
    }

    pub fn new_alloc(start: VirtAddr, size: PageSize) -> Self {
        Self::Cow(CowBackend {
            start,
            size,
            file: None,
            map_id: Arc::new(()),
            materialized: Arc::new(AtomicBool::new(false)),
        })
    }
}
