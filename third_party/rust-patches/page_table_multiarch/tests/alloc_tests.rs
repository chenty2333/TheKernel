use std::{
    alloc::{self, GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    marker::PhantomData,
    ptr,
};

use memory_addr::{PhysAddr, VirtAddr};
use page_table_entry::{GenericPTE, MappingFlags};
use page_table_multiarch::{
    PageSize, PageTable64, PagingError, PagingHandler, PagingMetaData, PagingResult,
};
use rand::{RngExt, SeedableRng, rngs::SmallRng};

/// Creates a layout for allocating `num` pages with alignment of `2^align_pow2`
/// pages.
const fn pages_layout(num: usize, align: usize) -> Layout {
    if !align.is_power_of_two() {
        panic!("alignment must be a power of two");
    }
    if align % 4096 != 0 {
        panic!("alignment must be a multiple of 4K");
    }
    unsafe { Layout::from_size_align_unchecked(4096 * num, align) }
}

const PAGE_LAYOUT: Layout = pages_layout(1, 4096);

thread_local! {
    static ALLOCATED: RefCell<HashSet<usize>> = RefCell::default();
    static ALIGN: RefCell<HashMap<usize, usize>> = RefCell::default();
    static FAIL_NEXT_HEAP_ALLOCATION: Cell<bool> = const { Cell::new(false) };
    static TRACK_HEAP_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static HEAP_ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static FAIL_FRAME_ALLOCATION_AT: Cell<Option<usize>> = const { Cell::new(None) };
    static FRAME_ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static CORRUPT_TABLE_PTR_FLAGS: Cell<bool> = const { Cell::new(false) };
}

struct TestAllocator;

#[global_allocator]
static TEST_ALLOCATOR: TestAllocator = TestAllocator;

fn heap_allocation_should_fail() -> bool {
    FAIL_NEXT_HEAP_ALLOCATION
        .try_with(|fail| fail.replace(false))
        .unwrap_or(false)
}

fn record_heap_allocation() {
    let _ = TRACK_HEAP_ALLOCATIONS.try_with(|tracking| {
        if tracking.get() {
            HEAP_ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if heap_allocation_should_fail() {
            return ptr::null_mut();
        }
        record_heap_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if heap_allocation_should_fail() {
            return ptr::null_mut();
        }
        record_heap_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if heap_allocation_should_fail() {
            return ptr::null_mut();
        }
        record_heap_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn fail_next_heap_allocation() {
    FAIL_NEXT_HEAP_ALLOCATION.with(|fail| fail.set(true));
}

fn clear_heap_allocation_failure() {
    FAIL_NEXT_HEAP_ALLOCATION.with(|fail| fail.set(false));
}

fn start_heap_allocation_tracking() {
    HEAP_ALLOCATION_COUNT.with(|count| count.set(0));
    TRACK_HEAP_ALLOCATIONS.with(|tracking| tracking.set(true));
}

fn stop_heap_allocation_tracking() -> usize {
    TRACK_HEAP_ALLOCATIONS.with(|tracking| tracking.set(false));
    HEAP_ALLOCATION_COUNT.with(Cell::get)
}

fn fail_frame_allocation_at(attempt: usize) {
    FRAME_ALLOCATION_COUNT.with(|count| count.set(0));
    FAIL_FRAME_ALLOCATION_AT.with(|fail_at| fail_at.set(Some(attempt)));
}

fn clear_frame_allocation_failure() {
    FAIL_FRAME_ALLOCATION_AT.with(|fail_at| fail_at.set(None));
    FRAME_ALLOCATION_COUNT.with(|count| count.set(0));
}

fn frame_allocation_should_fail() -> bool {
    let attempt = FRAME_ALLOCATION_COUNT.with(|count| {
        let attempt = count.get() + 1;
        count.set(attempt);
        attempt
    });
    FAIL_FRAME_ALLOCATION_AT.with(|fail_at| fail_at.get() == Some(attempt))
}

struct TrackPagingHandler<M: PagingMetaData>(PhantomData<M>);

impl<M: PagingMetaData> PagingHandler for TrackPagingHandler<M> {
    fn alloc_frame() -> Option<PhysAddr> {
        if frame_allocation_should_fail() {
            return None;
        }
        let ptr = unsafe { alloc::alloc(PAGE_LAYOUT) } as usize;
        assert!(
            ptr <= M::PA_MAX_ADDR,
            "allocated frame address exceeds PA_MAX_ADDR"
        );
        ALLOCATED.with_borrow_mut(|it| it.insert(ptr));
        Some(PhysAddr::from_usize(ptr))
    }

    fn alloc_frames(num: usize, align: usize) -> Option<PhysAddr> {
        let layout = pages_layout(num, align);
        let ptr = unsafe { alloc::alloc(layout) } as usize;
        assert!(
            ptr <= M::PA_MAX_ADDR,
            "allocated frame address exceeds PA_MAX_ADDR"
        );
        ALLOCATED.with_borrow_mut(|it| {
            for i in 0..num {
                it.insert(ptr + i * 4096);
            }
        });
        ALIGN.with_borrow_mut(|it| {
            it.insert(ptr, align);
        });
        Some(PhysAddr::from_usize(ptr))
    }

    fn dealloc_frame(paddr: PhysAddr) {
        let ptr = paddr.as_usize();
        ALLOCATED.with_borrow_mut(|it| {
            assert!(it.remove(&ptr), "dealloc a frame that was not allocated");
        });
        unsafe {
            alloc::dealloc(ptr as _, PAGE_LAYOUT);
        }
    }

    fn dealloc_frames(paddr: PhysAddr, num: usize) {
        let ptr = paddr.as_usize();
        ALLOCATED.with_borrow_mut(|it| {
            for i in 0..num {
                let addr = ptr + i * 4096;
                assert!(it.remove(&addr), "dealloc a frame that was not allocated");
            }
        });
        let align = ALIGN.with_borrow_mut(|it| {
            it.remove(&ptr)
                .expect("dealloc frames that were not allocated")
        });
        let layout = pages_layout(num, align);
        unsafe {
            alloc::dealloc(ptr as _, layout);
        }
    }

    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        assert!(paddr.as_usize() > 0);
        VirtAddr::from_usize(paddr.as_usize())
    }
}

struct TablePtrMeta;

impl PagingMetaData for TablePtrMeta {
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 48;
    const VA_MAX_BITS: usize = 48;

    type VirtAddr = VirtAddr;

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}
}

#[derive(Clone, Copy, Debug)]
struct TablePtrPte(u64);

impl TablePtrPte {
    const PRESENT: u64 = 1 << 0;
    const HUGE: u64 = 1 << 1;
    const READ: u64 = 1 << 2;
    const WRITE: u64 = 1 << 3;
    const EXECUTE: u64 = 1 << 4;
    const USER: u64 = 1 << 5;
    const DEVICE: u64 = 1 << 6;
    const UNCACHED: u64 = 1 << 7;
    const PHYS_ADDR_MASK: u64 = !0xfff;

    fn flags_from_bits(bits: u64) -> MappingFlags {
        let mut flags = MappingFlags::empty();
        if bits & Self::READ != 0 {
            flags |= MappingFlags::READ;
        }
        if bits & Self::WRITE != 0 {
            flags |= MappingFlags::WRITE;
        }
        if bits & Self::EXECUTE != 0 {
            flags |= MappingFlags::EXECUTE;
        }
        if bits & Self::USER != 0 {
            flags |= MappingFlags::USER;
        }
        if bits & Self::DEVICE != 0 {
            flags |= MappingFlags::DEVICE;
        }
        if bits & Self::UNCACHED != 0 {
            flags |= MappingFlags::UNCACHED;
        }
        flags
    }

    fn bits_from_flags(flags: MappingFlags) -> u64 {
        let mut bits = 0;
        if flags.contains(MappingFlags::READ) {
            bits |= Self::READ;
        }
        if flags.contains(MappingFlags::WRITE) {
            bits |= Self::WRITE;
        }
        if flags.contains(MappingFlags::EXECUTE) {
            bits |= Self::EXECUTE;
        }
        if flags.contains(MappingFlags::USER) {
            bits |= Self::USER;
        }
        if flags.contains(MappingFlags::DEVICE) {
            bits |= Self::DEVICE;
        }
        if flags.contains(MappingFlags::UNCACHED) {
            bits |= Self::UNCACHED;
        }
        bits
    }
}

impl GenericPTE for TablePtrPte {
    fn new_page(paddr: PhysAddr, flags: MappingFlags, is_huge: bool) -> Self {
        let mut bits = Self::PRESENT | Self::bits_from_flags(flags);
        if is_huge {
            bits |= Self::HUGE;
        }
        Self(bits | ((paddr.as_usize() as u64) & Self::PHYS_ADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        Self((paddr.as_usize() as u64) & Self::PHYS_ADDR_MASK)
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from_usize((self.0 & Self::PHYS_ADDR_MASK) as usize)
    }

    fn flags(&self) -> MappingFlags {
        let flags = Self::flags_from_bits(self.0);
        if CORRUPT_TABLE_PTR_FLAGS.with(Cell::get) {
            flags | MappingFlags::WRITE
        } else {
            flags
        }
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 =
            (self.0 & !Self::PHYS_ADDR_MASK) | ((paddr.as_usize() as u64) & Self::PHYS_ADDR_MASK);
    }

    fn set_flags(&mut self, flags: MappingFlags, is_huge: bool) {
        let mut bits = Self::PRESENT | Self::bits_from_flags(flags);
        if is_huge {
            bits |= Self::HUGE;
        }
        self.0 = (self.0 & Self::PHYS_ADDR_MASK) | bits;
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_unused(&self) -> bool {
        self.0 == 0
    }

    fn is_present(&self) -> bool {
        self.0 & Self::PRESENT != 0
    }

    fn is_huge(&self) -> bool {
        self.0 & Self::HUGE != 0
    }

    fn clear(&mut self) {
        self.0 = 0;
    }
}

#[test]
fn test_map_region_rolls_back_after_late_table_allocation_failure() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());
    clear_frame_allocation_failure();

    let start = 0x1ff000;
    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    fail_frame_allocation_at(4);
    let result = {
        let mut cursor = table.cursor_no_flush();
        cursor.map_region(
            VirtAddr::from_usize(start),
            |vaddr| PhysAddr::from_usize(0x1000_0000 + vaddr.as_usize() - start),
            0x2000,
            MappingFlags::READ | MappingFlags::WRITE,
            false,
        )
    };
    clear_frame_allocation_failure();

    assert_eq!(result, Err(PagingError::NoMemory));
    assert!(matches!(
        table.query(VirtAddr::from_usize(start)),
        Err(PagingError::NotMapped)
    ));
    assert!(matches!(
        table.query(VirtAddr::from_usize(start + 0x1000)),
        Err(PagingError::NotMapped)
    ));

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );
    Ok(())
}

#[test]
fn test_map_region_rolls_back_before_existing_conflict() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());

    let start = 0x400000;
    let conflict_vaddr = VirtAddr::from_usize(start + 0x1000);
    let conflict_paddr = PhysAddr::from_usize(0x3000_0000);
    let conflict_flags = MappingFlags::READ | MappingFlags::USER;
    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            conflict_vaddr,
            conflict_paddr,
            PageSize::Size4K,
            conflict_flags,
        )?;
    }

    let result = {
        let mut cursor = table.cursor_no_flush();
        cursor.map_region(
            VirtAddr::from_usize(start),
            |vaddr| PhysAddr::from_usize(0x2000_0000 + vaddr.as_usize() - start),
            0x2000,
            MappingFlags::READ | MappingFlags::WRITE,
            false,
        )
    };

    assert_eq!(result, Err(PagingError::AlreadyMapped));
    assert!(matches!(
        table.query(VirtAddr::from_usize(start)),
        Err(PagingError::NotMapped)
    ));
    assert_eq!(
        table.query(conflict_vaddr)?,
        (conflict_paddr, conflict_flags, PageSize::Size4K)
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );
    Ok(())
}

