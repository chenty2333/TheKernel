//! Generic IOMMU page-table geometry adapters from FreeBSD `iommu_utils.c`.
//!
//! Translated from FreeBSD `sys/x86/iommu/iommu_utils.c` (BSD-2-Clause;
//! FreeBSD source snapshot 2026-10-08). Copyright (c) 2013, 2014, 2024 The
//! FreeBSD Foundation. This software was developed by Konstantin Belousov
//! <kib@FreeBSD.org> under sponsorship from the FreeBSD Foundation.
//! VM objects, sf_buf mappings, bus topology, interrupt resources and debugger
//! registration remain TheKernel framework responsibilities.

use crate::{Error, busdma::Constraints};

pub const PAGE_SHIFT: u32 = 12;
pub const PTE_INDEX_BITS: u32 = 9;
pub const PTE_PER_PAGE: u64 = 1 << PTE_INDEX_BITS;
pub const PTE_INDEX_MASK: u64 = PTE_PER_PAGE - 1;

/// Offset of the page-table entry mapping `base` at `level`.
// upstream: iommu_utils.c pglvl_pgtbl_pte_off()
pub fn pglvl_pgtbl_pte_off(page_levels: u8, base: u64, level: u8) -> Result<usize, Error> {
    if page_levels == 0 || level >= page_levels {
        return Err(Error::InvalidRange);
    }
    let shift = PAGE_SHIFT + u32::from(page_levels - level - 1) * PTE_INDEX_BITS;
    Ok(((base >> shift) & PTE_INDEX_MASK) as usize)
}

/// Page-table object index that contains the entry for `base` at `level`.
// upstream: iommu_utils.c pglvl_pgtbl_get_pindex()
pub fn pglvl_pgtbl_get_pindex(page_levels: u8, base: u64, level: u8) -> Result<u64, Error> {
    if page_levels == 0 || level >= page_levels {
        return Err(Error::InvalidRange);
    }
    let mut index = 0u64;
    let mut parent = 0u64;
    for current_level in 0..level {
        let offset = pglvl_pgtbl_pte_off(page_levels, base, current_level)? as u64;
        index = offset
            .checked_add(
                parent
                    .checked_mul(PTE_PER_PAGE)
                    .ok_or(Error::InvalidRange)?,
            )
            .and_then(|index| index.checked_add(1))
            .ok_or(Error::InvalidRange)?;
        parent = index;
    }
    Ok(index)
}

/// Maximum number of page-table pages needed to cover one address space.
// upstream: iommu_utils.c pglvl_max_pages()
pub fn pglvl_max_pages(page_levels: u8) -> Result<u64, Error> {
    if page_levels == 0 || page_levels > 7 {
        return Err(Error::InvalidRange);
    }
    let mut pages = 0u64;
    for _ in 0..page_levels {
        pages = pages
            .checked_mul(PTE_PER_PAGE)
            .and_then(|value| value.checked_add(1))
            .ok_or(Error::InvalidRange)?;
    }
    Ok(pages)
}

/// Guest-address page size mapped by a page-table level.
// upstream: iommu_utils.c pglvl_page_size()
pub fn pglvl_page_size(total_levels: u8, level: u8) -> Result<u64, Error> {
    if total_levels == 0 || level >= total_levels || total_levels > 7 {
        return Err(Error::InvalidRange);
    }
    let shift = PAGE_SHIFT + u32::from(total_levels - level - 1) * PTE_INDEX_BITS;
    1u64.checked_shl(shift).ok_or(Error::InvalidRange)
}

/// Build the bus-DMA limits installed from a requester domain's address width.
// upstream: iommu_utils.c iommu_device_tag_init()
pub fn iommu_device_dma_constraints(domain_end: u64, bus_space_max: u64) -> Constraints {
    let maximum = domain_end.min(bus_space_max);
    Constraints {
        max_segment_size: maximum.min(usize::MAX as u64) as usize,
        max_segments: usize::MAX,
        alignment: 1,
        boundary: 0,
        low_address: maximum,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pte_offsets_and_table_page_indexes_follow_radix_walk() {
        let address = 0x1234_5678_9abc;
        assert_eq!(pglvl_pgtbl_pte_off(4, address, 0).unwrap(), 0x24);
        assert_eq!(pglvl_pgtbl_pte_off(4, address, 1).unwrap(), 0x0d1);
        assert_eq!(pglvl_pgtbl_pte_off(4, address, 2).unwrap(), 0x0b3);
        assert_eq!(pglvl_pgtbl_pte_off(4, address, 3).unwrap(), 0x189);
        assert_eq!(pglvl_pgtbl_get_pindex(4, address, 0).unwrap(), 0);
        assert_eq!(pglvl_pgtbl_get_pindex(4, address, 1).unwrap(), 0x25);
        assert_eq!(
            pglvl_pgtbl_get_pindex(4, address, 2).unwrap(),
            0x25 * 512 + 0x0d2
        );
        assert_eq!(
            pglvl_pgtbl_get_pindex(4, address, 3).unwrap(),
            (0x25 * 512 + 0x0d2) * 512 + 0x0b4
        );
    }

    #[test]
    fn table_counts_and_level_sizes_are_bounded() {
        assert_eq!(pglvl_max_pages(1).unwrap(), 1);
        assert_eq!(pglvl_max_pages(4).unwrap(), 134_480_385);
        assert_eq!(pglvl_page_size(4, 0).unwrap(), 1 << 39);
        assert_eq!(pglvl_page_size(4, 3).unwrap(), 1 << 12);
        assert_eq!(pglvl_page_size(4, 4), Err(Error::InvalidRange));
        assert_eq!(pglvl_max_pages(8), Err(Error::InvalidRange));
    }

    #[test]
    fn requester_constraints_use_domain_and_bus_dma_low_address() {
        let limits = iommu_device_dma_constraints(0xffff_ffff, 0xffff);
        assert_eq!(limits.low_address, 0xffff);
        assert_eq!(limits.max_segment_size, 0xffff);
        assert_eq!(limits.max_segments, usize::MAX);
    }
}
