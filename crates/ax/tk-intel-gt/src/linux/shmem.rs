// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI file-backed anonymous page cache for the source-order i915 GEM
//! translations.  The target kernel has no swap or tmpfs mount layer, so this
//! module owns zero-filled, page-aligned RAM pages until they are truncated or
//! their last reference is dropped.  Writeback cannot evict them: with no swap
//! device it leaves them dirty and reports the Linux no-swap result instead of
//! pretending that data was written.

#![allow(unsafe_code, non_camel_case_types, non_snake_case)]

use alloc::{
    alloc::{alloc, alloc_zeroed, dealloc},
    collections::BTreeMap,
};
use core::{
    cell::UnsafeCell,
    ffi::{CStr, c_char, c_int, c_long, c_ulong, c_void},
    hint::spin_loop,
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use crate::{
    i915_gem_object_types_upstream::Page,
    linux::{
        config::{GFP_KERNEL, PAGE_SHIFT, PAGE_SIZE},
        print::{self, CFormatArg, DrmLogLevel},
    },
};

const ENOMEM: c_int = 12;
const EFAULT: c_int = 14;
const EINVAL: c_int = 22;
const EFBIG: c_int = 27;
const ENOSPC: c_int = 28;
const FMODE_READ: u32 = 1;
const FMODE_WRITE: u32 = 2;
const ITER_SOURCE: u32 = 1;
const WB_SYNC_ALL: c_int = 1;
const AOP_WRITEPAGE_ACTIVATE: c_int = 0x80000;
const MAX_FILE_SIZE: u64 = i64::MAX as u64;
const PAGE_BATCH_SIZE: usize = 15;
const PAGE_MAGIC: u64 = 0x544b_5041_4745_7631;

const PAGE_DIRTY: u32 = 1 << 0;
const PAGE_ACCESSED: u32 = 1 << 1;
const PAGE_UNEVICTABLE: u32 = 1 << 2;
const PAGE_MAPPED: u32 = 1 << 3;
const PAGE_UPTODATE: u32 = 1 << 4;
const PAGE_WRITEBACK: u32 = 1 << 5;
const PAGE_LOCKED: u32 = 1 << 6;

const PAGE_LAYOUT: core::alloc::Layout =
    match core::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE) {
        Ok(layout) => layout,
        Err(_) => panic!("invalid page allocation layout"),
    };

struct SpinLock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    const fn new(value: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        SpinLockGuard { lock: self }
    }
}

struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the guard holds the lock for its lifetime.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the guard holds the lock for its lifetime.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

#[repr(C)]
struct PageRecord {
    magic: u64,
    address: *mut u8,
    physical: usize,
    refs: AtomicUsize,
    flags: AtomicU32,
    index: AtomicU64,
    mapping: AtomicPtr<AddressSpace>,
    next: *mut PageRecord,
}

unsafe impl Send for PageRecord {}

#[derive(Default)]
struct PageRegistry {
    first: *mut PageRecord,
    count: usize,
}

unsafe impl Send for PageRegistry {}

static PAGE_REGISTRY: SpinLock<PageRegistry> = SpinLock::new(PageRegistry {
    first: ptr::null_mut(),
    count: 0,
});

/// Linux `struct folio` uses `struct page` as its first member.  This target
/// keeps both as opaque pointer identities; `page_folio()` is therefore an
/// address-preserving conversion, while storage and accounting live in the
/// registered `PageRecord`.
#[repr(C)]
pub struct Folio {
    _opaque: [u8; 0],
}

/// The in-memory address-space state used by the anonymous shmem cache.
#[repr(C)]
pub struct AddressSpace {
    pages: SpinLock<BTreeMap<u64, usize>>,
    gfp_mask: AtomicU32,
    unevictable: AtomicBool,
    host: AtomicPtr<Inode>,
    writeback_index: AtomicU64,
}

impl AddressSpace {
    fn new() -> Self {
        Self {
            pages: SpinLock::new(BTreeMap::new()),
            gfp_mask: AtomicU32::new(GFP_KERNEL),
            unevictable: AtomicBool::new(false),
            host: AtomicPtr::new(ptr::null_mut()),
            writeback_index: AtomicU64::new(0),
        }
    }
}

/// Minimal inode owner for a LinuxKPI anonymous shmem file.
#[repr(C)]
pub struct Inode {
    pub mapping: *mut AddressSpace,
    pub size: AtomicU64,
    pub name: [u8; 32],
}

unsafe impl Send for Inode {}

/// Linux `struct file` prefix consumed by the i915 GEM transcription.
#[repr(C)]
pub struct File {
    pub _f_lock: [u8; 4],
    pub f_mode: u32,
    pub f_op: *const FileOperations,
    pub f_mapping: *mut AddressSpace,
    pub _private_data: *mut c_void,
    pub f_inode: *mut Inode,
    pub f_flags: u32,
    pub f_iocb_flags: u32,
    refs: AtomicUsize,
    name: [u8; 32],
}

unsafe impl Send for File {}

