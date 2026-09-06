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
use spin::Mutex;

use crate::mem::{phys_to_virt, virt_to_phys};

// The boot/runtime kernel root is shared by every address space.  Mutating it
// therefore needs one global serialization point; callers also own the
// architecture-wide TLB invalidation before releasing this gate.
static KERNEL_PAGE_TABLE_GATE: Mutex<()> = Mutex::new(());

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

/// Mutates the permanent kernel page table without taking ownership of its
/// root.  The closure must not retain the borrowed table or cursor.  This is
/// the sole entry point for global direct-map lifecycle changes.
pub fn with_active_kernel_page_table<T>(operation: impl FnOnce(&mut PageTable) -> T) -> T {
    let _gate = KERNEL_PAGE_TABLE_GATE.lock();
    let root = axcpu::asm::kernel_task_page_table_root();
    // SAFETY: boot publication names the permanently owned kernel root;
    // the gate serializes mutations and PageTable's borrowed-root mode never
    // deallocates this boot/runtime-owned hierarchy.
    let mut table = unsafe { PageTable::from_existing_root(root) };
    operation(&mut table)
}
