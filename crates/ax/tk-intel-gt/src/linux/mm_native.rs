// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Original LinuxKPI MM operations over native axmm address spaces. No Linux
//! MM implementation is copied. A process owner binds its native address space
//! explicitly; kernel tasks without a binding cannot create user mappings.
use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    ffi::{c_int, c_long, c_ulong, c_void},
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use axhal::paging::MappingFlags;
use memory_addr::{MemoryAddr, VirtAddrRange};

use super::{
    mm::{VM_WRITE, VmAreaStruct},
    shmem,
};
use crate::{
    i915_gem_object_types_upstream::Page,
    intel_context_types_upstream::File,
    linux::mmu_notifier::{MmuIntervalNotifier, MmuIntervalNotifierOps, MmuNotifierRange},
    linux_config::PAGE_SIZE,
};

pub struct MmStruct {
    aspace: UnsafeCell<axmm::AddrSpace>,
    vmas: UnsafeCell<BTreeMap<usize, NativeVma>>,
    locked: AtomicBool,
    user_access_owner: AtomicU64,
    user_access_start: AtomicUsize,
    user_access_end: AtomicUsize,
    /// `struct mmu_interval_notifier *` registered on this mm.
    interval_notifiers: spin::Mutex<Vec<usize>>,
    /// Sequence stamped into each invalidated notifier (`mm->notifier_seq`).
    notifier_seq: AtomicU64,
}
struct NativeVma {
    area: Box<VmAreaStruct>,
    pages: Vec<usize>,
    file: usize,
}
impl Drop for NativeVma {
    fn drop(&mut self) {
        for page in &self.pages {
            unsafe { shmem::put_page(*page as *mut Page) };
        }
        if self.file != 0 {
            unsafe { shmem::fput_file(self.file as *mut shmem::File) };
        }
    }
}
// All access to native MM/VMA storage is serialized by the mmap lock. The
// current-task binding owns an Arc until the process explicitly unbinds it.
unsafe impl Send for MmStruct {}
unsafe impl Sync for MmStruct {}
static MAPPINGS: spin::Mutex<BTreeMap<u64, Arc<MmStruct>>> = spin::Mutex::new(BTreeMap::new());
impl MmStruct {
    pub fn new(aspace: axmm::AddrSpace) -> Arc<Self> {
        Arc::new(Self {
            aspace: UnsafeCell::new(aspace),
            vmas: UnsafeCell::new(BTreeMap::new()),
            locked: AtomicBool::new(false),
            user_access_owner: AtomicU64::new(u64::MAX),
            user_access_start: AtomicUsize::new(0),
            user_access_end: AtomicUsize::new(0),
            interval_notifiers: spin::Mutex::new(Vec::new()),
            notifier_seq: AtomicU64::new(0),
        })
    }
}
/// Process-lifetime binding, not automatically installed by the GT feature.
pub fn bind_current_mm(mm: Arc<MmStruct>) -> Result<(), &'static str> {
    let task = axtask::current_may_uninit().ok_or("no current task")?;
    let mut mappings = MAPPINGS.lock();
    if mappings.contains_key(&task.id().as_u64()) {
        return Err("task already has an MM binding");
    }
    mappings.insert(task.id().as_u64(), mm);
    Ok(())
}
/// Call only after all userptr/MM users of the process have quiesced.
pub unsafe fn unbind_current_mm() {
    if let Some(task) = axtask::current_may_uninit() {
        MAPPINGS.lock().remove(&task.id().as_u64());
    }
}
pub fn current_mm() -> *mut MmStruct {
    let Some(task) = axtask::current_may_uninit() else {
        return ptr::null_mut();
    };
    MAPPINGS
        .lock()
        .get(&task.id().as_u64())
        .map_or(ptr::null_mut(), |mm| Arc::as_ptr(mm).cast_mut())
}
pub unsafe fn mmap_read_lock(mm: *mut MmStruct) {
    assert!(!mm.is_null(), "mmap lock without a user address space");
    while unsafe {
        (*mm)
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
    }
    .is_err()
    {
        axtask::yield_now();
    }
}
pub unsafe fn mmap_read_unlock(mm: *mut MmStruct) {
    assert!(!mm.is_null());
    assert!(
        unsafe { (*mm).locked.swap(false, Ordering::Release) },
        "unbalanced mmap unlock"
    );
}

