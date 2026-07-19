use std::{
    alloc::{self, GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    marker::PhantomData,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender},
    },
    time::Duration,
};

use memory_addr::{MemoryAddr, PhysAddr, VirtAddr};
use page_table_entry::{GenericPTE, MappingFlags};
use page_table_multiarch::{
    PageSize, PageTable64, PagingError, PagingHandler, PagingMetaData, PagingResult,
    PrepareTableFramesError, PreparedMapError, PreparedPageTableFrames,
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
    static FRAME_DEALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static CORRUPT_TABLE_PTR_FLAGS: Cell<bool> = const { Cell::new(false) };
    static NEW_TABLE_HOOK: RefCell<Option<Arc<NewTableHook>>> = const { RefCell::new(None) };
    static NEW_PAGE_HOOK: RefCell<Option<Arc<NewTableHook>>> = const { RefCell::new(None) };
}

struct NewTableHook {
    trigger_at: usize,
    calls: AtomicUsize,
    paddrs: [AtomicUsize; 3],
    ready: SyncSender<()>,
    resume: Mutex<Receiver<()>>,
}

impl NewTableHook {
    fn observe(&self, paddr: PhysAddr) {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(slot) = self.paddrs.get(call) {
            slot.store(paddr.as_usize(), Ordering::SeqCst);
        }
        if call + 1 == self.trigger_at {
            self.ready
                .send(())
                .expect("prepared publish observer disappeared");
            self.resume
                .lock()
                .expect("prepared publish resume lock poisoned")
                .recv_timeout(Duration::from_secs(5))
                .expect("prepared publish observer did not resume commit");
        }
    }
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

fn frame_allocation_count() -> usize {
    FRAME_ALLOCATION_COUNT.with(Cell::get)
}

fn frame_deallocation_count() -> usize {
    FRAME_DEALLOCATION_COUNT.with(Cell::get)
}

fn reset_frame_activity_counts() {
    FRAME_ALLOCATION_COUNT.with(|count| count.set(0));
    FRAME_DEALLOCATION_COUNT.with(|count| count.set(0));
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
        FRAME_DEALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
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

struct PreparedMeta3;

impl PagingMetaData for PreparedMeta3 {
    const LEVELS: usize = 3;
    const PA_MAX_BITS: usize = 48;
    const VA_MAX_BITS: usize = 39;

    type VirtAddr = VirtAddr;

    fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}
}

struct PreparedMeta4;

impl PagingMetaData for PreparedMeta4 {
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
        NEW_PAGE_HOOK.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook.observe(paddr);
            }
        });
        let mut bits = Self::PRESENT | Self::bits_from_flags(flags);
        if is_huge {
            bits |= Self::HUGE;
        }
        Self(bits | ((paddr.as_usize() as u64) & Self::PHYS_ADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        NEW_TABLE_HOOK.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook.observe(paddr);
            }
        });
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

type PreparedTable<M> = PageTable64<M, TablePtrPte, TrackPagingHandler<M>>;
type PreparedFrames<M> = PreparedPageTableFrames<TrackPagingHandler<M>>;

fn reset_prepared_test_state() {
    ALLOCATED.with_borrow_mut(|allocated| allocated.clear());
    clear_frame_allocation_failure();
    reset_frame_activity_counts();
    NEW_TABLE_HOOK.with_borrow_mut(|hook| *hook = None);
    NEW_PAGE_HOOK.with_borrow_mut(|hook| *hook = None);
}

fn assert_prepared_test_frames_reclaimed() {
    assert_eq!(
        ALLOCATED.with_borrow(|allocated| allocated.len()),
        0,
        "prepared page-table test leaked frames"
    );
}

