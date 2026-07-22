//! LoongArch64 specific page table structures.

use core::arch::asm;

use memory_addr::VirtAddr;
#[cfg(not(feature = "asid-fast-switch"))]
use memory_addr::{MemoryAddr, PAGE_SIZE_4K};
use page_table_entry::loongarch64::LA64PTE;

use crate::{PageTable64, PageTable64Cursor, PagingMetaData};

/// Metadata of LoongArch64 page tables.
#[derive(Copy, Clone, Debug)]
pub struct LA64MetaData;

impl LA64MetaData {
    /// PWCL(Page Walk Controller for Lower Half Address Space) CSR flags
    ///
    /// <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-lower-half-address-space>
    ///
    /// | BitRange | Name      | Value |
    /// | ----     | ----      | ----  |
    /// | 4:0      | PTBase    |    12 |
    /// | 9:5      | PTWidth   |     9 |
    /// | 14:10    | Dir1Base  |    21 |
    /// | 19:15    | Dir1Width |     9 |
    /// | 24:20    | Dir2Base  |    30 |
    /// | 29:25    | Dir2Width |     9 |
    /// | 31:30    | PTEWidth  |     0 |
    pub const PWCL_VALUE: u32 = 12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25);

    /// PWCH(Page Walk Controller for Higher Half Address Space) CSR flags
    ///
    /// <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-higher-half-address-space>
    ///
    /// | BitRange | Name                            | Value |
    /// | ----     | ----                            | ----  |
    /// | 5:0      | Dir3Base                        |    39 |
    /// | 11:6     | Dir3Width                       |     9 |
    /// | 17:12    | Dir4Base                        |     0 |
    /// | 23:18    | Dir4Width                       |     0 |
    /// | 24       | 0                               |     0 |
    /// | 24       | HPTW_En(CPUCFG.2.HPTW(bit24)=1) |     0 |
    /// | 31:25    | 0                               |     0 |
    pub const PWCH_VALUE: u32 = 39 | (9 << 6);
}

impl PagingMetaData for LA64MetaData {
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 48;
    const VA_MAX_BITS: usize = 48;

    type VirtAddr = VirtAddr;

    #[inline]
    fn flush_tlb(vaddr: Option<VirtAddr>) {
        #[cfg(feature = "asid-fast-switch")]
        let _ = vaddr;
        #[cfg(feature = "asid-fast-switch")]
        unsafe {
            // A cursor can own an inactive page-table root; the current ASID
            // therefore cannot safely scope this invalidation.  Keep the
            // generic cursor conservative until owner scope is part of its
            // API.  Current-address-space fault repair uses axcpu's explicit
            // VA+current-ASID helper.
            asm!("dbar 0; invtlb 0x00, $r0, $r0");
        }

        #[cfg(not(feature = "asid-fast-switch"))]
        unsafe {
            if let Some(vaddr) = vaddr {
                // One LoongArch TLB entry contains the adjacent even/odd page
                // pair. INVTLB's VA operand therefore names the pair rather
                // than an individual 4-KiB leaf.  The default configuration
                // installs every user root with ASID 0.
                let pair = vaddr.align_down(2 * PAGE_SIZE_4K);
                asm!("dbar 0; invtlb 0x05, $r0, {reg}", reg = in(reg) pair.as_usize());
            } else {
                asm!("dbar 0; invtlb 0x00, $r0, $r0");
            }
        }
    }
}

/// loongarch64 page table
///
/// <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#section-multi-level-page-table-structure-supported-by-page-walking>
///
/// 4 levels:
///
/// using page table dir3, dir2, dir1 and pt, ignore dir4
pub type LA64PageTable<H> = PageTable64<LA64MetaData, LA64PTE, H>;
/// loongarch64 page table cursor.
pub type LA64PageTableCursor<'a, H> = PageTable64Cursor<'a, LA64MetaData, LA64PTE, H>;
