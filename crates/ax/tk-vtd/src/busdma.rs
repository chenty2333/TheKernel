//! Core bus-DMA map load/unload behavior adapted from FreeBSD's IOMMU busdma.
//!
//! Translated from FreeBSD `sys/dev/iommu/busdma_iommu.c` (BSD-2-Clause;
//! FreeBSD source snapshot 2026-10-08). Copyright (c) 2013 The FreeBSD
//! Foundation. This software was developed by Konstantin Belousov
//! <kib@FreeBSD.org> under sponsorship from the FreeBSD Foundation. The busdma
//! tag/callback, VM-page, taskqueue, and KMSAN framework are represented by
//! TheKernel's DMA facade and caller-owned physical ranges.

use alloc::vec::Vec;

use crate::{Backend, Direction, Dma, Error, Mapping, PciRequester};

/// Segment limits corresponding to the constraints carried by a FreeBSD
/// `bus_dma_tag`. A zero boundary means no boundary restriction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Constraints {
    pub max_segment_size: usize,
    pub max_segments: usize,
    pub alignment: u64,
    pub boundary: u64,
    pub low_address: u64,
}

impl Constraints {
    pub const fn unrestricted() -> Self {
        Self {
            max_segment_size: usize::MAX,
            max_segments: usize::MAX,
            alignment: 1,
            boundary: 0,
            low_address: u64::MAX,
        }
    }
}

/// One busdma map's committed IOMMU entries. Failed loads roll back every
/// mapping already installed, matching busdma's all-or-nothing load contract.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DmaMap {
    mappings: Vec<Mapping>,
}

impl DmaMap {
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// Load physical ranges into device-visible segments.
    // upstream: busdma_iommu.c iommu_bus_dmamap_load_something1()
    pub fn load<B: Backend>(
        &mut self,
        dma: &mut Dma<B>,
        requester: PciRequester,
        ranges: &[(u64, usize)],
        direction: Direction,
        limits: Constraints,
    ) -> Result<Vec<(u64, usize)>, Error> {
        if !self.mappings.is_empty()
            || limits.max_segment_size == 0
            || limits.max_segments == 0
            || limits.alignment == 0
            || !limits.alignment.is_power_of_two()
            || (limits.boundary != 0 && !limits.boundary.is_power_of_two())
        {
            return Err(Error::InvalidRange);
        }

        let mut output = Vec::new();
        for &(physical, length) in ranges {
            let mut offset = 0usize;
            while offset < length {
                let address = physical
                    .checked_add(offset as u64)
                    .ok_or(Error::InvalidRange)?;
                let mut chunk = (length - offset).min(limits.max_segment_size);
                if limits.boundary != 0 {
                    let remaining = limits.boundary - (address & (limits.boundary - 1));
                    chunk = chunk.min(remaining as usize);
                }
                if chunk == 0 || address & (limits.alignment - 1) != 0 {
                    self.rollback(dma, requester);
                    return Err(Error::InvalidRange);
                }
                let mapping = match dma.map(requester, address, chunk, direction) {
                    Ok(mapping) => mapping,
                    Err(error) => {
                        self.rollback(dma, requester);
                        return Err(error);
                    }
                };
                let Some(device_end) = mapping.device_address.checked_add(chunk as u64 - 1) else {
                    let _ = dma.unmap(requester, mapping);
                    self.rollback(dma, requester);
                    return Err(Error::InvalidRange);
                };
                if device_end > limits.low_address
                    || (limits.boundary != 0
                        && mapping.device_address / limits.boundary != device_end / limits.boundary)
                {
                    let _ = dma.unmap(requester, mapping);
                    self.rollback(dma, requester);
                    return Err(Error::MapFailed);
                }
                if self.mappings.len() == limits.max_segments {
                    let _ = dma.unmap(requester, mapping);
                    self.rollback(dma, requester);
                    return Err(Error::MapFailed);
                }
                self.mappings.push(mapping);
                output.push((mapping.device_address, chunk));
                offset += chunk;
            }
        }
        Ok(output)
    }

    /// Load a scatter/gather list of physical pages with an initial page offset.
    // upstream: busdma_iommu.c iommu_bus_dmamap_load_ma()
    pub fn load_pages<B: Backend>(
        &mut self,
        dma: &mut Dma<B>,
        requester: PciRequester,
        pages: &[u64],
        first_offset: usize,
        length: usize,
        direction: Direction,
        limits: Constraints,
    ) -> Result<Vec<(u64, usize)>, Error> {
        const PAGE_SIZE: usize = 4096;
        if first_offset >= PAGE_SIZE || (length != 0 && pages.is_empty()) {
            return Err(Error::InvalidRange);
        }
        let page_count = first_offset
            .checked_add(length)
            .and_then(|span| span.checked_add(PAGE_SIZE - 1))
            .ok_or(Error::InvalidRange)?
            / PAGE_SIZE;
        if page_count > pages.len() {
            return Err(Error::InvalidRange);
        }
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(page_count)
            .map_err(|_| Error::OutOfMemory)?;
        let mut remaining = length;
        let mut page_offset = first_offset;
        for &page in pages.iter().take(page_count) {
            if page & (PAGE_SIZE as u64 - 1) != 0 {
                return Err(Error::InvalidRange);
            }
            let chunk = remaining.min(PAGE_SIZE - page_offset);
            if chunk != 0 {
                ranges.push((
                    page.checked_add(page_offset as u64)
                        .ok_or(Error::InvalidRange)?,
                    chunk,
                ));
                remaining -= chunk;
            }
            page_offset = 0;
        }
        if remaining != 0 {
            return Err(Error::InvalidRange);
        }
        self.load(dma, requester, &ranges, direction, limits)
    }

