//! Default memory allocator implementation using axallocator crate.
//!
//! This is the standard ArceOS memory allocator implementation that uses
//! the axallocator crate with support for different byte allocator algorithms
//! (TLSF, slab, buddy) and page allocation.

#![allow(dead_code)]

use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::NonNull,
};

use axallocator::{AllocResult, BaseAllocator, BitmapPageAllocator, ByteAllocator, PageAllocator};
use kspin::SpinNoIrq;

use super::{UsageKind, Usages, PageAccountingHooks, page_accounting::AccountingSlot};

/// The global allocator instance for standard mode.
#[cfg_attr(all(target_os = "none", not(test)), global_allocator)]
static GLOBAL_ALLOCATOR: GlobalAllocator = GlobalAllocator::new();

const PAGE_SIZE: usize = 0x1000;
const MIN_HEAP_SIZE: usize = 0x8000; // 32 K

cfg_if::cfg_if! {
    if #[cfg(feature = "slab")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = axallocator::SlabByteAllocator;
    } else if #[cfg(feature = "buddy")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = axallocator::BuddyByteAllocator;
    } else if #[cfg(feature = "tlsf")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = axallocator::TlsfByteAllocator;
    }
}

/// The global allocator used by ArceOS.
///
/// It combines a [`ByteAllocator`] and a [`PageAllocator`] into a simple
/// two-level allocator: firstly tries allocate from the byte allocator, if
/// there is no memory, asks the page allocator for more memory and adds it to
/// the byte allocator.
pub struct GlobalAllocator {
    balloc: SpinNoIrq<DefaultByteAllocator>,
    #[cfg(not(feature = "level-1"))]
    palloc: SpinNoIrq<BitmapPageAllocator<PAGE_SIZE>>,
    usages: SpinNoIrq<Usages>,
    accounting: AccountingSlot,
}

impl Default for GlobalAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalAllocator {
    /// Creates an empty [`GlobalAllocator`].
    pub const fn new() -> Self {
        Self {
            balloc: SpinNoIrq::new(DefaultByteAllocator::new()),
            #[cfg(not(feature = "level-1"))]
            palloc: SpinNoIrq::new(BitmapPageAllocator::new()),
            usages: SpinNoIrq::new(Usages::new()),
            accounting: AccountingSlot::new(),
        }
    }

    /// Installs immutable lifetime hooks before userspace allocations start.
    /// The policy lives above this generic allocator; it may deny a physical
    /// reservation without publishing a frame or consuming allocator usage.
    pub fn install_page_accounting(&self, hooks: &'static PageAccountingHooks) -> bool {
        self.accounting.install(hooks)
    }

    fn finish_page_allocation(&self, addr: usize, num_pages: usize, kind: UsageKind) -> AllocResult<usize> {
        if !self.accounting.admit(addr, num_pages, kind) {
            self.return_raw_pages(addr, num_pages);
            return Err(axallocator::AllocError::NoMemory);
        }
        if cfg!(feature = "level-1") || !matches!(kind, UsageKind::RustHeap) {
            self.usages.lock().alloc(kind, num_pages * PAGE_SIZE);
        }
        Ok(addr)
    }

    fn return_raw_pages(&self, pos: usize, num_pages: usize) {
        #[cfg(feature = "level-1")]
        {
            let layout = Layout::from_size_align(num_pages * PAGE_SIZE, PAGE_SIZE).unwrap();
            let ptr = NonNull::new(pos as *mut u8).unwrap();
            self.balloc.lock().dealloc(ptr, layout);
        }
        #[cfg(not(feature = "level-1"))]
        self.palloc.lock().dealloc_pages(pos, num_pages);
    }

