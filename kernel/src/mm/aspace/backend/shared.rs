use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTableCursor};
use axsync::Mutex;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};

use super::{AddrSpace, Backend, BackendOps, alloc_frame, dealloc_frame, divide_page, pages_in};

pub struct SharedPages {
    phys_pages: Mutex<Vec<PhysAddr>>,
    pub size: PageSize,
}
impl SharedPages {
    pub fn new(size: usize, page_size: PageSize) -> AxResult<Self> {
        let num_pages = divide_page(size, page_size);
        let mut phys_pages = Vec::with_capacity(num_pages);
        for _ in 0..num_pages {
            phys_pages.push(alloc_frame(true, page_size)?);
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

        let mut new_pages = Vec::with_capacity(len - current_len);
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
        pages.extend(new_pages);
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
        debug_assert!(start.is_aligned(self.pages.size));
        let start_index = divide_page(start - self.start, self.pages.size);
        let count = divide_page(size, self.pages.size);
        self.pages.ensure_len(start_index + count)
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult {
        debug!("Shared::map: {:?} {:?}", range, flags);
        let start_index = divide_page(range.start - self.start, self.pages.size);
        let count = divide_page(range.size(), self.pages.size);
        let pages = self.pages.pages_range(start_index, count)?;
        for (vaddr, paddr) in pages_in(range, self.pages.size)?.zip(pages.into_iter()) {
            pt.map(vaddr, paddr, self.pages.size, flags)?;
        }
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        debug!("Shared::unmap: {:?}", range);
        for vaddr in pages_in(range, self.pages.size)? {
            pt.unmap(vaddr)?;
        }
        Ok(())
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
        Self::Shared(SharedBackend {
            start,
            pages,
            map_id: Arc::new(()),
        })
    }
}
