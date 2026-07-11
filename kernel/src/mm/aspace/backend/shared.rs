use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTableCursor, PagingError};
use axsync::Mutex;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, PopulateCallback, alloc_frame, dealloc_frame, divide_page,
    page_table_flags, pages_in,
};

pub struct SharedPages {
    phys_pages: Mutex<Vec<PhysAddr>>,
    pub size: PageSize,
}
impl SharedPages {
    pub fn new(size: usize, page_size: PageSize) -> AxResult<Self> {
        if !page_size.is_aligned(size) {
            return Err(AxError::InvalidInput);
        }
        let num_pages = size / page_size as usize;
        let mut phys_pages = Vec::new();
        phys_pages
            .try_reserve_exact(num_pages)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..num_pages {
            match alloc_frame(true, page_size) {
                Ok(frame) => phys_pages.push(frame),
                Err(err) => {
                    for frame in phys_pages {
                        dealloc_frame(frame, page_size);
                    }
                    return Err(err);
                }
            }
        }
        Ok(Self {
            phys_pages: Mutex::new(phys_pages),
            size: page_size,
        })
    }

    pub fn len(&self) -> usize {
        self.phys_pages.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.phys_pages.lock().is_empty()
    }

    pub fn ensure_len(&self, len: usize) -> AxResult {
        let current_len = self.phys_pages.lock().len();
        if current_len >= len {
            return Ok(());
        }

        let mut new_pages = Vec::new();
        new_pages
            .try_reserve_exact(len - current_len)
            .map_err(|_| AxError::NoMemory)?;
        for _ in current_len..len {
            match alloc_frame(true, self.size) {
                Ok(frame) => new_pages.push(frame),
                Err(err) => {
                    for frame in new_pages {
                        dealloc_frame(frame, self.size);
                    }
                    return Err(err);
                }
            }
        }

        let mut pages = self.phys_pages.lock();
        if pages.len() >= len {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Ok(());
        }
        let needed = len - pages.len();
        if pages.try_reserve_exact(needed).is_err() {
            drop(pages);
            for frame in new_pages {
                dealloc_frame(frame, self.size);
            }
            return Err(AxError::NoMemory);
        }
        let unused = new_pages.split_off(needed);
        pages.extend(new_pages);
        drop(pages);
        for frame in unused {
            dealloc_frame(frame, self.size);
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> usize {
        self.len() * self.size as usize
    }

    pub fn read_bytes(&self, offset: usize, mut buf: &mut [u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)? > self.total_bytes() {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        let pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = pages[page_index];
            let src = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(src as *const u8, buf.as_mut_ptr(), chunk_len);
            }
            let (_, rest) = buf.split_at_mut(chunk_len);
            buf = rest;
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    pub fn write_bytes(&self, offset: usize, mut buf: &[u8]) -> AxResult {
        if offset.checked_add(buf.len()).ok_or(AxError::InvalidInput)? > self.total_bytes() {
            return Err(AxError::InvalidInput);
        }

        let page_bytes = self.size as usize;
        let pages = self.phys_pages.lock();
        let mut page_index = offset / page_bytes;
        let mut page_offset = offset % page_bytes;

        while !buf.is_empty() {
            let phys = pages[page_index];
            let dst = axhal::mem::phys_to_virt(phys).as_usize() + page_offset;
            let chunk_len = (page_bytes - page_offset).min(buf.len());
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, chunk_len);
            }
            buf = &buf[chunk_len..];
            page_index += 1;
            page_offset = 0;
        }

        Ok(())
    }

    fn pages_range(&self, start_index: usize, count: usize) -> AxResult<Vec<PhysAddr>> {
        let pages = self.phys_pages.lock();
        let end = start_index
            .checked_add(count)
            .ok_or(AxError::InvalidInput)?;
        if end > pages.len() {
            return Err(AxError::NoMemory);
        }
        Ok(pages[start_index..end].to_vec())
    }
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        for frame in self.phys_pages.lock().iter() {
            dealloc_frame(*frame, self.size);
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    pages: Arc<SharedPages>,
    may_protect: MappingFlags,
    map_id: Arc<()>,
}
impl SharedBackend {
    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.map_id, &other.map_id)
            && self.start == other.start
            && Arc::ptr_eq(&self.pages, &other.pages)
    }