    /// Load one contiguous physical extent through the generic segment mapper.
    // upstream: busdma_iommu.c iommu_bus_dmamap_load_phys()
    pub fn load_phys<B: Backend>(
        &mut self,
        dma: &mut Dma<B>,
        requester: PciRequester,
        physical: u64,
        length: usize,
        direction: Direction,
        limits: Constraints,
    ) -> Result<Vec<(u64, usize)>, Error> {
        self.load(dma, requester, &[(physical, length)], direction, limits)
    }

    /// Tear down all IOMMU entries owned by this map.
    // upstream: busdma_iommu.c iommu_bus_dmamap_unload()
    pub fn unload<B: Backend>(
        &mut self,
        dma: &mut Dma<B>,
        requester: PciRequester,
    ) -> Result<(), Error> {
        let mappings = core::mem::take(&mut self.mappings);
        let mut first_error = None;
        for mapping in mappings.into_iter().rev() {
            if let Err(error) = dma.unmap(requester, mapping) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn rollback<B: Backend>(&mut self, dma: &mut Dma<B>, requester: PciRequester) {
        for mapping in core::mem::take(&mut self.mappings).into_iter().rev() {
            let _ = dma.unmap(requester, mapping);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[derive(Default)]
    struct Fake {
        active: Vec<(u64, usize)>,
    }
    impl Backend for Fake {
        fn map(
            &mut self,
            _: PciRequester,
            physical: u64,
            length: usize,
            _: Direction,
        ) -> Result<u64, Error> {
            let device = physical + 0x1000;
            self.active.push((device, length));
            Ok(device)
        }
        fn unmap(&mut self, _: PciRequester, address: u64, length: usize) -> Result<(), Error> {
            let Some(index) = self
                .active
                .iter()
                .position(|entry| *entry == (address, length))
            else {
                return Err(Error::InvalidRange);
            };
            self.active.remove(index);
            Ok(())
        }
    }

    fn setup() -> (Dma<Fake>, PciRequester) {
        (
            Dma::new(Fake::default(), true),
            PciRequester {
                segment: 0,
                bus: 0,
                device: 1,
                function: 0,
            },
        )
    }

    #[test]
    fn load_splits_at_segment_and_boundary_limits_then_unloads() {
        let (mut dma, requester) = setup();
        let mut map = DmaMap::default();
        let segments = map
            .load(
                &mut dma,
                requester,
                &[(0x1000, 0x3000)],
                Direction::Bidirectional,
                Constraints {
                    max_segment_size: 0x2000,
                    max_segments: 4,
                    alignment: 0x1000,
                    boundary: 0x1000,
                    low_address: u64::MAX,
                },
            )
            .unwrap();
        assert_eq!(
            segments,
            vec![(0x2000, 0x1000), (0x3000, 0x1000), (0x4000, 0x1000)]
        );
        assert_eq!(map.mappings().len(), 3);
        map.unload(&mut dma, requester).unwrap();
        assert!(map.mappings().is_empty());
        assert!(dma.into_backend().active.is_empty());
    }

    #[test]
    fn load_rolls_back_partial_work_when_segment_limit_is_exceeded() {
        let (mut dma, requester) = setup();
        let mut map = DmaMap::default();
        let result = map.load(
            &mut dma,
            requester,
            &[(0x1000, 0x3000)],
            Direction::ToDevice,
            Constraints {
                max_segment_size: 0x1000,
                max_segments: 2,
                alignment: 0x1000,
                boundary: 0,
                low_address: u64::MAX,
            },
        );
        assert_eq!(result, Err(Error::MapFailed));
        assert!(map.mappings().is_empty());
        assert!(dma.into_backend().active.is_empty());
    }

    #[test]
    fn load_ma_preserves_page_offset_across_scattered_physical_pages() {
        let (mut dma, requester) = setup();
        let mut map = DmaMap::default();
        let segments = map
            .load_pages(
                &mut dma,
                requester,
                &[0x10_000, 0x30_000],
                0x800,
                0x1000,
                Direction::FromDevice,
                Constraints::unrestricted(),
            )
            .unwrap();
        assert_eq!(segments, vec![(0x11_800, 0x800), (0x31_000, 0x800)]);
        map.unload(&mut dma, requester).unwrap();
        assert!(dma.into_backend().active.is_empty());
    }
}