/// Linux `struct file_operations` prefix including `write_iter`.
#[repr(C)]
pub struct FileOperations {
    pub owner: *mut c_void,
    pub fop_flags: u32,
    pub _pad: u32,
    pub _llseek: *const c_void,
    pub _read: *const c_void,
    pub _write: *const c_void,
    pub _read_iter: *const c_void,
    pub write_iter: Option<unsafe extern "C" fn(*mut Kiocb, *mut IovIter) -> isize>,
}

unsafe impl Sync for FileOperations {}

/// Source-compatible writeback batch prefix.
#[repr(C)]
pub struct FolioBatch {
    pub nr: u8,
    pub i: u8,
    pub percpu_pvec_drained: bool,
    pub _pad: [u8; 5],
    pub folios: [*mut Folio; PAGE_BATCH_SIZE],
}

/// Fields of Linux `struct writeback_control` used by this i915 path.
#[repr(C)]
pub struct WritebackControl {
    pub nr_to_write: c_long,
    pub pages_skipped: c_long,
    pub range_start: u64,
    pub range_end: u64,
    pub sync_mode: c_int,
    pub _flags: u32,
    pub fbatch: FolioBatch,
    pub index: u64,
    pub saved_err: c_int,
    pub _cgroup_writeback: [u8; 64],
}

/// The `kiocb` prefix initialized by the synchronous i915 pwrite path.
#[repr(C)]
pub struct Kiocb {
    pub ki_filp: *mut File,
    pub ki_pos: i64,
    pub ki_complete: Option<unsafe extern "C" fn(*mut Kiocb, c_long)>,
    pub private: *mut c_void,
    pub ki_flags: c_int,
    pub ki_ioprio: u16,
    pub ki_write_stream: u8,
    pub _pad: u8,
    pub ki_waitq: *mut c_void,
}

/// Linux `iov_iter` storage used for an ubuf iterator.  The shape matches the
/// existing source prefix; the first four words carry the LinuxKPI state.
#[repr(C)]
pub struct IovIter {
    pub _opaque: [usize; 8],
}

/// Opaque vfsmount pointer accepted by shmem_file_setup_with_mnt().
#[repr(C)]
pub struct VfsMount {
    _opaque: [u8; 0],
}

const _: [(); 8] = [(); offset_of!(File, f_op)];
const _: [(); 16] = [(); offset_of!(File, f_mapping)];
const _: [(); 40] = [(); offset_of!(File, f_flags)];
const _: [(); 48] = [(); offset_of!(FileOperations, write_iter)];
const _: [(); 128] = [(); size_of::<FolioBatch>()];

static FILE_OPERATIONS: FileOperations = FileOperations {
    owner: ptr::null_mut(),
    fop_flags: 0,
    _pad: 0,
    _llseek: ptr::null(),
    _read: ptr::null(),
    _write: ptr::null(),
    _read_iter: ptr::null(),
    write_iter: Some(shmem_write_iter),
};

unsafe fn alloc_object<T>(value: T) -> *mut T {
    let layout = core::alloc::Layout::new::<T>();
    let object = unsafe { alloc(layout) }.cast::<T>();
    if object.is_null() {
        return ptr::null_mut();
    }
    unsafe { object.write(value) };
    object
}

unsafe fn free_object<T>(object: *mut T) {
    if object.is_null() {
        return;
    }
    unsafe {
        ptr::drop_in_place(object);
        dealloc(object.cast(), core::alloc::Layout::new::<T>());
    }
}

fn phys_addr(address: *mut u8) -> usize {
    #[cfg(target_os = "none")]
    {
        axhal::mem::virt_to_phys((address as usize).into()).as_usize()
    }
    #[cfg(not(target_os = "none"))]
    {
        // Host-only unit models have no target page tables.  The pointer is
        // used only as an identity within that model, never as hardware PFN
        // evidence.
        address as usize
    }
}

unsafe fn alloc_page(mapping: *mut AddressSpace, index: u64) -> *mut Page {
    let address = unsafe { alloc_zeroed(PAGE_LAYOUT) };
    if address.is_null() {
        return ptr::null_mut();
    }

    let physical = phys_addr(address);
    let record = unsafe {
        alloc_object(PageRecord {
            magic: PAGE_MAGIC,
            address,
            physical,
            refs: AtomicUsize::new(1), // address_space cache reference
            flags: AtomicU32::new(PAGE_UPTODATE),
            index: AtomicU64::new(index),
            mapping: AtomicPtr::new(mapping),
            next: ptr::null_mut(),
        })
    };
    if record.is_null() {
        unsafe { dealloc(address, PAGE_LAYOUT) };
        return ptr::null_mut();
    }

    {
        let mut registry = PAGE_REGISTRY.lock();
        let mut current = registry.first;
        while !current.is_null() {
            if unsafe { (*current).physical >> PAGE_SHIFT } == physical >> PAGE_SHIFT {
                drop(registry);
                unsafe {
                    dealloc(address, PAGE_LAYOUT);
                    free_object(record);
                }
                return ptr::null_mut();
            }
            current = unsafe { (*current).next };
        }
        unsafe { (*record).next = registry.first };
        registry.first = record;
        registry.count += 1;
    }
    record.cast::<Page>()
}

fn find_record(registry: &PageRegistry, page: *const Page) -> *mut PageRecord {
    if page.is_null() {
        return ptr::null_mut();
    }
    let mut current = registry.first;
    while !current.is_null() {
        if current.cast::<Page>() == page.cast_mut() {
            return current;
        }
        current = unsafe { (*current).next };
    }
    ptr::null_mut()
}

