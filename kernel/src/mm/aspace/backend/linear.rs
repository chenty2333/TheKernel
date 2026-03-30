use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTableCursor};
use axsync::Mutex;
use memory_addr::{PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use super::{AddrSpace, Backend, BackendOps};

/// Linear mapping backend.
///
/// The virtual-to-physical offset is linear within a bounded physical window.
#[derive(Clone)]
pub struct LinearBackend {
    start: VirtAddr,
    phys_start: PhysAddr,
    max_size: usize,
    map_id: Arc<()>,
}

impl LinearBackend {
    fn check_range(&self, range: VirtAddrRange) -> AxResult {
        let offset = range
            .start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let end = offset
            .checked_add(range.size())
            .ok_or(AxError::InvalidInput)?;
        if end > self.max_size {
            return Err(AxError::NoMemory);
        }
        Ok(())
    }

    fn pa(&self, va: VirtAddr) -> PhysAddr {
        self.phys_start + (va - self.start)
    }

    pub(crate) fn ensure_range_covered(&self, start: VirtAddr, size: usize) -> AxResult {
        self.check_range(VirtAddrRange::from_start_size(start, size))
    }

    fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: Arc<()>,
    ) -> AxResult<Backend> {
        let prefix = old_start
            .as_usize()
            .checked_sub(self.start.as_usize())
            .ok_or(AxError::InvalidInput)?;
        let start = VirtAddr::from(
            new_start
                .as_usize()
                .checked_sub(prefix)
                .ok_or(AxError::InvalidInput)?,
        );
        Ok(Backend::Linear(Self {
            start,
            phys_start: self.phys_start,
            max_size: self
                .max_size
                .checked_sub(prefix)
                .ok_or(AxError::InvalidInput)?,
            map_id,
        }))
    }

    pub(crate) fn relocate(&self, old_start: VirtAddr, new_start: VirtAddr) -> AxResult<Backend> {
        self.clone_for_range(old_start, new_start, self.map_id.clone())
    }

    pub(crate) fn duplicate_mapping(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> AxResult<Backend> {
        self.clone_for_range(old_start, new_start, Arc::new(()))
    }

    pub(crate) fn compatible_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.map_id, &other.map_id)
            && self.start == other.start
            && self.phys_start == other.phys_start
            && self.max_size == other.max_size
    }
}

impl BackendOps for LinearBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pt: &mut PageTableCursor) -> AxResult {
        self.check_range(range)?;
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::map: {range:?} -> {pa_range:?} {flags:?}");
        pt.map_region(range.start, |va| self.pa(va), range.size(), flags, false)?;
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        self.check_range(range)?;
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        pt.unmap_region(range.start, range.size())?;
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
        Ok(Backend::Linear(self.clone()))
    }
}

impl Backend {
    pub fn new_linear(start: VirtAddr, phys_start: PhysAddr, max_size: usize) -> Self {
        Self::Linear(LinearBackend {
            start,
            phys_start,
            max_size,
            map_id: Arc::new(()),
        })
    }
}