    /// Returns the name of the allocator.
    pub const fn name(&self) -> &'static str {
        cfg_if::cfg_if! {
            if #[cfg(feature = "slab")] {
                "slab"
            } else if #[cfg(feature = "buddy")] {
                "buddy"
            } else if #[cfg(feature = "tlsf")] {
                "TLSF"
            } else {
                "unknown"
            }
        }
    }

    /// Initializes the allocator with the given region.
    ///
    /// It firstly adds the whole region to the page allocator, then allocates
    /// a small region (32 KB) to initialize the byte allocator. Therefore,
    /// the given region must be larger than 32 KB.
    pub fn init(&self, start_vaddr: usize, size: usize) {
        assert!(size > MIN_HEAP_SIZE);
        #[cfg(not(feature = "level-1"))]
        {
            let init_heap_size = MIN_HEAP_SIZE;
            self.palloc.lock().init(start_vaddr, size);
            let heap_ptr = self
                .alloc_pages(init_heap_size / PAGE_SIZE, PAGE_SIZE, UsageKind::RustHeap)
                .unwrap();

            self.balloc.lock().init(heap_ptr, init_heap_size);
        }
        #[cfg(feature = "level-1")]
        {
            self.balloc.lock().init(start_vaddr, size);
        }
    }

    /// Add the given region to the allocator.
    ///
    /// It will add the whole region to the byte allocator.
    pub fn add_memory(&self, start_vaddr: usize, size: usize) -> AllocResult {
        #[cfg(feature = "level-1")]
        {
            self.balloc.lock().add_memory(start_vaddr, size)
        }
        #[cfg(not(feature = "level-1"))]
        {
            self.palloc.lock().add_memory(start_vaddr, size)
        }
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// It firstly tries to allocate from the byte allocator. If there is no
    /// memory, it asks the page allocator for more memory and adds it to the
    /// byte allocator.
    pub fn alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        #[cfg(feature = "level-1")]
        {
            self.alloc_level1(layout)
        }
        #[cfg(not(feature = "level-1"))]
        {
            self.alloc_level2(layout)
        }
    }

    #[cfg(feature = "level-1")]
    fn alloc_level1(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        // single-level allocator: only use the byte allocator.
        let mut balloc = self.balloc.lock();
        let ptr = balloc.alloc(layout)?;
        self.usages.lock().alloc(UsageKind::RustHeap, layout.size());
        Ok(ptr)
    }

    #[cfg(not(feature = "level-1"))]
    fn alloc_level2(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        // simple two-level allocator: if no heap memory, allocate from the page allocator.
        let mut balloc = self.balloc.lock();
        loop {
            if let Ok(ptr) = balloc.alloc(layout) {
                self.usages.lock().alloc(UsageKind::RustHeap, layout.size());
                return Ok(ptr);
            } else {
                let old_size = balloc.total_bytes();
                let expand_size = old_size
                    .max(layout.size())
                    .next_power_of_two()
                    .max(PAGE_SIZE);

                let mut try_size = expand_size;
                let min_size = PAGE_SIZE.max(layout.size());
                loop {
                    let heap_ptr = match self.alloc_pages(
                        try_size / PAGE_SIZE,
                        PAGE_SIZE,
                        UsageKind::RustHeap,
                    ) {
                        Ok(ptr) => ptr,
                        Err(err) => {
                            try_size /= 2;
                            if try_size < min_size {
                                return Err(err);
                            }
                            continue;
                        }
                    };
                    debug!(
                        "expand heap memory: [{:#x}, {:#x})",
                        heap_ptr,
                        heap_ptr + try_size
                    );
                    balloc.add_memory(heap_ptr, try_size)?;
                    break;
                }
            }
        }
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// The region should be allocated by [`alloc`], and `align_pow2` should be
    /// the same as the one used in [`alloc`]. Otherwise, the behavior is
    /// undefined.
    pub fn dealloc(&self, pos: NonNull<u8>, layout: Layout) {
        self.usages
            .lock()
            .dealloc(UsageKind::RustHeap, layout.size());
        self.balloc.lock().dealloc(pos, layout)
    }

    /// Allocates contiguous pages.
    ///
    /// It allocates `num_pages` pages from the page allocator.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    pub fn alloc_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        #[cfg(feature = "level-1")]
        let addr = {
            let layout = Layout::from_size_align(num_pages * PAGE_SIZE, align_pow2).unwrap();
            self.balloc.lock().alloc(layout)?.as_ptr() as usize
        };
        #[cfg(not(feature = "level-1"))]
        let addr = self.palloc.lock().alloc_pages(num_pages, align_pow2)?;
        self.finish_page_allocation(addr, num_pages, kind)
    }

    /// Allocates contiguous pages starting from the given address.
    ///
    /// It allocates `num_pages` pages from the page allocator starting from the
    /// given address.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    pub fn alloc_pages_at(
        &self,
        start: usize,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        #[cfg(feature = "level-1")]
        {
            let _ = (start, num_pages, align_pow2, kind);
            unimplemented!("level-1 allocator does not support alloc_pages_at")
        }
        #[cfg(not(feature = "level-1"))]
        {
            let addr = self
                .palloc
                .lock()
                .alloc_pages_at(start, num_pages, align_pow2)?;
            self.finish_page_allocation(addr, num_pages, kind)
        }
    }

    /// Atomically reserves a fixed physical page range for an owner which
    /// must never be moved by the allocator, such as kexec.
    pub fn replace_pages_at(
        &self,
        start: usize,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        self.alloc_pages_at(start, num_pages, align_pow2, UsageKind::Kexec)
    }

    /// Gives back the allocated pages starts from `pos` to the page allocator.
    ///
    /// The pages should be allocated by [`alloc_pages`], and `align_pow2`
    /// should be the same as the one used in [`alloc_pages`]. Otherwise, the
    /// behavior is undefined.
    pub fn dealloc_pages(&self, pos: usize, num_pages: usize, kind: UsageKind) {
        self.accounting.retire(pos, num_pages, kind);
        self.usages.lock().dealloc(kind, num_pages * PAGE_SIZE);
        self.return_raw_pages(pos, num_pages);
    }

    /// Returns the number of allocated bytes in the byte allocator.
    pub fn used_bytes(&self) -> usize {
        self.balloc.lock().used_bytes()
    }

    /// Returns the number of available bytes in the byte allocator.
    pub fn available_bytes(&self) -> usize {
        self.balloc.lock().available_bytes()
    }

    /// Returns the number of allocated pages in the page allocator.
    pub fn used_pages(&self) -> usize {
        #[cfg(feature = "level-1")]
        {
            self.used_bytes().div_ceil(PAGE_SIZE)
        }
        #[cfg(not(feature = "level-1"))]
        {
            self.palloc.lock().used_pages()
        }
    }

    /// Returns the number of available pages in the page allocator.
    pub fn available_pages(&self) -> usize {
        #[cfg(feature = "level-1")]
        {
            self.available_bytes().div_ceil(PAGE_SIZE)
        }
        #[cfg(not(feature = "level-1"))]
        self.palloc.lock().available_pages()
    }

    /// Returns the usage statistics of the allocator.
    pub fn usages(&self) -> Usages {
        *self.usages.lock()
    }
}