/// Bridge called by `linux::page::page_to_pfn` and the scatterlist iterator.
pub unsafe fn page_to_phys(page: *const Page) -> usize {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, page);
    assert!(
        !record.is_null(),
        "page_to_phys received an unregistered Page"
    );
    unsafe { (*record).physical }
}

/// Bridge called by the source-derived PFN-to-page path.
pub fn pfn_to_page(pfn: c_ulong) -> *mut Page {
    let registry = PAGE_REGISTRY.lock();
    let mut current = registry.first;
    while !current.is_null() {
        if unsafe { ((*current).physical >> PAGE_SHIFT) as c_ulong } == pfn {
            return current.cast::<Page>();
        }
        current = unsafe { (*current).next };
    }
    ptr::null_mut()
}

/// Bridge called by `linux::highmem::kmap_local_page`.
pub unsafe fn page_address(page: *mut Page) -> *mut c_void {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, page);
    assert!(
        !record.is_null(),
        "page_address received an unregistered Page"
    );
    unsafe { (*record).address.cast() }
}

/// Increment the Linux page reference count.
pub unsafe fn get_page(page: *mut Page) {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, page);
    assert!(!record.is_null(), "get_page received an unregistered Page");
    let old = unsafe { (*record).refs.fetch_add(1, Ordering::Relaxed) };
    assert!(
        old != 0 && old != usize::MAX,
        "invalid page reference count"
    );
}

/// Increment a folio reference; folio and page have the same identity here.
pub unsafe fn folio_get(folio: *mut Folio) {
    unsafe { get_page(folio.cast()) };
}

/// Decrement a Linux page reference and free backing storage on the last put.
pub unsafe fn put_page(page: *mut Page) {
    if page.is_null() {
        return;
    }
    let mut free = false;
    let record = {
        let mut registry = PAGE_REGISTRY.lock();
        let mut link = &mut registry.first as *mut *mut PageRecord;
        let mut current = unsafe { *link };
        while !current.is_null() && current.cast::<Page>() != page {
            link = unsafe { ptr::addr_of_mut!((*current).next) };
            current = unsafe { *link };
        }
        assert!(!current.is_null(), "put_page received an unregistered Page");
        let record = current;
        let old = unsafe { (*record).refs.fetch_sub(1, Ordering::AcqRel) };
        assert!(old != 0, "page reference underflow");
        if old == 1 {
            unsafe { *link = (*record).next };
            registry.count -= 1;
            free = true;
        }
        record
    };
    if free {
        unsafe {
            (*record).magic = 0;
            dealloc((*record).address, PAGE_LAYOUT);
            free_object(record);
        }
    }
}

/// Decrement a folio reference; folio and page have the same identity here.
pub unsafe fn folio_put(folio: *mut Folio) {
    unsafe { put_page(folio.cast()) };
}

/// Linux `page_folio()` for the registered base-page folios used here.
pub unsafe fn page_folio(page: *mut Page) -> *mut Folio {
    if page.is_null() {
        return ptr::null_mut();
    }
    let registry = PAGE_REGISTRY.lock();
    assert!(
        !find_record(&registry, page).is_null(),
        "page_folio received an unregistered Page"
    );
    page.cast()
}

/// Linux page lock used by userptr's notifier-safe invalidation path.
pub unsafe fn trylock_page(page: *mut Page) -> bool {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, page);
    if record.is_null() {
        return false;
    }
    unsafe { (*record).flags.fetch_or(PAGE_LOCKED, Ordering::Acquire) & PAGE_LOCKED == 0 }
}

/// Unlock a page locked by `trylock_page` or writeback iteration.
pub unsafe fn unlock_page(page: *mut Page) {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, page);
    assert!(
        !record.is_null(),
        "unlock_page received an unregistered Page"
    );
    let old = unsafe { (*record).flags.fetch_and(!PAGE_LOCKED, Ordering::Release) };
    assert!(old & PAGE_LOCKED != 0, "unlock_page on an unlocked page");
}

unsafe fn lock_page(page: *mut Page) {
    while !unsafe { trylock_page(page) } {
        spin_loop();
    }
}

/// Linux `folio_batch_init()`.
pub fn folio_batch_init(batch: &mut FolioBatch) {
    batch.nr = 0;
    batch.i = 0;
    batch.percpu_pvec_drained = false;
}

/// Linux `folio_batch_add()`; returns remaining slots.
pub fn folio_batch_add(batch: &mut FolioBatch, folio: *mut Folio) -> u32 {
    if folio.is_null() || batch.nr as usize >= PAGE_BATCH_SIZE {
        return 0;
    }
    batch.folios[batch.nr as usize] = folio;
    batch.nr += 1;
    (PAGE_BATCH_SIZE - batch.nr as usize) as u32
}

