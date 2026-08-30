//! Page table manipulation.

use axalloc::{UsageKind, global_allocator};
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use page_table_multiarch::PagingHandler;
#[doc(no_inline)]
pub use page_table_multiarch::x86_64::Pkey;
#[doc(no_inline)]
pub use page_table_multiarch::{
    MappingFlags, PageSize, PagingError, PagingResult, PrepareTableFramesError, PreparedMapError,
};

use crate::mem::{phys_to_virt, virt_to_phys};

/// Implementation of [`PagingHandler`], to provide physical memory manipulation to
/// the [page_table_multiarch] crate.
pub struct PagingHandlerImpl;

/// Architecture-specific reservation of preallocated 64-bit page-table
/// frames for one or more prepared leaf publications.
pub type PreparedPageTableFrames = page_table_multiarch::PreparedPageTableFrames<PagingHandlerImpl>;

impl PagingHandler for PagingHandlerImpl {
    fn alloc_frame() -> Option<PhysAddr> {
        Self::alloc_frames(1, PAGE_SIZE_4K)
    }

    fn alloc_frames(num: usize, align: usize) -> Option<PhysAddr> {
        global_allocator()
            .alloc_pages(num, align, UsageKind::PageTable)
            .map(|vaddr| virt_to_phys(vaddr.into()))
            .ok()
    }

    fn dealloc_frame(paddr: PhysAddr) {
        Self::dealloc_frames(paddr, 1)
    }

    fn dealloc_frames(paddr: PhysAddr, num: usize) {
        global_allocator().dealloc_pages(phys_to_virt(paddr).as_usize(), num, UsageKind::PageTable);
    }

    #[inline]
    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        phys_to_virt(paddr)
    }
}

/// The x86_64 page table.
pub type PageTable = page_table_multiarch::x86_64::X64PageTable<PagingHandlerImpl>;
/// The x86_64 page table cursor.
pub type PageTableCursor<'a> =
    page_table_multiarch::x86_64::X64PageTableCursor<'a, PagingHandlerImpl>;