#[test]
fn test_map_region_rollback_mismatch_is_fail_stop() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());
    CORRUPT_TABLE_PTR_FLAGS.with(|corrupt| corrupt.set(false));

    let start = 0x800000;
    let conflict_vaddr = VirtAddr::from_usize(start + 0x1000);
    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            conflict_vaddr,
            PhysAddr::from_usize(0x5000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    let rollback = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cursor = table.cursor_no_flush();
        let _ = cursor.map_region(
            VirtAddr::from_usize(start),
            |vaddr| {
                if vaddr == conflict_vaddr {
                    CORRUPT_TABLE_PTR_FLAGS.with(|corrupt| corrupt.set(true));
                }
                PhysAddr::from_usize(0x4000_0000 + vaddr.as_usize() - start)
            },
            0x2000,
            MappingFlags::READ,
            false,
        );
    }));
    CORRUPT_TABLE_PTR_FLAGS.with(|corrupt| corrupt.set(false));

    let panic = rollback.expect_err("rollback mismatch must fail-stop");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("map_region rollback mismatch"));
    assert!(matches!(
        table.query(VirtAddr::from_usize(start)),
        Err(PagingError::NotMapped)
    ));
    assert_eq!(
        table.query(conflict_vaddr)?,
        (
            PhysAddr::from_usize(0x5000_0000),
            MappingFlags::READ,
            PageSize::Size4K,
        )
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );
    Ok(())
}