fn mmap_read_trylock(mm: *mut MmStruct) -> bool {
    !mm.is_null()
        && unsafe {
            (*mm)
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        }
}
/// Native tasks have no killable-wait signal channel here; successful return
/// means the real MM lock was acquired, not that a missing lock was skipped.
pub unsafe fn mmap_write_lock_killable(mm: *mut MmStruct) -> c_int {
    if mm.is_null() {
        return -crate::linux_config::ENODEV;
    }
    unsafe { mmap_read_lock(mm) };
    0
}
pub unsafe fn mmap_write_unlock(mm: *mut MmStruct) {
    unsafe { mmap_read_unlock(mm) }
}
/// This serialized native backend deliberately provides stronger exclusion
/// than a Linux read semaphore; it does not allow a writer past a reader.
pub unsafe fn find_vma(mm: *mut MmStruct, addr: c_ulong) -> *mut VmAreaStruct {
    if mm.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (&mut *(*mm).vmas.get())
            .values_mut()
            .find(|v| v.area.vm_end > addr)
            .map_or(ptr::null_mut(), |v| &mut *v.area)
    }
}
pub struct VmaIterator {
    mm: *mut MmStruct,
    next: c_ulong,
}
impl VmaIterator {
    pub fn new(mm: *mut MmStruct, addr: c_ulong) -> Self {
        Self { mm, next: addr }
    }
}
pub unsafe fn vma_find(iter: *mut VmaIterator, end: c_ulong) -> *mut VmAreaStruct {
    let iter = unsafe { &mut *iter };
    if iter.next >= end {
        return ptr::null_mut();
    }
    let vma = unsafe { find_vma(iter.mm, iter.next) };
    if vma.is_null() || unsafe { (*vma).vm_start >= end } {
        return ptr::null_mut();
    }
    iter.next = unsafe { (*vma).vm_end };
    vma
}
fn map_flags(prot: u64) -> MappingFlags {
    let mut flags = MappingFlags::USER | MappingFlags::READ;
    if prot & 2 != 0 {
        flags |= MappingFlags::WRITE;
    }
    if prot & (1u64 << 63) == 0 {
        flags |= MappingFlags::EXECUTE;
    }
    if prot & 0x18 != 0 {
        flags |= MappingFlags::UNCACHED;
    }
    flags
}
/// Install a real page-table mapping into the bound native address space.
/// The caller owns the mmap lock and the backing page/IO range lifetime.
pub unsafe fn map_pfn(mm: *mut MmStruct, addr: usize, pfn: usize, prot: u64) -> c_int {
    if mm.is_null() || addr & 4095 != 0 {
        return -crate::linux_config::EINVAL;
    }
    if unsafe { (&*(*mm).aspace.get()).query_leaf(addr.into()).is_ok() } {
        unsafe { invalidate_interval_notifiers(mm, addr, 4096, MMU_NOTIFY_CLEAR) };
    }
    let space = unsafe { &mut *(*mm).aspace.get() };
    if space.query_leaf(addr.into()).is_ok() && space.unmap(addr.into(), 4096).is_err() {
        return -crate::linux_config::EIO;
    }
    match space.map_linear(addr.into(), (pfn << 12).into(), 4096, map_flags(prot)) {
        Ok(()) => {
            unsafe { axhal::asm::flush_tlb(Some(addr.into())) };
            0
        }
        Err(_) => -crate::linux_config::ENOMEM,
    }
}
/// Range record handed to interval-notifier callbacks. Field order follows
/// Linux `struct mmu_notifier_range` (CONFIG_MMU_NOTIFIER).
#[repr(C)]
struct NotifierRange {
    mm: *mut MmStruct,
    start: c_ulong,
    end: c_ulong,
    flags: u32,
    event: u32,
    owner: *mut c_void,
}

const MMU_NOTIFY_UNMAP: u32 = 0;
const MMU_NOTIFY_CLEAR: u32 = 1;