/// Release page references held by a batch.
pub unsafe fn __folio_batch_release(batch: *mut FolioBatch) {
    if batch.is_null() {
        return;
    }
    let nr = unsafe { (*batch).nr as usize }.min(PAGE_BATCH_SIZE);
    for index in 0..nr {
        let folio = unsafe { (*batch).folios[index] };
        if !folio.is_null() {
            let locked = {
                let registry = PAGE_REGISTRY.lock();
                let record = find_record(&registry, folio.cast());
                !record.is_null()
                    && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_LOCKED != 0 }
            };
            if locked {
                unsafe { unlock_page(folio.cast()) };
            }
            unsafe { folio_put(folio) };
            unsafe { (*batch).folios[index] = ptr::null_mut() };
        }
    }
    unsafe {
        (*batch).nr = 0;
        (*batch).i = 0;
        (*batch).percpu_pvec_drained = true;
    }
}

/// TheKernel has no reclaim LRU; this pass records the mapping's unevictable
/// state on each page rather than pretending to move pages between lists.
pub unsafe fn check_move_unevictable_folios(batch: *mut FolioBatch) {
    if batch.is_null() {
        return;
    }
    let nr = unsafe { (*batch).nr as usize }.min(PAGE_BATCH_SIZE);
    for index in 0..nr {
        let folio = unsafe { (*batch).folios[index] };
        let registry = PAGE_REGISTRY.lock();
        let record = find_record(&registry, folio.cast());
        if record.is_null() {
            continue;
        }
        let mapping = unsafe { (*record).mapping.load(Ordering::Acquire) };
        if !mapping.is_null() && unsafe { (*mapping).unevictable.load(Ordering::Acquire) } {
            unsafe { (*record).flags.fetch_or(PAGE_UNEVICTABLE, Ordering::AcqRel) };
        } else {
            unsafe {
                (*record)
                    .flags
                    .fetch_and(!PAGE_UNEVICTABLE, Ordering::AcqRel)
            };
        }
    }
}

/// Linux `mapping_set_unevictable()`.
pub unsafe fn mapping_set_unevictable(mapping: *mut AddressSpace) {
    if mapping.is_null() {
        return;
    }
    unsafe { (*mapping).unevictable.store(true, Ordering::Release) };
    {
        let pages = unsafe { (*mapping).pages.lock() };
        for page in pages.values().copied() {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page as *const Page);
            if !record.is_null() {
                unsafe { (*record).flags.fetch_or(PAGE_UNEVICTABLE, Ordering::AcqRel) };
            }
        }
    }
}

/// Linux `mapping_clear_unevictable()`.
pub unsafe fn mapping_clear_unevictable(mapping: *mut AddressSpace) {
    if mapping.is_null() {
        return;
    }
    unsafe { (*mapping).unevictable.store(false, Ordering::Release) };
    {
        let pages = unsafe { (*mapping).pages.lock() };
        for page in pages.values().copied() {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page as *const Page);
            if !record.is_null() {
                unsafe {
                    (*record)
                        .flags
                        .fetch_and(!PAGE_UNEVICTABLE, Ordering::AcqRel)
                };
            }
        }
    }
}

/// Linux `mapping_gfp_mask()`.
pub unsafe fn mapping_gfp_mask(mapping: *const AddressSpace) -> u32 {
    if mapping.is_null() {
        return GFP_KERNEL;
    }
    unsafe { (*mapping).gfp_mask.load(Ordering::Acquire) }
}

/// Linux `mapping_gfp_constraint()`.
pub unsafe fn mapping_gfp_constraint(mapping: *const AddressSpace, mask: u32) -> u32 {
    unsafe { mapping_gfp_mask(mapping) & mask }
}

/// Linux `mapping_set_gfp_mask()`.
pub unsafe fn mapping_set_gfp_mask(mapping: *mut AddressSpace, mask: u32) {
    if !mapping.is_null() {
        unsafe { (*mapping).gfp_mask.store(mask, Ordering::Release) };
    }
}

/// Linux `folio_mark_dirty()` for an anonymous page-cache folio.
pub unsafe fn folio_mark_dirty(folio: *mut Folio) -> bool {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, folio.cast());
    assert!(
        !record.is_null(),
        "folio_mark_dirty received an unregistered folio"
    );
    unsafe { (*record).flags.fetch_or(PAGE_DIRTY, Ordering::AcqRel) & PAGE_DIRTY == 0 }
}

/// Linux `folio_mark_accessed()`.
pub unsafe fn folio_mark_accessed(folio: *mut Folio) {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, folio.cast());
    assert!(
        !record.is_null(),
        "folio_mark_accessed received an unregistered folio"
    );
    unsafe { (*record).flags.fetch_or(PAGE_ACCESSED, Ordering::Relaxed) };
}

/// Linux highmem API: account an access to a page-cache page.
pub unsafe fn mark_page_accessed(page: *mut Page) {
    let folio = unsafe { page_folio(page) };
    if !folio.is_null() {
        unsafe { folio_mark_accessed(folio) };
    }
}

/// Linux `folio_nr_pages()`; this KPI currently allocates order-0 pages only.
pub unsafe fn folio_nr_pages(folio: *const Folio) -> usize {
    let registry = PAGE_REGISTRY.lock();
    assert!(
        !find_record(&registry, folio.cast()).is_null(),
        "folio_nr_pages received an unregistered folio"
    );
    1
}