#[test]
fn test_drain_allocation_failure_preserves_present_leaves() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());
    clear_heap_allocation_failure();

    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x1000),
            PhysAddr::from_usize(0x2000_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x9000),
            PhysAddr::from_usize(0x2000_1000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    fail_next_heap_allocation();
    let result = {
        let mut cursor = table.cursor_no_flush();
        cursor.drain_present_leaves(VirtAddr::from_usize(0), 0x20_000)
    };
    clear_heap_allocation_failure();

    assert_eq!(result, Err(PagingError::NoMemory));
    assert_eq!(
        table.query(VirtAddr::from_usize(0x1000))?,
        (
            PhysAddr::from_usize(0x2000_0000),
            MappingFlags::READ | MappingFlags::WRITE,
            PageSize::Size4K,
        )
    );
    assert_eq!(
        table.query(VirtAddr::from_usize(0x9000))?,
        (
            PhysAddr::from_usize(0x2000_1000),
            MappingFlags::READ,
            PageSize::Size4K,
        )
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );
    Ok(())
}

#[test]
fn test_collect_and_drain_use_one_exact_leaf_allocation() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());

    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x1000),
            PhysAddr::from_usize(0x2000_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x9000),
            PhysAddr::from_usize(0x2000_1000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }
    let expected = [
        (
            VirtAddr::from_usize(0x1000),
            PhysAddr::from_usize(0x2000_0000),
            MappingFlags::READ | MappingFlags::WRITE,
            PageSize::Size4K,
        ),
        (
            VirtAddr::from_usize(0x9000),
            PhysAddr::from_usize(0x2000_1000),
            MappingFlags::READ,
            PageSize::Size4K,
        ),
    ];

    start_heap_allocation_tracking();
    let collected = table.collect_present_leaves(VirtAddr::from_usize(0), 0x20_000);
    let collect_allocations = stop_heap_allocation_tracking();
    let collected = collected?;
    assert_eq!(collect_allocations, 1);
    assert_eq!(collected.as_slice(), &expected);
    assert!(collected.capacity() >= collected.len());

    start_heap_allocation_tracking();
    let drained = {
        let mut cursor = table.cursor_no_flush();
        cursor.drain_present_leaves(VirtAddr::from_usize(0), 0x20_000)
    };
    let drain_allocations = stop_heap_allocation_tracking();
    let drained = drained?;
    assert_eq!(drain_allocations, 1);
    assert_eq!(drained.as_slice(), &expected);
    assert!(drained.capacity() >= drained.len());

    drop(collected);
    drop(drained);
    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );
    Ok(())
}

