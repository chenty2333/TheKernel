use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize, PageTable, PageTableCursor};
use axsync::Mutex;
use memory_addr::{PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use super::{
    AddrSpace, Backend, BackendOps, MappingStatus, page_table_flags, preflight_dense_unmap,
};

/// Linear mapping backend.
///
/// The virtual-to-physical offset is linear within a bounded physical window.
#[derive(Clone)]
pub struct LinearBackend {
    start: VirtAddr,
    phys_start: PhysAddr,
    max_size: usize,
    map_id: Arc<()>,
    status: MappingStatus,
}

impl LinearBackend {
    pub(super) const fn mapping_status(&self) -> &MappingStatus {
        &self.status
    }

    pub(super) fn mapping_status_mut(&mut self) -> &mut MappingStatus {
        &mut self.status
    }

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
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
        self.check_range(range)
    }

    fn clone_for_range(
        &self,
        old_start: VirtAddr,
        new_start: VirtAddr,
        map_id: Arc<()>,
    ) -> AxResult<Backend> {
        let (start, backing_advance) =
            super::relocate_affine_origin(self.start, old_start, new_start)?;
        let phys_start = PhysAddr::from(
            self.phys_start
                .as_usize()
                .checked_add(backing_advance)
                .ok_or(AxError::InvalidInput)?,
        );
        let max_size = self
            .max_size
            .checked_sub(backing_advance)
            .ok_or(AxError::InvalidInput)?;
        Ok(Backend::Linear(Self {
            start,
            phys_start,
            // `start` remains the relocated origin of the complete physical
            // window when representable. A low-address suffix rebase advances
            // the physical cursor and drops only the unreachable prefix.
            max_size,
            map_id,
            status: self.status.relocated(old_start, new_start)?,
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
        let map_id = Arc::try_new(()).map_err(|_| AxError::NoMemory)?;
        self.clone_for_range(old_start, new_start, map_id)
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
        let pa_range = PhysAddrRange::try_from_start_size(self.pa(range.start), range.size())
            .ok_or(AxError::InvalidInput)?;
        debug!("Linear::map: {range:?} -> {pa_range:?} {flags:?}");
        pt.map_region(
            range.start,
            |va| self.pa(va),
            range.size(),
            page_table_flags(flags),
            false,
        )?;
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableCursor) -> AxResult {
        self.check_range(range)?;
        let pa_range = PhysAddrRange::try_from_start_size(self.pa(range.start), range.size())
            .ok_or(AxError::InvalidInput)?;
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        pt.unmap_region(range.start, range.size())?;
        Ok(())
    }

    fn preflight_unmap(&self, range: VirtAddrRange, pt: &PageTable) -> AxResult {
        self.check_range(range)?;
        preflight_dense_unmap(range, PageSize::Size4K, pt)
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
            status: MappingStatus::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocating_a_linear_suffix_preserves_coverage_and_merge_identity() {
        let origin = VirtAddr::from(0x4000);
        let relocated_origin = VirtAddr::from(0x10_000);
        let map_id = Arc::new(());
        let backend = LinearBackend {
            start: origin,
            phys_start: PhysAddr::from(0x80_000),
            max_size: 0x4000,
            map_id: map_id.clone(),
            status: MappingStatus::default(),
        };

        let Backend::Linear(first) = backend
            .clone_for_range(origin, relocated_origin, map_id.clone())
            .unwrap()
        else {
            unreachable!();
        };
        let Backend::Linear(suffix) = backend
            .clone_for_range(origin + 0x2000, relocated_origin + 0x2000, map_id)
            .unwrap()
        else {
            unreachable!();
        };

        suffix
            .ensure_range_covered(relocated_origin + 0x2000, 0x2000)
            .unwrap();
        assert!(first.compatible_with(&suffix));
    }

    #[test]
    fn low_address_linear_suffix_rebases_the_physical_window() {
        let origin = VirtAddr::from(0x4000);
        let source = VirtAddr::from(0x8000);
        let destination = VirtAddr::from(0x1000);
        let phys_origin = PhysAddr::from(0x80_000);
        let map_id = Arc::new(());
        let backend = LinearBackend {
            start: origin,
            phys_start: phys_origin,
            max_size: 0x8000,
            map_id: map_id.clone(),
            status: MappingStatus::default(),
        };

        let Backend::Linear(first) = backend
            .clone_for_range(source, destination, map_id.clone())
            .unwrap()
        else {
            unreachable!();
        };
        let Backend::Linear(second_fragment) = backend
            .clone_for_range(source, destination, map_id)
            .unwrap()
        else {
            unreachable!();
        };

        assert_eq!(first.start, destination);
        assert_eq!(first.phys_start, phys_origin + 0x4000);
        assert_eq!(first.max_size, 0x4000);
        assert_eq!(first.pa(destination), backend.pa(source));
        assert_eq!(first.pa(destination + 0x1000), backend.pa(source + 0x1000));
        first.ensure_range_covered(destination, 0x4000).unwrap();
        assert!(first.compatible_with(&second_fragment));
    }
}