/// Returns the reference to the global allocator.
pub fn global_allocator() -> &'static GlobalAllocator {
    &GLOBAL_ALLOCATOR
}

/// Initializes the global allocator with the given memory region.
///
/// Note that the memory region bounds are just numbers, and the allocator
/// does not actually access the region. Users should ensure that the region
/// is valid and not being used by others, so that the allocated memory is also
/// valid.
///
/// This function should be called only once, and before any allocation.
///
/// # Arguments
///
/// - `start_vaddr`: The starting virtual address of the memory region.
/// - `size`: The size of the memory region in bytes.
pub fn global_init(start_vaddr: usize, size: usize) {
    debug!(
        "initialize global allocator at: [{:#x}, {:#x})",
        start_vaddr,
        start_vaddr + size
    );
    GLOBAL_ALLOCATOR.init(start_vaddr, size);
}

/// Add the given memory region to the global allocator.
///
/// Users should ensure that the region is valid and not being used by others,
/// so that the allocated memory is also valid.
///
/// It's similar to [`global_init`], but can be called multiple times.
///
/// # Arguments
///
/// - `start_vaddr`: The starting virtual address of the memory region.
/// - `size`: The size of the memory region in bytes.
pub fn global_add_memory(start_vaddr: usize, size: usize) -> AllocResult {
    debug!(
        "add a memory region to global allocator: [{:#x}, {:#x})",
        start_vaddr,
        start_vaddr + size
    );
    GLOBAL_ALLOCATOR.add_memory(start_vaddr, size)
}