/// Linux `mmu_notifier_invalidate_range_start()` for interval notifiers: every
/// notifier whose interval overlaps `[start, start + size)` is invalidated
/// before the PTEs go away. Each invalidation takes a fresh `notifier_seq`,
/// which the callback stores into the notifier's `invalidate_seq`, so readers
/// that sampled an earlier sequence see the retry.
unsafe fn invalidate_interval_notifiers(mm: *mut MmStruct, start: usize, size: usize, event: u32) {
    let subscribers = unsafe { (*mm).interval_notifiers.lock().clone() };
    if subscribers.is_empty() || size == 0 {
        return;
    }
    let Some(end) = start.checked_add(size) else {
        return;
    };
    let cur_seq = unsafe { (*mm).notifier_seq.fetch_add(1, Ordering::AcqRel) } + 1;
    let range = NotifierRange {
        mm,
        start: start as c_ulong,
        end: end as c_ulong,
        flags: 0,
        event,
        owner: ptr::null_mut(),
    };
    for raw in subscribers {
        let notifier = raw as *mut MmuIntervalNotifier;
        let (first, last) = unsafe {
            (
                (*notifier).interval_tree.start,
                (*notifier).interval_tree.last,
            )
        };
        if first >= end || start > last {
            continue;
        }
        let ops = unsafe { (*notifier).ops };
        let invalidate =
            unsafe { (*ops).invalidate }.expect("interval notifier without invalidate");
        let blockable = unsafe {
            invalidate(
                notifier,
                ptr::addr_of!(range).cast::<MmuNotifierRange>(),
                cur_seq as c_ulong,
            )
        };
        assert!(
            blockable,
            "blockable interval-notifier invalidation refused"
        );
    }
}

pub unsafe fn zap_range(mm: *mut MmStruct, addr: usize, size: usize) {
    assert!(!mm.is_null());
    unsafe { invalidate_interval_notifiers(mm, addr, size, MMU_NOTIFY_UNMAP) };
    let space = unsafe { &mut *(*mm).aspace.get() };
    if size != 0 {
        let page_size = crate::linux_config::PAGE_SIZE;
        let mut offset = 0usize;
        while offset < size {
            let page = addr + offset;
            if space.query_leaf(page.into()).is_ok() {
                space
                    .unmap(page.into(), page_size)
                    .expect("native MM lost a mapped leaf during zap");
            }
            offset = offset.saturating_add(page_size);
        }
        unsafe { axhal::asm::flush_tlb(None) };
    }
}