/// Linux `folio_mapped()` for this mapping implementation.  There is no
/// userspace mmap path in this KPI yet, so only explicit map-count updates can
/// report a folio mapped.
pub unsafe fn folio_mapped(folio: *const Folio) -> bool {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, folio.cast());
    !record.is_null() && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_MAPPED != 0 }
}

/// Mark a page as mapped/unmapped for future address-space integrations.
pub unsafe fn set_folio_mapped(folio: *mut Folio, mapped: bool) {
    let registry = PAGE_REGISTRY.lock();
    let record = find_record(&registry, folio.cast());
    assert!(
        !record.is_null(),
        "set_folio_mapped received an unregistered folio"
    );
    if mapped {
        unsafe { (*record).flags.fetch_or(PAGE_MAPPED, Ordering::AcqRel) };
    } else {
        unsafe { (*record).flags.fetch_and(!PAGE_MAPPED, Ordering::AcqRel) };
    }
}

fn err_ptr<T>(error: c_int) -> *mut T {
    crate::linux_config::ERR_PTR(error)
}

fn copy_file_name(name: *const c_char) -> [u8; 32] {
    let mut result = [0; 32];
    if !name.is_null() {
        // All callers pass a valid NUL-terminated kernel string.
        let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
        let count = bytes.len().min(result.len() - 1);
        result[..count].copy_from_slice(&bytes[..count]);
    }
    result
}

unsafe fn cleanup_mapping(mapping: *mut AddressSpace) {
    if mapping.is_null() {
        return;
    }
    loop {
        let page = {
            let mut pages = unsafe { (*mapping).pages.lock() };
            let Some((&index, &page)) = pages.first_key_value() else {
                break;
            };
            pages.remove(&index);
            page as *mut Page
        };
        {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page);
            if !record.is_null() {
                unsafe { (*record).mapping.store(ptr::null_mut(), Ordering::Release) };
            }
        }
        unsafe { put_page(page) };
    }
    unsafe { free_object(mapping) };
}

unsafe fn setup_file(name: *const c_char, size: u64) -> *mut File {
    if name.is_null() || size > MAX_FILE_SIZE {
        return err_ptr(-EINVAL);
    }
    let name = copy_file_name(name);
    let mapping = unsafe { alloc_object(AddressSpace::new()) };
    if mapping.is_null() {
        return err_ptr(-ENOMEM);
    }
    let inode = unsafe {
        alloc_object(Inode {
            mapping,
            size: AtomicU64::new(size),
            name,
        })
    };
    if inode.is_null() {
        unsafe { cleanup_mapping(mapping) };
        return err_ptr(-ENOMEM);
    }
    unsafe { (*mapping).host.store(inode, Ordering::Release) };
    let file = unsafe {
        alloc_object(File {
            _f_lock: [0; 4],
            f_mode: FMODE_READ | FMODE_WRITE,
            f_op: &FILE_OPERATIONS,
            f_mapping: mapping,
            _private_data: ptr::null_mut(),
            f_inode: inode,
            f_flags: 0,
            f_iocb_flags: 0,
            refs: AtomicUsize::new(1),
            name,
        })
    };
    if file.is_null() {
        unsafe {
            free_object(inode);
            cleanup_mapping(mapping);
        }
        return err_ptr(-ENOMEM);
    }
    file
}

/// Create a zero-filled, file-backed anonymous LinuxKPI shmem object.
pub unsafe fn shmem_file_setup(name: *const c_char, size: u64, _flags: c_ulong) -> *mut File {
    unsafe { setup_file(name, size) }
}

/// The target has no tmpfs mount layer; a caller-supplied mount does not alter
/// the semantics of this anonymous page-cache owner.
pub unsafe fn shmem_file_setup_with_mnt(
    _mnt: *mut VfsMount,
    name: *const c_char,
    size: u64,
    flags: c_ulong,
) -> *mut File {
    unsafe { shmem_file_setup(name, size, flags) }
}

/// Acquire a reference to a LinuxKPI shmem file for a VMA or other owner.
pub unsafe fn get_file_file(file: *mut File) -> *mut File {
    if file.is_null() || crate::linux_config::IS_ERR(file) {
        return file;
    }
    let old = unsafe { (*file).refs.fetch_add(1, Ordering::Relaxed) };
    assert!(
        old != 0 && old != usize::MAX,
        "invalid file reference count"
    );
    file
}

/// Drop a shmem file reference and release its cache on the final put.
pub unsafe fn fput_file(file: *mut File) {
    if file.is_null() || crate::linux_config::IS_ERR(file) {
        return;
    }
    let old = unsafe { (*file).refs.fetch_sub(1, Ordering::AcqRel) };
    assert!(old != 0, "file reference underflow");
    if old != 1 {
        return;
    }
    let mapping = unsafe { (*file).f_mapping };
    let inode = unsafe { (*file).f_inode };
    unsafe {
        cleanup_mapping(mapping);
        free_object(inode);
        free_object(file);
    }
}

/// Linux-compatible `fput(void *)` wrapper for callers that keep the original
/// opaque file-pointer type.
pub unsafe fn fput(file: *mut c_void) {
    unsafe { fput_file(file.cast()) };
}

