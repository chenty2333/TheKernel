// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Original LinuxKPI MM operations over native axmm address spaces. No Linux
//! MM implementation is copied. A process owner binds its native address space
//! explicitly; kernel tasks without a binding cannot create user mappings.
use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    ffi::{c_int, c_ulong, c_void},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use axhal::paging::MappingFlags;
use memory_addr::{MemoryAddr, VirtAddrRange};

use super::{
    mm::{VM_WRITE, VmAreaStruct},
    shmem,
};
use crate::{i915_gem_object_types_upstream::Page, intel_context_types_upstream::File};

pub struct MmStruct {
    aspace: UnsafeCell<axmm::AddrSpace>,
    vmas: UnsafeCell<BTreeMap<usize, NativeVma>>,
    locked: AtomicBool,
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
pub unsafe fn zap_range(mm: *mut MmStruct, addr: usize, size: usize) {
    assert!(!mm.is_null());
    let space = unsafe { &mut *(*mm).aspace.get() };
    if size != 0 {
        space
            .unmap(addr.into(), size)
            .expect("invalid native MM unmap");
        unsafe { axhal::asm::flush_tlb(None) };
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