/// Linux `unmap_mapping_range()` for native MM mappings created by this
/// LinuxKPI backend. `vm_mmap()` currently admits shared file mappings only,
/// so `even_cows` has no private-COW distinction to apply. The file/page refs
/// remain owned by each VMA until normal VMA teardown; this operation only
/// removes the affected PTEs, matching the source API's revocation semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmap_mapping_range(
    mapping: *mut c_void,
    holebegin: c_long,
    holelen: c_long,
    _even_cows: i32,
) {
    if mapping.is_null() || holebegin < 0 || holelen < 0 {
        return;
    }
    let page_size = crate::linux_config::PAGE_SIZE as u64;
    let hole_start = holebegin as u64;
    let hole_end = if holelen == 0 {
        u64::MAX
    } else {
        hole_start.saturating_add(holelen as u64)
    };

    // Clone the process MM owners before taking their sleepable mmap locks;
    // duplicate thread bindings are harmless because zapping is idempotent.
    let mms = {
        let registry = MAPPINGS.lock();
        let mut mms = Vec::new();
        mms.try_reserve_exact(registry.len())
            .expect("cannot snapshot native MM owners for mapping revocation");
        mms.extend(registry.values().cloned());
        mms
    };
    for mm_owner in mms {
        let mm = Arc::as_ptr(&mm_owner).cast_mut();
        unsafe { mmap_read_lock(mm) };
        let vmas = unsafe { &*(*mm).vmas.get() };
        for vma in vmas.values() {
            let area = &vma.area;
            if area.vm_file.is_null() {
                continue;
            }
            let file = area.vm_file.cast::<shmem::File>();
            if unsafe { (*file).f_mapping } != mapping.cast() {
                continue;
            }

            let file_start = (area.vm_pgoff as u64).saturating_mul(page_size);
            let address_start = area.vm_start as u64;
            let vma_len = area.vm_end.saturating_sub(area.vm_start) as u64;
            let file_end = file_start.saturating_add(vma_len);
            let first = core::cmp::max(file_start, hole_start);
            let last = core::cmp::min(file_end, hole_end);
            if first >= last {
                continue;
            }

            let first_page = first / page_size * page_size;
            let last_page = last.saturating_add(page_size - 1) / page_size * page_size;
            let address = address_start.saturating_add(first_page.saturating_sub(file_start));
            let length = last_page.saturating_sub(first_page) as usize;
            unsafe { zap_range(mm, address as usize, length) };
        }
        unsafe { mmap_read_unlock(mm) };
    }
}
pub unsafe fn vm_mmap(
    file: *mut File,
    addr: c_ulong,
    size: c_ulong,
    prot: u32,
    flags: u32,
    offset: u64,
) -> c_ulong {
    let mm = current_mm();
    if mm.is_null() {
        return (-crate::linux_config::ENODEV as isize) as c_ulong;
    }
    if file.is_null()
        || size == 0
        || (size | offset as c_ulong) & 4095 != 0
        || flags != 1
        || prot & !7 != 0
    {
        return (-crate::linux_config::EINVAL as isize) as c_ulong;
    }
    unsafe { mmap_read_lock(mm) };
    let result = (|| -> Result<usize, c_int> {
        let space = unsafe { &mut *(*mm).aspace.get() };
        let start = space
            .find_free_area(
                if addr == 0 {
                    space.base()
                } else {
                    (addr as usize).into()
                },
                size as usize,
                VirtAddrRange::new(space.base(), space.end()),
            )
            .ok_or(-crate::linux_config::ENOMEM)?
            .as_usize();
        let file = file.cast::<shmem::File>();
        let mapping = unsafe { (*file).f_mapping };
        let mut pages = Vec::new();
        pages
            .try_reserve_exact((size >> 12) as usize)
            .map_err(|_| -crate::linux_config::ENOMEM)?;
        let pgprot =
            5 | if prot & 2 != 0 { 2 } else { 0 } | if prot & 4 == 0 { 1u64 << 63 } else { 0 };
        let mut mapped = 0usize;
        for index in 0..(size >> 12) as usize {
            let page = unsafe {
                shmem::shmem_read_mapping_page(
                    mapping,
                    (offset >> 12) as c_ulong + index as c_ulong,
                )
            };
            let result = if (page as isize) < 0 && (page as usize) >= usize::MAX - 4095 {
                page as isize as i32
            } else {
                pages.push(page as usize);
                unsafe {
                    map_pfn(
                        mm,
                        start + index * 4096,
                        shmem::page_to_phys(page) >> 12,
                        pgprot,
                    )
                }
            };
            if result != 0 {
                if mapped != 0 {
                    unsafe { zap_range(mm, start, mapped) }
                }
                for page in pages {
                    unsafe { shmem::put_page(page as *mut Page) }
                }
                return Err(result);
            }
            mapped += 4096;
        }
        let mut area: Box<VmAreaStruct> = Box::new(unsafe { core::mem::zeroed() });
        area.vm_start = start as c_ulong;
        area.vm_end = (start + size as usize) as c_ulong;
        area.vm_mm = mm.cast();
        area.vm_flags = if prot & 2 != 0 { VM_WRITE } else { 0 };
        area.vm_page_prot.pgprot = pgprot as c_ulong;
        area.vm_pgoff = (offset >> 12) as c_ulong;
        area.vm_file = file.cast();
        unsafe { shmem::get_file_file(file) };
        unsafe {
            (&mut *(*mm).vmas.get()).insert(
                start,
                NativeVma {
                    area,
                    pages,
                    file: file as usize,
                },
            )
        };
        Ok(start)
    })();
    unsafe { mmap_read_unlock(mm) };
    result.unwrap_or_else(|err| err as isize as usize) as c_ulong
}