/// Return the page-cache folio at @index, creating a zeroed page on a miss.
pub unsafe fn shmem_read_folio_gfp(
    mapping: *mut AddressSpace,
    index: c_ulong,
    _gfp: u32,
) -> *mut Folio {
    if mapping.is_null() {
        return err_ptr(-EINVAL);
    }
    let index = index as u64;
    let inode = unsafe { (*mapping).host.load(Ordering::Acquire) };
    if inode.is_null() {
        return err_ptr(-EINVAL);
    }
    let size = unsafe { (*inode).size.load(Ordering::Acquire) };
    let Some(page_start) = index.checked_mul(PAGE_SIZE as u64) else {
        return err_ptr(-EFBIG);
    };
    if page_start >= size {
        return err_ptr(-EINVAL);
    }

    let page = {
        let mut pages = unsafe { (*mapping).pages.lock() };
        let page = if let Some(page) = pages.get(&index).copied() {
            page as *mut Page
        } else {
            let page = unsafe { alloc_page(mapping, index) };
            if page.is_null() {
                return err_ptr(-ENOMEM);
            }
            pages.insert(index, page as usize);
            page
        };
        // Take the caller's reference before releasing the mapping lock so a
        // concurrent truncate cannot remove the cache reference first.
        unsafe { get_page(page) };
        page
    };
    unsafe { lock_page(page) };
    unsafe { unlock_page(page) };
    page.cast::<Folio>()
}

/// Linux `shmem_read_mapping_page_gfp()` page-returning wrapper.
pub unsafe fn shmem_read_mapping_page_gfp(
    mapping: *mut AddressSpace,
    index: c_ulong,
    gfp: u32,
) -> *mut Page {
    unsafe { shmem_read_folio_gfp(mapping, index, gfp) }.cast()
}

/// Linux `shmem_read_mapping_page()` with the mapping's allocation mask.
pub unsafe fn shmem_read_mapping_page(mapping: *mut AddressSpace, index: c_ulong) -> *mut Page {
    let gfp = unsafe { mapping_gfp_mask(mapping) };
    unsafe { shmem_read_mapping_page_gfp(mapping, index, gfp) }
}

/// Drop all page-cache entries in the inclusive byte range.  The GEM caller
/// uses [0, -1], which releases every cached page while preserving file size.
pub unsafe fn shmem_truncate_range(inode: *mut Inode, start: i64, end: i64) {
    if inode.is_null() {
        return;
    }
    let mapping = unsafe { (*inode).mapping };
    if mapping.is_null() || start < 0 {
        return;
    }
    let first = (start as u64) >> PAGE_SHIFT;
    let last = if end < 0 {
        u64::MAX
    } else {
        (end as u64) >> PAGE_SHIFT
    };
    if first > last {
        return;
    }
    loop {
        let page = {
            let mut pages = unsafe { (*mapping).pages.lock() };
            let key = pages.range(first..=last).next().map(|(index, _)| *index);
            key.and_then(|index| pages.remove(&index))
                .map(|page| page as *mut Page)
        };
        let Some(page) = page else {
            break;
        };
        {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page);
            if !record.is_null() {
                unsafe { (*record).mapping.store(ptr::null_mut(), Ordering::Release) };
            }
        }
        unsafe { put_page(page) };
    }
}

/// Set up a synchronous `kiocb` for a file write iterator.
pub unsafe fn init_sync_kiocb(kiocb: &mut Kiocb, file: *mut File) {
    kiocb.ki_filp = file;
    kiocb.ki_pos = 0;
    kiocb.ki_complete = None;
    kiocb.private = ptr::null_mut();
    kiocb.ki_flags = if file.is_null() {
        0
    } else {
        unsafe { (*file).f_iocb_flags as c_int }
    };
    kiocb.ki_ioprio = 0;
    kiocb.ki_write_stream = 0;
    kiocb._pad = 0;
    kiocb.ki_waitq = ptr::null_mut();
}

/// Initialize a single-user-buffer iterator.  The backing words are private to
/// this KPI and keep the source-layout size of Linux `struct iov_iter`.
pub unsafe fn iov_iter_ubuf(iter: &mut IovIter, direction: u32, buffer: *mut c_void, count: usize) {
    iter._opaque = [0; 8];
    iter._opaque[0] = direction as usize;
    iter._opaque[1] = buffer as usize;
    iter._opaque[2] = count;
}

