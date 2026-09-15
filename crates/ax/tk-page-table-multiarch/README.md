# page_table_multiarch

[![Crates.io](https://img.shields.io/crates/v/page_table_multiarch)](https://crates.io/crates/page_table_multiarch)
[![Docs.rs](https://docs.rs/page_table_multiarch/badge.svg)](https://docs.rs/page_table_multiarch)
[![CI](https://github.com/arceos-org/page_table_multiarch/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/arceos-org/page_table_multiarch/actions/workflows/ci.yml)

This crate provides generic, unified, and OS-free page table structures for x86_64.

The core struct is [`PageTable64<M, PTE, H>`][1]. OS-functions and architecture-dependent types are provided by generic parameters:

- `M`: The architecture-dependent metadata, requires to implement the [`PagingMetaData`][3] trait.
- `PTE`: The architecture-dependent page table entry, requires to implement the [`GenericPTE`][4] trait.
- `H`: OS-functions such as physical memory allocation, requires to implement the [`PagingHandler`][5] trait.

The supported architecture and page table structure is:

- x86_64: [`x86_64::X64PageTable`][2]

[1]: https://docs.rs/page_table_multiarch/latest/page_table_multiarch/struct.PageTable64.html
[2]: https://docs.rs/page_table_multiarch/latest/page_table_multiarch/x86_64/type.X64PageTable.html
[3]: https://docs.rs/page_table_multiarch/latest/page_table_multiarch/trait.PagingMetaData.html
[4]: https://docs.rs/page_table_entry/latest/page_table_entry/trait.GenericPTE.html
[5]: https://docs.rs/page_table_multiarch/latest/page_table_multiarch/trait.PagingHandler.html


## Examples (x86_64)

```rust
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr};
use page_table_multiarch::x86_64::{X64PageTable};
use page_table_multiarch::{MappingFlags, PagingHandler, PageSize};

use core::alloc::Layout;

extern crate alloc;

struct PagingHandlerImpl;

impl PagingHandler for PagingHandlerImpl {
    fn alloc_frame() -> Option<PhysAddr> {
        let layout = Layout::from_size_align(0x1000, 0x1000).unwrap();
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        Some(PhysAddr::from(ptr as usize))
    }

    fn alloc_frames(num_pages: usize, align: usize) -> Option<PhysAddr> {
        let layout = Layout::from_size_align(num_pages * 0x1000, align).unwrap();
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        Some(PhysAddr::from(ptr as usize))
    }

    fn dealloc_frame(paddr: PhysAddr) {
        let layout = Layout::from_size_align(0x1000, 0x1000).unwrap();
        let ptr = paddr.as_usize() as *mut u8;
        unsafe { alloc::alloc::dealloc(ptr, layout) };
    }

    fn dealloc_frames(paddr: PhysAddr, num_pages: usize) {
        let layout = Layout::from_size_align(num_pages * 0x1000, 0x1000).unwrap();
        let ptr = paddr.as_usize() as *mut u8;
        unsafe { alloc::alloc::dealloc(ptr, layout) };
    }

    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        VirtAddr::from(paddr.as_usize())
    }
}

let vaddr = VirtAddr::from(0xdead_beef_000);
let paddr = PhysAddr::from(0x2000);
let flags = MappingFlags::READ | MappingFlags::WRITE;
let mut pt = X64PageTable::<PagingHandlerImpl>::try_new().unwrap();

assert!(pt.root_paddr().is_aligned_4k());
assert!(pt.cursor().map(vaddr, paddr, PageSize::Size4K, flags).is_ok());
assert_eq!(pt.query(vaddr), Ok((paddr, flags, PageSize::Size4K)));
```