/// Fault-checked usercopy through the native address space, never a raw copy
/// from an untrusted user pointer. Linux returns the number of uncopied bytes.
pub unsafe fn copy_from_user(to: *mut c_void, from: *const c_void, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    let mm = current_mm();
    if mm.is_null() {
        return size;
    }
    unsafe { mmap_read_lock(mm) };
    let result = unsafe {
        (&*(*mm).aspace.get()).read(
            (from as usize).into(),
            core::slice::from_raw_parts_mut(to.cast(), size),
        )
    };
    unsafe { mmap_read_unlock(mm) };
    if result.is_ok() { 0 } else { size }
}
pub unsafe fn copy_to_user(to: *mut c_void, from: *const c_void, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    let mm = current_mm();
    if mm.is_null() {
        return size;
    }
    unsafe { mmap_read_lock(mm) };
    let result = unsafe {
        (&*(*mm).aspace.get()).write(
            (to as usize).into(),
            core::slice::from_raw_parts(from.cast(), size),
        )
    };
    unsafe { mmap_read_unlock(mm) };
    if result.is_ok() { 0 } else { size }
}

/// Linux `copy_from_user()` ABI entry; see [`copy_from_user`].
#[unsafe(export_name = "copy_from_user")]
pub unsafe extern "C" fn copy_from_user_c(to: *mut c_void, from: *const c_void, n: usize) -> usize {
    unsafe { copy_from_user(to, from, n) }
}

/// Linux `copy_to_user()` ABI entry; see [`copy_to_user`].
#[unsafe(export_name = "copy_to_user")]
pub unsafe extern "C" fn copy_to_user_c(to: *mut c_void, from: *const c_void, n: usize) -> usize {
    unsafe { copy_to_user(to, from, n) }
}

/// Linux `__copy_to_user()`: the caller has already performed `access_ok()`;
/// the native backend still performs the checked address-space write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __copy_to_user(to: *mut c_void, from: *const c_void, n: usize) -> usize {
    unsafe { copy_to_user(to, from, n) }
}

/// Linux `__copy_to_user_inatomic()`: never sleeps or faults in. If the
/// address space lock is contended the whole copy is reported as uncopied,
/// which is the value Linux's inatomic callers already retry on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __copy_to_user_inatomic(
    to: *mut c_void,
    from: *const c_void,
    n: usize,
) -> usize {
    if n == 0 {
        return 0;
    }
    let mm = current_mm();
    if mm.is_null() || !mmap_read_trylock(mm) {
        return n;
    }
    let result = unsafe {
        (&*(*mm).aspace.get()).write(
            (to as usize).into(),
            core::slice::from_raw_parts(from.cast(), n),
        )
    };
    unsafe { mmap_read_unlock(mm) };
    if result.is_ok() { 0 } else { n }
}

/// Linux `copy_from_user_inatomic_nontemporal()`. The non-temporal store hint
/// only selects the cache allocation policy of the destination writes; the
/// copied bytes and the uncopied-byte result are those of the inatomic copy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copy_from_user_inatomic_nontemporal(
    to: *mut c_void,
    from: *const c_void,
    n: usize,
) -> usize {
    unsafe { __copy_from_user_inatomic(to, from, n) }
}

/// Linux `access_ok()` on x86_64: a user range is admissible when it does not
/// wrap and ends at or below `TASK_SIZE_MAX`. This only range-checks the
/// pointer; the copy itself is still checked page by page by axmm.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn access_ok(addr: *const c_void, size: u64) -> bool {
    const TASK_SIZE_MAX: u64 = 0x0000_7fff_ffff_f000;
    let addr = addr as u64;
    addr <= TASK_SIZE_MAX && size <= TASK_SIZE_MAX - addr
}

/// Linux usercopy ABI. The native MM implementation walks the current
/// process's checked address space directly and returns the uncopied byte
/// count, rather than dereferencing an unchecked userspace virtual address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __copy_from_user(
    to: *mut c_void,
    from: *const c_void,
    size: usize,
) -> usize {
    unsafe { copy_from_user(to, from, size) }
}