unsafe fn write_at(file: *mut File, mut position: u64, data: *const u8, count: usize) -> isize {
    if file.is_null() || unsafe { crate::linux_config::IS_ERR(file) } {
        return -EINVAL as isize;
    }
    let mapping = unsafe { (*file).f_mapping };
    let inode = unsafe { (*file).f_inode };
    let Some(end) = position.checked_add(count as u64) else {
        return -EFBIG as isize;
    };
    if end > MAX_FILE_SIZE || mapping.is_null() || inode.is_null() {
        return -EFBIG as isize;
    }
    if count != 0 && data.is_null() {
        return -EFAULT as isize;
    }
    let mut written = 0usize;
    while written < count {
        let index = (position >> PAGE_SHIFT) as c_ulong;
        let offset = (position as usize) & (PAGE_SIZE - 1);
        let this = (PAGE_SIZE - offset).min(count - written);
        let folio = unsafe { shmem_read_folio_gfp(mapping, index, mapping_gfp_mask(mapping)) };
        if crate::linux_config::IS_ERR(folio) {
            return if written != 0 {
                written as isize
            } else {
                crate::linux_config::PTR_ERR(folio) as isize
            };
        }
        let page = folio.cast::<Page>();
        unsafe { lock_page(page) };
        let target = unsafe { page_address(page).cast::<u8>().add(offset) };
        unsafe { ptr::copy_nonoverlapping(data.add(written), target, this) };
        {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page);
            assert!(!record.is_null(), "write_at lost its page registration");
            unsafe { (*record).flags.fetch_or(PAGE_UPTODATE, Ordering::Release) };
        }
        unsafe { folio_mark_dirty(folio) };
        unsafe { folio_mark_accessed(folio) };
        unsafe { unlock_page(page) };
        unsafe { put_page(page) };
        written += this;
        position += this as u64;
    }
    let size = unsafe { (*inode).size.load(Ordering::Acquire) };
    if end > size {
        unsafe { (*inode).size.store(end, Ordering::Release) };
    }
    written as isize
}

unsafe extern "C" fn shmem_write_iter(kiocb: *mut Kiocb, iter: *mut IovIter) -> isize {
    if kiocb.is_null() || iter.is_null() {
        return -EINVAL as isize;
    }
    let state = unsafe { &(*iter)._opaque };
    if state[0] != ITER_SOURCE as usize {
        return -EINVAL as isize;
    }
    let file = unsafe { (*kiocb).ki_filp };
    let position = unsafe { (*kiocb).ki_pos };
    if position < 0 {
        return -EINVAL as isize;
    }
    let result = unsafe { write_at(file, position as u64, state[1] as *const u8, state[2]) };
    if result > 0 {
        unsafe { (*kiocb).ki_pos = (*kiocb).ki_pos.saturating_add(result as i64) };
    }
    result
}

/// Write directly to the anonymous shmem page cache from a kernel buffer.
pub unsafe fn kernel_write(
    file: *mut File,
    data: *const c_void,
    count: usize,
    pos: *mut i64,
) -> isize {
    if pos.is_null() || unsafe { *pos < 0 } || (data.is_null() && count != 0) {
        return -EINVAL as isize;
    }
    let result = unsafe { write_at(file, *pos as u64, data.cast(), count) };
    if result > 0 {
        unsafe { *pos = (*pos).saturating_add(result as i64) };
    }
    result
}

/// Files on x86_64 use large-file offsets by default.
pub fn force_o_largefile() -> bool {
    usize::BITS > 32
}

/// The target has no swap device; writeback retains dirty folios in memory.
pub unsafe fn shmem_writeout(
    folio: *mut Folio,
    _plug: *mut *mut c_void,
    _list: *mut c_void,
) -> c_int {
    if folio.is_null() {
        return -EINVAL;
    }
    unsafe { folio_mark_dirty(folio) };
    // Linux reports AOP_WRITEPAGE_ACTIVATE when shmem cannot allocate swap.
    // This direct writeback caller has no VM writepage wrapper to unlock it.
    let page = folio.cast::<Page>();
    let locked = {
        let registry = PAGE_REGISTRY.lock();
        let record = find_record(&registry, page);
        !record.is_null() && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_LOCKED != 0 }
    };
    if locked {
        unsafe { unlock_page(page) };
    }
    AOP_WRITEPAGE_ACTIVATE
}

/// Redirty a folio declined by writeback and release the writeback lock.
pub unsafe fn folio_redirty_for_writepage(wbc: *mut WritebackControl, folio: *mut Folio) -> bool {
    if folio.is_null() {
        return false;
    }
    if !wbc.is_null() {
        unsafe { (*wbc).pages_skipped = (*wbc).pages_skipped.saturating_add(1) };
    }
    let changed = unsafe { folio_mark_dirty(folio) };
    let page = folio.cast::<Page>();
    let locked = {
        let registry = PAGE_REGISTRY.lock();
        let record = find_record(&registry, page);
        !record.is_null() && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_LOCKED != 0 }
    };
    if locked {
        unsafe { unlock_page(page) };
    }
    changed
}