fn run_test_for<M: PagingMetaData<VirtAddr = VirtAddr>, PTE: GenericPTE>() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| {
        it.clear();
    });

    let vaddr_mask = ((1u64 << M::VA_MAX_BITS) - 1) & !0xfff;

    let mut table = PageTable64::<M, PTE, TrackPagingHandler<M>>::try_new().unwrap();
    let mut pages = HashSet::new();
    let mut rng = SmallRng::seed_from_u64(1234);

    for _ in 0..2048 {
        let mut cursor = table.cursor_no_flush();
        if rng.random_ratio(3, 4) || pages.is_empty() {
            // insert a mapping
            let addr = loop {
                let addr = rng.random::<u64>() & vaddr_mask;
                if pages.insert(addr) {
                    break addr;
                }
            };
            cursor.map(
                VirtAddr::from_usize(addr as usize),
                PhysAddr::from_usize((rng.random::<u64>() & vaddr_mask) as usize),
                PageSize::Size4K,
                MappingFlags::READ | MappingFlags::WRITE,
            )?;
        } else {
            // remove a mapping
            let addr = *pages.iter().next().unwrap();
            cursor.unmap(VirtAddr::from_usize(addr as usize))?;
            pages.remove(&addr);
        }
    }

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[cfg(target_pointer_width = "32")]
fn run_test_for_32bit<M: PagingMetaData<VirtAddr = VirtAddr>, PTE: GenericPTE>() -> PagingResult<()>
{
    use page_table_multiarch::PageTable32;
    ALLOCATED.with_borrow_mut(|it| {
        it.clear();
    });

    let vaddr_mask = ((1u64 << M::VA_MAX_BITS) - 1) & !0xfff;

    let mut table = PageTable32::<M, PTE, TrackPagingHandler<M>>::try_new().unwrap();
    let mut pages = HashSet::new();
    let mut rng = SmallRng::seed_from_u64(5678);
    for _ in 0..512 {
        // Fewer iterations for 32-bit to avoid address space exhaustion
        if rng.random_ratio(3, 4) || pages.is_empty() {
            // insert a mapping
            let addr = loop {
                let addr = rng.random::<u32>() & (vaddr_mask as u32);
                if pages.insert(addr as u64) {
                    break addr as u64;
                }
            };
            table
                .map(
                    VirtAddr::from_usize(addr as usize),
                    PhysAddr::from_usize((rng.random::<u32>() & (vaddr_mask as u32)) as usize),
                    PageSize::Size4K,
                    MappingFlags::READ | MappingFlags::WRITE,
                )?
                .ignore();
        } else {
            // remove a mapping
            let addr = *pages.iter().next().unwrap();
            table.unmap(VirtAddr::from_usize(addr as usize))?.2.ignore();
            pages.remove(&addr);
        }
    }

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[test]
#[cfg(any(target_arch = "arm", docsrs))]
#[cfg(target_pointer_width = "32")]
fn test_dealloc_arm32() -> PagingResult<()> {
    run_test_for_32bit::<
        page_table_multiarch::arm::A32PagingMetaData,
        page_table_entry::arm::A32PTE,
    >()?;
    Ok(())
}

#[test]
#[cfg(any(target_arch = "x86_64", docsrs))]
fn test_dealloc_x86() -> PagingResult<()> {
    run_test_for::<
        page_table_multiarch::x86_64::X64PagingMetaData,
        page_table_entry::x86_64::X64PTE,
    >()?;
    Ok(())
}

#[test]
#[cfg(any(target_arch = "x86_64", docsrs))]
fn test_collect_present_leaves_x86() -> PagingResult<()> {
    type Meta = page_table_multiarch::x86_64::X64PagingMetaData;
    type Pte = page_table_entry::x86_64::X64PTE;

    ALLOCATED.with_borrow_mut(|it| it.clear());

    let mut table = PageTable64::<Meta, Pte, TrackPagingHandler<Meta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x1000),
            PhysAddr::from_usize(0x2000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x9000),
            PhysAddr::from_usize(0xa000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    let sparse = table.collect_present_leaves(VirtAddr::from_usize(0), 0x20_000)?;
    assert_eq!(
        sparse,
        vec![
            (
                VirtAddr::from_usize(0x1000),
                PhysAddr::from_usize(0x2000),
                MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
            ),
            (
                VirtAddr::from_usize(0x9000),
                PhysAddr::from_usize(0xa000),
                MappingFlags::READ,
                PageSize::Size4K,
            ),
        ]
    );

    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x20_0000),
            PhysAddr::from_usize(0x40_0000),
            PageSize::Size2M,
            MappingFlags::READ,
        )?;
    }

    assert!(matches!(
        table.collect_present_leaves(VirtAddr::from_usize(0x20_1000), 0x1000),
        Err(PagingError::NotAligned)
    ));

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[test]
fn test_collect_present_leaves_nonpresent_tables() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());

    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x3fff_9000),
            PhysAddr::from_usize(0x2000_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x3fff_a000),
            PhysAddr::from_usize(0x2000_1000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    let sparse = table.collect_present_leaves(VirtAddr::from_usize(0x3fff_9000), 0x3000)?;
    assert_eq!(
        sparse,
        vec![
            (
                VirtAddr::from_usize(0x3fff_9000),
                PhysAddr::from_usize(0x2000_0000),
                MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
            ),
            (
                VirtAddr::from_usize(0x3fff_a000),
                PhysAddr::from_usize(0x2000_1000),
                MappingFlags::READ,
                PageSize::Size4K,
            ),
        ]
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[test]
#[cfg(any(target_arch = "x86_64", docsrs))]
fn test_drain_present_leaves_x86() -> PagingResult<()> {
    type Meta = page_table_multiarch::x86_64::X64PagingMetaData;
    type Pte = page_table_entry::x86_64::X64PTE;

    ALLOCATED.with_borrow_mut(|it| it.clear());

    let mut table = PageTable64::<Meta, Pte, TrackPagingHandler<Meta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x1000),
            PhysAddr::from_usize(0x2000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x9000),
            PhysAddr::from_usize(0xa000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    let drained = {
        let mut cursor = table.cursor_no_flush();
        cursor.drain_present_leaves(VirtAddr::from_usize(0), 0x20_000)?
    };
    assert_eq!(
        drained,
        vec![
            (
                VirtAddr::from_usize(0x1000),
                PhysAddr::from_usize(0x2000),
                MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
            ),
            (
                VirtAddr::from_usize(0x9000),
                PhysAddr::from_usize(0xa000),
                MappingFlags::READ,
                PageSize::Size4K,
            ),
        ]
    );
    assert_eq!(
        table.collect_present_leaves(VirtAddr::from_usize(0), 0x20_000)?,
        Vec::new()
    );

    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x20_0000),
            PhysAddr::from_usize(0x40_0000),
            PageSize::Size2M,
            MappingFlags::READ,
        )?;
    }
    assert!(matches!(
        {
            let mut cursor = table.cursor_no_flush();
            cursor.drain_present_leaves(VirtAddr::from_usize(0x20_1000), 0x1000)
        },
        Err(PagingError::NotAligned)
    ));
    assert_eq!(
        table.collect_present_leaves(VirtAddr::from_usize(0x20_0000), 0x20_0000)?,
        vec![(
            VirtAddr::from_usize(0x20_0000),
            PhysAddr::from_usize(0x40_0000),
            MappingFlags::READ,
            PageSize::Size2M,
        )]
    );

    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x10_0000),
            PhysAddr::from_usize(0x50_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
    }
    assert!(matches!(
        {
            let mut cursor = table.cursor_no_flush();
            cursor.drain_present_leaves(VirtAddr::from_usize(0x10_0000), 0x101000)
        },
        Err(PagingError::NotAligned)
    ));
    assert_eq!(
        table.collect_present_leaves(VirtAddr::from_usize(0x10_0000), 0x30_0000)?,
        vec![
            (
                VirtAddr::from_usize(0x10_0000),
                PhysAddr::from_usize(0x50_0000),
                MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
            ),
            (
                VirtAddr::from_usize(0x20_0000),
                PhysAddr::from_usize(0x40_0000),
                MappingFlags::READ,
                PageSize::Size2M,
            ),
        ]
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[test]
fn test_drain_present_leaves_nonpresent_tables() -> PagingResult<()> {
    ALLOCATED.with_borrow_mut(|it| it.clear());

    let mut table =
        PageTable64::<TablePtrMeta, TablePtrPte, TrackPagingHandler<TablePtrMeta>>::try_new()?;
    {
        let mut cursor = table.cursor_no_flush();
        cursor.map(
            VirtAddr::from_usize(0x3fff_9000),
            PhysAddr::from_usize(0x2000_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        cursor.map(
            VirtAddr::from_usize(0x3fff_a000),
            PhysAddr::from_usize(0x2000_1000),
            PageSize::Size4K,
            MappingFlags::READ,
        )?;
    }

    let drained = {
        let mut cursor = table.cursor_no_flush();
        cursor.drain_present_leaves(VirtAddr::from_usize(0x3fff_9000), 0x3000)?
    };
    assert_eq!(
        drained,
        vec![
            (
                VirtAddr::from_usize(0x3fff_9000),
                PhysAddr::from_usize(0x2000_0000),
                MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
            ),
            (
                VirtAddr::from_usize(0x3fff_a000),
                PhysAddr::from_usize(0x2000_1000),
                MappingFlags::READ,
                PageSize::Size4K,
            ),
        ]
    );
    assert_eq!(
        table.collect_present_leaves(VirtAddr::from_usize(0x3fff_9000), 0x3000)?,
        vec![]
    );

    drop(table);
    assert_eq!(
        ALLOCATED.with_borrow(|it| it.len()),
        0,
        "Some frames were not deallocated"
    );

    Ok(())
}

#[test]
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64", docsrs))]
fn test_dealloc_riscv() -> PagingResult<()> {
    run_test_for::<
        page_table_multiarch::riscv::Sv39MetaData<VirtAddr>,
        page_table_entry::riscv::Rv64PTE,
    >()?;
    run_test_for::<
        page_table_multiarch::riscv::Sv48MetaData<VirtAddr>,
        page_table_entry::riscv::Rv64PTE,
    >()?;
    Ok(())
}

#[test]
#[cfg(any(target_arch = "aarch64", docsrs))]
fn test_dealloc_aarch64() -> PagingResult<()> {
    run_test_for::<
        page_table_multiarch::aarch64::A64PagingMetaData,
        page_table_entry::aarch64::A64PTE,
    >()?;
    Ok(())
}

#[test]
#[cfg(any(target_arch = "loongarch64", docsrs))]
fn test_dealloc_loongarch64() -> PagingResult<()> {
    run_test_for::<
        page_table_multiarch::loongarch64::LA64MetaData,
        page_table_entry::loongarch64::LA64PTE,
    >()?;
    Ok(())
}