unsafe impl GlobalAlloc for GlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let inner = move || {
            if let Ok(ptr) = GlobalAllocator::alloc(self, layout) {
                ptr.as_ptr()
            } else {
                alloc::alloc::handle_alloc_error(layout)
            }
        };

        #[cfg(feature = "tracking")]
        {
            crate::tracking::with_state(|state| match state {
                None => inner(),
                Some(state) => {
                    let ptr = inner();
                    let generation = state.generation;
                    state.generation += 1;
                    state.map.insert(
                        ptr as usize,
                        crate::tracking::AllocationInfo {
                            layout,
                            backtrace: axbacktrace::Backtrace::capture(),
                            generation,
                        },
                    );
                    ptr
                }
            })
        }

        #[cfg(not(feature = "tracking"))]
        inner()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = NonNull::new(ptr).expect("dealloc null ptr");
        let inner = || GlobalAllocator::dealloc(self, ptr, layout);

        #[cfg(feature = "tracking")]
        crate::tracking::with_state(|state| match state {
            None => inner(),
            Some(state) => {
                let address = ptr.as_ptr() as usize;
                state.map.remove(&address);
                inner()
            }
        });

        #[cfg(not(feature = "tracking"))]
        inner();
    }
}

#[cfg(all(test, not(feature = "level-1")))]
mod accounting_tests {
    extern crate std;
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    static FAIL: AtomicBool = AtomicBool::new(false);
    static CHARGED: AtomicUsize = AtomicUsize::new(0);
    static HOOKS: PageAccountingHooks = PageAccountingHooks {
        allocated: |_, pages, _| {
            if FAIL.load(Ordering::Relaxed) { return false; }
            CHARGED.fetch_add(pages, Ordering::Relaxed); true
        },
        deallocated: |_, pages, _| { CHARGED.fetch_sub(pages, Ordering::Relaxed); },
    };
    #[test]
    fn physical_lifetime_admission_refunds_rejection_and_final_frame_return() {
        let layout = Layout::from_size_align(1024 * 1024, 1 << 30).unwrap();
        // SAFETY: the aligned test region stays alive until the allocator and
        // all allocations are dropped; it is freed with its original layout.
        let region = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!region.is_null());
        let allocator = GlobalAllocator::new();
        allocator.init(region as usize, layout.size());
        assert!(allocator.install_page_accounting(&HOOKS));
        assert!(!allocator.install_page_accounting(&HOOKS));
        let available = allocator.available_pages();
        let addr = allocator.alloc_pages(2, PAGE_SIZE, UsageKind::VirtMem).unwrap();
        assert_eq!(CHARGED.load(Ordering::Relaxed), 2);
        allocator.dealloc_pages(addr, 2, UsageKind::VirtMem);
        assert_eq!(CHARGED.load(Ordering::Relaxed), 0);
        assert_eq!(allocator.available_pages(), available);
        FAIL.store(true, Ordering::Relaxed);
        assert!(allocator.alloc_pages(2, PAGE_SIZE, UsageKind::VirtMem).is_err());
        assert_eq!(allocator.available_pages(), available);
        assert_eq!(allocator.usages().get(UsageKind::VirtMem), 0);
        FAIL.store(false, Ordering::Relaxed);
        let addr = allocator.alloc_pages(1, PAGE_SIZE, UsageKind::PageCache).unwrap();
        assert_eq!(CHARGED.load(Ordering::Relaxed), 1);
        allocator.dealloc_pages(addr, 1, UsageKind::PageCache);
        let addr = allocator.alloc_pages(1, PAGE_SIZE, UsageKind::Global).unwrap();
        assert_eq!(CHARGED.load(Ordering::Relaxed), 0);
        allocator.dealloc_pages(addr, 1, UsageKind::Global);
        drop(allocator);
        unsafe { std::alloc::dealloc(region, layout); }
    }
}