/// In the native-MM backend page faults are explicit address-space lookups;
/// this entry therefore has the same checked-copy semantics and never invokes
/// a hardware fault handler from the caller's atomic relocation pass.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __copy_from_user_inatomic(
    to: *mut c_void,
    from: *const c_void,
    size: usize,
) -> usize {
    if size == 0 {
        return 0;
    }
    let mm = current_mm();
    if !mmap_read_trylock(mm) {
        return size;
    }
    let result = unsafe {
        (&*(*mm).aspace.get()).read(
            (from as usize).into(),
            core::slice::from_raw_parts_mut(to.cast(), size),
        )
    };
    unsafe { mmap_read_unlock(mm) };
    if result.is_ok() { 0 } else { size }
}

/// Linux `pin_user_pages_fast(start, nr_pages, gup_flags, pages)`. Each page
/// is resolved through the current process's checked address space, must be
/// readable (and writable under `FOLL_WRITE`), and receives one page
/// reference that `unpin_user_pages` drops. A short pin returns the number
/// pinned; a failure before the first page returns a negative errno.
#[unsafe(export_name = "pin_user_pages_fast")]
pub unsafe extern "C" fn pin_user_pages_fast_c(
    start: c_ulong,
    nr_pages: i32,
    gup_flags: u32,
    pages: *mut *mut Page,
) -> i32 {
    const FOLL_WRITE: u32 = 1 << 0;
    if nr_pages <= 0 {
        return 0;
    }
    if pages.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let mm = current_mm();
    if mm.is_null() {
        return -crate::linux_config::EFAULT;
    }
    let count = nr_pages as usize;
    let mut pinned = 0usize;
    let mut failure = 0i32;
    unsafe { mmap_read_lock(mm) };
    {
        let aspace = unsafe { &*(*mm).aspace.get() };
        for index in 0..count {
            let Some(address) = (start as usize).checked_add(index * PAGE_SIZE) else {
                failure = -crate::linux_config::EFAULT;
                break;
            };
            let Ok((physical, flags, _)) = aspace.query_leaf(address.into()) else {
                failure = -crate::linux_config::EFAULT;
                break;
            };
            let writable_ok = gup_flags & FOLL_WRITE == 0 || flags.contains(MappingFlags::WRITE);
            if !flags.contains(MappingFlags::READ) || !writable_ok {
                failure = -crate::linux_config::EFAULT;
                break;
            }
            let page = unsafe { shmem::pin_physical_frame(physical.as_usize()) };
            if page.is_null() {
                failure = -crate::linux_config::ENOMEM;
                break;
            }
            unsafe { pages.add(index).write(page) };
            pinned += 1;
        }
    }
    unsafe { mmap_read_unlock(mm) };
    if pinned == 0 && failure != 0 {
        return failure;
    }
    pinned as i32
}

/// Linux `unpin_user_pages(pages, npages)`: drops the references taken by
/// [`pin_user_pages_fast_c`].
///
/// # Safety
/// Every non-NULL entry must be a page returned by a pin and not yet unpinned.
#[unsafe(export_name = "unpin_user_pages")]
pub unsafe extern "C" fn unpin_user_pages_c(pages: *mut *mut Page, npages: c_ulong) {
    for index in 0..npages as usize {
        let page = unsafe { pages.add(index).read() };
        if !page.is_null() {
            unsafe { shmem::put_page(page) };
        }
    }
}

/// Linux `mmu_interval_notifier_insert()`: subscribe `notifier` to
/// invalidations of `[start, start + length)` in `mm`. Its sequence starts at
/// the mm's current `notifier_seq`.
///
/// # Safety
/// `notifier` must stay valid until [`mmu_interval_notifier_remove`].
#[unsafe(export_name = "mmu_interval_notifier_insert")]
pub unsafe extern "C" fn mmu_interval_notifier_insert(
    notifier: *mut MmuIntervalNotifier,
    mm: *mut MmStruct,
    start: c_ulong,
    length: c_ulong,
    ops: *const MmuIntervalNotifierOps,
) -> c_int {
    if notifier.is_null() || mm.is_null() || ops.is_null() || length == 0 {
        return -crate::linux_config::EINVAL;
    }
    let Some(last) = (start as usize).checked_add(length as usize - 1) else {
        return -crate::linux_config::EINVAL;
    };
    unsafe {
        let mut subscribers = (*mm).interval_notifiers.lock();
        if subscribers.contains(&(notifier as usize)) {
            return -crate::linux_config::EINVAL;
        }
        (*notifier).interval_tree.start = start as usize;
        (*notifier).interval_tree.last = last;
        (*notifier).interval_tree.subtree_last = last;
        (*notifier).ops = ops;
        (*notifier).mm = mm;
        (*notifier).invalidate_seq = (*mm).notifier_seq.load(Ordering::Acquire) as c_ulong;
        subscribers.push(notifier as usize);
    }
    0
}