fn run_prepared_path_matrix<M>()
where
    M: PagingMetaData<VirtAddr = VirtAddr>,
{
    const TARGET_PAGE: usize = 0x1234_5000;
    const TARGET_OFFSET: usize = 0x321;
    const FLAGS: MappingFlags = MappingFlags::READ.union(MappingFlags::WRITE);

    // A completely absent 4K path consumes every intermediate level below
    // the root and performs no allocation or deallocation during commit.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(TARGET_PAGE + TARGET_OFFSET);
    let target = PhysAddr::from_usize(0x4000_0123);
    let required = M::LEVELS - 1;
    assert_eq!(
        table
            .required_prepared_frames(vaddr, PageSize::Size4K)
            .unwrap(),
        required
    );
    let mut prepared = PreparedFrames::<M>::try_new(required).unwrap();
    let allocations_before = frame_allocation_count();
    let deallocations_before = frame_deallocation_count();
    start_heap_allocation_tracking();
    let commit = table
        .cursor_no_flush()
        .map_prepared(vaddr, target, PageSize::Size4K, FLAGS, &mut prepared)
        .unwrap();
    let heap_allocations = stop_heap_allocation_tracking();
    assert_eq!(heap_allocations, 0);
    assert_eq!(frame_allocation_count(), allocations_before);
    assert_eq!(frame_deallocation_count(), deallocations_before);
    assert_eq!(commit.consumed_frames(), required);
    assert!(prepared.is_empty());
    assert_eq!(
        table.query(vaddr).unwrap(),
        (
            PhysAddr::from_usize(0x4000_0000 + TARGET_OFFSET),
            FLAGS,
            PageSize::Size4K,
        )
    );
    let deallocations_before_reservation_drop = frame_deallocation_count();
    drop(prepared);
    assert_eq!(
        frame_deallocation_count(),
        deallocations_before_reservation_drop,
        "reservation Drop reclaimed a published table frame"
    );
    drop(table);
    assert_prepared_test_frames_reclaimed();

    // One live top-level branch plus an absent lower branch exercises a
    // partially missing path on both three- and four-level metadata.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let level_one_shift = 12 + (M::LEVELS - 2) * 9;
    let sibling = VirtAddr::from_usize(TARGET_PAGE ^ (1 << level_one_shift));
    table
        .cursor_no_flush()
        .map(
            sibling,
            PhysAddr::from_usize(0x5000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
    let vaddr = VirtAddr::from_usize(TARGET_PAGE);
    let required = M::LEVELS - 2;
    assert_eq!(
        table
            .required_prepared_frames(vaddr, PageSize::Size4K)
            .unwrap(),
        required
    );
    let mut prepared = PreparedFrames::<M>::try_max().unwrap();
    let commit = table
        .cursor_no_flush()
        .map_prepared(
            vaddr,
            PhysAddr::from_usize(0x5100_0000),
            PageSize::Size4K,
            FLAGS,
            &mut prepared,
        )
        .unwrap();
    assert_eq!(commit.consumed_frames(), required);
    assert_eq!(prepared.len(), 3 - required);
    assert_eq!(
        table.query(vaddr).unwrap(),
        (PhysAddr::from_usize(0x5100_0000), FLAGS, PageSize::Size4K,)
    );
    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();

    // Unmapping leaves intermediate tables in place, so remapping the same
    // leaf consumes no reserved frame and preserves the whole reservation.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(TARGET_PAGE);
    table
        .cursor_no_flush()
        .map(
            vaddr,
            PhysAddr::from_usize(0x5200_0000),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
    table.cursor_no_flush().unmap(vaddr).unwrap();
    assert_eq!(
        table
            .required_prepared_frames(vaddr, PageSize::Size4K)
            .unwrap(),
        0
    );
    let mut prepared = PreparedFrames::<M>::try_max().unwrap();
    let commit = table
        .cursor_no_flush()
        .map_prepared(
            vaddr,
            PhysAddr::from_usize(0x5300_0000),
            PageSize::Size4K,
            FLAGS,
            &mut prepared,
        )
        .unwrap();
    assert_eq!(commit.consumed_frames(), 0);
    assert_eq!(prepared.len(), 3);
    drop(prepared);
    assert_eq!(
        table.query(vaddr).unwrap(),
        (PhysAddr::from_usize(0x5300_0000), FLAGS, PageSize::Size4K,)
    );
    drop(table);
    assert_prepared_test_frames_reclaimed();
}

#[test]
fn test_prepared_paths_meta3() {
    run_prepared_path_matrix::<PreparedMeta3>();
}

#[test]
fn test_prepared_paths_meta4() {
    run_prepared_path_matrix::<PreparedMeta4>();
}

fn run_prepared_error_matrix<M>()
where
    M: PagingMetaData<VirtAddr = VirtAddr>,
{
    const VADDR: usize = 0x2000_0000;

    // A conflicting live leaf is detected before publication and retains the
    // reservation in full.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(VADDR);
    let old_target = PhysAddr::from_usize(0x6000_0000);
    table
        .cursor_no_flush()
        .map(vaddr, old_target, PageSize::Size4K, MappingFlags::READ)
        .unwrap();
    let mut prepared = PreparedFrames::<M>::try_max().unwrap();
    let allocations_before = frame_allocation_count();
    let deallocations_before = frame_deallocation_count();
    assert_eq!(
        table.cursor_no_flush().map_prepared(
            vaddr,
            PhysAddr::from_usize(0x6100_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
            &mut prepared,
        ),
        Err(PreparedMapError::Paging(PagingError::AlreadyMapped))
    );
    assert_eq!(prepared.len(), 3);
    assert_eq!(frame_allocation_count(), allocations_before);
    assert_eq!(frame_deallocation_count(), deallocations_before);
    assert_eq!(
        table.query(vaddr).unwrap(),
        (old_target, MappingFlags::READ, PageSize::Size4K)
    );
    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();

    // A huge mapping blocks a lower-level prepared leaf without modifying the
    // huge entry or consuming the reservation.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let huge_base = VirtAddr::from_usize(0x4000_0000);
    let huge_target = PhysAddr::from_usize(0x8000_0000);
    table
        .cursor_no_flush()
        .map(huge_base, huge_target, PageSize::Size1G, MappingFlags::READ)
        .unwrap();
    let lower = VirtAddr::from_usize(0x4000_1000);
    assert_eq!(
        table.required_prepared_frames(lower, PageSize::Size4K),
        Err(PagingError::MappedToHugePage)
    );
    let mut prepared = PreparedFrames::<M>::try_max().unwrap();
    assert_eq!(
        table.cursor_no_flush().map_prepared(
            lower,
            PhysAddr::from_usize(0x9000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
            &mut prepared,
        ),
        Err(PreparedMapError::Paging(PagingError::MappedToHugePage))
    );
    assert_eq!(prepared.len(), 3);
    assert_eq!(
        table.query(lower).unwrap(),
        (
            huge_target.add(0x1000),
            MappingFlags::READ,
            PageSize::Size1G,
        )
    );
    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();

    // An undersized reservation reports an exact retry requirement and leaves
    // both the live tree and reservation untouched.
    reset_prepared_test_state();
    let mut table = PreparedTable::<M>::try_new().unwrap();
    let required = M::LEVELS - 1;
    let available = required - 1;
    let mut prepared = PreparedFrames::<M>::try_new(available).unwrap();
    assert_eq!(
        table.cursor_no_flush().map_prepared(
            vaddr,
            PhysAddr::from_usize(0xa000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
            &mut prepared,
        ),
        Err(PreparedMapError::NeedMore {
            required,
            available,
        })
    );
    assert_eq!(prepared.len(), available);
    assert_eq!(table.query(vaddr), Err(PagingError::NotMapped));
    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();
}

#[test]
fn test_prepared_errors_meta3() {
    run_prepared_error_matrix::<PreparedMeta3>();
}

#[test]
fn test_prepared_errors_meta4() {
    run_prepared_error_matrix::<PreparedMeta4>();
}

#[test]
fn test_prepared_reservation_allocation_failure_is_atomic() {
    reset_prepared_test_state();
    let table = PreparedTable::<PreparedMeta4>::try_new().unwrap();
    assert_eq!(ALLOCATED.with_borrow(|allocated| allocated.len()), 1);

    fail_frame_allocation_at(2);
    assert!(matches!(
        PreparedFrames::<PreparedMeta4>::try_new(3),
        Err(PrepareTableFramesError::NoMemory)
    ));
    clear_frame_allocation_failure();
    assert_eq!(
        ALLOCATED.with_borrow(|allocated| allocated.len()),
        1,
        "partial reservation survived allocation failure"
    );
    assert_eq!(
        PreparedFrames::<PreparedMeta4>::try_new(4).unwrap_err(),
        PrepareTableFramesError::TooMany {
            requested: 4,
            maximum: 3,
        }
    );

    drop(table);
    assert_prepared_test_frames_reclaimed();
}

#[test]
fn test_prepared_commit_drops_only_unused_frames_after_unlock() {
    reset_prepared_test_state();
    let mut table = PreparedTable::<PreparedMeta4>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(0x3456_7000);
    // Differing at the final intermediate index creates exactly one missing
    // table frame for `vaddr`.
    table
        .cursor_no_flush()
        .map(
            VirtAddr::from_usize(vaddr.as_usize() ^ (1 << 21)),
            PhysAddr::from_usize(0xb000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
    assert_eq!(
        table
            .required_prepared_frames(vaddr, PageSize::Size4K)
            .unwrap(),
        1
    );

    let mut prepared = PreparedFrames::<PreparedMeta4>::try_max().unwrap();
    let commit = table
        .cursor_no_flush()
        .map_prepared(
            vaddr,
            PhysAddr::from_usize(0xb100_0000),
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
            &mut prepared,
        )
        .unwrap();
    assert_eq!(commit.consumed_frames(), 1);
    assert_eq!(prepared.len(), 2);
    let allocated_before_drop = ALLOCATED.with_borrow(|allocated| allocated.len());
    let deallocations_before_drop = frame_deallocation_count();
    drop(prepared);
    assert_eq!(
        ALLOCATED.with_borrow(|allocated| allocated.len()),
        allocated_before_drop - 2
    );
    assert_eq!(frame_deallocation_count(), deallocations_before_drop + 2);
    assert_eq!(
        table.query(vaddr).unwrap(),
        (
            PhysAddr::from_usize(0xb100_0000),
            MappingFlags::READ | MappingFlags::WRITE,
            PageSize::Size4K,
        )
    );
    drop(table);
    assert_prepared_test_frames_reclaimed();
}

fn run_prepared_huge_matrix<M>()
where
    M: PagingMetaData<VirtAddr = VirtAddr>,
{
    for (vaddr, target, page_size, required) in [
        (
            0x4000_1234usize,
            0xc123_4567usize,
            PageSize::Size1G,
            M::LEVELS - 3,
        ),
        (
            0x200_1234usize,
            0xd023_4567usize,
            PageSize::Size2M,
            M::LEVELS - 2,
        ),
    ] {
        reset_prepared_test_state();
        let mut table = PreparedTable::<M>::try_new().unwrap();
        let vaddr = VirtAddr::from_usize(vaddr);
        let target = PhysAddr::from_usize(target);
        assert_eq!(
            table.required_prepared_frames(vaddr, page_size).unwrap(),
            required
        );
        let mut prepared = PreparedFrames::<M>::try_max().unwrap();
        let commit = table
            .cursor_no_flush()
            .map_prepared(vaddr, target, page_size, MappingFlags::READ, &mut prepared)
            .unwrap();
        assert_eq!(commit.consumed_frames(), required);
        assert_eq!(prepared.len(), 3 - required);
        assert_eq!(
            table.query(vaddr).unwrap(),
            (
                target
                    .align_down(page_size)
                    .add(page_size.align_offset(vaddr.as_usize())),
                MappingFlags::READ,
                page_size,
            )
        );
        drop(prepared);
        drop(table);
        assert_prepared_test_frames_reclaimed();
    }
}

#[test]
fn test_prepared_huge_paths_meta3() {
    run_prepared_huge_matrix::<PreparedMeta3>();
}

#[test]
fn test_prepared_huge_paths_meta4() {
    run_prepared_huge_matrix::<PreparedMeta4>();
}

#[test]
fn test_prepared_subtree_is_invisible_until_single_parent_publish() {
    reset_prepared_test_state();
    let mut table = PreparedTable::<PreparedMeta4>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(0x1234_5000);
    let target = PhysAddr::from_usize(0xe000_0000);
    let mut prepared = PreparedFrames::<PreparedMeta4>::try_max().unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let hook = Arc::new(NewTableHook {
        trigger_at: 3,
        calls: AtomicUsize::new(0),
        paddrs: core::array::from_fn(|_| AtomicUsize::new(0)),
        ready: ready_tx,
        resume: Mutex::new(resume_rx),
    });
    NEW_TABLE_HOOK.with_borrow_mut(|slot| *slot = Some(Arc::clone(&hook)));

    let root = table.root_paddr().as_usize();
    let vaddr_usize = vaddr.as_usize();
    let observer_hook = Arc::clone(&hook);
    let observer = std::thread::spawn(move || {
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("prepared commit did not reach its final publication");

        let table_at =
            |paddr: usize| unsafe { core::slice::from_raw_parts(paddr as *const TablePtrPte, 512) };
        let index_at = |level: usize| {
            let shift = 12 + (PreparedMeta4::LEVELS - 1 - level) * 9;
            (vaddr_usize >> shift) & 511
        };

        let root_entry = table_at(root)[index_at(0)];
        assert!(
            root_entry.is_unused(),
            "prepared subtree became reachable before final parent publish"
        );

        // `new_table` calls 0 and 1 built the two offline links; call 2
        // constructed the still-unpublished root pointer.
        let first = observer_hook.paddrs[2].load(Ordering::SeqCst);
        let second = observer_hook.paddrs[0].load(Ordering::SeqCst);
        let third = observer_hook.paddrs[1].load(Ordering::SeqCst);
        let first_entry = table_at(first)[index_at(1)];
        let second_entry = table_at(second)[index_at(2)];
        let leaf = table_at(third)[index_at(3)];
        assert_eq!(first_entry.paddr().as_usize(), second);
        assert_eq!(second_entry.paddr().as_usize(), third);
        assert!(leaf.is_present());
        assert_eq!(leaf.paddr(), target);

        resume_tx
            .send(())
            .expect("prepared commit disappeared before publication");
    });

    let commit = table
        .cursor_no_flush()
        .map_prepared(
            vaddr,
            target,
            PageSize::Size4K,
            MappingFlags::READ,
            &mut prepared,
        )
        .unwrap();
    NEW_TABLE_HOOK.with_borrow_mut(|slot| *slot = None);
    observer
        .join()
        .expect("prepared publication observer failed");

    assert_eq!(hook.calls.load(Ordering::SeqCst), 3);
    assert_eq!(commit.consumed_frames(), 3);
    assert!(prepared.is_empty());
    assert_eq!(
        table.query(vaddr).unwrap(),
        (target, MappingFlags::READ, PageSize::Size4K)
    );

    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();
}

#[test]
fn test_prepared_existing_path_leaf_is_release_published_once() {
    reset_prepared_test_state();
    let mut table = PreparedTable::<PreparedMeta4>::try_new().unwrap();
    let vaddr = VirtAddr::from_usize(0x2345_6000);

    // Seed and clear a leaf so every intermediate table already exists.
    table
        .cursor_no_flush()
        .map(
            vaddr,
            PhysAddr::from_usize(0xf000_0000),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
    table.cursor_no_flush().unmap(vaddr).unwrap();
    assert_eq!(
        table
            .required_prepared_frames(vaddr, PageSize::Size4K)
            .unwrap(),
        0
    );

    let target = TrackPagingHandler::<PreparedMeta4>::alloc_frame().unwrap();
    const DATA_MARKER: u64 = 0xfeed_cafe_1234_5678;
    unsafe {
        *(TrackPagingHandler::<PreparedMeta4>::phys_to_virt(target).as_mut_ptr() as *mut u64) =
            DATA_MARKER;
    }
    let mut prepared = PreparedFrames::<PreparedMeta4>::try_new(0).unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let hook = Arc::new(NewTableHook {
        trigger_at: 1,
        calls: AtomicUsize::new(0),
        paddrs: core::array::from_fn(|_| AtomicUsize::new(0)),
        ready: ready_tx,
        resume: Mutex::new(resume_rx),
    });
    NEW_PAGE_HOOK.with_borrow_mut(|slot| *slot = Some(Arc::clone(&hook)));

    let root = table.root_paddr().as_usize();
    let vaddr_usize = vaddr.as_usize();
    let observer = std::thread::spawn(move || {
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("prepared leaf commit did not reach publication");
        let table_at =
            |paddr: usize| unsafe { core::slice::from_raw_parts(paddr as *const TablePtrPte, 512) };
        let index_at = |level: usize| {
            let shift = 12 + (PreparedMeta4::LEVELS - 1 - level) * 9;
            (vaddr_usize >> shift) & 511
        };

        let mut table_paddr = root;
        for level in 0..PreparedMeta4::LEVELS {
            let entry = table_at(table_paddr)[index_at(level)];
            if level == PreparedMeta4::LEVELS - 1 {
                assert!(
                    entry.is_unused(),
                    "existing-path leaf became visible before release publication"
                );
            } else {
                assert!(!entry.is_unused());
                table_paddr = entry.paddr().as_usize();
            }
        }
        assert_eq!(
            unsafe { *(target.as_usize() as *const u64) },
            DATA_MARKER,
            "prepared data was not initialized before leaf publication"
        );
        resume_tx
            .send(())
            .expect("prepared leaf commit disappeared before publication");
    });

    let commit = table
        .cursor_no_flush()
        .map_prepared(
            vaddr,
            target,
            PageSize::Size4K,
            MappingFlags::READ | MappingFlags::WRITE,
            &mut prepared,
        )
        .unwrap();
    NEW_PAGE_HOOK.with_borrow_mut(|slot| *slot = None);
    observer.join().expect("prepared leaf observer failed");

    assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
    assert_eq!(commit.consumed_frames(), 0);
    assert!(prepared.is_empty());
    assert_eq!(
        table.query(vaddr).unwrap(),
        (
            target,
            MappingFlags::READ | MappingFlags::WRITE,
            PageSize::Size4K,
        )
    );

    table.cursor_no_flush().unmap(vaddr).unwrap();
    TrackPagingHandler::<PreparedMeta4>::dealloc_frame(target);
    drop(prepared);
    drop(table);
    assert_prepared_test_frames_reclaimed();
}