/// Iterate dirty folios in an address_space and lock each page for writeback.
/// The returned page reference is retained in the writeback batch until the
/// next iterator call, matching the Linux writeback_control lifetime.
pub unsafe fn writeback_iter(
    mapping: *mut AddressSpace,
    wbc: *mut WritebackControl,
    previous: *mut Folio,
    error: *mut c_int,
) -> *mut Folio {
    if mapping.is_null() || wbc.is_null() || error.is_null() {
        return ptr::null_mut();
    }
    if previous.is_null() {
        folio_batch_init(unsafe { &mut (*wbc).fbatch });
        unsafe {
            (*wbc).index = (*wbc).range_start >> PAGE_SHIFT;
            (*wbc).saved_err = 0;
            *error = 0;
        }
    } else {
        unsafe { __folio_batch_release(ptr::addr_of_mut!((*wbc).fbatch)) };
        let record = {
            let registry = PAGE_REGISTRY.lock();
            find_record(&registry, previous.cast())
        };
        if !record.is_null() {
            let next = unsafe { (*record).index.load(Ordering::Acquire).saturating_add(1) };
            unsafe { (*wbc).index = (*wbc).index.max(next) };
        }
        unsafe { (*wbc).nr_to_write = (*wbc).nr_to_write.saturating_sub(1) };
        if unsafe { *error != 0 && (*wbc).saved_err == 0 } {
            unsafe { (*wbc).saved_err = *error };
        }
        if unsafe { (*error != 0 && (*wbc).sync_mode != WB_SYNC_ALL) || (*wbc).nr_to_write <= 0 } {
            unsafe { *error = (*wbc).saved_err };
            return ptr::null_mut();
        }
    }

    loop {
        let candidate = {
            let pages = unsafe { (*mapping).pages.lock() };
            let candidate = pages
                .range(unsafe { (*wbc).index }..)
                .find(|(index, page)| {
                    let index = **index;
                    let page = **page as *const Page;
                    (index << PAGE_SHIFT) <= unsafe { (*wbc).range_end } && {
                        let registry = PAGE_REGISTRY.lock();
                        let record = find_record(&registry, page);
                        !record.is_null()
                            && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_DIRTY != 0 }
                    }
                })
                .map(|(index, page)| (*index, *page as *mut Page));
            if let Some((_, page)) = candidate {
                // Keep the page alive while dropping the address-space lock.
                unsafe { get_page(page) };
            }
            candidate
        };
        let Some((index, page)) = candidate else {
            unsafe { __folio_batch_release(ptr::addr_of_mut!((*wbc).fbatch)) };
            unsafe { *error = (*wbc).saved_err };
            return ptr::null_mut();
        };
        unsafe { (*wbc).index = index.saturating_add(1) };
        unsafe { lock_page(page) };
        let dirty = {
            let registry = PAGE_REGISTRY.lock();
            let record = find_record(&registry, page);
            !record.is_null()
                && unsafe { (*record).flags.load(Ordering::Acquire) & PAGE_DIRTY != 0 }
        };
        if !dirty {
            unsafe { unlock_page(page) };
            unsafe { put_page(page) };
            continue;
        }
        let folio = page.cast::<Folio>();
        folio_batch_add(unsafe { &mut (*wbc).fbatch }, folio);
        return folio;
    }
}

/// Number of available RAM pages on the target, or modeled live pages on a
/// host-only build.
pub fn totalram_pages() -> c_ulong {
    #[cfg(target_os = "none")]
    {
        use axhal::mem::MemRegionFlags;
        let bytes = axhal::mem::memory_regions()
            .filter(|region| region.flags.contains(MemRegionFlags::FREE))
            .fold(0usize, |total, region| total.saturating_add(region.size));
        (bytes / PAGE_SIZE) as c_ulong
    }
    #[cfg(not(target_os = "none"))]
    {
        PAGE_REGISTRY.lock().count as c_ulong
    }
}

/// TheKernel has no kswapd worker; there is no background-reclaim caller.
pub fn current_is_kswapd() -> bool {
    false
}

/// LinuxKPI `dev_warn()` adapter for the one-argument i915 callsite.
pub fn dev_warn<T, A: CFormatArg>(device: *mut T, format: &str, argument: A) {
    let _ = device;
    let args: [&dyn CFormatArg; 1] = [&argument];
    let message = print::format_c_message(format, &args);
    print::drm_log_at(
        DrmLogLevel::Warn,
        "i915 LinuxKPI warning",
        file!(),
        line!(),
        &message,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_pages_survive_no_swap_writeback_and_truncate() {
        unsafe {
            let file = shmem_file_setup(c"native-test".as_ptr(), (2 * PAGE_SIZE) as u64, 0);
            assert!(!file.is_null() && (file as isize) > 0);
            let payload = [0xa5u8; 32];
            let mut pos = (PAGE_SIZE - 16) as i64;
            assert_eq!(
                kernel_write(file, payload.as_ptr().cast(), payload.len(), &mut pos),
                32
            );
            assert_eq!(pos, (PAGE_SIZE + 16) as i64);
            let mapping = (*file).f_mapping;
            let folio = shmem_read_folio_gfp(mapping, 0, GFP_KERNEL);
            assert!(!folio.is_null() && (folio as isize) > 0);
            let page = folio.cast::<Page>();
            let addr = page_address(page).cast::<u8>();
            assert_eq!(*addr, 0);
            assert_eq!(*addr.add(PAGE_SIZE - 1), 0xa5);
            assert_eq!(
                shmem_writeout(folio, ptr::null_mut(), ptr::null_mut()),
                AOP_WRITEPAGE_ACTIVATE
            );
            assert_eq!(*addr.add(PAGE_SIZE - 1), 0xa5);
            shmem_truncate_range((*file).f_inode, 0, -1);
            // A caller reference survives removal of the mapping reference.
            assert_eq!(*addr.add(PAGE_SIZE - 1), 0xa5);
            folio_put(folio);
            let fresh = shmem_read_folio_gfp(mapping, 0, GFP_KERNEL);
            assert_eq!(
                *page_address(fresh.cast()).cast::<u8>().add(PAGE_SIZE - 1),
                0
            );
            folio_put(fresh);
            fput_file(file);
        }
    }
}