/// Linux `mmu_interval_notifier_remove()`. Invalidations are synchronous in
/// this owner, so no callback can still be running when the subscription is
/// dropped.
///
/// # Safety
/// `notifier` must have been inserted and not yet removed.
#[unsafe(export_name = "mmu_interval_notifier_remove")]
pub unsafe extern "C" fn mmu_interval_notifier_remove(notifier: *mut MmuIntervalNotifier) {
    if notifier.is_null() {
        return;
    }
    let mm = unsafe { (*notifier).mm };
    if mm.is_null() {
        return;
    }
    unsafe {
        (*mm)
            .interval_notifiers
            .lock()
            .retain(|&entry| entry != notifier as usize);
        (*notifier).mm = ptr::null_mut();
    }
}

/// Linux `mmu_interval_read_begin()`: the sequence a reader samples before it
/// pins pages. A later invalidation stores a different value, which
/// `mmu_interval_read_retry` detects.
///
/// # Safety
/// `notifier` must be an inserted notifier.
#[unsafe(export_name = "mmu_interval_read_begin")]
pub unsafe extern "C" fn mmu_interval_read_begin(notifier: *mut MmuIntervalNotifier) -> c_ulong {
    unsafe { ptr::addr_of!((*notifier).invalidate_seq).read_volatile() }
}

/// `get_user()` / `__get_user()` for one typed value.
pub unsafe fn __get_user<T: Copy>(value: &mut T, from: *const T) -> c_int {
    if unsafe { copy_from_user((value as *mut T).cast(), from.cast(), size_of::<T>()) } == 0 {
        0
    } else {
        -crate::linux_config::EFAULT
    }
}

/// `put_user()` / `__put_user()` for one typed value.
pub unsafe fn __put_user<T: Copy>(value: T, to: *mut T) -> c_int {
    if unsafe { copy_to_user(to.cast(), core::ptr::addr_of!(value).cast(), size_of::<T>()) } == 0 {
        0
    } else {
        -crate::linux_config::EFAULT
    }
}

/// Inatomic `unsafe_put_user()` is valid only inside a successful user-access
/// session. It verifies the saved session bounds and writes through the native
/// page tables directly, without taking the mmap lock or yielding.
pub unsafe fn unsafe_put_user<T: Copy>(value: T, to: *mut T) -> c_int {
    let size = size_of::<T>();
    if size == 0 {
        return 0;
    }
    let mm = current_mm();
    let Some(task_id) = current_task_id() else {
        return -crate::linux_config::EFAULT;
    };
    if mm.is_null() {
        return -crate::linux_config::EFAULT;
    }
    let address = to as usize;
    let Some(end) = address.checked_add(size) else {
        return -crate::linux_config::EFAULT;
    };
    let owner = unsafe { (*mm).user_access_owner.load(Ordering::Acquire) };
    let start = unsafe { (*mm).user_access_start.load(Ordering::Relaxed) };
    let session_end = unsafe { (*mm).user_access_end.load(Ordering::Relaxed) };
    if owner != task_id || address < start || end > session_end {
        return -crate::linux_config::EFAULT;
    }
    let result = unsafe {
        (&*(*mm).aspace.get()).write(
            address.into(),
            core::slice::from_raw_parts(ptr::addr_of!(value).cast::<u8>(), size),
        )
    };
    if result.is_ok() {
        0
    } else {
        -crate::linux_config::EFAULT
    }
}