    fn clone_for_range_with_id(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: Arc<()>,
    ) -> Self {
        let delta = old_start.sub_addr(self.start);
        Self {
            start: new_start.sub(delta),
            pages: self.pages.clone(),
            may_protect: self.may_protect,
            map_id,
        }
    }

    pub(crate) fn clone_for_range(&self, old_start: VirtAddr, new_start: VirtAddr) -> Self {
        self.clone_for_range_with_id(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(&self, old_start: VirtAddr, new_start: VirtAddr) -> Self {
        self.clone_for_range_with_id(old_start, new_start, Arc::new(()))
    }

    pub(crate) fn ensure_range_covered(&self, start: VirtAddr, size: usize) -> AxResult {
        let offset = start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let start_index = divide_page(offset, self.pages.size)?;
        let count = divide_page(size, self.pages.size)?;
        self.pages.ensure_len(
            start_index
                .checked_add(count)
                .ok_or(AxError::InvalidInput)?,
        )
    }

    pub(crate) fn check_protect_flags(&self, flags: MappingFlags) -> AxResult {
        let requested = flags & access_flags();
        if !self.may_protect.contains(requested) {
            return Err(AxError::PermissionDenied);
        }
        Ok(())
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        debug!("Shared::map: {:?} {:?}", range, flags);
        pages_in(range, self.pages.size)?;
        self.check_protect_flags(flags)?;
        Ok(())
    }

    fn on_protect(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pt: &mut PageTableCursor,
    ) -> AxResult {
        self.check_protect_flags(new_flags)
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        debug!("Shared::unmap: {:?}", range);
        for vaddr in pages_in(range, self.pages.size)? {
            match pt.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(err.into()),
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
        let offset = range
            .start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let start_index = divide_page(offset, self.pages.size)?;
        let count = divide_page(range.size(), self.pages.size)?;
        let pages = self.pages.pages_range(start_index, count)?;
        let mut populated = 0;

        for (vaddr, paddr) in pages_in(range, self.pages.size)?.zip(pages.into_iter()) {
            match pt.query(vaddr) {
                Ok((mapped_paddr, page_flags, page_size)) => {
                    if page_size != self.pages.size || mapped_paddr != paddr {
                        return Err(AxError::BadAddress);
                    }
                    if access_flags.contains(MappingFlags::WRITE)
                        && !page_flags.contains(MappingFlags::WRITE)
                    {
                        pt.remap(vaddr, paddr, page_table_flags(flags))?;
                        populated += 1;
                    } else if page_flags.contains(access_flags) {
                        populated += 1;
                    }
                }
                Err(PagingError::NotMapped) => {
                    pt.map(vaddr, paddr, self.pages.size, page_table_flags(flags))?;
                    populated += 1;
                }
                Err(_) => return Err(AxError::BadAddress),
            }
        }

        Ok((populated, None))
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pt: &mut PageTableCursor,
        _new_pt: &mut PageTableCursor,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }
}

impl Backend {
    pub fn new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> Self {
        Self::new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> AxResult<Self> {
        Self::try_new_shared_with_may_protect(start, pages, access_flags())
    }

    pub fn try_new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> AxResult<Self> {
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        Ok(Self::Shared(SharedBackend {
            start,
            pages,
            may_protect: may_protect & access_flags(),
            map_id,
        }))
    }

    pub fn new_shared_with_may_protect(
        start: VirtAddr,
        pages: Arc<SharedPages>,
        may_protect: MappingFlags,
    ) -> Self {
        Self::Shared(SharedBackend {
            start,
            pages,
            may_protect: may_protect & access_flags(),
            map_id: Arc::new(()),
        })
    }
}

fn access_flags() -> MappingFlags {
    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE
}