/// Check every leaf covered by the complete byte range, not only its first
/// address. `query_leaf()` resolves large-page leaves too, so stepping by the
/// native base page safely covers the range independent of leaf size.
fn user_write_range_mapped(mm: *mut MmStruct, address: usize, size: usize) -> bool {
    if mm.is_null() || size == 0 {
        return size == 0 && !mm.is_null();
    }
    let Some(last_byte) = address.checked_add(size - 1) else {
        return false;
    };
    let space = unsafe { &*(*mm).aspace.get() };
    if !space.contains_range(address.into(), size) {
        return false;
    }
    let page_size = crate::linux_config::PAGE_SIZE;
    let mut page = address & !(page_size - 1);
    let last_page = last_byte & !(page_size - 1);
    loop {
        let Ok((_, flags, _)) = space.query_leaf(page.into()) else {
            return false;
        };
        if !flags.contains(MappingFlags::WRITE) {
            return false;
        }
        if page == last_page {
            return true;
        }
        let Some(next) = page.checked_add(page_size) else {
            return false;
        };
        page = next;
    }
}

fn current_task_id() -> Option<u64> {
    axtask::current_may_uninit().map(|task| task.id().as_u64())
}

/// Begin a native-MM user write region without sleeping. The mmap lock remains
/// held until the matching end call, preventing unmap/protection changes while
/// the inatomic scalar writes are performed. A contended lock, absent task/MM,
/// non-writable leaf, or invalid byte range fails closed.
pub fn user_access_begin(address: *const c_void, size: usize) -> bool {
    if size == 0 {
        return true;
    }
    if address.is_null() {
        return false;
    }
    let Some(session_end) = (address as usize).checked_add(size) else {
        return false;
    };
    let Some(task_id) = current_task_id() else {
        return false;
    };
    let mm = current_mm();
    if mm.is_null() || unsafe { (*mm).user_access_owner.load(Ordering::Acquire) != u64::MAX } {
        return false;
    }
    if !mmap_read_trylock(mm) {
        return false;
    }
    if !user_write_range_mapped(mm, address as usize, size) {
        unsafe { mmap_read_unlock(mm) };
        return false;
    }
    unsafe {
        (*mm)
            .user_access_start
            .store(address as usize, Ordering::Relaxed);
        (*mm).user_access_end.store(session_end, Ordering::Relaxed);
        (*mm).user_access_owner.store(task_id, Ordering::Release);
    }
    true
}

/// Linux defines `user_write_access_begin()` as `user_access_begin()` on the
/// supported x86 configuration; the native-MM adapter uses the same checked,
/// nonblocking writeable-range session for both names.
pub fn user_write_access_begin(address: *const c_void, size: usize) -> bool {
    user_access_begin(address, size)
}

/// Close the current task's validated user-access session and release the
/// mmap lock. Zero-length sessions are no-ops, matching the begin helper.
pub fn user_access_end() {
    let Some(task_id) = current_task_id() else {
        return;
    };
    let mm = current_mm();
    if mm.is_null() {
        return;
    }
    let owner = unsafe { (*mm).user_access_owner.load(Ordering::Acquire) };
    if owner == u64::MAX {
        return;
    }
    assert_eq!(owner, task_id, "user access session ended by another task");
    unsafe {
        (*mm).user_access_owner.store(u64::MAX, Ordering::Release);
        (*mm).user_access_start.store(0, Ordering::Relaxed);
        (*mm).user_access_end.store(0, Ordering::Relaxed);
        mmap_read_unlock(mm);
    }
}

/// Alias matching Linux `user_write_access_end()` on the native backend.
pub fn user_write_access_end() {
    user_access_end();
}

/// The native usercopy path does explicit page-table lookup instead of fault
/// recovery; these scope markers do not need to alter CPU fault state.
pub fn pagefault_disable() {}
pub fn pagefault_enable() {}

/// Linux u64 pointer conversion for internal, non-user ABI pointers.
pub fn u64_to_ptr<T>(address: u64) -> *mut T {
    address as usize as *mut T
}

#[cfg(test)]
mod usercopy_range_tests {
    use super::*;

    #[test]
    fn access_ok_bounds_the_user_address_space() {
        let top = 0x0000_7fff_ffff_f000usize as *const c_void;
        unsafe {
            assert!(access_ok(0x1000 as *const c_void, 0x1000));
            assert!(access_ok(top, 0));
            assert!(!access_ok(top, 1));
            assert!(!access_ok(0x1000 as *const c_void, u64::MAX));
        }
    }
}
